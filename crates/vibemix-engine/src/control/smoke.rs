//! Split out of the controller; see `mod.rs`.

use super::*;

/// Runs the engine binary as a throwaway child that loads and runs a plugin
/// offline, with a 3 s timeout. Safe to call from any thread.
pub(super) fn run_smoke_process(key: &str, args: &[String]) -> SmokeVerdict {
    let Ok(executable) = std::env::current_exe() else {
        return SmokeVerdict::Inconclusive;
    };
    let mut command = Command::new(executable);
    command.args(args);
    if !crate::smoke::debug_enabled() {
        // (In debug mode stdio stays inherited so a plugin's own diagnostics
        // remain visible.)
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    // The child disables its own core dumps (see the `run_smoke*` entry
    // points), so no `pre_exec` is needed here. That matters: `pre_exec`
    // forces `fork()` in the multithreaded engine (RT thread, health
    // reader, plugin UI threads), which can deadlock the child while it
    // holds a lock another thread took at fork time.
    let Ok(mut child) = command.spawn() else {
        return SmokeVerdict::Inconclusive;
    };

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return SmokeVerdict::Pass,
            Ok(Some(status)) => {
                let reason = if let Some(signal) = status.signal() {
                    let name = crate::smoke::signal_name(signal);
                    tracing::warn!(plugin = %key, signal, "plugin crashed during smoke test, refusing to load");
                    name.to_string()
                } else {
                    let code = status.code().unwrap_or(-1);
                    tracing::warn!(plugin = %key, code, "plugin failed smoke test, refusing to load");
                    format!("exit {code}")
                };
                return SmokeVerdict::Fail(reason);
            }
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(plugin = %key, "plugin smoke test timed out, refusing to load");
                    return SmokeVerdict::Fail("timeout".to_string());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return SmokeVerdict::Inconclusive,
        }
    }
}

/// Mono/stereo LV2 plugins plus every VST3 effect, in catalog order.
/// Runs the engine binary with `args` as a throwaway UI smoke test. A child
/// that exits non-zero or exceeds `timeout` is considered unsafe; a failure to
/// spawn at all is treated as safe (same as the LV2 path).
pub(super) fn ui_smoke_ok(args: &[&str], timeout: Duration) -> bool {
    let Ok(executable) = std::env::current_exe() else {
        return true;
    };
    let Ok(mut child) = Command::new(executable)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return true;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return true,
        }
    }
}

/// Locks an editor-smoke verdict map, recovering from a poisoned mutex (a
/// background smoke thread should never panic, but a lock must not take the
/// control loop down).
pub(super) fn ui_safe_map(
    map: &Mutex<HashMap<String, bool>>,
) -> std::sync::MutexGuard<'_, HashMap<String, bool>> {
    map.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Runs an editor smoke test in a detached background thread and records the
/// verdict, keyed by `key`.
pub(super) fn spawn_ui_smoke(
    map: Arc<Mutex<HashMap<String, bool>>>,
    key: String,
    args: Vec<String>,
) {
    let _ = std::thread::Builder::new()
        .name("ui-smoke".to_string())
        .spawn(move || {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let safe = ui_smoke_ok(&refs, Duration::from_secs(10));
            if !safe {
                tracing::warn!(plugin = %key, "plugin UI failed smoke test; not opening");
            }
            ui_safe_map(&map).insert(key, safe);
        });
}

impl Controller {
    /// Starts a plugin health sweep: a full one when the cache is stale/empty,
    /// otherwise a targeted one for newly installed plugins (if any).
    pub(super) fn start_health_scan(&mut self) {
        if self.health.sweepable(&self.plugins).is_empty() {
            return;
        }
        if self.health.is_stale() {
            self.scan.start(None);
            return;
        }
        let untested: Vec<String> = self
            .health
            .untested(&self.plugins)
            .into_iter()
            .map(str::to_string)
            .collect();
        if !untested.is_empty() {
            self.scan.start(Some(&untested));
        }
    }

    pub(super) fn record_health(&mut self, uri: &str, status: Status, reason: String) {
        self.health.set(uri, status, reason, VerdictMeta::default());
        self.health_dirty = true;
        self.health_immediate = true;
    }

