//! Split out of the controller; see `mod.rs`.

use super::*;

impl Controller {
    pub(super) fn open_ui(&mut self, location: u32, target: u32, slot: u32) {
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
    pub(super) fn reflect_param_to_ui(
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

    pub(super) fn apply_vst3_param(
        &self,
        location: u32,
        target: u32,
        slot: u32,
        param: usize,
        value: f32,
    ) {
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
    pub(super) fn drain_vst3_param_edits(&self) -> bool {
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
    pub(super) fn drain_clap_param_edits(&self) -> bool {
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

    pub(super) fn pump_uis(&mut self) {
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
    pub(super) fn poll_ui_host(&mut self) {
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
    pub(super) fn remote_ui_inputs(&self, id: u32) -> Option<Arc<SharedUiInputs>> {
        self.uis.values().find_map(|ui| match ui {
            OpenUi::Lv2Remote { id: ui_id, context } if *ui_id == id => context.ui_inputs.clone(),
            _ => None,
        })
    }

    /// Folds a control-port edit made in a remote editor into the shared
    /// parameter state (the remote equivalent of `ui_port_write`).
    pub(super) fn apply_remote_ui_write(&mut self, id: u32, port: u32, value: f32) {
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

    pub(super) fn clamp_plugin_param(
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

    /// Closes one open editor and drops it from the map. Remote LV2 editors
    /// live in the UI host, so they need an explicit `Close`; in-process LV2
    /// and VST3 editors tear their window down when dropped.
    pub(super) fn close_ui(&mut self, key: (u32, u32, u32)) {
        if let Some(OpenUi::Lv2Remote { id, .. }) = self.uis.remove(&key) {
            self.ui_service.close(id);
        }
    }

    /// Follows a structural change in one plugin chain by remapping the slots of
    /// its open editors: `map` returns the editor's new slot, or `None` to close
    /// it. LV2 editors carry their slot in their write context, so it is updated
    /// too (VST3 reads the slot from the map key).
    pub(super) fn remap_uis_for(
        &mut self,
        location: u32,
        target: u32,
        map: impl Fn(u32) -> Option<u32>,
    ) {
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
    pub(super) fn close_uis_where(&mut self, mut predicate: impl FnMut(u32, u32, u32) -> bool) {
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
    pub(super) fn close_all_uis(&mut self) {
        let keys: Vec<(u32, u32, u32)> = self.uis.keys().copied().collect();
        for key in keys {
            self.close_ui(key);
        }
    }
}
