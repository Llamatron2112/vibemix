//! Rust wrapper over the CLAP host shim (`shim.cpp`).
//!
//! Mirrors the shape of `vibemix-vst3::host`: discovery, instantiation, audio
//! buffers, parameters, state and editors. Only audio-effect hosting is
//! supported for now.
//!
//! # Safety model
//!
//! Every `unsafe` call here goes through the C++ shim with the opaque `handle`
//! returned by the shim's `open`. The invariants are:
//!
//! - `handle` is non-null and live from `instantiate` until [`PluginInstance`]'s
//!   `Drop`, which closes it. Channel buffers are `Box<[f32]>` owned by the
//!   instance; `Drop::drop` runs before its fields are freed, so the shim never
//!   sees a dangling buffer.
//! - `InstanceHandle` and `View` copy the raw pointer without borrowing the
//!   instance. They are only sound while the instance is alive; the engine
//!   closes editors and drops handles before it retires an instance.
//! - The shim bounds-checks parameter indices; Rust wrappers clamp frame counts
//!   to `max_frames`, the size every shim buffer is allocated with.
//! - `run` and the audio-thread setters must only be called from one thread at
//!   a time (the engine's audio callback); everything else is control-thread.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

use serde::{Deserialize, Serialize};

const MAX_FRAMES: usize = 8192;

// clap_param_info_flags
const CLAP_PARAM_IS_STEPPED: i32 = 1 << 0;

extern "C" {
    fn clap_string_free(text: *mut c_char);
    fn clap_scan(dir: *const c_char) -> *mut c_char;
    fn clap_open(
        bundle: *const c_char,
        plugin_id: *const c_char,
        sample_rate: f64,
        min_frames: c_int,
        max_frames: c_int,
    ) -> *mut c_void;
    fn clap_close(handle: *mut c_void);
    fn clap_channel_counts(handle: *mut c_void, in_channels: *mut c_int, out_channels: *mut c_int);
    fn clap_has_sidechain(handle: *mut c_void) -> c_int;
    fn clap_connect_input(handle: *mut c_void, channel: c_int, data: *mut f32);
    fn clap_connect_output(handle: *mut c_void, channel: c_int, data: *mut f32);
    fn clap_write_sidechain(
        handle: *mut c_void,
        left: *const f32,
        right: *const f32,
        frames: c_int,
    );
    fn clap_set_param_rt(handle: *mut c_void, index: c_int, value: f32);
    fn clap_run(handle: *mut c_void, frames: c_int);
    fn clap_midi_event(handle: *mut c_void, sample_offset: i32, status: i32, d1: i32, d2: i32);
    #[allow(clippy::too_many_arguments)]
    fn clap_set_position(
        handle: *mut c_void,
        seconds: f64,
        beats: f64,
        bpm: f64,
        playing: bool,
        bar_number: i32,
        tsig_num: u16,
        tsig_denom: u16,
    );
    fn clap_param_count(handle: *mut c_void) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn clap_param_info(
        handle: *mut c_void,
        index: c_int,
        name: *mut c_char,
        name_cap: c_int,
        module: *mut c_char,
        module_cap: c_int,
        def: *mut f32,
        flags: *mut c_int,
    ) -> c_int;
    fn clap_set_param(handle: *mut c_void, index: c_int, value: f32);
    fn clap_get_param(handle: *mut c_void, index: c_int) -> f32;
    fn clap_param_value_to_text(
        handle: *mut c_void,
        index: c_int,
        value: f32,
        out: *mut c_char,
        cap: c_int,
    ) -> c_int;
    fn clap_take_param_edit(handle: *mut c_void, index: *mut c_int, value: *mut f32) -> c_int;
    fn clap_save_state(handle: *mut c_void, out: *mut c_char, cap: c_int) -> c_int;
    fn clap_load_state(handle: *mut c_void, data: *const c_char, len: c_int);
    fn clap_latency(handle: *mut c_void) -> c_int;
    fn clap_bus_info(handle: *mut c_void, out: *mut c_char, cap: c_int) -> c_int;
    fn clap_editor_info(handle: *mut c_void, out: *mut c_char, cap: c_int) -> c_int;
    fn clap_poll(handle: *mut c_void);
    fn clap_view_open(instance: *mut c_void, title: *const c_char) -> *mut c_void;
    fn clap_view_pump(view: *mut c_void);
    fn clap_view_wants_close(view: *mut c_void) -> c_int;
    fn clap_view_close(view: *mut c_void);
}

