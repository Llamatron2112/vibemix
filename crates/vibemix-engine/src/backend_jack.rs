use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use jack::{
    AudioIn, AudioOut, Client, ClientOptions, Control, Frames, MidiIn, NotificationHandler, Port,
    ProcessHandler, ProcessScope, Unowned,
};
use rtrb::{Consumer, Producer};
use vibemix_core::model::{BusConnections, MixerModel, PluginFormat, PluginSlot, StripConnections};
use vibemix_lv2::{Host, MidiEvent, PluginInstance, SharedOutputs, SharedUiInputs, StateEntry, StateProxy};

use crate::plugin::PluginNode;

use crate::dsp;
use crate::midi::{Clock, MidiBlock};
use crate::midi_map::SharedMidi;
use crate::params::{
    SharedClock, SharedMeters, SharedParams, MAX_PARAMS, MAX_PLUGINS, PLUGIN_CHAINS,
};

const SMOOTHING: f32 = 0.005;
const COMMAND_CAPACITY: usize = 64;

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
type StripProxies = [Vec<Option<SlotState>>; PLUGIN_CHAINS];
type StripHandles = [Vec<Option<usize>>; PLUGIN_CHAINS];
type StripOutputs = [Vec<Option<Arc<SharedOutputs>>>; PLUGIN_CHAINS];
type StripUiInputs = [Vec<Option<Arc<SharedUiInputs>>>; PLUGIN_CHAINS];

struct Built {
    strip_chains: Vec<StripChains>,
    strip_proxies: Vec<StripProxies>,
    strip_handles: Vec<StripHandles>,
    strip_outputs: Vec<StripOutputs>,
    bus_chains: Vec<Vec<SlotPlugin>>,
    bus_proxies: Vec<Vec<Option<SlotState>>>,
    bus_handles: Vec<Vec<Option<usize>>>,
    bus_outputs: Vec<Vec<Option<Arc<SharedOutputs>>>>,
    strip_ui_inputs: Vec<StripUiInputs>,
    bus_ui_inputs: Vec<Vec<Option<Arc<SharedUiInputs>>>>,
}

pub enum SlotPlugin {
    Stereo(Box<dyn PluginNode>),
    Mono(Box<dyn PluginNode>, Box<dyn PluginNode>),
    /// Placeholder for a plugin that could not be loaded. It keeps the DSP
    /// chain aligned with the model slots and passes audio through untouched.
    Missing,
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
    /// `"strip_in"`, `"bus_out"`, `"bus_ret"` or `"midi_in"`.
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
}

