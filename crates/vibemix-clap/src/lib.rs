//! CLAP plugin host: discovery, instancing, parameters, state and editors.
//!
//! Wraps the CLAP C ABI (vendored under `third_party/clap`) behind a small Rust API
//! mirroring the VST3 host in `vibemix-vst3`, so the engine can host CLAP
//! audio-effect plugins through the same `PluginNode` abstraction.

mod host;

pub use host::{
    decode_blob, encode_blob, ControlPort, Host, InstanceHandle, PluginInfo, PluginInstance,
    ScalePoint, View,
};
