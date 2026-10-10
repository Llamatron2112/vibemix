//! Lock-free MIDI control-surface bindings (CC → mixer target), applied on the
//! audio thread.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibemix_core::model::{MidiBinding, MidiTarget};
use vibemix_core::model::MAX_SEND_DELAY_MS;

use crate::params::SharedParams;

pub const MAX_BINDINGS: usize = 256;
pub const OMNI: u32 = 16;

fn target_code(target: MidiTarget) -> u32 {
    match target {
        MidiTarget::StripGain => 0,
        MidiTarget::StripPan => 1,
        MidiTarget::StripMute => 2,
        MidiTarget::StripSolo => 3,
        MidiTarget::BusGain => 4,
        MidiTarget::BusMute => 5,
        MidiTarget::SendGain => 6,
        MidiTarget::SendDelay => 7,
        MidiTarget::PluginParam => 8,
        MidiTarget::PluginBypass => 9,
    }
}

pub fn target_from_code(code: u32) -> MidiTarget {
    match code {
        1 => MidiTarget::StripPan,
        2 => MidiTarget::StripMute,
        3 => MidiTarget::StripSolo,
        4 => MidiTarget::BusGain,
        5 => MidiTarget::BusMute,
        6 => MidiTarget::SendGain,
        7 => MidiTarget::SendDelay,
        8 => MidiTarget::PluginParam,
        9 => MidiTarget::PluginBypass,
        _ => MidiTarget::StripGain,
    }
}

struct Slot {
    used: AtomicBool,
    channel: AtomicU32,
    cc: AtomicU32,
    relative: AtomicBool,
    target: AtomicU32,
    location: AtomicU32,
    target_id: AtomicU32,
    slot: AtomicU32,
    param: AtomicU32,
    min: AtomicU32,
    max: AtomicU32,
}

