use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vibemix_core::config::config_dir;
use vibemix_core::ids::{BusId, StripId};
use vibemix_core::model::{Bus, MixerModel, PluginSlot, Send, Strip, WindowGeometry};
use vibemix_core::params::{clamp_trim_db, Gain, Pan};
use vibemix_lv2::{find_ui, Host, PluginInfo, PluginInstance, UiCandidate, UiWindow};
use vibemix_proto::ControlMessage;

use crate::backend_jack::{encode_state, make_slot_plugin, JackEngine};
use crate::params::{SharedMeters, SharedParams, MAX_PARAMS, MAX_PLUGINS, PLUGIN_CHAINS};

const PLUGIN_LOC_STRIP_PRE: u32 = 0;
const PLUGIN_LOC_STRIP_POST: u32 = 1;

const METER_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Default)]
struct Actions {
    snapshot: bool,
    catalog: bool,
}

pub struct Controller {
    socket: UdpSocket,
    params: Arc<SharedParams>,
    meters: Arc<SharedMeters>,
    model: MixerModel,
    engine: JackEngine,
    host: Host,
    /// Previous lilv worlds, kept alive so plugin instances (and UIs) created
    /// from them stay valid after a rescan swaps in a fresh `host`.
    retired_hosts: Vec<Host>,
    plugins: Vec<PluginInfo>,
    catalog_json: String,
    smoke_ok: HashSet<String>,
    smoke_bad: HashSet<String>,
    uis: HashMap<(u32, u32, u32), (UiWindow, Box<UiWriteCtx>)>,
    ui_cache: HashMap<String, Option<UiCandidate>>,
    ui_safe: HashMap<String, bool>,
    missing_report: Option<String>,
    pending_session: Option<(bool, String, String)>,
}

