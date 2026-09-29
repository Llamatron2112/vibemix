use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vibemix_core::config::config_dir;
use vibemix_core::ids::{BusId, StripId};
use vibemix_core::model::{
    Bus, MidiBinding, MidiTarget, MixerModel, PluginSlot, Send, Strip, WindowGeometry,
    MAX_SEND_DELAY_MS,
};
use crate::midi_map::target_from_code;
use vibemix_core::model::PluginFormat;
use vibemix_core::params::{clamp_trim_db, Gain, Pan};
use vibemix_lv2::{find_ui, Host, SharedOutputs, UiCandidate, UiWindow};
use vibemix_proto::ControlMessage;

use crate::backend_jack::{encode_state, instantiate_slot, JackEngine};
use crate::catalog::PluginEntry;
use crate::dsp;
use crate::health::{HealthStore, Scan, ScanEvent, Status};
use crate::params::{SharedMeters, SharedParams, MAX_PARAMS, MAX_PLUGINS, PLUGIN_CHAINS};

const PLUGIN_LOC_STRIP_PRE: u32 = 0;
const PLUGIN_LOC_STRIP_POST: u32 = 1;

const METER_INTERVAL: Duration = Duration::from_millis(20);
/// Minimum gap between catalog rebuilds while the plugin sweep streams results.
const CATALOG_THROTTLE: Duration = Duration::from_millis(250);

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
    /// Previous lilv worlds, kept alive so plugin instances (and UIs) created
    /// from them stay valid after a rescan swaps in a fresh `host`.
    retired_hosts: Vec<Host>,
    plugins: Vec<PluginEntry>,
    catalog_json: String,
    health: HealthStore,
    health_dirty: bool,
    /// Force the next catalog rebuild immediately (manual verdicts, sweep done).
    health_immediate: bool,
    scan: Scan,
    smoke_ok: HashSet<String>,
    smoke_bad: HashSet<String>,
    uis: HashMap<(u32, u32, u32), (UiWindow, Box<UiWriteCtx>)>,
    ui_cache: HashMap<String, Option<UiCandidate>>,
    ui_safe: HashMap<String, bool>,
    missing_report: Option<String>,
    pending_session: Option<(bool, String, String)>,
    sample_rate: u32,
    clipboard: Option<PluginClipboard>,
    clipboard_dirty: bool,
    undo_stack: VecDeque<UndoEntry>,
    redo_stack: Vec<UndoEntry>,
    undo_bytes: usize,
    undo_coalesce: Option<(Instant, EditKey)>,
    edit_dirty: bool,
    pending_learn: Option<MidiBinding>,
}

struct UiWriteCtx {
    location: u32,
    target: usize,
    slot: usize,
    params: Arc<SharedParams>,
    ports: Vec<(u32, usize)>,
    outputs: Option<Arc<SharedOutputs>>,
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
    /// Forwards the plugin's output port values to its native UI (host ->
    /// `ui:portEvent`), so meters/analysers animate even for plugins that do
    /// not read their DSP ports directly via `instance-access`.
    fn forward_outputs(&mut self, window: &UiWindow) {
        let Some(outputs) = &self.outputs else {
            return;
        };
        if !outputs.is_active() {
            return;
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
            let value = f32::from_bits(bits);
            window.port_event(
                outputs.control_port(index),
                std::mem::size_of::<f32>() as u32,
                0,
                &value.to_ne_bytes(),
            );
        }

        let atoms = outputs.atom_port_count();
        if self.last_atom_seq.len() != atoms {
            self.last_atom_seq = vec![usize::MAX; atoms];
        }
        for index in 0..atoms {
            let seq = outputs.atom_seq(index);
            if seq == 0 || self.last_atom_seq[index] == seq {
                continue;
            }
            if let Some((bytes, format)) = outputs.read_atom(index) {
                self.last_atom_seq[index] = seq;
                window.port_event(outputs.atom_port(index), bytes.len() as u32, format, &bytes);
            }
        }
    }
}

