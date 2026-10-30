//! Split out of the controller; see `mod.rs`.

use super::*;

pub(super) fn plugin_list_json<F: Fn(usize, usize) -> f32>(
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

pub(super) fn build_state_json(
    model: &MixerModel,
    params: &SharedParams,
    plugins: &[PluginEntry],
    sample_rate: u32,
    xruns: u64,
    dsp_load: (f32, f32),
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
        "dsp_load": dsp_load.0,
        "dsp_peak": dsp_load.1,
        "beats_per_bar": model.beats_per_bar,
        "midi_bindings": model.midi_bindings,
        "transport_ui": model.transport_ui,
        "layout": model.layout,
        "midi_sync": model.midi_sync,
        "midi_connections": model.midi_connections,
        "midi_out_connections": model.midi_out_connections,
        "clock": model.clock,
        "recorder": model.recorder,
        "strip_capacity": params.strip_count(),
        "bus_capacity": params.bus_count(),
    })
    .to_string()
}

pub fn apply_model(params: &SharedParams, model: &MixerModel) {
    params.set_beats_per_bar(model.beats_per_bar);
    params.set_midi_sync(model.midi_sync);
    params.set_clock_master(model.clock.master);
    let clock_bpm = if model.clock.bpm.is_finite() {
        model.clock.bpm.clamp(20.0, 400.0)
    } else {
        120.0
    };
    let clock_milli = (clock_bpm * 100.0).round() as u32 * 10;
    params.set_clock_bpm_milli(clock_milli);
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

/// Copies the live values in `params` back into `model`. MIDI control-surface
/// CCs and keyboard bindings change only `params` (they run on the audio thread
/// or bypass the model), so this must run before the model is saved or
/// snapshotted for undo, otherwise those changes would be lost.
pub fn sync_model_from_params(params: &SharedParams, model: &mut MixerModel) {
    for bus in &mut model.buses {
        let index = bus.id.0 as usize;
        bus.gain = Gain(params.bus_gain(index));
        bus.mute = params.bus_mute(index);
        if index != 0 {
            bus.send_gain = Gain(params.bus_send_gain(index));
            bus.return_gain = Gain(params.bus_return_gain(index));
        }
        for (slot, plugin) in bus.plugins.iter_mut().enumerate() {
            plugin.bypassed = params.bus_plugin_bypass(index, slot);
            let count = plugin.params.len().min(MAX_PARAMS);
            for (param, value) in plugin.params.iter_mut().enumerate().take(count) {
                *value = params.bus_plugin_param(index, slot, param);
            }
        }
    }

    for strip in &mut model.strips {
        let index = strip.id.0 as usize;
        strip.gain = Gain(params.strip_gain(index));
        strip.pan = Pan(params.strip_pan(index));
        strip.mute = params.strip_mute(index);
        strip.solo = params.strip_solo(index);
        for send in &mut strip.sends {
            let bus = send.bus.0 as usize;
            send.gain = Gain(params.send_gain(index, bus));
            send.delay_ms = params.send_delay(index, bus);
            send.pre_fader = params.send_pre_fader(index, bus);
        }
        for chain in 0..PLUGIN_CHAINS {
            for (slot, plugin) in strip_plugin_list_mut(strip, chain).iter_mut().enumerate() {
                plugin.bypassed = params.plugin_bypass(index, chain, slot);
                let count = plugin.params.len().min(MAX_PARAMS);
                for (param, value) in plugin.params.iter_mut().enumerate().take(count) {
                    *value = params.plugin_param(index, chain, slot, param);
                }
            }
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

/// The control socket is open to every local process, so a save path is
/// untrusted: refuse to replace an existing file that is not already a vibemix
/// session (empty files are fine), e.g. a shell config.
fn check_overwrite_session(path: &str) -> Result<(), String> {
    match fs::read_to_string(path) {
        Ok(existing) if !existing.trim().is_empty() => toml::from_str::<MixerModel>(&existing)
            .map(|_| ())
            .map_err(|_| format!("refusing to overwrite {path}: not a vibemix session")),
        _ => Ok(()),
    }
}

pub(super) fn save_session(model: &MixerModel, path: &str) -> Result<(), String> {
    check_overwrite_session(path)
        .inspect_err(|error| tracing::warn!(%error, "session save refused"))?;
    let text = toml::to_string_pretty(model).map_err(|error| {
        tracing::warn!(%error, "session serialize failed");
        error.to_string()
    })?;
    vibemix_core::config::write_atomic(std::path::Path::new(path), text).map_err(|error| {
        tracing::warn!(%error, %path, "session save failed");
        error.to_string()
    })?;
    tracing::info!(%path, "session saved");
    Ok(())
}

pub(super) fn load_session(path: &str) -> Result<MixerModel, String> {
    let text = fs::read_to_string(path).map_err(|error| {
        tracing::warn!(%error, %path, "session read failed");
        error.to_string()
    })?;
    let mut model: MixerModel = toml::from_str(&text).map_err(|error| {
        tracing::warn!(%error, %path, "session parse failed");
        error.to_string()
    })?;
    if !model.clock.bpm.is_finite() {
        model.clock.bpm = 120.0;
    }
    model.clock.bpm = (model.clock.bpm.clamp(20.0, 400.0) * 100.0).round() / 100.0;
    // Sessions saved before the master bus was capitalised still say "master".
    if let Some(master) = model.buses.iter_mut().find(|bus| bus.id.0 == 0) {
        if master.name == "master" {
            master.name = "Master".to_string();
        }
    }
    Ok(model)
}

/// Whether two models have the same engine-visible structure (ignoring order,
/// scalar values and window geometry). A change means undo must rebuild.
pub(super) fn structure_eq(a: &MixerModel, b: &MixerModel) -> bool {
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

pub(super) fn plugin_lists_eq(a: &[PluginSlot], b: &[PluginSlot]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.uri == y.uri
                && x.state == y.state
                && x.sidechain_enabled == y.sidechain_enabled
                && sidechain_strips(x) == sidechain_strips(y)
        })
}

pub(super) fn sidechain_strips(plugin: &PluginSlot) -> Vec<u32> {
    let mut strips: Vec<u32> = plugin
        .sidechain
        .iter()
        .map(|source| source.strip.0)
        .collect();
    strips.sort_unstable();
    strips
}

pub(super) fn sends_eq(a: &[Send], b: &[Send]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.bus == y.bus && x.pre_fader == y.pre_fader)
}

pub(super) fn model_bytes(model: &MixerModel) -> usize {
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

pub(super) fn missing_report_json(
    missing: &[crate::backend_jack::MissingConnection],
) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    serde_json::to_string(missing).ok()
}

/// Sets a plugin's sidechain send gain for `strip`, creating the entry if
/// needed (kept at gain 0 so its pre/post choice survives).
pub(super) fn set_sidechain_source(list: &mut Vec<SidechainSource>, strip: StripId, gain: f32) {
    if let Some(entry) = list.iter_mut().find(|entry| entry.strip == strip) {
        entry.gain = Gain(gain);
    } else {
        let mut entry = SidechainSource::new(strip);
        entry.gain = Gain(gain);
        list.push(entry);
    }
}

pub(super) fn set_sidechain_pre(list: &mut Vec<SidechainSource>, strip: StripId, pre: bool) {
    if let Some(entry) = list.iter_mut().find(|entry| entry.strip == strip) {
        entry.pre_fader = pre;
    } else {
        let mut entry = SidechainSource::new(strip);
        entry.gain = Gain(0.0);
        entry.pre_fader = pre;
        list.push(entry);
    }
}

pub(super) fn free_strip_slot(params: &SharedParams) -> Option<usize> {
    (0..params.strip_count()).find(|index| !params.strip_active(*index))
}

pub(super) fn free_bus_slot(params: &SharedParams) -> Option<usize> {
    (0..params.bus_count()).find(|index| !params.bus_active(*index))
}

/// Cheap, allocation-free signature of the model's sidechain routing (the only
/// input to [`compute_strip_order`]). Unequal signatures require a recompute;
/// equal ones never do.
pub(super) fn sidechain_signature(model: &MixerModel) -> u64 {
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
pub(super) fn compute_strip_order(
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

impl Controller {
    pub(super) fn capture_plugin_states(&mut self) {
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

    pub(super) fn snapshot_entry(&self, label: &str) -> UndoEntry {
        let mut model = self.model.clone();
        sync_model_from_params(&self.params, &mut model);
        model.window = None;
        let bytes = model_bytes(&model);
        UndoEntry {
            model,
            label: label.to_string(),
            bytes,
        }
    }

    pub(super) fn push_undo(&mut self, label: &str, key: Option<EditKey>) {
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

    pub(super) fn undo(&mut self) -> bool {
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

    pub(super) fn redo(&mut self) -> bool {
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

    pub(super) fn restore_model(&mut self, snapshot: MixerModel) {
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

    pub(super) fn apply_model_plugin_params(&self) {
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

    pub(super) fn clear_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.undo_bytes = 0;
        self.undo_coalesce = None;
        self.edit_dirty = true;
    }
}
