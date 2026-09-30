use rosc::{OscMessage, OscPacket, OscType};

use crate::address as addr;

#[derive(Debug, Clone, PartialEq)]
// `Meters` is the bulk streaming message and is intentionally much larger than
// the other variants; the enum lives on the stack briefly per control message.
#[allow(clippy::large_enum_variant)]
pub enum ControlMessage {
    SetStripGain {
        strip: u32,
        gain: f32,
    },
    SetStripMute {
        strip: u32,
        mute: bool,
    },
    SetStripSolo {
        strip: u32,
        solo: bool,
    },
    SetStripPan {
        strip: u32,
        pan: f32,
    },
    SetStripInputGain {
        strip: u32,
        db: f32,
    },
    SetStripName {
        strip: u32,
        name: String,
    },
    MoveStrip {
        strip: u32,
        to_index: u32,
    },
    SetBusGain {
        bus: u32,
        gain: f32,
    },
    SetBusMute {
        bus: u32,
        mute: bool,
    },
    SetBusName {
        bus: u32,
        name: String,
    },
    MoveBus {
        bus: u32,
        to_index: u32,
    },
    SetBusMixMaster {
        bus: u32,
        on: bool,
    },
    SetBusReturn {
        bus: u32,
        on: bool,
    },
    SetBusSendGain {
        bus: u32,
        gain: f32,
    },
    SetBusReturnGain {
        bus: u32,
        gain: f32,
    },
    SetSendGain {
        strip: u32,
        bus: u32,
        gain: f32,
    },
    SetSendDelay {
        strip: u32,
        bus: u32,
        ms: f32,
    },
    SetSendPreFader {
        strip: u32,
        bus: u32,
        pre: bool,
    },
    AddStrip,
    AddBus,
    RemoveStrip {
        strip: u32,
    },
    RemoveBus {
        bus: u32,
    },
    DuplicateStrip {
        strip: u32,
    },
    DuplicateBus {
        bus: u32,
    },
    AddPlugin {
        location: u32,
        target: u32,
        uri: String,
    },
    RemovePlugin {
        location: u32,
        target: u32,
        slot: u32,
    },
    MovePlugin {
        location: u32,
        target: u32,
        from: u32,
        to: u32,
    },
    SetPluginParam {
        location: u32,
        target: u32,
        slot: u32,
        param: u32,
        value: f32,
    },
    SetPluginBypass {
        location: u32,
        target: u32,
        slot: u32,
        bypassed: bool,
    },
    SetPluginSidechainEnabled {
        location: u32,
        target: u32,
        slot: u32,
        enabled: bool,
    },
    SetPluginSidechain {
        location: u32,
        target: u32,
        slot: u32,
        /// Source strip.
        strip: i32,
        gain: f32,
    },
    SetPluginSidechainPre {
        location: u32,
        target: u32,
        slot: u32,
        strip: u32,
        pre: bool,
    },
    OpenPluginUi {
        location: u32,
        target: u32,
        slot: u32,
    },
    ClosePluginUi {
        location: u32,
        target: u32,
        slot: u32,
    },
    CopyPlugin {
        location: u32,
        target: u32,
        slot: u32,
    },
    CutPlugin {
        location: u32,
        target: u32,
        slot: u32,
    },
    PastePlugin {
        location: u32,
        target: u32,
    },
    Clipboard {
        has: bool,
        location: u32,
        name: String,
    },
    Catalog {
        json: String,
    },
    RescanPlugins,
    RetestPlugins,
    CancelPluginScan,
    PluginScan {
        running: bool,
        done: u32,
        total: u32,
    },
    QueryState,
    State {
        json: String,
    },
    SaveSession {
        path: String,
        width: i32,
        height: i32,
        maximized: bool,
    },
    LoadSession {
        path: String,
    },
    NewSession,
    SessionResult {
        ok: bool,
        path: String,
        error: String,
    },
    Undo,
    Redo,
    EditState {
        undo_depth: u32,
        redo_depth: u32,
        undo_label: String,
        redo_label: String,
    },
    Clock {
        locked: bool,
        playing: bool,
        bpm: f32,
        beats: f32,
        seconds: f32,
        beats_per_bar: f32,
    },
    SetBeatsPerBar {
        value: f32,
    },
    SetTransportUi {
        json: String,
    },
    MidiLearn {
        kind: u32,
        location: u32,
        target: u32,
        slot: u32,
        param: u32,
        min: f32,
        max: f32,
    },
    MidiLearnCancel,
    MidiBindingsClear,
    SetMidiSync {
        on: bool,
    },
    SetStripMidiChannel {
        strip: u32,
        channel: u32,
    },
    SetBusMidiChannel {
        bus: u32,
        channel: u32,
    },
    SetPluginMidiChannel {
        location: u32,
        target: u32,
        slot: u32,
        channel: u32,
    },
    MidiBind {
        kind: u32,
        location: u32,
        target: u32,
        slot: u32,
        param: u32,
        min: f32,
        max: f32,
        channel: u32,
        cc: u32,
    },
    MidiUnbind {
        kind: u32,
        location: u32,
        target: u32,
        slot: u32,
        param: u32,
    },
    MissingConnections {
        json: String,
    },
    Meters {
        strips_in_l: Vec<f32>,
        strips_in_r: Vec<f32>,
        strips_gain_l: Vec<f32>,
        strips_gain_r: Vec<f32>,
        strips_out_l: Vec<f32>,
        strips_out_r: Vec<f32>,
        buses_in_l: Vec<f32>,
        buses_in_r: Vec<f32>,
        buses_gain_l: Vec<f32>,
        buses_gain_r: Vec<f32>,
        buses_ret_l: Vec<f32>,
        buses_ret_r: Vec<f32>,
        buses_l: Vec<f32>,
        buses_r: Vec<f32>,
    },
}