    /// Runs the engine binary as a throwaway smoke test for `key` (an LV2 URI or
    /// a VST3 class id), caching and recording the verdict. Blocks for up to the
    /// smoke timeout; interactive inserts use [`Self::insert_plugin_slot_deferred`]
    /// instead so the control loop keeps running.
    pub(super) fn run_smoke_child(&mut self, key: &str, args: &[&str]) -> bool {
        if self.smoke_ok.contains(key) {
            return true;
        }
        if self.smoke_bad.contains(key) {
            return false;
        }
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let verdict = run_smoke_process(key, &args);
        self.apply_smoke_verdict(key, verdict)
    }

    /// Caches and records a smoke verdict; returns whether loading is allowed.
    pub(super) fn apply_smoke_verdict(&mut self, key: &str, verdict: SmokeVerdict) -> bool {
        match verdict {
            SmokeVerdict::Pass => {
                self.smoke_ok.insert(key.to_string());
                self.record_health(key, Status::Ok, String::new());
                true
            }
            SmokeVerdict::Fail(reason) => {
                self.smoke_bad.insert(key.to_string());
                self.record_health(key, Status::Failed, reason);
                false
            }
            // The child could not be run or polled; do not block the plugin.
            SmokeVerdict::Inconclusive => true,
        }
    }

    /// The cache key and child arguments for smoke-testing `plugin`.
    pub(super) fn smoke_request(plugin: &PluginSlot) -> Option<(String, Vec<String>)> {
        let flag = match plugin.format {
            PluginFormat::Lv2 => {
                return Some((
                    plugin.uri.clone(),
                    vec!["--smoke".to_string(), plugin.uri.clone()],
                ));
            }
            PluginFormat::Vst3 => "--smoke-vst3",
            PluginFormat::Clap => "--smoke-clap",
        };
        let bundle = plugin.bundle.clone()?;
        Some((
            plugin.uri.clone(),
            vec![flag.to_string(), bundle, plugin.uri.clone()],
        ))
    }

    /// Like [`Self::insert_plugin_slot`], but a plugin whose smoke test has not
    /// run yet is tested on a worker thread and inserted when it passes, so a
    /// slow or hanging plugin never stalls meters and faders. Returns whether a
    /// snapshot is due now; a deferred insert reports through `poll_smoke`.
    pub(super) fn insert_plugin_slot_deferred(
        &mut self,
        location: u32,
        target: u32,
        plugin: PluginSlot,
    ) -> bool {
        let Some((key, args)) = Self::smoke_request(&plugin) else {
            tracing::warn!(uri = %plugin.uri, "plugin has no bundle path");
            return false;
        };
        if self.smoke_ok.contains(&key) || self.smoke_bad.contains(&key) {
            return self.insert_plugin_slot(location, target, plugin);
        }
        if self.smoke_inflight.insert(key.clone()) {
            let tx = self.smoke_tx.clone();
            let thread_key = key.clone();
            std::thread::spawn(move || {
                let verdict = run_smoke_process(&thread_key, &args);
                let _ = tx.send((thread_key, verdict));
            });
        }
        self.deferred_inserts.push(DeferredInsert {
            key,
            location,
            target,
            plugin,
        });
        false
    }

    /// Collects finished background smoke tests and completes the inserts that
    /// were waiting for them. Returns true when the model changed.
    pub(super) fn poll_smoke(&mut self) -> bool {
        let mut changed = false;
        while let Ok((key, verdict)) = self.smoke_rx.try_recv() {
            self.smoke_inflight.remove(&key);
            let allowed = self.apply_smoke_verdict(&key, verdict);
            let waiting = std::mem::take(&mut self.deferred_inserts);
            for deferred in waiting {
                if deferred.key != key {
                    self.deferred_inserts.push(deferred);
                } else if allowed {
                    changed |= self.insert_plugin_slot(
                        deferred.location,
                        deferred.target,
                        deferred.plugin,
                    );
                }
            }
        }
        changed
    }

    pub(super) fn plugin_is_safe(&mut self, uri: &str) -> bool {
        self.run_smoke_child(uri, &["--smoke", uri])
    }

    pub(super) fn vst3_is_safe(&mut self, bundle: &str, class_id: &str) -> bool {
        self.run_smoke_child(class_id, &["--smoke-vst3", bundle, class_id])
    }

