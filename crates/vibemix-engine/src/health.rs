//! Persistent plugin health verdicts and the batch sweep driver.
//!
//! The engine smoke-tests every hostable plugin in an isolated child process
//! (`vibemix-engine --scan-plugins`) and records the outcome. The results are
//! cached in the config directory and surfaced in the plugin picker so broken
//! plugins are visible (red) rather than silently hidden.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};

use serde::{Deserialize, Serialize};
use vibemix_core::config::config_dir;
use vibemix_core::model::PluginFormat;
use vibemix_lv2::HOST_ABI_VERSION;

use crate::catalog::PluginEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Failed,
    Unsupported,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Verdict {
    pub status: Status,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct HealthFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: HashMap<String, Verdict>,
}

/// Persisted verdicts, keyed by plugin URI and tagged with the host ABI version
/// so a host fix invalidates stale failures.
pub struct HealthStore {
    path: PathBuf,
    loaded_version: u32,
    entries: HashMap<String, Verdict>,
    dirty: bool,
}

impl HealthStore {
    pub fn load() -> Self {
        let path = config_dir().join("plugin_health.json");
        let file: HealthFile = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let loaded_version = file.version;
        let entries = if loaded_version == HOST_ABI_VERSION {
            file.entries
        } else {
            HashMap::new()
        };
        Self {
            path,
            loaded_version,
            entries,
            dirty: false,
        }
    }

    /// True when the cache is missing or was written by a different host build,
    /// in which case every plugin must be retested.
    pub fn is_stale(&self) -> bool {
        self.loaded_version != HOST_ABI_VERSION || self.entries.is_empty()
    }

    /// Ids the sweep would test (`hostable` mono/stereo LV2 plugins).
    pub fn sweepable<'a>(&self, plugins: &'a [PluginEntry]) -> Vec<&'a str> {
        plugins
            .iter()
            .filter(|entry| sweepable(entry))
            .map(|entry| entry.id.as_str())
            .collect()
    }

    /// Sweepable plugins that have no verdict yet, e.g. newly installed ones.
    pub fn untested<'a>(&self, plugins: &'a [PluginEntry]) -> Vec<&'a str> {
        plugins
            .iter()
            .filter(|entry| sweepable(entry) && !self.entries.contains_key(&entry.id))
            .map(|entry| entry.id.as_str())
            .collect()
    }

    pub fn get(&self, uri: &str) -> Option<&Verdict> {
        self.entries.get(uri)
    }

    pub fn set(&mut self, uri: &str, status: Status, reason: impl Into<String>) {
        let next = Verdict {
            status,
            reason: reason.into(),
        };
        if self.entries.get(uri) == Some(&next) {
            return;
        }
        self.entries.insert(uri.to_string(), next);
        self.dirty = true;
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let file = HealthFile {
            version: HOST_ABI_VERSION,
            entries: self.entries.clone(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&file) {
            let parent = self.path.parent().unwrap_or_else(|| std::path::Path::new("."));
            let _ = fs::create_dir_all(parent);
            let _ = fs::write(&self.path, text);
            self.dirty = false;
        }
    }
}

/// Whether the sweep tests this plugin. LV2 is limited to hostable mono/stereo
/// audio; VST3 has no channel metadata until instantiated, so test them all.
fn sweepable(entry: &PluginEntry) -> bool {
    if !entry.hostable {
        return false;
    }
    match entry.format {
        // `inputs` counts sidechain inputs too; require at least the main pair.
        PluginFormat::Lv2 => {
            (entry.outputs == 2 && entry.inputs >= 2) || (entry.outputs == 1 && entry.inputs >= 1)
        }
        PluginFormat::Vst3 => true,
    }
}

/// One line of the `--scan-plugins` stdout protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanEvent {
    Start { total: usize },
    Verdict {
        uri: String,
        status: Status,
        reason: String,
    },
    Done { total: usize, failed: usize },
}

