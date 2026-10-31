#![allow(clippy::too_many_arguments)]

use std::fs;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use cxx_qt_lib::QString;
use vibemix_proto::{message::ChunkAssembler, ControlMessage};

static SOCKET: OnceLock<UnixDatagram> = OnceLock::new();
static ENGINE_PATH: OnceLock<PathBuf> = OnceLock::new();
static VIEW: OnceLock<Mutex<View>> = OnceLock::new();
static PENDING_REPORT: Mutex<Option<String>> = Mutex::new(None);
static PENDING_SESSION: Mutex<Option<(bool, String, String)>> = Mutex::new(None);
static SESSION_PATH: Mutex<String> = Mutex::new(String::new());
static STARTUP_WRITE: Mutex<()> = Mutex::new(());
static SESSION_DIR: Mutex<String> = Mutex::new(String::new());
static SAVED_STATE: Mutex<String> = Mutex::new(String::new());
static RECENT: Mutex<Vec<String>> = Mutex::new(Vec::new());
static DIRTY: AtomicBool = AtomicBool::new(false);
static PENDING_CLEAN: AtomicBool = AtomicBool::new(false);
static SESSION_NONCE: AtomicU64 = AtomicU64::new(0);
static WINDOW_NONCE: AtomicU64 = AtomicU64::new(0);
static LAST_WINDOW: Mutex<Option<(i32, i32, bool)>> = Mutex::new(None);
static PENDING_WINDOW: Mutex<Option<(i32, i32, bool)>> = Mutex::new(None);
static SAMPLE_RATE: AtomicU32 = AtomicU32::new(48000);
/// Milliseconds since `START` at which the last datagram arrived from the
/// engine; the engine sends meters at 50 Hz, so silence means it is gone.
static LAST_RX_MS: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<std::time::Instant> = OnceLock::new();
const ENGINE_SILENCE_MS: u64 = 5000;
static XRUNS: AtomicU32 = AtomicU32::new(0);
/// Audio-callback load in tenths of a percent (average, peak).
static DSP_LOAD: AtomicU32 = AtomicU32::new(0);
static DSP_PEAK: AtomicU32 = AtomicU32::new(0);
static CLIPBOARD: Mutex<(bool, u32, String)> = Mutex::new((false, 0, String::new()));
static EDIT_STATE: Mutex<(u32, u32, String, String)> =
    Mutex::new((0, 0, String::new(), String::new()));
type ClockState = (bool, bool, f32, f32, f32, f32, f32, u32);
static CLOCK: Mutex<ClockState> = Mutex::new((false, false, 120.0, 0.0, 0.0, 4.0, 0.0, 0));

// Revision counters so the 30 Hz poll only clones/re-serialises the large
// payloads (structure, values, catalog, meters) when they actually changed.
static STRUCTURE_REV: AtomicU64 = AtomicU64::new(0);
static VALUES_REV: AtomicU64 = AtomicU64::new(0);
static CATALOG_REV: AtomicU64 = AtomicU64::new(0);
static METERS_REV: AtomicU64 = AtomicU64::new(0);
static LAST_STRUCTURE_REV: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_VALUES_REV: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_CATALOG_REV: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_METERS_REV: AtomicU64 = AtomicU64::new(u64::MAX);
static PORTS_REV: AtomicU64 = AtomicU64::new(0);
static LAST_PORTS_REV: AtomicU64 = AtomicU64::new(u64::MAX);
static PORTS: Mutex<String> = Mutex::new(String::new());
static RECORDER: Mutex<String> = Mutex::new(String::new());
static LOOPS: Mutex<String> = Mutex::new(String::new());
static WAVEFORM: Mutex<String> = Mutex::new(String::new());
static RECORDER_REV: AtomicU64 = AtomicU64::new(0);
static LOOPS_REV: AtomicU64 = AtomicU64::new(0);
static WAVEFORM_REV: AtomicU64 = AtomicU64::new(0);
static LAST_RECORDER_REV: AtomicU64 = AtomicU64::new(0);
static LAST_LOOPS_REV: AtomicU64 = AtomicU64::new(0);
static LAST_WAVEFORM_REV: AtomicU64 = AtomicU64::new(0);
static PENDING_CONNECTION_ERROR: Mutex<Option<String>> = Mutex::new(None);
static LAST_CLOCK: Mutex<Option<ClockState>> = Mutex::new(None);
static LAST_SESSION_PATH: Mutex<String> = Mutex::new(String::new());
static LAST_RECENT: Mutex<String> = Mutex::new(String::new());
/// Whether the engine has sent its plugin catalog yet.
static CATALOG_READY: AtomicBool = AtomicBool::new(false);
/// Plugin health sweep progress: `(running, done, total)`.
static SCAN: Mutex<(bool, u32, u32)> = Mutex::new((false, 0, 0));

struct View {
    structure_json: String,
    values_json: String,
    catalog_json: String,
    strip_ids: Vec<u32>,
    bus_ids: Vec<u32>,
    strip_in_l: Vec<f32>,
    strip_in_r: Vec<f32>,
    strip_gain_l: Vec<f32>,
    strip_gain_r: Vec<f32>,
    strip_out_l: Vec<f32>,
    strip_out_r: Vec<f32>,
    bus_in_l: Vec<f32>,
    bus_in_r: Vec<f32>,
    bus_gain_l: Vec<f32>,
    bus_gain_r: Vec<f32>,
    bus_ret_l: Vec<f32>,
    bus_ret_r: Vec<f32>,
    bus_l: Vec<f32>,
    bus_r: Vec<f32>,
    connected: bool,
}

/// Locks the shared [`View`], recovering from a poisoned mutex. `View` is plain
/// data, so a panic while it was locked leaves it usable; refusing to lock
/// again (the default `expect` behaviour) would wedge the whole GUI.
fn view_lock() -> std::sync::MutexGuard<'static, View> {
    view()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn view() -> &'static Mutex<View> {
    VIEW.get_or_init(|| {
        Mutex::new(View {
            structure_json: "{\"strips\":[],\"buses\":[]}".to_string(),
            values_json: "{\"strips\":{},\"buses\":{},\"params\":{}}".to_string(),
            catalog_json: "[]".to_string(),
            strip_ids: Vec::new(),
            bus_ids: Vec::new(),
            strip_in_l: Vec::new(),
            strip_in_r: Vec::new(),
            strip_gain_l: Vec::new(),
            strip_gain_r: Vec::new(),
            strip_out_l: Vec::new(),
            strip_out_r: Vec::new(),
            bus_in_l: Vec::new(),
            bus_in_r: Vec::new(),
            bus_gain_l: Vec::new(),
            bus_gain_r: Vec::new(),
            bus_ret_l: Vec::new(),
            bus_ret_r: Vec::new(),
            bus_l: Vec::new(),
            bus_r: Vec::new(),
            connected: false,
        })
    })
}

fn note_action(action: &str) {
    tracing::debug!(action, "action");
}

const MAX_RECENT: usize = 10;

fn config_dir() -> PathBuf {
    vibemix_core::config::config_dir()
}

fn load_recent() {
    let Ok(text) = fs::read_to_string(config_dir().join("recent.json")) else {
        return;
    };
    if let Ok(list) = serde_json::from_str::<Vec<String>>(&text) {
        if let Ok(mut guard) = RECENT.lock() {
            *guard = list;
        }
    }
}

fn save_recent() {
    let list = RECENT.lock().map(|guard| guard.clone()).unwrap_or_default();
    if let Ok(text) = serde_json::to_string_pretty(&list) {
        let dir = config_dir();
        let _ = fs::create_dir_all(&dir);
        let _ = vibemix_core::config::write_atomic(&dir.join("recent.json"), text);
    }
}

fn note_recent(path: &str) {
    if path.is_empty() {
        return;
    }
    if let Ok(mut guard) = RECENT.lock() {
        guard.retain(|entry| entry != path);
        guard.insert(0, path.to_string());
        guard.truncate(MAX_RECENT);
    }
    save_recent();
}

/// Read-modify-write the global launch preferences. The engine reads the same
/// file at startup, and the GUI only writes it when a session is loaded/saved
/// or the startup toggle changes.
fn update_startup_settings(update: impl FnOnce(&mut vibemix_core::config::StartupSettings)) {
    let _guard = STARTUP_WRITE.lock();
    let mut settings = vibemix_core::config::load_startup_settings();
    update(&mut settings);
    if let Err(error) = vibemix_core::config::save_startup_settings(&settings) {
        tracing::warn!(%error, "failed to save startup settings");
    }
}