    pub(super) fn clap_is_safe(&mut self, bundle: &str, plugin_id: &str) -> bool {
        self.run_smoke_child(plugin_id, &["--smoke-clap", bundle, plugin_id])
    }

    pub(super) fn ui_is_safe(&mut self, uri: &str) -> bool {
        if let Some(safe) = ui_safe_map(&self.ui_safe).get(uri).copied() {
            return safe;
        }
        // Fallback for a plugin whose background smoke has not finished (e.g. a
        // session restore, which does not go through `insert_plugin_slot`).
        let safe = ui_smoke_ok(&["--smoke-ui", uri], Duration::from_secs(10));
        if !safe {
            tracing::warn!(%uri, "plugin UI failed smoke test; not opening");
        }
        ui_safe_map(&self.ui_safe).insert(uri.to_string(), safe);
        safe
    }

    /// Runs the VST3 editor headlessly in a child so a crashing editor is
    /// refused rather than taking the engine down.
    pub(super) fn vst3_ui_is_safe(&mut self, bundle: &str, class_id: &str) -> bool {
        if let Some(safe) = ui_safe_map(&self.vst3_ui_safe).get(class_id).copied() {
            return safe;
        }
        let safe = ui_smoke_ok(
            &["--smoke-vst3-ui", bundle, class_id],
            Duration::from_secs(10),
        );
        if !safe {
            tracing::warn!(%class_id, "VST3 editor failed smoke test; not opening");
        }
        ui_safe_map(&self.vst3_ui_safe).insert(class_id.to_string(), safe);
        safe
    }

    /// Runs the CLAP editor headlessly in a child so a crashing editor is
    /// refused rather than taking the engine down.
    pub(super) fn clap_ui_is_safe(&mut self, bundle: &str, plugin_id: &str) -> bool {
        if let Some(safe) = ui_safe_map(&self.clap_ui_safe).get(plugin_id).copied() {
            return safe;
        }
        let safe = ui_smoke_ok(
            &["--smoke-clap-ui", bundle, plugin_id],
            Duration::from_secs(10),
        );
        if !safe {
            tracing::warn!(%plugin_id, "CLAP editor failed smoke test; not opening");
        }
        ui_safe_map(&self.clap_ui_safe).insert(plugin_id.to_string(), safe);
        safe
    }

    /// Starts the editor safety smoke for a newly inserted plugin in a
    /// background thread, so the first "show UI" click finds the verdict cached
    /// instead of spawning a heavy child on the control path while audio runs.
    pub(super) fn schedule_ui_smoke(&self, format: PluginFormat, uri: &str, bundle: Option<&str>) {
        let has_ui = self
            .plugins
            .iter()
            .find(|entry| entry.id == uri)
            .map(|entry| entry.has_ui)
            .unwrap_or(true);
        if !has_ui {
            return;
        }
        match format {
            PluginFormat::Vst3 => {
                let Some(bundle) = bundle else {
                    return;
                };
                if ui_safe_map(&self.vst3_ui_safe).contains_key(uri) {
                    return;
                }
                spawn_ui_smoke(
                    Arc::clone(&self.vst3_ui_safe),
                    uri.to_string(),
                    vec![
                        "--smoke-vst3-ui".to_string(),
                        bundle.to_string(),
                        uri.to_string(),
                    ],
                );
            }
            PluginFormat::Lv2 => {
                // Only in-process (instance-access) editors need the smoke; the
                // out-of-process host isolates them already.
                let needs_local = find_ui(uri)
                    .map(|candidate| candidate.requires_instance_access)
                    .unwrap_or(false);
                if !needs_local || ui_safe_map(&self.ui_safe).contains_key(uri) {
                    return;
                }
                spawn_ui_smoke(
                    Arc::clone(&self.ui_safe),
                    uri.to_string(),
                    vec!["--smoke-ui".to_string(), uri.to_string()],
                );
            }
            PluginFormat::Clap => {
                let Some(bundle) = bundle else {
                    return;
                };
                if ui_safe_map(&self.clap_ui_safe).contains_key(uri) {
                    return;
                }
                spawn_ui_smoke(
                    Arc::clone(&self.clap_ui_safe),
                    uri.to_string(),
                    vec![
                        "--smoke-clap-ui".to_string(),
                        bundle.to_string(),
                        uri.to_string(),
                    ],
                );
            }
        }
    }
}
