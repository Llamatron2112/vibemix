//! MIDI clock-slave state: derives tempo/transport from an incoming MIDI clock
//! and produces the `time:Position` handed to plugins each block. Everything is
//! allocation-free so it can run on the audio thread.

use vibemix_lv2::{MidiEvent, TimePosition};

pub const MAX_MIDI_EVENTS: usize = 256;

/// Fixed-capacity list of the MIDI events seen in the current block.
pub struct MidiBlock {
    events: [MidiEvent; MAX_MIDI_EVENTS],
    count: usize,
}

impl Default for MidiBlock {
    fn default() -> Self {
        Self {
            events: [MidiEvent::default(); MAX_MIDI_EVENTS],
            count: 0,
        }
    }
}

impl MidiBlock {
    pub fn clear(&mut self) {
        self.count = 0;
    }

    pub fn push(&mut self, event: MidiEvent) {
        if self.count < MAX_MIDI_EVENTS {
            self.events[self.count] = event;
            self.count += 1;
        }
    }

    pub fn as_slice(&self) -> &[MidiEvent] {
        &self.events[..self.count]
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn get(&self, index: usize) -> MidiEvent {
        self.events[index]
    }

    pub fn set(&mut self, index: usize, event: MidiEvent) {
        self.events[index] = event;
    }

    pub fn truncate(&mut self, len: usize) {
        self.count = len.min(self.count);
    }
}

const CLOCK: u8 = 0xF8;
const START: u8 = 0xFA;
const CONTINUE: u8 = 0xFB;
const STOP: u8 = 0xFC;
const RESET: u8 = 0xFF;

/// Number of inter-pulse intervals kept for the tempo-jitter/glitch estimate.
const JITTER_WINDOW: usize = 96;

/// Tracks position and tempo from incoming 24 PPQN MIDI clock.
pub struct Clock {
    pub playing: bool,
    pub locked: bool,
    pub bpm: f32,
    pub beats: f64,
    pub frames_since_start: u64,
    pub abs_frame: i64,
    pub pulses: u64,
    /// Standard deviation of the instantaneous tempo over the recent window.
    pub jitter_bpm: f32,
    /// Pulses in the recent window whose interval was far from the mean.
    pub glitches: u32,
    beats_per_bar: f32,
    sample_rate: f32,
    last_pulse_abs: i64,
    last_seen_abs: i64,
    interval_avg: f64,
    interval_count: u32,
    interval_window: [f64; JITTER_WINDOW],
    interval_len: usize,
    interval_pos: usize,
}

impl Clock {
    pub fn new(sample_rate: f32, beats_per_bar: f32) -> Self {
        Self {
            playing: false,
            locked: false,
            bpm: 120.0,
            beats: 0.0,
            frames_since_start: 0,
            abs_frame: 0,
            pulses: 0,
            jitter_bpm: 0.0,
            glitches: 0,
            beats_per_bar: if beats_per_bar > 0.0 {
                beats_per_bar
            } else {
                4.0
            },
            sample_rate,
            last_pulse_abs: -1,
            last_seen_abs: -1,
            interval_avg: 0.0,
            interval_count: 0,
            interval_window: [0.0; JITTER_WINDOW],
            interval_len: 0,
            interval_pos: 0,
        }
    }

