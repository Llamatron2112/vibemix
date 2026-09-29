//! Format-agnostic plugin node, hosted by the RT graph.
//!
//! The engine talks to hosted plugins only through this trait, so a second
//! format (VST3) can join the graph without touching the mixer. LV2 is the only
//! implementation today; state capture/UI still go through the LV2-specific
//! accessors stored alongside (see `backend_jack`).

use std::sync::Arc;

use vibemix_lv2::{MidiEvent, SharedOutputs, TimePosition};

pub trait PluginNode: Send {
    fn input_buffer_mut(&mut self, channel: usize) -> &mut [f32];
    fn output_buffer(&self, channel: usize) -> &[f32];

    fn control_count(&self) -> usize;
    fn control_default(&self, param: usize) -> f32;
    fn set_control(&mut self, param: usize, value: f32);

    fn write_midi(&mut self, events: &[MidiEvent]);
    fn write_position(&mut self, position: &TimePosition);
    fn run(&mut self, frames: usize);
    fn deliver_responses(&mut self);

    /// Snapshot shared with the control thread for native-UI output.
    fn outputs(&self) -> Option<Arc<SharedOutputs>>;

    /// Restores serialized state produced by the matching host (empty = no-op).
    fn load_state(&mut self, state: &str);
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

    fn run(&mut self, frames: usize) {
        self.run(frames)
    }

    fn deliver_responses(&mut self) {
        self.deliver_responses()
    }

    fn outputs(&self) -> Option<Arc<SharedOutputs>> {
        Some(self.outputs())
    }

    fn load_state(&mut self, state: &str) {
        self.load_state(state)
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

    fn write_midi(&mut self, _events: &[MidiEvent]) {}

    fn write_position(&mut self, _position: &TimePosition) {}

    fn run(&mut self, frames: usize) {
        self.run(frames)
    }

    fn deliver_responses(&mut self) {}

    fn outputs(&self) -> Option<Arc<SharedOutputs>> {
        None
    }

    fn load_state(&mut self, state: &str) {
        // VST3 state is a binary blob; wired up in a later step.
        let _ = state;
    }
}