struct UiWriteCtx {
    location: u32,
    target: usize,
    slot: usize,
    params: Arc<SharedParams>,
    ports: Vec<(u32, usize)>,
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
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("127.0.0.1", port))?;
        socket.set_read_timeout(Some(METER_INTERVAL))?;

        let plugins: Vec<PluginInfo> = {
            let discovered = discover_catalog(&host);
            if discovered.is_empty() {
                load_catalog()
            } else {
                discovered
            }
        };
        let catalog_json = build_catalog(&plugins);
        save_catalog(&plugins);

        let mut engine = engine;
        let missing_report = missing_report_json(&engine.take_missing_connections());

        Ok(Self {
            socket,
            params,
            meters,
            model,
            engine,
            host,
            retired_hosts: Vec::new(),
            plugins,
            catalog_json,
            smoke_ok: HashSet::new(),
            smoke_bad: HashSet::new(),
            uis: HashMap::new(),
            ui_cache: HashMap::new(),
            ui_safe: HashMap::new(),
            missing_report,
            pending_session: None,
        })
    }

    pub fn run(&mut self) {
        let mut buffer = [0u8; 65536];
        let mut target: Option<SocketAddr> = None;

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

            self.engine.drain_garbage();
            self.pump_uis();

            let Some(address) = target else {
                continue;
            };

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
        }
    }

    fn apply(&mut self, message: ControlMessage) -> Actions {
        tracing::debug!(?message, "control message");
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
            ControlMessage::QueryState => Actions {
                snapshot: true,
                catalog: true,
            },
            ControlMessage::RescanPlugins => {
                self.capture_plugin_states();
                let old = std::mem::replace(&mut self.host, Host::new());
                self.retired_hosts.push(old);
                self.plugins = discover_catalog(&self.host);
                if self.plugins.is_empty() {
                    self.plugins = load_catalog();
                }
                self.catalog_json = build_catalog(&self.plugins);
                save_catalog(&self.plugins);
                if let Err(error) = self.engine.rebuild_plugins(&self.host, &mut self.model, &self.params)
                {
                    tracing::warn!(%error, "failed to rebuild plugins after rescan");
                }
                tracing::info!(count = self.plugins.len(), "plugins rescanned");
                Actions {
                    snapshot: true,
                    catalog: true,
                }
            }
            ControlMessage::Catalog { .. }
            | ControlMessage::State { .. }
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
                match self.engine.rebuild_plugins(&self.host, &mut model, &self.params) {
                    Ok(()) => {
                        self.uis.clear();
                        self.model = model;
                        self.engine.sync_port_names(&self.model);
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
                    match self.engine.rebuild_plugins(&self.host, &mut loaded, &self.params) {
                        Ok(()) => {
                            self.uis.clear();
                            self.model = loaded;
                            self.engine.sync_port_names(&self.model);
                            self.missing_report = missing_report_json(
                                &self.engine.restore_connections(&self.model),
                            );
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
        let Ok(mut child) = Command::new(executable)
            .arg("--smoke")
            .arg(uri)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return true;
        };

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    self.smoke_ok.insert(uri.to_string());
                    return true;
                }
                Ok(Some(_)) => {
                    self.smoke_bad.insert(uri.to_string());
                    tracing::warn!(%uri, "plugin failed smoke test, refusing to load");
                    return false;
                }
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        self.smoke_bad.insert(uri.to_string());
                        tracing::warn!(%uri, "plugin smoke test timed out, refusing to load");
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
            .find(|info| info.uri == uri)
            .map(|info| {
                info.controls
                    .iter()
                    .enumerate()
                    .map(|(param, control)| (control.index as u32, param))
                    .collect()
            })
            .unwrap_or_default();

        let mut context = Box::new(UiWriteCtx {
            location,
            target: target as usize,
            slot: slot as usize,
            params: Arc::clone(&self.params),
            ports,
        });

        let Some(window) = (unsafe {
            self.host.open_ui(
                &uri,
                &candidate,
                handle,
                ui_port_write,
                &mut *context as *mut UiWriteCtx as *mut std::ffi::c_void,
                false,
            )
        }) else {
            tracing::warn!(%uri, "failed to open plugin UI");
            return;
        };

        tracing::info!(location, target, slot, %uri, "opened plugin UI");
        self.uis.insert(key, (window, context));
    }

    fn pump_uis(&mut self) {
        self.uis.retain(|_, (window, _)| {
            window.pump();
            !window.wants_close()
        });
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
            .find(|info| info.uri == uri)
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
        let Some(info) = self.plugins.iter().find(|info| info.uri == uri).cloned() else {
            tracing::warn!(%uri, "plugin not in catalog");
            return false;
        };

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

        if !self.plugin_is_safe(uri) {
            return false;
        }

        let sample_rate = self.engine.sample_rate() as f64;
        let buffer_size = self.engine.buffer_size() as usize;
        let max_block = PluginInstance::max_frames();
        let Some(instance) = self.host.instantiate(uri, sample_rate, buffer_size, max_block) else {
            tracing::warn!(%uri, "plugin instantiate failed");
            return false;
        };
        let proxy = instance.state_proxy();
        let handle = instance.handle() as usize;
        let Some(slot_plugin) =
            make_slot_plugin(&self.host, uri, sample_rate, buffer_size, max_block, instance)
        else {
            tracing::warn!(%uri, "plugin is not mono/stereo");
            return false;
        };

        let params: Vec<f32> = info.controls.iter().map(|control| control.default).collect();

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
            if let Some(strip) = self.model.strip_mut(StripId(target)) {
                let mut plugin = PluginSlot::new(plugin_id(slot), uri);
                plugin.name = info.name;
                plugin.params = params;
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
            if let Some(bus) = self.model.bus_mut(BusId(target)) {
                let mut plugin = PluginSlot::new(plugin_id(slot), uri);
                plugin.name = info.name;
                plugin.params = params;
                bus.plugins.push(plugin);
            }
        }

        tracing::info!(location, target, slot, %uri, "added plugin");
        true
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

    fn send_state(&self, address: SocketAddr) {
        let message = ControlMessage::State {
            json: build_state_json(&self.model, &self.params, &self.plugins),
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

fn build_catalog(plugins: &[PluginInfo]) -> String {
    let list: Vec<serde_json::Value> = plugins
        .iter()
        .map(|info| serde_json::json!({ "uri": info.uri, "name": info.name, "has_ui": info.has_ui }))
        .collect();
    serde_json::Value::Array(list).to_string()
}

/// Hostable mono/stereo plugins, in catalog order.
fn discover_catalog(host: &Host) -> Vec<PluginInfo> {
    host.discover()
        .into_iter()
        .filter(|info| {
            info.hostable
                && ((info.audio_inputs.len() == 2 && info.audio_outputs.len() == 2)
                    || (info.audio_inputs.len() == 1 && info.audio_outputs.len() == 1))
        })
        .collect()
}

fn catalog_path() -> std::path::PathBuf {
    config_dir().join("catalog.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CatalogEntry {
    uri: String,
    name: String,
    #[serde(default)]
    has_ui: bool,
}

fn load_catalog() -> Vec<PluginInfo> {
    let Ok(text) = fs::read_to_string(catalog_path()) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<CatalogEntry>>(&text) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .map(|entry| PluginInfo {
            uri: entry.uri,
            name: entry.name,
            has_ui: entry.has_ui,
            audio_inputs: Vec::new(),
            audio_outputs: Vec::new(),
            controls: Vec::new(),
            atom_inputs: Vec::new(),
            atom_outputs: Vec::new(),
            required_features: Vec::new(),
            optional_features: Vec::new(),
            hostable: true,
        })
        .collect()
}

fn save_catalog(plugins: &[PluginInfo]) {
    let entries: Vec<CatalogEntry> = plugins
        .iter()
        .map(|info| CatalogEntry {
            uri: info.uri.clone(),
            name: info.name.clone(),
            has_ui: info.has_ui,
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
    catalog: &[PluginInfo],
    param_value: F,
) -> Vec<serde_json::Value> {
    plugins
        .iter()
        .enumerate()
        .map(|(slot, plugin)| {
            let info = catalog.iter().find(|info| info.uri == plugin.uri);
            let controls = info.map(|info| info.controls.as_slice()).unwrap_or(&[]);
            let has_ui = info.map(|info| info.has_ui).unwrap_or(false);
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
                "controls": control_list,
            })
        })
        .collect()
}

fn build_state_json(
    model: &MixerModel,
    params: &SharedParams,
    plugins: &[PluginInfo],
) -> String {
    let strips: Vec<serde_json::Value> = model
        .strips
        .iter()
        .map(|strip| {
            let strip_index = strip.id.0 as usize;
            let sends: Vec<serde_json::Value> = strip
                .sends
                .iter()
                .map(|send| serde_json::json!({ "bus": send.bus.0, "gain": send.gain.0 }))
                .collect();
            serde_json::json!({
                "id": strip.id.0,
                "name": strip.name,
                "gain": strip.gain.0,
                "pan": strip.pan.0,
                "input_gain_db": strip.input_gain_db,
                "mute": strip.mute,
                "solo": strip.solo,
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
                "gain": bus.gain.0,
                "mute": bus.mute,
                "mix_to_master": bus.mix_to_master,
                "return_inputs": bus.return_inputs,
                "send_gain": bus.send_gain.0,
                "return_gain": bus.return_gain.0,
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

    serde_json::json!({ "strips": strips, "buses": buses, "window": window }).to_string()
}

fn free_strip_slot(params: &SharedParams) -> Option<usize> {
    (0..params.strip_count()).find(|index| !params.strip_active(*index))
}

fn free_bus_slot(params: &SharedParams) -> Option<usize> {
    (0..params.bus_count()).find(|index| !params.bus_active(*index))
}

pub fn apply_model(params: &SharedParams, model: &MixerModel) {
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
        }
        for send in &strip.sends {
            params.set_send_gain(index, send.bus.0 as usize, send.gain.0);
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