/// One CLAP plugin exposed by a `.clap` bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    /// CLAP plugin id (e.g. `com.u-he.diva`).
    pub id: String,
    pub name: String,
    pub vendor: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub url: String,
    /// Comma-separated CLAP feature list.
    #[serde(default)]
    pub features: String,
    /// Path to the `.clap` bundle.
    pub bundle: String,
}

/// A named point on a discrete control.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalePoint {
    pub value: f32,
    pub label: String,
}

/// Control (parameter) metadata for the plugin editor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlPort {
    pub index: usize,
    pub name: String,
    pub default: f32,
    pub min: f32,
    pub max: f32,
    pub toggled: bool,
    pub integer: bool,
    pub scale_points: Vec<ScalePoint>,
}

/// Directories scanned by [`Host::discover`], plus `$CLAP_PATH` entries.
pub fn standard_dirs() -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("CLAP_PATH") {
        dirs.extend(std::env::split_paths(&path).map(|p| p.to_string_lossy().into_owned()));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(format!("{}/.clap", home.to_string_lossy()));
    }
    dirs.push("/usr/local/lib/clap".to_string());
    dirs.push("/usr/lib64/clap".to_string());
    dirs.push("/usr/lib/clap".to_string());
    dirs
}

#[derive(Default)]
pub struct Host;

impl Host {
    pub fn new() -> Self {
        Self
    }

    /// Enumerates the plugins in every standard directory (deduplicated by id).
    pub fn discover(&self) -> Vec<PluginInfo> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for dir in standard_dirs() {
            for info in Self::scan_dir(&dir) {
                if seen.insert(info.id.clone()) {
                    out.push(info);
                }
            }
        }
        out
    }

    /// Enumerates the plugins in a single directory.
    pub fn scan_dir(dir: &str) -> Vec<PluginInfo> {
        let Ok(c_dir) = CString::new(dir) else {
            return Vec::new();
        };
        let ptr = unsafe { clap_scan(c_dir.as_ptr()) };
        if ptr.is_null() {
            return Vec::new();
        }
        let text = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { clap_string_free(ptr) };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Instantiates `plugin_id` from `bundle`.
    #[allow(clippy::too_many_arguments)]
    pub fn instantiate(
        &self,
        bundle: &str,
        plugin_id: &str,
        sample_rate: f64,
        block_size: usize,
        max_frames: usize,
    ) -> Option<PluginInstance> {
        let c_bundle = CString::new(bundle).ok()?;
        let c_id = CString::new(plugin_id).ok()?;
        let handle = unsafe {
            clap_open(
                c_bundle.as_ptr(),
                c_id.as_ptr(),
                sample_rate,
                1,
                max_frames as c_int,
            )
        };
        if handle.is_null() {
            return None;
        }
        let _ = block_size;
        let (mut in_channels, mut out_channels) = (0, 0);
        unsafe { clap_channel_counts(handle, &mut in_channels, &mut out_channels) };
        let in_channels = in_channels.max(0) as usize;
        let out_channels = out_channels.max(0) as usize;
        let has_sidechain = unsafe { clap_has_sidechain(handle) != 0 };
        let mut instance = PluginInstance {
            handle,
            in_channels,
            out_channels,
            inputs: (0..in_channels)
                .map(|_| vec![0.0f32; max_frames].into())
                .collect(),
            outputs: (0..out_channels)
                .map(|_| vec![0.0f32; max_frames].into())
                .collect(),
            controls: Vec::new(),
            max_frames,
            has_sidechain,
            sample_rate,
        };
        instance.connect_buffers();
        instance.load_controls();
        Some(instance)
    }
}

