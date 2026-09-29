use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibemix_core::params::{clamp_trim_db, db_to_linear};

pub const MAX_PLUGINS: usize = 6;
pub const MAX_PARAMS: usize = 64;
/// Plugin insert chains per strip: 0 = pre-fader, 1 = post-fader.
pub const PLUGIN_CHAINS: usize = 2;

#[derive(Debug)]
pub struct SharedParams {
    strip_gain: Vec<AtomicU32>,
    strip_pan: Vec<AtomicU32>,
    strip_input_gain: Vec<AtomicU32>,
    strip_mute: Vec<AtomicBool>,
    strip_solo: Vec<AtomicBool>,
    strip_active: Vec<AtomicBool>,
    bus_gain: Vec<AtomicU32>,
    bus_mute: Vec<AtomicBool>,
    bus_active: Vec<AtomicBool>,
    bus_mix_master: Vec<AtomicBool>,
    bus_return: Vec<AtomicBool>,
    bus_send_gain: Vec<AtomicU32>,
    bus_return_gain: Vec<AtomicU32>,
    send_gain: Vec<Vec<AtomicU32>>,
    send_delay: Vec<Vec<AtomicU32>>,
    plugin_params: Vec<Vec<Vec<Vec<AtomicU32>>>>,
    plugin_bypass: Vec<Vec<Vec<AtomicBool>>>,
    bus_plugin_params: Vec<Vec<Vec<AtomicU32>>>,
    bus_plugin_bypass: Vec<Vec<AtomicBool>>,
    state_pause: AtomicBool,
    state_ack: AtomicBool,
    beats_per_bar: AtomicU32,
    midi_sync: AtomicBool,
}

impl SharedParams {
    pub fn new(strip_count: usize, bus_count: usize) -> Self {
        Self {
            strip_gain: gain_vec(strip_count),
            strip_pan: gain_vec(strip_count),
            strip_input_gain: gain_vec(strip_count),
            strip_mute: bool_vec(strip_count),
            strip_solo: bool_vec(strip_count),
            strip_active: bool_vec(strip_count),
            bus_gain: gain_vec(bus_count),
            bus_mute: bool_vec(bus_count),
            bus_active: bool_vec(bus_count),
            bus_mix_master: bool_vec(bus_count),
            bus_return: bool_vec(bus_count),
            bus_send_gain: gain_vec(bus_count),
            bus_return_gain: gain_vec(bus_count),
            send_gain: (0..strip_count).map(|_| zero_vec(bus_count)).collect(),
            send_delay: (0..strip_count).map(|_| zero_vec(bus_count)).collect(),
            plugin_params: (0..strip_count)
                .map(|_| {
                    (0..PLUGIN_CHAINS)
                        .map(|_| {
                            (0..MAX_PLUGINS)
                                .map(|_| zero_vec(MAX_PARAMS))
                                .collect()
                        })
                        .collect()
                })
                .collect(),
            plugin_bypass: (0..strip_count)
                .map(|_| {
                    (0..PLUGIN_CHAINS)
                        .map(|_| (0..MAX_PLUGINS).map(|_| AtomicBool::new(false)).collect())
                        .collect()
                })
                .collect(),
            bus_plugin_params: (0..bus_count)
                .map(|_| (0..MAX_PLUGINS).map(|_| zero_vec(MAX_PARAMS)).collect())
                .collect(),
            bus_plugin_bypass: (0..bus_count)
                .map(|_| (0..MAX_PLUGINS).map(|_| AtomicBool::new(false)).collect())
                .collect(),
            state_pause: AtomicBool::new(false),
            state_ack: AtomicBool::new(false),
            beats_per_bar: AtomicU32::new(4.0f32.to_bits()),
            midi_sync: AtomicBool::new(true),
        }
    }

    pub fn midi_sync(&self) -> bool {
        self.midi_sync.load(Ordering::Relaxed)
    }

    pub fn set_midi_sync(&self, value: bool) {
        self.midi_sync.store(value, Ordering::Relaxed);
    }

