//! Split out of the controller; see `mod.rs`.

use super::*;

/// Discovers installed plugins, falling back to the cached catalog when
/// discovery finds nothing.
pub fn discover_or_load_catalog(
    host: &Host,
    vst3: &vibemix_vst3::Host,
    clap: &vibemix_clap::Host,
) -> Vec<PluginEntry> {
    let discovered = discover_catalog(host, vst3, clap);
    if discovered.is_empty() {
        load_catalog()
    } else {
        discovered
    }
}

/// Replaces the `bundle` path of every VST3/CLAP plugin in `model` with the one
/// from the discovered `catalog` (or `None` when the plugin is not installed).
/// A session file is untrusted input: without this, its `bundle` field would be
/// handed straight to `dlopen`, so merely opening a session could run code.
pub fn trust_catalog_bundles(model: &mut MixerModel, catalog: &[PluginEntry]) {
    let resolve = |plugin: &mut PluginSlot| {
        // LV2 plugins are looked up by URI and never load a session-supplied path.
        if plugin.format == PluginFormat::Lv2 {
            return;
        }
        plugin.bundle = catalog
            .iter()
            .find(|entry| entry.format == plugin.format && entry.id == plugin.uri)
            .and_then(|entry| entry.bundle.clone());
    };
    for strip in &mut model.strips {
        strip.plugins.iter_mut().for_each(resolve);
        strip.plugins_post.iter_mut().for_each(resolve);
    }
    for bus in &mut model.buses {
        bus.plugins.iter_mut().for_each(resolve);
    }
}

pub(super) fn discover_catalog(
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

/// Picks the most specific VST3 subcategory that isn't a channel descriptor.
/// The SDK lists subcategories general-to-specific ("Fx|Delay"), but some
/// plugins (e.g. LSP) append the channel layout ("Fx|Stereo"), so the leaf is
/// not always the useful one.
pub(super) fn vst3_category(subcategories: &str, category: &str) -> String {
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

/// Picks a display category from a CLAP feature list. Channel/layout and
/// format features are ignored in favour of a concrete effect type.
pub(super) fn clap_category(features: &str) -> String {
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

pub(super) fn catalog_path() -> std::path::PathBuf {
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

pub(super) fn load_catalog() -> Vec<PluginEntry> {
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

pub(super) fn save_catalog(plugins: &[PluginEntry]) {
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
    if let Err(error) = vibemix_core::config::write_atomic(&dir.join("catalog.json"), text) {
        tracing::warn!(%error, "failed to save plugin catalog");
    }
}

pub(super) fn build_catalog(plugins: &[PluginEntry], health: &HealthStore) -> String {
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

pub(super) fn unsupported_required_feature(info: &vibemix_lv2::PluginInfo) -> Option<String> {
    info.required_features
        .iter()
        .map(String::as_str)
        .find(|feature| !SUPPORTED_REQUIRED_FEATURES.contains(feature))
        .map(|feature| format!("requires {}", short_feature(feature)))
}

pub(super) fn short_feature(uri: &str) -> &str {
    uri.rsplit(['#', '/']).next().unwrap_or(uri)
}

/// Required features the host implements. A plugin requiring anything else is
/// reported as unsupported rather than attempted.
pub(super) const SUPPORTED_REQUIRED_FEATURES: &[&str] = &[
    "http://lv2plug.in/ns/ext/urid#map",
    "http://lv2plug.in/ns/ext/urid#unmap",
    "http://lv2plug.in/ns/ext/worker#schedule",
    "http://lv2plug.in/ns/ext/buf-size#boundedBlockLength",
    "http://lv2plug.in/ns/ext/options#options",
];

impl Controller {
    /// Fills a catalog entry's VST3 control metadata by instantiating the plugin
    /// once (on the control thread, so not RT). LV2 metadata is discovered up
    /// front; VST3 parameter info is only available from an instance.
    pub(super) fn ensure_vst3_controls(&mut self, id: &str) {
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
    pub(super) fn ensure_clap_controls(&mut self, id: &str) {
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
    pub(super) fn ensure_model_plugin_controls(&mut self) {
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
}
