use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use jack::{
    AudioIn, AudioOut, Client, ClientOptions, Control, Frames, MidiIn, MidiOut,
    NotificationHandler, Port, ProcessHandler, ProcessScope, RawMidi, Unowned,
};
use rtrb::{Consumer, Producer};
use vibemix_core::model::{BusConnections, MixerModel, PluginFormat, PluginSlot, StripConnections};
use vibemix_core::recorder::{RecorderSourceKind, RecorderTap};
use vibemix_lv2::{
    Host, MidiEvent, PluginInstance, SharedOutputs, SharedUiInputs, StateEntry, StateProxy,
};

use crate::plugin::PluginNode;
use crate::recorder::{bar_boundary_offset, LoopPlayback, RecordedTake, Recorder, MAX_LOOPS};

use crate::dsp;
use crate::midi::{
    is_sync_status, is_transport_status, Clock, MasterClock, MidiBlock, CONTINUE, START, STOP,
};
use crate::midi_map::SharedMidi;
use crate::params::{
    SharedClock, SharedMeters, SharedParams, SharedRecorder, MAX_PARAMS, MAX_PLUGINS, PLUGIN_CHAINS,
};

const COMMAND_CAPACITY: usize = 64;

/// How many finished takes can wait for the control thread. Depth covers
/// back-to-back loop captures even while a previous take is being analyzed.
const TAKE_RING_DEPTH: usize = 8;

/// Fractional DSP-load peak decay per real second (independent of the period).
const PEAK_DECAY_PER_SECOND: f32 = 0.08;

/// Serialized plugin state, backed by either host's cross-thread state handle.
#[derive(Clone)]
pub enum SlotState {
    Lv2(StateProxy),
    Vst3(vibemix_vst3::InstanceHandle),
    Clap(vibemix_clap::InstanceHandle),
}

impl SlotState {
    /// Session form of the state, or `None` when there is nothing to save.
    pub fn encode(&self, host: &Host) -> Option<String> {
        match self {
            SlotState::Lv2(proxy) if proxy.supports_save() => {
                Some(encode_state(host, &proxy.save()))
            }
            SlotState::Lv2(_) => None,
            SlotState::Vst3(handle) => {
                let bytes = handle.save();
                (!bytes.is_empty()).then(|| vibemix_vst3::encode_blob(&bytes))
            }
            SlotState::Clap(handle) => {
                let bytes = handle.save();
                (!bytes.is_empty()).then(|| vibemix_clap::encode_blob(&bytes))
            }
        }
    }

    /// The VST3 instance handle, if this is a VST3 plugin.
    pub fn vst3_handle(&self) -> Option<vibemix_vst3::InstanceHandle> {
        match self {
            SlotState::Vst3(handle) => Some(handle.clone()),
            SlotState::Lv2(_) | SlotState::Clap(_) => None,
        }
    }

    /// The CLAP instance handle, if this is a CLAP plugin.
    pub fn clap_handle(&self) -> Option<vibemix_clap::InstanceHandle> {
        match self {
            SlotState::Clap(handle) => Some(handle.clone()),
            SlotState::Lv2(_) | SlotState::Vst3(_) => None,
        }
    }
}

type StripChains = [Vec<SlotPlugin>; PLUGIN_CHAINS];
/// Control-thread bookkeeping for one plugin slot, kept in lockstep with the
/// matching [`SlotPlugin`] on the audio thread.
#[derive(Clone, Default)]
struct SlotMeta {
    /// State save/restore handle.
    proxy: Option<SlotState>,
    /// Raw instance handle for native editors.
    handle: Option<usize>,
    outputs: Option<Arc<SharedOutputs>>,
    ui_inputs: Option<Arc<SharedUiInputs>>,
}

impl SlotMeta {
    fn of(plugin: &SlotPlugin, proxy: Option<SlotState>, handle: Option<usize>) -> Self {
        Self {
            proxy,
            handle,
            outputs: plugin.outputs(),
            ui_inputs: plugin.ui_inputs(),
        }
    }
}

type StripMeta = [Vec<SlotMeta>; PLUGIN_CHAINS];

struct Built {
    strip_chains: Vec<StripChains>,
    strip_meta: Vec<StripMeta>,
    bus_chains: Vec<Vec<SlotPlugin>>,
    bus_meta: Vec<Vec<SlotMeta>>,
}

pub enum SlotPlugin {
    Stereo(Box<dyn PluginNode>),
    Mono(Box<dyn PluginNode>, Box<dyn PluginNode>),
    /// Placeholder for a plugin that could not be loaded. It keeps the DSP
    /// chain aligned with the model slots and passes audio through untouched.
    Missing,
}

/// Every scratch buffer sized by the host period, in one unit so a period
/// change can swap them together. The fields mirror the matching `MixerGraph`
/// members; the allocation happens on the control thread and the swap in
/// `apply_commands` is plain pointer moves at the top of a block, when no
/// references are held — so no pause is required.
pub struct ScratchBuffers {
    chain_dry: [Vec<f32>; 2],
    strip_scratch: [Vec<f32>; 2],
    strip_pre_fader: Vec<[Vec<f32>; 2]>,
    strip_input: Vec<[Vec<f32>; 2]>,
    strip_post_fader: Vec<[Vec<f32>; 2]>,
    strip_pre_snapshot: Vec<[Vec<f32>; 2]>,
    strip_post_snapshot: Vec<[Vec<f32>; 2]>,
    send_scratch: [Vec<f32>; 2],
    sidechain_mix: [Vec<f32>; 2],
    sidechain_silence: [Vec<f32>; 2],
    bus_scratch: [Vec<f32>; 2],
    bus_accum: Vec<[Vec<f32>; 2]>,
    bus_out: Vec<[Vec<f32>; 2]>,
    record_mix: [Vec<f32>; 2],
}

impl ScratchBuffers {
    pub fn new(capacity: usize, strip_count: usize, bus_count: usize) -> Self {
        Self {
            chain_dry: [vec![0.0; capacity], vec![0.0; capacity]],
            strip_scratch: [vec![0.0; capacity], vec![0.0; capacity]],
            strip_pre_fader: (0..strip_count)
                .map(|_| [vec![0.0; capacity], vec![0.0; capacity]])
                .collect(),
            strip_input: (0..strip_count)
                .map(|_| [vec![0.0; capacity], vec![0.0; capacity]])
                .collect(),
            strip_post_fader: (0..strip_count)
                .map(|_| [vec![0.0; capacity], vec![0.0; capacity]])
                .collect(),
            strip_pre_snapshot: (0..strip_count)
                .map(|_| [vec![0.0; capacity], vec![0.0; capacity]])
                .collect(),
            strip_post_snapshot: (0..strip_count)
                .map(|_| [vec![0.0; capacity], vec![0.0; capacity]])
                .collect(),
            send_scratch: [vec![0.0; capacity], vec![0.0; capacity]],
            sidechain_mix: [vec![0.0; capacity], vec![0.0; capacity]],
            sidechain_silence: [vec![0.0; capacity], vec![0.0; capacity]],
            bus_scratch: [vec![0.0; capacity], vec![0.0; capacity]],
            bus_accum: (0..bus_count)
                .map(|_| [vec![0.0f32; capacity], vec![0.0f32; capacity]])
                .collect(),
            bus_out: (0..bus_count)
                .map(|_| [vec![0.0; capacity], vec![0.0; capacity]])
                .collect(),
            record_mix: [vec![0.0; capacity], vec![0.0; capacity]],
        }
    }
}

impl SlotPlugin {
    fn load_state(&mut self, state: &str) {
        match self {
            SlotPlugin::Stereo(instance) => instance.load_state(state),
            SlotPlugin::Mono(left, right) => {
                left.load_state(state);
                right.load_state(state);
            }
            SlotPlugin::Missing => {}
        }
    }

    /// Processing latency reported by the hosted plugin.
    fn latency(&self) -> u32 {
        match self {
            SlotPlugin::Stereo(instance) => instance.latency(),
            SlotPlugin::Mono(left, right) => left.latency().max(right.latency()),
            SlotPlugin::Missing => 0,
        }
    }

    /// Output snapshot shared with the control thread for native-UI updates.
    /// For mono plugins hosted as stereo the first instance is used.
    fn outputs(&self) -> Option<Arc<SharedOutputs>> {
        match self {
            SlotPlugin::Stereo(instance) => instance.outputs(),
            SlotPlugin::Mono(left, _) => left.outputs(),
            SlotPlugin::Missing => None,
        }
    }

    /// UI→DSP mailbox shared with the control thread.
    fn ui_inputs(&self) -> Option<Arc<SharedUiInputs>> {
        match self {
            SlotPlugin::Stereo(instance) => instance.ui_inputs(),
            SlotPlugin::Mono(left, _) => left.ui_inputs(),
            SlotPlugin::Missing => None,
        }
    }
}

#[derive(serde::Serialize)]
struct StateBlob {
    key: String,
    type_: String,
    flags: u32,
    value: Vec<u8>,
}

pub fn encode_state(host: &Host, entries: &[StateEntry]) -> String {
    let blobs: Vec<StateBlob> = entries
        .iter()
        .filter_map(|entry| {
            let key = host.unmap_uri(entry.key)?;
            let type_ = host.unmap_uri(entry.type_)?;
            Some(StateBlob {
                key,
                type_,
                flags: entry.flags,
                value: entry.value.clone(),
            })
        })
        .collect();
    serde_json::to_string(&blobs).unwrap_or_default()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MissingConnection {
    pub owner_kind: &'static str,
    pub owner: String,
    pub port_role: &'static str,
    pub port: String,
}

/// A JACK port as exposed to the GUI patchbay.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PortInfo {
    pub name: String,
    pub client: String,
    pub short: String,
    /// `"audio"` or `"midi"`.
    pub r#type: String,
    pub input: bool,
    pub output: bool,
    pub physical: bool,
    /// Ports connected to this one (sources for an input, sinks for an output).
    pub connections: Vec<String>,
    /// Set for vibemix's own ports so the GUI can label them nicely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<PortRole>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PortRole {
    /// `"strip_in"`, `"bus_out"`, `"bus_ret"`, `"midi_in"` or `"midi_out"`.
    pub kind: &'static str,
    pub id: u32,
    /// `"l"`, `"r"`, or empty for MIDI.
    pub channel: &'static str,
    /// The strip/bus display name (or "MIDI").
    pub label: String,
}

/// Forwards JACK notifications we care about to the engine/control thread.
pub struct Notifications {
    pending_rate: Arc<AtomicU32>,
    xruns: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
}

/// MIDI loss counters published by the audio thread once per block, read by
/// the control thread for GUI diagnostics. The audio thread cannot log, so
/// losses are surfaced here instead.
#[derive(Default)]
pub struct MidiStats {
    dropped: AtomicU64,
    oversize: AtomicU64,
    out_errors: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiStatsSnapshot {
    /// Incoming MIDI events dropped because a block already queued the maximum.
    pub dropped: u64,
    /// Incoming messages skipped because they are longer than the fixed
    /// 4-byte event payload (e.g. SysEx), which cannot be forwarded
    /// losslessly.
    pub oversize: u64,
    /// MIDI-out port writes that failed (e.g. the JACK port buffer was full),
    /// so clock/transport pulses were not delivered.
    pub out_errors: u64,
}

impl NotificationHandler for Notifications {
    /// Runs in a signal-handler-like context: only flag the shutdown here and
    /// let the control thread react.
    unsafe fn shutdown(&mut self, _status: jack::ClientStatus, _reason: &str) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    fn sample_rate(&mut self, _client: &Client, srate: Frames) -> Control {
        self.pending_rate.store(srate, Ordering::SeqCst);
        Control::Continue
    }

