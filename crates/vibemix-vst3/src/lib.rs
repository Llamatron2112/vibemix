//! VST3 plugin hosting for vibemix.
//!
//! Wraps the Steinberg VST3 SDK hosting helpers behind a small Rust API
//! (`Host`/`PluginInstance`) that mirrors `vibemix-lv2`. The SDK is MIT-licensed
//! (since 3.8; we vendor 3.8.1), so this crate stays under the project license.

mod host;

pub use host::{
    decode_blob, encode_blob, standard_dirs, ControlPort, Host, InstanceHandle, PluginInfo,
    PluginInstance, ScalePoint, View, MAX_FRAMES,
};
