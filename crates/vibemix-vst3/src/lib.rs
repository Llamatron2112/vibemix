//! VST3 plugin hosting for vibemix.
//!
//! Wraps the Steinberg VST3 SDK hosting helpers behind a small Rust API
//! (`Host`/`PluginInstance`) that mirrors `vibemix-lv2`. The SDK is GPLv3; this
//! crate and any build that enables it are therefore GPLv3.

mod host;

pub use host::{standard_dirs, ControlPort, Host, PluginInfo, PluginInstance, MAX_FRAMES};