    fn xrun(&mut self, _client: &Client) -> Control {
        self.xruns.fetch_add(1, Ordering::SeqCst);
        Control::Continue
    }
}

pub enum EngineCommand {
    AddPlugin {
        strip: usize,
        chain: usize,
        plugin: SlotPlugin,
    },
    RemovePlugin {
        strip: usize,
        chain: usize,
        slot: usize,
    },
    MovePlugin {
        strip: usize,
        chain: usize,
        from: usize,
        to: usize,
    },
    ClearStrip {
        strip: usize,
    },
    AddBusPlugin {
        bus: usize,
        plugin: SlotPlugin,
    },
    RemoveBusPlugin {
        bus: usize,
        slot: usize,
    },
    MoveBusPlugin {
        bus: usize,
        from: usize,
        to: usize,
    },
    ClearBus {
        bus: usize,
    },
    SetStripPorts {
        strip: usize,
        in_l: Port<AudioIn>,
        in_r: Port<AudioIn>,
    },
    ClearStripPorts {
        strip: usize,
    },
    SetBusPorts {
        bus: usize,
        out_l: Port<AudioOut>,
        out_r: Port<AudioOut>,
    },
    ClearBusPorts {
        bus: usize,
    },
    SetBusReturnPorts {
        bus: usize,
        in_l: Port<AudioIn>,
        in_r: Port<AudioIn>,
    },
    ClearBusReturnPorts {
        bus: usize,
    },
    Replace {
        chains: Vec<StripChains>,
        bus_chains: Vec<Vec<SlotPlugin>>,
    },
    SetSampleRate {
        sample_rate: f32,
        delays: Vec<Vec<dsp::DelayLine>>,
    },
    /// Swap in scratch buffers grown for a larger host period. The allocation
    /// happened on the control thread; the swap is applied at the top of the
    /// next callback pass, where no references into the scratch vectors are
    /// held yet.
    ResizeBuffers {
        capacity: usize,
        buffers: Box<ScratchBuffers>,
    },
    /// Sidechain-aware strip processing order and per-slot cycle flags.
    SetStripOrder {
        order: Vec<usize>,
        delayed: Vec<Vec<Vec<bool>>>,
    },
    /// Install the capture buffer and arm the recorder.
    RecorderArm {
        buffer: Box<[f32]>,
        capacity_frames: usize,
        expected_frames: usize,
        loop_pulses: u32,
        latency_frames: u32,
        loop_beats: f32,
    },
    RecorderCancel,
    SetLoop {
        slot: usize,
        buffer: Box<[f32]>,
        frames: usize,
        offset: i64,
    },
    /// Change the playback rotation in place, keeping the loop running.
    LoopOffset {
        slot: usize,
        offset: i64,
    },
    ClearLoop {
        slot: usize,
    },
    LoopPlay {
        slot: usize,
        quantized: bool,
    },
    LoopStop {
        slot: usize,
    },
    LoopGain {
        slot: usize,
        gain: f32,
    },
    LoopMute {
        slot: usize,
        mute: bool,
    },
}

/// Old strip order, retired on the control thread to keep the RT thread clean
/// (the fields are only held so their memory is freed off the audio thread).
pub struct RetiredOrder {
    _order: Vec<usize>,
    _delayed: Vec<Vec<Vec<bool>>>,
}

struct StripPorts {
    in_l: Port<AudioIn>,
    in_r: Port<AudioIn>,
}

struct BusPorts {
    out_l: Port<AudioOut>,
    out_r: Port<AudioOut>,
}

struct BusReturnPorts {
    in_l: Port<AudioIn>,
    in_r: Port<AudioIn>,
}

struct MixerGraph {
    params: Arc<SharedParams>,
    meters: Arc<SharedMeters>,
    strips: Vec<Option<StripPorts>>,
    buses: Vec<Option<BusPorts>>,
    bus_returns: Vec<Option<BusReturnPorts>>,
    bus_send_smooth: Vec<f32>,
    bus_return_smooth: Vec<f32>,
    plugin_chains: Vec<StripChains>,
    bus_chains: Vec<Vec<SlotPlugin>>,
    commands: Consumer<EngineCommand>,
    garbage: Producer<SlotPlugin>,
    garbage_bus_chains: Producer<Vec<Vec<SlotPlugin>>>,
    garbage_nested: Producer<Vec<StripChains>>,
    garbage_delays: Producer<Vec<Vec<dsp::DelayLine>>>,
    strip_gain_smooth: Vec<f32>,
    strip_pan_smooth: Vec<f32>,
    strip_input_smooth: Vec<f32>,
    bus_gain_smooth: Vec<f32>,
    send_gain_smooth: Vec<Vec<f32>>,
    send_delay_smooth: Vec<Vec<f32>>,
    send_delays: Vec<Vec<dsp::DelayLine>>,
    sample_rate: f32,
    /// Per-block smoothing coefficients, recomputed from the period each block.
    gain_coeff: f32,
    delay_coeff: f32,
    /// Plugin state is being captured; chains are not run this block.
    paused: bool,
    dsp_avg: f32,
    dsp_peak: f32,
    /// Wet fraction (1 = processed, 0 = bypassed) per insert, smoothed so a
    /// bypass toggle crossfades instead of clicking.
    bypass_mix_strip: Vec<[[f32; MAX_PLUGINS]; PLUGIN_CHAINS]>,
    bypass_mix_bus: Vec<[f32; MAX_PLUGINS]>,
    /// Copy of a chain's input while it is crossfading.
    chain_dry: [Vec<f32>; 2],
    strip_scratch: [Vec<f32>; 2],
    /// Per-strip pre-fader tap (after the pre-fader insert chain, before the
    /// fader/pan), used by pre-fader sends and sidechains.
    strip_pre_fader: Vec<[Vec<f32>; 2]>,
    /// Per-strip post-fader tap (after the post-fader insert chain), used by
    /// post-fader sends and sidechains.
    strip_post_fader: Vec<[Vec<f32>; 2]>,
    /// Previous block's strip taps, for delayed (cyclic) sidechain feeds.
    strip_pre_snapshot: Vec<[Vec<f32>; 2]>,
    strip_post_snapshot: Vec<[Vec<f32>; 2]>,
    send_scratch: [Vec<f32>; 2],
    /// Summed sidechain signal fed to a plugin this block.
    sidechain_mix: [Vec<f32>; 2],
    /// Silence fed to a plugin's sidechain input when nothing is routed.
    sidechain_silence: [Vec<f32>; 2],
    /// Order strips are processed in (sidechain contributors before consumers).
    strip_order: Vec<usize>,
    /// Per-slot cycle flags: delayed slots read last block's strip tap.
    strip_delayed: Vec<Vec<Vec<bool>>>,
    /// Whether any slot is delayed; the per-block tap snapshot is only needed
    /// then, which is rare (cycles).
    sidechain_delayed_any: bool,
    garbage_order: Producer<RetiredOrder>,
    garbage_scratch: Producer<ScratchBuffers>,
    bus_scratch: [Vec<f32>; 2],
    bus_accum: Vec<[Vec<f32>; 2]>,
    capacity: usize,
    midi_in: Port<MidiIn>,
    midi_out: Port<MidiOut>,
    /// Loss counters shared with the control thread (published once per block).
    midi_stats: Arc<MidiStats>,
    /// MIDI-out writes that failed this block's lifetime (cumulative).
    midi_out_errors: u64,
    midi: MidiBlock,
    /// Generated clock/transport events for the current block.
    master_midi: MidiBlock,
    master: MasterClock,
    master_active: bool,
    clock: Clock,
    shared_clock: Arc<SharedClock>,
    position: vibemix_lv2::TimePosition,
    midi_map: Arc<SharedMidi>,
    recorder: Recorder,
    takes: Producer<RecordedTake>,
    shared_recorder: Arc<SharedRecorder>,
    loop_slots: Vec<Option<LoopPlayback>>,
    loop_garbage: Producer<Box<[f32]>>,
    /// Informs the control thread that the period grew past the fixed
    /// capacity and scratch buffers must be resized (see `ResizeBuffers`).
    pending_period: Arc<AtomicU32>,
    buffer_size: usize,
    /// Strip input tap (after input gain, before the pre-fader insert chain).
    strip_input: Vec<[Vec<f32>; 2]>,
    /// Bus output tap (post plugins/fader; includes external returns).
    bus_out: Vec<[Vec<f32>; 2]>,
    /// Sum of the selected recorder sources for this block.
    record_mix: [Vec<f32>; 2],
    /// Transport-only MIDI events, kept after `midi_sync` filtering.
    transport_midi: MidiBlock,
}

impl MixerGraph {
    fn apply_commands(&mut self) {
        while let Ok(command) = self.commands.pop() {
            match command {
                EngineCommand::AddPlugin {
                    strip,
                    chain,
                    plugin,
                } => {
                    let target = self
                        .plugin_chains
                        .get_mut(strip)
                        .and_then(|chains| chains.get_mut(chain));
                    match target {
                        Some(chain) if chain.len() < MAX_PLUGINS => chain.push(plugin),
                        _ => {
                            if let Err(plugin) = self.garbage.push(plugin) {
                                std::mem::forget(plugin);
                            }
                        }
                    }
                }
                EngineCommand::RemovePlugin { strip, chain, slot } => {
                    if let Some(target) = self
                        .plugin_chains
                        .get_mut(strip)
                        .and_then(|chains| chains.get_mut(chain))
                    {
                        if slot < target.len() {
                            let plugin = target.remove(slot);
                            if let Err(plugin) = self.garbage.push(plugin) {
                                std::mem::forget(plugin);
                            }
                        }
                    }
                }
                EngineCommand::MovePlugin {
                    strip,
                    chain,
                    from,
                    to,
                } => {
                    if let Some(target) = self
                        .plugin_chains
                        .get_mut(strip)
                        .and_then(|chains| chains.get_mut(chain))
                    {
                        if from < target.len() && to < target.len() && from != to {
                            let plugin = target.remove(from);
                            target.insert(to, plugin);
                        }
                    }
                }
                EngineCommand::ClearStrip { strip } => {
                    if let Some(chains) = self.plugin_chains.get_mut(strip) {
                        for chain in chains.iter_mut() {
                            retire_plugins(chain, &mut self.garbage);
                        }
                    }
                }
                EngineCommand::AddBusPlugin { bus, plugin } => match self.bus_chains.get_mut(bus) {
                    Some(chain) if chain.len() < MAX_PLUGINS => chain.push(plugin),
                    _ => {
                        if let Err(plugin) = self.garbage.push(plugin) {
                            std::mem::forget(plugin);
                        }
                    }
                },
                EngineCommand::RemoveBusPlugin { bus, slot } => {
                    if let Some(chain) = self.bus_chains.get_mut(bus) {
                        if slot < chain.len() {
                            let plugin = chain.remove(slot);
                            if let Err(plugin) = self.garbage.push(plugin) {
                                std::mem::forget(plugin);
                            }
                        }
                    }
                }
                EngineCommand::MoveBusPlugin { bus, from, to } => {
                    if let Some(chain) = self.bus_chains.get_mut(bus) {
                        if from < chain.len() && to < chain.len() && from != to {
                            let plugin = chain.remove(from);
                            chain.insert(to, plugin);
                        }
                    }
                }
                EngineCommand::ClearBus { bus } => {
                    if let Some(chain) = self.bus_chains.get_mut(bus) {
                        retire_plugins(chain, &mut self.garbage);
                    }
                }
                EngineCommand::SetStripPorts { strip, in_l, in_r } => {
                    if let Some(slot) = self.strips.get_mut(strip) {
                        *slot = Some(StripPorts { in_l, in_r });
                    }
                }
                EngineCommand::ClearStripPorts { strip } => {
                    if let Some(slot) = self.strips.get_mut(strip) {
                        *slot = None;
                    }
                }
                EngineCommand::SetBusPorts { bus, out_l, out_r } => {
                    if let Some(slot) = self.buses.get_mut(bus) {
                        *slot = Some(BusPorts { out_l, out_r });
                    }
                }
                EngineCommand::ClearBusPorts { bus } => {
                    if let Some(slot) = self.buses.get_mut(bus) {
                        *slot = None;
                    }
                }
                EngineCommand::SetBusReturnPorts { bus, in_l, in_r } => {
                    if let Some(slot) = self.bus_returns.get_mut(bus) {
                        *slot = Some(BusReturnPorts { in_l, in_r });
                    }
                }
                EngineCommand::ClearBusReturnPorts { bus } => {
                    if let Some(slot) = self.bus_returns.get_mut(bus) {
                        *slot = None;
                    }
                }
                EngineCommand::Replace { chains, bus_chains } => {
                    let old = std::mem::replace(&mut self.plugin_chains, chains);
                    if let Err(chains) = self.garbage_nested.push(old) {
                        std::mem::forget(chains);
                    }
                    let old_bus = std::mem::replace(&mut self.bus_chains, bus_chains);
                    if let Err(old_bus) = self.garbage_bus_chains.push(old_bus) {
                        std::mem::forget(old_bus);
                    }
                }
                EngineCommand::SetSampleRate {
                    sample_rate,
                    delays,
                } => {
                    let old = std::mem::replace(&mut self.send_delays, delays);
                    if let Err(old) = self.garbage_delays.push(old) {
                        std::mem::forget(old);
                    }
                    let old_rate = self.sample_rate;
                    let ratio = if old_rate > 0.0 {
                        sample_rate / old_rate
                    } else {
                        1.0
                    };
                    for sends in &mut self.send_delay_smooth {
                        for value in sends.iter_mut() {
                            *value *= ratio;
                        }
                    }
                    self.sample_rate = sample_rate;
                }
                EngineCommand::ResizeBuffers { capacity, buffers } => {
                    // Applied at the top of the block, before any borrow of
                    // the scratch vectors; the old set is retired off-thread.
                    self.swap_scratch(*buffers);
                    self.capacity = capacity;
                }
                EngineCommand::SetStripOrder { order, delayed } => {
                    let old_order = std::mem::replace(&mut self.strip_order, order);
                    let old_delayed = std::mem::replace(&mut self.strip_delayed, delayed);
                    if let Err(old) = self.garbage_order.push(RetiredOrder {
                        _order: old_order,
                        _delayed: old_delayed,
                    }) {
                        std::mem::forget(old);
                    }
                    self.sidechain_delayed_any = self
                        .strip_delayed
                        .iter()
                        .any(|chains| chains.iter().any(|slots| slots.iter().any(|d| *d)));
                }
                EngineCommand::RecorderArm {
                    buffer,
                    capacity_frames,
                    expected_frames,
                    loop_pulses,
                    latency_frames,
                    loop_beats,
                } => {
                    self.recorder
                        .cancel(&mut self.loop_garbage, &self.shared_recorder);
                    let latency =
                        latency_frames + self.buffer_size as u32 + self.source_latency_frames();
                    self.recorder.arm(
                        buffer,
                        capacity_frames,
                        expected_frames,
                        loop_pulses,
                        latency,
                        loop_beats,
                        &self.shared_recorder,
                    );
                }
                EngineCommand::RecorderCancel => {
                    self.recorder
                        .cancel(&mut self.loop_garbage, &self.shared_recorder);
                }
                EngineCommand::SetLoop {
                    slot,
                    buffer,
                    frames,
                    offset,
                } => {
                    if slot < self.loop_slots.len() {
                        if let Some(old) = self.loop_slots[slot].take() {
                            self.retire_buffer(old.buffer);
                        }
                        let mut playback = LoopPlayback::new(buffer, frames);
                        playback.set_offset(offset);
                        self.loop_slots[slot] = Some(playback);
                    } else {
                        self.retire_buffer(buffer);
                    }
                }
                EngineCommand::LoopOffset { slot, offset } => {
                    if let Some(loop_slot) = self.loop_slots.get_mut(slot).and_then(Option::as_mut)
                    {
                        loop_slot.set_offset(offset);
                    }
                }
                EngineCommand::ClearLoop { slot } => {
                    if let Some(old) = self.loop_slots.get_mut(slot).and_then(Option::take) {
                        self.retire_buffer(old.buffer);
                    }
                }
                EngineCommand::LoopPlay { slot, quantized } => {
                    if let Some(loop_slot) = self.loop_slots.get_mut(slot).and_then(Option::as_mut)
                    {
                        loop_slot.quantized = quantized;
                        loop_slot.pending = true;
                        loop_slot.playing = false;
                        loop_slot.stopping = false;
                        loop_slot.pos = 0;
                    }
                }
                EngineCommand::LoopStop { slot } => {
                    if let Some(loop_slot) = self.loop_slots.get_mut(slot).and_then(Option::as_mut)
                    {
                        loop_slot.stop();
                    }
                }
                EngineCommand::LoopGain { slot, gain } => {
                    if let Some(loop_slot) = self.loop_slots.get_mut(slot).and_then(Option::as_mut)
                    {
                        loop_slot.gain = gain.clamp(0.0, 4.0);
                    }
                }
                EngineCommand::LoopMute { slot, mute } => {
                    if let Some(loop_slot) = self.loop_slots.get_mut(slot).and_then(Option::as_mut)
                    {
                        loop_slot.mute = mute;
                    }
                }
            }
        }
    }

    /// Swaps the freshly allocated scratch set into the graph, retiring the
    /// old one off-thread. Leak-bounded fallback like the other garbage rings.
    fn swap_scratch(&mut self, buffers: ScratchBuffers) {
        let old = ScratchBuffers {
            chain_dry: std::mem::replace(&mut self.chain_dry, buffers.chain_dry),
            strip_scratch: std::mem::replace(&mut self.strip_scratch, buffers.strip_scratch),
            strip_pre_fader: std::mem::replace(&mut self.strip_pre_fader, buffers.strip_pre_fader),
            strip_input: std::mem::replace(&mut self.strip_input, buffers.strip_input),
            strip_post_fader: std::mem::replace(
                &mut self.strip_post_fader,
                buffers.strip_post_fader,
            ),
            strip_pre_snapshot: std::mem::replace(
                &mut self.strip_pre_snapshot,
                buffers.strip_pre_snapshot,
            ),
            strip_post_snapshot: std::mem::replace(
                &mut self.strip_post_snapshot,
                buffers.strip_post_snapshot,
            ),
            send_scratch: std::mem::replace(&mut self.send_scratch, buffers.send_scratch),
            sidechain_mix: std::mem::replace(&mut self.sidechain_mix, buffers.sidechain_mix),
            sidechain_silence: std::mem::replace(
                &mut self.sidechain_silence,
                buffers.sidechain_silence,
            ),
            bus_scratch: std::mem::replace(&mut self.bus_scratch, buffers.bus_scratch),
            bus_accum: std::mem::replace(&mut self.bus_accum, buffers.bus_accum),
            bus_out: std::mem::replace(&mut self.bus_out, buffers.bus_out),
            record_mix: std::mem::replace(&mut self.record_mix, buffers.record_mix),
        };
        if let Err(old) = self.garbage_scratch.push(old) {
            // Ring sizes cover every queued command; overflow cannot happen,
            // but leaking is better than freeing on the audio thread.
            std::mem::forget(old);
        }
    }

    fn retire_buffer(&mut self, buffer: Box<[f32]>) {
        if let Err(buffer) = self.loop_garbage.push(buffer) {
            std::mem::forget(buffer);
        }
    }

    fn source_latency_frames(&self) -> u32 {
        let mut max = 0;
        for index in 0..self.shared_recorder.source_count() {
            let Some((kind, tap, id)) = self.shared_recorder.source(index) else {
                continue;
            };
            let id = id as usize;
            let latency = match kind {
                RecorderSourceKind::Strip => match tap {
                    RecorderTap::Input | RecorderTap::Output => 0,
                    RecorderTap::PreFader => self
                        .plugin_chains
                        .get(id)
                        .map(|chains| chain_latency(&chains[0]))
                        .unwrap_or(0),
                    RecorderTap::PostFader => self
                        .plugin_chains
                        .get(id)
                        .map(|chains| chain_latency(&chains[0]) + chain_latency(&chains[1]))
                        .unwrap_or(0),
                },
                RecorderSourceKind::Bus => {
                    if id != 0 && id < self.bus_chains.len() {
                        chain_latency(&self.bus_chains[id])
                    } else {
                        0
                    }
                }
            };
            max = max.max(latency);
        }
        max
    }

