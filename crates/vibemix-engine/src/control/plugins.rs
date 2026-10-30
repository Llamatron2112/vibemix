//! Split out of the controller; see `mod.rs`.

use super::*;

pub(super) fn plugin_id(slot: u32) -> vibemix_core::ids::PluginId {
    vibemix_core::ids::PluginId(slot)
}

/// Maps a plugin location to a strip chain index (0 = pre, 1 = post). Returns
/// `None` for bus inserts.
pub(super) fn strip_chain(location: u32) -> Option<usize> {
    match location {
        PLUGIN_LOC_STRIP_PRE => Some(0),
        PLUGIN_LOC_STRIP_POST => Some(1),
        _ => None,
    }
}

pub(super) fn strip_plugin_list(strip: &Strip, chain: usize) -> &Vec<PluginSlot> {
    if chain == 0 {
        &strip.plugins
    } else {
        &strip.plugins_post
    }
}

pub(super) fn strip_plugin_list_mut(strip: &mut Strip, chain: usize) -> &mut Vec<PluginSlot> {
    if chain == 0 {
        &mut strip.plugins
    } else {
        &mut strip.plugins_post
    }
}

impl Controller {
    pub(super) fn add_strip(&mut self) -> bool {
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

    pub(super) fn add_bus(&mut self) -> bool {
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

    pub(super) fn add_plugin(&mut self, location: u32, target: u32, uri: &str) -> bool {
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
        self.insert_plugin_slot_deferred(location, target, plugin)
    }

    /// Appends a fully-described plugin slot (uri, params, LV2 state, bypass) to
    /// a strip chain or bus. Shared by add, paste and duplicate.
    pub(super) fn insert_plugin_slot(
        &mut self,
        location: u32,
        target: u32,
        mut plugin: PluginSlot,
    ) -> bool {
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

        // Only now (after the smoke test) is it safe to instantiate the plugin
        // in-process to read its VST3/CLAP parameter metadata.
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
    pub(super) fn capture_slot_state(
        &self,
        location: u32,
        target: u32,
        slot: u32,
    ) -> Option<PluginSlot> {
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
    pub(super) fn plugin_missing(&self, location: u32, target: u32, slot: u32) -> bool {
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

    pub(super) fn copy_plugin(&mut self, location: u32, target: u32, slot: u32) -> bool {
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

    pub(super) fn cut_plugin(&mut self, location: u32, target: u32, slot: u32) -> bool {
        if !self.copy_plugin(location, target, slot) {
            return false;
        }
        self.remove_plugin(location, target, slot)
    }

    pub(super) fn paste_plugin(&mut self, location: u32, target: u32) -> bool {
        let Some(clipboard) = self.clipboard.clone() else {
            tracing::warn!("paste with empty clipboard");
            return false;
        };
        if clipboard.slot.missing {
            tracing::warn!("refusing to paste a missing plugin");
            return false;
        }
        self.insert_plugin_slot_deferred(location, target, clipboard.slot)
    }

    pub(super) fn duplicate_strip(&mut self, strip_id: u32) -> bool {
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

    pub(super) fn duplicate_bus(&mut self, bus_id: u32) -> bool {
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

    pub(super) fn remove_plugin(&mut self, location: u32, target: u32, slot: u32) -> bool {
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

    pub(super) fn move_plugin(&mut self, location: u32, target: u32, from: u32, to: u32) -> bool {
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
}