impl NotificationHandler for Notifications {
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
    /// Sidechain-aware strip processing order and per-slot cycle flags.
    SetStripOrder {
        order: Vec<usize>,
        delayed: Vec<Vec<Vec<bool>>>,
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
    garbage_chains: Producer<Vec<SlotPlugin>>,
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
    bus_scratch: [Vec<f32>; 2],
    bus_accum: Vec<[Vec<f32>; 2]>,
    capacity: usize,
    midi_in: Port<MidiIn>,
    midi: MidiBlock,
    clock: Clock,
    shared_clock: Arc<SharedClock>,
    position: vibemix_lv2::TimePosition,
    midi_map: Arc<SharedMidi>,
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
                            if let Err(plugin) = self.garbage.push(plugin) { std::mem::forget(plugin); }
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
                            if let Err(plugin) = self.garbage.push(plugin) { std::mem::forget(plugin); }
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
                            let taken = std::mem::take(chain);
                            if let Err(chain) = self.garbage_chains.push(taken) { std::mem::forget(chain); }
                        }
                    }
                }
                EngineCommand::AddBusPlugin { bus, plugin } => {
                    match self.bus_chains.get_mut(bus) {
                        Some(chain) if chain.len() < MAX_PLUGINS => chain.push(plugin),
                        _ => {
                            if let Err(plugin) = self.garbage.push(plugin) { std::mem::forget(plugin); }
                        }
                    }
                }
                EngineCommand::RemoveBusPlugin { bus, slot } => {
                    if let Some(chain) = self.bus_chains.get_mut(bus) {
                        if slot < chain.len() {
                            let plugin = chain.remove(slot);
                            if let Err(plugin) = self.garbage.push(plugin) { std::mem::forget(plugin); }
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
                        let taken = std::mem::take(chain);
                        if let Err(chain) = self.garbage_chains.push(taken) { std::mem::forget(chain); }
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
                EngineCommand::Replace {
                    chains,
                    bus_chains,
                } => {
                    let old = std::mem::replace(&mut self.plugin_chains, chains);
                    if let Err(chains) = self.garbage_nested.push(old) { std::mem::forget(chains); }
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
                    let ratio = if old_rate > 0.0 { sample_rate / old_rate } else { 1.0 };
                    for sends in &mut self.send_delay_smooth {
                        for value in sends.iter_mut() {
                            *value *= ratio;
                        }
                    }
                    self.sample_rate = sample_rate;
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
            }
        }
    }

    fn run_plugins(&mut self, index: usize, chain: usize, frames: usize) {
        if self.plugin_chains[index][chain].is_empty() {
            return;
        }

        let frames = frames.min(PluginInstance::max_frames());
        let position = self.position;
        let midi_sync = self.params.midi_sync();
        let plugins = &mut self.plugin_chains[index][chain];

        for (slot, plugin) in plugins.iter_mut().enumerate() {
            match plugin {
                SlotPlugin::Stereo(instance) => instance.deliver_responses(),
                SlotPlugin::Mono(left, right) => {
                    left.deliver_responses();
                    right.deliver_responses();
                }
                SlotPlugin::Missing => {}
            }

            if self.params.plugin_bypass(index, chain, slot) {
                continue;
            }

            match plugin {
                SlotPlugin::Stereo(instance) => {
                    set_controls(&mut **instance, &self.params, index, chain, slot);
                    instance.input_buffer_mut(0)[..frames]
                        .copy_from_slice(&self.strip_scratch[0][..frames]);
                    instance.input_buffer_mut(1)[..frames]
                        .copy_from_slice(&self.strip_scratch[1][..frames]);
                    instance.write_midi(self.midi.as_slice());
                    if midi_sync {
                        instance.write_position(&position);
                    }
                    instance.write_ui_events(midi_sync);
                    feed_sidechain(
                        &mut **instance,
                        false,
                        index,
                        chain,
                        slot,
                        self.strip_delayed[index][chain][slot],
                        &self.params,
                        &self.strip_pre_fader,
                        &self.strip_post_fader,
                        &self.strip_pre_snapshot,
                        &self.strip_post_snapshot,
                        &mut self.sidechain_mix,
                        &self.sidechain_silence,
                        frames,
                    );
                    instance.run(frames);

                    let out_l = instance.output_buffer(0);
                    let out_r = instance.output_buffer(1);
                    self.strip_scratch[0][..frames].copy_from_slice(&out_l[..frames]);
                    self.strip_scratch[1][..frames].copy_from_slice(&out_r[..frames]);
                }
                SlotPlugin::Mono(left, right) => {
                    for (channel, instance) in [(0usize, left), (1usize, right)] {
                        set_controls(&mut **instance, &self.params, index, chain, slot);
                        instance.input_buffer_mut(0)[..frames]
                            .copy_from_slice(&self.strip_scratch[channel][..frames]);
                        instance.write_midi(self.midi.as_slice());
                        if midi_sync {
                            instance.write_position(&position);
                        }
                        instance.write_ui_events(midi_sync);
                        feed_sidechain(
                            &mut **instance,
                            false,
                            index,
                            chain,
                            slot,
                            self.strip_delayed[index][chain][slot],
                            &self.params,
                            &self.strip_pre_fader,
                            &self.strip_post_fader,
                            &self.strip_pre_snapshot,
                            &self.strip_post_snapshot,
                            &mut self.sidechain_mix,
                            &self.sidechain_silence,
                            frames,
                        );
                        instance.run(frames);
                        let out = instance.output_buffer(0);
                        self.strip_scratch[channel][..frames].copy_from_slice(&out[..frames]);
                    }
                }
                SlotPlugin::Missing => {}
            }
        }
    }

    fn run_bus_plugins(&mut self, bus: usize, frames: usize) {
        if self.bus_chains[bus].is_empty() {
            return;
        }

        let frames = frames.min(PluginInstance::max_frames());
        let position = self.position;
        let midi_sync = self.params.midi_sync();
        let plugins = &mut self.bus_chains[bus];

        for (slot, plugin) in plugins.iter_mut().enumerate() {
            match plugin {
                SlotPlugin::Stereo(instance) => instance.deliver_responses(),
                SlotPlugin::Mono(left, right) => {
                    left.deliver_responses();
                    right.deliver_responses();
                }
                SlotPlugin::Missing => {}
            }

            if self.params.bus_plugin_bypass(bus, slot) {
                continue;
            }

            match plugin {
                SlotPlugin::Stereo(instance) => {
                    set_bus_controls(&mut **instance, &self.params, bus, slot);
                    instance.input_buffer_mut(0)[..frames]
                        .copy_from_slice(&self.bus_scratch[0][..frames]);
                    instance.input_buffer_mut(1)[..frames]
                        .copy_from_slice(&self.bus_scratch[1][..frames]);
                    instance.write_midi(self.midi.as_slice());
                    if midi_sync {
                        instance.write_position(&position);
                    }
                    instance.write_ui_events(midi_sync);
                    feed_sidechain(
                        &mut **instance,
                        true,
                        bus,
                        0,
                        slot,
                        false,
                        &self.params,
                        &self.strip_pre_fader,
                        &self.strip_post_fader,
                        &self.strip_pre_snapshot,
                        &self.strip_post_snapshot,
                        &mut self.sidechain_mix,
                        &self.sidechain_silence,
                        frames,
                    );
                    instance.run(frames);

                    let out_l = instance.output_buffer(0);
                    let out_r = instance.output_buffer(1);
                    self.bus_scratch[0][..frames].copy_from_slice(&out_l[..frames]);
                    self.bus_scratch[1][..frames].copy_from_slice(&out_r[..frames]);
                }
                SlotPlugin::Mono(left, right) => {
                    for (channel, instance) in [(0usize, left), (1usize, right)] {
                        set_bus_controls(&mut **instance, &self.params, bus, slot);
                        instance.input_buffer_mut(0)[..frames]
                            .copy_from_slice(&self.bus_scratch[channel][..frames]);
                        instance.write_midi(self.midi.as_slice());
                        if midi_sync {
                            instance.write_position(&position);
                        }
                        instance.write_ui_events(midi_sync);
                        feed_sidechain(
                            &mut **instance,
                            true,
                            bus,
                            0,
                            slot,
                            false,
                            &self.params,
                            &self.strip_pre_fader,
                            &self.strip_post_fader,
                            &self.strip_pre_snapshot,
                            &self.strip_post_snapshot,
                            &mut self.sidechain_mix,
                            &self.sidechain_silence,
                            frames,
                        );
                        instance.run(frames);
                        let out = instance.output_buffer(0);
                        self.bus_scratch[channel][..frames].copy_from_slice(&out[..frames]);
                    }
                }
                SlotPlugin::Missing => {}
            }
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
                continue;
            }

            {
                let Some(ports) = self.strips[index].as_ref() else {
                    self.meters.store_strip_in(index, 0.0, 0.0);
                    self.meters.store_strip_gain(index, 0.0, 0.0);
                    self.meters.store_strip_out(index, 0.0, 0.0);
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
            let input_smoothed = &mut self.strip_input_smooth[index];
            *input_smoothed += (input_target - *input_smoothed) * SMOOTHING;
            let input_gain = *input_smoothed;
            dsp::apply_gain(&mut self.strip_scratch[0][..frames], input_gain);
            dsp::apply_gain(&mut self.strip_scratch[1][..frames], input_gain);
            self.meters.store_strip_gain(
                index,
                in_peak_l * input_gain,
                in_peak_r * input_gain,
            );

            if !paused {
                self.run_plugins(index, 0, frames);
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
            let smoothed = &mut self.strip_gain_smooth[index];
            *smoothed += (target - *smoothed) * SMOOTHING;
            let gain = *smoothed;

            let pan_target = self.params.strip_pan(index);
            let pan = {
                let smoothed = &mut self.strip_pan_smooth[index];
                *smoothed += (pan_target - *smoothed) * SMOOTHING;
                *smoothed
            };
            let (pan_l, pan_r) = dsp::pan_gains(pan);

            dsp::apply_gain(&mut self.strip_scratch[0][..frames], gain * pan_l);
            dsp::apply_gain(&mut self.strip_scratch[1][..frames], gain * pan_r);

            if !paused {
                self.run_plugins(index, 1, frames);
            }

            // Post-fader tap (the same point a post-fader send uses).
            self.strip_post_fader[index][0][..frames]
                .copy_from_slice(&self.strip_scratch[0][..frames]);
            self.strip_post_fader[index][1][..frames]
                .copy_from_slice(&self.strip_scratch[1][..frames]);

            let strip_peak_l = dsp::peak(&self.strip_scratch[0][..frames]);
            let strip_peak_r = dsp::peak(&self.strip_scratch[1][..frames]);
            self.meters.store_strip_out(index, strip_peak_l, strip_peak_r);

            for bus in 0..bus_count {
                let gain_target = self.params.send_gain(index, bus);
                let gain = {
                    let smoothed = &mut self.send_gain_smooth[index][bus];
                    *smoothed += (gain_target - *smoothed) * SMOOTHING;
                    *smoothed
                };
                if gain <= 1.0e-6 {
                    continue;
                }

                let delay_target =
                    self.params.send_delay(index, bus) * self.sample_rate / 1000.0;
                let delay = {
                    let smoothed = &mut self.send_delay_smooth[index][bus];
                    *smoothed += (delay_target - *smoothed) * SMOOTHING;
                    *smoothed
                };

                let (src_l, src_r) = if self.params.send_pre_fader(index, bus) {
                    (
                        &self.strip_pre_fader[index][0],
                        &self.strip_pre_fader[index][1],
                    )
                } else {
                    (&self.strip_scratch[0], &self.strip_scratch[1])
                };

                let [send_l, send_r] = &mut self.send_scratch;
                self.send_delays[index][bus].process(
                    &src_l[..frames],
                    &src_r[..frames],
                    &mut send_l[..frames],
                    &mut send_r[..frames],
                    delay,
                );
                dsp::add_scaled(
                    &mut self.bus_accum[bus][0][..frames],
                    &send_l[..frames],
                    gain,
                );
                dsp::add_scaled(
                    &mut self.bus_accum[bus][1][..frames],
                    &send_r[..frames],
                    gain,
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

    fn process_one_bus(&mut self, bus: usize, scope: &ProcessScope, frames: usize) {
        if !self.params.bus_active(bus) {
            self.meters.store_bus_in(bus, 0.0, 0.0);
            self.meters.store_bus_gain(bus, 0.0, 0.0);
            self.meters.store_bus_ret(bus, 0.0, 0.0);
            self.meters.store_bus(bus, 0.0, 0.0);
            return;
        }

        self.bus_scratch[0][..frames].copy_from_slice(&self.bus_accum[bus][0][..frames]);
        self.bus_scratch[1][..frames].copy_from_slice(&self.bus_accum[bus][1][..frames]);

        let in_peak_l = dsp::peak(&self.bus_scratch[0][..frames]);
        let in_peak_r = dsp::peak(&self.bus_scratch[1][..frames]);
        self.meters.store_bus_in(bus, in_peak_l, in_peak_r);

        self.run_bus_plugins(bus, frames);

        // A returning bus is an external insert: the post-insert signal leaves
        // pre-fader for the device (scaled by the SEND trim), and the device's
        // output is mixed back in (dry + wet) before the fader. The master has
        // no separate send, so this only applies to non-master buses.
        let returning = bus != 0 && self.params.bus_return(bus);
        let send_gain = if returning {
            let target = self.params.bus_send_gain(bus);
            let smoothed = &mut self.bus_send_smooth[bus];
            *smoothed += (target - *smoothed) * SMOOTHING;
            *smoothed
        } else {
            1.0
        };

        let gain_peak_l = dsp::peak(&self.bus_scratch[0][..frames]);
        let gain_peak_r = dsp::peak(&self.bus_scratch[1][..frames]);
        self.meters.store_bus_gain(
            bus,
            gain_peak_l * send_gain,
            gain_peak_r * send_gain,
        );

        if returning {
            if let Some(ports) = self.buses[bus].as_mut() {
                let out_l = ports.out_l.as_mut_slice(scope);
                let out_r = ports.out_r.as_mut_slice(scope);
                dsp::scale_into(out_l, &self.bus_scratch[0][..frames], send_gain);
                dsp::scale_into(out_r, &self.bus_scratch[1][..frames], send_gain);
            }

            let ret_target = self.params.bus_return_gain(bus);
            let ret_smoothed = &mut self.bus_return_smooth[bus];
            *ret_smoothed += (ret_target - *ret_smoothed) * SMOOTHING;
            let return_gain = *ret_smoothed;

            if let Some(ports) = self.bus_returns[bus].as_ref() {
                let in_l = ports.in_l.as_slice(scope);
                let in_r = ports.in_r.as_slice(scope);
                let ret_peak_l = dsp::peak(&in_l[..frames]);
                let ret_peak_r = dsp::peak(&in_r[..frames]);
                self.meters.store_bus_ret(
                    bus,
                    ret_peak_l * return_gain,
                    ret_peak_r * return_gain,
                );
                dsp::add_scaled(
                    &mut self.bus_scratch[0][..frames],
                    &in_l[..frames],
                    return_gain,
                );
                dsp::add_scaled(
                    &mut self.bus_scratch[1][..frames],
                    &in_r[..frames],
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
        let smoothed = &mut self.bus_gain_smooth[bus];
        *smoothed += (target - *smoothed) * SMOOTHING;
        let gain = *smoothed;
        dsp::apply_gain(&mut self.bus_scratch[0][..frames], gain);
        dsp::apply_gain(&mut self.bus_scratch[1][..frames], gain);

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
    }
}

impl ProcessHandler for MixerGraph {
    fn process(&mut self, _client: &Client, scope: &ProcessScope) -> Control {
        let _rt_guard = crate::rt_guard::AudioScope::enter();
        dsp::flush_denormals();
        self.apply_commands();

        let paused = self.params.state_pause();
        self.params.set_state_ack(paused);

        let raw_frames = scope.n_frames() as usize;
        let frames = raw_frames.min(self.capacity);

        // MIDI: collect the block's events and advance clock/transport state.
        // `raw.time` is the frame offset within the block.
        self.midi.clear();
        for raw in self.midi_in.iter(scope) {
            self.midi.push(MidiEvent::new(raw.time as i64, raw.bytes));
        }

        // Clock/transport always tracks the input so the GUI can show position.
        self.clock.set_beats_per_bar(self.params.beats_per_bar());
        self.clock.set_sample_rate(self.sample_rate);
        self.clock.process_block(self.midi.as_slice(), frames);
        self.position = self.clock.position();
        self.shared_clock.publish(&self.clock);

        // When MIDI sync is off, drop system realtime/transport messages so
        // plugins don't receive clock (notes/CC are still forwarded).
        if !self.params.midi_sync() {
            let mut kept = 0usize;
            for index in 0..self.midi.len() {
                let event = self.midi.get(index);
                let status = event.as_slice().first().copied().unwrap_or(0);
                let transport = status >= 0xF8 || status == 0xF2 || status == 0xF3;
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
        self.process_buses(scope, frames);

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

        Control::Continue
    }

    fn buffer_size(&mut self, _client: &Client, size: Frames) -> Control {
        let size = size as usize;
        if size > self.capacity {
            tracing::warn!(
                size,
                capacity = self.capacity,
                "host period exceeds buffer capacity; processing is truncated"
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
    garbage_chains: Consumer<Vec<SlotPlugin>>,
    garbage_bus_chains: Consumer<Vec<Vec<SlotPlugin>>>,
    garbage_nested: Consumer<Vec<StripChains>>,
    garbage_delays: Consumer<Vec<Vec<dsp::DelayLine>>>,
    garbage_order: Consumer<RetiredOrder>,
    pending_rate: Arc<AtomicU32>,
    xruns: Arc<AtomicU64>,
    clock: Arc<SharedClock>,
    midi_map: Arc<SharedMidi>,
    midi_port: Port<Unowned>,
    /// The sample rate the graph (delay lines, plugins) was actually built
    /// with. JACK/PipeWire may report a different rate at client creation than
    /// the graph settles on, so track this separately from the live rate.
    graph_sample_rate: u32,
    state_proxies: Vec<StripProxies>,
    plugin_handles: Vec<StripHandles>,
    plugin_outputs: Vec<StripOutputs>,
    plugin_ui_inputs: Vec<StripUiInputs>,
    bus_state_proxies: Vec<Vec<Option<SlotState>>>,
    bus_plugin_handles: Vec<Vec<Option<usize>>>,
    bus_plugin_outputs: Vec<Vec<Option<Arc<SharedOutputs>>>>,
    bus_plugin_ui_inputs: Vec<Vec<Option<Arc<SharedUiInputs>>>>,
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
            (vibemix_core::model::MAX_SEND_DELAY_MS as f64 / 1000.0 * sample_rate).ceil() as usize + 1;

        let built = build_chains(lv2, vst3, clap, model, strip_count, bus_count, sample_rate, buffer_size, &params);
        let Built {
            strip_chains: plugin_chains,
            strip_proxies: state_proxies,
            strip_handles: plugin_handles,
            strip_outputs: plugin_outputs,
            bus_chains,
            bus_proxies: bus_state_proxies,
            bus_handles: bus_plugin_handles,
            bus_outputs: bus_plugin_outputs,
            strip_ui_inputs: plugin_ui_inputs,
            bus_ui_inputs: bus_plugin_ui_inputs,
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
        let shared_clock = Arc::new(SharedClock::new());
        let midi_map = Arc::new(SharedMidi::new());

        let (commands, command_consumer) =
            rtrb::RingBuffer::<EngineCommand>::new(COMMAND_CAPACITY);
        // Garbage queues are sized so a full command queue's worth of retired
        // objects can never overflow them (the control thread drains each loop).
        let garbage_plugins_capacity = COMMAND_CAPACITY * (strip_count + bus_count + 1);
        let garbage_chains_capacity = COMMAND_CAPACITY * (bus_count.max(PLUGIN_CHAINS) + 1);
        let garbage_nested_capacity = COMMAND_CAPACITY;
        let (garbage_producer, garbage) =
            rtrb::RingBuffer::<SlotPlugin>::new(garbage_plugins_capacity);
        let (garbage_chains_producer, garbage_chains) =
            rtrb::RingBuffer::<Vec<SlotPlugin>>::new(garbage_chains_capacity);
        let (garbage_bus_chains_producer, garbage_bus_chains) =
            rtrb::RingBuffer::<Vec<Vec<SlotPlugin>>>::new(4);
        let (garbage_nested_producer, garbage_nested) =
            rtrb::RingBuffer::<Vec<StripChains>>::new(garbage_nested_capacity);
        let (garbage_delays_producer, garbage_delays) =
            rtrb::RingBuffer::<Vec<Vec<dsp::DelayLine>>>::new(4);
        let (garbage_order_producer, garbage_order) = rtrb::RingBuffer::<RetiredOrder>::new(4);

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
            garbage_chains: garbage_chains_producer,
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
            strip_scratch: [vec![0.0; capacity], vec![0.0; capacity]],
            strip_pre_fader: (0..strip_count)
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
            bus_scratch: [vec![0.0; capacity], vec![0.0; capacity]],
            bus_accum: (0..bus_count)
                .map(|_| [vec![0.0f32; capacity], vec![0.0f32; capacity]])
                .collect(),
            capacity,
            midi_in,
            midi: MidiBlock::default(),
            clock: Clock::new(sample_rate as f32, 4.0),
            shared_clock: Arc::clone(&shared_clock),
            position: vibemix_lv2::TimePosition::default(),
            midi_map: Arc::clone(&midi_map),
        };

        let pending_rate = Arc::new(AtomicU32::new(0));
        let xruns = Arc::new(AtomicU64::new(0));
        let active = client
            .activate_async(
                Notifications {
                    pending_rate: Arc::clone(&pending_rate),
                    xruns: Arc::clone(&xruns),
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
            garbage_chains,
            garbage_bus_chains,
            garbage_nested,
            garbage_delays,
            garbage_order,
            pending_rate,
            xruns,
            clock: shared_clock,
            midi_map,
            midi_port,
            graph_sample_rate: sample_rate as u32,
            state_proxies,
            plugin_handles,
            plugin_outputs,
            plugin_ui_inputs,
            bus_state_proxies,
            bus_plugin_handles,
            bus_plugin_outputs,
            bus_plugin_ui_inputs,
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

    /// Returns and clears a pending sample-rate change reported by JACK.
    pub fn take_pending_sample_rate(&self) -> Option<u32> {
        let rate = self.pending_rate.swap(0, Ordering::SeqCst);
        (rate != 0).then_some(rate)
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
        let outputs = plugin.outputs();
        let ui_inputs = plugin.ui_inputs();
        self.commands
            .push(EngineCommand::AddPlugin {
                strip,
                chain,
                plugin,
            })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self
            .state_proxies
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if slots.len() < MAX_PLUGINS {
                slots.push(proxy);
            }
        }
        if let Some(slots) = self
            .plugin_handles
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if slots.len() < MAX_PLUGINS {
                slots.push(Some(handle));
            }
        }
        if let Some(slots) = self
            .plugin_outputs
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if slots.len() < MAX_PLUGINS {
                slots.push(outputs);
            }
        }
        if let Some(slots) = self
            .plugin_ui_inputs
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if slots.len() < MAX_PLUGINS {
                slots.push(ui_inputs);
            }
        }
        Ok(())
    }

    pub fn remove_plugin(&mut self, strip: usize, chain: usize, slot: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::RemovePlugin { strip, chain, slot })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self
            .state_proxies
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if slot < slots.len() {
                slots.remove(slot);
            }
        }
        if let Some(slots) = self
            .plugin_handles
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if slot < slots.len() {
                slots.remove(slot);
            }
        }
        if let Some(slots) = self
            .plugin_outputs
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if slot < slots.len() {
                slots.remove(slot);
            }
        }
        if let Some(slots) = self
            .plugin_ui_inputs
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
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
        if let Some(slots) = self
            .state_proxies
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if from < slots.len() && to < slots.len() {
                let proxy = slots.remove(from);
                slots.insert(to, proxy);
            }
        }
        if let Some(slots) = self
            .plugin_handles
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if from < slots.len() && to < slots.len() {
                let handle = slots.remove(from);
                slots.insert(to, handle);
            }
        }
        if let Some(slots) = self
            .plugin_outputs
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if from < slots.len() && to < slots.len() {
                let outputs = slots.remove(from);
                slots.insert(to, outputs);
            }
        }
        if let Some(slots) = self
            .plugin_ui_inputs
            .get_mut(strip)
            .and_then(|chains| chains.get_mut(chain))
        {
            if from < slots.len() && to < slots.len() {
                let inputs = slots.remove(from);
                slots.insert(to, inputs);
            }
        }
        Ok(())
    }

    pub fn clear_strip(&mut self, strip: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::ClearStrip { strip })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(chains) = self.state_proxies.get_mut(strip) {
            for slots in chains.iter_mut() {
                slots.clear();
            }
        }
        if let Some(chains) = self.plugin_handles.get_mut(strip) {
            for slots in chains.iter_mut() {
                slots.clear();
            }
        }
        if let Some(chains) = self.plugin_outputs.get_mut(strip) {
            for slots in chains.iter_mut() {
                slots.clear();
            }
        }
        if let Some(chains) = self.plugin_ui_inputs.get_mut(strip) {
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
        let outputs = plugin.outputs();
        let ui_inputs = plugin.ui_inputs();
        self.commands
            .push(EngineCommand::AddBusPlugin { bus, plugin })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.bus_state_proxies.get_mut(bus) {
            if slots.len() < MAX_PLUGINS {
                slots.push(proxy);
            }
        }
        if let Some(slots) = self.bus_plugin_handles.get_mut(bus) {
            if slots.len() < MAX_PLUGINS {
                slots.push(Some(handle));
            }
        }
        if let Some(slots) = self.bus_plugin_outputs.get_mut(bus) {
            if slots.len() < MAX_PLUGINS {
                slots.push(outputs);
            }
        }
        if let Some(slots) = self.bus_plugin_ui_inputs.get_mut(bus) {
            if slots.len() < MAX_PLUGINS {
                slots.push(ui_inputs);
            }
        }
        Ok(())
    }

    pub fn remove_bus_plugin(&mut self, bus: usize, slot: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::RemoveBusPlugin { bus, slot })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.bus_state_proxies.get_mut(bus) {
            if slot < slots.len() {
                slots.remove(slot);
            }
        }
        if let Some(slots) = self.bus_plugin_handles.get_mut(bus) {
            if slot < slots.len() {
                slots.remove(slot);
            }
        }
        if let Some(slots) = self.bus_plugin_outputs.get_mut(bus) {
            if slot < slots.len() {
                slots.remove(slot);
            }
        }
        if let Some(slots) = self.bus_plugin_ui_inputs.get_mut(bus) {
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
        if let Some(slots) = self.bus_state_proxies.get_mut(bus) {
            if from < slots.len() && to < slots.len() {
                let proxy = slots.remove(from);
                slots.insert(to, proxy);
            }
        }
        if let Some(slots) = self.bus_plugin_handles.get_mut(bus) {
            if from < slots.len() && to < slots.len() {
                let handle = slots.remove(from);
                slots.insert(to, handle);
            }
        }
        if let Some(slots) = self.bus_plugin_outputs.get_mut(bus) {
            if from < slots.len() && to < slots.len() {
                let outputs = slots.remove(from);
                slots.insert(to, outputs);
            }
        }
        if let Some(slots) = self.bus_plugin_ui_inputs.get_mut(bus) {
            if from < slots.len() && to < slots.len() {
                let inputs = slots.remove(from);
                slots.insert(to, inputs);
            }
        }
        Ok(())
    }

    pub fn clear_bus_plugins(&mut self, bus: usize) -> Result<()> {
        self.commands
            .push(EngineCommand::ClearBus { bus })
            .map_err(|_| anyhow!("engine command queue full"))?;
        if let Some(slots) = self.bus_state_proxies.get_mut(bus) {
            slots.clear();
        }
        if let Some(slots) = self.bus_plugin_handles.get_mut(bus) {
            slots.clear();
        }
        if let Some(slots) = self.bus_plugin_outputs.get_mut(bus) {
            slots.clear();
        }
        if let Some(slots) = self.bus_plugin_ui_inputs.get_mut(bus) {
            slots.clear();
        }
        Ok(())
    }

    pub fn bus_state_proxy(&self, bus: usize, slot: usize) -> Option<SlotState> {
        self.bus_state_proxies
            .get(bus)
            .and_then(|slots| slots.get(slot))
            .and_then(|proxy| proxy.clone())
    }

    pub fn bus_plugin_handle(&self, bus: usize, slot: usize) -> Option<*mut std::ffi::c_void> {
        self.bus_plugin_handles
            .get(bus)
            .and_then(|slots| slots.get(slot))
            .and_then(|handle| *handle)
            .map(|handle| handle as *mut std::ffi::c_void)
    }

    pub fn bus_plugin_outputs(&self, bus: usize, slot: usize) -> Option<Arc<SharedOutputs>> {
        self.bus_plugin_outputs
            .get(bus)
            .and_then(|slots| slots.get(slot))
            .and_then(|outputs| outputs.clone())
    }

    pub fn bus_plugin_ui_inputs(&self, bus: usize, slot: usize) -> Option<Arc<SharedUiInputs>> {
        self.bus_plugin_ui_inputs
            .get(bus)
            .and_then(|slots| slots.get(slot))
            .and_then(|inputs| inputs.clone())
    }

    pub fn add_strip_ports(&mut self, strip: usize) -> Result<()> {
        if strip >= self.strip_count {
            return Ok(());
        }
        if self.strip_ports.get(strip).is_some_and(|ports| ports.is_some()) {
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
        model.midi_connections = Some(self.midi_port.get_connections());

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
            restore_input(client, &self.midi_port, "midi_in", "midi in", sources, &mut missing);
        }

        for strip in &model.strips {
            let Some(connections) = &strip.connections else {
                continue;
            };
            let index = strip.id.0 as usize;
            let Some((in_l, in_r)) = self.strip_ports.get(index).and_then(Option::as_ref) else {
                continue;
            };
            restore_input(client, in_l, "in_l", &strip.name, &connections.in_l, &mut missing);
            restore_input(client, in_r, "in_r", &strip.name, &connections.in_r, &mut missing);
        }

        for bus in &model.buses {
            let Some(connections) = &bus.connections else {
                continue;
            };
            let index = bus.id.0 as usize;
            let Some((out_l, out_r)) = self.bus_ports.get(index).and_then(Option::as_ref) else {
                continue;
            };
            restore_output(client, out_l, "out_l", &bus.name, &connections.out_l, &mut missing);
            restore_output(client, out_r, "out_r", &bus.name, &connections.out_r, &mut missing);
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
            restore_input(client, ret_l, "ret_l", &bus.name, &connections.ret_l, &mut missing);
            restore_input(client, ret_r, "ret_r", &bus.name, &connections.ret_r, &mut missing);
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
                if let Some((ret_l, ret_r)) = self.bus_return_ports.get(index).and_then(Option::as_ref)
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
        self.state_proxies
            .get(strip)
            .and_then(|chains| chains.get(chain))
            .and_then(|slots| slots.get(slot))
            .and_then(|proxy| proxy.clone())
    }

    pub fn plugin_handle(
        &self,
        strip: usize,
        chain: usize,
        slot: usize,
    ) -> Option<*mut std::ffi::c_void> {
        self.plugin_handles
            .get(strip)
            .and_then(|chains| chains.get(chain))
            .and_then(|slots| slots.get(slot))
            .and_then(|handle| *handle)
            .map(|handle| handle as *mut std::ffi::c_void)
    }

    pub fn plugin_outputs(
        &self,
        strip: usize,
        chain: usize,
        slot: usize,
    ) -> Option<Arc<SharedOutputs>> {
        self.plugin_outputs
            .get(strip)
            .and_then(|chains| chains.get(chain))
            .and_then(|slots| slots.get(slot))
            .and_then(|outputs| outputs.clone())
    }

    pub fn plugin_ui_inputs(
        &self,
        strip: usize,
        chain: usize,
        slot: usize,
    ) -> Option<Arc<SharedUiInputs>> {
        self.plugin_ui_inputs
            .get(strip)
            .and_then(|chains| chains.get(chain))
            .and_then(|slots| slots.get(slot))
            .and_then(|inputs| inputs.clone())
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
        self.state_proxies = built.strip_proxies;
        self.plugin_handles = built.strip_handles;
        self.plugin_outputs = built.strip_outputs;
        self.plugin_ui_inputs = built.strip_ui_inputs;
        self.bus_state_proxies = built.bus_proxies;
        self.bus_plugin_handles = built.bus_handles;
        self.bus_plugin_outputs = built.bus_outputs;
        self.bus_plugin_ui_inputs = built.bus_ui_inputs;
        Ok(())
    }

    pub fn drain_garbage(&mut self) {
        while self.garbage.pop().is_ok() {}
        while self.garbage_chains.pop().is_ok() {}
        while self.garbage_bus_chains.pop().is_ok() {}
        while self.garbage_nested.pop().is_ok() {}
        while self.garbage_delays.pop().is_ok() {}
        while self.garbage_order.pop().is_ok() {}
    }
}

fn set_controls(
    instance: &mut dyn PluginNode,
    params: &SharedParams,
    strip: usize,
    chain: usize,
    slot: usize,
) {
    let controls = instance.control_count().min(MAX_PARAMS);
    for param in 0..controls {
        instance.set_control(param, params.plugin_param(strip, chain, slot, param));
    }
}

fn set_bus_controls(
    instance: &mut dyn PluginNode,
    params: &SharedParams,
    bus: usize,
    slot: usize,
) {
    let controls = instance.control_count().min(MAX_PARAMS);
    for param in 0..controls {
        instance.set_control(param, params.bus_plugin_param(bus, slot, param));
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
            if pre { &pre_snapshot[source] } else { &post_snapshot[source] }
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
    let mut strip_proxies: Vec<StripProxies> = (0..strip_count)
        .map(|_| std::array::from_fn(|_| Vec::with_capacity(MAX_PLUGINS)))
        .collect();
    let mut strip_handles: Vec<StripHandles> = (0..strip_count)
        .map(|_| std::array::from_fn(|_| Vec::with_capacity(MAX_PLUGINS)))
        .collect();
    let mut strip_outputs: Vec<StripOutputs> = (0..strip_count)
        .map(|_| std::array::from_fn(|_| Vec::with_capacity(MAX_PLUGINS)))
        .collect();
    let mut strip_ui_inputs: Vec<StripUiInputs> = (0..strip_count)
        .map(|_| std::array::from_fn(|_| Vec::with_capacity(MAX_PLUGINS)))
        .collect();
    let mut bus_chains: Vec<Vec<SlotPlugin>> = (0..bus_count)
        .map(|_| Vec::with_capacity(MAX_PLUGINS))
        .collect();
    let mut bus_proxies: Vec<Vec<Option<SlotState>>> = (0..bus_count).map(|_| Vec::new()).collect();
    let mut bus_handles: Vec<Vec<Option<usize>>> = (0..bus_count).map(|_| Vec::new()).collect();
    let mut bus_outputs: Vec<Vec<Option<Arc<SharedOutputs>>>> =
        (0..bus_count).map(|_| Vec::new()).collect();
    let mut bus_ui_inputs: Vec<Vec<Option<Arc<SharedUiInputs>>>> =
        (0..bus_count).map(|_| Vec::new()).collect();

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
                        let outputs = slot_plugin.outputs();
                        let ui_inputs = slot_plugin.ui_inputs();
                        strip_chains[index][chain].push(slot_plugin);
                        strip_proxies[index][chain].push(proxy);
                        strip_handles[index][chain].push(Some(handle));
                        strip_outputs[index][chain].push(outputs);
                        strip_ui_inputs[index][chain].push(ui_inputs);
                    }
                    None => {
                        plugin.missing = true;
                        strip_chains[index][chain].push(SlotPlugin::Missing);
                        strip_proxies[index][chain].push(None);
                        strip_handles[index][chain].push(None);
                        strip_outputs[index][chain].push(None);
                        strip_ui_inputs[index][chain].push(None);
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
                    let outputs = slot_plugin.outputs();
                    let ui_inputs = slot_plugin.ui_inputs();
                    bus_chains[index].push(slot_plugin);
                    bus_proxies[index].push(proxy);
                    bus_handles[index].push(Some(handle));
                    bus_outputs[index].push(outputs);
                    bus_ui_inputs[index].push(ui_inputs);
                }
                None => {
                    plugin.missing = true;
                    bus_chains[index].push(SlotPlugin::Missing);
                    bus_proxies[index].push(None);
                    bus_handles[index].push(None);
                    bus_outputs[index].push(None);
                    bus_ui_inputs[index].push(None);
                }
            }
            kept.push(plugin);
        }
        bus.plugins = kept;
    }

    Built {
        strip_chains,
        strip_proxies,
        strip_handles,
        strip_outputs,
        strip_ui_inputs,
        bus_chains,
        bus_proxies,
        bus_handles,
        bus_outputs,
        bus_ui_inputs,
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

fn register_strip_ports(
    client: &Client,
    strip: usize,
) -> Result<(Port<AudioIn>, Port<AudioIn>)> {
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

fn rename_port(
    client: *mut jack::jack_sys::jack_client_t,
    port: &mut Port<Unowned>,
    name: &str,
) {
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
        let _ = client.connect_ports_by_name(
            &format!("{client_name}:bus0_out_l"),
            "system:playback_1",
        );
        let _ = client.connect_ports_by_name(
            &format!("{client_name}:bus0_out_r"),
            "system:playback_2",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibemix_core::ids::StripId;
    use vibemix_core::model::{MixerModel, Strip};

    #[test]
    fn slug_replaces_separators_and_trims() {
        assert_eq!(port_slug("  ch 1 "), "ch_1");
        assert_eq!(port_slug("drums:kit"), "drums_kit");
        assert_eq!(port_slug("a   b"), "a_b");
        assert_eq!(port_slug("___"), "");
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

        let pre = vec![[vec![1.0f32; 4], vec![1.0; 4]], [vec![2.0; 4], vec![2.0; 4]]];
        let post = vec![[vec![10.0f32; 4], vec![10.0; 4]], [vec![20.0; 4], vec![20.0; 4]]];
        let zeros = vec![[vec![0.0f32; 4], vec![0.0; 4]], [vec![0.0; 4], vec![0.0; 4]]];
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
