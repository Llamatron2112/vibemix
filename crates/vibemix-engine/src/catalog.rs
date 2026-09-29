//! Format-agnostic plugin catalog entry, shared by discovery, persistence and
//! the GUI.

use vibemix_core::model::PluginFormat;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginEntry {
    pub format: PluginFormat,
    /// LV2 URI or VST3 class id (the identifier sent to the GUI and sessions).
    pub id: String,
    pub name: String,
    /// `.vst3` bundle path (VST3 only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(default)]
    pub has_ui: bool,
    #[serde(default)]
    pub inputs: usize,
    #[serde(default)]
    pub outputs: usize,
    #[serde(default)]
    pub midi: bool,
    #[serde(default)]
    pub hostable: bool,
    /// Static "unsupported" reason (e.g. missing LV2 required feature).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<String>,
    /// Control metadata (LV2 only; not persisted).
    #[serde(skip, default)]
    pub controls: Vec<vibemix_lv2::ControlPort>,
}