pub fn init(engine_socket: PathBuf) -> Result<(), String> {
    // Our own socket lives next to the engine's, in the same private directory.
    let directory = engine_socket
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(vibemix_proto::socket::runtime_dir);
    let (socket, own_path) =
        vibemix_proto::socket::bind_client(&directory, "gui").map_err(|error| {
            format!(
                "could not open the control socket in {}: {error}",
                directory.display()
            )
        })?;
    vibemix_proto::socket::remove_at_exit(own_path);
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| format!("could not set the socket timeout: {error}"))?;
    let _ = ENGINE_PATH.set(engine_socket);

    let receiver = socket
        .try_clone()
        .map_err(|error| format!("could not clone the control socket: {error}"))?;
    let _ = SOCKET.set(socket);

    load_recent();
    let sessions = config_dir().join("sessions");
    if fs::create_dir_all(&sessions).is_ok() {
        if let Ok(mut guard) = SESSION_DIR.lock() {
            *guard = sessions.to_string_lossy().to_string();
        }
    }
    std::thread::spawn(move || receive_loop(receiver));
    send(ControlMessage::QueryState);
    Ok(())
}

fn send(message: ControlMessage) {
    if let (Some(socket), Some(path)) = (SOCKET.get(), ENGINE_PATH.get()) {
        if let Ok(bytes) = message.encode() {
            // Never blocks; a missing engine or a full queue just drops it.
            let _ = vibemix_proto::socket::send_nonblocking(socket, &bytes, path);
        }
    }
}

fn receive_loop(socket: UnixDatagram) {
    let mut buffer = [0u8; 65536];
    let mut assembler = ChunkAssembler::new();
    loop {
        if let Ok((length, _)) = socket.recv_from(&mut buffer) {
            LAST_RX_MS.store(now_ms(), Ordering::Relaxed);
            // Reassemble chunked messages (e.g. a large plugin catalog) before
            // dispatching; non-chunk messages pass straight through.
            let assembled = ControlMessage::decode(&buffer[..length])
                .and_then(|message| assembler.accept(message));
            match assembled {
                Some(ControlMessage::State { json }) => {
                    // Parse the snapshot once and feed every consumer from it.
                    let value: serde_json::Value =
                        serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                    let (structure, values) = split_state(&value);
                    let (strip_ids, bus_ids) = extract_ids(&value);
                    let combined = format!("{structure}\n{values}");
                    let mut guard = view_lock();
                    guard.strip_ids = strip_ids;
                    guard.bus_ids = bus_ids;
                    guard.structure_json = structure;
                    guard.values_json = values;
                    guard.connected = true;
                    drop(guard);
                    STRUCTURE_REV.fetch_add(1, Ordering::Release);
                    VALUES_REV.fetch_add(1, Ordering::Release);

                    if PENDING_CLEAN.swap(false, Ordering::SeqCst) {
                        if let Ok(mut saved) = SAVED_STATE.lock() {
                            *saved = combined;
                        }
                        DIRTY.store(false, Ordering::SeqCst);
                    } else if let Ok(mut saved) = SAVED_STATE.lock() {
                        if saved.is_empty() {
                            *saved = combined;
                            DIRTY.store(false, Ordering::SeqCst);
                        } else {
                            DIRTY.store(*saved != combined, Ordering::SeqCst);
                        }
                    }

                    if let Some(window) = window_from_state(&value) {
                        let changed = LAST_WINDOW
                            .lock()
                            .map(|last| *last != Some(window))
                            .unwrap_or(true);
                        if changed {
                            if let Ok(mut last) = LAST_WINDOW.lock() {
                                *last = Some(window);
                            }
                            if let Ok(mut pending) = PENDING_WINDOW.lock() {
                                *pending = Some(window);
                            }
                        }
                    }

                    if let Some(rate) = sample_rate_from_state(&value) {
                        SAMPLE_RATE.store(rate, Ordering::SeqCst);
                    }
                    if let Some(xruns) = xruns_from_state(&value) {
                        XRUNS.store(xruns, Ordering::SeqCst);
                    }
                    if let Some((load, peak)) = dsp_from_state(&value) {
                        DSP_LOAD.store(load, Ordering::SeqCst);
                        DSP_PEAK.store(peak, Ordering::SeqCst);
                    }
                }
                Some(ControlMessage::SessionResult { ok, path, error }) => {
                    // Force the next snapshot to (re)apply the session's window.
                    if let Ok(mut last) = LAST_WINDOW.lock() {
                        *last = None;
                    }
                    if ok {
                        PENDING_CLEAN.store(true, Ordering::SeqCst);
                        if !path.is_empty() {
                            if let Ok(mut current) = SESSION_PATH.lock() {
                                *current = path.clone();
                            }
                            note_recent(&path);
                            update_startup_settings(|settings| {
                                settings.last_session = path.clone();
                            });
                        }
                    } else {
                        DIRTY.store(true, Ordering::SeqCst);
                    }
                    if let Ok(mut guard) = PENDING_SESSION.lock() {
                        *guard = Some((ok, path, error));
                    }
                }
                Some(ControlMessage::Catalog { json }) => {
                    let mut guard = view_lock();
                    guard.catalog_json = json;
                    guard.connected = true;
                    drop(guard);
                    CATALOG_READY.store(true, Ordering::Release);
                    CATALOG_REV.fetch_add(1, Ordering::Release);
                }
                Some(ControlMessage::PluginScan {
                    running,
                    done,
                    total,
                }) => {
                    if let Ok(mut guard) = SCAN.lock() {
                        *guard = (running, done, total);
                    }
                }
                Some(ControlMessage::MissingConnections { json }) => {
                    if let Ok(mut guard) = PENDING_REPORT.lock() {
                        *guard = Some(json);
                    }
                }
                Some(ControlMessage::Clipboard {
                    has,
                    location,
                    name,
                }) => {
                    if let Ok(mut guard) = CLIPBOARD.lock() {
                        *guard = (has, location, name);
                    }
                }
                Some(ControlMessage::EditState {
                    undo_depth,
                    redo_depth,
                    undo_label,
                    redo_label,
                }) => {
                    if let Ok(mut guard) = EDIT_STATE.lock() {
                        *guard = (undo_depth, redo_depth, undo_label, redo_label);
                    }
                }
                Some(ControlMessage::Clock {
                    locked,
                    playing,
                    bpm,
                    beats,
                    seconds,
                    beats_per_bar,
                    jitter_bpm,
                    glitches,
                }) => {
                    if let Ok(mut guard) = CLOCK.lock() {
                        *guard = (
                            locked,
                            playing,
                            bpm,
                            beats,
                            seconds,
                            beats_per_bar,
                            jitter_bpm,
                            glitches,
                        );
                    }
                }
                Some(ControlMessage::Meters(data)) => {
                    let mut guard = view_lock();
                    guard.strip_in_l = data.strips_in_l;
                    guard.strip_in_r = data.strips_in_r;
                    guard.strip_gain_l = data.strips_gain_l;
                    guard.strip_gain_r = data.strips_gain_r;
                    guard.strip_out_l = data.strips_out_l;
                    guard.strip_out_r = data.strips_out_r;
                    guard.bus_in_l = data.buses_in_l;
                    guard.bus_in_r = data.buses_in_r;
                    guard.bus_gain_l = data.buses_gain_l;
                    guard.bus_gain_r = data.buses_gain_r;
                    guard.bus_ret_l = data.buses_ret_l;
                    guard.bus_ret_r = data.buses_ret_r;
                    guard.bus_l = data.buses_l;
                    guard.bus_r = data.buses_r;
                    guard.connected = true;
                    drop(guard);
                    METERS_REV.fetch_add(1, Ordering::Release);
                }
                Some(ControlMessage::Ports { json }) => {
                    if let Ok(mut guard) = PORTS.lock() {
                        *guard = json;
                    }
                    PORTS_REV.fetch_add(1, Ordering::Release);
                }
                Some(ControlMessage::RecorderStatus { json }) => {
                    if let Ok(mut guard) = RECORDER.lock() {
                        *guard = json;
                    }
                    RECORDER_REV.fetch_add(1, Ordering::Release);
                }
                Some(ControlMessage::LoopList { json }) => {
                    if let Ok(mut guard) = LOOPS.lock() {
                        *guard = json;
                    }
                    LOOPS_REV.fetch_add(1, Ordering::Release);
                }
                Some(ControlMessage::LoopWaveform { json }) => {
                    if let Ok(mut guard) = WAVEFORM.lock() {
                        *guard = json;
                    }
                    WAVEFORM_REV.fetch_add(1, Ordering::Release);
                }
                Some(ControlMessage::ConnectResult { ok, error, .. }) => {
                    if let Ok(mut guard) = PENDING_CONNECTION_ERROR.lock() {
                        *guard = Some(if ok { String::new() } else { error });
                    }
                }
                _ => {}
            }
        }
    }
}

