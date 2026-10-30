#![allow(clippy::items_after_test_module)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::midi_map::target_from_code;
use vibemix_core::config::config_dir;
use vibemix_core::ids::{BusId, StripId};
use vibemix_core::model::PluginFormat;
use vibemix_core::model::{
    Bus, MidiBinding, MidiTarget, MixerModel, PluginSlot, Send, SidechainSource, Strip,
    WindowGeometry, MAX_SEND_DELAY_MS,
};
use vibemix_core::params::{clamp_trim_db, Gain, Pan};
use vibemix_core::recorder::{
    detect_transient, detect_transients_with_sensitivity, peaks, rotate_interleaved, snap_seam,
    LoopSlot, LoopWaveform, RecorderAction, RecorderSettings, RecorderSource, RecorderStatus,
    Transient,
};
use vibemix_lv2::{find_ui, Host, SharedOutputs, SharedUiInputs, UiCandidate, UiMessage, UiWindow};

use crate::ui_service::UiService;
use vibemix_proto::ControlMessage;

use crate::backend_jack::{instantiate_slot, JackEngine};
use crate::catalog::{ControlInfo, PluginEntry};
use crate::dsp;
use crate::health::{HealthStore, Scan, ScanEvent, Status, VerdictMeta};
use crate::params::{
    MeterLevels, SharedMeters, SharedParams, MAX_PARAMS, MAX_PLUGINS, PLUGIN_CHAINS,
};
use crate::recorder::MAX_LOOPS;

mod catalog;
mod loops;
mod plugins;
mod smoke;
mod state;
mod ui;

pub use catalog::{discover_or_load_catalog, trust_catalog_bundles};
pub use state::{apply_model, clamp_to_capacity, sync_model_from_params};

use catalog::*;
use plugins::*;
use state::*;

const PLUGIN_LOC_STRIP_PRE: u32 = 0;
const PLUGIN_LOC_STRIP_POST: u32 = 1;

