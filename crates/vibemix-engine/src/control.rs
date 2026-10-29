#![allow(clippy::items_after_test_module)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
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
    detect_transient, detect_transients, peaks, rotate_interleaved, snap_seam, LoopSlot,
    LoopWaveform, RecorderAction, RecorderSettings, RecorderSource, RecorderStatus, Transient,
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

const PLUGIN_LOC_STRIP_PRE: u32 = 0;
const PLUGIN_LOC_STRIP_POST: u32 = 1;

const METER_INTERVAL: Duration = Duration::from_millis(20);
/// Minimum gap between catalog rebuilds while the plugin sweep streams results.
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
    socket: UdpSocket,
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

impl Controller {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        port: u16,
        params: Arc<SharedParams>,
        meters: Arc<SharedMeters>,
        model: MixerModel,
        engine: JackEngine,
        host: Host,
        vst3: vibemix_vst3::Host,
        clap: vibemix_clap::Host,
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("127.0.0.1", port))?;
        socket.set_read_timeout(Some(METER_INTERVAL))?;

        let plugins: Vec<PluginEntry> = {
            let discovered = discover_catalog(&host, &vst3, &clap);
            if discovered.is_empty() {
                load_catalog()
            } else {
                discovered
            }
        };
        let health = HealthStore::load();
        let catalog_json = build_catalog(&plugins, &health);
        save_catalog(&plugins);

        let mut engine = engine;
        let missing_report = missing_report_json(&engine.take_missing_connections());
        // Track the rate the graph was built with, so a startup rate correction
        // from PipeWire (e.g. 48000 -> 44100) triggers a resync in the loop.
        let sample_rate = engine.graph_sample_rate();

        let mut controller = Self {
            socket,
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
            uis: HashMap::new(),
            ui_cache: HashMap::new(),
            ui_safe: Arc::new(Mutex::new(HashMap::new())),
            vst3_ui_safe: Arc::new(Mutex::new(HashMap::new())),
            clap_ui_safe: Arc::new(Mutex::new(HashMap::new())),
            missing_report,
            pending_session: None,
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
        let mut target: Option<SocketAddr> = None;
        let mut scan_was_running = false;
        let mut last_catalog = Instant::now() - CATALOG_THROTTLE;

        loop {
            let mut actions = Actions::default();

            match self.socket.recv_from(&mut buffer) {
                Ok((length, address)) => {
                    target = Some(address);
                    if let Some(message) = ControlMessage::decode(&buffer[..length]) {
                        actions = self.apply(message);
                    }
                }
                Err(error) => {
                    use std::io::ErrorKind;
                    if !matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
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
                actions.snapshot = true;
            }

            let Some(address) = target else {
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
            }
            if actions.snapshot {
                self.send_state(address);
                self.send_recorder(address);
                self.send_loops(address);
                self.recorder_dirty = false;
                self.loops_dirty = false;
            } else if param_edits {
                self.send_state(address);
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
            if self.edit_dirty || actions.snapshot || param_edits {
                self.edit_dirty = false;
                self.send_edit_state(address);
            }

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
            let _ = self.socket.send_to(&self.meter_bytes, address);

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
                let _ = self.socket.send_to(&bytes, address);
            }
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
                self.plugins = discover_catalog(&self.host, &self.vst3, &self.clap);
                if self.plugins.is_empty() {
                    self.plugins = load_catalog();
                }
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
                if let Ok(action) = serde_json::from_str::<RecorderAction>(&json) {
                    self.handle_recorder_action(action);
                }
                Actions::default()
            }
            ControlMessage::RecorderStatus { .. }
            | ControlMessage::LoopList { .. }
            | ControlMessage::LoopWaveform { .. } => {
                Actions::default()
            }
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
                Actions {
                    snapshot: changed,
                    catalog: false,
                }
            }
            ControlMessage::Clock { .. } => Actions::default(),
            ControlMessage::Meters(_) => Actions::default(),
        }
    }

    fn capture_plugin_states(&mut self) {
        self.pause_rt();

        for strip in &mut self.model.strips {
            let strip_index = strip.id.0 as usize;
            for chain in 0..PLUGIN_CHAINS {
                let plugins = strip_plugin_list_mut(strip, chain);
                for (slot, plugin) in plugins.iter_mut().enumerate() {
                    // Persist the live control values (written by either the
                    // editor or the plugin's native UI) into the model.
                    if let Some(controls) = self
                        .plugins
                        .iter()
                        .find(|info| info.id == plugin.uri)
                        .map(|info| info.controls.len().min(MAX_PARAMS))
                    {
                        if plugin.params.len() < controls {
                            plugin.params.resize(controls, 0.0);
                        }
                        for param in 0..controls {
                            plugin.params[param] =
                                self.params.plugin_param(strip_index, chain, slot, param);
                        }
                    }
                    if let Some(state) = self.engine.state_proxy(strip_index, chain, slot) {
                        if let Some(encoded) = state.encode(&self.host) {
                            plugin.state = encoded;
                        }
                    }
                }
            }
        }

        for bus in &mut self.model.buses {
            let bus_index = bus.id.0 as usize;
            for (slot, plugin) in bus.plugins.iter_mut().enumerate() {
                if let Some(controls) = self
                    .plugins
                    .iter()
                    .find(|info| info.id == plugin.uri)
                    .map(|info| info.controls.len().min(MAX_PARAMS))
                {
                    if plugin.params.len() < controls {
                        plugin.params.resize(controls, 0.0);
                    }
                    for param in 0..controls {
                        plugin.params[param] = self.params.bus_plugin_param(bus_index, slot, param);
                    }
                }
                if let Some(state) = self.engine.bus_state_proxy(bus_index, slot) {
                    if let Some(encoded) = state.encode(&self.host) {
                        plugin.state = encoded;
                    }
                }
            }
        }

        self.resume_rt();
    }

    fn pause_rt(&self) {
        self.params.set_state_pause(true);
        let deadline = Instant::now() + Duration::from_millis(500);
        while !self.params.state_ack() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn resume_rt(&self) {
        self.params.set_state_pause(false);
    }

    /// Starts a plugin health sweep: a full one when the cache is stale/empty,
    /// otherwise a targeted one for newly installed plugins (if any).
    fn start_health_scan(&mut self) {
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

    fn record_health(&mut self, uri: &str, status: Status, reason: String) {
        self.health.set(uri, status, reason, VerdictMeta::default());
        self.health_dirty = true;
        self.health_immediate = true;
    }

    /// Runs the engine binary as a throwaway smoke test for `key` (an LV2 URI or
    /// a VST3 class id), caching and recording the verdict.
    fn run_smoke_child(&mut self, key: &str, args: &[&str]) -> bool {
        if self.smoke_ok.contains(key) {
            return true;
        }
        if self.smoke_bad.contains(key) {
            return false;
        }

        let Ok(executable) = std::env::current_exe() else {
            return true;
        };
        let mut command = Command::new(executable);
        command.args(args);
        if crate::smoke::debug_enabled() {
            // Leave stdio inherited so a plugin's own diagnostics stay visible.
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        // The child disables its own core dumps (see the `run_smoke*` entry
        // points), so no `pre_exec` is needed here. That matters: `pre_exec`
        // forces `fork()` in the multithreaded engine (RT thread, health
        // reader, plugin UI threads), which can deadlock the child while it
        // holds a lock another thread took at fork time.
        let Ok(mut child) = command.spawn() else {
            return true;
        };

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    self.smoke_ok.insert(key.to_string());
                    self.record_health(key, Status::Ok, String::new());
                    return true;
                }
                Ok(Some(status)) => {
                    self.smoke_bad.insert(key.to_string());
                    let reason = if let Some(signal) = status.signal() {
                        let name = crate::smoke::signal_name(signal);
                        tracing::warn!(plugin = %key, signal, "plugin crashed during smoke test, refusing to load");
                        name.to_string()
                    } else {
                        let code = status.code().unwrap_or(-1);
                        tracing::warn!(plugin = %key, code, "plugin failed smoke test, refusing to load");
                        format!("exit {code}")
                    };
                    self.record_health(key, Status::Failed, reason);
                    return false;
                }
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        self.smoke_bad.insert(key.to_string());
                        tracing::warn!(plugin = %key, "plugin smoke test timed out, refusing to load");
                        self.record_health(key, Status::Failed, "timeout".to_string());
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return true,
            }
        }
    }

    fn plugin_is_safe(&mut self, uri: &str) -> bool {
        self.run_smoke_child(uri, &["--smoke", uri])
    }

    fn vst3_is_safe(&mut self, bundle: &str, class_id: &str) -> bool {
        self.run_smoke_child(class_id, &["--smoke-vst3", bundle, class_id])
    }

    fn clap_is_safe(&mut self, bundle: &str, plugin_id: &str) -> bool {
        self.run_smoke_child(plugin_id, &["--smoke-clap", bundle, plugin_id])
    }

    fn ui_is_safe(&mut self, uri: &str) -> bool {
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
    fn vst3_ui_is_safe(&mut self, bundle: &str, class_id: &str) -> bool {
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
    fn clap_ui_is_safe(&mut self, bundle: &str, plugin_id: &str) -> bool {
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
    fn schedule_ui_smoke(&self, format: PluginFormat, uri: &str, bundle: Option<&str>) {
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

    fn open_ui(&mut self, location: u32, target: u32, slot: u32) {
        let key = (location, target, slot);
        if self.uis.contains_key(&key) {
            return;
        }
        let plugin = if let Some(chain) = strip_chain(location) {
            self.model
                .strip(StripId(target))
                .and_then(|entry| strip_plugin_list(entry, chain).get(slot as usize))
                .map(|plugin| {
                    (
                        plugin.uri.clone(),
                        plugin.format,
                        plugin.bundle.clone(),
                        plugin.name.clone(),
                    )
                })
        } else {
            self.model
                .bus(BusId(target))
                .and_then(|entry| entry.plugins.get(slot as usize))
                .map(|plugin| {
                    (
                        plugin.uri.clone(),
                        plugin.format,
                        plugin.bundle.clone(),
                        plugin.name.clone(),
                    )
                })
        };
        let Some((uri, format, bundle, model_name)) = plugin else {
            return;
        };

        // Title the native window with the plugin's name (user rename wins,
        // then the catalog name, then the URI).
        let title = if model_name.is_empty() {
            self.plugins
                .iter()
                .find(|info| info.id == uri)
                .map(|info| info.name.clone())
                .unwrap_or_else(|| uri.clone())
        } else {
            model_name
        };

        if format == PluginFormat::Vst3 {
            let Some(bundle) = bundle else {
                tracing::warn!(%uri, "VST3 plugin has no bundle for editor");
                return;
            };
            if !self.vst3_ui_is_safe(&bundle, &uri) {
                return;
            }
            let state = if let Some(chain) = strip_chain(location) {
                self.engine
                    .state_proxy(target as usize, chain, slot as usize)
            } else {
                self.engine.bus_state_proxy(target as usize, slot as usize)
            };
            let Some(handle) = state.and_then(|state| state.vst3_handle()) else {
                tracing::warn!(%uri, "no VST3 instance for editor");
                return;
            };
            let Some(view) = handle.open_view(&title) else {
                tracing::warn!(%uri, "failed to open VST3 editor");
                return;
            };
            tracing::info!(location, target, slot, %uri, "opened VST3 editor");
            self.uis.insert(key, OpenUi::Vst3 { view, handle });
            return;
        }

        if format == PluginFormat::Clap {
            let Some(bundle) = bundle else {
                tracing::warn!(%uri, "CLAP plugin has no bundle for editor");
                return;
            };
            if !self.clap_ui_is_safe(&bundle, &uri) {
                return;
            }
            let state = if let Some(chain) = strip_chain(location) {
                self.engine
                    .state_proxy(target as usize, chain, slot as usize)
            } else {
                self.engine.bus_state_proxy(target as usize, slot as usize)
            };
            let Some(handle) = state.and_then(|state| state.clap_handle()) else {
                tracing::warn!(%uri, "no CLAP instance for editor");
                return;
            };
            let Some(view) = handle.open_view(&title) else {
                tracing::warn!(%uri, "failed to open CLAP editor");
                return;
            };
            tracing::info!(location, target, slot, %uri, "opened CLAP editor");
            self.uis.insert(key, OpenUi::Clap { view, handle });
            return;
        }

        let handle = if let Some(chain) = strip_chain(location) {
            self.engine
                .plugin_handle(target as usize, chain, slot as usize)
        } else {
            self.engine
                .bus_plugin_handle(target as usize, slot as usize)
        };

        let candidate = self
            .ui_cache
            .entry(uri.clone())
            .or_insert_with(|| find_ui(&uri))
            .clone();
        let Some(candidate) = candidate else {
            tracing::warn!(%uri, "no hostable plugin UI");
            return;
        };

        let ports: Vec<(u32, usize)> = self
            .plugins
            .iter()
            .find(|info| info.id == uri)
            .map(|info| {
                info.controls
                    .iter()
                    .enumerate()
                    .map(|(param, control)| (control.index as u32, param))
                    .collect()
            })
            .unwrap_or_default();

        let outputs = if let Some(chain) = strip_chain(location) {
            self.engine
                .plugin_outputs(target as usize, chain, slot as usize)
        } else {
            self.engine
                .bus_plugin_outputs(target as usize, slot as usize)
        };
        let ui_inputs = if let Some(chain) = strip_chain(location) {
            self.engine
                .plugin_ui_inputs(target as usize, chain, slot as usize)
        } else {
            self.engine
                .bus_plugin_ui_inputs(target as usize, slot as usize)
        };

        let mut context = Box::new(UiWriteCtx {
            location,
            target: target as usize,
            slot: slot as usize,
            params: Arc::clone(&self.params),
            ports,
            outputs,
            ui_inputs,
            last_control: Vec::new(),
            last_atom_seq: Vec::new(),
        });

        // Prefer hosting the editor out of process, so constructing a toolkit UI
        // cannot stall the engine's data-loop. UIs that read the instance
        // directly (`instance-access`), and explicit local requests, use the
        // in-process path.
        let force_local = std::env::var_os("VIBEMIX_UI_LOCAL").is_some();
        if !force_local && !candidate.requires_instance_access {
            let id = self.ui_service.alloc_id();
            if self
                .ui_service
                .open(id, &candidate, &uri, &title, 1.0, 30.0)
            {
                if let Some(outputs) = &context.outputs {
                    outputs.set_active(true);
                }
                tracing::info!(location, target, slot, %uri, "opened plugin UI (out of process)");
                self.uis.insert(key, OpenUi::Lv2Remote { id, context });
                return;
            }
            tracing::warn!(%uri, "UI host failed; falling back in-process");
        }

        let Some(handle) = handle else {
            return;
        };
        if !self.ui_is_safe(&uri) {
            return;
        }
        let Some(window) = (unsafe {
            self.host.open_ui(
                &uri,
                &candidate,
                handle,
                ui_port_write,
                &mut *context as *mut UiWriteCtx as *mut std::ffi::c_void,
                false,
                &title,
            )
        }) else {
            tracing::warn!(%uri, "failed to open plugin UI");
            return;
        };

        if let Some(outputs) = &context.outputs {
            outputs.set_active(true);
        }

        tracing::info!(location, target, slot, %uri, "opened plugin UI");
        self.uis.insert(key, OpenUi::Lv2Local { window, context });
    }

    /// Mirrors a host-driven parameter change into the plugin's native UI, so
    /// the UI's own controls follow edits made in the vibemix editor.
    /// Mirrors a host-driven parameter change into the plugin's open editor.
    ///
    /// Runs on the control/UI thread, never the audio thread. For VST3 this
    /// calls `IEditController::setParamNormalized`, which the spec sanctions
    /// from the UI thread (the processor is driven separately by the parameter
    /// queue in `set_controls`); plugins are required to make that reentrant.
    fn reflect_param_to_ui(
        &mut self,
        location: u32,
        target: u32,
        slot: u32,
        param: u32,
        value: f32,
    ) {
        let mut remote = None;
        let mut local = None;
        if let Some(ui) = self.uis.get(&(location, target, slot)) {
            let find = |context: &UiWriteCtx| {
                context
                    .ports
                    .iter()
                    .find(|(_, mapped)| *mapped == param as usize)
                    .map(|(port, _)| *port)
            };
            match ui {
                OpenUi::Lv2Remote { id, context } => {
                    if let Some(port) = find(context) {
                        remote = Some((*id, port));
                    }
                }
                OpenUi::Lv2Local { window, context } => {
                    if let Some(port) = find(context) {
                        local = Some((port, window));
                    }
                }
                OpenUi::Vst3 { view, .. } => {
                    view.set_param(param as usize, value);
                    return;
                }
                OpenUi::Clap { view, .. } => {
                    view.set_param(param as usize, value);
                    return;
                }
            }
        }
        if let Some((id, port)) = remote {
            self.ui_service
                .send_port_event(id, port, 0, None, value.to_ne_bytes().to_vec());
        } else if let Some((port, window)) = local {
            window.port_event(
                port,
                std::mem::size_of::<f32>() as u32,
                0,
                &value.to_ne_bytes(),
            );
        }
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

    fn apply_vst3_param(&self, location: u32, target: u32, slot: u32, param: usize, value: f32) {
        if let Some(chain) = strip_chain(location) {
            self.params
                .set_plugin_param(target as usize, chain, slot as usize, param, value);
        } else {
            self.params
                .set_bus_plugin_param(target as usize, slot as usize, param, value);
        }
    }

    /// Folds parameter moves made in open VST3 editors into the shared state
    /// (mirrors the LV2 `ui:portEvent` write-back). Returns true if any moved.
    fn drain_vst3_param_edits(&self) -> bool {
        let mut changed = false;
        for (key, ui) in self.uis.iter() {
            let OpenUi::Vst3 { handle, .. } = ui else {
                continue;
            };
            let (location, target, slot) = *key;
            while let Some((param, value)) = handle.take_param_edit() {
                self.apply_vst3_param(location, target, slot, param, value);
                changed = true;
            }

            // The plugin may have changed its own parameters, latency or IO.
            let flags = handle.take_restart_flags();
            if flags & VST3_RESTART_PARAM_VALUES != 0 {
                for param in 0..handle.param_count().min(MAX_PARAMS) {
                    let value = handle.param_value(param);
                    self.apply_vst3_param(location, target, slot, param, value);
                }
                changed = true;
            }
            if flags & (VST3_RESTART_LATENCY | VST3_RESTART_IO | VST3_RESTART_RELOAD) != 0 {
                let latency = handle.latency();
                tracing::info!(
                    flags,
                    latency,
                    location,
                    target,
                    slot,
                    "VST3 plugin requested restart"
                );
            }
        }
        changed
    }

    /// Folds parameter moves made in open CLAP editors into the shared state
    /// (mirrors the VST3 editor write-back). Returns true if any moved.
    fn drain_clap_param_edits(&self) -> bool {
        let mut changed = false;
        for (key, ui) in self.uis.iter() {
            let OpenUi::Clap { handle, .. } = ui else {
                continue;
            };
            let (location, target, slot) = *key;
            while let Some((param, value)) = handle.take_param_edit() {
                self.apply_vst3_param(location, target, slot, param, value);
                changed = true;
            }
        }
        changed
    }

    fn pump_uis(&mut self) {
        self.poll_ui_host();

        let mut closed = Vec::new();
        {
            let Controller {
                uis, ui_service, ..
            } = self;
            for (key, ui) in uis.iter_mut() {
                match ui {
                    OpenUi::Lv2Remote { id, context } => {
                        // The UI host pumps its own windows; the engine only
                        // forwards output snapshots.
                        for event in context.collect_outputs() {
                            ui_service.send_port_event(
                                *id,
                                event.port,
                                event.format,
                                event.type_uri,
                                event.bytes,
                            );
                        }
                    }
                    OpenUi::Lv2Local { window, context } => {
                        window.pump();
                        for event in context.collect_outputs() {
                            window.port_event(
                                event.port,
                                event.bytes.len() as u32,
                                event.format,
                                &event.bytes,
                            );
                        }
                        if window.wants_close() {
                            closed.push(*key);
                        }
                    }
                    OpenUi::Vst3 { view, .. } => {
                        view.pump();
                        if view.wants_close() {
                            closed.push(*key);
                        }
                    }
                    OpenUi::Clap { view, .. } => {
                        view.pump();
                        if view.wants_close() {
                            closed.push(*key);
                        }
                    }
                }
            }
        }
        for key in closed {
            self.uis.remove(&key);
        }
    }

    /// Applies messages from the out-of-process UI host: parameter edits and
    /// window-close requests.
    fn poll_ui_host(&mut self) {
        for message in self.ui_service.recv() {
            match message {
                UiMessage::Write {
                    id,
                    port,
                    format,
                    bytes,
                } => {
                    if format == 0 {
                        if bytes.len() != 4 {
                            continue;
                        }
                        let value = f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                        self.apply_remote_ui_write(id, port, value);
                    } else if let Some(inputs) = self.remote_ui_inputs(id) {
                        // Atom event from the UI: deliver it to the plugin input.
                        inputs.push(port, &bytes);
                    }
                }
                UiMessage::UridRequest { uri } => {
                    let urid = vibemix_lv2::urid::UridMap::shared().map(&uri);
                    self.ui_service.send_urid_reply(&uri, urid);
                }
                UiMessage::CloseRequest { id } => {
                    let key = self.uis.iter().find_map(|(key, ui)| match ui {
                        OpenUi::Lv2Remote { id: ui_id, .. } if *ui_id == id => Some(*key),
                        _ => None,
                    });
                    if let Some(key) = key {
                        self.uis.remove(&key);
                    }
                }
                UiMessage::Title { .. }
                | UiMessage::Opened { .. }
                | UiMessage::OpenFailed { .. } => {}
                UiMessage::Open { .. }
                | UiMessage::PortEvent { .. }
                | UiMessage::Close { .. }
                | UiMessage::UridTable { .. }
                | UiMessage::UridReply { .. }
                | UiMessage::Shutdown => {}
            }
        }
    }

    /// The UI→DSP mailbox for a remote UI, if one is open.
    fn remote_ui_inputs(&self, id: u32) -> Option<Arc<SharedUiInputs>> {
        self.uis.values().find_map(|ui| match ui {
            OpenUi::Lv2Remote { id: ui_id, context } if *ui_id == id => context.ui_inputs.clone(),
            _ => None,
        })
    }

    /// Folds a control-port edit made in a remote editor into the shared
    /// parameter state (the remote equivalent of `ui_port_write`).
    fn apply_remote_ui_write(&mut self, id: u32, port: u32, value: f32) {
        let mut found = None;
        for (key, ui) in self.uis.iter() {
            if let OpenUi::Lv2Remote { id: ui_id, context } = ui {
                if *ui_id == id {
                    found = context
                        .ports
                        .iter()
                        .find(|(mapped, _)| *mapped == port)
                        .map(|(_, param)| (*key, *param));
                    break;
                }
            }
        }
        let Some(((location, target, slot), param)) = found else {
            return;
        };
        if let Some(chain) = strip_chain(location) {
            self.params
                .set_plugin_param(target as usize, chain, slot as usize, param, value);
        } else {
            self.params
                .set_bus_plugin_param(target as usize, slot as usize, param, value);
        }
    }

    fn clamp_plugin_param(
        &self,
        location: u32,
        target: u32,
        slot: u32,
        param: u32,
        value: f32,
    ) -> f32 {
        let uri = if let Some(chain) = strip_chain(location) {
            self.model
                .strip(StripId(target))
                .and_then(|strip| strip_plugin_list(strip, chain).get(slot as usize))
                .map(|plugin| plugin.uri.clone())
        } else {
            self.model
                .bus(BusId(target))
                .and_then(|bus| bus.plugins.get(slot as usize))
                .map(|plugin| plugin.uri.clone())
        };
        let Some(uri) = uri else {
            return value;
        };
        let Some(control) = self
            .plugins
            .iter()
            .find(|info| info.id == uri)
            .and_then(|info| info.controls.get(param as usize))
        else {
            return value;
        };
        value.clamp(control.min, control.max)
    }

    fn add_strip(&mut self) -> bool {
        let Some(slot) = free_strip_slot(&self.params) else {
            return false;
        };

        if let Err(error) = self.engine.add_strip_ports(slot) {
            tracing::warn!(%error, slot, "failed to register strip ports");
            return false;
        }

        self.params.set_strip_active(slot, true);
        self.params.set_strip_gain(slot, 1.0);
        self.params.set_strip_pan(slot, 0.0);
        self.params.set_strip_input_gain_db(slot, 0.0);
        self.params.set_strip_mute(slot, false);
        self.params.set_strip_solo(slot, false);

        let mut strip = Strip::new(StripId(slot as u32), format!("ch {}", slot + 1));
        if let Some(first_bus) = self.model.buses.first().map(|bus| bus.id) {
            self.params.set_send_gain(slot, first_bus.0 as usize, 1.0);
            strip.sends.push(Send::new(first_bus));
        }

        self.model.strips.push(strip);
        self.engine.sync_port_names(&self.model);
        tracing::info!(slot, "added strip");
        true
    }

    fn add_bus(&mut self) -> bool {
        let Some(slot) = free_bus_slot(&self.params) else {
            return false;
        };

        if let Err(error) = self.engine.add_bus_ports(slot) {
            tracing::warn!(%error, slot, "failed to register bus ports");
            return false;
        }

        self.params.set_bus_active(slot, true);
        self.params.set_bus_gain(slot, 1.0);
        self.params.set_bus_mute(slot, false);
        self.params.set_bus_mix_master(slot, false);
        self.params.set_bus_return(slot, false);
        self.params.set_bus_send_gain(slot, 1.0);
        self.params.set_bus_return_gain(slot, 1.0);

        // Insert before the master so it stays rightmost in display order.
        let insert_at = self.model.buses.len().saturating_sub(1);
        self.model.buses.insert(
            insert_at,
            Bus::new(BusId(slot as u32), format!("bus {}", slot + 1)),
        );
        self.engine.sync_port_names(&self.model);
        tracing::info!(slot, "added bus");
        true
    }

    fn add_plugin(&mut self, location: u32, target: u32, uri: &str) -> bool {
        self.ensure_vst3_controls(uri);
        self.ensure_clap_controls(uri);
        let Some(info) = self.plugins.iter().find(|entry| entry.id == uri).cloned() else {
            tracing::warn!(%uri, "plugin not in catalog");
            return false;
        };
        let mut plugin = PluginSlot::new(plugin_id(0), uri);
        plugin.format = info.format;
        plugin.bundle = info.bundle.clone();
        plugin.name = info.name;
        plugin.params = info
            .controls
            .iter()
            .map(|control| control.default)
            .collect();
        self.insert_plugin_slot(location, target, plugin)
    }

    /// Fills a catalog entry's VST3 control metadata by instantiating the plugin
    /// once (on the control thread, so not RT). LV2 metadata is discovered up
    /// front; VST3 parameter info is only available from an instance.
    fn ensure_vst3_controls(&mut self, id: &str) {
        let Some(index) = self.plugins.iter().position(|entry| entry.id == id) else {
            return;
        };
        if self.plugins[index].format != PluginFormat::Vst3
            || (!self.plugins[index].controls.is_empty() && self.plugins[index].has_ui)
        {
            return;
        }
        let Some(bundle) = self.plugins[index].bundle.clone() else {
            return;
        };
        let sample_rate = self.engine.sample_rate() as f64;
        let buffer_size = self.engine.buffer_size() as usize;
        let Some(instance) = self.vst3.instantiate(
            &bundle,
            id,
            sample_rate,
            buffer_size,
            vibemix_vst3::PluginInstance::max_frames(),
        ) else {
            return;
        };
        self.plugins[index].controls = instance
            .controls()
            .iter()
            .cloned()
            .map(ControlInfo::from)
            .collect();
        self.plugins[index].has_ui = instance.has_editor();
        self.plugins[index].has_sidechain = instance.has_sidechain();
    }

    /// Fills a catalog entry's CLAP control metadata (and editor/sidechain
    /// flags) by instantiating the plugin once on the control thread.
    fn ensure_clap_controls(&mut self, id: &str) {
        let Some(index) = self.plugins.iter().position(|entry| entry.id == id) else {
            return;
        };
        if self.plugins[index].format != PluginFormat::Clap
            || (!self.plugins[index].controls.is_empty() && self.plugins[index].has_ui)
        {
            return;
        }
        let Some(bundle) = self.plugins[index].bundle.clone() else {
            return;
        };
        let sample_rate = self.engine.sample_rate() as f64;
        let buffer_size = self.engine.buffer_size() as usize;
        let Some(instance) = self.clap.instantiate(
            &bundle,
            id,
            sample_rate,
            buffer_size,
            vibemix_clap::PluginInstance::max_frames(),
        ) else {
            return;
        };
        self.plugins[index].controls = instance
            .controls()
            .iter()
            .cloned()
            .map(ControlInfo::from)
            .collect();
        self.plugins[index].has_ui = instance.has_editor();
        self.plugins[index].has_sidechain = instance.has_sidechain();
    }

    /// Fills VST3/CLAP parameter metadata (and editor availability) for plugins
    /// that are present in a loaded session. Fresh discovery cannot know either
    /// without instantiating, and `add_plugin` is not involved on restore.
    fn ensure_model_plugin_controls(&mut self) {
        let mut vst3_ids: HashSet<String> = HashSet::new();
        let mut clap_ids: HashSet<String> = HashSet::new();
        for strip in &self.model.strips {
            for plugin in strip.plugins.iter().chain(strip.plugins_post.iter()) {
                match plugin.format {
                    PluginFormat::Vst3 => {
                        vst3_ids.insert(plugin.uri.clone());
                    }
                    PluginFormat::Clap => {
                        clap_ids.insert(plugin.uri.clone());
                    }
                    PluginFormat::Lv2 => {}
                }
            }
        }
        for bus in &self.model.buses {
            for plugin in &bus.plugins {
                match plugin.format {
                    PluginFormat::Vst3 => {
                        vst3_ids.insert(plugin.uri.clone());
                    }
                    PluginFormat::Clap => {
                        clap_ids.insert(plugin.uri.clone());
                    }
                    PluginFormat::Lv2 => {}
                }
            }
        }
        for id in vst3_ids {
            self.ensure_vst3_controls(&id);
        }
        for id in clap_ids {
            self.ensure_clap_controls(&id);
        }
    }

    /// Appends a fully-described plugin slot (uri, params, LV2 state, bypass) to
    /// a strip chain or bus. Shared by add, paste and duplicate.
    fn insert_plugin_slot(&mut self, location: u32, target: u32, mut plugin: PluginSlot) -> bool {
        let slot = if let Some(chain) = strip_chain(location) {
            match self.model.strip(StripId(target)) {
                Some(strip) if strip_plugin_list(strip, chain).len() < MAX_PLUGINS => {
                    strip_plugin_list(strip, chain).len() as u32
                }
                Some(_) => {
                    tracing::warn!(target, chain, "strip plugin slots full");
                    return false;
                }
                None => return false,
            }
        } else {
            match self.model.bus(BusId(target)) {
                Some(bus) if bus.plugins.len() < MAX_PLUGINS => bus.plugins.len() as u32,
                Some(_) => {
                    tracing::warn!(target, "bus plugin slots full");
                    return false;
                }
                None => return false,
            }
        };

        self.ensure_vst3_controls(&plugin.uri);
        self.ensure_clap_controls(&plugin.uri);
        if plugin.params.is_empty() {
            if let Some(info) = self.plugins.iter().find(|entry| entry.id == plugin.uri) {
                plugin.params = info
                    .controls
                    .iter()
                    .map(|control| control.default)
                    .collect();
            }
        }

        // Plugins are smoke-tested in a throwaway child before being loaded.
        match plugin.format {
            PluginFormat::Lv2 => {
                if !self.plugin_is_safe(&plugin.uri) {
                    return false;
                }
            }
            PluginFormat::Vst3 => {
                let Some(bundle) = plugin.bundle.clone() else {
                    tracing::warn!(uri = %plugin.uri, "VST3 plugin has no bundle path");
                    return false;
                };
                if !self.vst3_is_safe(&bundle, &plugin.uri) {
                    return false;
                }
            }
            PluginFormat::Clap => {
                let Some(bundle) = plugin.bundle.clone() else {
                    tracing::warn!(uri = %plugin.uri, "CLAP plugin has no bundle path");
                    return false;
                };
                if !self.clap_is_safe(&bundle, &plugin.uri) {
                    return false;
                }
            }
        }

        let sample_rate = self.engine.sample_rate() as f64;
        let buffer_size = self.engine.buffer_size() as usize;
        let Some((slot_plugin, proxy, handle)) = instantiate_slot(
            &self.host,
            &self.vst3,
            &self.clap,
            &plugin,
            sample_rate,
            buffer_size,
        ) else {
            tracing::warn!(uri = %plugin.uri, "plugin instantiate failed");
            return false;
        };

        plugin.id = plugin_id(slot);
        plugin.missing = false;
        let uri = plugin.uri.clone();
        // Kick off the editor safety smoke in the background (before the plugin
        // is moved into the model) so the first "show UI" click is cheap.
        let ui_smoke = (plugin.format, plugin.uri.clone(), plugin.bundle.clone());
        let params = std::mem::take(&mut plugin.params);

        if let Some(chain) = strip_chain(location) {
            if let Err(error) =
                self.engine
                    .add_plugin(target as usize, chain, slot_plugin, proxy, handle)
            {
                tracing::warn!(%error, "failed to insert plugin");
                return false;
            }
            for (param, value) in params.iter().enumerate().take(MAX_PARAMS) {
                self.params
                    .set_plugin_param(target as usize, chain, slot as usize, param, *value);
            }
            self.params
                .set_plugin_bypass(target as usize, chain, slot as usize, plugin.bypassed);
            plugin.params = params;
            if let Some(strip) = self.model.strip_mut(StripId(target)) {
                strip_plugin_list_mut(strip, chain).push(plugin);
            }
        } else {
            if let Err(error) =
                self.engine
                    .add_bus_plugin(target as usize, slot_plugin, proxy, handle)
            {
                tracing::warn!(%error, "failed to insert plugin");
                return false;
            }
            for (param, value) in params.iter().enumerate().take(MAX_PARAMS) {
                self.params
                    .set_bus_plugin_param(target as usize, slot as usize, param, *value);
            }
            self.params
                .set_bus_plugin_bypass(target as usize, slot as usize, plugin.bypassed);
            plugin.params = params;
            if let Some(bus) = self.model.bus_mut(BusId(target)) {
                bus.plugins.push(plugin);
            }
        }

        self.schedule_ui_smoke(ui_smoke.0, &ui_smoke.1, ui_smoke.2.as_deref());
        tracing::info!(location, target, slot, uri = %uri, "inserted plugin");
        true
    }

    /// Captures a single model slot plus the live control values and LV2 state
    /// (including native-UI edits), for copy/paste and duplication.
    fn capture_slot_state(&self, location: u32, target: u32, slot: u32) -> Option<PluginSlot> {
        let mut plugin = if let Some(chain) = strip_chain(location) {
            strip_plugin_list(self.model.strip(StripId(target))?, chain)
                .get(slot as usize)
                .cloned()?
        } else {
            self.model
                .bus(BusId(target))?
                .plugins
                .get(slot as usize)
                .cloned()?
        };

        let controls = self
            .plugins
            .iter()
            .find(|info| info.id == plugin.uri)
            .map(|info| info.controls.len().min(MAX_PARAMS))
            .unwrap_or_else(|| plugin.params.len().min(MAX_PARAMS));
        if plugin.params.len() < controls {
            plugin.params.resize(controls, 0.0);
        }

        if let Some(chain) = strip_chain(location) {
            for param in 0..controls {
                plugin.params[param] =
                    self.params
                        .plugin_param(target as usize, chain, slot as usize, param);
            }
            plugin.bypassed = self
                .params
                .plugin_bypass(target as usize, chain, slot as usize);
            if let Some(state) = self
                .engine
                .state_proxy(target as usize, chain, slot as usize)
            {
                if let Some(encoded) = state.encode(&self.host) {
                    plugin.state = encoded;
                }
            }
        } else {
            for param in 0..controls {
                plugin.params[param] =
                    self.params
                        .bus_plugin_param(target as usize, slot as usize, param);
            }
            plugin.bypassed = self
                .params
                .bus_plugin_bypass(target as usize, slot as usize);
            if let Some(state) = self.engine.bus_state_proxy(target as usize, slot as usize) {
                if let Some(encoded) = state.encode(&self.host) {
                    plugin.state = encoded;
                }
            }
        }
        Some(plugin)
    }

    /// Whether the plugin at `slot` could not be loaded (uninstalled / failed).
    fn plugin_missing(&self, location: u32, target: u32, slot: u32) -> bool {
        if let Some(chain) = strip_chain(location) {
            self.model
                .strip(StripId(target))
                .and_then(|strip| strip_plugin_list(strip, chain).get(slot as usize))
                .map(|plugin| plugin.missing)
                .unwrap_or(false)
        } else {
            self.model
                .bus(BusId(target))
                .and_then(|bus| bus.plugins.get(slot as usize))
                .map(|plugin| plugin.missing)
                .unwrap_or(false)
        }
    }

    fn copy_plugin(&mut self, location: u32, target: u32, slot: u32) -> bool {
        if self.plugin_missing(location, target, slot) {
            tracing::warn!(location, target, slot, "refusing to copy a missing plugin");
            return false;
        }
        match self.capture_slot_state(location, target, slot) {
            Some(copied) => {
                self.clipboard = Some(PluginClipboard {
                    slot: copied,
                    location,
                });
                self.clipboard_dirty = true;
                tracing::info!(location, target, slot, "copied plugin");
                true
            }
            None => false,
        }
    }

    fn cut_plugin(&mut self, location: u32, target: u32, slot: u32) -> bool {
        if !self.copy_plugin(location, target, slot) {
            return false;
        }
        self.remove_plugin(location, target, slot)
    }

    fn paste_plugin(&mut self, location: u32, target: u32) -> bool {
        let Some(clipboard) = self.clipboard.clone() else {
            tracing::warn!("paste with empty clipboard");
            return false;
        };
        if clipboard.slot.missing {
            tracing::warn!("refusing to paste a missing plugin");
            return false;
        }
        self.insert_plugin_slot(location, target, clipboard.slot)
    }

    fn duplicate_strip(&mut self, strip_id: u32) -> bool {
        let Some(source) = self.model.strip(StripId(strip_id)).cloned() else {
            return false;
        };
        let Some(slot) = free_strip_slot(&self.params) else {
            tracing::warn!("no free strip slot to duplicate into");
            return false;
        };
        if let Err(error) = self.engine.add_strip_ports(slot) {
            tracing::warn!(%error, slot, "failed to register strip ports");
            return false;
        }

        let pre: Vec<PluginSlot> = source
            .plugins
            .iter()
            .enumerate()
            .map(|(index, plugin)| {
                self.capture_slot_state(PLUGIN_LOC_STRIP_PRE, strip_id, index as u32)
                    .unwrap_or_else(|| plugin.clone())
            })
            .collect();
        let post: Vec<PluginSlot> = source
            .plugins_post
            .iter()
            .enumerate()
            .map(|(index, plugin)| {
                self.capture_slot_state(PLUGIN_LOC_STRIP_POST, strip_id, index as u32)
                    .unwrap_or_else(|| plugin.clone())
            })
            .collect();

        self.params.set_strip_active(slot, true);
        self.params.set_strip_gain(slot, source.gain.0);
        self.params.set_strip_pan(slot, source.pan.0);
        self.params
            .set_strip_input_gain_db(slot, source.input_gain_db);
        self.params.set_strip_mute(slot, source.mute);
        self.params.set_strip_solo(slot, source.solo);
        for bus in 0..self.params.bus_count() {
            self.params.set_send_gain(slot, bus, 0.0);
            self.params.set_send_delay(slot, bus, 0.0);
        }
        for send in &source.sends {
            self.params
                .set_send_gain(slot, send.bus.0 as usize, send.gain.0);
            self.params
                .set_send_delay(slot, send.bus.0 as usize, send.delay_ms);
        }

        let mut duplicate = Strip::new(StripId(slot as u32), format!("{} copy", source.name));
        duplicate.gain = source.gain;
        duplicate.pan = source.pan;
        duplicate.input_gain_db = source.input_gain_db;
        duplicate.mute = source.mute;
        duplicate.solo = source.solo;
        duplicate.sends = source.sends.clone();
        self.model.strips.push(duplicate);

        let target = slot as u32;
        for plugin in pre {
            self.insert_plugin_slot(PLUGIN_LOC_STRIP_PRE, target, plugin);
        }
        for plugin in post {
            self.insert_plugin_slot(PLUGIN_LOC_STRIP_POST, target, plugin);
        }

        self.engine.sync_port_names(&self.model);
        tracing::info!(slot, source = strip_id, "duplicated strip");
        true
    }

    fn duplicate_bus(&mut self, bus_id: u32) -> bool {
        if bus_id == 0 {
            tracing::warn!("master bus cannot be duplicated");
            return false;
        }
        let Some(source) = self.model.bus(BusId(bus_id)).cloned() else {
            return false;
        };
        let Some(slot) = free_bus_slot(&self.params) else {
            tracing::warn!("no free bus slot to duplicate into");
            return false;
        };
        if let Err(error) = self.engine.add_bus_ports(slot) {
            tracing::warn!(%error, slot, "failed to register bus ports");
            return false;
        }
        if source.return_inputs {
            if let Err(error) = self.engine.add_bus_return_ports(slot) {
                tracing::warn!(%error, slot, "failed to register bus return ports");
            }
        }

        let plugins: Vec<PluginSlot> = source
            .plugins
            .iter()
            .enumerate()
            .map(|(index, plugin)| {
                self.capture_slot_state(PLUGIN_LOC_BUS, bus_id, index as u32)
                    .unwrap_or_else(|| plugin.clone())
            })
            .collect();

        self.params.set_bus_active(slot, true);
        self.params.set_bus_gain(slot, source.gain.0);
        self.params.set_bus_mute(slot, source.mute);
        self.params.set_bus_mix_master(slot, source.mix_to_master);
        self.params.set_bus_return(slot, source.return_inputs);
        self.params.set_bus_send_gain(slot, source.send_gain.0);
        self.params.set_bus_return_gain(slot, source.return_gain.0);

        let mut duplicate = Bus::new(BusId(slot as u32), format!("{} copy", source.name));
        duplicate.gain = source.gain;
        duplicate.mute = source.mute;
        duplicate.mix_to_master = source.mix_to_master;
        duplicate.return_inputs = source.return_inputs;
        duplicate.send_gain = source.send_gain;
        duplicate.return_gain = source.return_gain;
        let insert_at = self.model.buses.len().saturating_sub(1);
        self.model.buses.insert(insert_at, duplicate);

        for plugin in plugins {
            self.insert_plugin_slot(PLUGIN_LOC_BUS, slot as u32, plugin);
        }

        self.engine.sync_port_names(&self.model);
        tracing::info!(slot, source = bus_id, "duplicated bus");
        true
    }

    fn snapshot_entry(&self, label: &str) -> UndoEntry {
        let mut model = self.model.clone();
        model.window = None;
        let bytes = model_bytes(&model);
        UndoEntry {
            model,
            label: label.to_string(),
            bytes,
        }
    }

    fn push_undo(&mut self, label: &str, key: Option<EditKey>) {
        if let (Some(key), Some((instant, last))) = (key.as_ref(), self.undo_coalesce.as_ref()) {
            if last == key && instant.elapsed() < EDIT_COALESCE {
                self.undo_coalesce = Some((Instant::now(), key.clone()));
                return;
            }
        }

        let entry = self.snapshot_entry(label);
        self.undo_bytes += entry.bytes;
        self.undo_stack.push_back(entry);
        while self.undo_stack.len() > MAX_UNDO_ENTRIES
            || (self.undo_bytes > MAX_UNDO_BYTES && self.undo_stack.len() > 1)
        {
            if let Some(dropped) = self.undo_stack.pop_front() {
                self.undo_bytes = self.undo_bytes.saturating_sub(dropped.bytes);
            }
        }
        self.redo_stack.clear();
        self.undo_coalesce = key.map(|key| (Instant::now(), key));
        self.edit_dirty = true;
    }

    fn undo(&mut self) -> bool {
        let Some(entry) = self.undo_stack.pop_back() else {
            return false;
        };
        self.undo_bytes = self.undo_bytes.saturating_sub(entry.bytes);
        let current = self.snapshot_entry(&entry.label);
        self.redo_stack.push(current);
        self.restore_model(entry.model);
        self.undo_coalesce = None;
        self.edit_dirty = true;
        true
    }

    fn redo(&mut self) -> bool {
        let Some(entry) = self.redo_stack.pop() else {
            return false;
        };
        let current = self.snapshot_entry(&entry.label);
        self.undo_bytes += current.bytes;
        self.undo_stack.push_back(current);
        self.restore_model(entry.model);
        self.undo_coalesce = None;
        self.edit_dirty = true;
        true
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

    fn restore_model(&mut self, snapshot: MixerModel) {
        let structure_changed = !structure_eq(&self.model, &snapshot);
        let window = self.model.window.clone();
        self.model = snapshot;
        self.model.window = window;
        self.model.normalize_buses();
        self.rebuild_shared_bindings();

        apply_model(&self.params, &self.model);

        if structure_changed {
            if let Err(error) = self.engine.sync_strip_ports(&self.params) {
                tracing::warn!(%error, "failed to sync strip ports on undo");
            }
            if let Err(error) = self.engine.sync_bus_ports(&self.params) {
                tracing::warn!(%error, "failed to sync bus ports on undo");
            }
            if let Err(error) = self.engine.sync_bus_return_ports(&self.params) {
                tracing::warn!(%error, "failed to sync bus return ports on undo");
            }
            self.close_all_uis();
            let sample_rate = self.engine.sample_rate() as f64;
            if let Err(error) = self.engine.rebuild_plugins(
                &self.host,
                &self.vst3,
                &self.clap,
                &mut self.model,
                &self.params,
                sample_rate,
            ) {
                tracing::warn!(%error, "failed to rebuild plugins on undo");
            }
            self.missing_report =
                missing_report_json(&self.engine.restore_connections(&self.model));
        } else {
            self.apply_model_plugin_params();
        }

        self.engine.sync_port_names(&self.model);
    }

    fn apply_model_plugin_params(&self) {
        for strip in &self.model.strips {
            let index = strip.id.0 as usize;
            for chain in 0..PLUGIN_CHAINS {
                for (slot, plugin) in strip_plugin_list(strip, chain).iter().enumerate() {
                    for (param, value) in plugin.params.iter().enumerate().take(MAX_PARAMS) {
                        self.params
                            .set_plugin_param(index, chain, slot, param, *value);
                    }
                    self.params
                        .set_plugin_bypass(index, chain, slot, plugin.bypassed);
                }
            }
        }
        for bus in &self.model.buses {
            let index = bus.id.0 as usize;
            for (slot, plugin) in bus.plugins.iter().enumerate() {
                for (param, value) in plugin.params.iter().enumerate().take(MAX_PARAMS) {
                    self.params.set_bus_plugin_param(index, slot, param, *value);
                }
                self.params
                    .set_bus_plugin_bypass(index, slot, plugin.bypassed);
            }
        }
    }

    fn clear_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.undo_bytes = 0;
        self.undo_coalesce = None;
        self.edit_dirty = true;
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

    /// Closes one open editor and drops it from the map. Remote LV2 editors
    /// live in the UI host, so they need an explicit `Close`; in-process LV2
    /// and VST3 editors tear their window down when dropped.
    fn close_ui(&mut self, key: (u32, u32, u32)) {
        if let Some(OpenUi::Lv2Remote { id, .. }) = self.uis.remove(&key) {
            self.ui_service.close(id);
        }
    }

    /// Follows a structural change in one plugin chain by remapping the slots of
    /// its open editors: `map` returns the editor's new slot, or `None` to close
    /// it. LV2 editors carry their slot in their write context, so it is updated
    /// too (VST3 reads the slot from the map key).
    fn remap_uis_for(&mut self, location: u32, target: u32, map: impl Fn(u32) -> Option<u32>) {
        let keys: Vec<(u32, u32, u32)> = self
            .uis
            .keys()
            .filter(|(loc, tgt, _)| *loc == location && *tgt == target)
            .copied()
            .collect();
        let mut moved: Vec<((u32, u32, u32), OpenUi)> = Vec::new();
        for key in keys {
            match map(key.2) {
                Some(new_slot) => {
                    if let Some(mut ui) = self.uis.remove(&key) {
                        match &mut ui {
                            OpenUi::Lv2Remote { context, .. }
                            | OpenUi::Lv2Local { context, .. } => {
                                context.slot = new_slot as usize;
                            }
                            OpenUi::Vst3 { .. } => {}
                            OpenUi::Clap { .. } => {}
                        }
                        moved.push(((location, target, new_slot), ui));
                    }
                }
                None => self.close_ui(key),
            }
        }
        for (key, ui) in moved {
            self.uis.insert(key, ui);
        }
    }

    /// Closes every editor matching `predicate` (e.g. all editors on a strip).
    fn close_uis_where(&mut self, mut predicate: impl FnMut(u32, u32, u32) -> bool) {
        let keys: Vec<(u32, u32, u32)> = self
            .uis
            .keys()
            .filter(|(loc, tgt, slot)| predicate(*loc, *tgt, *slot))
            .copied()
            .collect();
        for key in keys {
            self.close_ui(key);
        }
    }

    /// Closes every open editor, e.g. before a rebuild that retires the plugin
    /// instances the editors point at.
    fn close_all_uis(&mut self) {
        let keys: Vec<(u32, u32, u32)> = self.uis.keys().copied().collect();
        for key in keys {
            self.close_ui(key);
        }
    }

    fn remove_plugin(&mut self, location: u32, target: u32, slot: u32) -> bool {
        if let Some(chain) = strip_chain(location) {
            let count = match self.model.strip(StripId(target)) {
                Some(target) if (slot as usize) < strip_plugin_list(target, chain).len() => {
                    strip_plugin_list(target, chain).len()
                }
                _ => return false,
            };

            self.pause_rt();
            if let Err(error) = self
                .engine
                .remove_plugin(target as usize, chain, slot as usize)
            {
                self.resume_rt();
                tracing::warn!(%error, "failed to remove plugin");
                return false;
            }
            for index in slot as usize..count - 1 {
                self.params
                    .swap_plugin_slots(target as usize, chain, index, index + 1);
            }
            for param in 0..MAX_PARAMS {
                self.params
                    .set_plugin_param(target as usize, chain, count - 1, param, 0.0);
            }
            self.params
                .set_plugin_bypass(target as usize, chain, count - 1, false);
            self.resume_rt();

            if let Some(model) = self.model.strip_mut(StripId(target)) {
                strip_plugin_list_mut(model, chain).remove(slot as usize);
            }
        } else {
            let count = match self.model.bus(BusId(target)) {
                Some(bus) if (slot as usize) < bus.plugins.len() => bus.plugins.len(),
                _ => return false,
            };

            self.pause_rt();
            if let Err(error) = self
                .engine
                .remove_bus_plugin(target as usize, slot as usize)
            {
                self.resume_rt();
                tracing::warn!(%error, "failed to remove bus plugin");
                return false;
            }
            for index in slot as usize..count - 1 {
                self.params
                    .swap_bus_plugin_slots(target as usize, index, index + 1);
            }
            for param in 0..MAX_PARAMS {
                self.params
                    .set_bus_plugin_param(target as usize, count - 1, param, 0.0);
            }
            self.params
                .set_bus_plugin_bypass(target as usize, count - 1, false);
            self.resume_rt();

            if let Some(bus) = self.model.bus_mut(BusId(target)) {
                bus.plugins.remove(slot as usize);
            }
        }
        // Close the removed plugin's editor and shift the rest up one slot so
        // each window stays attached to its own plugin.
        self.remap_uis_for(location, target, |s| {
            if s == slot {
                None
            } else if s > slot {
                Some(s - 1)
            } else {
                Some(s)
            }
        });
        tracing::info!(location, target, slot, "removed plugin");
        true
    }

    fn move_plugin(&mut self, location: u32, target: u32, from: u32, to: u32) -> bool {
        if let Some(chain) = strip_chain(location) {
            let count = match self.model.strip(StripId(target)) {
                Some(strip) => strip_plugin_list(strip, chain).len(),
                None => return false,
            };
            if from == to || from as usize >= count || to as usize >= count {
                return false;
            }

            self.pause_rt();
            if let Err(error) =
                self.engine
                    .move_plugin(target as usize, chain, from as usize, to as usize)
            {
                self.resume_rt();
                tracing::warn!(%error, "failed to move plugin");
                return false;
            }
            self.params
                .move_plugin_slot(target as usize, chain, from as usize, to as usize);
            self.resume_rt();

            if let Some(strip) = self.model.strip_mut(StripId(target)) {
                let list = strip_plugin_list_mut(strip, chain);
                let plugin = list.remove(from as usize);
                list.insert(to as usize, plugin);
            }
        } else {
            let count = match self.model.bus(BusId(target)) {
                Some(bus) => bus.plugins.len(),
                None => return false,
            };
            if from == to || from as usize >= count || to as usize >= count {
                return false;
            }

            self.pause_rt();
            if let Err(error) =
                self.engine
                    .move_bus_plugin(target as usize, from as usize, to as usize)
            {
                self.resume_rt();
                tracing::warn!(%error, "failed to move bus plugin");
                return false;
            }
            self.params
                .move_bus_plugin_slot(target as usize, from as usize, to as usize);
            self.resume_rt();

            if let Some(bus) = self.model.bus_mut(BusId(target)) {
                let plugin = bus.plugins.remove(from as usize);
                bus.plugins.insert(to as usize, plugin);
            }
        }
        // The moved plugin's editor follows `from -> to`; editors in between
        // shift by one so each window stays attached to its own plugin.
        self.remap_uis_for(location, target, |s| {
            if s == from {
                Some(to)
            } else if from < to && s > from && s <= to {
                Some(s - 1)
            } else if from > to && s >= to && s < from {
                Some(s + 1)
            } else {
                Some(s)
            }
        });
        tracing::info!(location, target, from, to, "moved plugin");
        true
    }

    fn send_catalog(&self, address: SocketAddr) {
        let message = ControlMessage::Catalog {
            json: self.catalog_json.clone(),
        };
        self.send_message(&message, address);
    }

    /// Encodes and sends a control message, splitting it into `Chunk` datagrams
    /// when it exceeds the UDP payload limit. Failures are logged: an oversized
    /// message used to be dropped silently, which hid the plugin catalog.
    fn send_message(&self, message: &ControlMessage, address: SocketAddr) {
        match message.encode_datagrams() {
            Ok(datagrams) => {
                for datagram in datagrams {
                    if let Err(error) = self.socket.send_to(&datagram, address) {
                        tracing::warn!(%error, %address, "failed to send control message");
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "failed to encode control message"),
        }
    }

    fn send_plugin_scan(&self, address: SocketAddr) {
        let message = ControlMessage::PluginScan {
            running: self.scan.running,
            done: self.scan.done as u32,
            total: self.scan.total as u32,
        };
        self.send_message(&message, address);
    }

    fn send_state(&self, address: SocketAddr) {
        let message = ControlMessage::State {
            json: build_state_json(
                &self.model,
                &self.params,
                &self.plugins,
                self.sample_rate,
                self.engine.xrun_count(),
            ),
        };
        self.send_message(&message, address);
    }

    fn send_recorder(&self, address: SocketAddr) {
        if let Ok(json) = serde_json::to_string(&self.recorder_status) {
            self.send_message(&ControlMessage::RecorderStatus { json }, address);
        }
    }

    fn send_loops(&self, address: SocketAddr) {
        if let Ok(json) = serde_json::to_string(&self.model.loops) {
            self.send_message(&ControlMessage::LoopList { json }, address);
        }
    }

    /// Send a high-resolution envelope for the editor. Kept out of the 30 Hz
    /// loop list so gain/play updates stay small.
    fn send_loop_waveform(&self, address: SocketAddr, request: PendingWaveform) {
        let Some(runtime) = self.loops.get(request.slot as usize).and_then(Option::as_ref) else {
            return;
        };
        let frames = runtime.slot.frames as usize;
        if frames == 0 || runtime.raw.len() < frames * 2 {
            return;
        }
        let (start, end) = if request.base {
            (0usize, frames)
        } else {
            let start = (request.start as usize).min(frames - 1);
            let end = (request.end as usize).min(frames);
            if end <= start {
                (0, frames)
            } else {
                (start, end)
            }
        };
        let nudge = if request.base {
            0
        } else {
            request
                .nudge
                .unwrap_or(runtime.slot.nudge_samples)
                .rem_euclid(frames as i64) as usize
        };
        let span = end - start;
        const RAW_SAMPLE_LIMIT: usize = 16_384;
        let samples = if !request.base && span <= RAW_SAMPLE_LIMIT {
            (start..end)
                .map(|frame| {
                    let raw = (nudge + frame) % frames;
                    (runtime.raw[raw * 2] + runtime.raw[raw * 2 + 1]) * 0.5
                })
                .collect::<Vec<f32>>()
        } else {
            Vec::new()
        };
        let buckets = if request.buckets == 0 {
            LOOP_WAVEFORM_BUCKETS
        } else {
            (request.buckets as usize).clamp(512, 32_768)
        };
        let peaks = if samples.is_empty() {
            let mut peaks = vec![0.0f32; buckets];
            for (bucket, peak) in peaks.iter_mut().enumerate() {
                let from = start + span * bucket / buckets;
                let to = (start + span * (bucket + 1) / buckets)
                    .max(from + 1)
                    .min(end);
                for frame in from..to {
                    let raw = (nudge + frame) % frames;
                    let index = raw * 2;
                    *peak = peak
                        .max(runtime.raw[index].abs())
                        .max(runtime.raw[index + 1].abs());
                }
            }
            peaks
        } else {
            Vec::new()
        };
        // High-pass energy (first difference) for the visual HF strip. Long
        // windows are skipped; zooming in brings it back.
        const HF_BUCKETS: usize = 2048;
        let hf = if !request.base && span <= self.engine.sample_rate() as usize * 10 {
            let mut hf = vec![0.0f32; HF_BUCKETS];
            for (bucket, value) in hf.iter_mut().enumerate() {
                let from = start + span * bucket / HF_BUCKETS;
                let to = (start + span * (bucket + 1) / HF_BUCKETS)
                    .max(from + 1)
                    .min(end);
                for frame in from..to {
                    let raw = (nudge + frame) % frames;
                    let previous = (raw + frames - 1) % frames;
                    let mono = (runtime.raw[raw * 2] + runtime.raw[raw * 2 + 1]) * 0.5;
                    let previous =
                        (runtime.raw[previous * 2] + runtime.raw[previous * 2 + 1]) * 0.5;
                    *value = value.max((mono - previous).abs());
                }
            }
            let peak = hf.iter().copied().fold(0.0f32, f32::max);
            if peak > 1.0e-6 {
                for value in &mut hf {
                    *value /= peak;
                }
            }
            hf
        } else {
            Vec::new()
        };
        let json = LoopWaveform {
            slot: request.slot,
            start: start as u64,
            end: end as u64,
            nudge: nudge as i64,
            seq: request.seq,
            peaks,
            samples,
            hf,
            transients: runtime.transients.clone(),
            base: request.base,
        };
        if let Ok(json) = serde_json::to_string(&json) {
            self.send_message(&ControlMessage::LoopWaveform { json }, address);
        }
    }

    /// Poll the audio thread for a finished take and refresh the status.
    fn poll_recorder(&mut self) {
        while let Some(take) = self.engine.take_recording() {
            self.finish_take(take);
        }

        let snapshot = self.engine.recorder().snapshot();
        let total = snapshot.total;
        let clock = self
            .engine
            .clock()
            .snapshot(self.engine.sample_rate() as f32);
        let message = if snapshot.state == crate::params::RecorderState::Armed && !clock.locked {
            "waiting for MIDI clock".to_string()
        } else {
            String::new()
        };
        let status = RecorderStatus {
            state: snapshot.state.as_str().to_string(),
            progress: if total > 0 {
                (snapshot.frames as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            },
            frames: snapshot.frames,
            peak: snapshot.peak,
            loop_beats: snapshot.loop_beats,
            loops_dir: loops_dir().to_string_lossy().into_owned(),
            message,
        };
        if status != self.recorder_status {
            self.recorder_status = status;
            self.recorder_dirty = true;
        }
    }

    /// Trim a captured take, apply the automatic/transient/manual offsets, and
    /// install it in the next free launcher slot.
    fn finish_take(&mut self, take: crate::recorder::RecordedTake) {
        let frames = take.frames;
        if frames == 0 {
            return;
        }
        let raw = take.buffer[..frames * 2].to_vec();
        let sample_rate = take.sample_rate;
        let base = if self.model.recorder.auto_latency {
            take.latency_frames as i64
        } else {
            0
        };
        let transients = detect_transients(&raw, frames, sample_rate);
        let transient = if self.model.recorder.transient {
            let window = sample_rate as i64 / 10;
            transients
                .iter()
                .find(|transient| (transient.frame as i64 - base).abs() <= window)
                .map(|transient| transient.frame as i64 - base)
                .unwrap_or_else(|| {
                    detect_transient(&raw, frames, sample_rate, base, 100.0)
                        .map(|onset| onset - base)
                        .unwrap_or(0)
                })
        } else {
            0
        };
        let manual = self.model.recorder.nudge_samples;
        let total = base + transient + manual;
        let rotated = rotate_interleaved(&raw, frames, total);
        let loop_peaks = peaks(&raw, frames, LOOP_PEAK_BUCKETS);

        let Some(slot_index) = self.loops.iter().position(Option::is_none) else {
            self.recorder_status.message = "loop slots are full".to_string();
            self.recorder_dirty = true;
            return;
        };
        let bpm = self
            .engine
            .clock()
            .snapshot(self.engine.sample_rate() as f32)
            .bpm;
        let slot = LoopSlot {
            id: slot_index as u32,
            name: format!("loop {}", slot_index + 1),
            path: None,
            frames: frames as u64,
            sample_rate,
            loop_beats: take.loop_beats,
            bpm,
            nudge_samples: total,
            base_nudge: base,
            transient_nudge: transient,
            manual_nudge: manual,
            peaks: loop_peaks,
            gain: 1.0,
            mute: false,
            playing: false,
            saved: false,
        };
        if let Err(error) = self
            .engine
            .set_loop(slot_index, rotated.into_boxed_slice(), frames)
        {
            tracing::warn!(%error, "failed to install recorded loop");
            self.recorder_status.message = error.to_string();
            self.recorder_dirty = true;
            return;
        }
        self.loops[slot_index] = Some(LoopRuntime {
            slot,
            raw,
            transients,
        });
        self.rebuild_model_loops();
        self.loops_dirty = true;
        self.recorder_status = RecorderStatus::default();
        self.recorder_dirty = true;
    }

    fn arm_recorder(&mut self) {
        let sources = self.model.recorder.sources.clone();
        if sources.is_empty() {
            self.recorder_status.message = "select at least one source".to_string();
            self.recorder_dirty = true;
            return;
        }
        self.engine.recorder().set_sources(&sources);

        let length = self.model.transport_ui.loop_length.max(1) as f32;
        let loop_beats = if self.model.transport_ui.loop_bars {
            length * self.model.beats_per_bar
        } else {
            length
        };
        let clock = self
            .engine
            .clock()
            .snapshot(self.engine.sample_rate() as f32);
        let bpm = if clock.bpm > 1.0 { clock.bpm } else { 120.0 };
        let sample_rate = self.engine.sample_rate() as usize;
        let expected =
            ((loop_beats as f64 * 60.0 / bpm as f64) * sample_rate as f64).ceil() as usize;
        // Allocate for a slow (30 BPM) take so a tempo lower than the current
        // estimate cannot overflow the buffer; the audio thread discards the
        // take if it still runs out.
        let capacity_frames =
            ((loop_beats as f64 * 60.0 / 30.0) * sample_rate as f64).ceil() as usize;
        let cap = sample_rate * 300;
        if expected == 0 {
            self.recorder_status.message = "loop length is zero".to_string();
            self.recorder_dirty = true;
            return;
        }
        if capacity_frames > cap {
            self.recorder_status.message = "loop is longer than five minutes".to_string();
            self.recorder_dirty = true;
            return;
        }
        let capacity = (capacity_frames + sample_rate).min(cap);
        let buffer = vec![0.0f32; capacity * 2].into_boxed_slice();
        let loop_pulses = (loop_beats * 24.0).round().max(1.0) as u32;

        match self
            .engine
            .arm_recorder(buffer, capacity, expected, loop_pulses, loop_beats)
        {
            Ok(()) => {
                self.recorder_status = RecorderStatus {
                    state: "armed".to_string(),
                    message: if clock.locked {
                        String::new()
                    } else {
                        "waiting for MIDI clock".to_string()
                    },
                    ..RecorderStatus::default()
                };
                self.recorder_dirty = true;
            }
            Err(error) => {
                self.recorder_status.message = error.to_string();
                self.recorder_dirty = true;
            }
        }
    }

    fn handle_recorder_action(&mut self, action: RecorderAction) {
        match action {
            RecorderAction::Arm => self.arm_recorder(),
            RecorderAction::Cancel | RecorderAction::Stop => {
                let _ = self.engine.cancel_recorder();
                self.recorder_status = RecorderStatus::default();
                self.recorder_dirty = true;
            }
            RecorderAction::SetNudge { slot, manual } => self.set_loop_nudge(slot, manual),
            RecorderAction::SetLoopGain { slot, gain } => {
                let gain = gain.clamp(0.0, 4.0);
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.gain = gain;
                    let _ = self.engine.loop_gain(slot as usize, gain);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::SetLoopMute { slot, mute } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.mute = mute;
                    let _ = self.engine.loop_mute(slot as usize, mute);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::LoopPlay { slot, quantized } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.playing = true;
                    let _ = self.engine.loop_play(slot as usize, quantized);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::LoopStop { slot } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.playing = false;
                    let _ = self.engine.loop_stop(slot as usize);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::LoopDelete { slot } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::take) {
                    let _ = runtime;
                    let _ = self.engine.clear_loop(slot as usize);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::LoopRename { slot, name } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.name = name;
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::Waveform {
                slot,
                start,
                end,
                nudge,
                seq,
                buckets,
                base,
            } => {
                self.pending_waveform = Some(PendingWaveform {
                    slot,
                    start,
                    end,
                    nudge,
                    seq,
                    buckets,
                    base,
                });
            }
            RecorderAction::SnapTransient { slot, frame } => {
                self.snap_transient(slot, frame);
            }
            RecorderAction::SnapSeam { slot } => self.snap_loop_seam(slot),
            RecorderAction::LoopSave {
                slot,
                path,
                bit_depth,
            } => self.save_loop(slot, path, bit_depth),
        }
    }

    fn set_loop_nudge(&mut self, slot: u32, manual: i64) {
        let Some(total) = self
            .loops
            .get(slot as usize)
            .and_then(Option::as_ref)
            .map(|runtime| runtime.slot.base_nudge + runtime.slot.transient_nudge + manual)
        else {
            return;
        };
        self.set_loop_total(slot, total);
    }

    /// Set the total rotation and reinstall the playback buffer.
    fn set_loop_total(&mut self, slot: u32, total: i64) {
        let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) else {
            return;
        };
        runtime.slot.nudge_samples = total;
        runtime.slot.manual_nudge =
            total - runtime.slot.base_nudge - runtime.slot.transient_nudge;
        runtime.slot.saved = false;
        let frames = runtime.slot.frames as usize;
        let rotated = rotate_interleaved(&runtime.raw, frames, total);
        let _ = self
            .engine
            .set_loop(slot as usize, rotated.into_boxed_slice(), frames);
        self.rebuild_model_loops();
        self.loops_dirty = true;
    }

    fn snap_transient(&mut self, slot: u32, frame: u64) {
        let Some(runtime) = self.loops.get(slot as usize).and_then(Option::as_ref) else {
            return;
        };
        let frames = runtime.slot.frames as i64;
        if frames <= 0 {
            return;
        }
        let total = (frame as i64).rem_euclid(frames);
        self.set_loop_total(slot, total);
        self.recorder_status.message = format!("snapped transient at {total} samples");
        self.recorder_dirty = true;
    }

    fn snap_loop_seam(&mut self, slot: u32) {
        let Some(runtime) = self.loops.get(slot as usize).and_then(Option::as_ref) else {
            return;
        };
        let frames = runtime.slot.frames as usize;
        if frames == 0 {
            return;
        }
        let current = runtime.slot.nudge_samples;
        let sample_rate = runtime.slot.sample_rate;
        let total = snap_seam(&runtime.raw, frames, current, sample_rate, 2.0);
        let delta = total - current;
        self.set_loop_total(slot, total);
        self.recorder_status.message = if delta == 0 {
            "seam already aligned".to_string()
        } else {
            format!("seam snapped by {delta} samples")
        };
        self.recorder_dirty = true;
    }

    fn save_loop(&mut self, slot: u32, path: Option<String>, bit_depth: u8) {
        let save_dir = self.model.recorder.save_dir.clone();
        let Some(runtime) = self.loops.get(slot as usize).and_then(Option::as_ref) else {
            return;
        };
        let frames = runtime.slot.frames as usize;
        let sample_rate = runtime.slot.sample_rate;
        let name = sanitize_file_name(&runtime.slot.name);
        let path = path
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let dir = if save_dir.trim().is_empty() {
                    loops_dir()
                } else {
                    PathBuf::from(&save_dir)
                };
                let stamp = timestamp_slug();
                let mut candidate = dir.join(format!("{name}-{stamp}.wav"));
                let mut suffix = 2;
                while candidate.exists() {
                    candidate = dir.join(format!("{name}-{stamp}-{suffix}.wav"));
                    suffix += 1;
                }
                candidate
            });
        match write_loop_wav(
            &path,
            &runtime.raw,
            frames,
            sample_rate,
            bit_depth,
            runtime.slot.nudge_samples,
        ) {
            Ok(()) => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.path = Some(path.to_string_lossy().into_owned());
                    runtime.slot.saved = true;
                }
                self.model.recorder.bit_depth = bit_depth;
                self.rebuild_model_loops();
                self.loops_dirty = true;
                self.recorder_status.message = format!("saved {}", path.display());
                self.recorder_dirty = true;
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed to save loop");
                self.recorder_status.message = error.to_string();
                self.recorder_dirty = true;
            }
        }
    }

    fn rebuild_model_loops(&mut self) {
        self.model.loops = self
            .loops
            .iter()
            .flatten()
            .map(|runtime| runtime.slot.clone())
            .collect();
    }

    fn clear_loops(&mut self) {
        for index in 0..MAX_LOOPS {
            let _ = self.engine.clear_loop(index);
            self.loops[index] = None;
        }
        self.model.loops.clear();
        self.loops_dirty = true;
    }

    fn load_loops_from_model(&mut self) {
        let saved = std::mem::take(&mut self.model.loops);
        for index in 0..MAX_LOOPS {
            let _ = self.engine.clear_loop(index);
            self.loops[index] = None;
        }
        let sample_rate = self.engine.sample_rate();
        for mut slot in saved {
            let Some(path) = slot.path.clone() else {
                continue;
            };
            match read_loop_wav(Path::new(&path), sample_rate) {
                Ok((raw, frames)) => {
                    let Some(index) = self.loops.iter().position(Option::is_none) else {
                        break;
                    };
                    slot.id = index as u32;
                    slot.frames = frames as u64;
                    slot.sample_rate = sample_rate;
                    slot.nudge_samples = 0;
                    slot.base_nudge = 0;
                    slot.transient_nudge = 0;
                    slot.manual_nudge = 0;
                    slot.playing = false;
                    slot.peaks = peaks(&raw, frames, LOOP_PEAK_BUCKETS);
                    if self
                        .engine
                        .set_loop(index, raw.clone().into_boxed_slice(), frames)
                        .is_ok()
                    {
                        let transients = detect_transients(&raw, frames, sample_rate);
                        self.loops[index] = Some(LoopRuntime {
                            slot,
                            raw,
                            transients,
                        });
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, path, "failed to load loop");
                }
            }
        }
        self.rebuild_model_loops();
        self.loops_dirty = true;
    }

    fn send_missing(&self, address: SocketAddr, json: &str) {
        let message = ControlMessage::MissingConnections {
            json: json.to_string(),
        };
        self.send_message(&message, address);
    }

    fn send_session_result(&self, address: SocketAddr, ok: bool, path: &str, error: &str) {
        let message = ControlMessage::SessionResult {
            ok,
            path: path.to_string(),
            error: error.to_string(),
        };
        self.send_message(&message, address);
    }

    fn send_clipboard(&self, address: SocketAddr) {
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

    fn send_edit_state(&self, address: SocketAddr) {
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

/// Whether two models have the same engine-visible structure (ignoring order,
/// scalar values and window geometry). A change means undo must rebuild.
fn structure_eq(a: &MixerModel, b: &MixerModel) -> bool {
    if a.strips.len() != b.strips.len() || a.buses.len() != b.buses.len() {
        return false;
    }
    let strips_match = a.strips.iter().all(|strip| {
        b.strips.iter().any(|other| {
            other.id == strip.id
                && plugin_lists_eq(&strip.plugins, &other.plugins)
                && plugin_lists_eq(&strip.plugins_post, &other.plugins_post)
                && sends_eq(&strip.sends, &other.sends)
        })
    });
    if !strips_match {
        return false;
    }
    a.buses.iter().all(|bus| {
        b.buses.iter().any(|other| {
            other.id == bus.id
                && other.return_inputs == bus.return_inputs
                && plugin_lists_eq(&bus.plugins, &other.plugins)
        })
    })
}

fn plugin_lists_eq(a: &[PluginSlot], b: &[PluginSlot]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.uri == y.uri
                && x.state == y.state
                && x.sidechain_enabled == y.sidechain_enabled
                && sidechain_strips(x) == sidechain_strips(y)
        })
}

fn sidechain_strips(plugin: &PluginSlot) -> Vec<u32> {
    let mut strips: Vec<u32> = plugin
        .sidechain
        .iter()
        .map(|source| source.strip.0)
        .collect();
    strips.sort_unstable();
    strips
}

fn sends_eq(a: &[Send], b: &[Send]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.bus == y.bus && x.pre_fader == y.pre_fader)
}

fn model_bytes(model: &MixerModel) -> usize {
    let mut total = 128 + model.name.len();
    let plugin_bytes = |plugin: &PluginSlot| {
        64 + plugin.uri.len() + plugin.name.len() + plugin.state.len() + plugin.params.len() * 4
    };
    for strip in &model.strips {
        total += 64 + strip.name.len();
        for plugin in strip.plugins.iter().chain(strip.plugins_post.iter()) {
            total += plugin_bytes(plugin);
        }
    }
    for bus in &model.buses {
        total += 64 + bus.name.len();
        for plugin in &bus.plugins {
            total += plugin_bytes(plugin);
        }
    }
    total
}

fn missing_report_json(missing: &[crate::backend_jack::MissingConnection]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    serde_json::to_string(missing).ok()
}

fn plugin_id(slot: u32) -> vibemix_core::ids::PluginId {
    vibemix_core::ids::PluginId(slot)
}

/// Maps a plugin location to a strip chain index (0 = pre, 1 = post). Returns
/// `None` for bus inserts.
fn strip_chain(location: u32) -> Option<usize> {
    match location {
        PLUGIN_LOC_STRIP_PRE => Some(0),
        PLUGIN_LOC_STRIP_POST => Some(1),
        _ => None,
    }
}

fn strip_plugin_list(strip: &Strip, chain: usize) -> &Vec<PluginSlot> {
    if chain == 0 {
        &strip.plugins
    } else {
        &strip.plugins_post
    }
}

fn strip_plugin_list_mut(strip: &mut Strip, chain: usize) -> &mut Vec<PluginSlot> {
    if chain == 0 {
        &mut strip.plugins
    } else {
        &mut strip.plugins_post
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

/// Required features the host implements. A plugin requiring anything else is
/// reported as unsupported rather than attempted.
const SUPPORTED_REQUIRED_FEATURES: &[&str] = &[
    "http://lv2plug.in/ns/ext/urid#map",
    "http://lv2plug.in/ns/ext/urid#unmap",
    "http://lv2plug.in/ns/ext/worker#schedule",
    "http://lv2plug.in/ns/ext/buf-size#boundedBlockLength",
    "http://lv2plug.in/ns/ext/options#options",
];

fn unsupported_required_feature(info: &vibemix_lv2::PluginInfo) -> Option<String> {
    info.required_features
        .iter()
        .map(String::as_str)
        .find(|feature| !SUPPORTED_REQUIRED_FEATURES.contains(feature))
        .map(|feature| format!("requires {}", short_feature(feature)))
}

fn short_feature(uri: &str) -> &str {
    uri.rsplit(['#', '/']).next().unwrap_or(uri)
}

fn build_catalog(plugins: &[PluginEntry], health: &HealthStore) -> String {
    let list: Vec<serde_json::Value> = plugins
        .iter()
        .map(|entry| {
            let verdict = health.get(&entry.id);
            let (status, reason) = if let Some(reason) = &entry.unsupported {
                (Some("unsupported"), reason.clone())
            } else if !entry.hostable {
                (Some("unsupported"), "unsupported port layout".to_string())
            } else if let Some(verdict) = verdict {
                (Some(verdict.status.as_str()), verdict.reason.clone())
            } else {
                (None, String::new())
            };
            // VST3/CLAP port layout and editor availability are only known after
            // the smoke sweep instantiates the plugin; merge what it reported.
            let has_sidechain =
                entry.has_sidechain || verdict.and_then(|v| v.has_sidechain).unwrap_or(false);
            let has_ui = entry.has_ui || verdict.and_then(|v| v.has_ui).unwrap_or(false);
            let inputs = if entry.inputs > 0 {
                entry.inputs
            } else {
                verdict.and_then(|v| v.inputs).unwrap_or(0)
            };
            let outputs = if entry.outputs > 0 {
                entry.outputs
            } else {
                verdict.and_then(|v| v.outputs).unwrap_or(0)
            };
            serde_json::json!({
                "uri": entry.id,
                "format": entry.format.as_str(),
                "name": entry.name,
                "vendor": entry.vendor,
                "category": entry.category,
                "version": entry.version,
                "description": entry.description,
                "homepage": entry.homepage,
                "license": entry.license,
                "bundle": entry.bundle,
                "has_ui": has_ui,
                "has_sidechain": has_sidechain,
                "inputs": inputs,
                "outputs": outputs,
                "midi": entry.midi,
                "status": status,
                "reason": reason,
            })
        })
        .collect();
    serde_json::Value::Array(list).to_string()
}

/// Mono/stereo LV2 plugins plus every VST3 effect, in catalog order.
/// Runs the engine binary with `args` as a throwaway UI smoke test. A child
/// that exits non-zero or exceeds `timeout` is considered unsafe; a failure to
/// spawn at all is treated as safe (same as the LV2 path).
fn ui_smoke_ok(args: &[&str], timeout: Duration) -> bool {
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
fn ui_safe_map(
    map: &Mutex<HashMap<String, bool>>,
) -> std::sync::MutexGuard<'_, HashMap<String, bool>> {
    map.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Runs an editor smoke test in a detached background thread and records the
/// verdict, keyed by `key`.
fn spawn_ui_smoke(map: Arc<Mutex<HashMap<String, bool>>>, key: String, args: Vec<String>) {
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

/// Picks the most specific VST3 subcategory that isn't a channel descriptor.
/// The SDK lists subcategories general-to-specific ("Fx|Delay"), but some
/// plugins (e.g. LSP) append the channel layout ("Fx|Stereo"), so the leaf is
/// not always the useful one.
fn vst3_category(subcategories: &str, category: &str) -> String {
    let segments: Vec<&str> = subcategories
        .split('|')
        .filter(|segment| !segment.is_empty())
        .collect();
    for segment in segments.iter().rev() {
        let lower = segment.to_ascii_lowercase();
        if lower != "mono" && lower != "stereo" && lower != "surround" {
            return (*segment).to_string();
        }
    }
    if let Some(first) = segments.first() {
        return (*first).to_string();
    }
    if category.is_empty() {
        "Effect".to_string()
    } else {
        category.to_string()
    }
}

fn discover_catalog(
    host: &Host,
    vst3: &vibemix_vst3::Host,
    clap: &vibemix_clap::Host,
) -> Vec<PluginEntry> {
    let mut entries: Vec<PluginEntry> = host
        .discover()
        .into_iter()
        .filter(|info| {
            // Main I/O must be mono or stereo; extra inputs are sidechain.
            let main = info.main_input_count;
            let outputs = info.audio_outputs.len();
            (main == 2 && outputs == 2) || (main == 1 && outputs == 1)
        })
        .map(|info| {
            let unsupported = unsupported_required_feature(&info);
            PluginEntry {
                format: PluginFormat::Lv2,
                id: info.uri,
                name: info.name,
                vendor: info.author,
                category: info.class_label,
                version: String::new(),
                description: info.description,
                homepage: info.homepage,
                license: info.license,
                bundle: info.bundle,
                has_ui: info.has_ui,
                has_sidechain: info.audio_inputs.len() > info.main_input_count,
                inputs: info.audio_inputs.len(),
                outputs: info.audio_outputs.len(),
                midi: !info.midi_inputs.is_empty(),
                hostable: info.hostable,
                unsupported,
                controls: info.controls.into_iter().map(ControlInfo::from).collect(),
            }
        })
        .collect();
    for info in vst3.discover() {
        let category = vst3_category(&info.subcategories, &info.category);
        entries.push(PluginEntry {
            format: PluginFormat::Vst3,
            id: info.class_id,
            name: info.name,
            vendor: info.vendor,
            category,
            version: info.version,
            description: String::new(),
            homepage: String::new(),
            license: String::new(),
            bundle: Some(info.bundle),
            has_ui: false,
            has_sidechain: false,
            inputs: 0,
            outputs: 0,
            midi: false,
            hostable: true,
            unsupported: None,
            controls: Vec::new(),
        });
    }
    for info in clap.discover() {
        let category = clap_category(&info.features);
        entries.push(PluginEntry {
            format: PluginFormat::Clap,
            id: info.id,
            name: info.name,
            vendor: info.vendor,
            category,
            version: info.version,
            description: info.description,
            homepage: info.url,
            license: String::new(),
            bundle: Some(info.bundle),
            has_ui: false,
            has_sidechain: false,
            inputs: 0,
            outputs: 0,
            midi: false,
            hostable: true,
            unsupported: None,
            controls: Vec::new(),
        });
    }
    entries
}

/// Picks a display category from a CLAP feature list. Channel/layout and
/// format features are ignored in favour of a concrete effect type.
fn clap_category(features: &str) -> String {
    let mut fallback = "Effect".to_string();
    for feature in features.split(',').map(str::trim).filter(|f| !f.is_empty()) {
        match feature {
            "instrument" | "synthesizer" | "sampler" | "drum-machine" | "drum" => {
                return "Instrument".to_string();
            }
            "audio-effect" | "mono" | "stereo" | "surround" | "ambisonic" | "note-effect"
            | "note-detector" => continue,
            "utility" => {
                fallback = "Utility".to_string();
                continue;
            }
            _ => {}
        }
        let mut chars = feature.chars();
        if let Some(first) = chars.next() {
            return first.to_uppercase().collect::<String>() + chars.as_str();
        }
    }
    fallback
}

fn catalog_path() -> std::path::PathBuf {
    config_dir().join("catalog.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CatalogEntry {
    #[serde(alias = "uri")]
    id: String,
    name: String,
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    homepage: String,
    #[serde(default)]
    license: String,
    #[serde(default)]
    format: PluginFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bundle: Option<String>,
    #[serde(default)]
    has_ui: bool,
    #[serde(default)]
    has_sidechain: bool,
    #[serde(default)]
    inputs: usize,
    #[serde(default)]
    outputs: usize,
    #[serde(default)]
    midi: bool,
}

fn load_catalog() -> Vec<PluginEntry> {
    let Ok(text) = fs::read_to_string(catalog_path()) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<CatalogEntry>>(&text) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .map(|entry| PluginEntry {
            format: entry.format,
            id: entry.id,
            name: entry.name,
            vendor: entry.vendor,
            category: entry.category,
            version: entry.version,
            description: entry.description,
            homepage: entry.homepage,
            license: entry.license,
            bundle: entry.bundle,
            has_ui: entry.has_ui,
            has_sidechain: entry.has_sidechain,
            inputs: entry.inputs,
            outputs: entry.outputs,
            midi: entry.midi,
            hostable: true,
            unsupported: None,
            controls: Vec::new(),
        })
        .collect()
}

fn save_catalog(plugins: &[PluginEntry]) {
    let entries: Vec<CatalogEntry> = plugins
        .iter()
        .map(|entry| CatalogEntry {
            id: entry.id.clone(),
            name: entry.name.clone(),
            vendor: entry.vendor.clone(),
            category: entry.category.clone(),
            version: entry.version.clone(),
            description: entry.description.clone(),
            homepage: entry.homepage.clone(),
            license: entry.license.clone(),
            format: entry.format,
            bundle: entry.bundle.clone(),
            has_ui: entry.has_ui,
            has_sidechain: entry.has_sidechain,
            inputs: entry.inputs,
            outputs: entry.outputs,
            midi: entry.midi,
        })
        .collect();
    let Ok(text) = serde_json::to_string_pretty(&entries) else {
        return;
    };
    let dir = config_dir();
    let _ = fs::create_dir_all(&dir);
    if let Err(error) = fs::write(dir.join("catalog.json"), text) {
        tracing::warn!(%error, "failed to save plugin catalog");
    }
}

fn plugin_list_json<F: Fn(usize, usize) -> f32>(
    plugins: &[PluginSlot],
    catalog: &[PluginEntry],
    param_value: F,
) -> Vec<serde_json::Value> {
    plugins
        .iter()
        .enumerate()
        .map(|(slot, plugin)| {
            let info = catalog.iter().find(|entry| entry.id == plugin.uri);
            let controls = info.map(|entry| entry.controls.as_slice()).unwrap_or(&[]);
            let has_ui = info.map(|entry| entry.has_ui).unwrap_or(false);
            let has_sidechain = info.map(|entry| entry.has_sidechain).unwrap_or(false);
            let control_list: Vec<serde_json::Value> = controls
                .iter()
                .enumerate()
                .take(MAX_PARAMS)
                .map(|(param, control)| {
                    let scale_points: Vec<serde_json::Value> = control
                        .scale_points
                        .iter()
                        .map(|point| {
                            serde_json::json!({ "value": point.value, "label": point.label })
                        })
                        .collect();
                    serde_json::json!({
                        "name": control.name,
                        "min": control.min,
                        "max": control.max,
                        "toggled": control.toggled,
                        "integer": control.integer,
                        "scale_points": scale_points,
                        "value": param_value(slot, param),
                    })
                })
                .collect();
            let sidechain: Vec<serde_json::Value> = plugin
                .sidechain
                .iter()
                .map(|source| {
                    serde_json::json!({
                        "strip": source.strip.0,
                        "gain": source.gain.0,
                        "pre_fader": source.pre_fader,
                    })
                })
                .collect();
            serde_json::json!({
                "slot": slot,
                "uri": plugin.uri,
                "name": plugin.name,
                "bypassed": plugin.bypassed,
                "missing": plugin.missing,
                "has_ui": has_ui,
                "has_sidechain": has_sidechain,
                "sidechain_enabled": plugin.sidechain_enabled,
                "sidechain": sidechain,
                "midi_channel": plugin.midi_channel,
                "controls": control_list,
            })
        })
        .collect()
}

fn build_state_json(
    model: &MixerModel,
    params: &SharedParams,
    plugins: &[PluginEntry],
    sample_rate: u32,
    xruns: u64,
) -> String {
    let strips: Vec<serde_json::Value> = model
        .strips
        .iter()
        .map(|strip| {
            let strip_index = strip.id.0 as usize;
            let sends: Vec<serde_json::Value> = strip
                .sends
                .iter()
                .map(|send| {
                    serde_json::json!({
                        "bus": send.bus.0,
                        "gain": params.send_gain(strip_index, send.bus.0 as usize),
                        "delay_ms": params.send_delay(strip_index, send.bus.0 as usize),
                    })
                })
                .collect();
            serde_json::json!({
                "id": strip.id.0,
                "name": strip.name,
                "midi_channel": strip.midi_channel,
                "gain": params.strip_gain(strip_index),
                "pan": params.strip_pan(strip_index),
                "input_gain_db": strip.input_gain_db,
                "mute": params.strip_mute(strip_index),
                "solo": params.strip_solo(strip_index),
                "plugins": plugin_list_json(&strip.plugins, plugins, |slot, param| {
                    params.plugin_param(strip_index, 0, slot, param)
                }),
                "plugins_post": plugin_list_json(&strip.plugins_post, plugins, |slot, param| {
                    params.plugin_param(strip_index, 1, slot, param)
                }),
                "sends": sends,
            })
        })
        .collect();

    let buses: Vec<serde_json::Value> = model
        .buses
        .iter()
        .map(|bus| {
            let bus_index = bus.id.0 as usize;
            serde_json::json!({
                "id": bus.id.0,
                "name": bus.name,
                "midi_channel": bus.midi_channel,
                "gain": params.bus_gain(bus_index),
                "mute": params.bus_mute(bus_index),
                "mix_to_master": bus.mix_to_master,
                "return_inputs": bus.return_inputs,
                "send_gain": params.bus_send_gain(bus_index),
                "return_gain": params.bus_return_gain(bus_index),
                "plugins": plugin_list_json(&bus.plugins, plugins, |slot, param| {
                    params.bus_plugin_param(bus_index, slot, param)
                }),
            })
        })
        .collect();

    let window = match &model.window {
        Some(geometry) => serde_json::json!({
            "width": geometry.width,
            "height": geometry.height,
            "maximized": geometry.maximized,
        }),
        None => serde_json::Value::Null,
    };

    serde_json::json!({
        "strips": strips,
        "buses": buses,
        "window": window,
        "sample_rate": sample_rate,
        "xruns": xruns,
        "beats_per_bar": model.beats_per_bar,
        "midi_bindings": model.midi_bindings,
        "transport_ui": model.transport_ui,
        "layout": model.layout,
        "midi_sync": model.midi_sync,
        "recorder": model.recorder,
        "strip_capacity": params.strip_count(),
        "bus_capacity": params.bus_count(),
    })
    .to_string()
}

fn free_strip_slot(params: &SharedParams) -> Option<usize> {
    (0..params.strip_count()).find(|index| !params.strip_active(*index))
}

fn free_bus_slot(params: &SharedParams) -> Option<usize> {
    (0..params.bus_count()).find(|index| !params.bus_active(*index))
}

/// Cheap, allocation-free signature of the model's sidechain routing (the only
/// input to [`compute_strip_order`]). Unequal signatures require a recompute;
/// equal ones never do.
fn sidechain_signature(model: &MixerModel) -> u64 {
    let mut acc = 0u64;
    for strip in &model.strips {
        for plugin in strip.plugins.iter().chain(strip.plugins_post.iter()) {
            for source in &plugin.sidechain {
                let pair = (strip.id.0 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    ^ (source.strip.0 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
                acc = acc.wrapping_mul(31).wrapping_add(pair);
            }
        }
    }
    acc
}

/// Strip processing order that places sidechain source strips before the
/// strips whose plugins consume them, so the source tap is complete in the
/// same block (`strip_count` is the mixing capacity). Consumers left in a
/// dependency cycle are flagged to read the previous block's tap.
fn compute_strip_order(
    model: &MixerModel,
    strip_count: usize,
) -> (Vec<usize>, Vec<Vec<Vec<bool>>>) {
    let mut delayed = vec![vec![vec![false; MAX_PLUGINS]; PLUGIN_CHAINS]; strip_count];
    // Consumers: (consumer strip, chain, slot, source strips).
    let mut consumers: Vec<(usize, usize, usize, Vec<usize>)> = Vec::new();
    let mut edges: HashSet<(usize, usize)> = HashSet::new();
    for strip in &model.strips {
        let sid = strip.id.0 as usize;
        if sid >= strip_count {
            continue;
        }
        for (plugins, chain) in [(&strip.plugins, 0usize), (&strip.plugins_post, 1usize)] {
            for (slot, plugin) in plugins.iter().enumerate() {
                if plugin.sidechain.is_empty() || slot >= MAX_PLUGINS {
                    continue;
                }
                let sources: Vec<usize> = plugin
                    .sidechain
                    .iter()
                    .map(|source| source.strip.0 as usize)
                    .filter(|source| *source < strip_count)
                    .collect();
                if sources.is_empty() {
                    continue;
                }
                for &source in &sources {
                    if source != sid {
                        edges.insert((source, sid));
                    }
                }
                consumers.push((sid, chain, slot, sources));
            }
        }
    }

    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); strip_count];
    let mut indegree = vec![0usize; strip_count];
    for &(source, destination) in &edges {
        adjacency[source].push(destination);
        indegree[destination] += 1;
    }

    // Kahn's algorithm (min-heap keeps the order deterministic).
    let mut heap = std::collections::BinaryHeap::new();
    for (node, &degree) in indegree.iter().enumerate() {
        if degree == 0 {
            heap.push(std::cmp::Reverse(node));
        }
    }
    let mut order = Vec::with_capacity(strip_count);
    while let Some(std::cmp::Reverse(node)) = heap.pop() {
        order.push(node);
        for &next in &adjacency[node] {
            indegree[next] -= 1;
            if indegree[next] == 0 {
                heap.push(std::cmp::Reverse(next));
            }
        }
    }
    if order.len() < strip_count {
        let placed: HashSet<usize> = order.iter().copied().collect();
        for node in 0..strip_count {
            if !placed.contains(&node) {
                order.push(node);
            }
        }
    }

    let mut position = vec![0usize; strip_count];
    for (pos, &sid) in order.iter().enumerate() {
        if sid < strip_count {
            position[sid] = pos;
        }
    }
    for (sid, chain, slot, sources) in &consumers {
        if sources
            .iter()
            .any(|&source| position[source] >= position[*sid])
        {
            delayed[*sid][*chain][*slot] = true;
        }
    }
    (order, delayed)
}

pub fn apply_model(params: &SharedParams, model: &MixerModel) {
    params.set_beats_per_bar(model.beats_per_bar);
    params.set_midi_sync(model.midi_sync);
    for index in 0..params.strip_count() {
        params.set_strip_active(index, false);
    }
    for index in 0..params.bus_count() {
        params.set_bus_active(index, false);
        params.set_bus_return(index, false);
        params.set_bus_send_gain(index, 1.0);
        params.set_bus_return_gain(index, 1.0);
    }

    for bus in &model.buses {
        let index = bus.id.0 as usize;
        params.set_bus_active(index, true);
        params.set_bus_gain(index, bus.gain.0);
        params.set_bus_mute(index, bus.mute);
        params.set_bus_mix_master(index, bus.mix_to_master);
        // The master has no separate send, so it never returns externally.
        params.set_bus_return(index, bus.return_inputs && index != 0);
        params.set_bus_send_gain(index, bus.send_gain.0);
        params.set_bus_return_gain(index, bus.return_gain.0);
        for (slot, plugin) in bus.plugins.iter().enumerate() {
            params.clear_bus_sidechain(index, slot);
            if plugin.sidechain_enabled {
                for entry in &plugin.sidechain {
                    let source = entry.strip.0 as usize;
                    params.set_bus_sidechain_gain(index, slot, source, entry.gain.0);
                    params.set_bus_sidechain_pre(index, slot, source, entry.pre_fader);
                }
            }
        }
    }

    for strip in &model.strips {
        let index = strip.id.0 as usize;
        params.set_strip_active(index, true);
        params.set_strip_gain(index, strip.gain.0);
        params.set_strip_pan(index, strip.pan.0);
        params.set_strip_input_gain_db(index, strip.input_gain_db);
        params.set_strip_mute(index, strip.mute);
        params.set_strip_solo(index, strip.solo);

        for bus in 0..params.bus_count() {
            params.set_send_gain(index, bus, 0.0);
            params.set_send_delay(index, bus, 0.0);
            params.set_send_pre_fader(index, bus, false);
        }
        for send in &strip.sends {
            params.set_send_gain(index, send.bus.0 as usize, send.gain.0);
            params.set_send_delay(index, send.bus.0 as usize, send.delay_ms);
            params.set_send_pre_fader(index, send.bus.0 as usize, send.pre_fader);
        }

        for (chain, plugins) in [&strip.plugins, &strip.plugins_post]
            .into_iter()
            .enumerate()
        {
            for (slot, plugin) in plugins.iter().enumerate() {
                params.clear_sidechain(index, chain, slot);
                if plugin.sidechain_enabled {
                    for entry in &plugin.sidechain {
                        let source = entry.strip.0 as usize;
                        params.set_sidechain_gain(index, chain, slot, source, entry.gain.0);
                        params.set_sidechain_pre(index, chain, slot, source, entry.pre_fader);
                    }
                }
            }
        }
    }
}

/// Sets a plugin's sidechain send gain for `strip`, creating the entry if
/// needed (kept at gain 0 so its pre/post choice survives).
fn set_sidechain_source(list: &mut Vec<SidechainSource>, strip: StripId, gain: f32) {
    if let Some(entry) = list.iter_mut().find(|entry| entry.strip == strip) {
        entry.gain = Gain(gain);
    } else {
        let mut entry = SidechainSource::new(strip);
        entry.gain = Gain(gain);
        list.push(entry);
    }
}

fn set_sidechain_pre(list: &mut Vec<SidechainSource>, strip: StripId, pre: bool) {
    if let Some(entry) = list.iter_mut().find(|entry| entry.strip == strip) {
        entry.pre_fader = pre;
    } else {
        let mut entry = SidechainSource::new(strip);
        entry.gain = Gain(0.0);
        entry.pre_fader = pre;
        list.push(entry);
    }
}

pub fn clamp_to_capacity(model: &mut MixerModel, strips: usize, buses: usize) {
    model.strips.retain(|strip| (strip.id.0 as usize) < strips);
    model.buses.retain(|bus| (bus.id.0 as usize) < buses);
    for strip in &mut model.strips {
        strip.sends.retain(|send| (send.bus.0 as usize) < buses);
        strip.plugins.truncate(MAX_PLUGINS);
        strip.plugins_post.truncate(MAX_PLUGINS);
        for plugin in strip
            .plugins
            .iter_mut()
            .chain(strip.plugins_post.iter_mut())
        {
            plugin
                .sidechain
                .retain(|source| (source.strip.0 as usize) < strips);
        }
    }
    for bus in &mut model.buses {
        bus.plugins.truncate(MAX_PLUGINS);
        for plugin in bus.plugins.iter_mut() {
            plugin
                .sidechain
                .retain(|source| (source.strip.0 as usize) < strips);
        }
        if bus.id.0 == 0 {
            bus.return_inputs = false;
        }
    }

    if model.strips.is_empty() || model.buses.is_empty() {
        *model = MixerModel::default_minimal();
    }
    model.normalize_buses();
}

fn save_session(model: &MixerModel, path: &str) -> Result<(), String> {
    let text = toml::to_string_pretty(model).map_err(|error| {
        tracing::warn!(%error, "session serialize failed");
        error.to_string()
    })?;
    fs::write(path, text).map_err(|error| {
        tracing::warn!(%error, %path, "session save failed");
        error.to_string()
    })?;
    tracing::info!(%path, "session saved");
    Ok(())
}

fn load_session(path: &str) -> Result<MixerModel, String> {
    let text = fs::read_to_string(path).map_err(|error| {
        tracing::warn!(%error, %path, "session read failed");
        error.to_string()
    })?;
    let mut model: MixerModel = toml::from_str(&text).map_err(|error| {
        tracing::warn!(%error, %path, "session parse failed");
        error.to_string()
    })?;
    // Sessions saved before the master bus was capitalised still say "master".
    if let Some(master) = model.buses.iter_mut().find(|bus| bus.id.0 == 0) {
        if master.name == "master" {
            master.name = "Master".to_string();
        }
    }
    Ok(model)
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

fn loops_dir() -> PathBuf {
    config_dir().join("loops")
}

fn timestamp_slug() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    if now == -1 {
        return "0".to_string();
    }
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return now.to_string();
    }
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn sanitize_file_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        "loop".to_string()
    } else {
        out
    }
}

/// Write an interleaved stereo loop, applying the circular nudge first.
fn write_loop_wav(
    path: &Path,
    raw: &[f32],
    frames: usize,
    sample_rate: u32,
    bit_depth: u8,
    nudge: i64,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let depth = match bit_depth {
        16 | 24 | 32 => bit_depth,
        _ => 24,
    };
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: depth as u16,
        sample_format: if depth == 32 {
            hound::SampleFormat::Float
        } else {
            hound::SampleFormat::Int
        },
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    let rotated = rotate_interleaved(raw, frames, nudge);
    for sample in rotated {
        match depth {
            16 => writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)?,
            24 => writer.write_sample((sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32)?,
            _ => writer.write_sample(sample)?,
        }
    }
    writer.finalize()?;
    Ok(())
}

/// Read a stereo WAV as interleaved f32. Keeps the engine sample rate fixed.
fn read_loop_wav(path: &Path, expected_rate: u32) -> anyhow::Result<(Vec<f32>, usize)> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    if spec.channels != 2 {
        anyhow::bail!("expected a stereo WAV, found {} channels", spec.channels);
    }
    if spec.sample_rate != expected_rate {
        anyhow::bail!(
            "loop is {} Hz but the engine runs at {} Hz",
            spec.sample_rate,
            expected_rate
        );
    }

    let mut out = Vec::new();
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => {
            for sample in reader.samples::<f32>() {
                out.push(sample?);
            }
        }
        (hound::SampleFormat::Int, 16) => {
            for sample in reader.samples::<i16>() {
                out.push(sample? as f32 / 32_768.0);
            }
        }
        (hound::SampleFormat::Int, 24) => {
            for sample in reader.samples::<i32>() {
                out.push(sample? as f32 / 8_388_608.0);
            }
        }
        (hound::SampleFormat::Int, 32) => {
            for sample in reader.samples::<i32>() {
                out.push(sample? as f32 / 2_147_483_648.0);
            }
        }
        (format, bits) => anyhow::bail!("unsupported WAV format: {format:?}, {bits} bits"),
    }
    if out.len() % 2 != 0 {
        anyhow::bail!("WAV has an odd number of samples");
    }
    let frames = out.len() / 2;
    if frames == 0 {
        anyhow::bail!("WAV is empty");
    }
    Ok((out, frames))
}

#[cfg(test)]
mod wav_tests {
    use super::*;

    #[test]
    fn wav_roundtrip_for_each_depth() {
        let dir = std::env::temp_dir().join(format!("vibemix-wav-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let raw = vec![0.0, 0.5, -0.25, 0.125, 0.75, -0.75, 0.0, 0.0];
        for depth in [16u8, 24, 32] {
            let path = dir.join(format!("loop-{depth}.wav"));
            write_loop_wav(&path, &raw, 4, 48_000, depth, 0).unwrap();
            let (read, frames) = read_loop_wav(&path, 48_000).unwrap();
            assert_eq!(frames, 4);
            assert_eq!(read.len(), raw.len());
            let tolerance = if depth == 16 { 1.0e-3 } else { 1.0e-4 };
            for (a, b) in read.iter().zip(raw.iter()) {
                assert!((a - b).abs() <= tolerance, "{depth}: {a} vs {b}");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