fn build_snapshot_json() -> String {
    view_lock().structure_json.clone()
}

fn build_values_json() -> String {
    view_lock().values_json.clone()
}

fn build_catalog_json() -> String {
    view_lock().catalog_json.clone()
}

fn build_meters_json() -> String {
    let guard = view_lock();

    let sample = |values: &[f32], index: usize| values.get(index).copied().unwrap_or(0.0);

    let mut strips = serde_json::Map::new();
    for id in &guard.strip_ids {
        let index = *id as usize;
        strips.insert(
            id.to_string(),
            serde_json::json!({
                "in": [
                    sample(&guard.strip_in_l, index),
                    sample(&guard.strip_in_r, index),
                ],
                "gain": [
                    sample(&guard.strip_gain_l, index),
                    sample(&guard.strip_gain_r, index),
                ],
                "out": [
                    sample(&guard.strip_out_l, index),
                    sample(&guard.strip_out_r, index),
                ],
            }),
        );
    }

    let mut buses = serde_json::Map::new();
    for id in &guard.bus_ids {
        let index = *id as usize;
        buses.insert(
            id.to_string(),
            serde_json::json!({
                "in": [
                    sample(&guard.bus_in_l, index),
                    sample(&guard.bus_in_r, index),
                ],
                "send": [
                    sample(&guard.bus_gain_l, index),
                    sample(&guard.bus_gain_r, index),
                ],
                "ret": [
                    sample(&guard.bus_ret_l, index),
                    sample(&guard.bus_ret_r, index),
                ],
                "out": [
                    sample(&guard.bus_l, index),
                    sample(&guard.bus_r, index),
                ],
            }),
        );
    }

    serde_json::json!({ "strips": strips, "buses": buses }).to_string()
}

fn now_ms() -> u64 {
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + 1
}

/// Connected means the engine has spoken and has not gone quiet since.
fn is_connected() -> bool {
    let heard = LAST_RX_MS.load(Ordering::Relaxed);
    view_lock().connected && heard != 0 && now_ms().saturating_sub(heard) < ENGINE_SILENCE_MS
}

fn extract_ids(value: &serde_json::Value) -> (Vec<u32>, Vec<u32>) {
    let collect = |key: &str| -> Vec<u32> {
        value
            .get(key)
            .and_then(|entry| entry.as_array())
            .map(|array| {
                array
                    .iter()
                    .filter_map(|entry| entry.get("id").and_then(|id| id.as_u64()))
                    .map(|id| id as u32)
                    .collect()
            })
            .unwrap_or_default()
    };

    (collect("strips"), collect("buses"))
}

fn window_from_state(value: &serde_json::Value) -> Option<(i32, i32, bool)> {
    let window = value.get("window")?;
    if window.is_null() {
        return None;
    }
    let width = window.get("width").and_then(serde_json::Value::as_i64)? as i32;
    let height = window.get("height").and_then(serde_json::Value::as_i64)? as i32;
    let maximized = window
        .get("maximized")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if width <= 0 || height <= 0 {
        return None;
    }
    Some((width, height, maximized))
}

fn sample_rate_from_state(value: &serde_json::Value) -> Option<u32> {
    let rate = value.get("sample_rate")?.as_u64()?;
    if rate == 0 {
        return None;
    }
    Some(rate as u32)
}

/// DSP load `(average, peak)` in tenths of a percent from a state snapshot.
fn dsp_from_state(value: &serde_json::Value) -> Option<(u32, u32)> {
    let tenths = |key: &str| -> Option<u32> {
        let fraction = value.get(key)?.as_f64()?;
        Some((fraction.clamp(0.0, 10.0) * 1000.0).round() as u32)
    };
    Some((tenths("dsp_load")?, tenths("dsp_peak")?))
}

fn xruns_from_state(value: &serde_json::Value) -> Option<u32> {
    let xruns = value.get("xruns")?.as_u64()?;
    Some(xruns.min(u32::MAX as u64) as u32)
}

fn build_plugins(
    plugins: Option<&Vec<serde_json::Value>>,
    location: u64,
    target: u64,
    params_values: &mut serde_json::Map<String, serde_json::Value>,
    sidechain_values: &mut serde_json::Map<String, serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut structure = Vec::new();
    let Some(plugins) = plugins else {
        return structure;
    };
    for plugin in plugins {
        let slot = plugin
            .get("slot")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let mut controls_structure = Vec::new();
        let mut controls_values = Vec::new();
        let mut sidechain = serde_json::Map::new();
        if let Some(sources) = plugin
            .get("sidechain")
            .and_then(serde_json::Value::as_array)
        {
            for source in sources {
                if let Some(strip) = source.get("strip").and_then(serde_json::Value::as_u64) {
                    sidechain.insert(
                        strip.to_string(),
                        serde_json::json!({
                            "gain": source.get("gain").cloned().unwrap_or(serde_json::json!(0.0)),
                            "pre_fader": source
                                .get("pre_fader")
                                .cloned()
                                .unwrap_or(serde_json::json!(true)),
                        }),
                    );
                }
            }
        }

        if let Some(controls) = plugin.get("controls").and_then(serde_json::Value::as_array) {
            for control in controls {
                controls_structure.push(serde_json::json!({
                    "name": control.get("name").cloned().unwrap_or(serde_json::json!("")),
                    "min": control.get("min").cloned().unwrap_or(serde_json::json!(0.0)),
                    "max": control.get("max").cloned().unwrap_or(serde_json::json!(1.0)),
                    "toggled": control.get("toggled").cloned().unwrap_or(serde_json::json!(false)),
                    "integer": control.get("integer").cloned().unwrap_or(serde_json::json!(false)),
                    "scale_points": control.get("scale_points").cloned().unwrap_or(serde_json::json!([])),
                }));
                controls_values.push(
                    control
                        .get("value")
                        .cloned()
                        .unwrap_or(serde_json::json!(0.0)),
                );
            }
        }

        structure.push(serde_json::json!({
            "slot": slot,
            "uri": plugin.get("uri").cloned().unwrap_or(serde_json::json!("")),
            "name": plugin.get("name").cloned().unwrap_or(serde_json::json!("")),
            "bypassed": plugin.get("bypassed").cloned().unwrap_or(serde_json::json!(false)),
            "missing": plugin.get("missing").cloned().unwrap_or(serde_json::json!(false)),
            "has_ui": plugin.get("has_ui").cloned().unwrap_or(serde_json::json!(false)),
            "has_sidechain": plugin.get("has_sidechain").cloned().unwrap_or(serde_json::json!(false)),
            "sidechain_enabled": plugin.get("sidechain_enabled").cloned().unwrap_or(serde_json::json!(false)),
            "midi_channel": plugin.get("midi_channel").cloned().unwrap_or(serde_json::Value::Null),
            "controls": controls_structure,
        }));
        let key = format!("{location}-{target}-{slot}");
        params_values.insert(key.clone(), serde_json::Value::Array(controls_values));
        sidechain_values.insert(key, serde_json::Value::Object(sidechain));
    }
    structure
}

