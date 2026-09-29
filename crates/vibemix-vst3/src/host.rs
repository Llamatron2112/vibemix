//! VST3 hosting: discovery and instances, wrapping the C shim around the
//! Steinberg SDK.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

use serde::{Deserialize, Serialize};

pub const MAX_FRAMES: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalePoint {
    pub value: f32,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlPort {
    pub index: usize,
    pub name: String,
    pub default: f32,
    pub flags: i32,
    /// 0 = continuous; 1 = toggle; >1 = discrete step count.
    pub step_count: i32,
    pub scale_points: Vec<ScalePoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    /// 32-hex-character VST3 class id.
    pub class_id: String,
    pub name: String,
    pub vendor: String,
    /// Path to the `.vst3` bundle.
    pub bundle: String,
}

extern "C" {
    fn vst3_string_free(text: *mut c_char);
    fn vst3_scan(dir: *const c_char) -> *mut c_char;
    fn vst3_open(
        bundle: *const c_char,
        class_id: *const c_char,
        sample_rate: f64,
        block_size: c_int,
        in_channels: *mut c_int,
        out_channels: *mut c_int,
    ) -> *mut c_void;
    fn vst3_close(handle: *mut c_void);
    fn vst3_connect_input(handle: *mut c_void, channel: c_int, data: *mut f32);
    fn vst3_connect_output(handle: *mut c_void, channel: c_int, data: *mut f32);
    fn vst3_param_count(handle: *mut c_void) -> c_int;
    fn vst3_param_info(
        handle: *mut c_void,
        index: c_int,
        name: *mut c_char,
        name_cap: c_int,
        def: *mut f32,
        flags: *mut c_int,
        step_count: *mut c_int,
    ) -> c_int;
    fn vst3_param_value_string(
        handle: *mut c_void,
        index: c_int,
        normalized: f32,
        out: *mut c_char,
        cap: c_int,
    ) -> c_int;
    fn vst3_set_param(handle: *mut c_void, index: c_int, normalized: f32);
    fn vst3_get_param(handle: *mut c_void, index: c_int) -> f32;
    fn vst3_run(handle: *mut c_void, frames: c_int);
    fn vst3_save_state(handle: *mut c_void, out: *mut c_char, cap: c_int) -> c_int;
    fn vst3_load_state(handle: *mut c_void, data: *const c_char, len: c_int);
}

/// Directories scanned by [`Host::discover`], plus `$VST3_PATH` entries.
pub fn standard_dirs() -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("VST3_PATH") {
        dirs.extend(std::env::split_paths(&path).map(|p| p.to_string_lossy().into_owned()));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(format!("{}/.vst3", home.to_string_lossy()));
    }
    dirs.push("/usr/local/lib/vst3".to_string());
    dirs.push("/usr/lib64/vst3".to_string());
    dirs.push("/usr/lib/vst3".to_string());
    dirs
}

#[derive(Default)]
pub struct Host;

impl Host {
    pub fn new() -> Self {
        Self
    }