    pub fn set_beats_per_bar(&mut self, beats_per_bar: f32) {
        if beats_per_bar > 0.0 {
            self.beats_per_bar = beats_per_bar;
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if sample_rate > 0.0 {
            self.sample_rate = sample_rate;
        }
    }

    /// Appends one inter-pulse interval and refreshes the jitter/glitch stats.
    fn push_interval(&mut self, interval: f64) {
        self.interval_window[self.interval_pos] = interval;
        self.interval_pos = (self.interval_pos + 1) % JITTER_WINDOW;
        if self.interval_len < JITTER_WINDOW {
            self.interval_len += 1;
        }
        self.update_stability();
    }

    fn reset_stability(&mut self) {
        self.interval_len = 0;
        self.interval_pos = 0;
        self.jitter_bpm = 0.0;
        self.glitches = 0;
    }

    /// Tempo jitter (std-dev of the instantaneous BPM) and pulse glitches over
    /// the recent window. Allocation-free so it can run on the audio thread.
    fn update_stability(&mut self) {
        let n = self.interval_len;
        if n == 0 {
            self.jitter_bpm = 0.0;
            self.glitches = 0;
            return;
        }
        let window = &self.interval_window[..n];
        let mean = window.iter().sum::<f64>() / n as f64;
        if mean <= 0.0 {
            return;
        }
        // Instantaneous tempo from each pulse period, in BPM.
        let scale = 60.0 * self.sample_rate as f64 / 24.0;
        let mean_bpm = window.iter().map(|&iv| scale / iv).sum::<f64>() / n as f64;
        let mut variance = 0.0;
        let mut glitches = 0u32;
        for &iv in window {
            let bpm = scale / iv;
            let delta = bpm - mean_bpm;
            variance += delta * delta;
            if iv < 0.5 * mean || iv > 1.5 * mean {
                glitches += 1;
            }
        }
        self.jitter_bpm = (variance / n as f64).sqrt() as f32;
        self.glitches = glitches;
    }

    /// Process a block's MIDI events (with frame offsets) and advance time.
    pub fn process_block(&mut self, events: &[MidiEvent], frames: usize) {
        let block_start = self.abs_frame;
        for event in events {
            let bytes = event.as_slice();
            let Some(&status) = bytes.first() else {
                continue;
            };
            match status {
                CLOCK => {
                    let abs = block_start + event.time;
                    self.beats += 1.0 / 24.0;
                    self.pulses += 1;
                    if self.last_pulse_abs >= 0 {
                        let interval = (abs - self.last_pulse_abs) as f64;
                        if interval > 0.0 {
                            if self.interval_count == 0 {
                                self.interval_avg = interval;
                            } else {
                                self.interval_avg = self.interval_avg * 0.9 + interval * 0.1;
                            }
                            self.interval_count = self.interval_count.saturating_add(1);
                            let seconds_per_pulse = self.interval_avg / self.sample_rate as f64;
                            let bpm = 60.0 / (24.0 * seconds_per_pulse);
                            if bpm.is_finite() && (20.0..=400.0).contains(&bpm) {
                                self.bpm = bpm as f32;
                            }
                            self.push_interval(interval);
                        }
                    }
                    self.last_pulse_abs = abs;
                    self.last_seen_abs = abs;
                    self.locked = true;
                }
                START => {
                    self.playing = true;
                    self.beats = 0.0;
                    self.frames_since_start = 0;
                    self.last_pulse_abs = -1;
                    self.interval_avg = 0.0;
                    self.interval_count = 0;
                    self.reset_stability();
                }
                CONTINUE => self.playing = true,
                STOP => self.playing = false,
                RESET => {
                    self.beats = 0.0;
                    self.frames_since_start = 0;
                }
                _ => {}
            }
        }

        self.abs_frame += frames as i64;
        if self.playing {
            self.frames_since_start += frames as u64;
        }
        if self.locked
            && self.last_seen_abs >= 0
            && self.abs_frame - self.last_seen_abs > self.sample_rate as i64
        {
            self.locked = false;
        }
    }

    pub fn position(&self) -> TimePosition {
        let beats_per_bar = self.beats_per_bar as f64;
        let bar = if beats_per_bar > 0.0 {
            (self.beats / beats_per_bar).floor() as i64
        } else {
            0
        };
        TimePosition {
            frame: self.frames_since_start as i64,
            beat: self.beats,
            bar,
            bar_beat: (self.beats % beats_per_bar) as f32,
            beats_per_minute: self.bpm,
            beats_per_bar: self.beats_per_bar,
            speed: if self.playing { 1.0 } else { 0.0 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock_at(bpm: f64, sample_rate: f32) -> Clock {
        let mut clock = Clock::new(sample_rate, 4.0);
        clock.playing = true;
        // Pulse interval in frames for the given tempo.
        let interval = (sample_rate as f64 * 60.0 / (bpm * 24.0)).round() as usize;
        for _ in 0..200 {
            clock.process_block(&[MidiEvent::new(0, &[CLOCK])], interval);
        }
        clock
    }

    #[test]
    fn derives_tempo() {
        let clock = clock_at(120.0, 48000.0);
        assert!((clock.bpm - 120.0).abs() < 1.0, "bpm={}", clock.bpm);
        assert!(clock.locked);
    }

    #[test]
    fn reports_tempo_jitter_and_glitches() {
        let sample_rate = 48000.0f32;
        let interval = (sample_rate as f64 * 60.0 / (120.0 * 24.0)).round() as usize;
        let mut clock = Clock::new(sample_rate, 4.0);
        clock.playing = true;
        for _ in 0..50 {
            clock.process_block(&[MidiEvent::new(0, &[CLOCK])], interval);
        }
        assert!(clock.jitter_bpm < 0.01, "jitter={}", clock.jitter_bpm);
        assert_eq!(clock.glitches, 0);

        // Alternating short/long pulses show up as tempo jitter, not glitches.
        for i in 0..24 {
            let step = if i % 2 == 0 { interval + 20 } else { interval - 20 };
            clock.process_block(&[MidiEvent::new(0, &[CLOCK])], step);
        }
        assert!(clock.jitter_bpm > 0.1, "jitter={}", clock.jitter_bpm);
        assert_eq!(clock.glitches, 0);

        // A dropped pulse (one 3x-long block, then a normal pulse) is a glitch.
        clock.process_block(&[MidiEvent::new(0, &[CLOCK])], interval * 3);
        clock.process_block(&[MidiEvent::new(0, &[CLOCK])], interval);
        assert!(clock.glitches >= 1, "glitches={}", clock.glitches);
    }

    #[test]
    fn start_resets_position() {
        let mut clock = Clock::new(48000.0, 4.0);
        clock.beats = 10.0;
        clock.frames_since_start = 12345;
        clock.process_block(&[MidiEvent::new(0, &[START])], 0);
        assert_eq!(clock.beats, 0.0);
        assert_eq!(clock.frames_since_start, 0);
        assert!(clock.playing);
    }

    #[test]
    fn stop_holds_position() {
        let mut clock = clock_at(120.0, 48000.0);
        clock.process_block(&[MidiEvent::new(0, &[STOP])], 48000);
        let beats = clock.beats;
        clock.process_block(&[], 48000);
        assert_eq!(clock.beats, beats);
        assert!((clock.position().speed - 0.0).abs() < f32::EPSILON);
    }
}
