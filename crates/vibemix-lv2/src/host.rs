use std::os::raw::c_char;
use std::ptr;

use libc::c_void;
use lilv::instance::ActiveInstance;
use lilv::World;
use lv2_raw::LV2Feature;

use crate::urid::UridMap;
use crate::worker::{Worker, WorkerInterface, WORKER_INTERFACE_URI, WORKER_SCHEDULE_URI};
use crate::state::{StateEntry, StateInterface, StateProxy, STATE_INTERFACE_URI};
use crate::ui::{self, UiCandidate, UiWindow};
use crate::ui_outputs::SharedOutputs;

use std::sync::Arc;

const OPTIONS_URI: &[u8] = b"http://lv2plug.in/ns/ext/options#options\0";
const BOUNDED_BLOCK_URI: &[u8] = b"http://lv2plug.in/ns/ext/buf-size#boundedBlockLength\0";
const WORKER_SCHEDULE_C: &[u8] = b"http://lv2plug.in/ns/ext/worker#schedule\0";

#[repr(C)]
struct Lv2Option {
    context: u32,
    subject: u32,
    key: u32,
    size: u32,
    type_: u32,
    value: *const c_void,
}

struct OptionFeatures {
    _options: Box<[Lv2Option]>,
    _values: Box<[u64]>,
    options_feature: LV2Feature,
    bounded_feature: LV2Feature,
}