pub struct PluginInstance {
    handle: *mut c_void,
    in_channels: usize,
    out_channels: usize,
    inputs: Vec<Box<[f32]>>,
    outputs: Vec<Box<[f32]>>,
    controls: Vec<ControlPort>,
    max_frames: usize,
    has_sidechain: bool,
    sample_rate: f64,
}

unsafe impl Send for PluginInstance {}

impl PluginInstance {
    fn connect_buffers(&mut self) {
        for (channel, buffer) in self.inputs.iter_mut().enumerate() {
            unsafe { clap_connect_input(self.handle, channel as c_int, buffer.as_mut_ptr()) };
        }
        for (channel, buffer) in self.outputs.iter_mut().enumerate() {
            unsafe { clap_connect_output(self.handle, channel as c_int, buffer.as_mut_ptr()) };
        }
    }

    fn load_controls(&mut self) {
        let count = unsafe { clap_param_count(self.handle) }.max(0);
        for index in 0..count {
            let mut name = [0i8; 256];
            let mut module = [0i8; 256];
            let mut default = 0.0f32;
            let mut flags = 0i32;
            let ok = unsafe {
                clap_param_info(
                    self.handle,
                    index,
                    name.as_mut_ptr(),
                    name.len() as c_int,
                    module.as_mut_ptr(),
                    module.len() as c_int,
                    &mut default,
                    &mut flags,
                )
            };
            if ok == 0 {
                continue;
            }
            // CLAP parameter values are plain (not normalised); min/max come
            // from the descriptor, which the shim does not expose here, so we
            // derive a usable range from the stepped flag.
            let mut min = 0.0f32;
            let mut max = 1.0f32;
            let mut text = [0i8; 128];
            let got = unsafe {
                clap_param_value_to_text(
                    self.handle,
                    index,
                    default,
                    text.as_mut_ptr(),
                    text.len() as c_int,
                )
            };
            let _ = got;
            // Fall back to 0..1 unless the plugin reports a wider plain range
            // via its value text; LSP and most effects use 0..1 normalised
            // parameters exposed through CLAP as plain 0..1.
            if default < min {
                min = default;
            }
            if default > max {
                max = default;
            }
            let stepped = flags & CLAP_PARAM_IS_STEPPED != 0;
            let toggled = stepped && (max - min) <= 1.0;
            let integer = stepped && !toggled;
            let name = unsafe { CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let _ = module;
            self.controls.push(ControlPort {
                index: index as usize,
                name,
                default,
                min,
                max,
                toggled,
                integer,
                scale_points: Vec::new(),
            });
        }
    }

    pub fn max_frames() -> usize {
        MAX_FRAMES
    }

    pub fn audio_input_count(&self) -> usize {
        self.in_channels
    }

    pub fn audio_output_count(&self) -> usize {
        self.out_channels
    }

    pub fn input_buffer_mut(&mut self, channel: usize) -> &mut [f32] {
        &mut self.inputs[channel]
    }

    pub fn output_buffer(&self, channel: usize) -> &[f32] {
        &self.outputs[channel]
    }

    pub fn control_count(&self) -> usize {
        self.controls.len()
    }

    pub fn control_name(&self, param: usize) -> &str {
        self.controls
            .get(param)
            .map(|c| c.name.as_str())
            .unwrap_or("")
    }

    pub fn control_default(&self, param: usize) -> f32 {
        self.controls.get(param).map(|c| c.default).unwrap_or(0.0)
    }

    pub fn controls(&self) -> &[ControlPort] {
        &self.controls
    }

    /// Queues a parameter change for the next [`run`](Self::run). Meant for the
    /// audio thread: non-blocking, allocation-free, and only sends a value that
    /// differs from the last one sent.
    pub fn set_control(&mut self, param: usize, value: f32) {
        if param < self.controls.len() {
            unsafe { clap_set_param_rt(self.handle, param as c_int, value) };
        }
    }

    pub fn control_value(&self, param: usize) -> f32 {
        if param < self.controls.len() {
            unsafe { clap_get_param(self.handle, param as c_int) }
        } else {
            0.0
        }
    }

    pub fn run(&mut self, frames: usize) {
        let frames = frames.min(self.max_frames) as c_int;
        unsafe { clap_run(self.handle, frames) };
    }

    /// Processing latency in samples reported by the plugin.
    pub fn latency(&self) -> u32 {
        unsafe { clap_latency(self.handle).max(0) as u32 }
    }

    /// Publishes the host transport for the next [`run`](Self::run).
    pub fn set_position(
        &mut self,
        frame: i64,
        beat: f64,
        bar: i64,
        bpm: f32,
        beats_per_bar: f32,
        speed: f32,
    ) {
        let playing = speed > 0.0;
        let seconds = if self.sample_rate > 0.0 {
            frame as f64 / self.sample_rate
        } else {
            0.0
        };
        let numerator = beats_per_bar.round().clamp(1.0, 32.0) as u16;
        unsafe {
            clap_set_position(
                self.handle,
                seconds,
                beat,
                bpm as f64,
                playing,
                bar as i32,
                numerator,
                4,
            )
        };
    }

    /// Queues one raw MIDI channel message for the next [`run`](Self::run).
    pub fn midi_event(&mut self, time: i64, bytes: &[u8]) {
        let Some(&status) = bytes.first() else {
            return;
        };
        if status >= 0xF0 {
            return;
        }
        let d1 = bytes.get(1).copied().unwrap_or(0);
        let d2 = bytes.get(2).copied().unwrap_or(0);
        unsafe {
            clap_midi_event(
                self.handle,
                time as i32,
                status as i32,
                d1 as i32,
                d2 as i32,
            )
        };
    }

    pub fn save_state(&mut self) -> Vec<u8> {
        save_blob(self.handle)
    }

    pub fn load_state(&mut self, state: &[u8]) {
        restore_blob(self.handle, state);
    }

    /// Whether the plugin offers an X11-embeddable editor.
    pub fn has_editor(&self) -> bool {
        !self.editor_info().is_empty() && self.editor_info() != "none"
    }

    pub fn has_sidechain(&self) -> bool {
        self.has_sidechain
    }

    /// Writes a stereo sidechain signal into the plugin's sidechain input bus.
    pub fn write_sidechain(&self, left: &[f32], right: &[f32], frames: usize) {
        // The shim copies `frames` samples from both slices into buffers sized
        // for `max_frames`, so clamp to what is actually readable and writable.
        let frames = frames.min(left.len()).min(right.len()).min(self.max_frames);
        // SAFETY: `handle` is live; both slices hold at least `frames` samples
        // and the shim's destination holds `max_frames` per channel.
        unsafe {
            clap_write_sidechain(self.handle, left.as_ptr(), right.as_ptr(), frames as c_int)
        };
    }

    /// Short description of the plugin's editor support (for diagnostics).
    pub fn editor_info(&self) -> String {
        let mut buf = [0i8; 256];
        let ok = unsafe { clap_editor_info(self.handle, buf.as_mut_ptr(), buf.len() as c_int) };
        if ok == 0 {
            return String::new();
        }
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    /// Short description of the plugin's audio buses (for diagnostics).
    pub fn bus_info(&self) -> String {
        let mut buf = [0i8; 256];
        let ok = unsafe { clap_bus_info(self.handle, buf.as_mut_ptr(), buf.len() as c_int) };
        if ok == 0 {
            return String::new();
        }
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    /// A handle for saving/restoring state from another thread (the control
    /// thread). Valid while the instance lives.
    pub fn instance_handle(&self) -> InstanceHandle {
        InstanceHandle {
            handle: self.handle,
        }
    }

    /// Runs the plugin's main-thread callbacks (timers, requested callbacks).
    pub fn poll(&self) {
        unsafe { clap_poll(self.handle) };
    }
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        unsafe { clap_close(self.handle) };
    }
}

/// Cross-thread handle to a live instance's state.
pub struct InstanceHandle {
    handle: *mut c_void,
}

unsafe impl Send for InstanceHandle {}

impl Clone for InstanceHandle {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle,
        }
    }
}