    /// Runs one insert chain (a strip's pre/post chain or a bus chain) over its
    /// scratch buffer. Mono plugins run as two instances, one per channel.
    fn run_chain(&mut self, target: ChainTarget, frames: usize) {
        let Self {
            plugin_chains,
            bus_chains,
            strip_scratch,
            bus_scratch,
            params,
            midi,
            position,
            strip_delayed,
            strip_pre_fader,
            strip_post_fader,
            strip_pre_snapshot,
            strip_post_snapshot,
            sidechain_mix,
            sidechain_silence,
            bypass_mix_strip,
            bypass_mix_bus,
            chain_dry,
            gain_coeff,
            ..
        } = self;
        let (plugins, scratch, wet_mix) = match target {
            ChainTarget::Strip { index, chain } => (
                &mut plugin_chains[index][chain],
                strip_scratch,
                &mut bypass_mix_strip[index][chain],
            ),
            ChainTarget::Bus(bus) => (&mut bus_chains[bus], bus_scratch, &mut bypass_mix_bus[bus]),
        };
        if plugins.is_empty() {
            return;
        }

        let frames = frames.min(PluginInstance::max_frames());
        let position = *position;
        let midi_sync = params.midi_sync();

        for (slot, plugin) in plugins.iter_mut().enumerate() {
            match plugin {
                SlotPlugin::Stereo(instance) => instance.deliver_responses(),
                SlotPlugin::Mono(left, right) => {
                    left.deliver_responses();
                    right.deliver_responses();
                }
                SlotPlugin::Missing => {}
            }

            // Bypass crossfades between dry and processed over a few ms instead
            // of switching hard; a fully bypassed plugin is not run at all.
            let wet_target = if target.bypassed(params, slot) {
                0.0
            } else {
                1.0
            };
            let (wet_start, wet_end) =
                dsp::smooth_step(&mut wet_mix[slot], wet_target, *gain_coeff);
            if wet_start == 0.0 && wet_end == 0.0 {
                continue;
            }
            let fading = wet_start != 1.0 || wet_end != 1.0;
            if fading {
                for channel in 0..2 {
                    chain_dry[channel][..frames].copy_from_slice(&scratch[channel][..frames]);
                }
            }
            let delayed = match target {
                ChainTarget::Strip { index, chain } => strip_delayed[index][chain][slot],
                ChainTarget::Bus(_) => false,
            };

            // Runs one plugin instance over `channels` of the scratch buffer
            // (instance port `n` carries scratch channel `channels[n]`).
            let mut run_node = |instance: &mut dyn PluginNode, channels: &[usize]| {
                target.set_controls(instance, params, slot);
                for (port, &channel) in channels.iter().enumerate() {
                    instance.input_buffer_mut(port)[..frames]
                        .copy_from_slice(&scratch[channel][..frames]);
                }
                instance.write_midi(midi.as_slice());
                if midi_sync {
                    instance.write_position(&position);
                }
                instance.write_ui_events(midi_sync);
                let (is_bus, index, chain) = target.sidechain_key();
                feed_sidechain(
                    instance,
                    is_bus,
                    index,
                    chain,
                    slot,
                    delayed,
                    params,
                    strip_pre_fader,
                    strip_post_fader,
                    strip_pre_snapshot,
                    strip_post_snapshot,
                    sidechain_mix,
                    sidechain_silence,
                    frames,
                );
                instance.run(frames);
                for (port, &channel) in channels.iter().enumerate() {
                    scratch[channel][..frames]
                        .copy_from_slice(&instance.output_buffer(port)[..frames]);
                }
            };

            match plugin {
                SlotPlugin::Stereo(instance) => run_node(&mut **instance, &[0, 1]),
                SlotPlugin::Mono(left, right) => {
                    run_node(&mut **left, &[0]);
                    run_node(&mut **right, &[1]);
                }
                SlotPlugin::Missing => {}
            }

            if fading {
                let step = (wet_end - wet_start) / frames.max(1) as f32;
                for channel in 0..2 {
                    dsp::sanitize(&mut scratch[channel][..frames]);
                    for (index, (out, dry)) in scratch[channel][..frames]
                        .iter_mut()
                        .zip(&chain_dry[channel][..frames])
                        .enumerate()
                    {
                        let wet = wet_start + step * index as f32;
                        *out = dry + (*out - dry) * wet;
                    }
                }
            }
        }
        dsp::sanitize(&mut scratch[0][..frames]);
        dsp::sanitize(&mut scratch[1][..frames]);
    }

    /// While plugin state is captured the chain cannot run. Passing the signal
    /// through dry would bypass limiters and compressors at full level, so a
    /// chain that is doing work is silenced for the (short) capture instead.
    fn silence_active_chain(&mut self, index: usize, chain: usize, frames: usize) {
        if chain_is_active(&self.plugin_chains[index][chain], |slot| {
            self.params.plugin_bypass(index, chain, slot)
        }) {
            self.strip_scratch[0][..frames].fill(0.0);
            self.strip_scratch[1][..frames].fill(0.0);
        }
    }

    fn silence_active_bus_chain(&mut self, bus: usize, frames: usize) {
        if chain_is_active(&self.bus_chains[bus], |slot| {
            self.params.bus_plugin_bypass(bus, slot)
        }) {
            self.bus_scratch[0][..frames].fill(0.0);
            self.bus_scratch[1][..frames].fill(0.0);
        }
    }

    fn process_strips(&mut self, scope: &ProcessScope, frames: usize, paused: bool) {
        let strip_count = self.strips.len();
        let bus_count = self.buses.len();

        let any_solo = (0..strip_count)
            .any(|index| self.params.strip_active(index) && self.params.strip_solo(index));

        for position in 0..strip_count {
            let index = self.strip_order.get(position).copied().unwrap_or(position);
            if !self.params.strip_active(index) {
                self.meters.store_strip_in(index, 0.0, 0.0);
                self.meters.store_strip_gain(index, 0.0, 0.0);
                self.meters.store_strip_out(index, 0.0, 0.0);
                for tap in [
                    &mut self.strip_input[index],
                    &mut self.strip_pre_fader[index],
                    &mut self.strip_post_fader[index],
                ] {
                    tap[0][..frames].fill(0.0);
                    tap[1][..frames].fill(0.0);
                }
                continue;
            }

            {
                let Some(ports) = self.strips[index].as_ref() else {
                    self.meters.store_strip_in(index, 0.0, 0.0);
                    self.meters.store_strip_gain(index, 0.0, 0.0);
                    self.meters.store_strip_out(index, 0.0, 0.0);
                    for tap in [
                        &mut self.strip_input[index],
                        &mut self.strip_pre_fader[index],
                        &mut self.strip_post_fader[index],
                    ] {
                        tap[0][..frames].fill(0.0);
                        tap[1][..frames].fill(0.0);
                    }
                    continue;
                };
                let in_l = ports.in_l.as_slice(scope);
                let in_r = ports.in_r.as_slice(scope);
                self.strip_scratch[0][..frames].copy_from_slice(&in_l[..frames]);
                self.strip_scratch[1][..frames].copy_from_slice(&in_r[..frames]);
            }

            let in_peak_l = dsp::peak(&self.strip_scratch[0][..frames]);
            let in_peak_r = dsp::peak(&self.strip_scratch[1][..frames]);
            self.meters.store_strip_in(index, in_peak_l, in_peak_r);

            let input_target = self.params.strip_input_gain(index);
            let (input_start, input_gain) = dsp::smooth_step(
                &mut self.strip_input_smooth[index],
                input_target,
                self.gain_coeff,
            );
            dsp::apply_gain_ramp(
                &mut self.strip_scratch[0][..frames],
                input_start,
                input_gain,
            );
            dsp::apply_gain_ramp(
                &mut self.strip_scratch[1][..frames],
                input_start,
                input_gain,
            );
            self.meters
                .store_strip_gain(index, in_peak_l * input_gain, in_peak_r * input_gain);

            // Input tap (after input gain, before the pre-fader insert chain).
            self.strip_input[index][0][..frames].copy_from_slice(&self.strip_scratch[0][..frames]);
            self.strip_input[index][1][..frames].copy_from_slice(&self.strip_scratch[1][..frames]);

            if paused {
                self.silence_active_chain(index, 0, frames);
            } else {
                self.run_chain(ChainTarget::Strip { index, chain: 0 }, frames);
            }

            // Pre-fader tap: after the pre-fader insert chain, before fader/pan.
            self.strip_pre_fader[index][0][..frames]
                .copy_from_slice(&self.strip_scratch[0][..frames]);
            self.strip_pre_fader[index][1][..frames]
                .copy_from_slice(&self.strip_scratch[1][..frames]);

            let muted =
                self.params.strip_mute(index) || (any_solo && !self.params.strip_solo(index));
            let target = if muted {
                0.0
            } else {
                self.params.strip_gain(index)
            };
            let (gain_start, gain_end) =
                dsp::smooth_step(&mut self.strip_gain_smooth[index], target, self.gain_coeff);

            let pan_target = self.params.strip_pan(index);
            let (pan_start, pan_end) = dsp::smooth_step(
                &mut self.strip_pan_smooth[index],
                pan_target,
                self.gain_coeff,
            );
            let (pan_l0, pan_r0) = dsp::pan_gains(pan_start);
            let (pan_l1, pan_r1) = dsp::pan_gains(pan_end);

            dsp::apply_gain_ramp(
                &mut self.strip_scratch[0][..frames],
                gain_start * pan_l0,
                gain_end * pan_l1,
            );
            dsp::apply_gain_ramp(
                &mut self.strip_scratch[1][..frames],
                gain_start * pan_r0,
                gain_end * pan_r1,
            );

            if paused {
                self.silence_active_chain(index, 1, frames);
            } else {
                self.run_chain(ChainTarget::Strip { index, chain: 1 }, frames);
            }

            // Post-fader tap (the same point a post-fader send uses).
            self.strip_post_fader[index][0][..frames]
                .copy_from_slice(&self.strip_scratch[0][..frames]);
            self.strip_post_fader[index][1][..frames]
                .copy_from_slice(&self.strip_scratch[1][..frames]);

            let strip_peak_l = dsp::peak(&self.strip_scratch[0][..frames]);
            let strip_peak_r = dsp::peak(&self.strip_scratch[1][..frames]);
            self.meters
                .store_strip_out(index, strip_peak_l, strip_peak_r);

            for bus in 0..bus_count {
                let gain_target = self.params.send_gain(index, bus);
                let (send_start, send_end) = dsp::smooth_step(
                    &mut self.send_gain_smooth[index][bus],
                    gain_target,
                    self.gain_coeff,
                );
                if send_start <= 1.0e-6 && send_end <= 1.0e-6 {
                    self.send_delays[index][bus].invalidate();
                    continue;
                }

                let delay_target = self.params.send_delay(index, bus) * self.sample_rate / 1000.0;
                let (delay_start, delay_end) = dsp::smooth_step(
                    &mut self.send_delay_smooth[index][bus],
                    delay_target,
                    self.delay_coeff,
                );

                let (src_l, src_r) = if self.params.send_pre_fader(index, bus) {
                    (
                        &self.strip_pre_fader[index][0],
                        &self.strip_pre_fader[index][1],
                    )
                } else {
                    (&self.strip_scratch[0], &self.strip_scratch[1])
                };

                let [send_l, send_r] = &mut self.send_scratch;
                self.send_delays[index][bus].process_ramp(
                    &src_l[..frames],
                    &src_r[..frames],
                    &mut send_l[..frames],
                    &mut send_r[..frames],
                    delay_start,
                    delay_end,
                );
                dsp::add_scaled_ramp(
                    &mut self.bus_accum[bus][0][..frames],
                    &send_l[..frames],
                    send_start,
                    send_end,
                );
                dsp::add_scaled_ramp(
                    &mut self.bus_accum[bus][1][..frames],
                    &send_r[..frames],
                    send_start,
                    send_end,
                );
            }
        }

        // Snapshot the strip taps for delayed (cyclic) sidechain feeds next
        // block; skip the copy entirely unless a cycle exists (rare).
        if self.sidechain_delayed_any {
            for strip in 0..strip_count.min(self.strip_pre_snapshot.len()) {
                let len = frames.min(self.strip_pre_snapshot[strip][0].len());
                self.strip_pre_snapshot[strip][0][..len]
                    .copy_from_slice(&self.strip_pre_fader[strip][0][..len]);
                self.strip_pre_snapshot[strip][1][..len]
                    .copy_from_slice(&self.strip_pre_fader[strip][1][..len]);
                self.strip_post_snapshot[strip][0][..len]
                    .copy_from_slice(&self.strip_post_fader[strip][0][..len]);
                self.strip_post_snapshot[strip][1][..len]
                    .copy_from_slice(&self.strip_post_fader[strip][1][..len]);
            }
        }
    }

    fn process_buses(&mut self, scope: &ProcessScope, frames: usize) {
        // Non-master buses first so their output can be routed into master.
        for bus in 1..self.buses.len() {
            self.process_one_bus(bus, scope, frames);
            if self.params.bus_active(bus) && self.params.bus_mix_master(bus) {
                dsp::add_scaled(
                    &mut self.bus_accum[0][0][..frames],
                    &self.bus_scratch[0][..frames],
                    1.0,
                );
                dsp::add_scaled(
                    &mut self.bus_accum[0][1][..frames],
                    &self.bus_scratch[1][..frames],
                    1.0,
                );
            }
        }
        // Master (bus 0) last, after all sends and routes are summed in.
        self.process_one_bus(0, scope, frames);
    }

    /// Mix launcher playback into the master input. Quantized slots start on
    /// the next bar boundary in this block; without a running clock they start
    /// immediately.
    fn mix_loops(&mut self, frames: usize, beats_before: f64, beats_per_bar: f32, playing: bool) {
        let bar = bar_boundary_offset(self.transport_midi.as_slice(), beats_before, beats_per_bar);
        let gain_coeff = self.gain_coeff;
        let (slots, [master_l, master_r]) = (&mut self.loop_slots, &mut self.bus_accum[0]);
        for slot in slots.iter_mut() {
            let Some(loop_slot) = slot.as_mut() else {
                continue;
            };
            let start = if loop_slot.pending {
                if loop_slot.quantized && playing {
                    let Some(offset) = bar else {
                        continue;
                    };
                    loop_slot.start();
                    offset
                } else {
                    loop_slot.start();
                    0
                }
            } else if loop_slot.playing {
                0
            } else {
                continue;
            };
            loop_slot.mix(
                &mut master_l[..frames],
                &mut master_r[..frames],
                start,
                frames,
                gain_coeff,
            );
        }
    }

    /// Sum the selected recorder sources for this block.
    fn build_record_mix(&mut self, frames: usize) {
        self.record_mix[0][..frames].fill(0.0);
        self.record_mix[1][..frames].fill(0.0);

        for index in 0..self.shared_recorder.source_count() {
            let Some((kind, tap, id)) = self.shared_recorder.source(index) else {
                continue;
            };
            let id = id as usize;
            let source = match kind {
                RecorderSourceKind::Strip => match tap {
                    RecorderTap::Input => self.strip_input.get(id).map(|tap| (&tap[0], &tap[1])),
                    RecorderTap::PreFader => {
                        self.strip_pre_fader.get(id).map(|tap| (&tap[0], &tap[1]))
                    }
                    RecorderTap::PostFader => {
                        self.strip_post_fader.get(id).map(|tap| (&tap[0], &tap[1]))
                    }
                    RecorderTap::Output => None,
                },
                RecorderSourceKind::Bus => {
                    if tap == RecorderTap::Output && id != 0 {
                        self.bus_out.get(id).map(|tap| (&tap[0], &tap[1]))
                    } else {
                        None
                    }
                }
            };
            let Some((left, right)) = source else {
                continue;
            };
            for i in 0..frames {
                self.record_mix[0][i] += left[i];
                self.record_mix[1][i] += right[i];
            }
        }
    }