fn split_state(value: &serde_json::Value) -> (String, String) {
    let mut strips_structure = Vec::new();
    let mut strips_values = serde_json::Map::new();
    let mut params_values = serde_json::Map::new();
    let mut sidechain_values = serde_json::Map::new();

    if let Some(strips) = value.get("strips").and_then(serde_json::Value::as_array) {
        for strip in strips {
            let id = strip
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let plugins_structure = build_plugins(
                strip.get("plugins").and_then(serde_json::Value::as_array),
                0,
                id,
                &mut params_values,
                &mut sidechain_values,
            );
            let plugins_post_structure = build_plugins(
                strip
                    .get("plugins_post")
                    .and_then(serde_json::Value::as_array),
                1,
                id,
                &mut params_values,
                &mut sidechain_values,
            );
            // Send levels change continuously, so keep them in `values` (which
            // does not reset the strip/bus repeaters) rather than in the
            // structural snapshot.
            let mut send_delays = serde_json::Map::new();
            let mut send_pre = serde_json::Map::new();
            let sends: serde_json::Value = strip
                .get("sends")
                .and_then(serde_json::Value::as_array)
                .map(|array| {
                    let mut map = serde_json::Map::new();
                    for send in array {
                        if let Some(bus) = send.get("bus").and_then(serde_json::Value::as_u64) {
                            let gain = send.get("gain").cloned().unwrap_or(serde_json::json!(0.0));
                            map.insert(bus.to_string(), gain);
                            let delay = send
                                .get("delay_ms")
                                .cloned()
                                .unwrap_or(serde_json::json!(0.0));
                            send_delays.insert(bus.to_string(), delay);
                            let pre = send
                                .get("pre_fader")
                                .cloned()
                                .unwrap_or(serde_json::json!(false));
                            send_pre.insert(bus.to_string(), pre);
                        }
                    }
                    serde_json::Value::Object(map)
                })
                .unwrap_or_else(|| serde_json::json!({}));

            strips_structure.push(serde_json::json!({
                "id": id,
                "name": strip.get("name").cloned().unwrap_or(serde_json::json!("")),
                "midi_channel": strip.get("midi_channel").cloned().unwrap_or(serde_json::Value::Null),
                "plugins": plugins_structure,
                "plugins_post": plugins_post_structure,
            }));
            strips_values.insert(
                id.to_string(),
                serde_json::json!({
                    "gain": strip.get("gain").cloned().unwrap_or(serde_json::json!(1.0)),
                    "pan": strip.get("pan").cloned().unwrap_or(serde_json::json!(0.0)),
                    "input_gain_db": strip.get("input_gain_db").cloned().unwrap_or(serde_json::json!(0.0)),
                    "mute": strip.get("mute").cloned().unwrap_or(serde_json::json!(false)),
                    "solo": strip.get("solo").cloned().unwrap_or(serde_json::json!(false)),
                    "sends": sends,
                    "send_delays": serde_json::Value::Object(send_delays),
                    "send_pre": serde_json::Value::Object(send_pre),
                }),
            );
        }
    }

    let mut buses_structure = Vec::new();
    let mut buses_values = serde_json::Map::new();
    if let Some(buses) = value.get("buses").and_then(serde_json::Value::as_array) {
        for bus in buses {
            let id = bus
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let plugins_structure = build_plugins(
                bus.get("plugins").and_then(serde_json::Value::as_array),
                2,
                id,
                &mut params_values,
                &mut sidechain_values,
            );
            buses_structure.push(serde_json::json!({
                "id": id,
                "name": bus.get("name").cloned().unwrap_or(serde_json::json!("")),
                "midi_channel": bus.get("midi_channel").cloned().unwrap_or(serde_json::Value::Null),
                "mix_to_master": bus.get("mix_to_master").cloned().unwrap_or(serde_json::json!(false)),
                "return_inputs": bus.get("return_inputs").cloned().unwrap_or(serde_json::json!(false)),
                "plugins": plugins_structure,
            }));
            buses_values.insert(
                id.to_string(),
                serde_json::json!({
                    "gain": bus.get("gain").cloned().unwrap_or(serde_json::json!(1.0)),
                    "mute": bus.get("mute").cloned().unwrap_or(serde_json::json!(false)),
                    "send_gain": bus.get("send_gain").cloned().unwrap_or(serde_json::json!(1.0)),
                    "return_gain": bus.get("return_gain").cloned().unwrap_or(serde_json::json!(1.0)),
                }),
            );
        }
    }

    // Low-frequency, structural metadata the QML reads from `mix`.
    let structure = serde_json::json!({
        "strips": strips_structure,
        "buses": buses_structure,
        "beats_per_bar": value.get("beats_per_bar").cloned().unwrap_or(serde_json::json!(4.0)),
        "midi_bindings": value.get("midi_bindings").cloned().unwrap_or(serde_json::json!([])),
        "midi_sync": value.get("midi_sync").cloned().unwrap_or(serde_json::json!(true)),
        "midi_connections": value.get("midi_connections").cloned().unwrap_or(serde_json::Value::Null),
        "midi_out_connections": value.get("midi_out_connections").cloned().unwrap_or(serde_json::Value::Null),
        "transport_ui": value.get("transport_ui").cloned().unwrap_or(serde_json::json!({})),
        "clock": value.get("clock").cloned().unwrap_or(serde_json::json!({})),
        "recorder": value.get("recorder").cloned().unwrap_or(serde_json::json!({})),
        "layout": value
            .get("layout")
            .cloned()
            .unwrap_or(serde_json::json!({"strips_per_row": 0, "buses_per_row": 0})),
        "strip_capacity": value.get("strip_capacity").cloned().unwrap_or(serde_json::json!(0)),
        "bus_capacity": value.get("bus_capacity").cloned().unwrap_or(serde_json::json!(0)),
    })
    .to_string();
    let values = serde_json::json!({
        "strips": strips_values,
        "buses": buses_values,
        "params": params_values,
        "sidechain": sidechain_values,
    })
    .to_string();
    (structure, values)
}

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(bool, connected)]
        #[qproperty(QString, snapshot)]
        #[qproperty(QString, values)]
        #[qproperty(QString, meters)]
        #[qproperty(QString, catalog)]
        #[qproperty(QString, report)]
        #[qproperty(QString, ports)]
        #[qproperty(QString, connection_error)]
        #[qproperty(bool, dirty)]
        #[qproperty(bool, load_last_session)]
        #[qproperty(QString, session_result)]
        #[qproperty(QString, session_path)]
        #[qproperty(QString, session_dir)]
        #[qproperty(QString, recent_files)]
        #[qproperty(QString, window_geometry)]
        #[qproperty(i32, sample_rate)]
        #[qproperty(i32, xruns)]
        #[qproperty(i32, dsp_load)]
        #[qproperty(i32, dsp_peak)]
        #[qproperty(bool, has_clipboard)]
        #[qproperty(i32, clipboard_location)]
        #[qproperty(QString, clipboard_name)]
        #[qproperty(i32, undo_depth)]
        #[qproperty(i32, redo_depth)]
        #[qproperty(QString, undo_label)]
        #[qproperty(QString, redo_label)]
        #[qproperty(QString, clock)]
        #[qproperty(QString, recorder)]
        #[qproperty(QString, loops)]
        #[qproperty(QString, waveform)]
        #[qproperty(bool, catalog_ready)]
        #[qproperty(bool, scan_running)]
        #[qproperty(i32, scan_done)]
        #[qproperty(i32, scan_total)]
        #[namespace = "vibemix"]
        type Backend = super::BackendRust;

        #[qinvokable]
        #[cxx_name = "poll"]
        fn poll(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "refresh"]
        fn refresh(&self);

        #[qinvokable]
        #[cxx_name = "rescanPlugins"]
        fn rescan_plugins(&self);

        #[qinvokable]
        #[cxx_name = "retestPlugins"]
        fn retest_plugins(&self);

        #[qinvokable]
        #[cxx_name = "queryPorts"]
        fn query_ports(&self);

        #[qinvokable]
        #[cxx_name = "connectPorts"]
        fn connect_ports(&self, source: &QString, sink: &QString);

        #[qinvokable]
        #[cxx_name = "disconnectPorts"]
        fn disconnect_ports(&self, source: &QString, sink: &QString);

        #[qinvokable]
        #[cxx_name = "cancelPluginScan"]
        fn cancel_plugin_scan(&self);

        #[qinvokable]
        #[cxx_name = "setStripGain"]
        fn update_strip_gain(&self, strip: i32, gain: f32);

        #[qinvokable]
        #[cxx_name = "setStripMute"]
        fn update_strip_mute(&self, strip: i32, mute: bool);

        #[qinvokable]
        #[cxx_name = "setStripSolo"]
        fn update_strip_solo(&self, strip: i32, solo: bool);

        #[qinvokable]
        #[cxx_name = "setStripPan"]
        fn update_strip_pan(&self, strip: i32, pan: f32);

        #[qinvokable]
        #[cxx_name = "setStripInputGain"]
        fn update_strip_input_gain(&self, strip: i32, db: f32);

        #[qinvokable]
        #[cxx_name = "setStripName"]
        fn update_strip_name(&self, strip: i32, name: &QString);

        #[qinvokable]
        #[cxx_name = "moveStrip"]
        fn update_move_strip(&self, strip: i32, to_index: i32);

        #[qinvokable]
        #[cxx_name = "setBusGain"]
        fn update_bus_gain(&self, bus: i32, gain: f32);

        #[qinvokable]
        #[cxx_name = "setBusMute"]
        fn update_bus_mute(&self, bus: i32, mute: bool);

        #[qinvokable]
        #[cxx_name = "setBusName"]
        fn update_bus_name(&self, bus: i32, name: &QString);

        #[qinvokable]
        #[cxx_name = "moveBus"]
        fn update_move_bus(&self, bus: i32, to_index: i32);

        #[qinvokable]
        #[cxx_name = "setBusMixMaster"]
        fn update_bus_mix_master(&self, bus: i32, on: bool);

        #[qinvokable]
        #[cxx_name = "setBusReturn"]
        fn update_bus_return(&self, bus: i32, on: bool);

        #[qinvokable]
        #[cxx_name = "setBusSendGain"]
        fn update_bus_send_gain(&self, bus: i32, gain: f32);

        #[qinvokable]
        #[cxx_name = "setBusReturnGain"]
        fn update_bus_return_gain(&self, bus: i32, gain: f32);

        #[qinvokable]
        #[cxx_name = "setSendGain"]
        fn update_send_gain(&self, strip: i32, bus: i32, gain: f32);

        #[qinvokable]
        #[cxx_name = "setSendDelay"]
        fn update_send_delay(&self, strip: i32, bus: i32, ms: f32);

        #[qinvokable]
        #[cxx_name = "setSendPreFader"]
        fn update_send_pre_fader(&self, strip: i32, bus: i32, pre: bool);

        #[qinvokable]
        #[cxx_name = "addStrip"]
        fn add_strip(&self);

        #[qinvokable]
        #[cxx_name = "addBus"]
        fn add_bus(&self);

        #[qinvokable]
        #[cxx_name = "removeStrip"]
        fn remove_strip(&self, strip: i32);

        #[qinvokable]
        #[cxx_name = "removeBus"]
        fn remove_bus(&self, bus: i32);

        #[qinvokable]
        #[cxx_name = "addPlugin"]
        fn add_plugin(&self, location: i32, target: i32, uri: &QString);

        #[qinvokable]
        #[cxx_name = "removePlugin"]
        fn remove_plugin(&self, location: i32, target: i32, slot: i32);

        #[qinvokable]
        #[cxx_name = "movePlugin"]
        fn move_plugin(&self, location: i32, target: i32, from: i32, to: i32);

        #[qinvokable]
        #[cxx_name = "setPluginParam"]
        fn update_plugin_param(
            &self,
            location: i32,
            target: i32,
            slot: i32,
            param: i32,
            value: f32,
        );

        #[qinvokable]
        #[cxx_name = "setPluginBypass"]
        fn update_plugin_bypass(&self, location: i32, target: i32, slot: i32, bypassed: bool);

        #[qinvokable]
        #[cxx_name = "setPluginSidechainEnabled"]
        fn update_plugin_sidechain_enabled(
            &self,
            location: i32,
            target: i32,
            slot: i32,
            enabled: bool,
        );

        #[qinvokable]
        #[cxx_name = "setSidechainSend"]
        fn update_plugin_sidechain(
            &self,
            location: i32,
            target: i32,
            slot: i32,
            strip: i32,
            gain: f32,
        );

        #[qinvokable]
        #[cxx_name = "setSidechainPreFader"]
        fn update_plugin_sidechain_pre(
            &self,
            location: i32,
            target: i32,
            slot: i32,
            strip: i32,
            pre: bool,
        );

        #[qinvokable]
        #[cxx_name = "openPluginUi"]
        fn open_plugin_ui(&self, location: i32, target: i32, slot: i32);

        #[qinvokable]
        #[cxx_name = "closePluginUi"]
        fn close_plugin_ui(&self, location: i32, target: i32, slot: i32);

        #[qinvokable]
        #[cxx_name = "copyPlugin"]
        fn copy_plugin(&self, location: i32, target: i32, slot: i32);

        #[qinvokable]
        #[cxx_name = "cutPlugin"]
        fn cut_plugin(&self, location: i32, target: i32, slot: i32);

        #[qinvokable]
        #[cxx_name = "pastePlugin"]
        fn paste_plugin(&self, location: i32, target: i32);

        #[qinvokable]
        #[cxx_name = "duplicateStrip"]
        fn duplicate_strip(&self, strip: i32);

        #[qinvokable]
        #[cxx_name = "duplicateBus"]
        fn duplicate_bus(&self, bus: i32);

        #[qinvokable]
        #[cxx_name = "undo"]
        fn undo(&self);

        #[qinvokable]
        #[cxx_name = "redo"]
        fn redo(&self);

        #[qinvokable]
        #[cxx_name = "setTransportUi"]
        fn set_transport_ui(&self, json: &QString);

        #[qinvokable]
        #[cxx_name = "setRecorderSources"]
        fn set_recorder_sources(&self, json: &QString);

        #[qinvokable]
        #[cxx_name = "setRecorderSettings"]
        fn set_recorder_settings(&self, json: &QString);

        #[qinvokable]
        #[cxx_name = "recorderAction"]
        fn recorder_action(&self, json: &QString);

        #[qinvokable]
        #[cxx_name = "setLayout"]
        fn set_layout(&self, strips_per_row: i32, buses_per_row: i32);

        #[qinvokable]
        #[cxx_name = "setBeatsPerBar"]
        fn set_beats_per_bar(&self, value: f32);

        #[qinvokable]
        #[cxx_name = "midiLearn"]
        fn midi_learn(
            &self,
            kind: i32,
            location: i32,
            target: i32,
            slot: i32,
            param: i32,
            min: f32,
            max: f32,
        );

        #[qinvokable]
        #[cxx_name = "midiLearnCancel"]
        fn midi_learn_cancel(&self);

        #[qinvokable]
        #[cxx_name = "midiBindingsClear"]
        fn midi_bindings_clear(&self);

        #[qinvokable]
        #[cxx_name = "setMidiSync"]
        fn set_midi_sync(&self, on: bool);

        #[qinvokable]
        #[cxx_name = "setClockMaster"]
        fn set_clock_master(&self, on: bool);

        #[qinvokable]
        #[cxx_name = "setClockBpm"]
        fn set_clock_bpm(&self, bpm: f32);

        #[qinvokable]
        #[cxx_name = "setClockPlaying"]
        fn set_clock_playing(&self, playing: bool);

        #[qinvokable]
        #[cxx_name = "setClockTransport"]
        fn set_clock_transport(&self, send: bool, receive: bool);

        #[qinvokable]
        #[cxx_name = "setLoadLastSessionOnStartup"]
        fn set_load_last_session_on_startup(self: Pin<&mut Self>, on: bool);

        #[qinvokable]
        #[cxx_name = "setStripMidiChannel"]
        fn set_strip_midi_channel(&self, strip: i32, channel: i32);

        #[qinvokable]
        #[cxx_name = "setBusMidiChannel"]
        fn set_bus_midi_channel(&self, bus: i32, channel: i32);

        #[qinvokable]
        #[cxx_name = "setPluginMidiChannel"]
        fn set_plugin_midi_channel(&self, location: i32, target: i32, slot: i32, channel: i32);

        #[qinvokable]
        #[cxx_name = "midiBind"]
        fn midi_bind(
            &self,
            kind: i32,
            location: i32,
            target: i32,
            slot: i32,
            param: i32,
            min: f32,
            max: f32,
            channel: i32,
            cc: i32,
        );

        #[qinvokable]
        #[cxx_name = "midiUnbind"]
        fn midi_unbind(&self, kind: i32, location: i32, target: i32, slot: i32, param: i32);

        #[qinvokable]
        #[cxx_name = "keyBind"]
        fn key_bind(
            &self,
            kind: i32,
            location: i32,
            target: i32,
            slot: i32,
            param: i32,
            key: &QString,
        );

        #[qinvokable]
        #[cxx_name = "keyTrigger"]
        fn key_trigger(&self, key: &QString);

        #[qinvokable]
        #[cxx_name = "saveSession"]
        fn save_session(&self, path: &QString, width: i32, height: i32, maximized: bool);

        #[qinvokable]
        #[cxx_name = "loadSession"]
        fn load_session(&self, path: &QString);

        #[qinvokable]
        #[cxx_name = "newSession"]
        fn new_session(&self);
    }
}