const MAX_FRAMES: usize = 8192;
const AUDIO_PORT: &str = "http://lv2plug.in/ns/lv2core#AudioPort";
const CONTROL_PORT: &str = "http://lv2plug.in/ns/lv2core#ControlPort";
const INPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#InputPort";
const OUTPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#OutputPort";
const ATOM_PORT: &str = "http://lv2plug.in/ns/ext/atom#AtomPort";
const ATOM_SEQUENCE: &str = "http://lv2plug.in/ns/ext/atom#Sequence";
const ATOM_BUFFER_BYTES: usize = 8192;
const LV2_TOGGLED: &str = "http://lv2plug.in/ns/lv2core#toggled";
const LV2_INTEGER: &str = "http://lv2plug.in/ns/lv2core#integer";
const LV2_PORT_PROPERTY: &str = "http://lv2plug.in/ns/lv2core#portProperty";
const LV2_SCALE_POINT: &str = "http://lv2plug.in/ns/lv2core#scalePoint";
const RDF_VALUE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#value";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const BLACKLIST: &[&str] = &[
    "http://calf.sourceforge.net/plugins/ReverseDelay",
    "http://gareus.org/oss/lv2/meters#EBUr128",
];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScalePoint {
    pub value: f32,
    pub label: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginInfo {
    pub uri: String,
    pub name: String,
    pub audio_inputs: Vec<usize>,
    pub audio_outputs: Vec<usize>,
    pub controls: Vec<ControlPort>,
    pub atom_inputs: Vec<usize>,
    pub atom_outputs: Vec<usize>,
    pub required_features: Vec<String>,
    pub optional_features: Vec<String>,
    pub hostable: bool,
    pub has_ui: bool,
}

pub struct Host {
    world: World,
    urid: &'static UridMap,
}

impl Host {
    pub fn new() -> Self {
        let world = World::new();
        world.load_all();
        Self {
            world,
            urid: UridMap::shared(),
        }
    }

    fn property_float(&self, port: &lilv::port::Port, uri: &str) -> Option<f32> {
        let predicate = self.world.new_uri(uri);
        port.value(&predicate).iter().find_map(|node| {
            node.as_float()
                .or_else(|| node.as_int().map(|value| value as f32))
                .or_else(|| {
                    node.as_bool()
                        .map(|value| if value { 1.0 } else { 0.0 })
                })
        })
    }

    fn control_flag(&self, port: &lilv::port::Port, uri: &str) -> bool {
        let property = self.world.new_uri(uri);
        if port.has_property(&property) || port.is_a(&property) {
            return true;
        }
        let port_property = self.world.new_uri(LV2_PORT_PROPERTY);
        port.value(&port_property)
            .iter()
            .any(|node| node.as_uri() == Some(uri))
    }

    fn scale_points(&self, port: &lilv::port::Port) -> Vec<ScalePoint> {
        let predicate = self.world.new_uri(LV2_SCALE_POINT);
        let value_pred = self.world.new_uri(RDF_VALUE);
        let label_pred = self.world.new_uri(RDFS_LABEL);

        let mut points = Vec::new();
        for point in port.value(&predicate).iter() {
            let value = self
                .world
                .get(Some(&point), Some(&value_pred), None)
                .and_then(|node| {
                    node.as_float()
                        .or_else(|| node.as_int().map(|value| value as f32))
                });
            let Some(value) = value else {
                continue;
            };
            let label = self
                .world
                .get(Some(&point), Some(&label_pred), None)
                .and_then(|node| node.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{value}"));
            points.push(ScalePoint { value, label });
        }
        points.sort_by(|a, b| {
            a.value
                .partial_cmp(&b.value)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        points
    }

    fn control_port(&self, port: &lilv::port::Port) -> ControlPort {
        let index = port.index();
        let min = self.property_float(port, "http://lv2plug.in/ns/lv2core#minimum");
        let max = self.property_float(port, "http://lv2plug.in/ns/lv2core#maximum");
        let default = self
            .property_float(port, "http://lv2plug.in/ns/lv2core#default")
            .or(min)
            .unwrap_or(0.0);

        let (min, max) = match (min, max) {
            (Some(min), Some(max)) if max > min => (min, max),
            _ => (default.min(0.0), default.max(1.0)),
        };
        let default = default.clamp(min, max);

        ControlPort {
            index,
            name: port_name(port, index),
            default,
            min,
            max,
            toggled: self.control_flag(port, LV2_TOGGLED),
            integer: self.control_flag(port, LV2_INTEGER),
            scale_points: self.scale_points(port),
        }
    }

    fn classify(&self, plugin: &lilv::plugin::Plugin) -> Classified {
        let audio = self.world.new_uri(AUDIO_PORT);
        let control = self.world.new_uri(CONTROL_PORT);
        let atom = self.world.new_uri(ATOM_PORT);
        let input = self.world.new_uri(INPUT_PORT);
        let output = self.world.new_uri(OUTPUT_PORT);

        let mut classified = Classified {
            hostable: true,
            ..Classified::default()
        };
        for port in plugin.iter_ports() {
            let index = port.index();
            if port.is_a(&audio) {
                if port.is_a(&input) {
                    classified.audio_inputs.push(index);
                } else if port.is_a(&output) {
                    classified.audio_outputs.push(index);
                } else {
                    classified.hostable = false;
                }
            } else if port.is_a(&control) {
                if port.is_a(&input) {
                    classified.controls.push(self.control_port(&port));
                } else if port.is_a(&output) {
                    classified.control_outputs.push(index);
                } else {
                    classified.hostable = false;
                }
            } else if port.is_a(&atom) {
                if port.is_a(&output) {
                    classified.atom_outputs.push(index);
                } else if port.is_a(&input) {
                    classified.atom_inputs.push(index);
                } else {
                    classified.hostable = false;
                }
            } else {
                classified.hostable = false;
            }
        }
        if let Some(uri) = plugin.uri().as_uri() {
            if is_blacklisted(uri) {
                classified.hostable = false;
            }
        }
        classified
    }

    pub fn discover(&self) -> Vec<PluginInfo> {
        let mut infos = Vec::new();
        for plugin in self.world.plugins().iter() {
            if !plugin.verify() {
                continue;
            }
            let classified = self.classify(&plugin);
            let uri = plugin.uri().as_uri().unwrap_or_default().to_string();
            infos.push(PluginInfo {
                has_ui: ui::find_ui(&uri).is_some(),
                uri,
                name: plugin.name().as_str().unwrap_or_default().to_string(),
                audio_inputs: classified.audio_inputs,
                audio_outputs: classified.audio_outputs,
                controls: classified.controls,
                atom_inputs: classified.atom_inputs,
                atom_outputs: classified.atom_outputs,
                required_features: feature_uris(&plugin.required_features()),
                optional_features: feature_uris(&plugin.optional_features()),
                hostable: classified.hostable,
            });
        }
        infos
    }

    fn build_option_features(
        &self,
        sample_rate: f32,
        nominal_block: u32,
        max_block: u32,
    ) -> OptionFeatures {
        let map = |uri: &str| self.urid.map(uri);
        let key_sample = map("http://lv2plug.in/ns/ext/parameters#sampleRate");
        let key_min = map("http://lv2plug.in/ns/ext/buf-size#minBlockLength");
        let key_max = map("http://lv2plug.in/ns/ext/buf-size#maxBlockLength");
        let key_nominal = map("http://lv2plug.in/ns/ext/buf-size#nominalBlockLength");
        let key_sequence = map("http://lv2plug.in/ns/ext/buf-size#sequenceSize");
        let type_float = map("http://lv2plug.in/ns/ext/atom#Float");
        let type_int = map("http://lv2plug.in/ns/ext/atom#Int");

        let mut values: Box<[u64]> = vec![0u64; 5].into_boxed_slice();
        values[0] = u64::from(sample_rate.to_bits());
        values[1] = 1;
        values[2] = u64::from(max_block);
        values[3] = u64::from(nominal_block);
        values[4] = u64::from(ATOM_BUFFER_BYTES as u32);

        let base = values.as_ptr();
        let value_at = |index: usize| unsafe { base.add(index) as *const c_void };

        let options: Box<[Lv2Option]> = vec![
            Lv2Option {
                context: 0,
                subject: 0,
                key: key_sample,
                size: 4,
                type_: type_float,
                value: value_at(0),
            },
            Lv2Option {
                context: 0,
                subject: 0,
                key: key_min,
                size: 4,
                type_: type_int,
                value: value_at(1),
            },
            Lv2Option {
                context: 0,
                subject: 0,
                key: key_max,
                size: 4,
                type_: type_int,
                value: value_at(2),
            },
            Lv2Option {
                context: 0,
                subject: 0,
                key: key_nominal,
                size: 4,
                type_: type_int,
                value: value_at(3),
            },
            Lv2Option {
                context: 0,
                subject: 0,
                key: key_sequence,
                size: 4,
                type_: type_int,
                value: value_at(4),
            },
            Lv2Option {
                context: 0,
                subject: 0,
                key: 0,
                size: 0,
                type_: 0,
                value: ptr::null(),
            },
        ]
        .into_boxed_slice();

        let options_feature = LV2Feature {
            uri: OPTIONS_URI.as_ptr() as *const c_char,
            data: options.as_ptr() as *mut c_void,
        };
        let bounded_feature = LV2Feature {
            uri: BOUNDED_BLOCK_URI.as_ptr() as *const c_char,
            data: ptr::null_mut(),
        };

        OptionFeatures {
            _options: options,
            _values: values,
            options_feature,
            bounded_feature,
        }
    }
    pub fn map_uri(&self, uri: &str) -> u32 {
        self.urid.map(uri)
    }

    pub fn unmap_uri(&self, urid: u32) -> Option<String> {
        self.urid.unmap(urid)
    }

    /// # Safety
    /// `plugin_handle` must be valid, and `write_user` must outlive the window.
    pub unsafe fn open_ui(
        &self,
        plugin_uri: &str,
        candidate: &UiCandidate,
        plugin_handle: *mut c_void,
        write_cb: ui::PortWrite,
        write_user: *mut c_void,
        headless: bool,
    ) -> Option<UiWindow> {
        ui::open(
            self.urid,
            plugin_uri,
            candidate,
            plugin_handle,
            write_cb,
            write_user,
            headless,
        )
    }

    pub fn instantiate(
        &self,
        uri: &str,
        sample_rate: f64,
        nominal_block: usize,
        max_block: usize,
    ) -> Option<PluginInstance> {
        let uri_node = self.world.new_uri(uri);
        let plugin = self.world.plugins().plugin(&uri_node)?;
        let classified = self.classify(&plugin);
        if !classified.hostable {
            return None;
        }

        let audio_inputs = classified.audio_inputs;
        let audio_outputs = classified.audio_outputs;
        let controls = classified.controls;
        let control_outputs = classified.control_outputs;
        let atom_inputs = classified.atom_inputs;
        let atom_outputs = classified.atom_outputs;
        let sequence_urid = self.urid.map(ATOM_SEQUENCE);

        let option_features = self.build_option_features(
            sample_rate as f32,
            nominal_block as u32,
            max_block as u32,
        );
        let mut features: Vec<&LV2Feature> = Vec::new();
        features.extend(self.urid.features());
        features.push(&option_features.options_feature);
        features.push(&option_features.bounded_feature);

        let needs_worker = plugin
            .required_features()
            .iter()
            .any(|node| node.as_uri() == Some(WORKER_SCHEDULE_URI));
        let mut worker = if needs_worker { Some(Worker::new()) } else { None };
        let worker_feature = worker.as_ref().map(|worker| LV2Feature {
            uri: WORKER_SCHEDULE_C.as_ptr() as *const c_char,
            data: worker.schedule_feature_data(),
        });
        if let Some(feature) = &worker_feature {
            features.push(feature);
        }

        let mut instance = unsafe { plugin.instantiate(sample_rate, features) }?;

        let mut keep_worker = false;
        if let Some(worker_ref) = worker.as_mut() {
            if let Some(interface) =
                unsafe { instance.extension_data::<WorkerInterface>(WORKER_INTERFACE_URI) }
            {
                worker_ref.attach(interface.as_ptr(), instance.handle());
                keep_worker = true;
            }
        }
        if !keep_worker {
            worker = None;
        }

        let state = unsafe { instance.extension_data::<StateInterface>(STATE_INTERFACE_URI) }
            .map(|interface| StateProxy::new(interface.as_ptr(), instance.handle()));

        let mut input_buffers: Vec<Box<[f32]>> = (0..audio_inputs.len())
            .map(|_| vec![0.0f32; MAX_FRAMES].into_boxed_slice())
            .collect();
        let mut output_buffers: Vec<Box<[f32]>> = (0..audio_outputs.len())
            .map(|_| vec![0.0f32; MAX_FRAMES].into_boxed_slice())
            .collect();
        let mut control_buffers: Vec<f32> = controls
            .iter()
            .map(|control| control.default)
            .collect();
        let mut control_output_buffers: Vec<f32> = vec![0.0f32; control_outputs.len()];
        let mut atom_buffers: Vec<Box<[u64]>> = Vec::new();
        for _ in &atom_inputs {
            atom_buffers.push(sequence_buffer(sequence_urid, 0));
        }
        for _ in &atom_outputs {
            atom_buffers.push(sequence_buffer(
                sequence_urid,
                (ATOM_BUFFER_BYTES - 12) as u32,
            ));
        }

        for (buffer, index) in input_buffers.iter_mut().zip(&audio_inputs) {
            unsafe { instance.connect_port_mut(*index, buffer.as_mut_ptr()) };
        }
        for (buffer, index) in output_buffers.iter_mut().zip(&audio_outputs) {
            unsafe { instance.connect_port_mut(*index, buffer.as_mut_ptr()) };
        }
        for (buffer, control) in control_buffers.iter_mut().zip(&controls) {
            unsafe { instance.connect_port_mut(control.index, std::ptr::from_mut(buffer)) };
        }
        for (buffer, index) in control_output_buffers.iter_mut().zip(&control_outputs) {
            unsafe { instance.connect_port_mut(*index, std::ptr::from_mut(buffer)) };
        }
        for (buffer, index) in atom_buffers
            .iter_mut()
            .zip(atom_inputs.iter().chain(atom_outputs.iter()))
        {
            unsafe { instance.connect_port_mut(*index, buffer.as_mut_ptr() as *mut u8) };
        }

        let active = unsafe { instance.activate() };

        let outputs = Arc::new(SharedOutputs::new(
            &control_outputs,
            &atom_outputs,
            ATOM_BUFFER_BYTES,
        ));

        Some(PluginInstance {
            active,
            audio_inputs,
            audio_outputs,
            controls,
            control_outputs,
            atom_inputs,
            atom_outputs,
            input_buffers,
            output_buffers,
            control_buffers,
            control_output_buffers,
            atom_buffers,
            _options: option_features,
            worker,
            state,
            outputs,
        })
    }
}

#[derive(Default)]
struct Classified {
    audio_inputs: Vec<usize>,
    audio_outputs: Vec<usize>,
    controls: Vec<ControlPort>,
    control_outputs: Vec<usize>,
    atom_inputs: Vec<usize>,
    atom_outputs: Vec<usize>,
    hostable: bool,
}

fn sequence_buffer(sequence_urid: u32, size: u32) -> Box<[u64]> {
    let mut buffer = vec![0u64; ATOM_BUFFER_BYTES / 8].into_boxed_slice();
    let words = buffer.as_mut_ptr() as *mut u32;
    unsafe {
        *words = size;
        *words.add(1) = sequence_urid;
        *words.add(2) = 0;
    }
    buffer
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

fn port_name(port: &lilv::port::Port, index: usize) -> String {
    port.name()
        .and_then(|node| node.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("param {index}"))
}

fn feature_uris(nodes: &lilv::node::Nodes) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|node| node.as_uri().map(str::to_string))
        .collect()
}

fn is_blacklisted(uri: &str) -> bool {
    if BLACKLIST.contains(&uri) {
        return true;
    }
    std::env::var("VIBEMIX_LV2_BLACKLIST")
        .map(|value| value.split(',').any(|entry| entry.trim() == uri))
        .unwrap_or(false)
}

pub struct PluginInstance {
    active: ActiveInstance,
    audio_inputs: Vec<usize>,
    audio_outputs: Vec<usize>,
    controls: Vec<ControlPort>,
    control_outputs: Vec<usize>,
    atom_inputs: Vec<usize>,
    atom_outputs: Vec<usize>,
    input_buffers: Vec<Box<[f32]>>,
    output_buffers: Vec<Box<[f32]>>,
    control_buffers: Vec<f32>,
    control_output_buffers: Vec<f32>,
    atom_buffers: Vec<Box<[u64]>>,
    _options: OptionFeatures,
    worker: Option<Box<Worker>>,
    state: Option<StateProxy>,
    outputs: Arc<SharedOutputs>,
}

unsafe impl Send for PluginInstance {}

impl PluginInstance {
    pub fn max_frames() -> usize {
        MAX_FRAMES
    }

    pub fn audio_input_count(&self) -> usize {
        self.audio_inputs.len()
    }

    pub fn audio_output_count(&self) -> usize {
        self.audio_outputs.len()
    }

    pub fn input_buffer_mut(&mut self, channel: usize) -> &mut [f32] {
        &mut self.input_buffers[channel]
    }

    pub fn output_buffer(&self, channel: usize) -> &[f32] {
        &self.output_buffers[channel]
    }

    pub fn control_count(&self) -> usize {
        self.controls.len()
    }

    pub fn control_name(&self, param: usize) -> &str {
        self.controls
            .get(param)
            .map(|control| control.name.as_str())
            .unwrap_or("")
    }

    pub fn control_min(&self, param: usize) -> f32 {
        self.controls
            .get(param)
            .map(|control| control.min)
            .unwrap_or(0.0)
    }

    pub fn control_max(&self, param: usize) -> f32 {
        self.controls
            .get(param)
            .map(|control| control.max)
            .unwrap_or(1.0)
    }

    pub fn control_default(&self, param: usize) -> f32 {
        self.controls
            .get(param)
            .map(|control| control.default)
            .unwrap_or(0.0)
    }

    pub fn control_value(&self, param: usize) -> f32 {
        self.control_buffers.get(param).copied().unwrap_or(0.0)
    }

    pub fn control_output_count(&self) -> usize {
        self.control_outputs.len()
    }

    pub fn atom_input_count(&self) -> usize {
        self.atom_inputs.len()
    }

    pub fn atom_output_count(&self) -> usize {
        self.atom_outputs.len()
    }

    pub fn atom_buffer_count(&self) -> usize {
        self.atom_buffers.len()
    }

    pub fn control_output_value(&self, param: usize) -> f32 {
        self.control_output_buffers
            .get(param)
            .copied()
            .unwrap_or(0.0)
    }

    pub fn set_control(&mut self, param: usize, value: f32) {
        if let Some(buffer) = self.control_buffers.get_mut(param) {
            *buffer = value;
        }
    }

    pub fn run(&mut self, frames: usize) {
        let frames = frames.min(MAX_FRAMES);
        unsafe { self.active.run(frames) };
        self.publish_outputs();
    }

    /// Sends the plugin's current output port values to the UI-facing snapshot.
    /// Does nothing (and costs nothing) unless a UI is open for this instance.
    fn publish_outputs(&self) {
        if !self.outputs.is_active() {
            return;
        }
        self.outputs.publish_control(&self.control_output_buffers);

        let base = self.atom_inputs.len();
        for (index, buffer) in self.atom_buffers.iter().enumerate().skip(base) {
            let words = buffer.as_ptr() as *const u32;
            // SAFETY: the buffer is a live `LV2_Atom_Sequence` written by the
            // plugin on this thread during `run`.
            let (size, format) = unsafe { (*words as usize, *words.add(1)) };
            let total = (std::mem::size_of::<u32>() * 2 + size).min(ATOM_BUFFER_BYTES);
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, total) };
            self.outputs.publish_atom(index - base, bytes, format);
        }
    }

    pub fn deliver_responses(&mut self) {
        if let Some(worker) = self.worker.as_mut() {
            worker.deliver_responses();
        }
    }

    pub fn state_proxy(&self) -> Option<StateProxy> {
        self.state.clone()
    }

    pub fn handle(&self) -> *mut c_void {
        self.active.instance().handle()
    }

    pub fn outputs(&self) -> Arc<SharedOutputs> {
        Arc::clone(&self.outputs)
    }

    pub fn restore_state(&mut self, entries: &[StateEntry]) {
        if let Some(state) = &self.state {
            state.restore(entries);
        }
    }
}
