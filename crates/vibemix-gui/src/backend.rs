use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use std::fs;

use cxx_qt_lib::QString;
use vibemix_proto::ControlMessage;

static SOCKET: OnceLock<UdpSocket> = OnceLock::new();
static ENGINE_ADDR: OnceLock<SocketAddr> = OnceLock::new();
static VIEW: OnceLock<Mutex<View>> = OnceLock::new();
static ACTION: Mutex<String> = Mutex::new(String::new());
static LAST_SNAPSHOT: Mutex<String> = Mutex::new(String::new());
static LAST_VALUES: Mutex<String> = Mutex::new(String::new());
static LAST_CATALOG: Mutex<String> = Mutex::new(String::new());
static PENDING_REPORT: Mutex<Option<String>> = Mutex::new(None);
static PENDING_SESSION: Mutex<Option<(bool, String, String)>> = Mutex::new(None);
static SESSION_PATH: Mutex<String> = Mutex::new(String::new());
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
static XRUNS: AtomicU32 = AtomicU32::new(0);
static CLIPBOARD: Mutex<(bool, u32, String)> = Mutex::new((false, 0, String::new()));
static EDIT_STATE: Mutex<(u32, u32, String, String)> =
    Mutex::new((0, 0, String::new(), String::new()));

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
    if let Ok(mut guard) = ACTION.lock() {
        *guard = action.to_string();
    }
}

fn last_action() -> String {
    ACTION
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
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
        let _ = fs::write(dir.join("recent.json"), text);
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

pub fn init(engine_port: u16) {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind osc client");
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");

    let address = format!("127.0.0.1:{engine_port}");
    let engine_address: SocketAddr = address.parse().expect("parse engine address");
    let _ = ENGINE_ADDR.set(engine_address);

    let receiver = socket.try_clone().expect("clone socket");
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
}

fn send(message: ControlMessage) {
    if let (Some(socket), Some(address)) = (SOCKET.get(), ENGINE_ADDR.get()) {
        if let Ok(bytes) = message.encode() {
            let _ = socket.send_to(&bytes, address);
        }
    }
}

fn receive_loop(socket: UdpSocket) {
    let mut buffer = [0u8; 65536];
    loop {
        if let Ok((length, _)) = socket.recv_from(&mut buffer) {
            match ControlMessage::decode(&buffer[..length]) {
                Some(ControlMessage::State { json }) => {
                    let (structure, values) = split_state(&json);
                    let (strip_ids, bus_ids) = extract_ids(&json);
                    let combined = format!("{structure}\n{values}");
                    let mut guard = view().lock().expect("view lock");
                    guard.strip_ids = strip_ids;
                    guard.bus_ids = bus_ids;
                    guard.structure_json = structure;
                    guard.values_json = values;
                    guard.connected = true;
                    drop(guard);

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

                    if let Some(window) = window_from_state(&json) {
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

                    if let Some(rate) = sample_rate_from_state(&json) {
                        SAMPLE_RATE.store(rate, Ordering::SeqCst);
                    }
                    if let Some(xruns) = xruns_from_state(&json) {
                        XRUNS.store(xruns, Ordering::SeqCst);
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
                        }
                    } else {
                        DIRTY.store(true, Ordering::SeqCst);
                    }
                    if let Ok(mut guard) = PENDING_SESSION.lock() {
                        *guard = Some((ok, path, error));
                    }
                }
                Some(ControlMessage::Catalog { json }) => {
                    let mut guard = view().lock().expect("view lock");
                    guard.catalog_json = json;
                    guard.connected = true;
                }
                Some(ControlMessage::MissingConnections { json }) => {
                    if let Ok(mut guard) = PENDING_REPORT.lock() {
                        *guard = Some(json);
                    }
                }
                Some(ControlMessage::Clipboard { has, location, name }) => {
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
                Some(ControlMessage::Meters {
                    strips_in_l,
                    strips_in_r,
                    strips_gain_l,
                    strips_gain_r,
                    strips_out_l,
                    strips_out_r,
                    buses_in_l,
                    buses_in_r,
                    buses_gain_l,
                    buses_gain_r,
                    buses_ret_l,
                    buses_ret_r,
                    buses_l,
                    buses_r,
                }) => {
                    let mut guard = view().lock().expect("view lock");
                    guard.strip_in_l = strips_in_l;
                    guard.strip_in_r = strips_in_r;
                    guard.strip_gain_l = strips_gain_l;
                    guard.strip_gain_r = strips_gain_r;
                    guard.strip_out_l = strips_out_l;
                    guard.strip_out_r = strips_out_r;
                    guard.bus_in_l = buses_in_l;
                    guard.bus_in_r = buses_in_r;
                    guard.bus_gain_l = buses_gain_l;
                    guard.bus_gain_r = buses_gain_r;
                    guard.bus_ret_l = buses_ret_l;
                    guard.bus_ret_r = buses_ret_r;
                    guard.bus_l = buses_l;
                    guard.bus_r = buses_r;
                    guard.connected = true;
                }
                _ => {}
            }
        }
    }
}

fn build_snapshot_json() -> String {
    view().lock().expect("view lock").structure_json.clone()
}

fn build_values_json() -> String {
    view().lock().expect("view lock").values_json.clone()
}

fn build_catalog_json() -> String {
    view().lock().expect("view lock").catalog_json.clone()
}

fn build_meters_json() -> String {
    let guard = view().lock().expect("view lock");

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

fn is_connected() -> bool {
    view().lock().expect("view lock").connected
}

fn extract_ids(json: &str) -> (Vec<u32>, Vec<u32>) {    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return (Vec::new(), Vec::new());
    };

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

fn window_from_state(json: &str) -> Option<(i32, i32, bool)> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
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

fn sample_rate_from_state(json: &str) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let rate = value.get("sample_rate")?.as_u64()?;
    if rate == 0 {
        return None;
    }
    Some(rate as u32)
}

fn xruns_from_state(json: &str) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let xruns = value.get("xruns")?.as_u64()?;
    Some(xruns.min(u32::MAX as u64) as u32)
}

fn build_plugins(
    plugins: Option<&Vec<serde_json::Value>>,
    location: u64,
    target: u64,
    params_values: &mut serde_json::Map<String, serde_json::Value>,
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
                controls_values
                    .push(control.get("value").cloned().unwrap_or(serde_json::json!(0.0)));
            }
        }

        structure.push(serde_json::json!({
            "slot": slot,
            "uri": plugin.get("uri").cloned().unwrap_or(serde_json::json!("")),
            "name": plugin.get("name").cloned().unwrap_or(serde_json::json!("")),
            "bypassed": plugin.get("bypassed").cloned().unwrap_or(serde_json::json!(false)),
            "has_ui": plugin.get("has_ui").cloned().unwrap_or(serde_json::json!(false)),
            "controls": controls_structure,
        }));
        params_values.insert(
            format!("{location}-{target}-{slot}"),
            serde_json::Value::Array(controls_values),
        );
    }
    structure
}