impl Slot {
    fn new() -> Self {
        Self {
            used: AtomicBool::new(false),
            channel: AtomicU32::new(0),
            cc: AtomicU32::new(0),
            relative: AtomicBool::new(false),
            target: AtomicU32::new(0),
            location: AtomicU32::new(0),
            target_id: AtomicU32::new(0),
            slot: AtomicU32::new(0),
            param: AtomicU32::new(0),
            min: AtomicU32::new(0.0f32.to_bits()),
            max: AtomicU32::new(1.0f32.to_bits()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Learned {
    pub channel: u8,
    pub cc: u8,
}

pub struct SharedMidi {
    slots: Box<[Slot]>,
    count: AtomicU32,
    learn_active: AtomicBool,
    learn_filter: AtomicU32,
    learn_channel: AtomicU32,
    learn_cc: AtomicU32,
    learn_done: AtomicBool,
    /// Set when a binding changed a parameter, so the controller broadcasts
    /// fresh state to the GUI without waiting for a query.
    changed: AtomicBool,
}

impl Default for SharedMidi {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedMidi {
    pub fn new() -> Self {
        let slots: Box<[Slot]> = (0..MAX_BINDINGS).map(|_| Slot::new()).collect();
        Self {
            slots,
            count: AtomicU32::new(0),
            learn_active: AtomicBool::new(false),
            learn_filter: AtomicU32::new(OMNI),
            learn_channel: AtomicU32::new(0),
            learn_cc: AtomicU32::new(0),
            learn_done: AtomicBool::new(false),
            changed: AtomicBool::new(false),
        }
    }

    /// Returns `true` once after the control surface has changed a parameter.
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }

    pub fn set_bindings(&self, bindings: &[MidiBinding]) {
        // Key-only bindings never reach the audio-thread map.
        let mut count = 0usize;
        for binding in bindings {
            let Some(cc) = binding.cc else {
                continue;
            };
            if count >= MAX_BINDINGS {
                break;
            }
            let slot = &self.slots[count];
            slot.channel.store(binding.channel as u32, Ordering::Relaxed);
            slot.cc.store(cc as u32, Ordering::Relaxed);
            slot.relative.store(binding.relative, Ordering::Relaxed);
            slot.target
                .store(target_code(binding.target), Ordering::Relaxed);
            slot.location.store(binding.location, Ordering::Relaxed);
            slot.target_id.store(binding.target_id, Ordering::Relaxed);
            slot.slot.store(binding.slot, Ordering::Relaxed);
            slot.param.store(binding.param, Ordering::Relaxed);
            slot.min.store(binding.min.to_bits(), Ordering::Relaxed);
            slot.max.store(binding.max.to_bits(), Ordering::Relaxed);
            slot.used.store(true, Ordering::Release);
            count += 1;
        }
        for slot in self.slots.iter().skip(count) {
            slot.used.store(false, Ordering::Release);
        }
        self.count.store(count as u32, Ordering::Release);
    }

    /// Starts learn mode. When `channel` is `Some`, only CCs on that channel
    /// are captured (a strip/bus/plugin with an assigned channel); `None`
    /// listens on any channel.
    pub fn start_learn(&self, channel: Option<u8>) {
        self.learn_filter
            .store(channel.map_or(OMNI, u32::from), Ordering::Relaxed);
        self.learn_done.store(false, Ordering::Relaxed);
        self.learn_active.store(true, Ordering::Release);
    }

    pub fn cancel_learn(&self) {
        self.learn_active.store(false, Ordering::Release);
    }

    pub fn take_learned(&self) -> Option<Learned> {
        if !self.learn_done.load(Ordering::Acquire) {
            return None;
        }
        self.learn_done.store(false, Ordering::Relaxed);
        self.learn_active.store(false, Ordering::Release);
        Some(Learned {
            channel: self.learn_channel.load(Ordering::Relaxed) as u8,
            cc: self.learn_cc.load(Ordering::Relaxed) as u8,
        })
    }

    /// Handles a CC message. Returns `true` if it was consumed (mapped, or
    /// captured for learn), in which case it should not be forwarded to plugins.
    pub fn handle_cc(&self, channel: u32, cc: u32, value: u32, params: &SharedParams) -> bool {
        if self.learn_active.load(Ordering::Acquire) {
            let filter = self.learn_filter.load(Ordering::Relaxed);
            if filter == OMNI || filter == channel {
                if !self.learn_done.load(Ordering::Relaxed) {
                    self.learn_channel.store(channel, Ordering::Relaxed);
                    self.learn_cc.store(cc, Ordering::Relaxed);
                    self.learn_done.store(true, Ordering::Release);
                }
                return true;
            }
            // Not the channel being learned; fall through so other bindings on
            // other channels keep working.
        }

        let count = (self.count.load(Ordering::Acquire) as usize).min(MAX_BINDINGS);
        for slot in &self.slots[..count] {
            if !slot.used.load(Ordering::Acquire) || slot.cc.load(Ordering::Relaxed) != cc {
                continue;
            }
            let bound_channel = slot.channel.load(Ordering::Relaxed);
            if bound_channel != OMNI && bound_channel != channel {
                continue;
            }
            self.apply(slot, value, params);
            self.changed.store(true, Ordering::Release);
            return true;
        }
        false
    }

    fn apply(&self, slot: &Slot, value: u32, params: &SharedParams) {
        let target = target_from_code(slot.target.load(Ordering::Relaxed));
        let relative = slot.relative.load(Ordering::Relaxed);
        let location = slot.location.load(Ordering::Relaxed);
        let id = slot.target_id.load(Ordering::Relaxed) as usize;
        let slot_index = slot.slot.load(Ordering::Relaxed) as usize;
        let param = slot.param.load(Ordering::Relaxed) as usize;
        let min = f32::from_bits(slot.min.load(Ordering::Relaxed));
        let max = f32::from_bits(slot.max.load(Ordering::Relaxed));
        let toggled = value >= 64;

        match target {
            MidiTarget::StripGain => {
                let v = scaled(params.strip_gain(id), value, relative, 0.0, 2.0);
                params.set_strip_gain(id, v);
            }
            MidiTarget::StripPan => {
                let v = scaled(params.strip_pan(id), value, relative, -1.0, 1.0);
                params.set_strip_pan(id, v);
            }
            MidiTarget::StripMute if !relative => params.set_strip_mute(id, toggled),
            MidiTarget::StripSolo if !relative => params.set_strip_solo(id, toggled),
            MidiTarget::BusGain => {
                let v = scaled(params.bus_gain(id), value, relative, 0.0, 2.0);
                params.set_bus_gain(id, v);
            }
            MidiTarget::BusMute if !relative => params.set_bus_mute(id, toggled),
            MidiTarget::SendGain => {
                let v = scaled(params.send_gain(id, slot_index), value, relative, 0.0, 2.0);
                params.set_send_gain(id, slot_index, v);
            }
            MidiTarget::SendDelay => {
                let v = scaled(
                    params.send_delay(id, slot_index),
                    value,
                    relative,
                    0.0,
                    MAX_SEND_DELAY_MS,
                );
                params.set_send_delay(id, slot_index, v);
            }
            MidiTarget::PluginParam => {
                if location <= 1 {
                    let chain = location as usize;
                    let current = params.plugin_param(id, chain, slot_index, param);
                    let v = scaled(current, value, relative, min, max);
                    params.set_plugin_param(id, chain, slot_index, param, v);
                } else {
                    let current = params.bus_plugin_param(id, slot_index, param);
                    let v = scaled(current, value, relative, min, max);
                    params.set_bus_plugin_param(id, slot_index, param, v);
                }
            }
            MidiTarget::PluginBypass if !relative => {
                if location <= 1 {
                    params.set_plugin_bypass(id, location as usize, slot_index, toggled);
                } else {
                    params.set_bus_plugin_bypass(id, slot_index, toggled);
                }
            }
            _ => {}
        }
    }
}

/// Absolute CC → `min..max`; relative CC → a small step on the current value.
fn scaled(current: f32, value: u32, relative: bool, min: f32, max: f32) -> f32 {
    if relative {
        let signed = if value >= 64 {
            value as i32 - 64
        } else {
            -(64 - value as i32)
        };
        let step = (max - min) / 100.0;
        (current + signed as f32 * step).clamp(min, max)
    } else {
        min + (value as f32 / 127.0) * (max - min)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_and_relative() {
        assert!((scaled(1.0, 127, false, 0.0, 2.0) - 2.0).abs() < 1e-6);
        assert!((scaled(1.0, 0, false, 0.0, 2.0)).abs() < 1e-6);
        assert!((scaled(1.0, 64, false, -1.0, 1.0)).abs() < 0.02);
        // Relative: 65 nudges up, 63 nudges down, 64 holds.
        assert!(scaled(1.0, 65, true, 0.0, 2.0) > 1.0);
        assert!(scaled(1.0, 63, true, 0.0, 2.0) < 1.0);
        assert!((scaled(1.0, 64, true, 0.0, 2.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn learn_records_cc() {
        let midi = SharedMidi::new();
        midi.start_learn(None);
        assert!(midi.handle_cc(0, 7, 100, &SharedParams::new(1, 1)));
        let learned = midi.take_learned().expect("learned");
        assert_eq!(learned.cc, 7);
        assert_eq!(learned.channel, 0);
    }

    #[test]
    fn control_changes_are_reported_once() {
        let params = SharedParams::new(1, 1);
        let midi = SharedMidi::new();
        midi.set_bindings(&[MidiBinding {
            channel: 0,
            cc: Some(7),
            key: None,
            relative: false,
            target: MidiTarget::StripGain,
            location: 0,
            target_id: 0,
            slot: 0,
            param: 0,
            min: 0.0,
            max: 2.0,
        }]);
        assert!(!midi.take_changed());
        assert!(midi.handle_cc(0, 7, 100, &params));
        assert!(midi.take_changed());
        assert!(!midi.take_changed());
    }

    #[test]
    fn learn_respects_assigned_channel() {
        let params = SharedParams::new(1, 1);
        let midi = SharedMidi::new();
        midi.start_learn(Some(3));
        // A CC on another channel is not captured.
        assert!(!midi.handle_cc(1, 7, 100, &params));
        assert!(midi.take_learned().is_none());
        // The assigned channel is captured.
        assert!(midi.handle_cc(3, 21, 5, &params));
        let learned = midi.take_learned().expect("learned");
        assert_eq!(learned.channel, 3);
        assert_eq!(learned.cc, 21);
    }
}
