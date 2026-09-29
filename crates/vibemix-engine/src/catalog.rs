//! Format-agnostic plugin catalog entry, shared by discovery, persistence and
//! the GUI.

use vibemix_core::model::PluginFormat;

/// A named point on a discrete control.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ControlPoint {
    pub value: f32,
    pub label: String,
}

/// Format-agnostic control metadata for the plugin editor.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ControlInfo {
    /// LV2 control-port index, or VST3 parameter index.
    pub index: usize,
    pub name: String,
    pub default: f32,
    pub min: f32,
    pub max: f32,
    pub toggled: bool,
    pub integer: bool,
    pub scale_points: Vec<ControlPoint>,
}

impl From<vibemix_lv2::ControlPort> for ControlInfo {
    fn from(port: vibemix_lv2::ControlPort) -> Self {
        Self {
            index: port.index,
            name: port.name,
            default: port.default,
            min: port.min,
            max: port.max,
            toggled: port.toggled,
            integer: port.integer,
            scale_points: port
                .scale_points
                .into_iter()
                .map(|point| ControlPoint {
                    value: point.value,
                    label: point.label,
                })
                .collect(),
        }
    }
}

impl From<vibemix_vst3::ControlPort> for ControlInfo {
    fn from(port: vibemix_vst3::ControlPort) -> Self {
        Self {
            index: port.index,
            name: port.name,
            default: port.default,
            min: 0.0,
            max: 1.0,
            toggled: port.step_count == 1,
            integer: port.step_count > 1,
            scale_points: port
                .scale_points
                .into_iter()
                .map(|point| ControlPoint {
                    value: point.value,
                    label: point.label,
                })
                .collect(),
        }
    }
}

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
    /// Control metadata (not persisted; VST3 is filled in on demand).
    #[serde(skip, default)]
    pub controls: Vec<ControlInfo>,
}
