//! Format-agnostic plugin node, hosted by the RT graph.
//!
//! The engine talks to hosted plugins only through this trait, so a second
//! format (VST3) can join the graph without touching the mixer. LV2 is the only
//! implementation today; state capture/UI still go through the LV2-specific
//! accessors stored alongside (see `backend_jack`).

use std::sync::Arc;

use vibemix_lv2::{MidiEvent, SharedOutputs, SharedUiInputs, TimePosition};

pub trait PluginNode: Send {
    fn input_buffer_mut(&mut self, channel: usize) -> &mut [f32];
    fn output_buffer(&self, channel: usize) -> &[f32];

    fn control_count(&self) -> usize;
    fn control_default(&self, param: usize) -> f32;
    fn set_control(&mut self, param: usize, value: f32);

    fn write_midi(&mut self, events: &[MidiEvent]);
    fn write_position(&mut self, position: &TimePosition);
    /// Appends UI→DSP events queued by the control thread. `time_cleared` is
    /// true when the host's own time input was already written this block.
    fn write_ui_events(&mut self, _time_cleared: bool) {}
    fn run(&mut self, frames: usize);
    fn deliver_responses(&mut self);

    /// Snapshot shared with the control thread for native-UI output.
    fn outputs(&self) -> Option<Arc<SharedOutputs>>;

    /// Control→audio mailbox for UI→DSP events (LV2 only).
    fn ui_inputs(&self) -> Option<Arc<SharedUiInputs>> {
        None
    }

    /// Restores serialized state produced by the matching host (empty = no-op).
    fn load_state(&mut self, state: &str);

    /// Whether the plugin has a sidechain input bus (VST3 aux input).
    fn has_sidechain(&self) -> bool {
        false
    }

    /// Feeds the plugin's sidechain input (no-op when it has none).
    fn write_sidechain(&mut self, _left: &[f32], _right: &[f32]) {}

    /// Processing latency in samples reported by the plugin.
    fn latency(&self) -> u32 {
        0
    }
}

impl PluginNode for vibemix_lv2::PluginInstance {
    fn input_buffer_mut(&mut self, channel: usize) -> &mut [f32] {
        self.input_buffer_mut(channel)
    }

    fn output_buffer(&self, channel: usize) -> &[f32] {
        self.output_buffer(channel)
    }

    fn control_count(&self) -> usize {
        self.control_count()
    }

    fn control_default(&self, param: usize) -> f32 {
        self.control_default(param)
    }

    fn set_control(&mut self, param: usize, value: f32) {
        self.set_control(param, value)
    }

    fn write_midi(&mut self, events: &[MidiEvent]) {
        self.write_midi(events)
    }

    fn write_position(&mut self, position: &TimePosition) {
        self.write_position(position)
    }

    fn write_ui_events(&mut self, time_cleared: bool) {
        self.write_ui_events(time_cleared)
    }

    fn run(&mut self, frames: usize) {
        self.run(frames)
    }

    fn deliver_responses(&mut self) {
        self.deliver_responses()
    }

    fn outputs(&self) -> Option<Arc<SharedOutputs>> {
        Some(self.outputs())
    }

    fn ui_inputs(&self) -> Option<Arc<SharedUiInputs>> {
        Some(self.ui_inputs())
    }

    fn load_state(&mut self, state: &str) {
        self.load_state(state)
    }

    fn has_sidechain(&self) -> bool {
        vibemix_lv2::PluginInstance::has_sidechain(self)
    }

    fn write_sidechain(&mut self, left: &[f32], right: &[f32]) {
        vibemix_lv2::PluginInstance::write_sidechain(self, left, right);
    }
}

impl PluginNode for vibemix_vst3::PluginInstance {
    fn input_buffer_mut(&mut self, channel: usize) -> &mut [f32] {
        self.input_buffer_mut(channel)
    }

    fn output_buffer(&self, channel: usize) -> &[f32] {
        self.output_buffer(channel)
    }

    fn control_count(&self) -> usize {
        self.control_count()
    }

    fn control_default(&self, param: usize) -> f32 {
        self.control_default(param)
    }

    fn set_control(&mut self, param: usize, value: f32) {
        self.set_control(param, value)
    }

    fn write_midi(&mut self, events: &[MidiEvent]) {
        for event in events {
            self.midi_event(event.time, event.as_slice());
        }
    }

    fn write_position(&mut self, position: &TimePosition) {
        self.set_position(
            position.frame,
            position.beat,
            position.bar,
            position.beats_per_minute,
            position.beats_per_bar,
            position.speed,
        );
    }

    fn run(&mut self, frames: usize) {
        self.run(frames)
    }

    fn deliver_responses(&mut self) {}

    fn outputs(&self) -> Option<Arc<SharedOutputs>> {
        None
    }

    fn load_state(&mut self, state: &str) {
        let bytes = vibemix_vst3::decode_blob(state);
        vibemix_vst3::PluginInstance::load_state(self, &bytes);
    }

    fn has_sidechain(&self) -> bool {
        vibemix_vst3::PluginInstance::has_sidechain(self)
    }

    fn write_sidechain(&mut self, left: &[f32], right: &[f32]) {
        vibemix_vst3::PluginInstance::write_sidechain(self, left, right, left.len());
    }

    fn latency(&self) -> u32 {
        vibemix_vst3::PluginInstance::latency(self)
    }
}

impl PluginNode for vibemix_clap::PluginInstance {
    fn input_buffer_mut(&mut self, channel: usize) -> &mut [f32] {
        self.input_buffer_mut(channel)
    }

    fn output_buffer(&self, channel: usize) -> &[f32] {
        self.output_buffer(channel)
    }

    fn control_count(&self) -> usize {
        self.control_count()
    }

    fn control_default(&self, param: usize) -> f32 {
        self.control_default(param)
    }

    fn set_control(&mut self, param: usize, value: f32) {
        self.set_control(param, value)
    }

    fn write_midi(&mut self, events: &[MidiEvent]) {
        for event in events {
            self.midi_event(event.time, event.as_slice());
        }
    }

    fn write_position(&mut self, position: &TimePosition) {
        self.set_position(
            position.frame,
            position.beat,
            position.bar,
            position.beats_per_minute,
            position.beats_per_bar,
            position.speed,
        );
    }

    fn run(&mut self, frames: usize) {
        self.run(frames)
    }

    fn deliver_responses(&mut self) {}

    fn outputs(&self) -> Option<Arc<SharedOutputs>> {
        None
    }

    fn load_state(&mut self, state: &str) {
        let bytes = vibemix_clap::decode_blob(state);
        vibemix_clap::PluginInstance::load_state(self, &bytes);
    }

    fn has_sidechain(&self) -> bool {
        vibemix_clap::PluginInstance::has_sidechain(self)
    }

    fn write_sidechain(&mut self, left: &[f32], right: &[f32]) {
        vibemix_clap::PluginInstance::write_sidechain(self, left, right, left.len());
    }

    fn latency(&self) -> u32 {
        vibemix_clap::PluginInstance::latency(self)
    }
}