#[derive(Debug)]
pub struct BackendRust {
    connected: bool,
    snapshot: QString,
    values: QString,
    meters: QString,
    catalog: QString,
    report: QString,
    ports: QString,
    connection_error: QString,
    dirty: bool,
    load_last_session: bool,
    session_result: QString,
    session_path: QString,
    session_dir: QString,
    recent_files: QString,
    window_geometry: QString,
    sample_rate: i32,
    xruns: i32,
    dsp_load: i32,
    dsp_peak: i32,
    has_clipboard: bool,
    clipboard_location: i32,
    clipboard_name: QString,
    undo_depth: i32,
    redo_depth: i32,
    undo_label: QString,
    redo_label: QString,
    clock: QString,
    recorder: QString,
    loops: QString,
    waveform: QString,
    catalog_ready: bool,
    scan_running: bool,
    scan_done: i32,
    scan_total: i32,
}

impl Default for BackendRust {
    fn default() -> Self {
        Self {
            connected: false,
            snapshot: QString::from("{\"strips\":[],\"buses\":[]}"),
            values: QString::from("{\"strips\":{},\"buses\":{},\"params\":{}}"),
            meters: QString::from("{\"strips\":{},\"buses\":{}}"),
            catalog: QString::from("[]"),
            report: QString::from(""),
            ports: QString::from("[]"),
            connection_error: QString::from(""),
            dirty: false,
            load_last_session: vibemix_core::config::load_startup_settings()
                .load_last_session,
            session_result: QString::from(""),
            session_path: QString::from(""),
            session_dir: QString::from(
                SESSION_DIR
                    .lock()
                    .map(|guard| guard.clone())
                    .unwrap_or_default()
                    .as_str(),
            ),
            recent_files: QString::from("[]"),
            window_geometry: QString::from(""),
            sample_rate: 48000,
            xruns: 0,
            dsp_load: 0,
            dsp_peak: 0,
            has_clipboard: false,
            clipboard_location: 0,
            clipboard_name: QString::from(""),
            undo_depth: 0,
            redo_depth: 0,
            undo_label: QString::from(""),
            redo_label: QString::from(""),
            clock: QString::from("{\"locked\":false,\"playing\":false,\"bpm\":120,\"beats\":0,\"seconds\":0,\"beats_per_bar\":4,\"jitter_bpm\":0,\"glitches\":0}"),
            recorder: QString::from("{\"state\":\"idle\",\"progress\":0,\"frames\":0,\"peak\":0,\"loop_beats\":0,\"loops_dir\":\"\",\"message\":\"\"}"),
            loops: QString::from("[]"),
            waveform: QString::from("{\"slot\":-1,\"peaks\":[]}"),
            catalog_ready: false,
            scan_running: false,
            scan_done: 0,
            scan_total: 0,
        }
    }
}