impl ControlMessage {
    pub fn to_packet(&self) -> OscPacket {
        use ControlMessage::*;

        let message = match self {
            SetStripGain { strip, gain } => OscMessage {
                addr: addr::STRIP_GAIN.to_string(),
                args: vec![OscType::Int(*strip as i32), OscType::Float(*gain)],
            },
            SetStripMute { strip, mute } => OscMessage {
                addr: addr::STRIP_MUTE.to_string(),
                args: vec![OscType::Int(*strip as i32), OscType::Int(*mute as i32)],
            },
            SetStripSolo { strip, solo } => OscMessage {
                addr: addr::STRIP_SOLO.to_string(),
                args: vec![OscType::Int(*strip as i32), OscType::Int(*solo as i32)],
            },
            SetStripPan { strip, pan } => OscMessage {
                addr: addr::STRIP_PAN.to_string(),
                args: vec![OscType::Int(*strip as i32), OscType::Float(*pan)],
            },
            SetStripInputGain { strip, db } => OscMessage {
                addr: addr::STRIP_INPUT_GAIN.to_string(),
                args: vec![OscType::Int(*strip as i32), OscType::Float(*db)],
            },
            SetStripName { strip, name } => OscMessage {
                addr: addr::STRIP_NAME.to_string(),
                args: vec![OscType::Int(*strip as i32), OscType::String(name.clone())],
            },
            MoveStrip { strip, to_index } => OscMessage {
                addr: addr::STRIP_MOVE.to_string(),
                args: vec![OscType::Int(*strip as i32), OscType::Int(*to_index as i32)],
            },
            SetBusGain { bus, gain } => OscMessage {
                addr: addr::BUS_GAIN.to_string(),
                args: vec![OscType::Int(*bus as i32), OscType::Float(*gain)],
            },
            SetBusMute { bus, mute } => OscMessage {
                addr: addr::BUS_MUTE.to_string(),
                args: vec![OscType::Int(*bus as i32), OscType::Int(*mute as i32)],
            },
            SetBusName { bus, name } => OscMessage {
                addr: addr::BUS_NAME.to_string(),
                args: vec![OscType::Int(*bus as i32), OscType::String(name.clone())],
            },
            MoveBus { bus, to_index } => OscMessage {
                addr: addr::BUS_MOVE.to_string(),
                args: vec![OscType::Int(*bus as i32), OscType::Int(*to_index as i32)],
            },
            SetBusMixMaster { bus, on } => OscMessage {
                addr: addr::BUS_MIX_MASTER.to_string(),
                args: vec![OscType::Int(*bus as i32), OscType::Int(*on as i32)],
            },
            SetBusReturn { bus, on } => OscMessage {
                addr: addr::BUS_RETURN.to_string(),
                args: vec![OscType::Int(*bus as i32), OscType::Int(*on as i32)],
            },
            SetBusSendGain { bus, gain } => OscMessage {
                addr: addr::BUS_SEND_GAIN.to_string(),
                args: vec![OscType::Int(*bus as i32), OscType::Float(*gain)],
            },
            SetBusReturnGain { bus, gain } => OscMessage {
                addr: addr::BUS_RETURN_GAIN.to_string(),
                args: vec![OscType::Int(*bus as i32), OscType::Float(*gain)],
            },
            SetSendGain { strip, bus, gain } => OscMessage {
                addr: addr::SEND_GAIN.to_string(),
                args: vec![
                    OscType::Int(*strip as i32),
                    OscType::Int(*bus as i32),
                    OscType::Float(*gain),
                ],
            },
            SetSendDelay { strip, bus, ms } => OscMessage {
                addr: addr::SEND_DELAY.to_string(),
                args: vec![
                    OscType::Int(*strip as i32),
                    OscType::Int(*bus as i32),
                    OscType::Float(*ms),
                ],
            },
            SetSendPreFader { strip, bus, pre } => OscMessage {
                addr: addr::SEND_PRE_FADER.to_string(),
                args: vec![
                    OscType::Int(*strip as i32),
                    OscType::Int(*bus as i32),
                    OscType::Int(*pre as i32),
                ],
            },
            AddStrip => OscMessage {
                addr: addr::STRIP_ADD.to_string(),
                args: Vec::new(),
            },
            AddBus => OscMessage {
                addr: addr::BUS_ADD.to_string(),
                args: Vec::new(),
            },
            RemoveStrip { strip } => OscMessage {
                addr: addr::STRIP_REMOVE.to_string(),
                args: vec![OscType::Int(*strip as i32)],
            },
            RemoveBus { bus } => OscMessage {
                addr: addr::BUS_REMOVE.to_string(),
                args: vec![OscType::Int(*bus as i32)],
            },
            DuplicateStrip { strip } => OscMessage {
                addr: addr::STRIP_DUPLICATE.to_string(),
                args: vec![OscType::Int(*strip as i32)],
            },
            DuplicateBus { bus } => OscMessage {
                addr: addr::BUS_DUPLICATE.to_string(),
                args: vec![OscType::Int(*bus as i32)],
            },
            AddPlugin {
                location,
                target,
                uri,
            } => OscMessage {
                addr: addr::PLUGIN_ADD.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::String(uri.clone()),
                ],
            },
            RemovePlugin {
                location,
                target,
                slot,
            } => OscMessage {
                addr: addr::PLUGIN_REMOVE.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                ],
            },
            MovePlugin {
                location,
                target,
                from,
                to,
            } => OscMessage {
                addr: addr::PLUGIN_MOVE.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*from as i32),
                    OscType::Int(*to as i32),
                ],
            },
            SetPluginParam {
                location,
                target,
                slot,
                param,
                value,
            } => OscMessage {
                addr: addr::PLUGIN_PARAM.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                    OscType::Int(*param as i32),
                    OscType::Float(*value),
                ],
            },
            Catalog { json } => OscMessage {
                addr: addr::CATALOG.to_string(),
                args: vec![OscType::String(json.clone())],
            },
            RescanPlugins => OscMessage {
                addr: addr::PLUGINS_RESCAN.to_string(),
                args: Vec::new(),
            },
            RetestPlugins => OscMessage {
                addr: addr::PLUGINS_RETEST.to_string(),
                args: Vec::new(),
            },
            CancelPluginScan => OscMessage {
                addr: addr::PLUGIN_SCAN_CANCEL.to_string(),
                args: Vec::new(),
            },
            PluginScan {
                running,
                done,
                total,
            } => OscMessage {
                addr: addr::PLUGIN_SCAN.to_string(),
                args: vec![
                    OscType::Int(*running as i32),
                    OscType::Int(*done as i32),
                    OscType::Int(*total as i32),
                ],
            },
            SetPluginBypass {
                location,
                target,
                slot,
                bypassed,
            } => OscMessage {
                addr: addr::PLUGIN_BYPASS.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                    OscType::Int(*bypassed as i32),
                ],
            },
            SetPluginSidechainEnabled {
                location,
                target,
                slot,
                enabled,
            } => OscMessage {
                addr: addr::PLUGIN_SIDECHAIN_ENABLED.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                    OscType::Int(*enabled as i32),
                ],
            },
            SetPluginSidechain {
                location,
                target,
                slot,
                strip,
                gain,
            } => OscMessage {
                addr: addr::PLUGIN_SIDECHAIN_SEND.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                    OscType::Int(*strip),
                    OscType::Float(*gain),
                ],
            },
            SetPluginSidechainPre {
                location,
                target,
                slot,
                strip,
                pre,
            } => OscMessage {
                addr: addr::PLUGIN_SIDECHAIN_PRE.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                    OscType::Int(*strip as i32),
                    OscType::Int(*pre as i32),
                ],
            },
            OpenPluginUi {
                location,
                target,
                slot,
            } => OscMessage {
                addr: addr::PLUGIN_UI.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                ],
            },
            ClosePluginUi {
                location,
                target,
                slot,
            } => OscMessage {
                addr: addr::PLUGIN_UI_CLOSE.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                ],
            },
            CopyPlugin {
                location,
                target,
                slot,
            } => OscMessage {
                addr: addr::PLUGIN_COPY.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                ],
            },
            CutPlugin {
                location,
                target,
                slot,
            } => OscMessage {
                addr: addr::PLUGIN_CUT.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                ],
            },
            PastePlugin { location, target } => OscMessage {
                addr: addr::PLUGIN_PASTE.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                ],
            },
            Clipboard { has, location, name } => OscMessage {
                addr: addr::PLUGIN_CLIPBOARD.to_string(),
                args: vec![
                    OscType::Int(*has as i32),
                    OscType::Int(*location as i32),
                    OscType::String(name.clone()),
                ],
            },
            QueryState => OscMessage {
                addr: addr::STATE_QUERY.to_string(),
                args: Vec::new(),
            },
            State { json } => OscMessage {
                addr: addr::STATE.to_string(),
                args: vec![OscType::String(json.clone())],
            },
            SaveSession {
                path,
                width,
                height,
                maximized,
            } => OscMessage {
                addr: addr::SESSION_SAVE.to_string(),
                args: vec![
                    OscType::String(path.clone()),
                    OscType::Int(*width),
                    OscType::Int(*height),
                    OscType::Int(*maximized as i32),
                ],
            },
            LoadSession { path } => OscMessage {
                addr: addr::SESSION_LOAD.to_string(),
                args: vec![OscType::String(path.clone())],
            },
            NewSession => OscMessage {
                addr: addr::SESSION_NEW.to_string(),
                args: Vec::new(),
            },
            SessionResult { ok, path, error } => OscMessage {
                addr: addr::SESSION_RESULT.to_string(),
                args: vec![
                    OscType::Int(*ok as i32),
                    OscType::String(path.clone()),
                    OscType::String(error.clone()),
                ],
            },
            Undo => OscMessage {
                addr: addr::EDIT_UNDO.to_string(),
                args: Vec::new(),
            },
            Redo => OscMessage {
                addr: addr::EDIT_REDO.to_string(),
                args: Vec::new(),
            },
            EditState {
                undo_depth,
                redo_depth,
                undo_label,
                redo_label,
            } => OscMessage {
                addr: addr::EDIT_STATE.to_string(),
                args: vec![
                    OscType::Int(*undo_depth as i32),
                    OscType::Int(*redo_depth as i32),
                    OscType::String(undo_label.clone()),
                    OscType::String(redo_label.clone()),
                ],
            },
            Clock {
                locked,
                playing,
                bpm,
                beats,
                seconds,
                beats_per_bar,
            } => OscMessage {
                addr: addr::CLOCK.to_string(),
                args: vec![
                    OscType::Int(*locked as i32),
                    OscType::Int(*playing as i32),
                    OscType::Float(*bpm),
                    OscType::Float(*beats),
                    OscType::Float(*seconds),
                    OscType::Float(*beats_per_bar),
                ],
            },
            SetBeatsPerBar { value } => OscMessage {
                addr: addr::CLOCK_BEATS_PER_BAR.to_string(),
                args: vec![OscType::Float(*value)],
            },
            SetTransportUi { json } => OscMessage {
                addr: addr::TRANSPORT_UI.to_string(),
                args: vec![OscType::String(json.clone())],
            },
            MidiLearn {
                kind,
                location,
                target,
                slot,
                param,
                min,
                max,
            } => OscMessage {
                addr: addr::MIDI_LEARN.to_string(),
                args: vec![
                    OscType::Int(*kind as i32),
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                    OscType::Int(*param as i32),
                    OscType::Float(*min),
                    OscType::Float(*max),
                ],
            },
            MidiLearnCancel => OscMessage {
                addr: addr::MIDI_LEARN_CANCEL.to_string(),
                args: Vec::new(),
            },
            MidiBindingsClear => OscMessage {
                addr: addr::MIDI_BINDINGS_CLEAR.to_string(),
                args: Vec::new(),
            },
            SetMidiSync { on } => OscMessage {
                addr: addr::MIDI_SYNC.to_string(),
                args: vec![OscType::Int(*on as i32)],
            },
            SetStripMidiChannel { strip, channel } => OscMessage {
                addr: addr::STRIP_MIDI_CHANNEL.to_string(),
                args: vec![
                    OscType::Int(*strip as i32),
                    OscType::Int(*channel as i32),
                ],
            },
            SetBusMidiChannel { bus, channel } => OscMessage {
                addr: addr::BUS_MIDI_CHANNEL.to_string(),
                args: vec![OscType::Int(*bus as i32), OscType::Int(*channel as i32)],
            },
            SetPluginMidiChannel {
                location,
                target,
                slot,
                channel,
            } => OscMessage {
                addr: addr::PLUGIN_MIDI_CHANNEL.to_string(),
                args: vec![
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                    OscType::Int(*channel as i32),
                ],
            },
            MidiBind {
                kind,
                location,
                target,
                slot,
                param,
                min,
                max,
                channel,
                cc,
            } => OscMessage {
                addr: addr::MIDI_BIND.to_string(),
                args: vec![
                    OscType::Int(*kind as i32),
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                    OscType::Int(*param as i32),
                    OscType::Float(*min),
                    OscType::Float(*max),
                    OscType::Int(*channel as i32),
                    OscType::Int(*cc as i32),
                ],
            },
            MidiUnbind {
                kind,
                location,
                target,
                slot,
                param,
            } => OscMessage {
                addr: addr::MIDI_UNBIND.to_string(),
                args: vec![
                    OscType::Int(*kind as i32),
                    OscType::Int(*location as i32),
                    OscType::Int(*target as i32),
                    OscType::Int(*slot as i32),
                    OscType::Int(*param as i32),
                ],
            },
            MissingConnections { json } => OscMessage {
                addr: addr::CONNECTIONS_MISSING.to_string(),
                args: vec![OscType::String(json.clone())],
            },
            Meters {
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
            } => {
                let strip_count = strips_out_l.len();
                let bus_count = buses_l.len();
                let mut args =
                    Vec::with_capacity(strip_count * 6 + bus_count * 8 + 2);
                args.push(OscType::Int(strip_count as i32));
                for run in [
                    strips_in_l,
                    strips_in_r,
                    strips_gain_l,
                    strips_gain_r,
                    strips_out_l,
                    strips_out_r,
                ] {
                    args.extend(run.iter().map(|level| OscType::Float(*level)));
                }
                args.push(OscType::Int(bus_count as i32));
                for run in [
                    buses_in_l,
                    buses_in_r,
                    buses_gain_l,
                    buses_gain_r,
                    buses_ret_l,
                    buses_ret_r,
                    buses_l,
                    buses_r,
                ] {
                    args.extend(run.iter().map(|level| OscType::Float(*level)));
                }
                OscMessage {
                    addr: addr::METERS.to_string(),
                    args,
                }
            }
        };

        OscPacket::Message(message)
    }

    pub fn from_packet(packet: OscPacket) -> Option<ControlMessage> {
        let message = match packet {
            OscPacket::Message(message) => message,
            OscPacket::Bundle(_) => return None,
        };
        let args = &message.args;

        match message.addr.as_str() {
            addr::STRIP_GAIN => Some(ControlMessage::SetStripGain {
                strip: int(args, 0)? as u32,
                gain: float(args, 1)?,
            }),
            addr::STRIP_MUTE => Some(ControlMessage::SetStripMute {
                strip: int(args, 0)? as u32,
                mute: int(args, 1)? != 0,
            }),
            addr::STRIP_SOLO => Some(ControlMessage::SetStripSolo {
                strip: int(args, 0)? as u32,
                solo: int(args, 1)? != 0,
            }),
            addr::STRIP_PAN => Some(ControlMessage::SetStripPan {
                strip: int(args, 0)? as u32,
                pan: float(args, 1)?,
            }),
            addr::STRIP_INPUT_GAIN => Some(ControlMessage::SetStripInputGain {
                strip: int(args, 0)? as u32,
                db: float(args, 1)?,
            }),
            addr::STRIP_NAME => Some(ControlMessage::SetStripName {
                strip: int(args, 0)? as u32,
                name: string(args, 1)?,
            }),
            addr::STRIP_MOVE => Some(ControlMessage::MoveStrip {
                strip: int(args, 0)? as u32,
                to_index: int(args, 1)? as u32,
            }),
            addr::BUS_GAIN => Some(ControlMessage::SetBusGain {
                bus: int(args, 0)? as u32,
                gain: float(args, 1)?,
            }),
            addr::BUS_MUTE => Some(ControlMessage::SetBusMute {
                bus: int(args, 0)? as u32,
                mute: int(args, 1)? != 0,
            }),
            addr::BUS_NAME => Some(ControlMessage::SetBusName {
                bus: int(args, 0)? as u32,
                name: string(args, 1)?,
            }),
            addr::BUS_MOVE => Some(ControlMessage::MoveBus {
                bus: int(args, 0)? as u32,
                to_index: int(args, 1)? as u32,
            }),
            addr::BUS_MIX_MASTER => Some(ControlMessage::SetBusMixMaster {
                bus: int(args, 0)? as u32,
                on: int(args, 1)? != 0,
            }),
            addr::BUS_RETURN => Some(ControlMessage::SetBusReturn {
                bus: int(args, 0)? as u32,
                on: int(args, 1)? != 0,
            }),
            addr::BUS_SEND_GAIN => Some(ControlMessage::SetBusSendGain {
                bus: int(args, 0)? as u32,
                gain: float(args, 1)?,
            }),
            addr::BUS_RETURN_GAIN => Some(ControlMessage::SetBusReturnGain {
                bus: int(args, 0)? as u32,
                gain: float(args, 1)?,
            }),
            addr::SEND_GAIN => Some(ControlMessage::SetSendGain {
                strip: int(args, 0)? as u32,
                bus: int(args, 1)? as u32,
                gain: float(args, 2)?,
            }),
            addr::SEND_DELAY => Some(ControlMessage::SetSendDelay {
                strip: int(args, 0)? as u32,
                bus: int(args, 1)? as u32,
                ms: float(args, 2)?,
            }),
            addr::SEND_PRE_FADER => Some(ControlMessage::SetSendPreFader {
                strip: int(args, 0)? as u32,
                bus: int(args, 1)? as u32,
                pre: int(args, 2)? != 0,
            }),
            addr::STRIP_ADD => Some(ControlMessage::AddStrip),
            addr::BUS_ADD => Some(ControlMessage::AddBus),
            addr::STRIP_REMOVE => Some(ControlMessage::RemoveStrip {
                strip: int(args, 0)? as u32,
            }),
            addr::BUS_REMOVE => Some(ControlMessage::RemoveBus {
                bus: int(args, 0)? as u32,
            }),
            addr::STRIP_DUPLICATE => Some(ControlMessage::DuplicateStrip {
                strip: int(args, 0)? as u32,
            }),
            addr::BUS_DUPLICATE => Some(ControlMessage::DuplicateBus {
                bus: int(args, 0)? as u32,
            }),
            addr::PLUGIN_ADD => Some(ControlMessage::AddPlugin {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                uri: string(args, 2)?,
            }),
            addr::PLUGIN_REMOVE => Some(ControlMessage::RemovePlugin {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
            }),
            addr::PLUGIN_MOVE => Some(ControlMessage::MovePlugin {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                from: int(args, 2)? as u32,
                to: int(args, 3)? as u32,
            }),
            addr::PLUGIN_PARAM => Some(ControlMessage::SetPluginParam {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
                param: int(args, 3)? as u32,
                value: float(args, 4)?,
            }),
            addr::CATALOG => Some(ControlMessage::Catalog {
                json: string(args, 0)?,
            }),
            addr::PLUGINS_RESCAN => Some(ControlMessage::RescanPlugins),
            addr::PLUGINS_RETEST => Some(ControlMessage::RetestPlugins),
            addr::PLUGIN_SCAN_CANCEL => Some(ControlMessage::CancelPluginScan),
            addr::PLUGIN_SCAN => Some(ControlMessage::PluginScan {
                running: int(args, 0)? != 0,
                done: int(args, 1)? as u32,
                total: int(args, 2)? as u32,
            }),
            addr::PLUGIN_BYPASS => Some(ControlMessage::SetPluginBypass {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
                bypassed: int(args, 3)? != 0,
            }),
            addr::PLUGIN_SIDECHAIN_ENABLED => Some(ControlMessage::SetPluginSidechainEnabled {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
                enabled: int(args, 3)? != 0,
            }),
            addr::PLUGIN_SIDECHAIN_SEND => Some(ControlMessage::SetPluginSidechain {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
                strip: int(args, 3)?,
                gain: float(args, 4)?,
            }),
            addr::PLUGIN_SIDECHAIN_PRE => Some(ControlMessage::SetPluginSidechainPre {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
                strip: int(args, 3)? as u32,
                pre: int(args, 4)? != 0,
            }),
            addr::PLUGIN_UI => Some(ControlMessage::OpenPluginUi {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
            }),
            addr::PLUGIN_UI_CLOSE => Some(ControlMessage::ClosePluginUi {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
            }),
            addr::PLUGIN_COPY => Some(ControlMessage::CopyPlugin {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
            }),
            addr::PLUGIN_CUT => Some(ControlMessage::CutPlugin {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
            }),
            addr::PLUGIN_PASTE => Some(ControlMessage::PastePlugin {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
            }),
            addr::PLUGIN_CLIPBOARD => Some(ControlMessage::Clipboard {
                has: int(args, 0)? != 0,
                location: int(args, 1)? as u32,
                name: string(args, 2)?,
            }),
            addr::STATE_QUERY => Some(ControlMessage::QueryState),
            addr::STATE => Some(ControlMessage::State {
                json: string(args, 0)?,
            }),
            addr::SESSION_SAVE => Some(ControlMessage::SaveSession {
                path: string(args, 0)?,
                width: int(args, 1)?,
                height: int(args, 2)?,
                maximized: int(args, 3)? != 0,
            }),
            addr::SESSION_LOAD => Some(ControlMessage::LoadSession {
                path: string(args, 0)?,
            }),
            addr::SESSION_NEW => Some(ControlMessage::NewSession),
            addr::SESSION_RESULT => Some(ControlMessage::SessionResult {
                ok: int(args, 0)? != 0,
                path: string(args, 1)?,
                error: string(args, 2)?,
            }),
            addr::EDIT_UNDO => Some(ControlMessage::Undo),
            addr::EDIT_REDO => Some(ControlMessage::Redo),
            addr::EDIT_STATE => Some(ControlMessage::EditState {
                undo_depth: int(args, 0)? as u32,
                redo_depth: int(args, 1)? as u32,
                undo_label: string(args, 2)?,
                redo_label: string(args, 3)?,
            }),
            addr::CLOCK => Some(ControlMessage::Clock {
                locked: int(args, 0)? != 0,
                playing: int(args, 1)? != 0,
                bpm: float(args, 2)?,
                beats: float(args, 3)?,
                seconds: float(args, 4)?,
                beats_per_bar: float(args, 5)?,
            }),
            addr::CLOCK_BEATS_PER_BAR => Some(ControlMessage::SetBeatsPerBar {
                value: float(args, 0)?,
            }),
            addr::TRANSPORT_UI => Some(ControlMessage::SetTransportUi {
                json: string(args, 0)?,
            }),
            addr::MIDI_LEARN => Some(ControlMessage::MidiLearn {
                kind: int(args, 0)? as u32,
                location: int(args, 1)? as u32,
                target: int(args, 2)? as u32,
                slot: int(args, 3)? as u32,
                param: int(args, 4)? as u32,
                min: float(args, 5)?,
                max: float(args, 6)?,
            }),
            addr::MIDI_LEARN_CANCEL => Some(ControlMessage::MidiLearnCancel),
            addr::MIDI_BINDINGS_CLEAR => Some(ControlMessage::MidiBindingsClear),
            addr::MIDI_SYNC => Some(ControlMessage::SetMidiSync {
                on: int(args, 0)? != 0,
            }),
            addr::STRIP_MIDI_CHANNEL => Some(ControlMessage::SetStripMidiChannel {
                strip: int(args, 0)? as u32,
                channel: int(args, 1)? as u32,
            }),
            addr::BUS_MIDI_CHANNEL => Some(ControlMessage::SetBusMidiChannel {
                bus: int(args, 0)? as u32,
                channel: int(args, 1)? as u32,
            }),
            addr::PLUGIN_MIDI_CHANNEL => Some(ControlMessage::SetPluginMidiChannel {
                location: int(args, 0)? as u32,
                target: int(args, 1)? as u32,
                slot: int(args, 2)? as u32,
                channel: int(args, 3)? as u32,
            }),
            addr::MIDI_BIND => Some(ControlMessage::MidiBind {
                kind: int(args, 0)? as u32,
                location: int(args, 1)? as u32,
                target: int(args, 2)? as u32,
                slot: int(args, 3)? as u32,
                param: int(args, 4)? as u32,
                min: float(args, 5)?,
                max: float(args, 6)?,
                channel: int(args, 7)? as u32,
                cc: int(args, 8)? as u32,
            }),
            addr::MIDI_UNBIND => Some(ControlMessage::MidiUnbind {
                kind: int(args, 0)? as u32,
                location: int(args, 1)? as u32,
                target: int(args, 2)? as u32,
                slot: int(args, 3)? as u32,
                param: int(args, 4)? as u32,
            }),
            addr::CONNECTIONS_MISSING => Some(ControlMessage::MissingConnections {
                json: string(args, 0)?,
            }),
            addr::METERS => {
                let strip_count = int(args, 0)? as usize;
                let strips_in_l = float_run(args, 1, strip_count)?;
                let strips_in_r = float_run(args, 1 + strip_count, strip_count)?;
                let strips_gain_l = float_run(args, 1 + strip_count * 2, strip_count)?;
                let strips_gain_r = float_run(args, 1 + strip_count * 3, strip_count)?;
                let strips_out_l = float_run(args, 1 + strip_count * 4, strip_count)?;
                let strips_out_r = float_run(args, 1 + strip_count * 5, strip_count)?;
                let bus_count_index = 1 + strip_count * 6;
                let bus_count = int(args, bus_count_index)? as usize;
                let buses_in_l = float_run(args, bus_count_index + 1, bus_count)?;
                let buses_in_r = float_run(args, bus_count_index + 1 + bus_count, bus_count)?;
                let buses_gain_l = float_run(args, bus_count_index + 1 + bus_count * 2, bus_count)?;
                let buses_gain_r = float_run(args, bus_count_index + 1 + bus_count * 3, bus_count)?;
                let buses_ret_l = float_run(args, bus_count_index + 1 + bus_count * 4, bus_count)?;
                let buses_ret_r = float_run(args, bus_count_index + 1 + bus_count * 5, bus_count)?;
                let buses_l = float_run(args, bus_count_index + 1 + bus_count * 6, bus_count)?;
                let buses_r = float_run(args, bus_count_index + 1 + bus_count * 7, bus_count)?;
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
                })
            }
            _ => None,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, rosc::OscError> {
        rosc::encoder::encode(&self.to_packet())
    }

    pub fn decode(bytes: &[u8]) -> Option<ControlMessage> {
        let (_remaining, packet) = rosc::decoder::decode_udp(bytes).ok()?;
        Self::from_packet(packet)
    }
}

