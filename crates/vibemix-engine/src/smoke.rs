//! Fault-isolated plugin smoke tests.
//!
//! A plugin is instantiated and run for a few blocks in a throwaway process, so
//! a crashing plugin never takes the engine down. The same logic backs the
//! per-add guard (`--smoke`) and the batch catalog sweep (`--scan-plugins`).

use std::io::Write;
use std::time::{Duration, Instant};

use vibemix_lv2::Host;

pub const SMOKE_OK: i32 = 0;
pub const SMOKE_INSTANTIATE_FAILED: i32 = 2;
pub const SMOKE_FAILED: i32 = 3;

const SMOKE_FRAMES: usize = 512;
const SMOKE_BLOCKS: usize = 64;
/// Per-plugin timeout for the batch sweep.
const SCAN_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether verbose plugin diagnostics are enabled (`--debug` sets this env).
pub fn debug_enabled() -> bool {
    std::env::var_os("VIBEMIX_DEBUG").is_some()
}

/// Disables core dumps for this process (and children forked from it).
pub fn disable_core_dumps() {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &limit);
    }
}

/// Instantiates `uri` and runs a few blocks of DC input. Returns the smoke exit
/// code ([`SMOKE_OK`] / [`SMOKE_INSTANTIATE_FAILED`]).
pub fn smoke_plugin(host: &Host, uri: &str) -> i32 {
    let Some(mut instance) = host.instantiate(uri, 48_000.0, SMOKE_FRAMES, 8192) else {
        return SMOKE_INSTANTIATE_FAILED;
    };
    for _ in 0..SMOKE_BLOCKS {
        for channel in 0..instance.audio_input_count() {
            for sample in instance.input_buffer_mut(channel)[..SMOKE_FRAMES].iter_mut() {
                *sample = 0.1;
            }
        }
        instance.run(SMOKE_FRAMES);
        instance.deliver_responses();
    }
    SMOKE_OK
}

/// Instantiates a VST3 plugin and runs a few blocks of DC input.
pub fn smoke_plugin_vst3(bundle: &str, class_id: &str) -> i32 {
    let host = vibemix_vst3::Host::new();
    let Some(mut instance) = host.instantiate(
        bundle,
        class_id,
        48_000.0,
        SMOKE_FRAMES,
        vibemix_vst3::PluginInstance::max_frames(),
    ) else {
        return SMOKE_INSTANTIATE_FAILED;
    };
    for _ in 0..SMOKE_BLOCKS {
        for channel in 0..instance.audio_input_count() {
            for sample in instance.input_buffer_mut(channel)[..SMOKE_FRAMES].iter_mut() {
                *sample = 0.1;
            }
        }
        instance.run(SMOKE_FRAMES);
    }
    SMOKE_OK
}

/// Creates and tears down a VST3 plugin's editor headlessly, so a crashing
/// editor is caught before the engine opens one for real.
pub fn smoke_plugin_vst3_ui(bundle: &str, class_id: &str) -> i32 {
    let host = vibemix_vst3::Host::new();
    let Some(instance) = host.instantiate(
        bundle,
        class_id,
        48_000.0,
        SMOKE_FRAMES,
        vibemix_vst3::PluginInstance::max_frames(),
    ) else {
        return SMOKE_INSTANTIATE_FAILED;
    };
    if instance.smoke_editor() {
        SMOKE_OK
    } else {
        SMOKE_FAILED
    }
}

/// A plugin to smoke-test.
enum SmokeTarget {
    Lv2 { uri: String, name: String },
    Vst3 {
        bundle: String,
        class_id: String,
        name: String,
    },
}

impl SmokeTarget {
    fn id(&self) -> &str {
        match self {
            SmokeTarget::Lv2 { uri, .. } => uri,
            SmokeTarget::Vst3 { class_id, .. } => class_id,
        }
    }

    fn name(&self) -> &str {
        match self {
            SmokeTarget::Lv2 { name, .. } | SmokeTarget::Vst3 { name, .. } => name,
        }
    }

    fn run(&self, host: &Host) -> i32 {
        match self {
            SmokeTarget::Lv2 { uri, .. } => smoke_plugin(host, uri),
            SmokeTarget::Vst3 {
                bundle, class_id, ..
            } => smoke_plugin_vst3(bundle, class_id),
        }
    }
}

/// Outcome of a smoke test child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildOutcome {
    Ok,
    InstantiateFailed,
    Failed(String),
    Timeout,
}