impl InstanceHandle {
    pub fn save(&self) -> Vec<u8> {
        save_blob(self.handle)
    }

    pub fn restore(&self, state: &[u8]) {
        restore_blob(self.handle, state);
    }

    /// Pops a parameter move made in the plugin's editor, if any.
    pub fn take_param_edit(&self) -> Option<(usize, f32)> {
        let (mut index, mut value) = (0, 0.0f32);
        let got = unsafe { clap_take_param_edit(self.handle, &mut index, &mut value) };
        (got != 0).then_some((index.max(0) as usize, value))
    }

    /// The plugin's reported processing latency in samples.
    pub fn latency(&self) -> u32 {
        unsafe { clap_latency(self.handle).max(0) as u32 }
    }

    pub fn param_count(&self) -> usize {
        unsafe { clap_param_count(self.handle).max(0) as usize }
    }

    pub fn param_value(&self, index: usize) -> f32 {
        unsafe { clap_get_param(self.handle, index as c_int) }
    }

    /// Runs the plugin's main-thread callbacks (timers, requested callbacks).
    pub fn poll(&self) {
        unsafe { clap_poll(self.handle) };
    }

    /// Opens the plugin's editor embedded into a fresh X11 window.
    pub fn open_view(&self, title: &str) -> Option<View> {
        let c_title = CString::new(title).ok()?;
        let handle = unsafe { clap_view_open(self.handle, c_title.as_ptr()) };
        (!handle.is_null()).then_some(View { handle })
    }
}