fn int(args: &[OscType], index: usize) -> Option<i32> {
    match args.get(index) {
        Some(OscType::Int(value)) => Some(*value),
        Some(OscType::Long(value)) => Some(*value as i32),
        _ => None,
    }
}

fn float_run(args: &[OscType], start: usize, count: usize) -> Option<Vec<f32>> {
    let mut values = Vec::with_capacity(count);
    for offset in 0..count {
        values.push(float(args, start + offset)?);
    }
    Some(values)
}

fn float(args: &[OscType], index: usize) -> Option<f32> {
    args.get(index).and_then(as_f32)
}

fn as_f32(value: &OscType) -> Option<f32> {
    match value {
        OscType::Float(value) => Some(*value),
        OscType::Double(value) => Some(*value as f32),
        _ => None,
    }
}

fn string(args: &[OscType], index: usize) -> Option<String> {
    match args.get(index) {
        Some(OscType::String(value)) => Some(value.clone()),
        _ => None,
    }
}

/// Writes the standard OSC framing for a `/vibemix/meters` packet into `out`
/// (cleared first): six strip taps then eight bus taps. This bypasses the
/// `ControlMessage::Meters` value, avoiding a per-frame allocation of fourteen
/// `Vec<f32>` plus the typetag list on the engine's control loop.
pub fn encode_meters(out: &mut Vec<u8>, strips: [&[f32]; 6], buses: [&[f32]; 8]) {
    out.clear();
    push_osc_string(out, addr::METERS);
    let strip_count = strips[0].len();
    let bus_count = buses[0].len();
    let mut tags = String::with_capacity(3 + strip_count * 6 + bus_count * 8);
    tags.push_str(",i");
    for _ in 0..strip_count * 6 {
        tags.push('f');
    }
    tags.push('i');
    for _ in 0..bus_count * 8 {
        tags.push('f');
    }
    push_osc_string(out, &tags);
    out.extend_from_slice(&(strip_count as i32).to_be_bytes());
    for run in strips {
        for value in run {
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
    out.extend_from_slice(&(bus_count as i32).to_be_bytes());
    for run in buses {
        for value in run {
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

fn push_osc_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(message: ControlMessage) {
        let bytes = message.encode().expect("encode");
        let decoded = ControlMessage::decode(&bytes).expect("decode");
        assert_eq!(message, decoded);
    }

    #[test]
    fn encode_meters_matches_decode() {
        let strips: Vec<Vec<f32>> = (0..6)
            .map(|run| (0..3).map(|i| (run * 10 + i) as f32).collect())
            .collect();
        let buses: Vec<Vec<f32>> = (0..8)
            .map(|run| (0..3).map(|i| (run * 10 + i) as f32 + 0.5).collect())
            .collect();
        let mut out = Vec::new();
        encode_meters(
            &mut out,
            [
                &strips[0], &strips[1], &strips[2], &strips[3], &strips[4], &strips[5],
            ],
            [
                &buses[0], &buses[1], &buses[2], &buses[3], &buses[4], &buses[5], &buses[6],
                &buses[7],
            ],
        );
        match ControlMessage::decode(&out) {
            Some(ControlMessage::Meters {
                strips_out_l,
                buses_l,
                buses_r,
                ..
            }) => {
                assert_eq!(strips_out_l, strips[4]);
                assert_eq!(buses_l, buses[6]);
                assert_eq!(buses_r, buses[7]);
            }
            other => panic!("unexpected decode: {other:?}"),
        }
    }

    #[test]
    fn decode_tolerates_garbage() {
        // Arbitrary bytes must never panic the decoder (it drives the engine
        // from untrusted UDP input).
        for len in 0..96u32 {
            let bytes: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(37) + 11) as u8).collect();
            let _ = ControlMessage::decode(&bytes);
        }
        // Truncations of a valid message must also be safe.
        let full = ControlMessage::SetPluginSidechainEnabled {
            location: 1,
            target: 2,
            slot: 3,
            enabled: true,
        }
        .encode()
        .expect("encode");
        for cut in 0..full.len() {
            let _ = ControlMessage::decode(&full[..cut]);
        }
    }

    #[test]
    fn roundtrip_scalar_params() {
        roundtrip(ControlMessage::SetStripGain {
            strip: 3,
            gain: 0.5,
        });
        roundtrip(ControlMessage::SetStripMute {
            strip: 1,
            mute: true,
        });
        roundtrip(ControlMessage::SetStripSolo {
            strip: 2,
            solo: true,
        });
        roundtrip(ControlMessage::SetStripPan {
            strip: 2,
            pan: -0.25,
        });
        roundtrip(ControlMessage::SetStripInputGain {
            strip: 2,
            db: 6.5,
        });
        roundtrip(ControlMessage::SetBusGain { bus: 0, gain: 1.5 });
        roundtrip(ControlMessage::SetBusMute { bus: 1, mute: true });
        roundtrip(ControlMessage::SetStripMidiChannel {
            strip: 4,
            channel: 2,
        });
        roundtrip(ControlMessage::SetBusMidiChannel {
            bus: 1,
            channel: 9,
        });
        roundtrip(ControlMessage::SetPluginMidiChannel {
            location: 0,
            target: 2,
            slot: 1,
            channel: 5,
        });
        roundtrip(ControlMessage::SetStripName {
            strip: 2,
            name: "drums".to_string(),
        });
        roundtrip(ControlMessage::SetBusName {
            bus: 1,
            name: "reverb".to_string(),
        });
        roundtrip(ControlMessage::MoveStrip {
            strip: 2,
            to_index: 0,
        });
        roundtrip(ControlMessage::MoveBus {
            bus: 1,
            to_index: 2,
        });
        roundtrip(ControlMessage::SetBusMixMaster { bus: 1, on: true });
        roundtrip(ControlMessage::SetBusReturn { bus: 2, on: true });
        roundtrip(ControlMessage::SetBusSendGain { bus: 2, gain: 0.6 });
        roundtrip(ControlMessage::SetBusReturnGain { bus: 2, gain: 0.7 });
        roundtrip(ControlMessage::SetSendGain {
            strip: 1,
            bus: 2,
            gain: 0.75,
        });
        roundtrip(ControlMessage::SetSendDelay {
            strip: 1,
            bus: 2,
            ms: 12.5,
        });
        roundtrip(ControlMessage::SetSendPreFader {
            strip: 1,
            bus: 2,
            pre: true,
        });
    }

    #[test]
    fn roundtrip_plugins_and_catalog() {
        roundtrip(ControlMessage::AddPlugin {
            location: 0,
            target: 0,
            uri: "http://example.com/plugin".to_string(),
        });
        roundtrip(ControlMessage::AddPlugin {
            location: 2,
            target: 1,
            uri: "http://example.com/bus".to_string(),
        });
        roundtrip(ControlMessage::RemovePlugin {
            location: 1,
            target: 0,
            slot: 1,
        });
        roundtrip(ControlMessage::MovePlugin {
            location: 0,
            target: 0,
            from: 2,
            to: 0,
        });
        roundtrip(ControlMessage::SetPluginParam {
            location: 1,
            target: 0,
            slot: 1,
            param: 2,
            value: 0.5,
        });
        roundtrip(ControlMessage::Catalog {
            json: "[{\"uri\":\"x\"}]".to_string(),
        });
        roundtrip(ControlMessage::RescanPlugins);
        roundtrip(ControlMessage::RetestPlugins);
        roundtrip(ControlMessage::CancelPluginScan);
        roundtrip(ControlMessage::PluginScan {
            running: true,
            done: 12,
            total: 226,
        });
        roundtrip(ControlMessage::SetPluginBypass {
            location: 2,
            target: 1,
            slot: 2,
            bypassed: true,
        });
        roundtrip(ControlMessage::SetPluginSidechainEnabled {
            location: 0,
            target: 1,
            slot: 0,
            enabled: true,
        });
        roundtrip(ControlMessage::SetPluginSidechain {
            location: 0,
            target: 1,
            slot: 0,
            strip: 3,
            gain: 0.5,
        });
        roundtrip(ControlMessage::SetPluginSidechainPre {
            location: 2,
            target: 1,
            slot: 0,
            strip: 3,
            pre: true,
        });
        roundtrip(ControlMessage::OpenPluginUi {
            location: 1,
            target: 1,
            slot: 0,
        });
        roundtrip(ControlMessage::ClosePluginUi {
            location: 0,
            target: 1,
            slot: 0,
        });
        roundtrip(ControlMessage::CopyPlugin {
            location: 1,
            target: 0,
            slot: 2,
        });
        roundtrip(ControlMessage::CutPlugin {
            location: 0,
            target: 3,
            slot: 1,
        });
        roundtrip(ControlMessage::PastePlugin {
            location: 2,
            target: 1,
        });
        roundtrip(ControlMessage::Clipboard {
            has: true,
            location: 1,
            name: "Compressor".to_string(),
        });
    }

    #[test]
    fn roundtrip_structure_and_session() {
        roundtrip(ControlMessage::AddStrip);
        roundtrip(ControlMessage::AddBus);
        roundtrip(ControlMessage::RemoveStrip { strip: 2 });
        roundtrip(ControlMessage::RemoveBus { bus: 1 });
        roundtrip(ControlMessage::DuplicateStrip { strip: 2 });
        roundtrip(ControlMessage::DuplicateBus { bus: 1 });
        roundtrip(ControlMessage::SaveSession {
            path: "/tmp/session.toml".to_string(),
            width: 1280,
            height: 720,
            maximized: true,
        });
        roundtrip(ControlMessage::LoadSession {
            path: "/tmp/session.toml".to_string(),
        });
        roundtrip(ControlMessage::NewSession);
        roundtrip(ControlMessage::SessionResult {
            ok: true,
            path: "/tmp/session.toml".to_string(),
            error: String::new(),
        });
        roundtrip(ControlMessage::SessionResult {
            ok: false,
            path: "/tmp/session.toml".to_string(),
            error: "no such file".to_string(),
        });
        roundtrip(ControlMessage::QueryState);
        roundtrip(ControlMessage::Undo);
        roundtrip(ControlMessage::Redo);
        roundtrip(ControlMessage::EditState {
            undo_depth: 2,
            redo_depth: 1,
            undo_label: "Paste plugin".to_string(),
            redo_label: "Move plugin".to_string(),
        });
        roundtrip(ControlMessage::MissingConnections {
            json: "[{\"port\":\"system:capture_9\"}]".to_string(),
        });
    }

    #[test]
    fn roundtrip_state_snapshot() {
        roundtrip(ControlMessage::State {
            json: "{\"name\":\"test\"}".to_string(),
        });
    }

    #[test]
    fn roundtrip_meters() {
        roundtrip(ControlMessage::Meters {
            strips_in_l: vec![0.0, 0.5, 1.0],
            strips_in_r: vec![0.1, 0.6, 0.9],
            strips_gain_l: vec![0.2, 0.7, 1.1],
            strips_gain_r: vec![0.3, 0.8, 1.2],
            strips_out_l: vec![0.4, 0.9, 1.3],
            strips_out_r: vec![0.5, 1.0, 1.4],
            buses_in_l: vec![0.11, 0.22],
            buses_in_r: vec![0.12, 0.23],
            buses_gain_l: vec![0.13, 0.24],
            buses_gain_r: vec![0.14, 0.25],
            buses_ret_l: vec![0.15, 0.26],
            buses_ret_r: vec![0.16, 0.27],
            buses_l: vec![0.25, 0.75],
            buses_r: vec![0.2, 0.7],
        });
    }

    #[test]
    fn unknown_address_is_none() {
        let packet = OscPacket::Message(OscMessage {
            addr: "/vibemix/nope".to_string(),
            args: Vec::new(),
        });
        assert_eq!(ControlMessage::from_packet(packet), None);
    }
}
