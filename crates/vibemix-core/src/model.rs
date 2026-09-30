use serde::{Deserialize, Serialize};

use crate::ids::{BusId, PluginId, StripId};
use crate::params::{Gain, Pan};

/// Plugin formats the mixer can host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PluginFormat {
    #[default]
    Lv2,
    Vst3,
}

impl PluginFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            PluginFormat::Lv2 => "lv2",
            PluginFormat::Vst3 => "vst3",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSlot {
    pub id: PluginId,
    /// Format-specific identifier: an LV2 URI or a VST3 class id.
    pub uri: String,
    #[serde(default)]
    pub format: PluginFormat,
    /// `.vst3` bundle path (VST3 only; unused by LV2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub params: Vec<f32>,
    #[serde(default)]
    pub state: String,
    pub bypassed: bool,
    /// Set when the plugin could not be loaded (not installed / failed to
    /// instantiate). Kept in the session, bypassed in the graph, shown red.
    #[serde(default, skip_serializing_if = "is_false")]
    pub missing: bool,
    /// MIDI channel (0-15) this plugin's parameter/bypass bindings listen on.
    /// `None` means any channel (omni).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi_channel: Option<u8>,
    /// Strips whose signals feed this plugin's sidechain input (VST3 aux
    /// input). Each entry behaves like a send: per-strip gain and pre/post
    /// tap. Empty = silence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidechain: Vec<SidechainSource>,
    /// Whether this plugin's sidechain input is routed (off by default; some
    /// plugins expose a sidechain that is not used).
    #[serde(default, skip_serializing_if = "is_false")]
    pub sidechain_enabled: bool,
}

/// One strip feeding a plugin's sidechain, with its own level and tap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidechainSource {
    pub strip: StripId,
    #[serde(default = "default_gain")]
    pub gain: Gain,
    #[serde(default)]
    pub pre_fader: bool,
}