pub fn parse_scan_line(line: &str) -> Option<ScanEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    match value.get("scan")?.as_str()? {
        "start" => Some(ScanEvent::Start {
            total: value.get("total")?.as_u64()? as usize,
        }),
        "plugin" => Some(ScanEvent::Verdict {
            uri: value.get("uri")?.as_str()?.to_string(),
            status: match value.get("status")?.as_str()? {
                "ok" => Status::Ok,
                _ => Status::Failed,
            },
            reason: value
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "done" => Some(ScanEvent::Done {
            total: value.get("total")?.as_u64()? as usize,
            failed: value.get("failed")?.as_u64()? as usize,
        }),
        _ => None,
    }
}

/// Drives the background `--scan-plugins` sweep.
pub struct Scan {
    pub running: bool,
    pub done: usize,
    pub total: usize,
    child: Option<Child>,
    rx: Option<Receiver<ScanEvent>>,
}

impl Default for Scan {
    fn default() -> Self {
        Self::new()
    }
}

impl Scan {
    pub fn new() -> Self {
        Self {
            running: false,
            done: 0,
            total: 0,
            child: None,
            rx: None,
        }
    }

    /// Spawns a sweep unless one is already running. `only` restricts it to the
    /// given plugin URIs (`None` sweeps everything). Returns whether it started.
    pub fn start(&mut self, only: Option<&[String]>) -> bool {
        if self.running {
            return false;
        }
        let Ok(executable) = std::env::current_exe() else {
            return false;
        };
        let mut command = Command::new(executable);
        command
            .arg("--scan-plugins")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(only) = only {
            for uri in only {
                command.arg("--only").arg(uri);
            }
        }
        // Safety: only affects the child. Run the sweep in its own process group
        // (so it can be killed wholesale) and at a lower priority (so the audio
        // thread and UI always win).
        unsafe {
            command.pre_exec(|| {
                libc::setpgid(0, 0);
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
                Ok(())
            });
        }
        let Ok(mut child) = command.spawn() else {
            return false;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(stdout) = child.stdout.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(event) = parse_scan_line(&line) {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                }
            });
        }
        self.child = Some(child);
        self.rx = Some(rx);
        self.running = true;
        self.done = 0;
        self.total = 0;
        true
    }

    /// Aborts a running sweep, killing the supervisor and any forked children.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id() as i32;
            // The supervisor is its own process-group leader, so kill the whole
            // group to reap any plugin child too.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
            let _ = child.wait();
        }
        self.rx = None;
        self.running = false;
        self.done = 0;
        self.total = 0;
    }

    /// Drains pending sweep events; updates `running`/`done`/`total`.
    pub fn poll(&mut self) -> Vec<ScanEvent> {
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        match &event {
                            ScanEvent::Start { total } => {
                                self.total = *total;
                                self.done = 0;
                            }
                            ScanEvent::Verdict { .. } => self.done += 1,
                            ScanEvent::Done { .. } => self.running = false,
                        }
                        events.push(event);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.running = false;
                        break;
                    }
                }
            }
        }
        if self.running {
            if let Some(child) = self.child.as_mut() {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    self.running = false;
                }
            }
        }
        if !self.running {
            if let Some(mut child) = self.child.take() {
                let _ = child.wait();
            }
            self.rx = None;
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scan_lines() {
        assert_eq!(
            parse_scan_line(r#"{"scan":"start","total":3}"#),
            Some(ScanEvent::Start { total: 3 })
        );
        assert_eq!(
            parse_scan_line(
                r#"{"scan":"plugin","uri":"u","name":"n","status":"failed","reason":"crash SIGSEGV"}"#
            ),
            Some(ScanEvent::Verdict {
                uri: "u".to_string(),
                status: Status::Failed,
                reason: "crash SIGSEGV".to_string(),
            })
        );
        assert_eq!(
            parse_scan_line(r#"{"scan":"done","total":3,"failed":1}"#),
            Some(ScanEvent::Done {
                total: 3,
                failed: 1
            })
        );
        assert_eq!(parse_scan_line("not json"), None);
        assert_eq!(parse_scan_line(r#"{"scan":"other"}"#), None);
    }

    fn store(version: u32, entries: HashMap<String, Verdict>) -> HealthStore {
        HealthStore {
            path: PathBuf::from("/nonexistent/plugin_health.json"),
            loaded_version: version,
            entries,
            dirty: false,
        }
    }

    #[test]
    fn stale_when_empty_or_version_changed() {
        assert!(store(HOST_ABI_VERSION, HashMap::new()).is_stale());
        let verdict = Verdict {
            status: Status::Ok,
            reason: String::new(),
        };
        let mut entries = HashMap::new();
        entries.insert("u".to_string(), verdict.clone());
        assert!(!store(HOST_ABI_VERSION, entries).is_stale());
        let mut stale = HashMap::new();
        stale.insert("u".to_string(), verdict);
        assert!(store(HOST_ABI_VERSION - 1, stale).is_stale());
    }

    fn plugin(id: &str, hostable: bool, inputs: usize, outputs: usize) -> PluginEntry {
        PluginEntry {
            format: PluginFormat::Lv2,
            id: id.to_string(),
            name: id.to_string(),
            bundle: None,
            has_ui: false,
            has_sidechain: false,
            inputs,
            outputs,
            midi: false,
            hostable,
            unsupported: None,
            controls: Vec::new(),
        }
    }

    #[test]
    fn untested_lists_new_hostable_plugins() {
        let mut entries = HashMap::new();
        entries.insert(
            "known".to_string(),
            Verdict {
                status: Status::Ok,
                reason: String::new(),
            },
        );
        let health = store(HOST_ABI_VERSION, entries);
        let plugins = vec![
            plugin("known", true, 2, 2),
            plugin("new", true, 2, 2),
            plugin("mono", true, 1, 1),
            plugin("unsupported", false, 2, 2),
            plugin("weird", true, 2, 0),
        ];
        let mut untested = health.untested(&plugins);
        untested.sort_unstable();
        assert_eq!(untested, vec!["mono", "new"]);
    }

    #[test]
    fn set_dedups_and_marks_dirty() {
        let mut health = store(HOST_ABI_VERSION, HashMap::new());
        health.set("u", Status::Ok, "");
        assert!(health.dirty);
        health.dirty = false;
        health.set("u", Status::Ok, "");
        assert!(!health.dirty);
        health.set("u", Status::Failed, "crash SIGSEGV");
        assert!(health.dirty);
        assert_eq!(health.get("u").map(|v| v.status), Some(Status::Failed));
    }
}