impl qobject::Backend {
    pub fn poll(mut self: Pin<&mut Self>) {
        let structure_rev = STRUCTURE_REV.load(Ordering::Acquire);
        if structure_rev != LAST_STRUCTURE_REV.load(Ordering::Relaxed) {
            LAST_STRUCTURE_REV.store(structure_rev, Ordering::Relaxed);
            let snapshot = build_snapshot_json();
            self.as_mut().set_snapshot(QString::from(snapshot.as_str()));
        }

        let values_rev = VALUES_REV.load(Ordering::Acquire);
        if values_rev != LAST_VALUES_REV.load(Ordering::Relaxed) {
            LAST_VALUES_REV.store(values_rev, Ordering::Relaxed);
            let values = build_values_json();
            self.as_mut().set_values(QString::from(values.as_str()));
        }

        let catalog_rev = CATALOG_REV.load(Ordering::Acquire);
        if catalog_rev != LAST_CATALOG_REV.load(Ordering::Relaxed) {
            LAST_CATALOG_REV.store(catalog_rev, Ordering::Relaxed);
            let catalog = build_catalog_json();
            self.as_mut().set_catalog(QString::from(catalog.as_str()));
        }

        let meters_rev = METERS_REV.load(Ordering::Acquire);
        if meters_rev != LAST_METERS_REV.load(Ordering::Relaxed) {
            LAST_METERS_REV.store(meters_rev, Ordering::Relaxed);
            self.as_mut()
                .set_meters(QString::from(build_meters_json().as_str()));
        }

        let ports_rev = PORTS_REV.load(Ordering::Acquire);
        if ports_rev != LAST_PORTS_REV.load(Ordering::Relaxed) {
            LAST_PORTS_REV.store(ports_rev, Ordering::Relaxed);
            let ports = PORTS.lock().map(|guard| guard.clone()).unwrap_or_default();
            self.as_mut().set_ports(QString::from(ports.as_str()));
        }

        let recorder_rev = RECORDER_REV.load(Ordering::Acquire);
        if recorder_rev != LAST_RECORDER_REV.load(Ordering::Relaxed) {
            LAST_RECORDER_REV.store(recorder_rev, Ordering::Relaxed);
            let json = RECORDER
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            self.as_mut().set_recorder(QString::from(json.as_str()));
        }

        let loops_rev = LOOPS_REV.load(Ordering::Acquire);
        if loops_rev != LAST_LOOPS_REV.load(Ordering::Relaxed) {
            LAST_LOOPS_REV.store(loops_rev, Ordering::Relaxed);
            let json = LOOPS.lock().map(|guard| guard.clone()).unwrap_or_default();
            self.as_mut().set_loops(QString::from(json.as_str()));
        }

        let waveform_rev = WAVEFORM_REV.load(Ordering::Acquire);
        if waveform_rev != LAST_WAVEFORM_REV.load(Ordering::Relaxed) {
            LAST_WAVEFORM_REV.store(waveform_rev, Ordering::Relaxed);
            let json = WAVEFORM
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            self.as_mut().set_waveform(QString::from(json.as_str()));
        }
        if let Some(error) = PENDING_CONNECTION_ERROR
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            self.as_mut()
                .set_connection_error(QString::from(error.as_str()));
        }
        self.as_mut().set_connected(is_connected());