/// Batch sweep: tests every hostable mono/stereo plugin in an isolated forked
/// child and streams one JSON verdict per line on stdout. `only` restricts the
/// set to the given URIs (`None` tests everything).
pub fn scan_plugins(only: Option<Vec<String>>) -> ! {
    disable_core_dumps();
    // Low priority so the audio thread and UI always win. The parent puts the
    // sweep in its own process group via `Command::process_group`, avoiding a
    // `pre_exec` (which would force a fork in the multithreaded engine).
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }

    let only: Option<std::collections::HashSet<String>> =
        only.map(|list| list.into_iter().collect());
    let in_only = |id: &str| only.as_ref().is_none_or(|set| set.contains(id));

    let host = Host::new();
    let mut targets: Vec<SmokeTarget> = host
        .discover()
        .into_iter()
        .filter(|info| {
            // Main inputs must be mono/stereo and match the outputs; extra
            // inputs are sidechain.
            info.hostable
                && ((info.main_input_count == 2 && info.audio_outputs.len() == 2)
                    || (info.main_input_count == 1 && info.audio_outputs.len() == 1))
                && in_only(&info.uri)
        })
        .map(|info| SmokeTarget::Lv2 {
            uri: info.uri,
            name: info.name,
        })
        .collect();
    for info in vibemix_vst3::Host::new().discover() {
        if in_only(&info.class_id) {
            targets.push(SmokeTarget::Vst3 {
                bundle: info.bundle,
                class_id: info.class_id,
                name: info.name,
            });
        }
    }

    emit(&serde_json::json!({ "scan": "start", "total": targets.len() }));

    let mut failed = 0usize;
    for target in &targets {
        let outcome = scan_one(&host, target);
        if !matches!(outcome, ChildOutcome::Ok) {
            failed += 1;
        }
        let (status, reason) = match &outcome {
            ChildOutcome::Ok => ("ok", String::new()),
            ChildOutcome::InstantiateFailed => ("failed", "instantiate failed".to_string()),
            ChildOutcome::Failed(reason) => ("failed", reason.clone()),
            ChildOutcome::Timeout => ("failed", "timeout".to_string()),
        };
        emit(&serde_json::json!({
            "scan": "plugin",
            "uri": target.id(),
            "name": target.name(),
            "status": status,
            "reason": reason,
        }));
    }

    emit(&serde_json::json!({
        "scan": "done",
        "total": targets.len(),
        "failed": failed,
    }));
    std::process::exit(0);
}

/// Forks, runs the smoke test in the child, and waits with a timeout.
fn scan_one(host: &Host, target: &SmokeTarget) -> ChildOutcome {
    // Flush so the forked child does not duplicate buffered output.
    let _ = std::io::stdout().flush();

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // Child: silence stdout/stderr and run the plugin, then `_exit` to
        // avoid running inherited atexit handlers.
        if let Ok(devnull) = std::fs::File::open("/dev/null") {
            use std::os::fd::AsRawFd;
            let fd = devnull.as_raw_fd();
            unsafe {
                libc::dup2(fd, libc::STDOUT_FILENO);
                libc::dup2(fd, libc::STDERR_FILENO);
            }
        }
        let code = target.run(host);
        unsafe { libc::_exit(if code == SMOKE_OK { 0 } else { 2 }) };
    }
    if pid < 0 {
        return ChildOutcome::Failed("fork failed".to_string());
    }

    let deadline = Instant::now() + SCAN_TIMEOUT;
    let mut status: libc::c_int = 0;
    loop {
        let ret = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if ret == pid {
            break;
        }
        if ret < 0 {
            return ChildOutcome::Failed("waitpid failed".to_string());
        }
        if Instant::now() > deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return ChildOutcome::Timeout;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    if libc::WIFSIGNALED(status) {
        ChildOutcome::Failed(signal_name(libc::WTERMSIG(status)).to_string())
    } else if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            0 => ChildOutcome::Ok,
            2 => ChildOutcome::InstantiateFailed,
            code => ChildOutcome::Failed(format!("exit {code}")),
        }
    } else {
        ChildOutcome::Failed("unknown".to_string())
    }
}

pub fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGSEGV => "crash SIGSEGV",
        libc::SIGABRT => "crash SIGABRT",
        libc::SIGBUS => "crash SIGBUS",
        libc::SIGFPE => "crash SIGFPE",
        libc::SIGILL => "crash SIGILL",
        libc::SIGKILL => "crash SIGKILL",
        libc::SIGSYS => "crash SIGSYS",
        _ => "crash signal",
    }
}

fn emit(value: &serde_json::Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
}