    /// Enumerates the effect classes in every standard directory (deduplicated
    /// by class id).
    pub fn discover(&self) -> Vec<PluginInfo> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for dir in standard_dirs() {
            for info in Self::scan_dir(&dir) {
                if seen.insert(info.class_id.clone()) {
                    out.push(info);
                }
            }
        }
        out
    }

    /// Enumerates the effect classes in a single directory.
    pub fn scan_dir(dir: &str) -> Vec<PluginInfo> {
        let Ok(c_dir) = CString::new(dir) else {
            return Vec::new();
        };
        let ptr = unsafe { vst3_scan(c_dir.as_ptr()) };
        if ptr.is_null() {
            return Vec::new();
        }
        let text = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
        unsafe { vst3_string_free(ptr) };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Instantiates `class_id` from `bundle`.
    pub fn instantiate(
        &self,
        bundle: &str,
        class_id: &str,
        sample_rate: f64,
        block_size: usize,
        max_frames: usize,
    ) -> Option<PluginInstance> {
        let c_bundle = CString::new(bundle).ok()?;
        let c_class = CString::new(class_id).ok()?;
        let mut in_channels: c_int = 0;
        let mut out_channels: c_int = 0;
        let handle = unsafe {
            vst3_open(
                c_bundle.as_ptr(),
                c_class.as_ptr(),
                sample_rate,
                block_size as c_int,
                &mut in_channels,
                &mut out_channels,
            )
        };
        if handle.is_null() {
            return None;
        }
        let in_channels = in_channels.max(0) as usize;
        let out_channels = out_channels.max(0) as usize;
        let mut instance = PluginInstance {
            handle,
            in_channels,
            out_channels,
            inputs: (0..in_channels).map(|_| vec![0.0f32; max_frames].into()).collect(),
            outputs: (0..out_channels).map(|_| vec![0.0f32; max_frames].into()).collect(),
            controls: Vec::new(),
            max_frames,
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
}

unsafe impl Send for PluginInstance {}

impl PluginInstance {
    fn connect_buffers(&mut self) {
        for (channel, buffer) in self.inputs.iter_mut().enumerate() {
            unsafe { vst3_connect_input(self.handle, channel as c_int, buffer.as_mut_ptr()) };
        }
        for (channel, buffer) in self.outputs.iter_mut().enumerate() {
            unsafe { vst3_connect_output(self.handle, channel as c_int, buffer.as_mut_ptr()) };
        }
    }

    fn load_controls(&mut self) {
        let count = unsafe { vst3_param_count(self.handle) }.max(0);
        for index in 0..count {
            let mut name = [0i8; 128];
            let mut default = 0.0f32;
            let mut flags = 0i32;
            let mut step_count = 0i32;
            let ok = unsafe {
                vst3_param_info(
                    self.handle,
                    index,
                    name.as_mut_ptr(),
                    name.len() as c_int,
                    &mut default,
                    &mut flags,
                    &mut step_count,
                )
            };
            if ok == 0 {
                continue;
            }
            let name = unsafe { CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let mut scale_points = Vec::new();
            if (2..=32).contains(&step_count) {
                for step in 0..=step_count {
                    let value = step as f32 / step_count as f32;
                    let mut text = [0i8; 128];
                    let got = unsafe {
                        vst3_param_value_string(
                            self.handle,
                            index,
                            value,
                            text.as_mut_ptr(),
                            text.len() as c_int,
                        )
                    };
                    let label = if got != 0 {
                        unsafe { CStr::from_ptr(text.as_ptr()) }
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        step.to_string()
                    };
                    scale_points.push(ScalePoint { value, label });
                }
            }
            self.controls.push(ControlPort {
                index: index as usize,
                name,
                default,
                flags,
                step_count,
                scale_points,
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
        self.controls.get(param).map(|c| c.name.as_str()).unwrap_or("")
    }

    pub fn control_default(&self, param: usize) -> f32 {
        self.controls.get(param).map(|c| c.default).unwrap_or(0.0)
    }

    pub fn controls(&self) -> &[ControlPort] {
        &self.controls
    }

    pub fn set_control(&mut self, param: usize, value: f32) {
        if param < self.controls.len() {
            unsafe { vst3_set_param(self.handle, param as c_int, value) };
        }
    }

    pub fn control_value(&self, param: usize) -> f32 {
        if param < self.controls.len() {
            unsafe { vst3_get_param(self.handle, param as c_int) }
        } else {
            0.0
        }
    }

    pub fn run(&mut self, frames: usize) {
        let frames = frames.min(self.max_frames) as c_int;
        unsafe { vst3_run(self.handle, frames) };
    }

    /// Component state as an opaque byte blob (empty when the plugin has none).
    pub fn save_state(&mut self) -> Vec<u8> {
        save_blob(self.handle)
    }

    pub fn load_state(&mut self, state: &[u8]) {
        restore_blob(self.handle, state);
    }

    /// A handle for saving/restoring state from another thread (the control
    /// thread), mirroring the LV2 state proxy. Valid while the instance lives.
    pub fn state_handle(&self) -> StateHandle {
        StateHandle {
            handle: self.handle,
        }
    }
}

/// Cross-thread handle to a live instance's state.
pub struct StateHandle {
    handle: *mut c_void,
}

unsafe impl Send for StateHandle {}

impl Clone for StateHandle {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle,
        }
    }
}

impl StateHandle {
    pub fn save(&self) -> Vec<u8> {
        save_blob(self.handle)
    }

    pub fn restore(&self, state: &[u8]) {
        restore_blob(self.handle, state);
    }
}

fn save_blob(handle: *mut c_void) -> Vec<u8> {
    let size = unsafe { vst3_save_state(handle, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut bytes = vec![0u8; size as usize];
    let written = unsafe { vst3_save_state(handle, bytes.as_mut_ptr() as *mut c_char, size) };
    bytes.truncate(written.max(0) as usize);
    bytes
}

fn restore_blob(handle: *mut c_void, state: &[u8]) {
    if state.is_empty() {
        return;
    }
    unsafe {
        vst3_load_state(
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
    let mut i = 0;
    while i + 1 < bytes.len() {
        let (Some(hi), Some(lo)) = (
            (bytes[i] as char).to_digit(16),
            (bytes[i + 1] as char).to_digit(16),
        ) else {
            break;
        };
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    out
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        unsafe { vst3_close(self.handle) };
    }
}