        if let Some(report) = PENDING_REPORT
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            self.as_mut().set_report(QString::from(report.as_str()));
        }

        self.as_mut().set_dirty(DIRTY.load(Ordering::SeqCst));
        if let Ok(path) = SESSION_PATH.lock() {
            let changed = LAST_SESSION_PATH
                .lock()
                .map(|last| *last != *path)
                .unwrap_or(true);
            if changed {
                self.as_mut().set_session_path(QString::from(path.as_str()));
                if let Ok(mut last) = LAST_SESSION_PATH.lock() {
                    *last = path.clone();
                }
            }
        }
        if let Ok(list) = RECENT.lock() {
            if let Ok(json) = serde_json::to_string(&*list) {
                let changed = LAST_RECENT.lock().map(|last| *last != json).unwrap_or(true);
                if changed {
                    self.as_mut().set_recent_files(QString::from(json.as_str()));
                    if let Ok(mut last) = LAST_RECENT.lock() {
                        *last = json;
                    }
                }
            }
        }
        if let Some((ok, path, error)) = PENDING_SESSION
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            let nonce = SESSION_NONCE.fetch_add(1, Ordering::SeqCst) + 1;
            let json = serde_json::json!({
                "ok": ok,
                "path": path,
                "error": error,
                "nonce": nonce,
            })
            .to_string();
            self.as_mut()
                .set_session_result(QString::from(json.as_str()));
        }

        if let Some((width, height, maximized)) = PENDING_WINDOW
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            let nonce = WINDOW_NONCE.fetch_add(1, Ordering::SeqCst) + 1;
            let json = serde_json::json!({
                "width": width,
                "height": height,
                "maximized": maximized,
                "nonce": nonce,
            })
            .to_string();
            self.as_mut()
                .set_window_geometry(QString::from(json.as_str()));
        }

        self.as_mut()
            .set_sample_rate(SAMPLE_RATE.load(Ordering::SeqCst) as i32);
        self.as_mut().set_xruns(XRUNS.load(Ordering::SeqCst) as i32);
        self.as_mut()
            .set_dsp_load(DSP_LOAD.load(Ordering::SeqCst) as i32);
        self.as_mut()
            .set_dsp_peak(DSP_PEAK.load(Ordering::SeqCst) as i32);

        if let Ok(clipboard) = CLIPBOARD.lock() {
            self.as_mut().set_has_clipboard(clipboard.0);
            self.as_mut().set_clipboard_location(clipboard.1 as i32);
            self.as_mut()
                .set_clipboard_name(QString::from(clipboard.2.as_str()));
        }
        if let Ok(edit) = EDIT_STATE.lock() {
            self.as_mut().set_undo_depth(edit.0 as i32);
            self.as_mut().set_redo_depth(edit.1 as i32);
            self.as_mut().set_undo_label(QString::from(edit.2.as_str()));
            self.as_mut().set_redo_label(QString::from(edit.3.as_str()));
        }
        if let Ok(clock) = CLOCK.lock() {
            let snapshot = (
                clock.0, clock.1, clock.2, clock.3, clock.4, clock.5, clock.6, clock.7,
            );
            let changed = LAST_CLOCK
                .lock()
                .map(|last| *last != Some(snapshot))
                .unwrap_or(true);
            if changed {
                let json = serde_json::json!({
                    "locked": clock.0,
                    "playing": clock.1,
                    "bpm": clock.2,
                    "beats": clock.3,
                    "seconds": clock.4,
                    "beats_per_bar": clock.5,
                    "jitter_bpm": clock.6,
                    "glitches": clock.7,
                })
                .to_string();
                self.as_mut().set_clock(QString::from(json.as_str()));
                if let Ok(mut last) = LAST_CLOCK.lock() {
                    *last = Some(snapshot);
                }
            }
        }

        self.as_mut()
            .set_catalog_ready(CATALOG_READY.load(Ordering::Relaxed));
        if let Ok(scan) = SCAN.lock() {
            self.as_mut().set_scan_running(scan.0);
            self.as_mut().set_scan_done(scan.1 as i32);
            self.as_mut().set_scan_total(scan.2 as i32);
        }
    }

    pub fn refresh(&self) {
        send(ControlMessage::QueryState);
    }

    pub fn rescan_plugins(&self) {
        note_action("scanning plugins");
        send(ControlMessage::RescanPlugins);
    }

    pub fn retest_plugins(&self) {
        note_action("testing plugins");
        send(ControlMessage::RetestPlugins);
    }

    pub fn query_ports(&self) {
        send(ControlMessage::PortsQuery);
    }

    pub fn connect_ports(&self, source: &QString, sink: &QString) {
        send(ControlMessage::Connect {
            source: source.to_string(),
            sink: sink.to_string(),
        });
    }

    pub fn disconnect_ports(&self, source: &QString, sink: &QString) {
        send(ControlMessage::Disconnect {
            source: source.to_string(),
            sink: sink.to_string(),
        });
    }

    pub fn cancel_plugin_scan(&self) {
        note_action("cancelling plugin test");
        send(ControlMessage::CancelPluginScan);
    }

    pub fn update_strip_gain(&self, strip: i32, gain: f32) {
        note_action("strip gain");
        send(ControlMessage::SetStripGain {
            strip: strip as u32,
            gain,
        });
    }

    pub fn update_strip_mute(&self, strip: i32, mute: bool) {
        note_action("strip mute");
        send(ControlMessage::SetStripMute {
            strip: strip as u32,
            mute,
        });
    }

    pub fn update_strip_solo(&self, strip: i32, solo: bool) {
        note_action("strip solo");
        send(ControlMessage::SetStripSolo {
            strip: strip as u32,
            solo,
        });
    }

    pub fn update_strip_pan(&self, strip: i32, pan: f32) {
        note_action("strip pan");
        send(ControlMessage::SetStripPan {
            strip: strip as u32,
            pan,
        });
    }

    pub fn update_strip_input_gain(&self, strip: i32, db: f32) {
        note_action("strip input gain");
        send(ControlMessage::SetStripInputGain {
            strip: strip as u32,
            db,
        });
    }

    pub fn update_strip_name(&self, strip: i32, name: &QString) {
        note_action("strip name");
        send(ControlMessage::SetStripName {
            strip: strip as u32,
            name: name.to_string(),
        });
    }

    pub fn update_move_strip(&self, strip: i32, to_index: i32) {
        note_action("move strip");
        send(ControlMessage::MoveStrip {
            strip: strip as u32,
            to_index: to_index.max(0) as u32,
        });
    }

    pub fn update_bus_gain(&self, bus: i32, gain: f32) {
        note_action("bus gain");
        send(ControlMessage::SetBusGain {
            bus: bus as u32,
            gain,
        });
    }

    pub fn update_bus_mute(&self, bus: i32, mute: bool) {
        note_action("bus mute");
        send(ControlMessage::SetBusMute {
            bus: bus as u32,
            mute,
        });
    }

    pub fn update_bus_name(&self, bus: i32, name: &QString) {
        note_action("bus name");
        send(ControlMessage::SetBusName {
            bus: bus as u32,
            name: name.to_string(),
        });
    }

    pub fn update_move_bus(&self, bus: i32, to_index: i32) {
        note_action("move bus");
        send(ControlMessage::MoveBus {
            bus: bus as u32,
            to_index: to_index.max(0) as u32,
        });
    }

    pub fn update_bus_mix_master(&self, bus: i32, on: bool) {
        note_action("bus mix to master");
        send(ControlMessage::SetBusMixMaster {
            bus: bus as u32,
            on,
        });
    }

    pub fn update_bus_return(&self, bus: i32, on: bool) {
        note_action("bus return");
        send(ControlMessage::SetBusReturn {
            bus: bus as u32,
            on,
        });
    }

    pub fn update_bus_send_gain(&self, bus: i32, gain: f32) {
        note_action("bus send gain");
        send(ControlMessage::SetBusSendGain {
            bus: bus as u32,
            gain,
        });
    }

    pub fn update_bus_return_gain(&self, bus: i32, gain: f32) {
        note_action("bus return gain");
        send(ControlMessage::SetBusReturnGain {
            bus: bus as u32,
            gain,
        });
    }

    pub fn update_send_gain(&self, strip: i32, bus: i32, gain: f32) {
        note_action("send gain");
        send(ControlMessage::SetSendGain {
            strip: strip as u32,
            bus: bus as u32,
            gain,
        });
    }

    pub fn update_send_delay(&self, strip: i32, bus: i32, ms: f32) {
        note_action("send delay");
        send(ControlMessage::SetSendDelay {
            strip: strip as u32,
            bus: bus as u32,
            ms,
        });
    }

    pub fn update_send_pre_fader(&self, strip: i32, bus: i32, pre: bool) {
        note_action("send pre-fader");
        send(ControlMessage::SetSendPreFader {
            strip: strip as u32,
            bus: bus as u32,
            pre,
        });
    }

    pub fn add_strip(&self) {
        note_action("add strip");
        send(ControlMessage::AddStrip);
    }

    pub fn add_bus(&self) {
        note_action("add bus");
        send(ControlMessage::AddBus);
    }

    pub fn remove_strip(&self, strip: i32) {
        note_action("remove strip");
        send(ControlMessage::RemoveStrip {
            strip: strip as u32,
        });
    }

    pub fn remove_bus(&self, bus: i32) {
        note_action("remove bus");
        send(ControlMessage::RemoveBus { bus: bus as u32 });
    }

    pub fn add_plugin(&self, location: i32, target: i32, uri: &QString) {
        note_action("add plugin");
        send(ControlMessage::AddPlugin {
            location: location as u32,
            target: target as u32,
            uri: uri.to_string(),
        });
    }

    pub fn remove_plugin(&self, location: i32, target: i32, slot: i32) {
        note_action("remove plugin");
        send(ControlMessage::RemovePlugin {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
        });
    }

    pub fn move_plugin(&self, location: i32, target: i32, from: i32, to: i32) {
        note_action("move plugin");
        send(ControlMessage::MovePlugin {
            location: location as u32,
            target: target as u32,
            from: from as u32,
            to: to as u32,
        });
    }

    pub fn update_plugin_param(
        &self,
        location: i32,
        target: i32,
        slot: i32,
        param: i32,
        value: f32,
    ) {
        note_action("plugin param");
        send(ControlMessage::SetPluginParam {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            param: param as u32,
            value,
        });
    }

    pub fn update_plugin_bypass(&self, location: i32, target: i32, slot: i32, bypassed: bool) {
        note_action("plugin bypass");
        send(ControlMessage::SetPluginBypass {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            bypassed,
        });
    }

    pub fn update_plugin_sidechain_enabled(
        &self,
        location: i32,
        target: i32,
        slot: i32,
        enabled: bool,
    ) {
        note_action("sidechain enable");
        send(ControlMessage::SetPluginSidechainEnabled {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            enabled,
        });
    }

    pub fn update_plugin_sidechain(
        &self,
        location: i32,
        target: i32,
        slot: i32,
        strip: i32,
        gain: f32,
    ) {
        note_action("sidechain send");
        send(ControlMessage::SetPluginSidechain {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            strip,
            gain,
        });
    }

    pub fn update_plugin_sidechain_pre(
        &self,
        location: i32,
        target: i32,
        slot: i32,
        strip: i32,
        pre: bool,
    ) {
        note_action("sidechain pre-fader");
        send(ControlMessage::SetPluginSidechainPre {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            strip: strip as u32,
            pre,
        });
    }

    pub fn open_plugin_ui(&self, location: i32, target: i32, slot: i32) {
        note_action("open plugin UI");
        send(ControlMessage::OpenPluginUi {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
        });
    }

    pub fn close_plugin_ui(&self, location: i32, target: i32, slot: i32) {
        note_action("close plugin UI");
        send(ControlMessage::ClosePluginUi {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
        });
    }

    pub fn copy_plugin(&self, location: i32, target: i32, slot: i32) {
        note_action("copy plugin");
        send(ControlMessage::CopyPlugin {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
        });
    }

    pub fn cut_plugin(&self, location: i32, target: i32, slot: i32) {
        note_action("cut plugin");
        send(ControlMessage::CutPlugin {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
        });
    }

    pub fn paste_plugin(&self, location: i32, target: i32) {
        note_action("paste plugin");
        send(ControlMessage::PastePlugin {
            location: location as u32,
            target: target as u32,
        });
    }

    pub fn duplicate_strip(&self, strip: i32) {
        note_action("duplicate strip");
        send(ControlMessage::DuplicateStrip {
            strip: strip as u32,
        });
    }

    pub fn duplicate_bus(&self, bus: i32) {
        note_action("duplicate bus");
        send(ControlMessage::DuplicateBus { bus: bus as u32 });
    }

    pub fn undo(&self) {
        note_action("undo");
        send(ControlMessage::Undo);
    }

    pub fn redo(&self) {
        note_action("redo");
        send(ControlMessage::Redo);
    }

    pub fn set_transport_ui(&self, json: &QString) {
        note_action("transport ui");
        send(ControlMessage::SetTransportUi {
            json: json.to_string(),
        });
    }

    pub fn set_recorder_sources(&self, json: &QString) {
        note_action("recorder sources");
        send(ControlMessage::SetRecorderSources {
            json: json.to_string(),
        });
    }

    pub fn set_recorder_settings(&self, json: &QString) {
        note_action("recorder settings");
        send(ControlMessage::SetRecorderSettings {
            json: json.to_string(),
        });
    }

    pub fn recorder_action(&self, json: &QString) {
        note_action("recorder action");
        send(ControlMessage::RecorderAction {
            json: json.to_string(),
        });
    }

    pub fn set_layout(&self, strips_per_row: i32, buses_per_row: i32) {
        note_action("layout");
        send(ControlMessage::SetLayout {
            strips_per_row: strips_per_row.clamp(0, 32) as u32,
            buses_per_row: buses_per_row.clamp(0, 32) as u32,
        });
    }

    pub fn set_beats_per_bar(&self, value: f32) {
        note_action("beats per bar");
        send(ControlMessage::SetBeatsPerBar { value });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn midi_learn(
        &self,
        kind: i32,
        location: i32,
        target: i32,
        slot: i32,
        param: i32,
        min: f32,
        max: f32,
    ) {
        note_action("midi learn");
        send(ControlMessage::MidiLearn {
            kind: kind as u32,
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            param: param as u32,
            min,
            max,
        });
    }

    pub fn midi_learn_cancel(&self) {
        note_action("midi learn cancel");
        send(ControlMessage::MidiLearnCancel);
    }

    pub fn midi_bindings_clear(&self) {
        note_action("clear midi bindings");
        send(ControlMessage::MidiBindingsClear);
    }

    pub fn set_midi_sync(&self, on: bool) {
        note_action("midi sync");
        send(ControlMessage::SetMidiSync { on });
    }

    pub fn set_clock_master(&self, on: bool) {
        note_action("clock master");
        send(ControlMessage::SetClockMaster { on });
    }

    pub fn set_clock_bpm(&self, bpm: f32) {
        note_action("clock bpm");
        send(ControlMessage::SetClockBpm { bpm });
    }

    pub fn set_clock_playing(&self, playing: bool) {
        note_action("clock transport");
        send(ControlMessage::SetClockPlaying { playing });
    }

    pub fn set_clock_transport(&self, send_transport: bool, receive_transport: bool) {
        note_action("clock transport options");
        send(ControlMessage::SetClockTransport {
            send: send_transport,
            receive: receive_transport,
        });
    }

    pub fn set_load_last_session_on_startup(mut self: Pin<&mut Self>, on: bool) {
        note_action("load last session on startup");
        update_startup_settings(|settings| settings.load_last_session = on);
        self.as_mut().set_load_last_session(on);
    }

    pub fn set_strip_midi_channel(&self, strip: i32, channel: i32) {
        note_action("strip midi channel");
        send(ControlMessage::SetStripMidiChannel {
            strip: strip as u32,
            channel: channel as u32,
        });
    }

    pub fn set_bus_midi_channel(&self, bus: i32, channel: i32) {
        note_action("bus midi channel");
        send(ControlMessage::SetBusMidiChannel {
            bus: bus as u32,
            channel: channel as u32,
        });
    }

    pub fn set_plugin_midi_channel(&self, location: i32, target: i32, slot: i32, channel: i32) {
        note_action("plugin midi channel");
        send(ControlMessage::SetPluginMidiChannel {
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            channel: channel as u32,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn midi_bind(
        &self,
        kind: i32,
        location: i32,
        target: i32,
        slot: i32,
        param: i32,
        min: f32,
        max: f32,
        channel: i32,
        cc: i32,
    ) {
        note_action("bind midi cc");
        send(ControlMessage::MidiBind {
            kind: kind as u32,
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            param: param as u32,
            min,
            max,
            channel: channel as u32,
            cc: cc as u32,
        });
    }

    pub fn midi_unbind(&self, kind: i32, location: i32, target: i32, slot: i32, param: i32) {
        note_action("unbind midi cc");
        send(ControlMessage::MidiUnbind {
            kind: kind as u32,
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            param: param as u32,
        });
    }

    pub fn key_bind(
        &self,
        kind: i32,
        location: i32,
        target: i32,
        slot: i32,
        param: i32,
        key: &QString,
    ) {
        note_action("bind key");
        send(ControlMessage::KeyBind {
            kind: kind as u32,
            location: location as u32,
            target: target as u32,
            slot: slot as u32,
            param: param as u32,
            key: key.to_string(),
        });
    }

    pub fn key_trigger(&self, key: &QString) {
        send(ControlMessage::KeyTrigger {
            key: key.to_string(),
        });
    }

    pub fn save_session(&self, path: &QString, width: i32, height: i32, maximized: bool) {
        note_action("save session");
        send(ControlMessage::SaveSession {
            path: path.to_string(),
            width,
            height,
            maximized,
        });
    }

    pub fn load_session(&self, path: &QString) {
        note_action("load session");
        send(ControlMessage::LoadSession {
            path: path.to_string(),
        });
    }

    pub fn new_session(&self) {
        note_action("new session");
        if let Ok(mut current) = SESSION_PATH.lock() {
            current.clear();
        }
        send(ControlMessage::NewSession);
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_ids, split_state};

    #[test]
    fn forwards_missing_plugin_flag() {
        let json = r#"{"strips":[{"id":0,"plugins":[{"slot":0,"uri":"u","name":"Gone","bypassed":false,"missing":true}],"plugins_post":[]}],"buses":[]}"#;
        let value: serde_json::Value = serde_json::from_str(json).expect("parse");
        let (structure, _values) = split_state(&value);
        assert!(
            structure.contains(r#""missing":true"#),
            "structure should carry the missing flag: {structure}"
        );
    }

    #[test]
    fn extracts_ids_from_snapshot() {
        let json =
            r#"{"strips":[{"id":0,"name":"ch 1","plugins":[]},{"id":2}],"buses":[{"id":0}]}"#;
        let value: serde_json::Value = serde_json::from_str(json).expect("parse");
        let (strips, buses) = extract_ids(&value);
        assert_eq!(strips, vec![0, 2]);
        assert_eq!(buses, vec![0]);
    }

    #[test]
    fn split_state_keeps_midi_connections() {
        let json = r#"{"strips":[],"buses":[],"midi_sync":false,"midi_connections":["in"],"midi_out_connections":["out"]}"#;
        let value: serde_json::Value = serde_json::from_str(json).expect("parse");
        let (structure, _values) = split_state(&value);
        assert!(structure.contains("\"midi_sync\":false"), "{structure}");
        assert!(
            structure.contains("\"midi_connections\":[\"in\"]"),
            "{structure}"
        );
        assert!(
            structure.contains("\"midi_out_connections\":[\"out\"]"),
            "{structure}"
        );
    }

    #[test]
    fn null_snapshot_yields_empty_ids() {
        let (strips, buses) = extract_ids(&serde_json::Value::Null);
        assert!(strips.is_empty());
        assert!(buses.is_empty());
    }
}