extern "C" fn ui_port_write(
    user: *mut std::ffi::c_void,
    port_index: u32,
    size: u32,
    format: u32,
    buffer: *const std::ffi::c_void,
) {
    if user.is_null() || format != 0 || size != 4 || buffer.is_null() {
        return;
    }
    let context = unsafe { &*(user as *const UiWriteCtx) };
    let value = unsafe { *(buffer as *const f32) };
    if let Some((_, param)) = context
        .ports
        .iter()
        .find(|(port, _)| *port == port_index)
    {
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
}

impl Controller {
    pub fn new(
        port: u16,
        params: Arc<SharedParams>,
        meters: Arc<SharedMeters>,
        model: MixerModel,
        engine: JackEngine,
        host: Host,
        vst3: vibemix_vst3::Host,
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("127.0.0.1", port))?;
        socket.set_read_timeout(Some(METER_INTERVAL))?;

        let plugins: Vec<PluginEntry> = {
            let discovered = discover_catalog(&host, &vst3);
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
            retired_hosts: Vec::new(),
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
            ui_safe: HashMap::new(),
            missing_report,
            pending_session: None,
            sample_rate,
            clipboard: None,
            clipboard_dirty: false,
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
            undo_bytes: 0,
            undo_coalesce: None,
            edit_dirty: false,
            pending_learn: None,
        };
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
                    ScanEvent::Verdict { uri, status, reason } => {
                        self.health.set(&uri, status, reason);
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
            self.pump_uis();

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
                    binding.cc = learned.cc;
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
            if actions.catalog {
                self.send_catalog(address);
            }
            if actions.snapshot {
                self.send_state(address);
            }
            if self.clipboard_dirty {
                self.clipboard_dirty = false;
                self.send_clipboard(address);
            }
            if self.edit_dirty || actions.snapshot {
                self.edit_dirty = false;
                self.send_edit_state(address);
            }

            let levels = self.meters.levels();
            let message = ControlMessage::Meters {
                strips_in_l: levels.strips_in_l,
                strips_in_r: levels.strips_in_r,
                strips_gain_l: levels.strips_gain_l,
                strips_gain_r: levels.strips_gain_r,
                strips_out_l: levels.strips_out_l,
                strips_out_r: levels.strips_out_r,
                buses_in_l: levels.buses_in_l,
                buses_in_r: levels.buses_in_r,
                buses_gain_l: levels.buses_gain_l,
                buses_gain_r: levels.buses_gain_r,
                buses_ret_l: levels.buses_ret_l,
                buses_ret_r: levels.buses_ret_r,
                buses_l: levels.buses_l,
                buses_r: levels.buses_r,
            };
            if let Ok(bytes) = message.encode() {
                let _ = self.socket.send_to(&bytes, address);
            }

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

        if let Err(error) =
            self.engine
                .rebuild_plugins(&self.host, &self.vst3, &mut self.model, &self.params, rate as f64)
        {
            tracing::warn!(%error, "failed to rebuild plugins after sample-rate change");
        }
        self.uis.clear();
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
                self.params
                    .set_send_delay(strip as usize, bus as usize, ms);
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
                self.uis.retain(|(entry_strip, _, _), _| *entry_strip != strip);
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
                Actions::default()
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
                self.uis.remove(&(location, target, slot));
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
                let old = std::mem::replace(&mut self.host, Host::new());
                self.retired_hosts.push(old);
                self.plugins = discover_catalog(&self.host, &self.vst3);
                if self.plugins.is_empty() {
                    self.plugins = load_catalog();
                }
                self.catalog_json = build_catalog(&self.plugins, &self.health);
                save_catalog(&self.plugins);
                let sample_rate = self.engine.sample_rate() as f64;
                if let Err(error) =
                    self.engine
                        .rebuild_plugins(&self.host, &self.vst3, &mut self.model, &self.params, sample_rate)
                {
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
            ControlMessage::Catalog { .. }
            | ControlMessage::State { .. }
            | ControlMessage::PluginScan { .. }
            | ControlMessage::SessionResult { .. }
            | ControlMessage::MissingConnections { .. } => Actions::default(),
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
                self.pending_session = Some(match save_session(&self.model, &path) {
                    Ok(()) => (true, path.clone(), String::new()),
                    Err(error) => (false, path.clone(), error),
                });
                Actions::default()
            }
            ControlMessage::NewSession => {
                let mut model = MixerModel::default_minimal();
                clamp_to_capacity(&mut model, self.params.strip_count(), self.params.bus_count());
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
                match self
                    .engine
                    .rebuild_plugins(&self.host, &self.vst3, &mut model, &self.params, sample_rate)
                {
                    Ok(()) => {
                        self.uis.clear();
                        self.model = model;
                        self.engine.sync_port_names(&self.model);
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
                    clamp_to_capacity(&mut loaded, self.params.strip_count(), self.params.bus_count());
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
                    match self
                        .engine
                        .rebuild_plugins(&self.host, &self.vst3, &mut loaded, &self.params, sample_rate)
                    {
                        Ok(()) => {
                            self.uis.clear();
                            self.model = loaded;
                            self.engine.sync_port_names(&self.model);
                            self.rebuild_shared_bindings();
                            self.missing_report = missing_report_json(
                                &self.engine.restore_connections(&self.model),
                            );
                            self.clear_history();
                            self.pending_session = Some((true, path.clone(), String::new()));
                            Actions {
                                snapshot: true,
                                catalog: false,
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "failed to rebuild plugins for session");
                            self.pending_session =
                                Some((false, path.clone(), error.to_string()));
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
                    cc: 0,
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
                self.model.midi_bindings.retain(|binding| {
                    !binding_targets(binding, target_kind, location, target, slot, param)
                });
                self.model.midi_bindings.push(MidiBinding {
                    channel,
                    cc: cc.min(127) as u8,
                    relative: false,
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
            ControlMessage::Clock { .. } => Actions::default(),
            ControlMessage::Meters { .. } => Actions::default(),
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
                    if let Some(proxy) = self.engine.state_proxy(strip_index, chain, slot) {
                        if proxy.supports_save() {
                            plugin.state = encode_state(&self.host, &proxy.save());
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
                if let Some(proxy) = self.engine.bus_state_proxy(bus_index, slot) {
                    if proxy.supports_save() {
                        plugin.state = encode_state(&self.host, &proxy.save());
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
        self.health.set(uri, status, reason);
        self.health_dirty = true;
        self.health_immediate = true;
    }

    fn plugin_is_safe(&mut self, uri: &str) -> bool {
        if self.smoke_ok.contains(uri) {
            return true;
        }
        if self.smoke_bad.contains(uri) {
            return false;
        }

        let Ok(executable) = std::env::current_exe() else {
            return true;
        };
        let mut command = Command::new(executable);
        command.arg("--smoke").arg(uri);
        if crate::smoke::debug_enabled() {
            // Leave stdio inherited so a plugin's own diagnostics stay visible.
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        // Safety: only adjusts the child's resource limits.
        unsafe {
            command.pre_exec(|| {
                crate::smoke::disable_core_dumps();
                Ok(())
            });
        }
        let Ok(mut child) = command.spawn() else {
            return true;
        };

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    self.smoke_ok.insert(uri.to_string());
                    self.record_health(uri, Status::Ok, String::new());
                    return true;
                }
                Ok(Some(status)) => {
                    self.smoke_bad.insert(uri.to_string());
                    let reason = if let Some(signal) = status.signal() {
                        let name = crate::smoke::signal_name(signal);
                        tracing::warn!(%uri, signal, "plugin crashed during smoke test, refusing to load");
                        name.to_string()
                    } else {
                        let code = status.code().unwrap_or(-1);
                        tracing::warn!(%uri, code, "plugin failed smoke test, refusing to load");
                        format!("exit {code}")
                    };
                    self.record_health(uri, Status::Failed, reason);
                    return false;
                }
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        self.smoke_bad.insert(uri.to_string());
                        tracing::warn!(%uri, "plugin smoke test timed out, refusing to load");
                        self.record_health(uri, Status::Failed, "timeout".to_string());
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return true,
            }
        }
    }

    fn ui_is_safe(&mut self, uri: &str) -> bool {
        if let Some(safe) = self.ui_safe.get(uri) {
            return *safe;
        }

        let safe = (|| {
            let executable = std::env::current_exe().ok()?;
            let mut child = Command::new(executable)
                .arg("--smoke-ui")
                .arg(uri)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return Some(status.success()),
                    Ok(None) => {
                        if Instant::now() > deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Some(false);
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return Some(true),
                }
            }
        })()
        .unwrap_or(true);

        if !safe {
            tracing::warn!(%uri, "plugin UI failed smoke test; not opening");
        }
        self.ui_safe.insert(uri.to_string(), safe);
        safe
    }

    fn open_ui(&mut self, location: u32, target: u32, slot: u32) {
        let key = (location, target, slot);
        if self.uis.contains_key(&key) {
            return;
        }
        let (uri, handle) = if let Some(chain) = strip_chain(location) {
            let uri = self
                .model
                .strip(StripId(target))
                .and_then(|entry| strip_plugin_list(entry, chain).get(slot as usize))
                .map(|plugin| plugin.uri.clone());
            let handle = self.engine.plugin_handle(target as usize, chain, slot as usize);
            match (uri, handle) {
                (Some(uri), Some(handle)) => (uri, handle),
                _ => return,
            }
        } else {
            let uri = self
                .model
                .bus(BusId(target))
                .and_then(|entry| entry.plugins.get(slot as usize))
                .map(|plugin| plugin.uri.clone());
            let handle = self.engine.bus_plugin_handle(target as usize, slot as usize);
            match (uri, handle) {
                (Some(uri), Some(handle)) => (uri, handle),
                _ => return,
            }
        };

        if !self.ui_is_safe(&uri) {
            return;
        }

        // Title the native window with the plugin's name (user rename wins,
        // then the catalog name, then the URI).
        let model_name = if let Some(chain) = strip_chain(location) {
            self.model
                .strip(StripId(target))
                .and_then(|entry| strip_plugin_list(entry, chain).get(slot as usize))
                .map(|plugin| plugin.name.clone())
        } else {
            self.model
                .bus(BusId(target))
                .and_then(|entry| entry.plugins.get(slot as usize))
                .map(|plugin| plugin.name.clone())
        };
        let title = model_name
            .filter(|name| !name.is_empty())
            .or_else(|| {
                self.plugins
                    .iter()
                    .find(|info| info.id == uri)
                    .map(|info| info.name.clone())
            })
            .unwrap_or_else(|| uri.clone());

        let candidate = self
            .ui_cache
            .entry(uri.clone())
            .or_insert_with(|| find_ui(&uri))
            .clone();
        let Some(candidate) = candidate else {
            tracing::warn!(%uri, "no hostable X11 UI");
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
            self.engine.plugin_outputs(target as usize, chain, slot as usize)
        } else {
            self.engine.bus_plugin_outputs(target as usize, slot as usize)
        };

        let mut context = Box::new(UiWriteCtx {
            location,
            target: target as usize,
            slot: slot as usize,
            params: Arc::clone(&self.params),
            ports,
            outputs,
            last_control: Vec::new(),
            last_atom_seq: Vec::new(),
        });

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
        self.uis.insert(key, (window, context));
    }

    /// Mirrors a host-driven parameter change into the plugin's native UI, so
    /// the UI's own controls follow edits made in the vibemix editor.
    fn reflect_param_to_ui(&self, location: u32, target: u32, slot: u32, param: u32, value: f32) {
        let Some((window, context)) = self.uis.get(&(location, target, slot)) else {
            return;
        };
        let Some((port, _)) = context
            .ports
            .iter()
            .find(|(_, mapped)| *mapped == param as usize)
        else {
            return;
        };
        window.port_event(
            *port,
            std::mem::size_of::<f32>() as u32,
            0,
            &value.to_ne_bytes(),
        );
    }

    fn pump_uis(&mut self) {
        let mut closed = Vec::new();
        for (key, (window, context)) in self.uis.iter_mut() {
            window.pump();
            context.forward_outputs(window);
            if window.wants_close() {
                closed.push(*key);
            }
        }
        for key in closed {
            self.uis.remove(&key);
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
        let Some(info) = self.plugins.iter().find(|entry| entry.id == uri).cloned() else {
            tracing::warn!(%uri, "plugin not in catalog");
            return false;
        };
        let mut plugin = PluginSlot::new(plugin_id(0), uri);
        plugin.format = info.format;
        plugin.bundle = info.bundle.clone();
        plugin.name = info.name;
        plugin.params = match info.format {
            PluginFormat::Lv2 => info
                .controls
                .iter()
                .map(|control| control.default)
                .collect(),
            // VST3 parameter metadata isn't in the catalog yet; seed the
            // defaults from a throwaway instance so the DSP starts neutral.
            PluginFormat::Vst3 => {
                let sample_rate = self.engine.sample_rate() as f64;
                let buffer_size = self.engine.buffer_size() as usize;
                info.bundle
                    .as_deref()
                    .and_then(|bundle| {
                        self.vst3.instantiate(
                            bundle,
                            &info.id,
                            sample_rate,
                            buffer_size,
                            vibemix_vst3::PluginInstance::max_frames(),
                        )
                    })
                    .map(|instance| {
                        (0..instance.control_count())
                            .map(|param| instance.control_default(param))
                            .collect()
                    })
                    .unwrap_or_default()
            }
        };
        self.insert_plugin_slot(location, target, plugin)
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

        // LV2 plugins are smoke-tested in a throwaway child. VST3 has no
        // sandboxed smoke test yet, so adding one instantiates it in-process.
        if plugin.format == PluginFormat::Lv2 && !self.plugin_is_safe(&plugin.uri) {
            return false;
        }

        let sample_rate = self.engine.sample_rate() as f64;
        let buffer_size = self.engine.buffer_size() as usize;
        let Some((slot_plugin, proxy, handle)) =
            instantiate_slot(&self.host, &self.vst3, &plugin, sample_rate, buffer_size)
        else {
            tracing::warn!(uri = %plugin.uri, "plugin instantiate failed");
            return false;
        };

        plugin.id = plugin_id(slot);
        plugin.missing = false;
        let uri = plugin.uri.clone();
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
                    self.params.plugin_param(target as usize, chain, slot as usize, param);
            }
            plugin.bypassed = self
                .params
                .plugin_bypass(target as usize, chain, slot as usize);
            if let Some(proxy) = self.engine.state_proxy(target as usize, chain, slot as usize) {
                if proxy.supports_save() {
                    plugin.state = encode_state(&self.host, &proxy.save());
                }
            }
        } else {
            for param in 0..controls {
                plugin.params[param] =
                    self.params.bus_plugin_param(target as usize, slot as usize, param);
            }
            plugin.bypassed = self.params.bus_plugin_bypass(target as usize, slot as usize);
            if let Some(proxy) = self.engine.bus_state_proxy(target as usize, slot as usize) {
                if proxy.supports_save() {
                    plugin.state = encode_state(&self.host, &proxy.save());
                }
            }
        }
        Some(plugin)
    }

    fn copy_plugin(&mut self, location: u32, target: u32, slot: u32) -> bool {
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
        self.params.set_strip_input_gain_db(slot, source.input_gain_db);
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
            self.uis.clear();
            let sample_rate = self.engine.sample_rate() as f64;
            if let Err(error) =
                self.engine
                    .rebuild_plugins(&self.host, &self.vst3, &mut self.model, &self.params, sample_rate)
            {
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
                    self.params
                        .set_bus_plugin_param(index, slot, param, *value);
                }
                self.params.set_bus_plugin_bypass(index, slot, plugin.bypassed);
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
            MidiTarget::BusGain | MidiTarget::BusMute => {
                self.model.bus(BusId(target_id)).and_then(|bus| bus.midi_channel)
            }
            MidiTarget::PluginParam | MidiTarget::PluginBypass => {
                if location <= 1 {
                    let strip = self.model.strip(StripId(target_id))?;
                    strip_plugin_list(strip, location as usize)
                        .get(slot as usize)?
                        .midi_channel
                } else {
                    self.model.bus(BusId(target_id))?.plugins.get(slot as usize)?.midi_channel
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

    fn remove_plugin(&mut self, location: u32, target: u32, slot: u32) -> bool {
        self.uis
            .retain(|(entry_loc, entry_target, _), _| {
                !(*entry_loc == location && *entry_target == target)
            });

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
            if let Err(error) = self.engine.remove_bus_plugin(target as usize, slot as usize) {
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
        tracing::info!(location, target, slot, "removed plugin");
        true
    }

    fn move_plugin(&mut self, location: u32, target: u32, from: u32, to: u32) -> bool {
        self.uis
            .retain(|(entry_loc, entry_target, _), _| {
                !(*entry_loc == location && *entry_target == target)
            });

        if let Some(chain) = strip_chain(location) {
            let count = match self.model.strip(StripId(target)) {
                Some(strip) => strip_plugin_list(strip, chain).len(),
                None => return false,
            };
            if from == to || from as usize >= count || to as usize >= count {
                return false;
            }

            self.pause_rt();
            if let Err(error) = self
                .engine
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
            if let Err(error) = self
                .engine
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
        tracing::info!(location, target, from, to, "moved plugin");
        true
    }

    fn send_catalog(&self, address: SocketAddr) {
        let message = ControlMessage::Catalog {
            json: self.catalog_json.clone(),
        };
        if let Ok(bytes) = message.encode() {
            let _ = self.socket.send_to(&bytes, address);
        }
    }

    fn send_plugin_scan(&self, address: SocketAddr) {
        let message = ControlMessage::PluginScan {
            running: self.scan.running,
            done: self.scan.done as u32,
            total: self.scan.total as u32,
        };
        if let Ok(bytes) = message.encode() {
            let _ = self.socket.send_to(&bytes, address);
        }
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
        if let Ok(bytes) = message.encode() {
            let _ = self.socket.send_to(&bytes, address);
        }
    }

    fn send_missing(&self, address: SocketAddr, json: &str) {
        let message = ControlMessage::MissingConnections {
            json: json.to_string(),
        };
        if let Ok(bytes) = message.encode() {
            let _ = self.socket.send_to(&bytes, address);
        }
    }

    fn send_session_result(&self, address: SocketAddr, ok: bool, path: &str, error: &str) {
        let message = ControlMessage::SessionResult {
            ok,
            path: path.to_string(),
            error: error.to_string(),
        };
        if let Ok(bytes) = message.encode() {
            let _ = self.socket.send_to(&bytes, address);
        }
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
        if let Ok(bytes) = message.encode() {
            let _ = self.socket.send_to(&bytes, address);
        }
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
        if let Ok(bytes) = message.encode() {
            let _ = self.socket.send_to(&bytes, address);
        }
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
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.uri == y.uri && x.state == y.state)
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
            let (status, reason) = if let Some(reason) = &entry.unsupported {
                (Some("unsupported"), reason.clone())
            } else if !entry.hostable {
                (Some("unsupported"), "unsupported port layout".to_string())
            } else if let Some(verdict) = health.get(&entry.id) {
                (Some(verdict.status.as_str()), verdict.reason.clone())
            } else {
                (None, String::new())
            };
            serde_json::json!({
                "uri": entry.id,
                "format": entry.format.as_str(),
                "name": entry.name,
                "has_ui": entry.has_ui,
                "inputs": entry.inputs,
                "outputs": entry.outputs,
                "midi": entry.midi,
                "status": status,
                "reason": reason,
            })
        })
        .collect();
    serde_json::Value::Array(list).to_string()
}

/// Mono/stereo LV2 plugins plus every VST3 effect, in catalog order.
fn discover_catalog(host: &Host, vst3: &vibemix_vst3::Host) -> Vec<PluginEntry> {
    let mut entries: Vec<PluginEntry> = host
        .discover()
        .into_iter()
        .filter(|info| {
            (info.audio_inputs.len() == 2 && info.audio_outputs.len() == 2)
                || (info.audio_inputs.len() == 1 && info.audio_outputs.len() == 1)
        })
        .map(|info| {
            let unsupported = unsupported_required_feature(&info);
            PluginEntry {
                format: PluginFormat::Lv2,
                id: info.uri,
                name: info.name,
                bundle: None,
                has_ui: info.has_ui,
                inputs: info.audio_inputs.len(),
                outputs: info.audio_outputs.len(),
                midi: !info.midi_inputs.is_empty(),
                hostable: info.hostable,
                unsupported,
                controls: info.controls,
            }
        })
        .collect();
    for info in vst3.discover() {
        entries.push(PluginEntry {
            format: PluginFormat::Vst3,
            id: info.class_id,
            name: info.name,
            bundle: Some(info.bundle),
            has_ui: false,
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

fn catalog_path() -> std::path::PathBuf {
    config_dir().join("catalog.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CatalogEntry {
    #[serde(alias = "uri")]
    id: String,
    name: String,
    #[serde(default)]
    format: PluginFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bundle: Option<String>,
    #[serde(default)]
    has_ui: bool,
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
            bundle: entry.bundle,
            has_ui: entry.has_ui,
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
            format: entry.format,
            bundle: entry.bundle.clone(),
            has_ui: entry.has_ui,
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
            serde_json::json!({
                "slot": slot,
                "uri": plugin.uri,
                "name": plugin.name,
                "bypassed": plugin.bypassed,
                "missing": plugin.missing,
                "has_ui": has_ui,
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
        "midi_sync": model.midi_sync,
    })
    .to_string()
}

fn free_strip_slot(params: &SharedParams) -> Option<usize> {
    (0..params.strip_count()).find(|index| !params.strip_active(*index))
}

fn free_bus_slot(params: &SharedParams) -> Option<usize> {
    (0..params.bus_count()).find(|index| !params.bus_active(*index))
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
        }
        for send in &strip.sends {
            params.set_send_gain(index, send.bus.0 as usize, send.gain.0);
            params.set_send_delay(index, send.bus.0 as usize, send.delay_ms);
        }
    }
}

pub fn clamp_to_capacity(model: &mut MixerModel, strips: usize, buses: usize) {
    model.strips.retain(|strip| (strip.id.0 as usize) < strips);
    model.buses.retain(|bus| (bus.id.0 as usize) < buses);
    for strip in &mut model.strips {
        strip.sends.retain(|send| (send.bus.0 as usize) < buses);
        strip.plugins.truncate(MAX_PLUGINS);
        strip.plugins_post.truncate(MAX_PLUGINS);
    }
    for bus in &mut model.buses {
        bus.plugins.truncate(MAX_PLUGINS);
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
    toml::from_str(&text).map_err(|error| {
        tracing::warn!(%error, %path, "session parse failed");
        error.to_string()
    })
}