    fn process_one_bus(&mut self, bus: usize, scope: &ProcessScope, frames: usize) {
        if !self.params.bus_active(bus) {
            self.meters.store_bus_in(bus, 0.0, 0.0);
            self.meters.store_bus_gain(bus, 0.0, 0.0);
            self.meters.store_bus_ret(bus, 0.0, 0.0);
            self.meters.store_bus(bus, 0.0, 0.0);
            self.bus_out[bus][0][..frames].fill(0.0);
            self.bus_out[bus][1][..frames].fill(0.0);
            return;
        }

        self.bus_scratch[0][..frames].copy_from_slice(&self.bus_accum[bus][0][..frames]);
        self.bus_scratch[1][..frames].copy_from_slice(&self.bus_accum[bus][1][..frames]);

        let in_peak_l = dsp::peak(&self.bus_scratch[0][..frames]);
        let in_peak_r = dsp::peak(&self.bus_scratch[1][..frames]);
        self.meters.store_bus_in(bus, in_peak_l, in_peak_r);

        if self.paused {
            self.silence_active_bus_chain(bus, frames);
        } else {
            self.run_chain(ChainTarget::Bus(bus), frames);
        }

        // A returning bus is an external insert: the post-insert signal leaves
        // pre-fader for the device (scaled by the SEND trim), and the device's
        // output is mixed back in (dry + wet) before the fader. The master has
        // no separate send, so this only applies to non-master buses.
        let returning = bus != 0 && self.params.bus_return(bus);
        let (send_start, send_gain) = if returning {
            let target = self.params.bus_send_gain(bus);
            dsp::smooth_step(&mut self.bus_send_smooth[bus], target, self.gain_coeff)
        } else {
            (1.0, 1.0)
        };

        let gain_peak_l = dsp::peak(&self.bus_scratch[0][..frames]);
        let gain_peak_r = dsp::peak(&self.bus_scratch[1][..frames]);
        self.meters
            .store_bus_gain(bus, gain_peak_l * send_gain, gain_peak_r * send_gain);

        if returning {
            if let Some(ports) = self.buses[bus].as_mut() {
                let out_l = ports.out_l.as_mut_slice(scope);
                let out_r = ports.out_r.as_mut_slice(scope);
                dsp::scale_into_ramp(out_l, &self.bus_scratch[0][..frames], send_start, send_gain);
                dsp::scale_into_ramp(out_r, &self.bus_scratch[1][..frames], send_start, send_gain);
            }

            let ret_target = self.params.bus_return_gain(bus);
            let (return_start, return_gain) = dsp::smooth_step(
                &mut self.bus_return_smooth[bus],
                ret_target,
                self.gain_coeff,
            );

            if let Some(ports) = self.bus_returns[bus].as_ref() {
                let in_l = ports.in_l.as_slice(scope);
                let in_r = ports.in_r.as_slice(scope);
                let ret_peak_l = dsp::peak(&in_l[..frames]);
                let ret_peak_r = dsp::peak(&in_r[..frames]);
                self.meters
                    .store_bus_ret(bus, ret_peak_l * return_gain, ret_peak_r * return_gain);
                dsp::add_scaled_ramp(
                    &mut self.bus_scratch[0][..frames],
                    &in_l[..frames],
                    return_start,
                    return_gain,
                );
                dsp::add_scaled_ramp(
                    &mut self.bus_scratch[1][..frames],
                    &in_r[..frames],
                    return_start,
                    return_gain,
                );
            } else {
                self.meters.store_bus_ret(bus, 0.0, 0.0);
            }
        } else {
            self.meters.store_bus_ret(bus, 0.0, 0.0);
        }

        let target = if self.params.bus_mute(bus) {
            0.0
        } else {
            self.params.bus_gain(bus)
        };
        let (gain_start, gain_end) =
            dsp::smooth_step(&mut self.bus_gain_smooth[bus], target, self.gain_coeff);
        dsp::apply_gain_ramp(&mut self.bus_scratch[0][..frames], gain_start, gain_end);
        dsp::apply_gain_ramp(&mut self.bus_scratch[1][..frames], gain_start, gain_end);

        let bus_peak_l = dsp::peak(&self.bus_scratch[0][..frames]);
        let bus_peak_r = dsp::peak(&self.bus_scratch[1][..frames]);
        self.meters.store_bus(bus, bus_peak_l, bus_peak_r);

        if !returning {
            if let Some(ports) = self.buses[bus].as_mut() {
                let out_l = ports.out_l.as_mut_slice(scope);
                let out_r = ports.out_r.as_mut_slice(scope);
                dsp::scale_into(out_l, &self.bus_scratch[0][..frames], 1.0);
                dsp::scale_into(out_r, &self.bus_scratch[1][..frames], 1.0);
            }
        }

        // Bus output tap for the loop recorder (post plugins and fader; for a
        // return bus this is the wet return, not the pre-fader send).
        self.bus_out[bus][0][..frames].copy_from_slice(&self.bus_scratch[0][..frames]);
        self.bus_out[bus][1][..frames].copy_from_slice(&self.bus_scratch[1][..frames]);
    }
}

impl ProcessHandler for MixerGraph {
    fn process(&mut self, _client: &Client, scope: &ProcessScope) -> Control {
        let _rt_guard = crate::rt_guard::AudioScope::enter();
        let started = std::time::Instant::now();
        dsp::flush_denormals();
        self.apply_commands();

        let paused = self.params.state_pause();
        self.paused = paused;
        self.params.set_state_ack(paused);

        let raw_frames = scope.n_frames() as usize;
        let frames = raw_frames.min(self.capacity);
        self.gain_coeff = dsp::smoothing_coeff(frames, self.sample_rate, dsp::GAIN_SMOOTH_MS);
        self.delay_coeff = dsp::smoothing_coeff(frames, self.sample_rate, dsp::DELAY_SMOOTH_MS);

        // MIDI: collect the block's events and advance clock/transport state.
        // `raw.time` is the frame offset within the block.
        self.midi.clear();
        self.master_midi.clear();
        self.transport_midi.clear();

        let master_mode = self.params.clock_master();
        let beats_per_bar = self.params.beats_per_bar();
        let beats_before;
        let playing;

        if master_mode {
            // The internal clock owns the transport; incoming clock/transport
            // is ignored so it cannot double-clock the graph. Transport can
            // still remote-control the internal start/stop.
            let receive_transport = self.params.clock_receive_transport();
            for raw in self.midi_in.iter(scope) {
                // Messages longer than the fixed 4-byte payload (SysEx and
                // friends) cannot be forwarded losslessly; skip them instead
                // of truncating them into garbage bytes.
                if raw.bytes.len() > 4 {
                    self.midi_stats.oversize.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let event = MidiEvent::new(raw.time as i64, raw.bytes);
                let status = event.as_slice().first().copied().unwrap_or(0);
                if is_transport_status(status) {
                    if receive_transport {
                        match status {
                            START => self.params.set_clock_playing(true),
                            CONTINUE => self.params.set_clock_playing(true),
                            STOP => self.params.set_clock_playing(false),
                            _ => {}
                        }
                    }
                    continue;
                }
                if is_sync_status(status) {
                    continue;
                }
                self.midi.push(event);
            }
            if !self.master_active {
                self.master.reset();
                self.master_active = true;
            }
            let (exact_beats, pulse_beats, frames_at_start) = self.master.process(
                frames,
                self.sample_rate as u32,
                self.params.clock_playing(),
                self.params.clock_bpm_milli(),
                &mut self.master_midi,
            );
            // Recorder/loop bar detection needs pulse-aligned beats; plugin
            // time uses the exact position including the phase.
            beats_before = pulse_beats;
            playing = self.master.is_playing();
            for index in 0..self.master_midi.len() {
                let event = self.master_midi.get(index);
                self.transport_midi.push(event);
                self.midi.push(event);
            }
            self.midi.sort_by_time();
            self.position =
                self.master
                    .position(exact_beats, frames_at_start, beats_per_bar, playing);
            self.shared_clock.publish_state(
                true,
                playing,
                self.master.bpm(),
                exact_beats,
                frames_at_start,
                0.0,
                0,
            );

            // Clock/transport goes to the MIDI output in master mode only.
            // `send_transport` can suppress start/stop while keeping clock.
            let send_transport = self.params.clock_send_transport();
            let mut writer = self.midi_out.writer(scope);
            for index in 0..self.master_midi.len() {
                let event = self.master_midi.get(index);
                let status = event.as_slice().first().copied().unwrap_or(0);
                if !send_transport && is_transport_status(status) {
                    continue;
                }
                if writer
                    .write(&RawMidi {
                        time: event.time.max(0) as Frames,
                        bytes: event.as_slice(),
                    })
                    .is_err()
                {
                    self.midi_out_errors += 1;
                }
            }
        } else {
            if self.master_active {
                self.master.reset();
                self.master_active = false;
                self.clock = Clock::new(self.sample_rate, beats_per_bar);
            }
            let receive_transport = self.params.clock_receive_transport();
            let send_transport = self.params.clock_send_transport();
            for raw in self.midi_in.iter(scope) {
                // Messages longer than the fixed 4-byte payload (SysEx and
                // friends) cannot be forwarded losslessly; skip them instead
                // of truncating them into garbage bytes.
                if raw.bytes.len() > 4 {
                    self.midi_stats.oversize.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let event = MidiEvent::new(raw.time as i64, raw.bytes);
                let status = event.as_slice().first().copied().unwrap_or(0);
                if is_transport_status(status) {
                    // Keep a raw copy for optional forwarding.
                    if send_transport {
                        self.master_midi.push(event);
                    }
                    if receive_transport {
                        self.midi.push(event);
                        self.transport_midi.push(event);
                    }
                } else {
                    self.midi.push(event);
                    if status >= 0xF8 {
                        self.transport_midi.push(event);
                    }
                }
            }

            // Clock/transport always tracks the input so the GUI can show position.
            beats_before = self.clock.beats;
            self.clock.set_beats_per_bar(beats_per_bar);
            self.clock.set_sample_rate(self.sample_rate);
            self.clock.process_block(self.midi.as_slice(), frames);
            playing = self.clock.playing;
            self.position = self.clock.position();
            self.shared_clock.publish(&self.clock);

            // Optional MIDI-transport thru (no clock) in slave mode.
            if send_transport {
                let mut writer = self.midi_out.writer(scope);
                for index in 0..self.master_midi.len() {
                    let event = self.master_midi.get(index);
                    if writer
                        .write(&RawMidi {
                            time: event.time.max(0) as Frames,
                            bytes: event.as_slice(),
                        })
                        .is_err()
                    {
                        self.midi_out_errors += 1;
                    }
                }
            }
        }

        // When MIDI sync is off, drop system realtime/transport messages so
        // plugins don't receive clock (notes/CC are still forwarded).
        if !self.params.midi_sync() {
            let mut kept = 0usize;
            for index in 0..self.midi.len() {
                let event = self.midi.get(index);
                let status = event.as_slice().first().copied().unwrap_or(0);
                let transport = is_sync_status(status);
                if !transport {
                    self.midi.set(kept, event);
                    kept += 1;
                }
            }
            self.midi.truncate(kept);
        }

        // Apply control-surface CCs; consumed ones are not forwarded to plugins.
        {
            let midi_map = &self.midi_map;
            let params = &self.params;
            let mut forwarded = 0usize;
            for index in 0..self.midi.len() {
                let event = self.midi.get(index);
                let bytes = event.as_slice();
                let is_cc = bytes.len() >= 3 && (bytes[0] & 0xF0) == 0xB0;
                let consumed = is_cc
                    && midi_map.handle_cc(
                        (bytes[0] & 0x0F) as u32,
                        bytes[1] as u32,
                        bytes[2] as u32,
                        params,
                    );
                if !consumed {
                    self.midi.set(forwarded, event);
                    forwarded += 1;
                }
            }
            self.midi.truncate(forwarded);
        }

        for accumulator in &mut self.bus_accum {
            accumulator[0][..frames].fill(0.0);
            accumulator[1][..frames].fill(0.0);
        }

        self.process_strips(scope, frames, paused);
        self.mix_loops(frames, beats_before, beats_per_bar, playing);
        self.process_buses(scope, frames);
        self.build_record_mix(frames);
        self.recorder.process(
            frames,
            self.transport_midi.as_slice(),
            beats_before,
            beats_per_bar,
            &self.record_mix[0],
            &self.record_mix[1],
            self.sample_rate as u32,
            &self.shared_recorder,
            &mut self.takes,
            &mut self.loop_garbage,
        );

        if raw_frames > frames {
            // Period beyond the fixed buffer capacity: silence the tail of the
            // bus outputs rather than emit stale samples.
            for slot in &mut self.buses {
                let Some(ports) = slot.as_mut() else {
                    continue;
                };
                let out_l = ports.out_l.as_mut_slice(scope);
                let out_r = ports.out_r.as_mut_slice(scope);
                out_l[frames..raw_frames].fill(0.0);
                out_r[frames..raw_frames].fill(0.0);
            }
        }

        // Callback load: time used over time available. The average smooths
        // over ~1 s; the peak holds and decays so brief spikes stay visible.
        if self.sample_rate > 0.0 && raw_frames > 0 {
            let budget = raw_frames as f32 / self.sample_rate;
            let load = started.elapsed().as_secs_f32() / budget;
            self.dsp_avg += (load - self.dsp_avg) * budget.min(1.0);
            // Linear decay per real second, so the peak's persistence does not
            // depend on the host period.
            self.dsp_peak = (self.dsp_peak - PEAK_DECAY_PER_SECOND * budget).max(load);
            self.meters.store_dsp(self.dsp_avg, self.dsp_peak);
        }

        // MIDI loss counters are published once per block; the audio thread
        // cannot log, so the control thread reports them instead.
        let dropped =
            self.midi.dropped() + self.master_midi.dropped() + self.transport_midi.dropped();
        self.midi_stats.dropped.store(dropped, Ordering::Relaxed);
        self.midi_stats
            .out_errors
            .store(self.midi_out_errors, Ordering::Relaxed);

        Control::Continue
    }

    fn buffer_size(&mut self, _client: &Client, size: Frames) -> Control {
        let size = size as usize;
        self.buffer_size = size;
        if size > self.capacity {
            // The control thread will allocate larger scratch buffers and
            // swap them in; until then frames beyond the capacity are
            // zero-filled rather than replaying stale samples.
            self.pending_period.store(size as u32, Ordering::SeqCst);
            tracing::warn!(
                size,
                capacity = self.capacity,
                "host period exceeds buffer capacity; resizing scratch buffers"
            );
        } else {
            tracing::debug!(size, "host period changed");
        }
        Control::Continue
    }
}

pub struct JackEngine {
    active: jack::AsyncClient<Notifications, MixerGraph>,
    commands: Producer<EngineCommand>,
    garbage: Consumer<SlotPlugin>,
    garbage_bus_chains: Consumer<Vec<Vec<SlotPlugin>>>,
    garbage_scratch: Consumer<ScratchBuffers>,
    garbage_nested: Consumer<Vec<StripChains>>,
    garbage_delays: Consumer<Vec<Vec<dsp::DelayLine>>>,
    garbage_order: Consumer<RetiredOrder>,
    take_rx: Consumer<RecordedTake>,
    loop_garbage_rx: Consumer<Box<[f32]>>,
    shared_recorder: Arc<SharedRecorder>,
    midi_stats: Arc<MidiStats>,
    pending_period: Arc<AtomicU32>,
    pending_rate: Arc<AtomicU32>,
    xruns: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    clock: Arc<SharedClock>,
    midi_map: Arc<SharedMidi>,
    midi_port: Port<Unowned>,
    midi_out_port: Port<Unowned>,
    /// The sample rate the graph (delay lines, plugins) was actually built
    /// with. JACK/PipeWire may report a different rate at client creation than
    /// the graph settles on, so track this separately from the live rate.
    graph_sample_rate: u32,
    strip_meta: Vec<StripMeta>,
    bus_meta: Vec<Vec<SlotMeta>>,
    strip_ports: Vec<Option<(Port<Unowned>, Port<Unowned>)>>,
    strip_count: usize,
    bus_ports: Vec<Option<(Port<Unowned>, Port<Unowned>)>>,
    bus_count: usize,
    bus_return_ports: Vec<Option<(Port<Unowned>, Port<Unowned>)>>,
    client_name: String,
    auto_connect: bool,
    pending_missing: Vec<MissingConnection>,
}

impl JackEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        client_name: &str,
        params: Arc<SharedParams>,
        meters: Arc<SharedMeters>,
        auto_connect: bool,
        lv2: &Host,
        vst3: &vibemix_vst3::Host,
        clap: &vibemix_clap::Host,
        model: &mut MixerModel,
    ) -> Result<Self> {
        let (client, _status) = Client::new(client_name, ClientOptions::NO_START_SERVER)?;

        let strip_count = params.strip_count();
        let bus_count = params.bus_count();
        let sample_rate = client.sample_rate() as f64;
        let buffer_size = client.buffer_size() as usize;
        // Buffer capacity is fixed for the client's lifetime so the audio
        // callback never allocates; periods larger than this are truncated.
        let capacity = buffer_size.max(PluginInstance::max_frames());
        let max_delay_samples =
            (vibemix_core::model::MAX_SEND_DELAY_MS as f64 / 1000.0 * sample_rate).ceil() as usize
                + 1;

        let built = build_chains(
            lv2,
            vst3,
            clap,
            model,
            strip_count,
            bus_count,
            sample_rate,
            buffer_size,
            &params,
        );
        let Built {
            strip_chains: plugin_chains,
            strip_meta,
            bus_chains,
            bus_meta,
        } = built;

        let mut strips: Vec<Option<StripPorts>> = (0..strip_count).map(|_| None).collect();
        let mut strip_ports: Vec<Option<(Port<Unowned>, Port<Unowned>)>> =
            (0..strip_count).map(|_| None).collect();
        for index in 0..strip_count {
            if !params.strip_active(index) {
                continue;
            }
            let (in_l, in_r) = register_strip_ports(&client, index)?;
            strip_ports[index] = Some((in_l.clone_unowned(), in_r.clone_unowned()));
            strips[index] = Some(StripPorts { in_l, in_r });
        }

        let mut buses: Vec<Option<BusPorts>> = (0..bus_count).map(|_| None).collect();
        let mut bus_ports: Vec<Option<(Port<Unowned>, Port<Unowned>)>> =
            (0..bus_count).map(|_| None).collect();
        for index in 0..bus_count {
            if !params.bus_active(index) {
                continue;
            }
            let (out_l, out_r) = register_bus_ports(&client, index)?;
            bus_ports[index] = Some((out_l.clone_unowned(), out_r.clone_unowned()));
            buses[index] = Some(BusPorts { out_l, out_r });
        }

        let mut bus_returns: Vec<Option<BusReturnPorts>> = (0..bus_count).map(|_| None).collect();
        let mut bus_return_ports: Vec<Option<(Port<Unowned>, Port<Unowned>)>> =
            (0..bus_count).map(|_| None).collect();
        for index in 0..bus_count {
            if !params.bus_active(index) || !params.bus_return(index) {
                continue;
            }
            let (in_l, in_r) = register_bus_return_ports(&client, index)?;
            bus_return_ports[index] = Some((in_l.clone_unowned(), in_r.clone_unowned()));
            bus_returns[index] = Some(BusReturnPorts { in_l, in_r });
        }

        let midi_in: Port<MidiIn> = client
            .register_port("midi_in", MidiIn::default())
            .context("register midi input port")?;
        let midi_port = midi_in.clone_unowned();
        let midi_out: Port<MidiOut> = client
            .register_port("midi_out", MidiOut::default())
            .context("register midi output port")?;
        let midi_out_port = midi_out.clone_unowned();
        let shared_clock = Arc::new(SharedClock::new());
        let midi_map = Arc::new(SharedMidi::new());

        let (commands, command_consumer) = rtrb::RingBuffer::<EngineCommand>::new(COMMAND_CAPACITY);
        // Garbage queues are sized so a full command queue's worth of retired
        // objects can never overflow them (the control thread drains each loop).
        let garbage_plugins_capacity = COMMAND_CAPACITY * (strip_count + bus_count + 1);
        let garbage_nested_capacity = COMMAND_CAPACITY;
        let (garbage_producer, garbage) =
            rtrb::RingBuffer::<SlotPlugin>::new(garbage_plugins_capacity);
        let (garbage_bus_chains_producer, garbage_bus_chains) =
            rtrb::RingBuffer::<Vec<Vec<SlotPlugin>>>::new(4);
        let (garbage_nested_producer, garbage_nested) =
            rtrb::RingBuffer::<Vec<StripChains>>::new(garbage_nested_capacity);
        let (garbage_delays_producer, garbage_delays) =
            rtrb::RingBuffer::<Vec<Vec<dsp::DelayLine>>>::new(4);
        let (garbage_order_producer, garbage_order) = rtrb::RingBuffer::<RetiredOrder>::new(4);
        // Resizes are rare (a period growth) and synchronous per command, so
        // two pending slots cover any queue state.
        let (garbage_scratch_producer, garbage_scratch) =
            rtrb::RingBuffer::<ScratchBuffers>::new(2);
        // Depth covers back-to-back takes even while the control thread runs
        // the (expensive) transient analysis of a previous one; taking ownership
        // of the buffer means depth only costs pre-allocated memory.
        let (takes, take_rx) = rtrb::RingBuffer::<RecordedTake>::new(TAKE_RING_DEPTH);
        let (loop_garbage, loop_garbage_rx) = rtrb::RingBuffer::<Box<[f32]>>::new(COMMAND_CAPACITY);
        let shared_recorder = Arc::new(SharedRecorder::new());
        let midi_stats = Arc::new(MidiStats::default());
        let pending_period = Arc::new(AtomicU32::new(0));
        let scratch = ScratchBuffers::new(capacity, strip_count, bus_count);

        let graph = MixerGraph {
            params: Arc::clone(&params),
            meters,
            strips,
            buses,
            bus_returns,
            bus_send_smooth: vec![1.0; bus_count],
            bus_return_smooth: vec![1.0; bus_count],
            plugin_chains,
            bus_chains,
            commands: command_consumer,
            garbage: garbage_producer,
            garbage_bus_chains: garbage_bus_chains_producer,
            garbage_nested: garbage_nested_producer,
            garbage_delays: garbage_delays_producer,
            strip_gain_smooth: vec![0.0; strip_count],
            strip_pan_smooth: vec![0.0; strip_count],
            strip_input_smooth: vec![1.0; strip_count],
            bus_gain_smooth: vec![0.0; bus_count],
            send_gain_smooth: (0..strip_count).map(|_| vec![0.0; bus_count]).collect(),
            send_delay_smooth: (0..strip_count).map(|_| vec![0.0; bus_count]).collect(),
            send_delays: (0..strip_count)
                .map(|_| {
                    (0..bus_count)
                        .map(|_| dsp::DelayLine::new(max_delay_samples))
                        .collect()
                })
                .collect(),
            sample_rate: sample_rate as f32,
            gain_coeff: 1.0,
            paused: false,
            dsp_avg: 0.0,
            dsp_peak: 0.0,
            bypass_mix_strip: vec![[[1.0; MAX_PLUGINS]; PLUGIN_CHAINS]; strip_count],
            bypass_mix_bus: vec![[1.0; MAX_PLUGINS]; bus_count],
            delay_coeff: 1.0,
            // Period-sized scratch buffers, built in one place so a later
            // resize produces the identical layout.
            chain_dry: scratch.chain_dry,
            strip_scratch: scratch.strip_scratch,
            strip_pre_fader: scratch.strip_pre_fader,
            strip_input: scratch.strip_input,
            strip_post_fader: scratch.strip_post_fader,
            strip_pre_snapshot: scratch.strip_pre_snapshot,
            strip_post_snapshot: scratch.strip_post_snapshot,
            send_scratch: scratch.send_scratch,
            sidechain_mix: scratch.sidechain_mix,
            sidechain_silence: scratch.sidechain_silence,
            bus_scratch: scratch.bus_scratch,
            bus_accum: scratch.bus_accum,
            bus_out: scratch.bus_out,
            record_mix: scratch.record_mix,
            strip_order: (0..strip_count).collect(),
            strip_delayed: (0..strip_count)
                .map(|_| {
                    (0..PLUGIN_CHAINS)
                        .map(|_| (0..MAX_PLUGINS).map(|_| false).collect())
                        .collect()
                })
                .collect(),
            sidechain_delayed_any: false,
            garbage_order: garbage_order_producer,
            garbage_scratch: garbage_scratch_producer,
            capacity,
            buffer_size,
            midi_in,
            midi_out,
            midi_stats: Arc::clone(&midi_stats),
            midi_out_errors: 0,
            midi: MidiBlock::default(),
            master_midi: MidiBlock::default(),
            master: MasterClock::new(),
            master_active: false,
            transport_midi: MidiBlock::default(),
            clock: Clock::new(sample_rate as f32, 4.0),
            shared_clock: Arc::clone(&shared_clock),
            position: vibemix_lv2::TimePosition::default(),
            midi_map: Arc::clone(&midi_map),
            recorder: Recorder::new(),
            takes,
            shared_recorder: Arc::clone(&shared_recorder),
            loop_slots: (0..MAX_LOOPS).map(|_| None).collect(),
            loop_garbage,
            pending_period: Arc::clone(&pending_period),
        };

        let pending_rate = Arc::new(AtomicU32::new(0));
        let xruns = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let active = client
            .activate_async(
                Notifications {
                    pending_rate: Arc::clone(&pending_rate),
                    xruns: Arc::clone(&xruns),
                    shutdown: Arc::clone(&shutdown),
                },
                graph,
            )
            .context("activate jack client")?;

        if auto_connect {
            auto_connect_ports(active.as_client(), client_name, strip_count, bus_count);
        }

        let mut engine = Self {
            active,
            commands,
            garbage,
            garbage_bus_chains,
            garbage_scratch,
            garbage_nested,
            garbage_delays,
            garbage_order,
            take_rx,
            loop_garbage_rx,
            shared_recorder: Arc::clone(&shared_recorder),
            midi_stats,
            pending_period,
            pending_rate,
            xruns,
            shutdown,
            clock: shared_clock,
            midi_map,
            midi_port,
            midi_out_port,
            graph_sample_rate: sample_rate as u32,
            strip_meta,
            bus_meta,
            strip_ports,
            strip_count,
            bus_ports,
            bus_count,
            bus_return_ports,
            client_name: client_name.to_string(),
            auto_connect,
            pending_missing: Vec::new(),
        };
        engine.sync_port_names(model);
        engine.pending_missing = engine.restore_connections(model);
        Ok(engine)
    }