impl SidechainSource {
    pub fn new(strip: StripId) -> Self {
        Self {
            strip,
            gain: Gain::UNITY,
            pre_fader: true,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_gain() -> Gain {
    Gain::UNITY
}

impl PluginSlot {
    pub fn new(id: PluginId, uri: impl Into<String>) -> Self {
        Self {
            id,
            uri: uri.into(),
            format: PluginFormat::Lv2,
            bundle: None,
            name: String::new(),
            params: Vec::new(),
            state: String::new(),
            bypassed: false,
            missing: false,
            midi_channel: None,
            sidechain: Vec::new(),
            sidechain_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Send {
    pub bus: BusId,
    pub gain: Gain,
    pub pre_fader: bool,
    /// Alignment delay for this send, in milliseconds (`0..MAX_SEND_DELAY_MS`),
    /// used to phase-align parallel paths (e.g. dry vs. latency-inducing FX).
    #[serde(default)]
    pub delay_ms: f32,
}

pub const MAX_SEND_DELAY_MS: f32 = 500.0;

impl Send {
    pub fn new(bus: BusId) -> Self {
        Self {
            bus,
            gain: Gain::UNITY,
            pre_fader: false,
            delay_ms: 0.0,
        }
    }
}

/// External JACK ports feeding a strip's input ports. `None` on the owning
/// strip means no patch information was saved, so connections are left alone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StripConnections {
    #[serde(default)]
    pub in_l: Vec<String>,
    #[serde(default)]
    pub in_r: Vec<String>,
}

/// External JACK ports fed by a bus's output ports. `None` on the owning bus
/// means no patch information was saved, so connections are left alone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BusConnections {
    #[serde(default)]
    pub out_l: Vec<String>,
    #[serde(default)]
    pub out_r: Vec<String>,
    #[serde(default)]
    pub ret_l: Vec<String>,
    #[serde(default)]
    pub ret_r: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strip {
    pub id: StripId,
    pub name: String,
    pub gain: Gain,
    pub pan: Pan,
    #[serde(default)]
    pub input_gain_db: f32,
    pub mute: bool,
    pub solo: bool,
    pub plugins: Vec<PluginSlot>,
    #[serde(default)]
    pub plugins_post: Vec<PluginSlot>,
    pub sends: Vec<Send>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<StripConnections>,
    /// MIDI channel (0-15) this strip's control bindings listen on. Several
    /// strips/buses may share a channel. `None` means any channel (omni).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi_channel: Option<u8>,
}

impl Strip {
    pub fn new(id: StripId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            gain: Gain::UNITY,
            pan: Pan::CENTER,
            input_gain_db: 0.0,
            mute: false,
            solo: false,
            plugins: Vec::new(),
            plugins_post: Vec::new(),
            sends: Vec::new(),
            connections: None,
            midi_channel: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bus {
    pub id: BusId,
    pub name: String,
    pub gain: Gain,
    pub mute: bool,
    pub plugins: Vec<PluginSlot>,
    #[serde(default)]
    pub mix_to_master: bool,
    /// When true, the bus acts as an external insert: `{slug}_out_l/r` is the
    /// (pre-fader) send to the device and its return is mixed back in before
    /// the fader.
    #[serde(default)]
    pub return_inputs: bool,
    /// Trim on the pre-fader send to the external device (`{slug}_out`).
    #[serde(default)]
    pub send_gain: Gain,
    /// Trim on the device's return (`{slug}_ret`) before it is mixed back in.
    #[serde(default)]
    pub return_gain: Gain,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<BusConnections>,
    /// MIDI channel (0-15) this bus's control bindings listen on. `None` means
    /// any channel (omni).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi_channel: Option<u8>,
}

impl Bus {
    pub fn new(id: BusId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            gain: Gain::UNITY,
            mute: false,
            plugins: Vec::new(),
            mix_to_master: false,
            return_inputs: false,
            send_gain: Gain::UNITY,
            return_gain: Gain::UNITY,
            connections: None,
            midi_channel: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub width: i32,
    pub height: i32,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MixerModel {
    pub name: String,
    pub strips: Vec<Strip>,
    pub buses: Vec<Bus>,
    /// Last window size/maximized state, saved with the session and restored on
    /// load. `None` for sessions that predate it (the GUI keeps its size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowGeometry>,
    /// Beats per bar used for bar counting (host tempo and loop display).
    #[serde(default = "default_beats_per_bar")]
    pub beats_per_bar: f32,
    /// Learned MIDI control-surface bindings.
    #[serde(default)]
    pub midi_bindings: Vec<MidiBinding>,
    /// Top-bar counter/loop display preferences.
    #[serde(default)]
    pub transport_ui: TransportUi,
    /// When true, follow the incoming MIDI clock and pass host tempo/transport
    /// to plugins.
    #[serde(default = "default_true")]
    pub midi_sync: bool,
    /// External JACK ports feeding the engine's MIDI input. `None` means no
    /// patch information was saved (connections are left alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi_connections: Option<Vec<String>>,
}

fn default_beats_per_bar() -> f32 {
    4.0
}

/// Top-bar counter/loop display preferences, saved with the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportUi {
    #[serde(default = "default_true")]
    pub beats: bool,
    #[serde(default)]
    pub time: bool,
    #[serde(default)]
    pub loop_indicator: bool,
    /// Loop length unit: bars when true, beats when false.
    #[serde(default)]
    pub loop_bars: bool,
    #[serde(default = "default_loop_length")]
    pub loop_length: u32,
}

fn default_true() -> bool {
    true
}

fn default_loop_length() -> u32 {
    8
}

impl Default for TransportUi {
    fn default() -> Self {
        Self {
            beats: true,
            time: false,
            loop_indicator: false,
            loop_bars: false,
            loop_length: 8,
        }
    }
}

/// What a MIDI control-surface CC drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiTarget {
    StripGain,
    StripPan,
    StripMute,
    StripSolo,
    BusGain,
    BusMute,
    SendGain,
    SendDelay,
    PluginParam,
    PluginBypass,
}

/// A learned MIDI CC → mixer target binding, saved with the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiBinding {
    /// 0..15, or 16 for omni.
    pub channel: u8,
    pub cc: u8,
    #[serde(default)]
    pub relative: bool,
    pub target: MidiTarget,
    /// `location` for plugin targets (0 strip pre, 1 strip post, 2 bus).
    #[serde(default)]
    pub location: u32,
    /// Strip id, bus id, or send destination bus id.
    #[serde(default)]
    pub target_id: u32,
    #[serde(default)]
    pub slot: u32,
    #[serde(default)]
    pub param: u32,
    /// Parameter range for `PluginParam` targets.
    #[serde(default)]
    pub min: f32,
    #[serde(default)]
    pub max: f32,
}

impl MixerModel {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            strips: Vec::new(),
            buses: Vec::new(),
            window: None,
            beats_per_bar: 4.0,
            midi_bindings: Vec::new(),
            transport_ui: TransportUi::default(),
            midi_sync: true,
            midi_connections: None,
        }
    }

    pub fn default_minimal() -> Self {
        let mut model = Self::new("untitled");
        let mut master = Bus::new(BusId(0), "master");
        master.id = BusId(0);
        model.buses.push(master);

        let mut strip = Strip::new(StripId(0), "ch 1");
        strip.sends.push(Send::new(BusId(0)));
        model.strips.push(strip);
        model
    }

    pub fn strip(&self, id: StripId) -> Option<&Strip> {
        self.strips.iter().find(|strip| strip.id == id)
    }

    pub fn strip_mut(&mut self, id: StripId) -> Option<&mut Strip> {
        self.strips.iter_mut().find(|strip| strip.id == id)
    }

    pub fn bus(&self, id: BusId) -> Option<&Bus> {
        self.buses.iter().find(|bus| bus.id == id)
    }

    pub fn bus_mut(&mut self, id: BusId) -> Option<&mut Bus> {
        self.buses.iter_mut().find(|bus| bus.id == id)
    }

    /// Moves the strip with `id` to `to_index` (clamped). Reordering only
    /// changes display order; engine parameters and ports are keyed by id.
    pub fn move_strip(&mut self, id: StripId, to_index: usize) {
        let Some(current) = self.strips.iter().position(|strip| strip.id == id) else {
            return;
        };
        let target = to_index.min(self.strips.len() - 1);
        if current == target {
            return;
        }
        let strip = self.strips.remove(current);
        self.strips.insert(target, strip);
    }

    /// Moves the bus with `id` to `to_index`. The master (bus 0) is pinned to
    /// the back (rightmost) and cannot be moved; other buses may only occupy the
    /// slots before it.
    pub fn move_bus(&mut self, id: BusId, to_index: usize) {
        if id == BusId(0) || self.buses.len() < 2 {
            return;
        }
        let Some(current) = self.buses.iter().position(|bus| bus.id == id) else {
            return;
        };
        let target = to_index.min(self.buses.len() - 2);
        if current == target {
            return;
        }
        let bus = self.buses.remove(current);
        self.buses.insert(target, bus);
    }

    /// Ensures the master (bus 0) sits last in display order. Called after
    /// loading a session so the GUI always shows it rightmost.
    pub fn normalize_buses(&mut self) {
        let Some(current) = self.buses.iter().position(|bus| bus.id == BusId(0)) else {
            return;
        };
        if current == self.buses.len() - 1 {
            return;
        }
        let master = self.buses.remove(current);
        self.buses.push(master);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_roundtrip() {
        let mut model = MixerModel::default_minimal();
        model.buses[0].connections = Some(BusConnections {
            out_l: vec!["system:playback_1".to_string()],
            out_r: vec!["system:playback_2".to_string()],
            ret_l: vec!["external:fx_out_l".to_string()],
            ret_r: vec![],
        });
        model.buses[0].mix_to_master = true;
        model.buses[0].return_inputs = true;
        model.buses[0].send_gain = Gain(0.5);
        model.buses[0].return_gain = Gain(0.75);
        model.strips[0].input_gain_db = 12.0;
        model.strips[0].midi_channel = Some(3);
        model.buses[0].midi_channel = Some(5);
        model.strips[0]
            .plugins_post
            .push(PluginSlot::new(PluginId(0), "http://example.com/post"));
        model.strips[0].sends[0].delay_ms = 12.5;
        model.strips[0].connections = Some(StripConnections {
            in_l: vec!["system:capture_1".to_string(), "app:out_l".to_string()],
            in_r: vec!["system:capture_2".to_string()],
        });
        model.window = Some(WindowGeometry {
            width: 1280,
            height: 720,
            maximized: true,
        });

        let text = toml::to_string(&model).expect("serialize");
        let parsed: MixerModel = toml::from_str(&text).expect("deserialize");
        assert_eq!(parsed.name, "untitled");
        assert_eq!(parsed.strips.len(), 1);
        assert_eq!(parsed.buses.len(), 1);
        assert_eq!(parsed.strips[0].name, "ch 1");
        assert_eq!(parsed.buses[0].name, "master");
        assert_eq!(parsed.strips[0].input_gain_db, 12.0);
        assert_eq!(parsed.strips[0].plugins_post.len(), 1);
        assert_eq!(
            parsed.strips[0].plugins_post[0].uri,
            "http://example.com/post"
        );
        assert!(parsed.buses[0].mix_to_master);
        assert!(parsed.buses[0].return_inputs);
        assert_eq!(parsed.buses[0].send_gain, Gain(0.5));
        assert_eq!(parsed.buses[0].return_gain, Gain(0.75));
        assert_eq!(parsed.strips[0].midi_channel, Some(3));
        assert_eq!(parsed.buses[0].midi_channel, Some(5));
        assert_eq!(parsed.strips[0].sends[0].delay_ms, 12.5);
        assert_eq!(
            parsed.buses[0]
                .connections
                .as_ref()
                .map(|c| c.ret_l.len()),
            Some(1)
        );
        assert_eq!(
            parsed.strips[0].connections.as_ref().map(|c| c.in_l.len()),
            Some(2)
        );
        assert_eq!(
            parsed.buses[0]
                .connections
                .as_ref()
                .map(|c| c.out_l.as_slice()),
            Some(["system:playback_1".to_string()].as_slice())
        );
        assert_eq!(
            parsed.window,
            Some(WindowGeometry {
                width: 1280,
                height: 720,
                maximized: true,
            })
        );
    }

    #[test]
    fn connections_absent_by_default() {
        let model = MixerModel::default_minimal();
        let text = toml::to_string(&model).expect("serialize");
        assert!(!text.contains("connections"));
        let parsed: MixerModel = toml::from_str(&text).expect("deserialize");
        assert!(parsed.strips[0].connections.is_none());
        assert!(parsed.buses[0].connections.is_none());
    }

    #[test]
    fn plugin_format_defaults_lv2_and_roundtrips() {
        let mut slot = PluginSlot::new(PluginId(0), "http://example.com/x");
        assert_eq!(slot.format, PluginFormat::Lv2);

        slot.format = PluginFormat::Vst3;
        let text = toml::to_string(&slot).expect("serialize");
        let parsed: PluginSlot = toml::from_str(&text).expect("deserialize");
        assert_eq!(parsed.format, PluginFormat::Vst3);

        // Sessions written before formats existed default to LV2.
        let legacy = "id = 0\nuri = \"http://example.com/y\"\nbypassed = false\n";
        let parsed: PluginSlot = toml::from_str(legacy).expect("deserialize legacy");
        assert_eq!(parsed.format, PluginFormat::Lv2);
    }

    #[test]
    fn reorder_changes_order_only() {
        let mut model = MixerModel::default_minimal();
        model.strips.push(Strip::new(StripId(1), "ch 2"));
        model.strips.push(Strip::new(StripId(2), "ch 3"));
        model.buses.push(Bus::new(BusId(1), "aux"));
        model.buses.push(Bus::new(BusId(2), "aux2"));
        model.normalize_buses();
        assert_eq!(
            model.buses.iter().map(|b| b.id.0).collect::<Vec<_>>(),
            vec![1, 2, 0]
        );

        model.move_strip(StripId(0), 2);
        assert_eq!(
            model.strips.iter().map(|s| s.id.0).collect::<Vec<_>>(),
            vec![1, 2, 0]
        );

        model.move_bus(BusId(2), 0);
        assert_eq!(
            model.buses.iter().map(|b| b.id.0).collect::<Vec<_>>(),
            vec![2, 1, 0]
        );

        // The master cannot be moved.
        model.move_bus(BusId(0), 0);
        assert_eq!(
            model.buses.iter().map(|b| b.id.0).collect::<Vec<_>>(),
            vec![2, 1, 0]
        );

        model.move_strip(StripId(0), 0);
        assert_eq!(
            model.strips.iter().map(|s| s.id.0).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }
}
