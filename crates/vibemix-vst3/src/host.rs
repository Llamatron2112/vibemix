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
    fn vst3_set_position(
        handle: *mut c_void,
        project_time_music: f64,
        bar_position_music: f64,
        tempo: f64,
        time_sig_num: c_int,
        time_sig_den: c_int,
        continuous_samples: i64,
        project_time_samples: i64,
        playing: c_int,
    );
    fn vst3_midi_event(
        handle: *mut c_void,
        sample_offset: c_int,
        status: c_int,
        d1: c_int,
        d2: c_int,
    );
    fn vst3_run(handle: *mut c_void, frames: c_int);
    fn vst3_status(
        handle: *mut c_void,
        setup: *mut c_int,
        active: *mut c_int,
        processing: *mut c_int,
        last: *mut c_int,
    ) -> c_int;
    fn vst3_bus_info(handle: *mut c_void, out: *mut c_char, cap: c_int) -> c_int;
    fn vst3_editor_info(handle: *mut c_void, out: *mut c_char, cap: c_int) -> c_int;
    fn vst3_has_editor(handle: *mut c_void) -> c_int;
    fn vst3_has_sidechain(handle: *mut c_void) -> c_int;
    fn vst3_write_sidechain(
        handle: *mut c_void,
        left: *const f32,
        right: *const f32,
        frames: c_int,
    );
    fn vst3_smoke_editor(instance: *mut c_void) -> c_int;
    fn vst3_take_param_edit(handle: *mut c_void, index: *mut c_int, value: *mut f32) -> c_int;
    fn vst3_take_restart_flags(handle: *mut c_void) -> c_int;
    fn vst3_latency(handle: *mut c_void) -> c_int;
    fn vst3_view_open(instance: *mut c_void, title: *const c_char) -> *mut c_void;
    fn vst3_view_pump(handle: *mut c_void);
    fn vst3_view_wants_close(handle: *mut c_void) -> c_int;
    fn vst3_view_set_param(handle: *mut c_void, index: c_int, value: f32);
    fn vst3_view_close(handle: *mut c_void);
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

    /// Publishes the host transport for the next [`run`](Self::run): musical
    /// position (quarter notes), tempo, time signature and play state. Plugins
    /// that sync to host tempo read this from the VST3 `ProcessContext`.
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
        let numerator = beats_per_bar.round().clamp(1.0, 32.0) as c_int;
        unsafe {
            vst3_set_position(
                self.handle,
                beat,
                bar as f64 * beats_per_bar as f64,
                bpm as f64,
                numerator,
                4,
                frame,
                frame,
                playing as c_int,
            )
        };
    }

    /// Queues one raw MIDI channel message for the next [`run`](Self::run).
    /// System realtime messages (clock/start/stop) are ignored here; tempo and
    /// transport are conveyed through [`set_position`](Self::set_position).
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
            vst3_midi_event(
                self.handle,
                time as c_int,
                status as c_int,
                d1 as c_int,
                d2 as c_int,
            )
        };
    }

    /// Component state as an opaque byte blob (empty when the plugin has none).
    pub fn save_state(&mut self) -> Vec<u8> {
        save_blob(self.handle)
    }

    pub fn load_state(&mut self, state: &[u8]) {
        restore_blob(self.handle, state);
    }

    /// Setup/`process` tresults: `(setup, active, processing, last_process)`.
    pub fn status(&self) -> (i32, i32, i32, i32) {
        let (mut setup, mut active, mut processing, mut last) = (0, 0, 0, 0);
        unsafe {
            vst3_status(
                self.handle,
                &mut setup,
                &mut active,
                &mut processing,
                &mut last,
            )
        };
        (setup, active, processing, last)
    }

    /// Whether the plugin offers an X11-embeddable editor.
    pub fn has_editor(&self) -> bool {
        unsafe { vst3_has_editor(self.handle) != 0 }
    }

    /// Whether the plugin has an auxiliary (sidechain) input bus.
    pub fn has_sidechain(&self) -> bool {
        unsafe { vst3_has_sidechain(self.handle) != 0 }
    }

    /// Writes a stereo sidechain signal into the plugin's sidechain input bus.
    pub fn write_sidechain(&self, left: &[f32], right: &[f32], frames: usize) {
        unsafe {
            vst3_write_sidechain(
                self.handle,
                left.as_ptr(),
                right.as_ptr(),
                frames as c_int,
            )
        };
    }

    /// Creates and tears down the editor headlessly (crash canary).
    pub fn smoke_editor(&self) -> bool {
        unsafe { vst3_smoke_editor(self.handle) != 0 }
    }

    /// Short description of the plugin's editor support (for diagnostics).
    pub fn editor_info(&self) -> String {
        let mut buf = [0i8; 256];
        let ok = unsafe { vst3_editor_info(self.handle, buf.as_mut_ptr(), buf.len() as c_int) };
        if ok == 0 {
            return String::new();
        }
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    /// Short description of the component's audio buses (for diagnostics).
    pub fn bus_info(&self) -> String {
        let mut buf = [0i8; 256];
        let ok = unsafe { vst3_bus_info(self.handle, buf.as_mut_ptr(), buf.len() as c_int) };
        if ok == 0 {
            return String::new();
        }
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    /// A handle for saving/restoring state from another thread (the control
    /// thread), mirroring the LV2 state proxy. Valid while the instance lives.
    pub fn instance_handle(&self) -> InstanceHandle {
        InstanceHandle {
            handle: self.handle,
        }
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
        let got = unsafe { vst3_take_param_edit(self.handle, &mut index, &mut value) };
        (got != 0).then_some((index.max(0) as usize, value))
    }

    /// Returns and clears the plugin's `IComponentHandler` restart flags.
    pub fn take_restart_flags(&self) -> i32 {
        unsafe { vst3_take_restart_flags(self.handle) }
    }

    /// The plugin's reported processing latency in samples.
    pub fn latency(&self) -> u32 {
        unsafe { vst3_latency(self.handle).max(0) as u32 }
    }

    pub fn param_count(&self) -> usize {
        unsafe { vst3_param_count(self.handle).max(0) as usize }
    }

    pub fn param_value(&self, index: usize) -> f32 {
        unsafe { vst3_get_param(self.handle, index as c_int) }
    }

    /// Opens the plugin's editor embedded into a fresh X11 window.
    pub fn open_view(&self, title: &str) -> Option<View> {
        let c_title = CString::new(title).ok()?;
        let handle = unsafe { vst3_view_open(self.handle, c_title.as_ptr()) };
        (!handle.is_null()).then_some(View { handle })
    }
}

/// A hosted plugin editor window (`IPlugView` embedded in X11).
pub struct View {
    handle: *mut c_void,
}

unsafe impl Send for View {}

impl View {
    pub fn pump(&self) {
        unsafe { vst3_view_pump(self.handle) };
    }

    pub fn wants_close(&self) -> bool {
        unsafe { vst3_view_wants_close(self.handle) != 0 }
    }

    /// Mirrors a host-driven parameter change into the editor.
    pub fn set_param(&self, index: usize, value: f32) {
        unsafe { vst3_view_set_param(self.handle, index as c_int, value) };
    }
}

impl Drop for View {
    fn drop(&mut self) {
        unsafe { vst3_view_close(self.handle) };
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