    pub fn sample_rate(&self) -> u32 {
        self.active.as_client().sample_rate()
    }

    pub fn buffer_size(&self) -> u32 {
        self.active.as_client().buffer_size()
    }

    pub fn recorder(&self) -> &Arc<SharedRecorder> {
        &self.shared_recorder
    }

    /// Current MIDI loss counters from the audio thread.
    pub fn midi_stats(&self) -> MidiStatsSnapshot {
        MidiStatsSnapshot {
            dropped: self.midi_stats.dropped.load(Ordering::Relaxed),
            oversize: self.midi_stats.oversize.load(Ordering::Relaxed),
            out_errors: self.midi_stats.out_errors.load(Ordering::Relaxed),
        }
    }

    pub fn take_recording(&mut self) -> Option<RecordedTake> {
        self.take_rx.pop().ok()
    }

    pub fn arm_recorder(
        &mut self,
        buffer: Box<[f32]>,
        capacity_frames: usize,
        expected_frames: usize,
        loop_pulses: u32,
        loop_beats: f32,
    ) -> Result<()> {
        self.commands
            .push(EngineCommand::RecorderArm {
                buffer,
                capacity_frames,
                expected_frames,
                loop_pulses,
                latency_frames: 0,
                loop_beats,
            })
            .map_err(|_| anyhow!("engine command queue full"))
    }

    pub fn cancel_recorder(&mut self) -> Result<()> {
        self.commands
            .push(EngineCommand::RecorderCancel)
            .map_err(|_| anyhow!("engine command queue full"))
    }

    pub fn set_loop(
        &mut self,
        slot: usize,
        buffer: Box<[f32]>,
        frames: usize,
        offset: i64,
    ) -> Result<()> {
        self.commands
            .push(EngineCommand::SetLoop {
                slot,
                buffer,
                frames,
                offset,
            })
            .map_err(|_| anyhow!("engine command queue full"))
    }

    /// Change a loop's playback rotation without replacing its buffer.
    pub fn loop_offset(&mut self, slot: usize, offset: i64) -> Result<()> {
        self.commands
            .push(EngineCommand::LoopOffset { slot, offset })
            .map_err(|_| anyhow!("engine command queue full"))
    }

    pub fn clear_loop(&mut self, slot: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::ClearLoop { slot })
            .map_err(|_| anyhow!("engine command queue full"))
    }

    pub fn loop_play(&mut self, slot: usize, quantized: bool) -> Result<()> {
        self.commands
            .push(EngineCommand::LoopPlay { slot, quantized })
            .map_err(|_| anyhow!("engine command queue full"))
    }