    pub fn beats_per_bar(&self) -> f32 {
        f32::from_bits(self.beats_per_bar.load(Ordering::Relaxed))
    }

    pub fn set_beats_per_bar(&self, value: f32) {
        if value > 0.0 {
            self.beats_per_bar.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn strip_count(&self) -> usize {
        self.strip_gain.len()
    }

    pub fn bus_count(&self) -> usize {
        self.bus_gain.len()
    }

    pub fn strip_active(&self, strip: usize) -> bool {
        get_bool(&self.strip_active, strip)
    }

    pub fn set_strip_active(&self, strip: usize, value: bool) {
        set_bool(&self.strip_active, strip, value);
    }

    pub fn strip_gain(&self, strip: usize) -> f32 {
        get_gain(&self.strip_gain, strip, 1.0)
    }

    pub fn set_strip_gain(&self, strip: usize, value: f32) {
        set_gain(&self.strip_gain, strip, value.clamp(0.0, 4.0));
    }

    pub fn strip_pan(&self, strip: usize) -> f32 {
        get_gain(&self.strip_pan, strip, 0.0)
    }

    pub fn set_strip_pan(&self, strip: usize, value: f32) {
        set_gain(&self.strip_pan, strip, value.clamp(-1.0, 1.0));
    }

    /// Linear input trim gain for a strip (defaults to unity).
    pub fn strip_input_gain(&self, strip: usize) -> f32 {
        get_gain(&self.strip_input_gain, strip, 1.0)
    }

    /// Sets the input trim from a decibel value, clamped to the supported range.
    pub fn set_strip_input_gain_db(&self, strip: usize, db: f32) {
        set_gain(&self.strip_input_gain, strip, db_to_linear(clamp_trim_db(db)));
    }

    pub fn strip_mute(&self, strip: usize) -> bool {
        get_bool(&self.strip_mute, strip)
    }

    pub fn set_strip_mute(&self, strip: usize, value: bool) {
        set_bool(&self.strip_mute, strip, value);
    }

    pub fn strip_solo(&self, strip: usize) -> bool {
        get_bool(&self.strip_solo, strip)
    }

    pub fn set_strip_solo(&self, strip: usize, value: bool) {
        set_bool(&self.strip_solo, strip, value);
    }

    pub fn bus_active(&self, bus: usize) -> bool {
        get_bool(&self.bus_active, bus)
    }

    pub fn set_bus_active(&self, bus: usize, value: bool) {
        set_bool(&self.bus_active, bus, value);
    }

    pub fn bus_gain(&self, bus: usize) -> f32 {
        get_gain(&self.bus_gain, bus, 1.0)
    }

    pub fn set_bus_gain(&self, bus: usize, value: f32) {
        set_gain(&self.bus_gain, bus, value.clamp(0.0, 4.0));
    }

    pub fn bus_mute(&self, bus: usize) -> bool {
        get_bool(&self.bus_mute, bus)
    }

    pub fn set_bus_mute(&self, bus: usize, value: bool) {
        set_bool(&self.bus_mute, bus, value);
    }

    pub fn bus_mix_master(&self, bus: usize) -> bool {
        get_bool(&self.bus_mix_master, bus)
    }

    pub fn set_bus_mix_master(&self, bus: usize, value: bool) {
        set_bool(&self.bus_mix_master, bus, value);
    }

    pub fn bus_return(&self, bus: usize) -> bool {
        get_bool(&self.bus_return, bus)
    }

    pub fn set_bus_return(&self, bus: usize, value: bool) {
        set_bool(&self.bus_return, bus, value);
    }

    pub fn bus_send_gain(&self, bus: usize) -> f32 {
        get_gain(&self.bus_send_gain, bus, 1.0)
    }

    pub fn set_bus_send_gain(&self, bus: usize, value: f32) {
        set_gain(&self.bus_send_gain, bus, value.clamp(0.0, 4.0));
    }

    pub fn bus_return_gain(&self, bus: usize) -> f32 {
        get_gain(&self.bus_return_gain, bus, 1.0)
    }

    pub fn set_bus_return_gain(&self, bus: usize, value: f32) {
        set_gain(&self.bus_return_gain, bus, value.clamp(0.0, 4.0));
    }

    pub fn send_gain(&self, strip: usize, bus: usize) -> f32 {
        self.send_gain
            .get(strip)
            .and_then(|sends| sends.get(bus))
            .map(|value| f32::from_bits(value.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    pub fn set_send_gain(&self, strip: usize, bus: usize, value: f32) {
        if let Some(slot) = self.send_gain.get(strip).and_then(|sends| sends.get(bus)) {
            slot.store(value.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn send_delay(&self, strip: usize, bus: usize) -> f32 {
        self.send_delay
            .get(strip)
            .and_then(|sends| sends.get(bus))
            .map(|value| f32::from_bits(value.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    pub fn set_send_delay(&self, strip: usize, bus: usize, value: f32) {
        if let Some(slot) = self.send_delay.get(strip).and_then(|sends| sends.get(bus)) {
            slot.store(
                value.clamp(0.0, vibemix_core::model::MAX_SEND_DELAY_MS).to_bits(),
                Ordering::Relaxed,
            );
        }
    }

    pub fn bus_plugin_param(&self, bus: usize, slot: usize, param: usize) -> f32 {
        self.bus_plugin_params
            .get(bus)
            .and_then(|slots| slots.get(slot))
            .and_then(|params| params.get(param))
            .map(|value| f32::from_bits(value.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    pub fn set_bus_plugin_param(&self, bus: usize, slot: usize, param: usize, value: f32) {
        if let Some(target) = self
            .bus_plugin_params
            .get(bus)
            .and_then(|slots| slots.get(slot))
            .and_then(|params| params.get(param))
        {
            target.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn bus_plugin_bypass(&self, bus: usize, slot: usize) -> bool {
        self.bus_plugin_bypass
            .get(bus)
            .and_then(|slots| slots.get(slot))
            .map(|value| value.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub fn set_bus_plugin_bypass(&self, bus: usize, slot: usize, value: bool) {
        if let Some(target) = self
            .bus_plugin_bypass
            .get(bus)
            .and_then(|slots| slots.get(slot))
        {
            target.store(value, Ordering::Relaxed);
        }
    }

    /// Swaps parameter and bypass values between two bus insert slots.
    pub fn swap_bus_plugin_slots(&self, bus: usize, a: usize, b: usize) {
        if a == b {
            return;
        }
        if let Some(slots) = self.bus_plugin_params.get(bus) {
            if let (Some(pa), Some(pb)) = (slots.get(a), slots.get(b)) {
                for (x, y) in pa.iter().zip(pb.iter()) {
                    let xv = x.load(Ordering::Relaxed);
                    let yv = y.load(Ordering::Relaxed);
                    x.store(yv, Ordering::Relaxed);
                    y.store(xv, Ordering::Relaxed);
                }
            }
        }
        if let Some(slots) = self.bus_plugin_bypass.get(bus) {
            if let (Some(pa), Some(pb)) = (slots.get(a), slots.get(b)) {
                let xv = pa.load(Ordering::Relaxed);
                let yv = pb.load(Ordering::Relaxed);
                pa.store(yv, Ordering::Relaxed);
                pb.store(xv, Ordering::Relaxed);
            }
        }
    }

    pub fn move_bus_plugin_slot(&self, bus: usize, from: usize, to: usize) {
        if from == to {
            return;
        }
        if from < to {
            for index in from..to {
                self.swap_bus_plugin_slots(bus, index, index + 1);
            }
        } else {
            for index in (to..from).rev() {
                self.swap_bus_plugin_slots(bus, index, index + 1);
            }
        }
    }

    pub fn plugin_param(&self, strip: usize, chain: usize, slot: usize, param: usize) -> f32 {
        self.plugin_params
            .get(strip)
            .and_then(|chains| chains.get(chain))
            .and_then(|slots| slots.get(slot))
            .and_then(|params| params.get(param))
            .map(|value| f32::from_bits(value.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    pub fn set_plugin_param(
        &self,
        strip: usize,
        chain: usize,
        slot: usize,
        param: usize,
        value: f32,
    ) {
        if let Some(target) = self
            .plugin_params
            .get(strip)
            .and_then(|chains| chains.get(chain))
            .and_then(|slots| slots.get(slot))
            .and_then(|params| params.get(param))
        {
            target.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn plugin_bypass(&self, strip: usize, chain: usize, slot: usize) -> bool {
        self.plugin_bypass
            .get(strip)
            .and_then(|chains| chains.get(chain))
            .and_then(|slots| slots.get(slot))
            .map(|value| value.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub fn set_plugin_bypass(&self, strip: usize, chain: usize, slot: usize, value: bool) {
        if let Some(target) = self
            .plugin_bypass
            .get(strip)
            .and_then(|chains| chains.get(chain))
            .and_then(|slots| slots.get(slot))
        {
            target.store(value, Ordering::Relaxed);
        }
    }

    /// Swaps the parameter and bypass values between two plugin slots. Only
    /// atomics are touched, so this is safe to call through a shared reference.
    /// Callers should pause the RT thread to avoid a transient mismatch.
    pub fn swap_plugin_slots(&self, strip: usize, chain: usize, a: usize, b: usize) {
        if a == b {
            return;
        }
        if let Some(chains) = self.plugin_params.get(strip) {
            if let Some(slots) = chains.get(chain) {
                if let (Some(pa), Some(pb)) = (slots.get(a), slots.get(b)) {
                    for (x, y) in pa.iter().zip(pb.iter()) {
                        let xv = x.load(Ordering::Relaxed);
                        let yv = y.load(Ordering::Relaxed);
                        x.store(yv, Ordering::Relaxed);
                        y.store(xv, Ordering::Relaxed);
                    }
                }
            }
        }
        if let Some(chains) = self.plugin_bypass.get(strip) {
            if let Some(slots) = chains.get(chain) {
                if let (Some(pa), Some(pb)) = (slots.get(a), slots.get(b)) {
                    let xv = pa.load(Ordering::Relaxed);
                    let yv = pb.load(Ordering::Relaxed);
                    pa.store(yv, Ordering::Relaxed);
                    pb.store(xv, Ordering::Relaxed);
                }
            }
        }
    }

    /// Moves the plugin at `from` to position `to`, shifting the slots in
    /// between. Implemented as a sequence of adjacent swaps.
    pub fn move_plugin_slot(&self, strip: usize, chain: usize, from: usize, to: usize) {
        if from == to {
            return;
        }
        if from < to {
            for index in from..to {
                self.swap_plugin_slots(strip, chain, index, index + 1);
            }
        } else {
            for index in (to..from).rev() {
                self.swap_plugin_slots(strip, chain, index, index + 1);
            }
        }
    }

    pub fn state_pause(&self) -> bool {
        self.state_pause.load(Ordering::Relaxed)
    }

    pub fn set_state_pause(&self, value: bool) {
        self.state_pause.store(value, Ordering::Relaxed);
    }

    pub fn state_ack(&self) -> bool {
        self.state_ack.load(Ordering::Relaxed)
    }

    pub fn set_state_ack(&self, value: bool) {
        self.state_ack.store(value, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct SharedMeters {
    strip_in_l: Vec<AtomicU32>,
    strip_in_r: Vec<AtomicU32>,
    strip_gain_l: Vec<AtomicU32>,
    strip_gain_r: Vec<AtomicU32>,
    strip_out_l: Vec<AtomicU32>,
    strip_out_r: Vec<AtomicU32>,
    bus_in_l: Vec<AtomicU32>,
    bus_in_r: Vec<AtomicU32>,
    bus_gain_l: Vec<AtomicU32>,
    bus_gain_r: Vec<AtomicU32>,
    bus_ret_l: Vec<AtomicU32>,
    bus_ret_r: Vec<AtomicU32>,
    bus_l: Vec<AtomicU32>,
    bus_r: Vec<AtomicU32>,
}

/// A snapshot of every meter, grouped by tap point. Strip values are indexed by
/// strip id; bus values by bus id.
pub struct MeterLevels {
    pub strips_in_l: Vec<f32>,
    pub strips_in_r: Vec<f32>,
    pub strips_gain_l: Vec<f32>,
    pub strips_gain_r: Vec<f32>,
    pub strips_out_l: Vec<f32>,
    pub strips_out_r: Vec<f32>,
    pub buses_in_l: Vec<f32>,
    pub buses_in_r: Vec<f32>,
    pub buses_gain_l: Vec<f32>,
    pub buses_gain_r: Vec<f32>,
    pub buses_ret_l: Vec<f32>,
    pub buses_ret_r: Vec<f32>,
    pub buses_l: Vec<f32>,
    pub buses_r: Vec<f32>,
}

impl SharedMeters {
    pub fn new(strip_count: usize, bus_count: usize) -> Self {
        Self {
            strip_in_l: zero_vec(strip_count),
            strip_in_r: zero_vec(strip_count),
            strip_gain_l: zero_vec(strip_count),
            strip_gain_r: zero_vec(strip_count),
            strip_out_l: zero_vec(strip_count),
            strip_out_r: zero_vec(strip_count),
            bus_in_l: zero_vec(bus_count),
            bus_in_r: zero_vec(bus_count),
            bus_gain_l: zero_vec(bus_count),
            bus_gain_r: zero_vec(bus_count),
            bus_ret_l: zero_vec(bus_count),
            bus_ret_r: zero_vec(bus_count),
            bus_l: zero_vec(bus_count),
            bus_r: zero_vec(bus_count),
        }
    }

    pub fn store_strip_in(&self, strip: usize, left: f32, right: f32) {
        store_pair(&self.strip_in_l, &self.strip_in_r, strip, left, right);
    }

    pub fn store_strip_gain(&self, strip: usize, left: f32, right: f32) {
        store_pair(&self.strip_gain_l, &self.strip_gain_r, strip, left, right);
    }

    pub fn store_strip_out(&self, strip: usize, left: f32, right: f32) {
        store_pair(&self.strip_out_l, &self.strip_out_r, strip, left, right);
    }

    pub fn store_bus_in(&self, bus: usize, left: f32, right: f32) {
        store_pair(&self.bus_in_l, &self.bus_in_r, bus, left, right);
    }

    pub fn store_bus_gain(&self, bus: usize, left: f32, right: f32) {
        store_pair(&self.bus_gain_l, &self.bus_gain_r, bus, left, right);
    }

    pub fn store_bus_ret(&self, bus: usize, left: f32, right: f32) {
        store_pair(&self.bus_ret_l, &self.bus_ret_r, bus, left, right);
    }

    pub fn store_bus(&self, bus: usize, left: f32, right: f32) {
        store_pair(&self.bus_l, &self.bus_r, bus, left, right);
    }

    pub fn levels(&self) -> MeterLevels {
        MeterLevels {
            strips_in_l: collect(&self.strip_in_l),
            strips_in_r: collect(&self.strip_in_r),
            strips_gain_l: collect(&self.strip_gain_l),
            strips_gain_r: collect(&self.strip_gain_r),
            strips_out_l: collect(&self.strip_out_l),
            strips_out_r: collect(&self.strip_out_r),
            buses_in_l: collect(&self.bus_in_l),
            buses_in_r: collect(&self.bus_in_r),
            buses_gain_l: collect(&self.bus_gain_l),
            buses_gain_r: collect(&self.bus_gain_r),
            buses_ret_l: collect(&self.bus_ret_l),
            buses_ret_r: collect(&self.bus_ret_r),
            buses_l: collect(&self.bus_l),
            buses_r: collect(&self.bus_r),
        }
    }
}

fn store_pair(left_slots: &[AtomicU32], right_slots: &[AtomicU32], index: usize, left: f32, right: f32) {
    if let Some(slot) = left_slots.get(index) {
        slot.store(left.to_bits(), Ordering::Relaxed);
    }
    if let Some(slot) = right_slots.get(index) {
        slot.store(right.to_bits(), Ordering::Relaxed);
    }
}

fn collect(values: &[AtomicU32]) -> Vec<f32> {
    values
        .iter()
        .map(|value| f32::from_bits(value.load(Ordering::Relaxed)))
        .collect()
}

fn gain_vec(count: usize) -> Vec<AtomicU32> {
    (0..count).map(|_| AtomicU32::new(1.0f32.to_bits())).collect()
}

fn zero_vec(count: usize) -> Vec<AtomicU32> {
    (0..count).map(|_| AtomicU32::new(0.0f32.to_bits())).collect()
}

fn bool_vec(count: usize) -> Vec<AtomicBool> {
    (0..count).map(|_| AtomicBool::new(false)).collect()
}

fn get_gain(values: &[AtomicU32], index: usize, fallback: f32) -> f32 {
    values
        .get(index)
        .map(|value| f32::from_bits(value.load(Ordering::Relaxed)))
        .unwrap_or(fallback)
}

fn set_gain(values: &[AtomicU32], index: usize, value: f32) {
    if let Some(slot) = values.get(index) {
        slot.store(value.to_bits(), Ordering::Relaxed);
    }
}

fn get_bool(values: &[AtomicBool], index: usize) -> bool {
    values
        .get(index)
        .map(|value| value.load(Ordering::Relaxed))
        .unwrap_or(false)
}

fn set_bool(values: &[AtomicBool], index: usize, value: bool) {
    if let Some(slot) = values.get(index) {
        slot.store(value, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::SharedParams;

    fn fill(params: &SharedParams, chain: usize, slots: usize, params_per_slot: usize) {
        for slot in 0..slots {
            for param in 0..params_per_slot {
                params.set_plugin_param(0, chain, slot, param, (slot * 10 + param) as f32);
            }
        }
    }

    #[test]
    fn move_plugin_slot_shifts_parameters_forward() {
        let params = SharedParams::new(1, 1);
        fill(&params, 0, 3, 4);
        params.set_plugin_bypass(0, 0, 0, true);

        params.move_plugin_slot(0, 0, 0, 2);

        assert_eq!(params.plugin_param(0, 0, 0, 0), 10.0);
        assert_eq!(params.plugin_param(0, 0, 1, 0), 20.0);
        assert_eq!(params.plugin_param(0, 0, 2, 0), 0.0);
        assert_eq!(params.plugin_param(0, 0, 2, 3), 3.0);
        assert!(params.plugin_bypass(0, 0, 2));
        assert!(!params.plugin_bypass(0, 0, 0));
    }

    #[test]
    fn move_plugin_slot_shifts_parameters_backward() {
        let params = SharedParams::new(1, 1);
        fill(&params, 0, 3, 4);
        params.set_plugin_bypass(0, 0, 2, true);

        params.move_plugin_slot(0, 0, 2, 0);

        assert_eq!(params.plugin_param(0, 0, 0, 0), 20.0);
        assert_eq!(params.plugin_param(0, 0, 1, 0), 0.0);
        assert_eq!(params.plugin_param(0, 0, 2, 0), 10.0);
        assert!(params.plugin_bypass(0, 0, 0));
        assert!(!params.plugin_bypass(0, 0, 2));
    }

    #[test]
    fn plugin_chains_are_independent() {
        let params = SharedParams::new(1, 1);
        params.set_plugin_param(0, 0, 0, 0, 1.0);
        params.set_plugin_param(0, 1, 0, 0, 2.0);
        params.set_plugin_bypass(0, 1, 0, true);

        assert_eq!(params.plugin_param(0, 0, 0, 0), 1.0);
        assert_eq!(params.plugin_param(0, 1, 0, 0), 2.0);
        assert!(!params.plugin_bypass(0, 0, 0));
        assert!(params.plugin_bypass(0, 1, 0));
    }

    #[test]
    fn sends_are_dense_per_bus() {
        let params = SharedParams::new(2, 3);
        params.set_send_gain(0, 1, 0.5);
        params.set_send_gain(1, 2, 0.25);

        assert_eq!(params.send_gain(0, 0), 0.0);
        assert_eq!(params.send_gain(0, 1), 0.5);
        assert_eq!(params.send_gain(1, 2), 0.25);
        assert_eq!(params.send_gain(1, 1), 0.0);

        params.set_send_delay(0, 1, 12.5);
        assert_eq!(params.send_delay(0, 1), 12.5);
        assert_eq!(params.send_delay(0, 0), 0.0);
    }

    #[test]
    fn bus_plugin_slots_and_mix_are_independent() {
        let params = SharedParams::new(1, 2);
        params.set_bus_plugin_param(1, 0, 0, 3.0);
        params.set_bus_plugin_bypass(1, 0, true);
        params.set_bus_mix_master(1, true);

        assert_eq!(params.bus_plugin_param(1, 0, 0), 3.0);
        assert!(params.bus_plugin_bypass(1, 0));
        assert!(params.bus_mix_master(1));
        assert_eq!(params.bus_plugin_param(0, 0, 0), 0.0);
        assert!(!params.bus_mix_master(0));
    }

    #[test]
    fn bus_return_is_per_bus() {
        let params = SharedParams::new(1, 2);
        params.set_bus_return(1, true);
        params.set_bus_send_gain(1, 0.5);

        assert!(!params.bus_return(0));
        assert!(params.bus_return(1));
        assert_eq!(params.bus_send_gain(1), 0.5);
        assert_eq!(params.bus_send_gain(0), 1.0);
    }
}

/// Transport state derived from the incoming MIDI clock and published to the
/// control thread (and thus the GUI).
pub struct SharedClock {
    locked: AtomicBool,
    playing: AtomicBool,
    bpm: AtomicU32,
    beats: AtomicU64,
    frames: AtomicU64,
}

#[derive(Clone, Copy, Default)]
pub struct ClockSnapshot {
    pub locked: bool,
    pub playing: bool,
    pub bpm: f32,
    pub beats: f64,
    pub seconds: f64,
}

impl SharedClock {
    pub fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            playing: AtomicBool::new(false),
            bpm: AtomicU32::new(120.0f32.to_bits()),
            beats: AtomicU64::new(0f64.to_bits()),
            frames: AtomicU64::new(0),
        }
    }

    pub fn publish(&self, locked: bool, playing: bool, bpm: f32, beats: f64, frames: u64) {
        self.locked.store(locked, Ordering::Relaxed);
        self.playing.store(playing, Ordering::Relaxed);
        self.bpm.store(bpm.to_bits(), Ordering::Relaxed);
        self.beats.store(beats.to_bits(), Ordering::Relaxed);
        self.frames.store(frames, Ordering::Relaxed);
    }

    pub fn snapshot(&self, sample_rate: f32) -> ClockSnapshot {
        let frames = self.frames.load(Ordering::Relaxed);
        ClockSnapshot {
            locked: self.locked.load(Ordering::Relaxed),
            playing: self.playing.load(Ordering::Relaxed),
            bpm: f32::from_bits(self.bpm.load(Ordering::Relaxed)),
            beats: f64::from_bits(self.beats.load(Ordering::Relaxed)),
            seconds: if sample_rate > 0.0 {
                frames as f64 / sample_rate as f64
            } else {
                0.0
            },
        }
    }
}

impl Default for SharedClock {
    fn default() -> Self {
        Self::new()
    }
}