/// A hosted plugin editor window (`clap.gui` embedded in X11).
pub struct View {
    handle: *mut c_void,
}

unsafe impl Send for View {}

impl View {
    pub fn pump(&self) {
        unsafe {
            clap_view_pump(self.handle);
            clap_poll(self.handle);
        }
    }

    pub fn wants_close(&self) -> bool {
        unsafe { clap_view_wants_close(self.handle) != 0 }
    }

    /// Mirrors a host-driven parameter change into the plugin.
    pub fn set_param(&self, index: usize, value: f32) {
        unsafe { clap_set_param(self.handle, index as c_int, value) };
    }
}

impl Drop for View {
    fn drop(&mut self) {
        unsafe { clap_view_close(self.handle) };
    }
}

fn save_blob(handle: *mut c_void) -> Vec<u8> {
    let size = unsafe { clap_save_state(handle, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut bytes = vec![0u8; size as usize];
    let written = unsafe { clap_save_state(handle, bytes.as_mut_ptr() as *mut c_char, size) };
    bytes.truncate(written.max(0) as usize);
    bytes
}

fn restore_blob(handle: *mut c_void, state: &[u8]) {
    if state.is_empty() {
        return;
    }
    unsafe {
        clap_load_state(
            handle,
            state.as_ptr() as *const c_char,
            state.len() as c_int,
        )
    };
}

/// Hex-encodes a state blob for storage in the session's string field.
pub fn encode_blob(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

pub fn decode_blob(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut high: Option<u8> = None;
    for &byte in bytes {
        let Some(value) = (byte as char).to_digit(16) else {
            continue;
        };
        match high.take() {
            Some(hi) => out.push((hi << 4) | value as u8),
            None => high = Some(value as u8),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_round_trips() {
        let bytes = vec![0u8, 1, 2, 15, 16, 255, 128];
        assert_eq!(decode_blob(&encode_blob(&bytes)), bytes);
        assert_eq!(decode_blob(""), Vec::<u8>::new());
        // Odd-length and non-hex input are tolerated.
        assert_eq!(decode_blob("fff"), vec![0xff]);
        assert_eq!(decode_blob("zz"), Vec::<u8>::new());
    }
}