    pub fn loop_stop(&mut self, slot: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::LoopStop { slot })
            .map_err(|_| anyhow!("engine command queue full"))
    }

    pub fn loop_gain(&mut self, slot: usize, gain: f32) -> Result<()> {
        self.commands
            .push(EngineCommand::LoopGain { slot, gain })
            .map_err(|_| anyhow!("engine command queue full"))
    }

    pub fn loop_mute(&mut self, slot: usize, mute: bool) -> Result<()> {
        self.commands
            .push(EngineCommand::LoopMute { slot, mute })
            .map_err(|_| anyhow!("engine command queue full"))
    }

    /// Returns and clears a pending sample-rate change reported by JACK.
    pub fn take_pending_sample_rate(&self) -> Option<u32> {
        let rate = self.pending_rate.swap(0, Ordering::SeqCst);
        (rate != 0).then_some(rate)
    }

    /// Returns and clears a period-growth request from the JACK notification.
    /// The value is the new period, which the tied scratch capacity must grow
    /// to cover.
    pub fn take_pending_buffer_size(&self) -> Option<usize> {
        let size = self.pending_period.swap(0, Ordering::SeqCst);
        (size != 0).then_some(size as usize)
    }

    /// Replaces the graph's period-sized scratch buffers with a set built for
    /// `capacity`. Applied on the audio thread at the top of the next block:
    /// the swap is pointer moves, the allocation happened here.
    pub fn resize_buffers(&mut self, capacity: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::ResizeBuffers {
                capacity,
                buffers: Box::new(ScratchBuffers::new(
                    capacity,
                    self.strip_count,
                    self.bus_count,
                )),
            })
            .map_err(|_| anyhow!("engine command queue full"))
    }

    /// Whether the JACK/PipeWire server has shut this client down.
    pub fn server_shut_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub fn xrun_count(&self) -> u64 {
        self.xruns.load(Ordering::Relaxed)
    }

    pub fn clock(&self) -> &Arc<SharedClock> {
        &self.clock
    }

    pub fn midi_map(&self) -> &Arc<SharedMidi> {
        &self.midi_map
    }

    /// Swaps in delay lines sized for a new sample rate and updates the graph's
    /// rate used for delay conversion.
    pub fn set_sample_rate(
        &mut self,
        sample_rate: f32,
        delays: Vec<Vec<dsp::DelayLine>>,
    ) -> Result<()> {
        self.commands
            .push(EngineCommand::SetSampleRate {
                sample_rate,
                delays,
            })
            .map_err(|_| anyhow!("engine command queue full"))?;
        self.graph_sample_rate = sample_rate as u32;
        Ok(())
    }

    /// The sample rate the graph was built with (may lag the live JACK rate
    /// until a rate change is processed).
    pub fn graph_sample_rate(&self) -> u32 {
        self.graph_sample_rate
    }

    /// Installs a sidechain-aware strip processing order (and cycle flags).
    pub fn set_strip_order(
        &mut self,
        order: Vec<usize>,
        delayed: Vec<Vec<Vec<bool>>>,
    ) -> Result<()> {
        self.commands
            .push(EngineCommand::SetStripOrder { order, delayed })
            .map_err(|_| anyhow!("engine command queue full"))?;
        Ok(())
    }

    pub fn add_plugin(
        &mut self,
        strip: usize,
        chain: usize,
        plugin: SlotPlugin,
        proxy: Option<SlotState>,
        handle: usize,
    ) -> Result<()> {
        let meta = SlotMeta::of(&plugin, proxy, Some(handle));
        self.commands
            .push(EngineCommand::AddPlugin {
                strip,
                chain,
                plugin,
            })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.strip_meta_mut(strip, chain) {
            if slots.len() < MAX_PLUGINS {
                slots.push(meta);
            }
        }
        Ok(())
    }

    pub fn remove_plugin(&mut self, strip: usize, chain: usize, slot: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::RemovePlugin { strip, chain, slot })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.strip_meta_mut(strip, chain) {
            if slot < slots.len() {
                slots.remove(slot);
            }
        }
        Ok(())
    }

    pub fn move_plugin(
        &mut self,
        strip: usize,
        chain: usize,
        from: usize,
        to: usize,
    ) -> Result<()> {
        self.commands
            .push(EngineCommand::MovePlugin {
                strip,
                chain,
                from,
                to,
            })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.strip_meta_mut(strip, chain) {
            if from < slots.len() && to < slots.len() {
                let meta = slots.remove(from);
                slots.insert(to, meta);
            }
        }
        Ok(())
    }

    pub fn clear_strip(&mut self, strip: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::ClearStrip { strip })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(chains) = self.strip_meta.get_mut(strip) {
            for slots in chains.iter_mut() {
                slots.clear();
            }
        }
        Ok(())
    }

    pub fn add_bus_plugin(
        &mut self,
        bus: usize,
        plugin: SlotPlugin,
        proxy: Option<SlotState>,
        handle: usize,
    ) -> Result<()> {
        let meta = SlotMeta::of(&plugin, proxy, Some(handle));
        self.commands
            .push(EngineCommand::AddBusPlugin { bus, plugin })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.bus_meta.get_mut(bus) {
            if slots.len() < MAX_PLUGINS {
                slots.push(meta);
            }
        }
        Ok(())
    }

    pub fn remove_bus_plugin(&mut self, bus: usize, slot: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::RemoveBusPlugin { bus, slot })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.bus_meta.get_mut(bus) {
            if slot < slots.len() {
                slots.remove(slot);
            }
        }
        Ok(())
    }

    pub fn move_bus_plugin(&mut self, bus: usize, from: usize, to: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::MoveBusPlugin { bus, from, to })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.bus_meta.get_mut(bus) {
            if from < slots.len() && to < slots.len() {
                let meta = slots.remove(from);
                slots.insert(to, meta);
            }
        }
        Ok(())
    }

    pub fn clear_bus_plugins(&mut self, bus: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::ClearBus { bus })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.bus_meta.get_mut(bus) {
            slots.clear();
        }
        Ok(())
    }

    fn strip_meta_mut(&mut self, strip: usize, chain: usize) -> Option<&mut Vec<SlotMeta>> {
        self.strip_meta
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
    }

    fn slot_meta(&self, strip: usize, chain: usize, slot: usize) -> Option<&SlotMeta> {
        self.strip_meta
            .get(strip)
            .and_then(|chains| chains.get(chain))
            .and_then(|slots| slots.get(slot))
    }

    fn bus_slot_meta(&self, bus: usize, slot: usize) -> Option<&SlotMeta> {
        self.bus_meta.get(bus).and_then(|slots| slots.get(slot))
    }

    pub fn bus_state_proxy(&self, bus: usize, slot: usize) -> Option<SlotState> {
        self.bus_slot_meta(bus, slot)?.proxy.clone()
    }

    pub fn bus_plugin_handle(&self, bus: usize, slot: usize) -> Option<*mut std::ffi::c_void> {
        self.bus_slot_meta(bus, slot)?
            .handle
            .map(|handle| handle as *mut std::ffi::c_void)
    }

    pub fn bus_plugin_outputs(&self, bus: usize, slot: usize) -> Option<Arc<SharedOutputs>> {
        self.bus_slot_meta(bus, slot)?.outputs.clone()
    }

    pub fn bus_plugin_ui_inputs(&self, bus: usize, slot: usize) -> Option<Arc<SharedUiInputs>> {
        self.bus_slot_meta(bus, slot)?.ui_inputs.clone()
    }

    pub fn add_strip_ports(&mut self, strip: usize) -> Result<()> {
        if strip >= self.strip_count {
            return Ok(());
        }
        if self
            .strip_ports
            .get(strip)
            .is_some_and(|ports| ports.is_some())
        {
            return Ok(());
        }

        let (in_l, in_r) = register_strip_ports(self.active.as_client(), strip)?;
        let handles = (in_l.clone_unowned(), in_r.clone_unowned());
        if self
            .commands
            .push(EngineCommand::SetStripPorts { strip, in_l, in_r })
            .is_err()
        {
            let client = self.active.as_client();
            let _ = client.unregister_port(handles.0);
            let _ = client.unregister_port(handles.1);
            return Err(anyhow!("engine command queue full"));
        }
        self.strip_ports[strip] = Some(handles);

        if self.auto_connect && strip < 4 {
            let client = self.active.as_client();
            let _ = client.connect_ports_by_name(
                &format!("system:capture_{}", strip * 2 + 1),
                &format!("{}:strip{strip}_in_l", self.client_name),
            );
            let _ = client.connect_ports_by_name(
                &format!("system:capture_{}", strip * 2 + 2),
                &format!("{}:strip{strip}_in_r", self.client_name),
            );
        }
        Ok(())
    }

    pub fn remove_strip_ports(&mut self, strip: usize) -> Result<()> {
        let Some(slot) = self.strip_ports.get_mut(strip) else {
            return Ok(());
        };
        let Some((in_l, in_r)) = slot.take() else {
            return Ok(());
        };

        if self
            .commands
            .push(EngineCommand::ClearStripPorts { strip })
            .is_err()
        {
            *slot = Some((in_l, in_r));
            return Err(anyhow!("engine command queue full"));
        }

        let client = self.active.as_client();
        let _ = client.unregister_port(in_l);
        let _ = client.unregister_port(in_r);
        Ok(())
    }

    pub fn sync_strip_ports(&mut self, params: &SharedParams) -> Result<()> {
        for index in 0..self.strip_count {
            if params.strip_active(index) {
                self.add_strip_ports(index)?;
            } else {
                self.remove_strip_ports(index)?;
            }
        }
        Ok(())
    }

    pub fn add_bus_ports(&mut self, bus: usize) -> Result<()> {
        if bus >= self.bus_count {
            return Ok(());
        }
        if self.bus_ports.get(bus).is_some_and(|ports| ports.is_some()) {
            return Ok(());
        }

        let (out_l, out_r) = register_bus_ports(self.active.as_client(), bus)?;
        let handles = (out_l.clone_unowned(), out_r.clone_unowned());
        if self
            .commands
            .push(EngineCommand::SetBusPorts { bus, out_l, out_r })
            .is_err()
        {
            let client = self.active.as_client();
            let _ = client.unregister_port(handles.0);
            let _ = client.unregister_port(handles.1);
            return Err(anyhow!("engine command queue full"));
        }
        self.bus_ports[bus] = Some(handles);

        if self.auto_connect && bus == 0 {
            let client = self.active.as_client();
            let _ = client.connect_ports_by_name(
                &format!("{}:bus0_out_l", self.client_name),
                "system:playback_1",
            );
            let _ = client.connect_ports_by_name(
                &format!("{}:bus0_out_r", self.client_name),
                "system:playback_2",
            );
        }
        Ok(())
    }

    pub fn remove_bus_ports(&mut self, bus: usize) -> Result<()> {
        let Some(slot) = self.bus_ports.get_mut(bus) else {
            return Ok(());
        };
        let Some((out_l, out_r)) = slot.take() else {
            return Ok(());
        };

        if self
            .commands
            .push(EngineCommand::ClearBusPorts { bus })
            .is_err()
        {
            *slot = Some((out_l, out_r));
            return Err(anyhow!("engine command queue full"));
        }

        let client = self.active.as_client();
        let _ = client.unregister_port(out_l);
        let _ = client.unregister_port(out_r);
        Ok(())
    }

    pub fn sync_bus_ports(&mut self, params: &SharedParams) -> Result<()> {
        for index in 0..self.bus_count {
            if params.bus_active(index) {
                self.add_bus_ports(index)?;
            } else {
                self.remove_bus_ports(index)?;
            }
        }
        Ok(())
    }

    pub fn add_bus_return_ports(&mut self, bus: usize) -> Result<()> {
        if bus >= self.bus_count {
            return Ok(());
        }
        if self
            .bus_return_ports
            .get(bus)
            .is_some_and(|ports| ports.is_some())
        {
            return Ok(());
        }

        let (in_l, in_r) = register_bus_return_ports(self.active.as_client(), bus)?;
        let handles = (in_l.clone_unowned(), in_r.clone_unowned());
        if self
            .commands
            .push(EngineCommand::SetBusReturnPorts { bus, in_l, in_r })
            .is_err()
        {
            let client = self.active.as_client();
            let _ = client.unregister_port(handles.0);
            let _ = client.unregister_port(handles.1);
            return Err(anyhow!("engine command queue full"));
        }
        self.bus_return_ports[bus] = Some(handles);
        Ok(())
    }

    pub fn remove_bus_return_ports(&mut self, bus: usize) -> Result<()> {
        let Some(slot) = self.bus_return_ports.get_mut(bus) else {
            return Ok(());
        };
        let Some((in_l, in_r)) = slot.take() else {
            return Ok(());
        };

        if self
            .commands
            .push(EngineCommand::ClearBusReturnPorts { bus })
            .is_err()
        {
            *slot = Some((in_l, in_r));
            return Err(anyhow!("engine command queue full"));
        }

        let client = self.active.as_client();
        let _ = client.unregister_port(in_l);
        let _ = client.unregister_port(in_r);
        Ok(())
    }

    pub fn sync_bus_return_ports(&mut self, params: &SharedParams) -> Result<()> {
        for index in 0..self.bus_count {
            if index != 0 && params.bus_active(index) && params.bus_return(index) {
                self.add_bus_return_ports(index)?;
            } else {
                self.remove_bus_return_ports(index)?;
            }
        }
        Ok(())
    }

    pub fn sync_port_names(&self, model: &MixerModel) {
        let strip_bases = strip_port_bases(model);
        let bus_bases = bus_port_bases(model);

        let mut desired: Vec<(Port<Unowned>, String)> = Vec::new();

        for strip in &model.strips {
            let index = strip.id.0 as usize;
            let Some(base) = strip_bases.get(&index) else {
                continue;
            };
            let Some((in_l, in_r)) = self.strip_ports.get(index).and_then(Option::as_ref) else {
                continue;
            };
            desired.push((in_l.clone(), format!("{base}_in_l")));
            desired.push((in_r.clone(), format!("{base}_in_r")));
        }

        for bus in &model.buses {
            let index = bus.id.0 as usize;
            let Some(base) = bus_bases.get(&index) else {
                continue;
            };
            let Some((out_l, out_r)) = self.bus_ports.get(index).and_then(Option::as_ref) else {
                continue;
            };
            desired.push((out_l.clone(), format!("{base}_out_l")));
            desired.push((out_r.clone(), format!("{base}_out_r")));
        }

        for bus in &model.buses {
            let index = bus.id.0 as usize;
            let Some(base) = bus_bases.get(&index) else {
                continue;
            };
            let Some((ret_l, ret_r)) = self.bus_return_ports.get(index).and_then(Option::as_ref)
            else {
                continue;
            };
            desired.push((ret_l.clone(), format!("{base}_ret_l")));
            desired.push((ret_r.clone(), format!("{base}_ret_r")));
        }

        let mut changed: Vec<(Port<Unowned>, String)> = desired
            .into_iter()
            .filter(|(port, target)| {
                port.short_name()
                    .map(|current| &current != target)
                    .unwrap_or(true)
            })
            .collect();

        if changed.is_empty() {
            return;
        }

        let client = self.active.as_client().raw();
        for (position, (port, _)) in changed.iter_mut().enumerate() {
            let temp = format!("__vibemix_tmp_{position}");
            rename_port(client, port, &temp);
        }
        for (port, target) in &mut changed {
            rename_port(client, port, target);
        }
    }

    /// Records the external JACK ports currently feeding each strip input and
    /// each bus output into the model, so a session save can restore them.
    pub fn capture_connections(&self, model: &mut MixerModel) {
        let exists = |name: &str| self.active.as_client().port_by_name(name).is_some();
        let saved_in = model.midi_connections.take();
        model.midi_connections = Some(merge_missing_connections(
            self.midi_port.get_connections(),
            saved_in.as_deref(),
            &exists,
        ));
        let saved_out = model.midi_out_connections.take();
        model.midi_out_connections = Some(merge_missing_connections(
            self.midi_out_port.get_connections(),
            saved_out.as_deref(),
            &exists,
        ));

        for strip in &mut model.strips {
            let index = strip.id.0 as usize;
            let Some((in_l, in_r)) = self.strip_ports.get(index).and_then(Option::as_ref) else {
                continue;
            };
            strip.connections = Some(StripConnections {
                in_l: in_l.get_connections(),
                in_r: in_r.get_connections(),
            });
        }

        for bus in &mut model.buses {
            let index = bus.id.0 as usize;
            let Some((out_l, out_r)) = self.bus_ports.get(index).and_then(Option::as_ref) else {
                continue;
            };
            bus.connections = Some(BusConnections {
                out_l: out_l.get_connections(),
                out_r: out_r.get_connections(),
                ..Default::default()
            });
        }

        for bus in &mut model.buses {
            let index = bus.id.0 as usize;
            let Some((ret_l, ret_r)) = self.bus_return_ports.get(index).and_then(Option::as_ref)
            else {
                continue;
            };
            let connections = bus.connections.get_or_insert_with(BusConnections::default);
            connections.ret_l = ret_l.get_connections();
            connections.ret_r = ret_r.get_connections();
        }
    }

    /// Reconnects the saved external ports for every strip/bus that has patch
    /// information (`Some`). Each such port is first disconnected, making the
    /// saved patch authoritative. Connections whose other end is not present
    /// are returned so the caller can report them.
    pub fn restore_connections(&self, model: &MixerModel) -> Vec<MissingConnection> {
        let client = self.active.as_client();
        let mut missing = Vec::new();

        if let Some(sources) = &model.midi_connections {
            restore_input(
                client,
                &self.midi_port,
                "midi_in",
                "midi in",
                sources,
                &mut missing,
            );
        }
        if let Some(sinks) = &model.midi_out_connections {
            restore_output(
                client,
                &self.midi_out_port,
                "midi_out",
                "midi out",
                sinks,
                &mut missing,
            );
        }

        for strip in &model.strips {
            let Some(connections) = &strip.connections else {
                continue;
            };
            let index = strip.id.0 as usize;
            let Some((in_l, in_r)) = self.strip_ports.get(index).and_then(Option::as_ref) else {
                continue;
            };
            restore_input(
                client,
                in_l,
                "in_l",
                &strip.name,
                &connections.in_l,
                &mut missing,
            );
            restore_input(
                client,
                in_r,
                "in_r",
                &strip.name,
                &connections.in_r,
                &mut missing,
            );
        }

        for bus in &model.buses {
            let Some(connections) = &bus.connections else {
                continue;
            };
            let index = bus.id.0 as usize;
            let Some((out_l, out_r)) = self.bus_ports.get(index).and_then(Option::as_ref) else {
                continue;
            };
            restore_output(
                client,
                out_l,
                "out_l",
                &bus.name,
                &connections.out_l,
                &mut missing,
            );
            restore_output(
                client,
                out_r,
                "out_r",
                &bus.name,
                &connections.out_r,
                &mut missing,
            );
        }

        for bus in &model.buses {
            let Some(connections) = &bus.connections else {
                continue;
            };
            let index = bus.id.0 as usize;
            let Some((ret_l, ret_r)) = self.bus_return_ports.get(index).and_then(Option::as_ref)
            else {
                continue;
            };
            restore_input(
                client,
                ret_l,
                "ret_l",
                &bus.name,
                &connections.ret_l,
                &mut missing,
            );
            restore_input(
                client,
                ret_r,
                "ret_r",
                &bus.name,
                &connections.ret_r,
                &mut missing,
            );
        }

        if !missing.is_empty() {
            tracing::warn!(
                count = missing.len(),
                "some saved connections could not be restored"
            );
        }
        missing
    }

    pub fn take_missing_connections(&mut self) -> Vec<MissingConnection> {
        std::mem::take(&mut self.pending_missing)
    }

    /// Every JACK port with its flags, current connections and (for vibemix's
    /// own ports) its role. Used by the GUI patchbay. Not RT safe: allocates
    /// and talks to the server, so call it from the control thread.
    pub fn ports(&self, model: &MixerModel) -> Vec<PortInfo> {
        let roles = {
            let mut roles: std::collections::HashMap<String, PortRole> =
                std::collections::HashMap::new();
            let mut add = |port: &Port<Unowned>, role: PortRole| {
                if let Ok(name) = port.name() {
                    roles.insert(name, role);
                }
            };

            for strip in &model.strips {
                let index = strip.id.0 as usize;
                if let Some((in_l, in_r)) = self.strip_ports.get(index).and_then(Option::as_ref) {
                    add(
                        in_l,
                        PortRole {
                            kind: "strip_in",
                            id: strip.id.0,
                            channel: "l",
                            label: strip.name.clone(),
                        },
                    );
                    add(
                        in_r,
                        PortRole {
                            kind: "strip_in",
                            id: strip.id.0,
                            channel: "r",
                            label: strip.name.clone(),
                        },
                    );
                }
            }

            for bus in &model.buses {
                let index = bus.id.0 as usize;
                if let Some((out_l, out_r)) = self.bus_ports.get(index).and_then(Option::as_ref) {
                    add(
                        out_l,
                        PortRole {
                            kind: "bus_out",
                            id: bus.id.0,
                            channel: "l",
                            label: bus.name.clone(),
                        },
                    );
                    add(
                        out_r,
                        PortRole {
                            kind: "bus_out",
                            id: bus.id.0,
                            channel: "r",
                            label: bus.name.clone(),
                        },
                    );
                }
                if let Some((ret_l, ret_r)) =
                    self.bus_return_ports.get(index).and_then(Option::as_ref)
                {
                    add(
                        ret_l,
                        PortRole {
                            kind: "bus_ret",
                            id: bus.id.0,
                            channel: "l",
                            label: bus.name.clone(),
                        },
                    );
                    add(
                        ret_r,
                        PortRole {
                            kind: "bus_ret",
                            id: bus.id.0,
                            channel: "r",
                            label: bus.name.clone(),
                        },
                    );
                }
            }

            add(
                &self.midi_port,
                PortRole {
                    kind: "midi_in",
                    id: 0,
                    channel: "",
                    label: "MIDI".to_string(),
                },
            );
            add(
                &self.midi_out_port,
                PortRole {
                    kind: "midi_out",
                    id: 0,
                    channel: "",
                    label: "MIDI".to_string(),
                },
            );
            roles
        };

        let client = self.active.as_client();
        let mut ports = Vec::new();
        for name in client.ports(None, None, jack::PortFlags::empty()) {
            let Some(port) = client.port_by_name(&name) else {
                continue;
            };
            let flags = port.flags();
            let kind = match port.port_type() {
                Ok(kind) if kind.contains("midi") => "midi",
                _ => "audio",
            };
            let (client_name, short) = name
                .split_once(':')
                .unwrap_or((name.as_str(), name.as_str()));
            ports.push(PortInfo {
                name: name.clone(),
                client: client_name.to_string(),
                short: short.to_string(),
                r#type: kind.to_string(),
                input: flags.contains(jack::PortFlags::IS_INPUT),
                output: flags.contains(jack::PortFlags::IS_OUTPUT),
                physical: flags.contains(jack::PortFlags::IS_PHYSICAL),
                connections: port.get_connections(),
                role: roles.get(&name).cloned(),
            });
        }
        ports
    }

    /// Connects a JACK source (output) to a sink (input).
    pub fn connect_port(&self, source: &str, sink: &str) -> Result<()> {
        self.active
            .as_client()
            .connect_ports_by_name(source, sink)
            .with_context(|| format!("connecting {source} -> {sink}"))?;
        Ok(())
    }

    /// Disconnects a JACK source (output) from a sink (input).
    pub fn disconnect_port(&self, source: &str, sink: &str) -> Result<()> {
        self.active
            .as_client()
            .disconnect_ports_by_name(source, sink)
            .with_context(|| format!("disconnecting {source} -> {sink}"))?;
        Ok(())
    }

    pub fn state_proxy(&self, strip: usize, chain: usize, slot: usize) -> Option<SlotState> {
        self.slot_meta(strip, chain, slot)?.proxy.clone()
    }

    pub fn plugin_handle(
        &self,
        strip: usize,
        chain: usize,
        slot: usize,
    ) -> Option<*mut std::ffi::c_void> {
        self.slot_meta(strip, chain, slot)?
            .handle
            .map(|handle| handle as *mut std::ffi::c_void)
    }

    pub fn plugin_outputs(
        &self,
        strip: usize,
        chain: usize,
        slot: usize,
    ) -> Option<Arc<SharedOutputs>> {
        self.slot_meta(strip, chain, slot)?.outputs.clone()
    }

    pub fn plugin_ui_inputs(
        &self,
        strip: usize,
        chain: usize,
        slot: usize,
    ) -> Option<Arc<SharedUiInputs>> {
        self.slot_meta(strip, chain, slot)?.ui_inputs.clone()
    }

    pub fn rebuild_plugins(
        &mut self,
        lv2: &Host,
        vst3: &vibemix_vst3::Host,
        clap: &vibemix_clap::Host,
        model: &mut MixerModel,
        params: &SharedParams,
        sample_rate: f64,
    ) -> Result<()> {
        let buffer_size = self.buffer_size() as usize;
        let built = build_chains(
            lv2,
            vst3,
            clap,
            model,
            self.strip_count,
            self.bus_count,
            sample_rate,
            buffer_size,
            params,
        );
        self.commands
            .push(EngineCommand::Replace {
                chains: built.strip_chains,
                bus_chains: built.bus_chains,
            })
            .map_err(|_| anyhow!("engine command queue full"))?;
        self.strip_meta = built.strip_meta;
        self.bus_meta = built.bus_meta;
        Ok(())
    }

    pub fn drain_garbage(&mut self) {
        while self.garbage.pop().is_ok() {}
        while self.garbage_bus_chains.pop().is_ok() {}
        while self.garbage_scratch.pop().is_ok() {}
        while self.garbage_nested.pop().is_ok() {}
        while self.garbage_delays.pop().is_ok() {}
        while self.garbage_order.pop().is_ok() {}
        while self.loop_garbage_rx.pop().is_ok() {}
    }
}