fn split_state(json: &str) -> (String, String) {
    let value: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);

    let mut strips_structure = Vec::new();
    let mut strips_values = serde_json::Map::new();
    let mut params_values = serde_json::Map::new();

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
            );
            let plugins_post_structure = build_plugins(
                strip.get("plugins_post").and_then(serde_json::Value::as_array),
                1,
                id,
                &mut params_values,
            );
            // Send levels change continuously, so keep them in `values` (which
            // does not reset the strip/bus repeaters) rather than in the
            // structural snapshot.
            let mut send_delays = serde_json::Map::new();
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
                        }
                    }
                    serde_json::Value::Object(map)
                })
                .unwrap_or_else(|| serde_json::json!({}));

            strips_structure.push(serde_json::json!({
                "id": id,
                "name": strip.get("name").cloned().unwrap_or(serde_json::json!("")),
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
            );
            buses_structure.push(serde_json::json!({
                "id": id,
                "name": bus.get("name").cloned().unwrap_or(serde_json::json!("")),
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

    let structure =
        serde_json::json!({"strips": strips_structure, "buses": buses_structure}).to_string();
    let values = serde_json::json!({
        "strips": strips_values,
        "buses": buses_values,
        "params": params_values,
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
        #[qproperty(QString, action)]
        #[qproperty(QString, report)]
        #[qproperty(bool, dirty)]
        #[qproperty(QString, session_result)]
        #[qproperty(QString, session_path)]
        #[qproperty(QString, session_dir)]
        #[qproperty(QString, recent_files)]
        #[qproperty(QString, window_geometry)]
        #[qproperty(i32, sample_rate)]
        #[qproperty(i32, xruns)]
        #[qproperty(bool, has_clipboard)]
        #[qproperty(i32, clipboard_location)]
        #[qproperty(QString, clipboard_name)]
        #[qproperty(i32, undo_depth)]
        #[qproperty(i32, redo_depth)]
        #[qproperty(QString, undo_label)]
        #[qproperty(QString, redo_label)]
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
        fn update_plugin_param(&self, location: i32, target: i32, slot: i32, param: i32, value: f32);

        #[qinvokable]
        #[cxx_name = "setPluginBypass"]
        fn update_plugin_bypass(&self, location: i32, target: i32, slot: i32, bypassed: bool);

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
    action: QString,
    report: QString,
    dirty: bool,
    session_result: QString,
    session_path: QString,
    session_dir: QString,
    recent_files: QString,
    window_geometry: QString,
    sample_rate: i32,
    xruns: i32,
    has_clipboard: bool,
    clipboard_location: i32,
    clipboard_name: QString,
    undo_depth: i32,
    redo_depth: i32,
    undo_label: QString,
    redo_label: QString,
}

impl Default for BackendRust {
    fn default() -> Self {
        Self {
            connected: false,
            snapshot: QString::from("{\"strips\":[],\"buses\":[]}"),
            values: QString::from("{\"strips\":{},\"buses\":{},\"params\":{}}"),
            meters: QString::from("{\"strips\":{},\"buses\":{}}"),
            catalog: QString::from("[]"),
            action: QString::from(""),
            report: QString::from(""),
            dirty: false,
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
            has_clipboard: false,
            clipboard_location: 0,
            clipboard_name: QString::from(""),
            undo_depth: 0,
            redo_depth: 0,
            undo_label: QString::from(""),
            redo_label: QString::from(""),
        }
    }
}

impl qobject::Backend {
    pub fn poll(mut self: Pin<&mut Self>) {
        let snapshot = build_snapshot_json();
        let snapshot_changed = {
            let mut last = LAST_SNAPSHOT.lock().expect("snapshot lock");
            if *last != snapshot {
                *last = snapshot.clone();
                true
            } else {
                false
            }
        };
        if snapshot_changed {
            self.as_mut().set_snapshot(QString::from(snapshot.as_str()));
        }

        let values = build_values_json();
        let values_changed = {
            let mut last = LAST_VALUES.lock().expect("values lock");
            if *last != values {
                *last = values.clone();
                true
            } else {
                false
            }
        };
        if values_changed {
            self.as_mut().set_values(QString::from(values.as_str()));
        }

        let catalog = build_catalog_json();
        let catalog_changed = {
            let mut last = LAST_CATALOG.lock().expect("catalog lock");
            if *last != catalog {
                *last = catalog.clone();
                true
            } else {
                false
            }
        };
        if catalog_changed {
            self.as_mut().set_catalog(QString::from(catalog.as_str()));
        }

        self.as_mut()
            .set_meters(QString::from(build_meters_json().as_str()));
        self.as_mut().set_connected(is_connected());
        self.as_mut()
            .set_action(QString::from(last_action().as_str()));

        if let Some(report) = PENDING_REPORT.lock().ok().and_then(|mut guard| guard.take()) {
            self.as_mut()
                .set_report(QString::from(report.as_str()));
        }

        self.as_mut().set_dirty(DIRTY.load(Ordering::SeqCst));
        if let Ok(path) = SESSION_PATH.lock() {
            self.as_mut().set_session_path(QString::from(path.as_str()));
        }
        if let Ok(list) = RECENT.lock() {
            if let Ok(json) = serde_json::to_string(&*list) {
                self.as_mut().set_recent_files(QString::from(json.as_str()));
            }
        }
        if let Some((ok, path, error)) =
            PENDING_SESSION.lock().ok().and_then(|mut guard| guard.take())
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

        if let Some((width, height, maximized)) =
            PENDING_WINDOW.lock().ok().and_then(|mut guard| guard.take())
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
        self.as_mut()
            .set_xruns(XRUNS.load(Ordering::SeqCst) as i32);

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
    }

    pub fn refresh(&self) {
        send(ControlMessage::QueryState);
    }

    pub fn rescan_plugins(&self) {
        note_action("scanning plugins");
        send(ControlMessage::RescanPlugins);
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

    pub fn update_plugin_param(&self, location: i32, target: i32, slot: i32, param: i32, value: f32) {
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
    use super::extract_ids;

    #[test]
    fn extracts_ids_from_snapshot() {
        let json = r#"{"strips":[{"id":0,"name":"ch 1","plugins":[]},{"id":2}],"buses":[{"id":0}]}"#;
        let (strips, buses) = extract_ids(json);
        assert_eq!(strips, vec![0, 2]);
        assert_eq!(buses, vec![0]);
    }

    #[test]
    fn bad_json_yields_empty_ids() {
        let (strips, buses) = extract_ids("not json");
        assert!(strips.is_empty());
        assert!(buses.is_empty());
    }
}