/// How often the session is autosaved for crash recovery (seconds; the
/// `VIBEMIX_AUTOSAVE_SECS` environment variable overrides it, e.g. for tests).
fn autosave_interval() -> Duration {
    let seconds = std::env::var("VIBEMIX_AUTOSAVE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(60);
    Duration::from_secs(seconds)
}
/// Where the periodic autosave lives; removed on a clean shutdown.
pub fn autosave_path() -> PathBuf {
    config_dir().join("autosave.toml")
}

/// Holds the pid of the engine that owns the autosave.
pub fn autosave_owner_path() -> PathBuf {
    config_dir().join("autosave.pid")
}

const METER_INTERVAL: Duration = Duration::from_millis(20);
/// Minimum spacing of full-state snapshots sent to the GUI.
const SNAPSHOT_MIN_INTERVAL: Duration = Duration::from_millis(33);
/// Minimum gap between catalog rebuilds while the plugin sweep streams results.
/// The GUI polls `QueryState` every second, and the plugin catalog is hundreds
/// of KB, so a query only re-sends it to a new GUI or after this long (which
/// still repairs a catalog whose chunks were lost on the way).
const CATALOG_RESEND: Duration = Duration::from_secs(30);
const CATALOG_THROTTLE: Duration = Duration::from_millis(250);
/// Minimum gap between state echoes while a plugin parameter is edited, so the
/// editor's value display tracks a drag without flooding the control socket.
const PARAM_ECHO_INTERVAL: Duration = Duration::from_millis(50);

// `IComponentHandler::restartComponent` flags (VST3 `RestartFlags`).
const VST3_RESTART_RELOAD: i32 = 1 << 0;
const VST3_RESTART_IO: i32 = 1 << 1;
const VST3_RESTART_PARAM_VALUES: i32 = 1 << 2;
const VST3_RESTART_LATENCY: i32 = 1 << 3;

#[derive(Default)]
struct Actions {
    snapshot: bool,
    catalog: bool,
}

/// One plugin copied for later pasting, plus the chain it was copied from.
#[derive(Clone)]
struct PluginClipboard {
    slot: PluginSlot,
    location: u32,
}

/// A loop slot loaded in memory. `raw` is the unrotated capture (or the saved
/// file when loaded from disk); the engine holds the rotated playback copy.
struct LoopRuntime {
    slot: LoopSlot,
    raw: Vec<f32>,
    transients: Vec<Transient>,
}

struct PendingWaveform {
    slot: u32,
    start: u64,
    end: u64,
    nudge: Option<i64>,
    seq: u64,
    buckets: u32,
    base: bool,
}

const LOOP_PEAK_BUCKETS: usize = 512;
const LOOP_WAVEFORM_BUCKETS: usize = 4096;

/// A full mixer-model snapshot for undo/redo.
struct UndoEntry {
    model: MixerModel,
    label: String,
    bytes: usize,
}

/// Identifies a continuously-edited parameter so rapid changes (a fader or
/// knob drag) collapse into a single undo step.
#[derive(Clone, PartialEq, Eq)]
enum EditKey {
    StripGain(u32),
    StripPan(u32),
    StripInput(u32),
    BusGain(u32),
    BusSendGain(u32),
    BusReturnGain(u32),
    SendGain(u32, u32),
    SendDelay(u32, u32),
    PluginParam(u32, u32, u32, u32),
}

const EDIT_COALESCE: Duration = Duration::from_millis(450);
const MAX_UNDO_ENTRIES: usize = 64;
const MAX_UNDO_BYTES: usize = 64 * 1024 * 1024;
/// Plugin location that selects a bus insert chain (0/1 are strip pre/post).
const PLUGIN_LOC_BUS: u32 = 2;

pub struct Controller {
    socket: UnixDatagram,
    /// Set when a send finds the peer's socket gone (the GUI exited); the run
    /// loop then stops addressing it.
    peer_gone: std::cell::Cell<bool>,
    params: Arc<SharedParams>,
    meters: Arc<SharedMeters>,
    model: MixerModel,
    engine: JackEngine,
    host: Host,
    /// VST3 host (discovery + instantiation).
    vst3: vibemix_vst3::Host,
    /// CLAP host (discovery + instantiation).
    clap: vibemix_clap::Host,
    /// Out-of-process LV2 UI host (spawned lazily on the first remote open).
    ui_service: UiService,
    plugins: Vec<PluginEntry>,
    catalog_json: String,
    health: HealthStore,
    health_dirty: bool,
    /// Force the next catalog rebuild immediately (manual verdicts, sweep done).
    health_immediate: bool,
    scan: Scan,
    smoke_ok: HashSet<String>,
    smoke_bad: HashSet<String>,
    /// Smoke tests running on worker threads, keyed like `smoke_ok`.
    smoke_inflight: HashSet<String>,
    smoke_tx: mpsc::Sender<(String, SmokeVerdict)>,
    smoke_rx: mpsc::Receiver<(String, SmokeVerdict)>,
    /// Plugin inserts waiting for their smoke test to finish.
    deferred_inserts: Vec<DeferredInsert>,
    uis: HashMap<(u32, u32, u32), OpenUi>,
    ui_cache: HashMap<String, Option<UiCandidate>>,
    /// LV2 editor smoke verdicts, keyed by plugin URI. Populated ahead of the
    /// first open by a background thread so the click never spawns the child.
    ui_safe: Arc<Mutex<HashMap<String, bool>>>,
    /// Per-class VST3 editor smoke verdicts, filled the same way.
    vst3_ui_safe: Arc<Mutex<HashMap<String, bool>>>,
    /// Per-id CLAP editor smoke verdicts, filled the same way.
    clap_ui_safe: Arc<Mutex<HashMap<String, bool>>>,
    missing_report: Option<String>,
    pending_session: Option<(bool, String, String)>,
    /// Port graph JSON requested by the GUI patchbay, sent on the next loop.
    pending_ports: Option<String>,
    /// Outcome of a patchbay connect/disconnect, sent on the next loop.
    pending_connect_result: Option<ControlMessage>,
    sample_rate: u32,
    clipboard: Option<PluginClipboard>,
    clipboard_dirty: bool,
    undo_stack: VecDeque<UndoEntry>,
    redo_stack: Vec<UndoEntry>,
    undo_bytes: usize,
    undo_coalesce: Option<(Instant, EditKey)>,
    edit_dirty: bool,
    pending_learn: Option<MidiBinding>,
    /// Last time a plugin-parameter edit echoed state to the UI.
    last_param_echo: Instant,
    /// Last installed sidechain strip order (recomputed each poll, pushed on change).
    /// Hash of the sidechain routing; the topological strip order is only
    /// recomputed when this changes.
    last_routing_signature: u64,
    last_autosave: Instant,
    /// Where and when the plugin catalog was last sent (see `CATALOG_RESEND`).
    catalog_sent_to: Option<PathBuf>,
    catalog_sent_at: Instant,
    last_autosave_text: String,
    /// Reused meter snapshot and OSC packet buffers (no per-frame allocation).
    meter_levels: MeterLevels,
    meter_bytes: Vec<u8>,
    loops: Vec<Option<LoopRuntime>>,
    recorder_status: RecorderStatus,
    recorder_dirty: bool,
    loops_dirty: bool,
    pending_waveform: Option<PendingWaveform>,
}

/// An open plugin editor: an LV2 toolkit UI (in the engine or the out-of-process
/// UI host) or a VST3 `IPlugView`.
enum OpenUi {
    /// LV2 editor hosted in the out-of-process UI host, keyed by host id.
    Lv2Remote { id: u32, context: Box<UiWriteCtx> },
    /// LV2 editor hosted in-process (instance-access UIs, or `VIBEMIX_UI_LOCAL`).
    Lv2Local {
        window: UiWindow,
        context: Box<UiWriteCtx>,
    },
    Vst3 {
        view: vibemix_vst3::View,
        /// Kept so editor-driven parameter moves can be drained.
        handle: vibemix_vst3::InstanceHandle,
    },
    Clap {
        view: vibemix_clap::View,
        /// Kept so editor-driven parameter moves can be drained.
        handle: vibemix_clap::InstanceHandle,
    },
}

/// A port value to deliver to an editor (`ui:portEvent`).
struct OutputEvent {
    port: u32,
    format: u32,
    /// Set for atom payloads; remapped to the host's URID out of process.
    type_uri: Option<String>,
    bytes: Vec<u8>,
}

struct UiWriteCtx {
    location: u32,
    target: usize,
    slot: usize,
    params: Arc<SharedParams>,
    ports: Vec<(u32, usize)>,
    outputs: Option<Arc<SharedOutputs>>,
    /// Control→audio mailbox for UI→DSP atom events.
    ui_inputs: Option<Arc<SharedUiInputs>>,
    last_control: Vec<Option<u32>>,
    last_atom_seq: Vec<usize>,
}

impl Drop for UiWriteCtx {
    fn drop(&mut self) {
        if let Some(outputs) = &self.outputs {
            outputs.set_active(false);
        }
    }
}

impl UiWriteCtx {
    /// Collects the plugin's output port values that changed since the last call
    /// (host -> `ui:portEvent`), so meters/analysers animate even for plugins
    /// that do not read their DSP ports directly via `instance-access`.
    fn collect_outputs(&mut self) -> Vec<OutputEvent> {
        let mut events = Vec::new();
        let Some(outputs) = &self.outputs else {
            return events;
        };
        if !outputs.is_active() {
            return events;
        }

        let controls = outputs.control_port_count();
        if self.last_control.len() != controls {
            self.last_control = vec![None; controls];
        }
        for index in 0..controls {
            let bits = outputs.control_value(index).to_bits();
            if self.last_control[index] == Some(bits) {
                continue;
            }
            self.last_control[index] = Some(bits);
            events.push(OutputEvent {
                port: outputs.control_port(index),
                format: 0,
                type_uri: None,
                bytes: f32::from_bits(bits).to_ne_bytes().to_vec(),
            });
        }

        let atoms = outputs.atom_port_count();
        if self.last_atom_seq.len() != atoms {
            self.last_atom_seq = vec![usize::MAX; atoms];
        }
        // Per the atom extension, sequence events transfer individually to the
        // UI with `format = atom:eventTransfer`.
        let event_transfer =
            vibemix_lv2::urid::UridMap::shared().map(vibemix_lv2::EVENT_TRANSFER_URI);
        for index in 0..atoms {
            let seq = outputs.atom_seq(index);
            if seq == 0 || self.last_atom_seq[index] == seq {
                continue;
            }
            if let Some((bytes, _)) = outputs.read_atom(index) {
                self.last_atom_seq[index] = seq;
                let port = outputs.atom_port(index);
                vibemix_lv2::for_each_event(&bytes, |atom| {
                    events.push(OutputEvent {
                        port,
                        format: event_transfer,
                        type_uri: None,
                        bytes: atom.to_vec(),
                    });
                });
            }
        }
        events
    }
}

extern "C" fn ui_port_write(
    user: *mut std::ffi::c_void,
    port_index: u32,
    size: u32,
    format: u32,
    buffer: *const std::ffi::c_void,
) {
    if user.is_null() || buffer.is_null() {
        return;
    }
    let context = unsafe { &*(user as *const UiWriteCtx) };
    if format == 0 {
        if size != 4 {
            return;
        }
        let value = unsafe { *(buffer as *const f32) };
        if let Some((_, param)) = context.ports.iter().find(|(port, _)| *port == port_index) {
            if let Some(chain) = strip_chain(context.location) {
                context
                    .params
                    .set_plugin_param(context.target, chain, context.slot, *param, value);
            } else {
                context
                    .params
                    .set_bus_plugin_param(context.target, context.slot, *param, value);
            }
        }
        return;
    }
    // Atom event (`atom:eventTransfer`): hand it to the audio thread.
    if let Some(inputs) = &context.ui_inputs {
        let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
        inputs.push(port_index, bytes);
    }
}

impl Drop for Controller {
    /// Fields drop in declaration order, so `engine` (which owns the plugin
    /// instances) is freed before `uis`. Open editors hold raw pointers into
    /// those instances, so they must be closed first, while the instances are
    /// still alive; otherwise shutdown hangs with the editor windows left
    /// behind, unresponsive.
    fn drop(&mut self) {
        self.close_all_uis();
    }
}

impl Controller {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        socket: UnixDatagram,
        params: Arc<SharedParams>,
        meters: Arc<SharedMeters>,
        model: MixerModel,
        engine: JackEngine,
        host: Host,
        vst3: vibemix_vst3::Host,
        clap: vibemix_clap::Host,
        plugins: Vec<PluginEntry>,
        startup_session: Option<String>,
    ) -> std::io::Result<Self> {
        socket.set_read_timeout(Some(METER_INTERVAL / 2))?;

        let health = HealthStore::load();
        let catalog_json = build_catalog(&plugins, &health);
        save_catalog(&plugins);

        let mut engine = engine;
        let missing_report = missing_report_json(&engine.take_missing_connections());
        // Track the rate the graph was built with, so a startup rate correction
        // from PipeWire (e.g. 48000 -> 44100) triggers a resync in the loop.
        let sample_rate = engine.graph_sample_rate();

        let (smoke_tx, smoke_rx) = mpsc::channel();
        let mut controller = Self {
            socket,
            peer_gone: std::cell::Cell::new(false),
            params,
            meters,
            model,
            engine,
            host,
            vst3,
            clap,
            ui_service: UiService::new(),
            plugins,
            catalog_json,
            health,
            health_dirty: false,
            health_immediate: false,
            scan: Scan::new(),
            smoke_ok: HashSet::new(),
            smoke_bad: HashSet::new(),
            smoke_inflight: HashSet::new(),
            smoke_tx,
            smoke_rx,
            deferred_inserts: Vec::new(),
            uis: HashMap::new(),
            ui_cache: HashMap::new(),
            ui_safe: Arc::new(Mutex::new(HashMap::new())),
            vst3_ui_safe: Arc::new(Mutex::new(HashMap::new())),
            clap_ui_safe: Arc::new(Mutex::new(HashMap::new())),
            missing_report,
            pending_session: startup_session.map(|path| (true, path, String::new())),
            pending_ports: None,
            pending_connect_result: None,
            sample_rate,
            clipboard: None,
            clipboard_dirty: false,
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
            undo_bytes: 0,
            undo_coalesce: None,
            edit_dirty: false,
            pending_learn: None,
            last_param_echo: Instant::now() - PARAM_ECHO_INTERVAL,
            last_routing_signature: 0,
            last_autosave: Instant::now(),
            catalog_sent_to: None,
            catalog_sent_at: Instant::now(),
            last_autosave_text: String::new(),
            meter_levels: MeterLevels::default(),
            meter_bytes: Vec::new(),
            loops: (0..MAX_LOOPS).map(|_| None).collect(),
            recorder_status: RecorderStatus::default(),
            recorder_dirty: false,
            loops_dirty: true,
            pending_waveform: None,
        };
        controller.ensure_model_plugin_controls();
        controller
            .engine
            .recorder()
            .set_sources(&controller.model.recorder.sources);
        controller.load_loops_from_model();
        save_catalog(&controller.plugins);
        controller.rebuild_shared_bindings();
        controller.start_health_scan();
        Ok(controller)
    }

    pub fn run(&mut self) {
        let mut buffer = [0u8; 65536];
        let mut target: Option<PathBuf> = None;
        let mut scan_was_running = false;
        let mut last_catalog = Instant::now() - CATALOG_THROTTLE;
        let mut last_snapshot = Instant::now() - SNAPSHOT_MIN_INTERVAL;
        let mut last_meters = Instant::now() - METER_INTERVAL;
        let mut snapshot_pending = false;
        let mut state_pending = false;
        let autosave_every = autosave_interval();

        while !crate::SHUTDOWN.load(std::sync::atomic::Ordering::SeqCst) {
            let mut actions = Actions::default();

            if self.peer_gone.replace(false) {
                // The GUI's socket vanished: stop answering it until it speaks again.
                target = None;
                self.catalog_sent_to = None;
            }

            match self.socket.recv_from(&mut buffer) {
                Ok((length, from)) => {
                    // Replies go to the sender's socket path; a sender that did
                    // not bind one can still send commands but gets no answers.
                    let sender = vibemix_proto::socket::peer_path(&from);
                    if sender.is_some() {
                        target = sender.clone();
                    }
                    if let Some(message) = ControlMessage::decode(&buffer[..length]) {
                        let is_query = matches!(message, ControlMessage::QueryState);
                        // A bug in one message handler must not take the whole
                        // engine (and the audio) down with it.
                        actions = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            self.apply(message)
                        }))
                        .unwrap_or_else(|_| {
                            tracing::error!("a control message handler panicked; continuing");
                            Actions::default()
                        });
                        if is_query
                            && sender.is_some()
                            && self.catalog_sent_to == sender
                            && self.catalog_sent_at.elapsed() < CATALOG_RESEND
                        {
                            actions.catalog = false;
                        }
                    }
                }
                Err(error) => {
                    use std::io::ErrorKind;
                    if !matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) {
                        tracing::warn!(%error, "control socket error");
                    }
                }
            }

            // Merge plugin-sweep results; the catalog is rebuilt at most every
            // CATALOG_THROTTLE (and immediately on manual verdicts / sweep end)
            // so a large sweep doesn't spam the control thread or the GUI.
            for event in self.scan.poll() {
                match event {
                    ScanEvent::Start { .. } => {}
                    ScanEvent::Verdict {
                        uri,
                        status,
                        reason,
                        meta,
                    } => {
                        self.health.set(&uri, status, reason, meta);
                        self.health_dirty = true;
                    }
                    ScanEvent::Done { .. } => {
                        self.health_dirty = true;
                        self.health_immediate = true;
                        tracing::info!("plugin health sweep finished");
                    }
                }
            }

            if self.health_dirty
                && (self.health_immediate || last_catalog.elapsed() >= CATALOG_THROTTLE)
            {
                self.health_dirty = false;
                self.health_immediate = false;
                last_catalog = Instant::now();
                self.health.save();
                self.catalog_json = build_catalog(&self.plugins, &self.health);
                actions.catalog = true;
            }

            if self.engine.server_shut_down() {
                tracing::error!("the JACK/PipeWire server shut down; exiting");
                std::process::exit(1);
            }
            if self.last_autosave.elapsed() >= autosave_every {
                self.autosave();
            }
            if self.poll_smoke() {
                actions.snapshot = true;
            }
            self.engine.drain_garbage();
            self.poll_recorder();
            self.pump_uis();
            let vst3_edits = self.drain_vst3_param_edits();
            let clap_edits = self.drain_clap_param_edits();
            let param_edits = vst3_edits || clap_edits;

            // Keep the sidechain-aware strip order up to date. The topological
            // sort allocates, so only run it when the sidechain routing (the
            // only input to the order) actually changes.
            let signature = sidechain_signature(&self.model);
            if signature != self.last_routing_signature {
                self.last_routing_signature = signature;
                let (order, delayed) = compute_strip_order(&self.model, self.params.strip_count());
                if let Err(error) = self.engine.set_strip_order(order, delayed) {
                    tracing::warn!(%error, "failed to install strip order");
                }
            }

            if let Some(learned) = self.engine.midi_map().take_learned() {
                if let Some(mut binding) = self.pending_learn.take() {
                    binding.channel = self
                        .target_channel(
                            binding.target,
                            binding.location,
                            binding.target_id,
                            binding.slot,
                        )
                        .unwrap_or(crate::midi_map::OMNI as u8);
                    binding.cc = Some(learned.cc);
                    let target = binding.target;
                    let location = binding.location;
                    let target_id = binding.target_id;
                    let slot = binding.slot;
                    let param = binding.param;
                    self.model.midi_bindings.retain(|existing| {
                        !binding_targets(existing, target, location, target_id, slot, param)
                    });
                    self.model.midi_bindings.push(binding);
                    self.rebuild_shared_bindings();
                    actions.snapshot = true;
                    tracing::info!(
                        channel = learned.channel,
                        cc = learned.cc,
                        "learned MIDI control-surface binding"
                    );
                }
            }

            if let Some(rate) = self.engine.take_pending_sample_rate() {
                if rate != self.sample_rate {
                    self.handle_sample_rate(rate);
                    actions.snapshot = true;
                }
            }

            // A MIDI control-surface CC changed a parameter; push it to the UI
            // immediately instead of waiting for the next state query.
            if self.engine.midi_map().take_changed() {
                sync_model_from_params(&self.params, &mut self.model);
                actions.snapshot = true;
            }

            let Some(address) = target.as_deref() else {
                continue;
            };

            if self.scan.running || scan_was_running {
                self.send_plugin_scan(address);
                scan_was_running = self.scan.running;
            }

            if let Some(json) = self.missing_report.take() {
                self.send_missing(address, &json);
            }
            if let Some((ok, path, error)) = self.pending_session.take() {
                self.send_session_result(address, ok, &path, &error);
            }
            if let Some(json) = self.pending_ports.take() {
                self.send_message(&ControlMessage::Ports { json }, address);
            }
            if let Some(message) = self.pending_connect_result.take() {
                self.send_message(&message, address);
            }
            if let Some(slot) = self.pending_waveform.take() {
                self.send_loop_waveform(address, slot);
            }
            if actions.catalog {
                self.send_catalog(address);
                self.catalog_sent_to = Some(address.to_path_buf());
                self.catalog_sent_at = Instant::now();
            }
            // Full-state snapshots are expensive (the whole model is serialised
            // and re-parsed by the GUI), and a fader drag produces many edits
            // per second. Coalesce them: the latest state is always sent, at
            // most once per SNAPSHOT_MIN_INTERVAL.
            snapshot_pending |= actions.snapshot;
            state_pending |= param_edits;
            let mut sent_state = false;
            if (snapshot_pending || state_pending)
                && last_snapshot.elapsed() >= SNAPSHOT_MIN_INTERVAL
            {
                last_snapshot = Instant::now();
                self.send_state(address);
                if snapshot_pending {
                    self.send_recorder(address);
                    self.send_loops(address);
                    self.recorder_dirty = false;
                    self.loops_dirty = false;
                }
                snapshot_pending = false;
                state_pending = false;
                sent_state = true;
            }
            if self.recorder_dirty {
                self.send_recorder(address);
                self.recorder_dirty = false;
            }
            if self.loops_dirty {
                self.send_loops(address);
                self.loops_dirty = false;
            }
            if self.clipboard_dirty {
                self.clipboard_dirty = false;
                self.send_clipboard(address);
            }
            if self.edit_dirty || sent_state {
                self.edit_dirty = false;
                self.send_edit_state(address);
            }

            // Meters and clock go out on a steady timer, not once per control
            // message (the socket timeout is half the interval, so an idle loop
            // still reaches this on time).
            if last_meters.elapsed() < METER_INTERVAL {
                continue;
            }
            last_meters = Instant::now();

            // Encode meters straight into a reused packet buffer (no per-frame
            // `MeterLevels`/`ControlMessage` allocations).
            self.meters.levels_into(&mut self.meter_levels);
            vibemix_proto::encode_meters(
                &mut self.meter_bytes,
                [
                    &self.meter_levels.strips_in_l,
                    &self.meter_levels.strips_in_r,
                    &self.meter_levels.strips_gain_l,
                    &self.meter_levels.strips_gain_r,
                    &self.meter_levels.strips_out_l,
                    &self.meter_levels.strips_out_r,
                ],
                [
                    &self.meter_levels.buses_in_l,
                    &self.meter_levels.buses_in_r,
                    &self.meter_levels.buses_gain_l,
                    &self.meter_levels.buses_gain_r,
                    &self.meter_levels.buses_ret_l,
                    &self.meter_levels.buses_ret_r,
                    &self.meter_levels.buses_l,
                    &self.meter_levels.buses_r,
                ],
            );
            self.send_datagram(&self.meter_bytes, address);

            let clock = self
                .engine
                .clock()
                .snapshot(self.engine.sample_rate() as f32);
            let message = ControlMessage::Clock {
                locked: clock.locked,
                playing: clock.playing,
                bpm: clock.bpm,
                beats: clock.beats as f32,
                seconds: clock.seconds as f32,
                beats_per_bar: self.params.beats_per_bar(),
                jitter_bpm: clock.jitter_bpm,
                glitches: clock.glitches,
            };
            if let Ok(bytes) = message.encode() {
                self.send_datagram(&bytes, address);
            }
        }

        // Third-party plugin teardown can hang; never let that leave editor
        // windows stuck on screen.
        crate::start_shutdown_watchdog();

        // A clean shutdown leaves nothing to recover.
        let _ = fs::remove_file(autosave_path());
        let _ = fs::remove_file(autosave_owner_path());
    }

    /// Writes the current session to the autosave file when it changed, so a
    /// crashed engine can restore it on the next start. Unlike a real save this
    /// never pauses the audio thread, so plugin state blobs are those captured
    /// at the last explicit save; parameters and the mix are always current.
    fn autosave(&mut self) {
        self.last_autosave = Instant::now();
        sync_model_from_params(&self.params, &mut self.model);
        self.engine.capture_connections(&mut self.model);
        let Ok(text) = toml::to_string_pretty(&self.model) else {
            return;
        };
        if text == self.last_autosave_text {
            return;
        }
        // Record which process owns the file, so another engine sharing this
        // config directory does not mistake a live autosave for a crashed one.
        let _ = vibemix_core::config::write_atomic(
            &autosave_owner_path(),
            std::process::id().to_string(),
        );
        match vibemix_core::config::write_atomic(&autosave_path(), &text) {
            Ok(()) => self.last_autosave_text = text,
            Err(error) => tracing::warn!(%error, "autosave failed"),
        }
    }

    /// Rebuilds everything that depends on the sample rate (delay-line buffers
    /// and plugin instances) after a JACK rate change.
    fn handle_sample_rate(&mut self, rate: u32) {
        tracing::info!(rate, "sample rate changed; rebuilding rate-dependent state");
        self.capture_plugin_states();

        let sample_rate = rate as f32;
        let max_delay_samples = (MAX_SEND_DELAY_MS / 1000.0 * sample_rate).ceil() as usize + 1;
        let delays: Vec<Vec<dsp::DelayLine>> = (0..self.params.strip_count())
            .map(|_| {
                (0..self.params.bus_count())
                    .map(|_| dsp::DelayLine::new(max_delay_samples))
                    .collect()
            })
            .collect();
        if let Err(error) = self.engine.set_sample_rate(sample_rate, delays) {
            tracing::warn!(%error, "failed to push sample-rate change");
        }
        self.sample_rate = rate;

        if let Err(error) = self.engine.rebuild_plugins(
            &self.host,
            &self.vst3,
            &self.clap,
            &mut self.model,
            &self.params,
            rate as f64,
        ) {
            tracing::warn!(%error, "failed to rebuild plugins after sample-rate change");
        }
        self.close_all_uis();
    }

    fn apply(&mut self, message: ControlMessage) -> Actions {
        tracing::debug!(?message, "control message");

        if let Some((label, key)) = edit_label(&message) {
            self.push_undo(label, key);
        }

        match message {
            ControlMessage::SetStripGain { strip, gain } => {
                let gain = gain.clamp(0.0, 4.0);
                self.params.set_strip_gain(strip as usize, gain);
                if let Some(target) = self.model.strip_mut(StripId(strip)) {
                    target.gain = Gain(gain);
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetStripMute { strip, mute } => {
                self.params.set_strip_mute(strip as usize, mute);
                if let Some(target) = self.model.strip_mut(StripId(strip)) {
                    target.mute = mute;
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetStripSolo { strip, solo } => {
                self.params.set_strip_solo(strip as usize, solo);
                if let Some(target) = self.model.strip_mut(StripId(strip)) {
                    target.solo = solo;
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetStripPan { strip, pan } => {
                let pan = pan.clamp(-1.0, 1.0);
                self.params.set_strip_pan(strip as usize, pan);
                if let Some(target) = self.model.strip_mut(StripId(strip)) {
                    target.pan = Pan(pan);
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetStripInputGain { strip, db } => {
                let db = clamp_trim_db(db);
                self.params.set_strip_input_gain_db(strip as usize, db);
                if let Some(target) = self.model.strip_mut(StripId(strip)) {
                    target.input_gain_db = db;
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetStripName { strip, name } => {
                if let Some(name) = sanitize_name(name) {
                    if let Some(target) = self.model.strip_mut(StripId(strip)) {
                        target.name = name;
                    }
                }
                self.engine.sync_port_names(&self.model);
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::MoveStrip { strip, to_index } => {
                self.model.move_strip(StripId(strip), to_index as usize);
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetBusGain { bus, gain } => {
                let gain = gain.clamp(0.0, 4.0);
                self.params.set_bus_gain(bus as usize, gain);
                if let Some(target) = self.model.bus_mut(BusId(bus)) {
                    target.gain = Gain(gain);
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetBusMute { bus, mute } => {
                self.params.set_bus_mute(bus as usize, mute);
                if let Some(target) = self.model.bus_mut(BusId(bus)) {
                    target.mute = mute;
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetBusName { bus, name } => {
                if let Some(name) = sanitize_name(name) {
                    if let Some(target) = self.model.bus_mut(BusId(bus)) {
                        target.name = name;
                    }
                }
                self.engine.sync_port_names(&self.model);
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::MoveBus { bus, to_index } => {
                self.model.move_bus(BusId(bus), to_index as usize);
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetBusMixMaster { bus, on } => {
                self.params.set_bus_mix_master(bus as usize, on);
                if let Some(target) = self.model.bus_mut(BusId(bus)) {
                    target.mix_to_master = on;
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetBusReturn { bus, on } => {
                if bus == 0 {
                    // The master has no separate send for an external insert.
                    Actions::default()
                } else {
                    self.params.set_bus_return(bus as usize, on);
                    if let Some(target) = self.model.bus_mut(BusId(bus)) {
                        target.return_inputs = on;
                        if on {
                            target.mix_to_master = true;
                        }
                    }
                    if on {
                        self.params.set_bus_mix_master(bus as usize, true);
                    }
                    if let Err(error) = self.engine.sync_bus_return_ports(&self.params) {
                        tracing::warn!(%error, "failed to sync bus return ports");
                    }
                    self.engine.sync_port_names(&self.model);
                    Actions {
                        snapshot: true,
                        catalog: false,
                    }
                }
            }
            ControlMessage::SetBusSendGain { bus, gain } => {
                let gain = gain.clamp(0.0, 4.0);
                self.params.set_bus_send_gain(bus as usize, gain);
                if let Some(target) = self.model.bus_mut(BusId(bus)) {
                    target.send_gain = Gain(gain);
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetBusReturnGain { bus, gain } => {
                let gain = gain.clamp(0.0, 4.0);
                self.params.set_bus_return_gain(bus as usize, gain);
                if let Some(target) = self.model.bus_mut(BusId(bus)) {
                    target.return_gain = Gain(gain);
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetSendGain { strip, bus, gain } => {
                let gain = gain.clamp(0.0, 4.0);
                self.params
                    .set_send_gain(strip as usize, bus as usize, gain);
                if let Some(target) = self.model.strip_mut(StripId(strip)) {
                    if let Some(send) = target.sends.iter_mut().find(|send| send.bus.0 == bus) {
                        send.gain = Gain(gain);
                    } else {
                        let mut send = Send::new(BusId(bus));
                        send.gain = Gain(gain);
                        target.sends.push(send);
                    }
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetSendDelay { strip, bus, ms } => {
                let ms = ms.clamp(0.0, vibemix_core::model::MAX_SEND_DELAY_MS);
                self.params.set_send_delay(strip as usize, bus as usize, ms);
                if let Some(target) = self.model.strip_mut(StripId(strip)) {
                    if let Some(send) = target.sends.iter_mut().find(|send| send.bus.0 == bus) {
                        send.delay_ms = ms;
                    } else {
                        let mut send = Send::new(BusId(bus));
                        send.delay_ms = ms;
                        target.sends.push(send);
                    }
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetSendPreFader { strip, bus, pre } => {
                self.params
                    .set_send_pre_fader(strip as usize, bus as usize, pre);
                if let Some(target) = self.model.strip_mut(StripId(strip)) {
                    if let Some(send) = target.sends.iter_mut().find(|send| send.bus.0 == bus) {
                        send.pre_fader = pre;
                    } else {
                        let mut send = Send::new(BusId(bus));
                        send.pre_fader = pre;
                        target.sends.push(send);
                    }
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::AddStrip => Actions {
                snapshot: self.add_strip(),
                catalog: false,
            },
            ControlMessage::AddBus => Actions {
                snapshot: self.add_bus(),
                catalog: false,
            },
            ControlMessage::RemoveStrip { strip } => {
                self.params.set_strip_active(strip as usize, false);
                self.model.strips.retain(|entry| entry.id.0 != strip);
                self.engine.clear_strip(strip as usize).ok();
                if let Err(error) = self.engine.remove_strip_ports(strip as usize) {
                    tracing::warn!(%error, strip, "failed to unregister strip ports");
                }
                self.engine.sync_port_names(&self.model);
                // Close the editors of every plugin on the removed strip, on
                // both the pre- and post-fader chains.
                self.close_uis_where(|location, target, _| {
                    location != PLUGIN_LOC_BUS && target == strip
                });
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::RemoveBus { bus } => {
                self.params.set_bus_active(bus as usize, false);
                self.params.set_bus_return(bus as usize, false);
                self.model.buses.retain(|entry| entry.id.0 != bus);
                self.engine.clear_bus_plugins(bus as usize).ok();
                if let Err(error) = self.engine.remove_bus_ports(bus as usize) {
                    tracing::warn!(%error, bus, "failed to unregister bus ports");
                }
                if let Err(error) = self.engine.remove_bus_return_ports(bus as usize) {
                    tracing::warn!(%error, bus, "failed to unregister bus return ports");
                }
                self.engine.sync_port_names(&self.model);
                // Close the editors of every plugin on the removed bus.
                self.close_uis_where(|location, target, _| {
                    location == PLUGIN_LOC_BUS && target == bus
                });
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::DuplicateStrip { strip } => Actions {
                snapshot: self.duplicate_strip(strip),
                catalog: false,
            },
            ControlMessage::DuplicateBus { bus } => Actions {
                snapshot: self.duplicate_bus(bus),
                catalog: false,
            },
            ControlMessage::AddPlugin {
                location,
                target,
                uri,
            } => Actions {
                snapshot: self.add_plugin(location, target, &uri),
                catalog: false,
            },
            ControlMessage::RemovePlugin {
                location,
                target,
                slot,
            } => Actions {
                snapshot: self.remove_plugin(location, target, slot),
                catalog: false,
            },
            ControlMessage::MovePlugin {
                location,
                target,
                from,
                to,
            } => Actions {
                snapshot: self.move_plugin(location, target, from, to),
                catalog: false,
            },
            ControlMessage::SetPluginParam {
                location,
                target,
                slot,
                param,
                value,
            } => {
                let value = self.clamp_plugin_param(location, target, slot, param, value);
                if let Some(chain) = strip_chain(location) {
                    self.params.set_plugin_param(
                        target as usize,
                        chain,
                        slot as usize,
                        param as usize,
                        value,
                    );
                    if let Some(strip) = self.model.strip_mut(StripId(target)) {
                        if let Some(plugin) =
                            strip_plugin_list_mut(strip, chain).get_mut(slot as usize)
                        {
                            while plugin.params.len() <= param as usize {
                                plugin.params.push(0.0);
                            }
                            plugin.params[param as usize] = value;
                        }
                    }
                } else {
                    self.params.set_bus_plugin_param(
                        target as usize,
                        slot as usize,
                        param as usize,
                        value,
                    );
                    if let Some(bus) = self.model.bus_mut(BusId(target)) {
                        if let Some(plugin) = bus.plugins.get_mut(slot as usize) {
                            while plugin.params.len() <= param as usize {
                                plugin.params.push(0.0);
                            }
                            plugin.params[param as usize] = value;
                        }
                    }
                }
                self.reflect_param_to_ui(location, target, slot, param, value);
                // Echo state back so the editor's value display follows the
                // drag promptly, but throttle it to avoid a message per pixel.
                let echo = self.last_param_echo.elapsed() >= PARAM_ECHO_INTERVAL;
                if echo {
                    self.last_param_echo = Instant::now();
                }
                Actions {
                    snapshot: echo,
                    catalog: false,
                }
            }
            ControlMessage::SetPluginBypass {
                location,
                target,
                slot,
                bypassed,
            } => {
                if let Some(chain) = strip_chain(location) {
                    self.params
                        .set_plugin_bypass(target as usize, chain, slot as usize, bypassed);
                    if let Some(strip) = self.model.strip_mut(StripId(target)) {
                        if let Some(plugin) =
                            strip_plugin_list_mut(strip, chain).get_mut(slot as usize)
                        {
                            plugin.bypassed = bypassed;
                        }
                    }
                } else {
                    self.params
                        .set_bus_plugin_bypass(target as usize, slot as usize, bypassed);
                    if let Some(bus) = self.model.bus_mut(BusId(target)) {
                        if let Some(plugin) = bus.plugins.get_mut(slot as usize) {
                            plugin.bypassed = bypassed;
                        }
                    }
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetPluginSidechainEnabled {
                location,
                target,
                slot,
                enabled,
            } => {
                if let Some(chain) = strip_chain(location) {
                    if let Some(plugin) = self.model.strip_mut(StripId(target)).and_then(|strip| {
                        strip_plugin_list_mut(strip, chain).get_mut(slot as usize)
                    }) {
                        plugin.sidechain_enabled = enabled;
                    }
                } else if let Some(plugin) = self
                    .model
                    .bus_mut(BusId(target))
                    .and_then(|bus| bus.plugins.get_mut(slot as usize))
                {
                    plugin.sidechain_enabled = enabled;
                }
                self.apply_sidechain_params(location, target, slot);
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetPluginSidechain {
                location,
                target,
                slot,
                strip,
                gain,
            } => {
                let gain = gain.clamp(0.0, 4.0);
                if let Ok(source) = usize::try_from(strip) {
                    if source < self.model.strips.len() {
                        let strip_id = StripId(source as u32);
                        if let Some(chain) = strip_chain(location) {
                            if let Some(plugin) =
                                self.model.strip_mut(StripId(target)).and_then(|strip| {
                                    strip_plugin_list_mut(strip, chain).get_mut(slot as usize)
                                })
                            {
                                set_sidechain_source(&mut plugin.sidechain, strip_id, gain);
                            }
                        } else if let Some(plugin) = self
                            .model
                            .bus_mut(BusId(target))
                            .and_then(|bus| bus.plugins.get_mut(slot as usize))
                        {
                            set_sidechain_source(&mut plugin.sidechain, strip_id, gain);
                        }
                        self.apply_sidechain_params(location, target, slot);
                    }
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetPluginSidechainPre {
                location,
                target,
                slot,
                strip,
                pre,
            } => {
                let source = strip as usize;
                if source < self.model.strips.len() {
                    let strip_id = StripId(strip);
                    if let Some(chain) = strip_chain(location) {
                        if let Some(plugin) =
                            self.model.strip_mut(StripId(target)).and_then(|strip| {
                                strip_plugin_list_mut(strip, chain).get_mut(slot as usize)
                            })
                        {
                            set_sidechain_pre(&mut plugin.sidechain, strip_id, pre);
                        }
                    } else if let Some(plugin) = self
                        .model
                        .bus_mut(BusId(target))
                        .and_then(|bus| bus.plugins.get_mut(slot as usize))
                    {
                        set_sidechain_pre(&mut plugin.sidechain, strip_id, pre);
                    }
                    self.apply_sidechain_params(location, target, slot);
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::OpenPluginUi {
                location,
                target,
                slot,
            } => {
                self.open_ui(location, target, slot);
                Actions::default()
            }
            ControlMessage::ClosePluginUi {
                location,
                target,
                slot,
            } => {
                if let Some(OpenUi::Lv2Remote { id, .. }) =
                    self.uis.remove(&(location, target, slot))
                {
                    self.ui_service.close(id);
                }
                Actions::default()
            }
            ControlMessage::CopyPlugin {
                location,
                target,
                slot,
            } => {
                self.copy_plugin(location, target, slot);
                Actions::default()
            }
            ControlMessage::CutPlugin {
                location,
                target,
                slot,
            } => Actions {
                snapshot: self.cut_plugin(location, target, slot),
                catalog: false,
            },
            ControlMessage::PastePlugin { location, target } => Actions {
                snapshot: self.paste_plugin(location, target),
                catalog: false,
            },
            ControlMessage::Clipboard { .. } => Actions::default(),
            ControlMessage::Undo => Actions {
                snapshot: self.undo(),
                catalog: false,
            },
            ControlMessage::Redo => Actions {
                snapshot: self.redo(),
                catalog: false,
            },
            ControlMessage::EditState { .. } => Actions::default(),
            ControlMessage::QueryState => Actions {
                snapshot: true,
                catalog: true,
            },
            ControlMessage::RescanPlugins => {
                self.capture_plugin_states();
                // Replace the discovery world. Live instances hold their own
                // `Arc<World>`, so dropping the old host here is safe; no
                // retired-host list (which would grow without bound) is needed.
                self.host = Host::new();
                self.plugins = discover_or_load_catalog(&self.host, &self.vst3, &self.clap);
                trust_catalog_bundles(&mut self.model, &self.plugins);
                self.ensure_model_plugin_controls();
                self.catalog_json = build_catalog(&self.plugins, &self.health);
                save_catalog(&self.plugins);
                let sample_rate = self.engine.sample_rate() as f64;
                // Rebinding plugins recreates every instance, so any open editor
                // (which holds the old instance handle) must be closed first or
                // it would touch freed memory once the old graph is retired.
                self.close_all_uis();
                if let Err(error) = self.engine.rebuild_plugins(
                    &self.host,
                    &self.vst3,
                    &self.clap,
                    &mut self.model,
                    &self.params,
                    sample_rate,
                ) {
                    tracing::warn!(%error, "failed to rebuild plugins after rescan");
                }
                tracing::info!(count = self.plugins.len(), "plugins rescanned");
                // Health-check any plugins discovered by the rescan.
                self.start_health_scan();
                Actions {
                    snapshot: true,
                    catalog: true,
                }
            }
            ControlMessage::RetestPlugins => {
                self.scan.start(None);
                Actions::default()
            }
            ControlMessage::CancelPluginScan => {
                self.scan.stop();
                Actions::default()
            }
            ControlMessage::PortsQuery => {
                match serde_json::to_string(&self.engine.ports(&self.model)) {
                    Ok(json) => self.pending_ports = Some(json),
                    Err(error) => tracing::warn!(%error, "failed to encode the port graph"),
                }
                Actions::default()
            }
            ControlMessage::Connect { source, sink } => {
                let outcome = self.engine.connect_port(&source, &sink);
                self.record_connection(outcome, source, sink);
                Actions::default()
            }
            ControlMessage::Disconnect { source, sink } => {
                let outcome = self.engine.disconnect_port(&source, &sink);
                self.record_connection(outcome, source, sink);
                Actions::default()
            }
            ControlMessage::Catalog { .. }
            | ControlMessage::State { .. }
            | ControlMessage::PluginScan { .. }
            | ControlMessage::SessionResult { .. }
            | ControlMessage::MissingConnections { .. }
            | ControlMessage::Ports { .. }
            | ControlMessage::ConnectResult { .. } => Actions::default(),
            // Chunk envelopes are reassembled by the receiver before dispatch;
            // the engine never expects one inbound, but ignore it if it arrives.
            ControlMessage::Chunk { .. } => Actions::default(),
            ControlMessage::SaveSession {
                path,
                width,
                height,
                maximized,
            } => {
                self.model.window = Some(WindowGeometry {
                    width: width.max(1),
                    height: height.max(1),
                    maximized,
                });
                sync_model_from_params(&self.params, &mut self.model);
                self.capture_plugin_states();
                self.engine.capture_connections(&mut self.model);
                self.rebuild_model_loops();
                self.pending_session = Some(match save_session(&self.model, &path) {
                    Ok(()) => (true, path.clone(), String::new()),
                    Err(error) => (false, path.clone(), error),
                });
                Actions::default()
            }
            ControlMessage::NewSession => {
                let mut model = MixerModel::default_minimal();
                clamp_to_capacity(
                    &mut model,
                    self.params.strip_count(),
                    self.params.bus_count(),
                );
                apply_model(&self.params, &model);
                if let Err(error) = self.engine.sync_strip_ports(&self.params) {
                    tracing::warn!(%error, "failed to sync strip ports for new session");
                }
                if let Err(error) = self.engine.sync_bus_ports(&self.params) {
                    tracing::warn!(%error, "failed to sync bus ports for new session");
                }
                if let Err(error) = self.engine.sync_bus_return_ports(&self.params) {
                    tracing::warn!(%error, "failed to sync bus return ports for new session");
                }
                let sample_rate = self.engine.sample_rate() as f64;
                match self.engine.rebuild_plugins(
                    &self.host,
                    &self.vst3,
                    &self.clap,
                    &mut model,
                    &self.params,
                    sample_rate,
                ) {
                    Ok(()) => {
                        self.close_all_uis();
                        self.model = model;
                        self.params.set_clock_playing(false);
                        self.engine.sync_port_names(&self.model);
                        self.engine
                            .recorder()
                            .set_sources(&self.model.recorder.sources);
                        self.clear_loops();
                        self.rebuild_shared_bindings();
                        self.clear_history();
                        self.pending_session = Some((true, String::new(), String::new()));
                        Actions {
                            snapshot: true,
                            catalog: false,
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to rebuild plugins for new session");
                        self.pending_session = Some((false, String::new(), error.to_string()));
                        Actions::default()
                    }
                }
            }
            ControlMessage::LoadSession { path } => match load_session(&path) {
                Ok(mut loaded) => {
                    clamp_to_capacity(
                        &mut loaded,
                        self.params.strip_count(),
                        self.params.bus_count(),
                    );
                    trust_catalog_bundles(&mut loaded, &self.plugins);
                    apply_model(&self.params, &loaded);
                    if let Err(error) = self.engine.sync_strip_ports(&self.params) {
                        tracing::warn!(%error, "failed to sync strip ports for session");
                    }
                    if let Err(error) = self.engine.sync_bus_ports(&self.params) {
                        tracing::warn!(%error, "failed to sync bus ports for session");
                    }
                    if let Err(error) = self.engine.sync_bus_return_ports(&self.params) {
                        tracing::warn!(%error, "failed to sync bus return ports for session");
                    }
                    let sample_rate = self.engine.sample_rate() as f64;
                    match self.engine.rebuild_plugins(
                        &self.host,
                        &self.vst3,
                        &self.clap,
                        &mut loaded,
                        &self.params,
                        sample_rate,
                    ) {
                        Ok(()) => {
                            self.close_all_uis();
                            self.model = loaded;
                            self.ensure_model_plugin_controls();
                            save_catalog(&self.plugins);
                            self.engine.sync_port_names(&self.model);
                            self.engine
                                .recorder()
                                .set_sources(&self.model.recorder.sources);
                            self.load_loops_from_model();
                            self.params.set_clock_playing(false);
                            self.rebuild_shared_bindings();
                            self.missing_report =
                                missing_report_json(&self.engine.restore_connections(&self.model));
                            self.clear_history();
                            self.pending_session = Some((true, path.clone(), String::new()));
                            Actions {
                                snapshot: true,
                                catalog: true,
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "failed to rebuild plugins for session");
                            self.pending_session = Some((false, path.clone(), error.to_string()));
                            Actions::default()
                        }
                    }
                }
                Err(error) => {
                    self.pending_session = Some((false, path.clone(), error));
                    Actions::default()
                }
            },
            ControlMessage::SetBeatsPerBar { value } => {
                let value = value.clamp(1.0, 16.0);
                self.params.set_beats_per_bar(value);
                self.model.beats_per_bar = value;
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetTransportUi { json } => {
                if let Ok(ui) = serde_json::from_str::<vibemix_core::model::TransportUi>(&json) {
                    self.model.transport_ui = ui;
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetRecorderSources { json } => {
                if let Ok(sources) = serde_json::from_str::<Vec<RecorderSource>>(&json) {
                    self.model.recorder.sources = sources;
                    self.engine
                        .recorder()
                        .set_sources(&self.model.recorder.sources);
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetRecorderSettings { json } => {
                if let Ok(settings) = serde_json::from_str::<RecorderSettings>(&json) {
                    self.engine.recorder().set_sources(&settings.sources);
                    self.model.recorder = settings;
                }
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::RecorderAction { json } => {
                let mut snapshot = false;
                if let Ok(action) = serde_json::from_str::<RecorderAction>(&json) {
                    snapshot = self.handle_recorder_action(action);
                }
                Actions {
                    snapshot,
                    catalog: false,
                }
            }
            ControlMessage::RecorderStatus { .. }
            | ControlMessage::LoopList { .. }
            | ControlMessage::LoopWaveform { .. } => Actions::default(),
            ControlMessage::SetLayout {
                strips_per_row,
                buses_per_row,
            } => {
                self.model.layout.strips_per_row = strips_per_row;
                self.model.layout.buses_per_row = buses_per_row;
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::MidiLearn {
                kind,
                location,
                target,
                slot,
                param,
                min,
                max,
            } => {
                let target_kind = target_from_code(kind);
                let expected = self.target_channel(target_kind, location, target, slot);
                self.pending_learn = Some(MidiBinding {
                    channel: expected.unwrap_or(crate::midi_map::OMNI as u8),
                    cc: None,
                    key: None,
                    relative: false,
                    target: target_kind,
                    location,
                    target_id: target,
                    slot,
                    param,
                    min,
                    max,
                });
                self.engine.midi_map().start_learn(expected);
                Actions::default()
            }
            ControlMessage::MidiLearnCancel => {
                self.engine.midi_map().cancel_learn();
                self.pending_learn = None;
                Actions::default()
            }
            ControlMessage::MidiBindingsClear => {
                self.model.midi_bindings.clear();
                self.rebuild_shared_bindings();
                self.pending_learn = None;
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetMidiSync { on } => {
                self.params.set_midi_sync(on);
                self.model.midi_sync = on;
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetClockMaster { on } => {
                self.params.set_clock_master(on);
                self.params.set_clock_playing(false);
                self.model.clock.master = on;
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetClockBpm { bpm } => {
                let bpm = if bpm.is_finite() {
                    bpm.clamp(20.0, 400.0)
                } else {
                    120.0
                };
                let milli = (bpm * 100.0).round() as u32 * 10;
                self.params.set_clock_bpm_milli(milli);
                self.model.clock.bpm = milli as f32 / 1000.0;
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetClockPlaying { playing } => {
                if self.params.clock_master() {
                    self.params.set_clock_playing(playing);
                }
                Actions::default()
            }
            ControlMessage::SetStripMidiChannel { strip, channel } => {
                if let Some(entry) = self.model.strip_mut(StripId(strip)) {
                    entry.midi_channel = midi_channel_value(channel);
                }
                self.rebuild_shared_bindings();
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetBusMidiChannel { bus, channel } => {
                if let Some(entry) = self.model.bus_mut(BusId(bus)) {
                    entry.midi_channel = midi_channel_value(channel);
                }
                self.rebuild_shared_bindings();
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::SetPluginMidiChannel {
                location,
                target,
                slot,
                channel,
            } => {
                let value = midi_channel_value(channel);
                if location <= 1 {
                    if let Some(strip) = self.model.strip_mut(StripId(target)) {
                        if let Some(plugin) =
                            strip_plugin_list_mut(strip, location as usize).get_mut(slot as usize)
                        {
                            plugin.midi_channel = value;
                        }
                    }
                } else if let Some(plugin) = self
                    .model
                    .bus_mut(BusId(target))
                    .and_then(|bus| bus.plugins.get_mut(slot as usize))
                {
                    plugin.midi_channel = value;
                }
                self.rebuild_shared_bindings();
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::MidiBind {
                kind,
                location,
                target,
                slot,
                param,
                min,
                max,
                channel: _,
                cc,
            } => {
                let target_kind = target_from_code(kind);
                // The channel is always owned by the strip/bus/plugin, never
                // chosen per binding.
                let channel = self
                    .target_channel(target_kind, location, target, slot)
                    .unwrap_or(crate::midi_map::OMNI as u8);
                // Preserve a keyboard binding for the same target.
                let existing = self
                    .model
                    .midi_bindings
                    .iter()
                    .find(|binding| {
                        binding_targets(binding, target_kind, location, target, slot, param)
                    })
                    .map(|binding| (binding.key.clone(), binding.relative));
                let (key, relative) = existing.unwrap_or((None, false));
                self.model.midi_bindings.retain(|binding| {
                    !binding_targets(binding, target_kind, location, target, slot, param)
                });
                self.model.midi_bindings.push(MidiBinding {
                    channel,
                    cc: Some(cc.min(127) as u8),
                    key,
                    relative,
                    target: target_kind,
                    location,
                    target_id: target,
                    slot,
                    param,
                    min,
                    max,
                });
                self.rebuild_shared_bindings();
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::MidiUnbind {
                kind,
                location,
                target,
                slot,
                param,
            } => {
                let target_kind = target_from_code(kind);
                self.model.midi_bindings.retain(|binding| {
                    !binding_targets(binding, target_kind, location, target, slot, param)
                });
                self.rebuild_shared_bindings();
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::KeyBind {
                kind,
                location,
                target,
                slot,
                param,
                key,
            } => {
                let target_kind = target_from_code(kind);
                let channel = self
                    .target_channel(target_kind, location, target, slot)
                    .unwrap_or(crate::midi_map::OMNI as u8);
                // Keep any MIDI CC / relative flag already bound to this target.
                let existing = self
                    .model
                    .midi_bindings
                    .iter()
                    .find(|binding| {
                        binding_targets(binding, target_kind, location, target, slot, param)
                    })
                    .map(|binding| (binding.cc, binding.relative, binding.min, binding.max));
                let (cc, relative, min, max) = existing.unwrap_or((None, false, 0.0, 1.0));
                self.model.midi_bindings.retain(|binding| {
                    !binding_targets(binding, target_kind, location, target, slot, param)
                });
                let key = key.trim();
                // A key-only binding is dropped when the key is cleared and no
                // CC remains.
                if !key.is_empty() || cc.is_some() {
                    self.model.midi_bindings.push(MidiBinding {
                        channel,
                        cc,
                        key: (!key.is_empty()).then(|| key.to_string()),
                        relative,
                        target: target_kind,
                        location,
                        target_id: target,
                        slot,
                        param,
                        min,
                        max,
                    });
                }
                self.rebuild_shared_bindings();
                Actions {
                    snapshot: true,
                    catalog: false,
                }
            }
            ControlMessage::KeyTrigger { key } => {
                let mut changed = false;
                for binding in self.model.midi_bindings.iter() {
                    if binding.key.as_deref() == Some(key.as_str())
                        && apply_key_binding(&self.params, binding)
                    {
                        changed = true;
                    }
                }
                if changed {
                    sync_model_from_params(&self.params, &mut self.model);
                }
                Actions {
                    snapshot: changed,
                    catalog: false,
                }
            }
            ControlMessage::Clock { .. } => Actions::default(),
            ControlMessage::Meters(_) => Actions::default(),
        }
    }

    fn pause_rt(&self) {
        self.params.set_state_pause(true);
        let deadline = Instant::now() + Duration::from_millis(500);
        while !self.params.state_ack() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if !self.params.state_ack() {
            tracing::warn!("audio thread did not acknowledge the state pause in time");
        }
    }

    fn resume_rt(&self) {
        self.params.set_state_pause(false);
    }

    /// Re-seeds a plugin's sidechain gains/pre from its model config (sources
    /// are ignored while the sidechain is disabled).
    fn apply_sidechain_params(&mut self, location: u32, target: u32, slot: u32) {
        let entries: Vec<(usize, f32, bool)> = if let Some(chain) = strip_chain(location) {
            self.model
                .strip(StripId(target))
                .and_then(|strip| strip_plugin_list(strip, chain).get(slot as usize))
                .map(|plugin| {
                    plugin
                        .sidechain
                        .iter()
                        .map(|entry| (entry.strip.0 as usize, entry.gain.0, entry.pre_fader))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            self.model
                .bus(BusId(target))
                .and_then(|bus| bus.plugins.get(slot as usize))
                .map(|plugin| {
                    plugin
                        .sidechain
                        .iter()
                        .map(|entry| (entry.strip.0 as usize, entry.gain.0, entry.pre_fader))
                        .collect()
                })
                .unwrap_or_default()
        };
        let enabled = if let Some(chain) = strip_chain(location) {
            self.model
                .strip(StripId(target))
                .and_then(|strip| strip_plugin_list(strip, chain).get(slot as usize))
                .map(|plugin| plugin.sidechain_enabled)
                .unwrap_or(false)
        } else {
            self.model
                .bus(BusId(target))
                .and_then(|bus| bus.plugins.get(slot as usize))
                .map(|plugin| plugin.sidechain_enabled)
                .unwrap_or(false)
        };

        if let Some(chain) = strip_chain(location) {
            self.params
                .clear_sidechain(target as usize, chain, slot as usize);
            if enabled {
                for (source, gain, pre) in entries {
                    self.params.set_sidechain_gain(
                        target as usize,
                        chain,
                        slot as usize,
                        source,
                        gain,
                    );
                    self.params.set_sidechain_pre(
                        target as usize,
                        chain,
                        slot as usize,
                        source,
                        pre,
                    );
                }
            }
        } else {
            self.params
                .clear_bus_sidechain(target as usize, slot as usize);
            if enabled {
                for (source, gain, pre) in entries {
                    self.params.set_bus_sidechain_gain(
                        target as usize,
                        slot as usize,
                        source,
                        gain,
                    );
                    self.params
                        .set_bus_sidechain_pre(target as usize, slot as usize, source, pre);
                }
            }
        }
    }

    /// Records the outcome of a patchbay connect/disconnect, keeps the model's
    /// saved connections current (so a session save captures the live patch),
    /// and queues a result for the GUI.
    fn record_connection(&mut self, outcome: anyhow::Result<()>, source: String, sink: String) {
        let (ok, error) = match outcome {
            Ok(()) => {
                self.engine.capture_connections(&mut self.model);
                (true, String::new())
            }
            Err(error) => {
                tracing::warn!(%error, %source, %sink, "JACK connection change failed");
                (false, error.to_string())
            }
        };
        self.pending_connect_result = Some(ControlMessage::ConnectResult {
            ok,
            source,
            sink,
            error,
        });
    }

    /// The MIDI channel a target listens on: the owning strip/bus channel for
    /// mixer targets, or the owning plugin's channel for plugin targets.
    /// `None` means any channel (omni).
    fn target_channel(
        &self,
        target: MidiTarget,
        location: u32,
        target_id: u32,
        slot: u32,
    ) -> Option<u8> {
        match target {
            MidiTarget::StripGain
            | MidiTarget::StripPan
            | MidiTarget::StripMute
            | MidiTarget::StripSolo
            | MidiTarget::SendGain
            | MidiTarget::SendDelay => self
                .model
                .strip(StripId(target_id))
                .and_then(|strip| strip.midi_channel),
            MidiTarget::BusGain | MidiTarget::BusMute => self
                .model
                .bus(BusId(target_id))
                .and_then(|bus| bus.midi_channel),
            MidiTarget::PluginParam | MidiTarget::PluginBypass => {
                if location <= 1 {
                    let strip = self.model.strip(StripId(target_id))?;
                    strip_plugin_list(strip, location as usize)
                        .get(slot as usize)?
                        .midi_channel
                } else {
                    self.model
                        .bus(BusId(target_id))?
                        .plugins
                        .get(slot as usize)?
                        .midi_channel
                }
            }
        }
    }

    /// Pushes the effective bindings to the audio-thread table, assigning each
    /// binding the channel of its owning strip/bus/plugin (`OMNI` when unset).
    fn rebuild_shared_bindings(&mut self) {
        let mut effective = self.model.midi_bindings.clone();
        for binding in &mut effective {
            binding.channel = self
                .target_channel(
                    binding.target,
                    binding.location,
                    binding.target_id,
                    binding.slot,
                )
                .unwrap_or(crate::midi_map::OMNI as u8);
        }
        self.engine.midi_map().set_bindings(&effective);
    }

    fn send_catalog(&self, address: &Path) {
        let message = ControlMessage::Catalog {
            json: self.catalog_json.clone(),
        };
        self.send_message(&message, address);
    }

    /// Encodes and sends a control message, splitting it into `Chunk` datagrams
    /// when it exceeds the UDP payload limit. Failures are logged: an oversized
    /// message used to be dropped silently, which hid the plugin catalog.
    /// Sends one datagram without ever blocking the control loop: a full queue
    /// (a GUI that is not keeping up) drops it, like UDP did, and a vanished
    /// peer is remembered so it is not addressed again.
    fn send_datagram(&self, data: &[u8], address: &Path) {
        use vibemix_proto::socket::{peer_is_gone, send_nonblocking};
        match send_nonblocking(&self.socket, data, address) {
            Ok(()) => {}
            Err(error) if peer_is_gone(&error) => self.peer_gone.set(true),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                tracing::warn!(%error, address = %address.display(), "failed to send control message");
            }
        }
    }

    fn send_message(&self, message: &ControlMessage, address: &Path) {
        match message.encode_datagrams() {
            Ok(datagrams) => {
                for datagram in datagrams {
                    self.send_datagram(&datagram, address);
                }
            }
            Err(error) => tracing::warn!(%error, "failed to encode control message"),
        }
    }

    fn send_plugin_scan(&self, address: &Path) {
        let message = ControlMessage::PluginScan {
            running: self.scan.running,
            done: self.scan.done as u32,
            total: self.scan.total as u32,
        };
        self.send_message(&message, address);
    }

    fn send_state(&self, address: &Path) {
        let message = ControlMessage::State {
            json: build_state_json(
                &self.model,
                &self.params,
                &self.plugins,
                self.sample_rate,
                self.engine.xrun_count(),
                self.meters.dsp_load(),
            ),
        };
        self.send_message(&message, address);
    }

    fn send_missing(&self, address: &Path, json: &str) {
        let message = ControlMessage::MissingConnections {
            json: json.to_string(),
        };
        self.send_message(&message, address);
    }

    fn send_session_result(&self, address: &Path, ok: bool, path: &str, error: &str) {
        let message = ControlMessage::SessionResult {
            ok,
            path: path.to_string(),
            error: error.to_string(),
        };
        self.send_message(&message, address);
    }

    fn send_clipboard(&self, address: &Path) {
        let (has, location, name) = match &self.clipboard {
            Some(clipboard) => (true, clipboard.location, clipboard.slot.name.clone()),
            None => (false, 0, String::new()),
        };
        let message = ControlMessage::Clipboard {
            has,
            location,
            name,
        };
        self.send_message(&message, address);
    }

    fn send_edit_state(&self, address: &Path) {
        let message = ControlMessage::EditState {
            undo_depth: self.undo_stack.len() as u32,
            redo_depth: self.redo_stack.len() as u32,
            undo_label: self
                .undo_stack
                .back()
                .map(|entry| entry.label.clone())
                .unwrap_or_default(),
            redo_label: self
                .redo_stack
                .last()
                .map(|entry| entry.label.clone())
                .unwrap_or_default(),
        };
        self.send_message(&message, address);
    }
}

/// Returns the undo label and (for continuous parameters) the coalescing key
/// for a mutating message, or `None` if the message does not change the model.
fn edit_label(message: &ControlMessage) -> Option<(&'static str, Option<EditKey>)> {
    use ControlMessage::*;
    Some(match message {
        SetStripGain { strip, .. } => ("Set strip gain", Some(EditKey::StripGain(*strip))),
        SetStripPan { strip, .. } => ("Set strip pan", Some(EditKey::StripPan(*strip))),
        SetStripInputGain { strip, .. } => ("Set input gain", Some(EditKey::StripInput(*strip))),
        SetStripMute { .. } => ("Toggle strip mute", None),
        SetStripSolo { .. } => ("Toggle strip solo", None),
        SetStripName { .. } => ("Rename strip", None),
        MoveStrip { .. } => ("Move strip", None),
        SetBusGain { bus, .. } => ("Set bus gain", Some(EditKey::BusGain(*bus))),
        SetBusSendGain { bus, .. } => ("Set bus send gain", Some(EditKey::BusSendGain(*bus))),
        SetBusReturnGain { bus, .. } => ("Set bus return gain", Some(EditKey::BusReturnGain(*bus))),
        SetBusMute { .. } => ("Toggle bus mute", None),
        SetBusName { .. } => ("Rename bus", None),
        MoveBus { .. } => ("Move bus", None),
        SetBusMixMaster { .. } => ("Toggle mix to master", None),
        SetBusReturn { .. } => ("Toggle external return", None),
        SetSendGain { strip, bus, .. } => ("Set send gain", Some(EditKey::SendGain(*strip, *bus))),
        SetSendDelay { strip, bus, .. } => {
            ("Set send delay", Some(EditKey::SendDelay(*strip, *bus)))
        }
        AddStrip => ("Add strip", None),
        AddBus => ("Add bus", None),
        RemoveStrip { .. } => ("Remove strip", None),
        RemoveBus { .. } => ("Remove bus", None),
        DuplicateStrip { .. } => ("Duplicate strip", None),
        DuplicateBus { .. } => ("Duplicate bus", None),
        AddPlugin { .. } => ("Add plugin", None),
        RemovePlugin { .. } => ("Remove plugin", None),
        MovePlugin { .. } => ("Move plugin", None),
        SetPluginParam {
            location,
            target,
            slot,
            param,
            ..
        } => (
            "Set plugin parameter",
            Some(EditKey::PluginParam(*location, *target, *slot, *param)),
        ),
        SetPluginBypass { .. } => ("Toggle plugin bypass", None),
        CutPlugin { .. } => ("Cut plugin", None),
        PastePlugin { .. } => ("Paste plugin", None),
        SetBeatsPerBar { .. } => ("Set beats per bar", None),
        _ => return None,
    })
}

/// Whether `binding` drives the given mixer target.
fn binding_targets(
    binding: &MidiBinding,
    kind: MidiTarget,
    location: u32,
    target_id: u32,
    slot: u32,
    param: u32,
) -> bool {
    binding.target == kind
        && binding.location == location
        && binding.target_id == target_id
        && binding.slot == slot
        && binding.param == param
}

/// Applies a keyboard-triggered binding by toggling its button target. Returns
/// false for targets that are not buttons (so keys never touch continuous
/// controls).
fn apply_key_binding(params: &SharedParams, binding: &MidiBinding) -> bool {
    let id = binding.target_id as usize;
    let slot = binding.slot as usize;
    match binding.target {
        MidiTarget::StripMute => {
            params.set_strip_mute(id, !params.strip_mute(id));
            true
        }
        MidiTarget::StripSolo => {
            params.set_strip_solo(id, !params.strip_solo(id));
            true
        }
        MidiTarget::BusMute => {
            params.set_bus_mute(id, !params.bus_mute(id));
            true
        }
        MidiTarget::PluginBypass => {
            if binding.location <= 1 {
                let chain = binding.location as usize;
                params.set_plugin_bypass(id, chain, slot, !params.plugin_bypass(id, chain, slot));
            } else {
                params.set_bus_plugin_bypass(id, slot, !params.bus_plugin_bypass(id, slot));
            }
            true
        }
        _ => false,
    }
}

/// Converts a raw control-protocol channel (`0..15`, or `>= OMNI` for "any")
/// into the model's optional channel.
fn midi_channel_value(channel: u32) -> Option<u8> {
    if channel >= crate::midi_map::OMNI {
        None
    } else {
        Some(channel.min(15) as u8)
    }
}

const MAX_NAME_LEN: usize = 64;

fn sanitize_name(name: String) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_NAME_LEN).collect())
}

/// Outcome of a plugin smoke test.
enum SmokeVerdict {
    Pass,
    Fail(String),
    /// The child could not be spawned or polled.
    Inconclusive,
}

/// A plugin insert held back until its smoke test finishes.
struct DeferredInsert {
    key: String,
    location: u32,
    target: u32,
    plugin: PluginSlot,
}

#[cfg(test)]
mod fuzz_tests;

#[cfg(test)]
mod overwrite_guard_tests {
    use super::*;

    #[test]
    fn saving_a_session_will_not_clobber_other_files() {
        let dir = std::env::temp_dir().join(format!("vibemix-guard-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let other = dir.join("bashrc");
        fs::write(&other, "export PATH=/usr/bin\n").unwrap();
        let model = MixerModel::default_minimal();

        assert!(save_session(&model, other.to_str().unwrap()).is_err());
        assert_eq!(
            fs::read_to_string(&other).unwrap(),
            "export PATH=/usr/bin\n"
        );

        let session = dir.join("a.toml");
        assert!(save_session(&model, session.to_str().unwrap()).is_ok());
        assert!(save_session(&model, session.to_str().unwrap()).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod bundle_trust_tests {
    use super::*;
    use vibemix_core::ids::PluginId;

    #[test]
    fn session_bundle_paths_are_not_trusted() {
        let mut model = MixerModel::default_minimal();
        let mut plugin = PluginSlot::new(PluginId(0), "unknown-id");
        plugin.format = PluginFormat::Clap;
        plugin.bundle = Some("/tmp/evil.clap".to_string());
        model.strips[0].plugins.push(plugin);

        trust_catalog_bundles(&mut model, &[]);

        assert_eq!(model.strips[0].plugins[0].bundle, None);
    }
}

#[cfg(test)]
mod param_sync_tests {
    use super::*;

    #[test]
    fn live_param_changes_reach_the_model() {
        let mut model = MixerModel::default_minimal();
        let params = SharedParams::new(4, 4);
        apply_model(&params, &model);

        params.set_strip_gain(0, 0.25);
        params.set_strip_pan(0, -0.5);
        params.set_strip_mute(0, true);
        params.set_bus_gain(0, 0.5);
        params.set_send_gain(0, 0, 0.75);
        params.set_send_delay(0, 0, 12.0);

        sync_model_from_params(&params, &mut model);

        assert_eq!(model.strips[0].gain.0, 0.25);
        assert_eq!(model.strips[0].pan.0, -0.5);
        assert!(model.strips[0].mute);
        assert_eq!(model.buses[0].gain.0, 0.5);
        assert_eq!(model.strips[0].sends[0].gain.0, 0.75);
        assert_eq!(model.strips[0].sends[0].delay_ms, 12.0);
    }
}

#[cfg(test)]
mod sidechain_order_tests {
    use super::*;
    use vibemix_core::ids::{PluginId, StripId};
    use vibemix_core::model::{PluginSlot, Strip};

    fn model_with_sidechain(self_cycle: bool) -> MixerModel {
        let mut model = MixerModel::default_minimal();
        model.strips.push(Strip::new(StripId(1), "b"));

        // Strip 0's plugin takes its sidechain from strip 1.
        let mut plugin = PluginSlot::new(PluginId(0), "x");
        plugin.sidechain.push(SidechainSource::new(if self_cycle {
            StripId(0)
        } else {
            StripId(1)
        }));
        model.strips[0].plugins.push(plugin);
        model
    }

    #[test]
    fn contributors_run_before_consumers() {
        let model = model_with_sidechain(false);
        let (order, delayed) = compute_strip_order(&model, 2);
        assert_eq!(
            order,
            vec![1, 0],
            "source strip 1 must precede consumer strip 0"
        );
        assert!(delayed
            .iter()
            .all(|c| c.iter().all(|s| s.iter().all(|d| !d))));
    }

    #[test]
    fn self_cycle_is_delayed() {
        let model = model_with_sidechain(true);
        let (_, delayed) = compute_strip_order(&model, 2);
        assert!(delayed[0][0][0], "self-sidechain consumer must be delayed");
    }

    #[test]
    fn missing_plugin_is_flagged_in_state() {
        let mut plugin = PluginSlot::new(PluginId(0), "http://example.com/gone");
        plugin.missing = true;
        let list = plugin_list_json(&[plugin], &[], |_, _| 0.0);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["missing"], serde_json::json!(true));
    }
}