/// Moves every plugin out of `chain` into the garbage queue while keeping the
/// chain's allocation, so a later `push` on the audio thread cannot reallocate.
fn retire_plugins<T>(chain: &mut Vec<T>, garbage: &mut Producer<T>) {
    while let Some(plugin) = chain.pop() {
        if let Err(rtrb::PushError::Full(plugin)) = garbage.push(plugin) {
            std::mem::forget(plugin);
        }
    }
}

/// Whether any plugin in `chain` would currently process audio.
fn chain_is_active(chain: &[SlotPlugin], bypassed: impl Fn(usize) -> bool) -> bool {
    chain
        .iter()
        .enumerate()
        .any(|(slot, plugin)| !matches!(plugin, SlotPlugin::Missing) && !bypassed(slot))
}

fn chain_latency(chain: &[SlotPlugin]) -> u32 {
    chain.iter().map(|plugin| plugin.latency()).sum()
}

/// Which insert chain a plugin runs in.
#[derive(Clone, Copy)]
enum ChainTarget {
    /// A strip's pre-fader (`chain` 0) or post-fader (`chain` 1) inserts.
    Strip {
        index: usize,
        chain: usize,
    },
    Bus(usize),
}

impl ChainTarget {
    fn bypassed(self, params: &SharedParams, slot: usize) -> bool {
        match self {
            Self::Strip { index, chain } => params.plugin_bypass(index, chain, slot),
            Self::Bus(bus) => params.bus_plugin_bypass(bus, slot),
        }
    }

    /// Copies the shared control values into the plugin's control buffers.
    fn set_controls(self, instance: &mut dyn PluginNode, params: &SharedParams, slot: usize) {
        let controls = instance.control_count().min(MAX_PARAMS);
        for param in 0..controls {
            let value = match self {
                Self::Strip { index, chain } => params.plugin_param(index, chain, slot, param),
                Self::Bus(bus) => params.bus_plugin_param(bus, slot, param),
            };
            instance.set_control(param, value);
        }
    }

    /// `(is_bus, index, chain)` as `feed_sidechain` addresses a slot.
    fn sidechain_key(self) -> (bool, usize, usize) {
        match self {
            Self::Strip { index, chain } => (false, index, chain),
            Self::Bus(bus) => (true, bus, 0),
        }
    }
}

/// Sums a plugin's per-strip sidechain sends into `mix` and feeds the plugin's
/// sidechain input. Delayed (cyclic) slots read the previous block's taps.
#[allow(clippy::too_many_arguments)]
fn feed_sidechain(
    instance: &mut dyn PluginNode,
    is_bus: bool,
    index: usize,
    chain: usize,
    slot: usize,
    delayed: bool,
    params: &SharedParams,
    strip_pre: &[[Vec<f32>; 2]],
    strip_post: &[[Vec<f32>; 2]],
    pre_snapshot: &[[Vec<f32>; 2]],
    post_snapshot: &[[Vec<f32>; 2]],
    mix: &mut [Vec<f32>; 2],
    silence: &[Vec<f32>; 2],
    frames: usize,
) {
    if !instance.has_sidechain() {
        return;
    }
    mix[0][..frames].fill(0.0);
    mix[1][..frames].fill(0.0);
    let mut any = false;
    let mut gather = |source: usize, gain: f32, pre: bool| {
        if source >= strip_pre.len() {
            return;
        }
        let tap = if delayed {
            if pre {
                &pre_snapshot[source]
            } else {
                &post_snapshot[source]
            }
        } else if pre {
            &strip_pre[source]
        } else {
            &strip_post[source]
        };
        dsp::add_scaled(&mut mix[0][..frames], &tap[0][..frames], gain);
        dsp::add_scaled(&mut mix[1][..frames], &tap[1][..frames], gain);
        any = true;
    };
    if is_bus {
        params.for_each_bus_sidechain(index, slot, &mut gather);
    } else {
        params.for_each_sidechain(index, chain, slot, &mut gather);
    }
    let (left, right) = if any {
        (&mix[0], &mix[1])
    } else {
        (&silence[0], &silence[1])
    };
    instance.write_sidechain(&left[..frames], &right[..frames]);
}

pub fn instantiate_slot(
    lv2: &Host,
    vst3: &vibemix_vst3::Host,
    clap: &vibemix_clap::Host,
    plugin: &PluginSlot,
    sample_rate: f64,
    buffer_size: usize,
) -> Option<(SlotPlugin, Option<SlotState>, usize)> {
    match plugin.format {
        PluginFormat::Lv2 => instantiate_lv2(lv2, plugin, sample_rate, buffer_size),
        PluginFormat::Vst3 => {
            let (mut slot_plugin, handle) =
                instantiate_vst3(vst3, plugin, sample_rate, buffer_size)?;
            slot_plugin.load_state(&plugin.state);
            Some((slot_plugin, Some(SlotState::Vst3(handle)), 0))
        }
        PluginFormat::Clap => {
            let (mut slot_plugin, handle) =
                instantiate_clap(clap, plugin, sample_rate, buffer_size)?;
            slot_plugin.load_state(&plugin.state);
            Some((slot_plugin, Some(SlotState::Clap(handle)), 0))
        }
    }
}

fn instantiate_clap(
    clap: &vibemix_clap::Host,
    plugin: &PluginSlot,
    sample_rate: f64,
    buffer_size: usize,
) -> Option<(SlotPlugin, vibemix_clap::InstanceHandle)> {
    let bundle = plugin.bundle.as_deref()?;
    let open = |clap: &vibemix_clap::Host| {
        clap.instantiate(
            bundle,
            &plugin.uri,
            sample_rate,
            buffer_size,
            vibemix_clap::PluginInstance::max_frames(),
        )
    };
    let first = open(clap)?;
    let handle = first.instance_handle();
    let slot_plugin = match (first.audio_input_count(), first.audio_output_count()) {
        (2, 2) => SlotPlugin::Stereo(Box::new(first) as Box<dyn PluginNode>),
        (1, 1) => {
            let second = open(clap)?;
            SlotPlugin::Mono(
                Box::new(first) as Box<dyn PluginNode>,
                Box::new(second) as Box<dyn PluginNode>,
            )
        }
        _ => return None,
    };
    Some((slot_plugin, handle))
}

fn instantiate_vst3(
    vst3: &vibemix_vst3::Host,
    plugin: &PluginSlot,
    sample_rate: f64,
    buffer_size: usize,
) -> Option<(SlotPlugin, vibemix_vst3::InstanceHandle)> {
    let bundle = plugin.bundle.as_deref()?;
    let open = |vst3: &vibemix_vst3::Host| {
        vst3.instantiate(
            bundle,
            &plugin.uri,
            sample_rate,
            buffer_size,
            vibemix_vst3::PluginInstance::max_frames(),
        )
    };
    let first = open(vst3)?;
    let handle = first.instance_handle();
    let slot_plugin = match (first.audio_input_count(), first.audio_output_count()) {
        (2, 2) => SlotPlugin::Stereo(Box::new(first) as Box<dyn PluginNode>),
        (1, 1) => {
            let second = open(vst3)?;
            SlotPlugin::Mono(
                Box::new(first) as Box<dyn PluginNode>,
                Box::new(second) as Box<dyn PluginNode>,
            )
        }
        _ => return None,
    };
    Some((slot_plugin, handle))
}

fn instantiate_lv2(
    host: &Host,
    plugin: &PluginSlot,
    sample_rate: f64,
    buffer_size: usize,
) -> Option<(SlotPlugin, Option<SlotState>, usize)> {
    let Some(instance) = host.instantiate(
        &plugin.uri,
        sample_rate,
        buffer_size,
        PluginInstance::max_frames(),
    ) else {
        tracing::warn!(uri = %plugin.uri, "plugin not hostable, dropped from session");
        return None;
    };
    let proxy = instance.state_proxy().map(SlotState::Lv2);
    let handle = instance.handle() as usize;
    let Some(mut slot_plugin) = make_slot_plugin(
        host,
        &plugin.uri,
        sample_rate,
        buffer_size,
        PluginInstance::max_frames(),
        instance,
    ) else {
        tracing::warn!(uri = %plugin.uri, "plugin is not mono/stereo, dropped from session");
        return None;
    };

    slot_plugin.load_state(&plugin.state);

    Some((slot_plugin, proxy, handle))
}

#[allow(clippy::too_many_arguments)]
fn build_chains(
    lv2: &Host,
    vst3: &vibemix_vst3::Host,
    clap: &vibemix_clap::Host,
    model: &mut MixerModel,
    strip_count: usize,
    bus_count: usize,
    sample_rate: f64,
    buffer_size: usize,
    params: &SharedParams,
) -> Built {
    let mut strip_chains: Vec<StripChains> = (0..strip_count)
        .map(|_| std::array::from_fn(|_| Vec::with_capacity(MAX_PLUGINS)))
        .collect();
    let mut strip_meta: Vec<StripMeta> = (0..strip_count)
        .map(|_| std::array::from_fn(|_| Vec::with_capacity(MAX_PLUGINS)))
        .collect();
    let mut bus_chains: Vec<Vec<SlotPlugin>> = (0..bus_count)
        .map(|_| Vec::with_capacity(MAX_PLUGINS))
        .collect();
    let mut bus_meta: Vec<Vec<SlotMeta>> = (0..bus_count).map(|_| Vec::new()).collect();

    for strip in &mut model.strips {
        let index = strip.id.0 as usize;
        if index >= strip_count {
            continue;
        }

        for (chain, list) in [&mut strip.plugins, &mut strip.plugins_post]
            .into_iter()
            .enumerate()
        {
            let mut kept = Vec::new();
            for mut plugin in list.drain(..).take(MAX_PLUGINS) {
                let slot = strip_chains[index][chain].len();
                match instantiate_slot(lv2, vst3, clap, &plugin, sample_rate, buffer_size) {
                    Some((slot_plugin, proxy, handle)) => {
                        plugin.missing = false;
                        let controls = controls_of(&slot_plugin);
                        for param in 0..controls.min(MAX_PARAMS) {
                            let value = plugin
                                .params
                                .get(param)
                                .copied()
                                .unwrap_or_else(|| control_default_of(&slot_plugin, param));
                            params.set_plugin_param(index, chain, slot, param, value);
                        }
                        params.set_plugin_bypass(index, chain, slot, plugin.bypassed);
                        strip_meta[index][chain].push(SlotMeta::of(
                            &slot_plugin,
                            proxy,
                            Some(handle),
                        ));
                        strip_chains[index][chain].push(slot_plugin);
                    }
                    None => {
                        plugin.missing = true;
                        strip_chains[index][chain].push(SlotPlugin::Missing);
                        strip_meta[index][chain].push(SlotMeta::default());
                    }
                }
                kept.push(plugin);
            }
            *list = kept;
        }
    }

    for bus in &mut model.buses {
        let index = bus.id.0 as usize;
        if index >= bus_count {
            continue;
        }

        let mut kept = Vec::new();
        for mut plugin in bus.plugins.drain(..).take(MAX_PLUGINS) {
            let slot = bus_chains[index].len();
            match instantiate_slot(lv2, vst3, clap, &plugin, sample_rate, buffer_size) {
                Some((slot_plugin, proxy, handle)) => {
                    plugin.missing = false;
                    let controls = controls_of(&slot_plugin);
                    for param in 0..controls.min(MAX_PARAMS) {
                        let value = plugin
                            .params
                            .get(param)
                            .copied()
                            .unwrap_or_else(|| control_default_of(&slot_plugin, param));
                        params.set_bus_plugin_param(index, slot, param, value);
                    }
                    params.set_bus_plugin_bypass(index, slot, plugin.bypassed);
                    bus_meta[index].push(SlotMeta::of(&slot_plugin, proxy, Some(handle)));
                    bus_chains[index].push(slot_plugin);
                }
                None => {
                    plugin.missing = true;
                    bus_chains[index].push(SlotPlugin::Missing);
                    bus_meta[index].push(SlotMeta::default());
                }
            }
            kept.push(plugin);
        }
        bus.plugins = kept;
    }

    Built {
        strip_chains,
        strip_meta,
        bus_chains,
        bus_meta,
    }
}

pub fn make_slot_plugin(
    host: &Host,
    uri: &str,
    sample_rate: f64,
    nominal_block: usize,
    max_block: usize,
    instance: PluginInstance,
) -> Option<SlotPlugin> {
    // Extra audio inputs beyond the main pair are sidechain, not host shape.
    match (instance.main_input_count(), instance.audio_output_count()) {
        (2, 2) => Some(SlotPlugin::Stereo(Box::new(instance) as Box<dyn PluginNode>)),
        (1, 1) => {
            let second = host.instantiate(uri, sample_rate, nominal_block, max_block)?;
            Some(SlotPlugin::Mono(
                Box::new(instance) as Box<dyn PluginNode>,
                Box::new(second) as Box<dyn PluginNode>,
            ))
        }
        _ => None,
    }
}

fn controls_of(plugin: &SlotPlugin) -> usize {
    match plugin {
        SlotPlugin::Stereo(instance) => instance.control_count(),
        SlotPlugin::Mono(instance, _) => instance.control_count(),
        SlotPlugin::Missing => 0,
    }
}

fn control_default_of(plugin: &SlotPlugin, param: usize) -> f32 {
    match plugin {
        SlotPlugin::Stereo(instance) => instance.control_default(param),
        SlotPlugin::Mono(instance, _) => instance.control_default(param),
        SlotPlugin::Missing => 0.0,
    }
}

fn register_strip_ports(client: &Client, strip: usize) -> Result<(Port<AudioIn>, Port<AudioIn>)> {
    let in_l = client.register_port(&format!("strip{strip}_in_l"), AudioIn::default())?;
    match client.register_port(&format!("strip{strip}_in_r"), AudioIn::default()) {
        Ok(in_r) => Ok((in_l, in_r)),
        Err(error) => {
            let _ = client.unregister_port(in_l);
            Err(error.into())
        }
    }
}

fn register_bus_ports(client: &Client, bus: usize) -> Result<(Port<AudioOut>, Port<AudioOut>)> {
    let out_l = client.register_port(&format!("bus{bus}_out_l"), AudioOut::default())?;
    match client.register_port(&format!("bus{bus}_out_r"), AudioOut::default()) {
        Ok(out_r) => Ok((out_l, out_r)),
        Err(error) => {
            let _ = client.unregister_port(out_l);
            Err(error.into())
        }
    }
}

fn register_bus_return_ports(
    client: &Client,
    bus: usize,
) -> Result<(Port<AudioIn>, Port<AudioIn>)> {
    let in_l = client.register_port(&format!("bus{bus}_ret_l"), AudioIn::default())?;
    match client.register_port(&format!("bus{bus}_ret_r"), AudioIn::default()) {
        Ok(in_r) => Ok((in_l, in_r)),
        Err(error) => {
            let _ = client.unregister_port(in_l);
            Err(error.into())
        }
    }
}

const MAX_PORT_SLUG: usize = 48;

type PortRenameFn = unsafe extern "C" fn(
    *mut jack::jack_sys::jack_client_t,
    *mut jack::jack_sys::jack_port_t,
    *const std::os::raw::c_char,
) -> std::os::raw::c_int;

/// Resolves `jack_port_rename` from the loaded JACK library. The `jack` crate
/// only exposes `Port::set_name`, which is a silent no-op on PipeWire's JACK
/// shim; `jack_port_rename` is the function that actually renames the port.
fn port_rename_fn() -> Option<PortRenameFn> {
    static RENAME: std::sync::OnceLock<Option<PortRenameFn>> = std::sync::OnceLock::new();
    *RENAME.get_or_init(|| {
        let library = jack::jack_sys::library().ok()?;
        let symbol = unsafe { library.get::<PortRenameFn>(b"jack_port_rename\0") }.ok()?;
        Some(*symbol)
    })
}

fn rename_port(client: *mut jack::jack_sys::jack_client_t, port: &mut Port<Unowned>, name: &str) {
    if let Some(rename) = port_rename_fn() {
        let Ok(cname) = std::ffi::CString::new(name) else {
            tracing::warn!(name, "port name contains a nul byte");
            return;
        };
        let result = unsafe { rename(client, port.raw(), cname.as_ptr()) };
        if result != 0 {
            tracing::warn!(name, "jack_port_rename failed");
        }
    } else if let Err(error) = port.set_name(name) {
        tracing::warn!(%error, name, "failed to rename port");
    }
}

/// Keep saved MIDI peers whose JACK port is currently absent, so a device that
/// enumerates late is not dropped from the session by the next save. A peer
/// that exists but is disconnected is intentionally omitted.
fn merge_missing_connections(
    current: Vec<String>,
    saved: Option<&[String]>,
    exists: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut out = current;
    if let Some(saved) = saved {
        for name in saved {
            if !out.iter().any(|entry| entry == name) && !exists(name) {
                out.push(name.clone());
            }
        }
    }
    out
}

fn restore_input(
    client: &Client,
    port: &Port<Unowned>,
    role: &'static str,
    owner: &str,
    sources: &[String],
    missing: &mut Vec<MissingConnection>,
) {
    let _ = client.disconnect(port);
    let Ok(name) = port.name() else {
        return;
    };
    for source in sources {
        if client.connect_ports_by_name(source, &name).is_err() {
            missing.push(MissingConnection {
                owner_kind: "strip",
                owner: owner.to_string(),
                port_role: role,
                port: source.clone(),
            });
        }
    }
}

fn restore_output(
    client: &Client,
    port: &Port<Unowned>,
    role: &'static str,
    owner: &str,
    destinations: &[String],
    missing: &mut Vec<MissingConnection>,
) {
    let _ = client.disconnect(port);
    let Ok(name) = port.name() else {
        return;
    };
    for destination in destinations {
        if client.connect_ports_by_name(&name, destination).is_err() {
            missing.push(MissingConnection {
                owner_kind: "bus",
                owner: owner.to_string(),
                port_role: role,
                port: destination.clone(),
            });
        }
    }
}

fn port_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut underscore = false;
    for ch in name.trim().chars() {
        if ch == ':' || ch.is_whitespace() || ch.is_control() {
            if !underscore {
                slug.push('_');
                underscore = true;
            }
        } else {
            slug.push(ch);
            underscore = false;
        }
    }
    slug.trim_matches('_').chars().take(MAX_PORT_SLUG).collect()
}

fn resolve_port_bases(entries: &[(usize, String)]) -> std::collections::HashMap<usize, String> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (_, base) in entries {
        *counts.entry(base.as_str()).or_insert(0) += 1;
    }
    entries
        .iter()
        .map(|(index, base)| {
            let resolved = if counts.get(base.as_str()).copied().unwrap_or(0) > 1 {
                format!("{base}_{index}")
            } else {
                base.clone()
            };
            (*index, resolved)
        })
        .collect()
}

fn strip_port_bases(model: &MixerModel) -> std::collections::HashMap<usize, String> {
    let entries: Vec<(usize, String)> = model
        .strips
        .iter()
        .map(|strip| {
            let index = strip.id.0 as usize;
            let base = port_slug(&strip.name);
            let base = if base.is_empty() {
                format!("strip{index}")
            } else {
                base
            };
            (index, base)
        })
        .collect();
    resolve_port_bases(&entries)
}

fn bus_port_bases(model: &MixerModel) -> std::collections::HashMap<usize, String> {
    let entries: Vec<(usize, String)> = model
        .buses
        .iter()
        .map(|bus| {
            let index = bus.id.0 as usize;
            let base = port_slug(&bus.name);
            let base = if base.is_empty() {
                format!("bus{index}")
            } else {
                base
            };
            (index, base)
        })
        .collect();
    resolve_port_bases(&entries)
}

fn auto_connect_ports(client: &Client, client_name: &str, strip_count: usize, bus_count: usize) {
    for strip in 0..strip_count.min(4) {
        let _ = client.connect_ports_by_name(
            &format!("system:capture_{}", strip * 2 + 1),
            &format!("{client_name}:strip{strip}_in_l"),
        );
        let _ = client.connect_ports_by_name(
            &format!("system:capture_{}", strip * 2 + 2),
            &format!("{client_name}:strip{strip}_in_r"),
        );
    }

    if bus_count > 0 {
        let _ =
            client.connect_ports_by_name(&format!("{client_name}:bus0_out_l"), "system:playback_1");
        let _ =
            client.connect_ports_by_name(&format!("{client_name}:bus0_out_r"), "system:playback_2");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibemix_core::ids::StripId;
    use vibemix_core::model::{MixerModel, Strip};

    #[test]
    fn retiring_a_chain_keeps_its_capacity() {
        let (mut garbage, mut drain) = rtrb::RingBuffer::<u32>::new(8);
        let mut chain: Vec<u32> = Vec::with_capacity(MAX_PLUGINS);
        chain.extend([1, 2, 3]);
        retire_plugins(&mut chain, &mut garbage);
        assert!(chain.is_empty());
        assert!(chain.capacity() >= MAX_PLUGINS);
        let mut retired = 0;
        while drain.pop().is_ok() {
            retired += 1;
        }
        assert_eq!(retired, 3);
    }

    #[test]
    fn slug_replaces_separators_and_trims() {
        assert_eq!(port_slug("  ch 1 "), "ch_1");
        assert_eq!(port_slug("drums:kit"), "drums_kit");
        assert_eq!(port_slug("a   b"), "a_b");
        assert_eq!(port_slug("___"), "");
    }

    #[test]
    fn saved_midi_peers_survive_while_missing() {
        let saved = vec![
            "present-disconnected".to_string(),
            "missing-device".to_string(),
        ];
        let current = vec!["connected".to_string()];
        let merged = merge_missing_connections(current, Some(&saved), &|name| {
            name == "present-disconnected"
        });
        assert_eq!(merged, vec!["connected", "missing-device"]);

        // Once the missing peer exists, a normal disconnection drops it.
        let merged =
            merge_missing_connections(vec!["connected".to_string()], Some(&saved), &|_| true);
        assert_eq!(merged, vec!["connected"]);
    }

    #[test]
    fn duplicate_strip_names_get_id_suffix() {
        let mut model = MixerModel::new("t");
        model.strips.push(Strip::new(StripId(0), "vocals"));
        model.strips.push(Strip::new(StripId(1), "vocals"));
        model.strips.push(Strip::new(StripId(2), "guitar"));

        let bases = strip_port_bases(&model);
        assert_eq!(bases.get(&0).map(String::as_str), Some("vocals_0"));
        assert_eq!(bases.get(&1).map(String::as_str), Some("vocals_1"));
        assert_eq!(bases.get(&2).map(String::as_str), Some("guitar"));
    }

    #[test]
    fn default_model_names_slug() {
        let model = MixerModel::default_minimal();
        assert_eq!(
            strip_port_bases(&model).get(&0).map(String::as_str),
            Some("ch_1")
        );
        assert_eq!(
            bus_port_bases(&model).get(&0).map(String::as_str),
            Some("Master")
        );
    }

    #[test]
    fn scratch_buffers_cover_the_period() {
        let scratch = ScratchBuffers::new(4096, 3, 2);
        assert_eq!(scratch.chain_dry.len(), 2);
        assert_eq!(scratch.chain_dry[0].len(), 4096);
        assert_eq!(scratch.strip_scratch[0].len(), 4096);
        for field in [
            &scratch.strip_pre_fader,
            &scratch.strip_input,
            &scratch.strip_post_fader,
            &scratch.strip_pre_snapshot,
            &scratch.strip_post_snapshot,
        ] {
            assert_eq!(field.len(), 3);
            assert_eq!(field[0][0].len(), 4096);
            assert_eq!(field[0][1].len(), 4096);
        }
        assert_eq!(scratch.send_scratch[0].len(), 4096);
        assert_eq!(scratch.sidechain_mix[0].len(), 4096);
        assert_eq!(scratch.sidechain_silence[0].len(), 4096);
        assert_eq!(scratch.bus_scratch[0].len(), 4096);
        assert_eq!(scratch.bus_accum.len(), 2);
        assert_eq!(scratch.bus_accum[0][0].len(), 4096);
        assert_eq!(scratch.bus_out.len(), 2);
        assert_eq!(scratch.bus_out[0][0].len(), 4096);
        assert_eq!(scratch.record_mix[0].len(), 4096);
        assert_eq!(scratch.record_mix[1].len(), 4096);
    }
}

#[cfg(test)]
mod sidechain_feed_tests {
    use super::*;

    struct Recorder {
        has: bool,
        left: Vec<f32>,
    }

    impl PluginNode for Recorder {
        fn input_buffer_mut(&mut self, _channel: usize) -> &mut [f32] {
            &mut []
        }
        fn output_buffer(&self, _channel: usize) -> &[f32] {
            &[]
        }
        fn control_count(&self) -> usize {
            0
        }
        fn control_default(&self, _param: usize) -> f32 {
            0.0
        }
        fn set_control(&mut self, _param: usize, _value: f32) {}
        fn write_midi(&mut self, _events: &[MidiEvent]) {}
        fn write_position(&mut self, _position: &vibemix_lv2::TimePosition) {}
        fn run(&mut self, _frames: usize) {}
        fn deliver_responses(&mut self) {}
        fn outputs(&self) -> Option<Arc<SharedOutputs>> {
            None
        }
        fn load_state(&mut self, _state: &str) {}
        fn has_sidechain(&self) -> bool {
            self.has
        }
        fn write_sidechain(&mut self, left: &[f32], _right: &[f32]) {
            self.left = left.to_vec();
        }
    }

    #[test]
    fn sums_sources_with_gains_and_taps() {
        let params = SharedParams::new(2, 1);
        // Plugin on strip 0: source strip 0 pre-fader at 0.5, strip 1 post at 0.25.
        params.set_sidechain_gain(0, 0, 0, 0, 0.5);
        params.set_sidechain_pre(0, 0, 0, 0, true);
        params.set_sidechain_gain(0, 0, 0, 1, 0.25);
        params.set_sidechain_pre(0, 0, 0, 1, false);

        let pre = vec![
            [vec![1.0f32; 4], vec![1.0; 4]],
            [vec![2.0; 4], vec![2.0; 4]],
        ];
        let post = vec![
            [vec![10.0f32; 4], vec![10.0; 4]],
            [vec![20.0; 4], vec![20.0; 4]],
        ];
        let zeros = vec![
            [vec![0.0f32; 4], vec![0.0; 4]],
            [vec![0.0; 4], vec![0.0; 4]],
        ];
        let silence = [vec![0.0f32; 4], vec![0.0; 4]];
        let mut mix = [vec![0.0f32; 4], vec![0.0; 4]];

        let mut recorder = Recorder {
            has: true,
            left: Vec::new(),
        };
        feed_sidechain(
            &mut recorder,
            false,
            0,
            0,
            0,
            false,
            &params,
            &pre,
            &post,
            &zeros,
            &zeros,
            &mut mix,
            &silence,
            4,
        );

        // 1.0 * 0.5 (pre) + 20.0 * 0.25 (post) = 5.5
        assert!((recorder.left[0] - 5.5).abs() < 1e-4, "{:?}", recorder.left);
    }
}
