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

    /// Stable insertion sort by frame offset, used when generated clock events
    /// are merged with incoming MIDI.
    pub fn sort_by_time(&mut self) {
        for index in 1..self.count {
            let event = self.events[index];
            let mut position = index;
            while position > 0 && self.events[position - 1].time > event.time {
                self.events[position] = self.events[position - 1];
                position -= 1;
            }
            self.events[position] = event;
        }
    }
}

pub(crate) const CLOCK: u8 = 0xF8;
pub(crate) const START: u8 = 0xFA;
pub(crate) const CONTINUE: u8 = 0xFB;
pub(crate) const STOP: u8 = 0xFC;
const RESET: u8 = 0xFF;

/// System transport messages (start/continue/stop, song position/select),
/// excluding the clock pulse itself.
pub fn is_transport_status(status: u8) -> bool {
    matches!(status, 0xF2 | 0xF3 | 0xFA | 0xFB | 0xFC)
}

/// Any MIDI sync/realtime message: transport plus clock and the remaining
/// system realtime bytes.
pub fn is_sync_status(status: u8) -> bool {
    status >= 0xF8 || is_transport_status(status)
}

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
            // Forget the tempo history: the first pulse after the gap would
            // otherwise measure an interval as long as the whole outage.
            self.last_pulse_abs = -1;
            self.interval_avg = 0.0;
            self.interval_count = 0;
            self.reset_stability();
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

/// Internal MIDI clock generator.
///
/// The pulse phase is integer (`1 / (2500 * sample_rate)` of a pulse) and
/// advanced one frame at a time, so 24 PPQN spacing is exact and does not
/// drift. Each pulse is queued with its frame offset inside the block.
pub struct MasterClock {
    playing: bool,
    bpm_milli: u32,
    pulses: u64,
    phase: u64,
    frames_since_start: u64,
}

impl Default for MasterClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MasterClock {
    pub fn new() -> Self {
        Self {
            playing: false,
            bpm_milli: 120_000,
            pulses: 0,
            phase: 0,
            frames_since_start: 0,
        }
    }

    /// Forget the position and stop. The next play emits `START`.
    pub fn reset(&mut self) {
        self.playing = false;
        self.pulses = 0;
        self.phase = 0;
        self.frames_since_start = 0;
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn bpm(&self) -> f32 {
        self.bpm_milli as f32 / 1000.0
    }

    fn beats_at(&self, sample_rate: u32) -> f64 {
        if sample_rate == 0 {
            return 0.0;
        }
        let den = 2500u64 * sample_rate as u64;
        self.pulses as f64 / 24.0 + (self.phase as f64 / den as f64) / 24.0
    }

    /// Build a plugin transport position for the block that starts at `beats`.
    pub fn position(
        &self,
        beats: f64,
        frames: u64,
        beats_per_bar: f32,
        playing: bool,
    ) -> TimePosition {
        let bpb = if beats_per_bar > 0.0 {
            beats_per_bar as f64
        } else {
            4.0
        };
        TimePosition {
            frame: frames as i64,
            beat: beats,
            bar: (beats / bpb).floor() as i64,
            bar_beat: (beats % bpb) as f32,
            beats_per_minute: self.bpm(),
            beats_per_bar: bpb as f32,
            speed: if playing { 1.0 } else { 0.0 },
        }
    }

    /// Returns the exact beat position, the pulse-aligned beat position and
    /// the frame count at the start of the block.
    ///
    /// Bar detection (the recorder and quantized loop starts) must start from
    /// `pulse_beats`, because clock events advance it by exactly `1/24`.
    /// `exact_beats` includes the fractional phase and is for plugin time.
    /// A stop resets the position to zero, so the next play always sends
    /// `START`.
    pub fn process(
        &mut self,
        frames: usize,
        sample_rate: u32,
        playing: bool,
        bpm_milli: u32,
        out: &mut MidiBlock,
    ) -> (f64, f64, u64) {
        self.bpm_milli = bpm_milli.clamp(20_000, 400_000);
        if playing != self.playing {
            if playing {
                out.push(MidiEvent::new(0, &[START]));
                self.pulses = 0;
                self.phase = 0;
                self.frames_since_start = 0;
                self.playing = true;
            } else {
                out.push(MidiEvent::new(0, &[STOP]));
                self.playing = false;
                self.pulses = 0;
                self.phase = 0;
                self.frames_since_start = 0;
                return (0.0, 0.0, 0);
            }
        }
        if !self.playing {
            return (0.0, 0.0, 0);
        }

        let beats_before = self.beats_at(sample_rate);
        let pulse_beats = self.pulses as f64 / 24.0;
        let frames_before = self.frames_since_start;
        let den = 2500u64 * sample_rate as u64;
        if den > 0 {
            let increment = self.bpm_milli as u64;
            for frame in 0..frames {
                self.phase += increment;
                if self.phase >= den {
                    self.phase -= den;
                    self.pulses += 1;
                    out.push(MidiEvent::new(frame as i64, &[CLOCK]));
                }
            }
        }
        self.frames_since_start += frames as u64;
        (beats_before, pulse_beats, frames_before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_transport_and_sync_messages() {
        for status in [0xF2, 0xF3, 0xFA, 0xFB, 0xFC] {
            assert!(is_transport_status(status), "{status:#x}");
            assert!(is_sync_status(status), "{status:#x}");
        }
        for status in [0xF8, 0xF9, 0xFE, 0xFF] {
            assert!(!is_transport_status(status), "{status:#x}");
            assert!(is_sync_status(status), "{status:#x}");
        }
        for status in [0x90, 0xB0, 0xE0] {
            assert!(!is_transport_status(status), "{status:#x}");
            assert!(!is_sync_status(status), "{status:#x}");
        }
    }

    #[test]
    fn master_clock_pulses_are_exact() {
        let sample_rate = 48_000;
        let mut master = MasterClock::new();
        let mut out = MidiBlock::default();

        // Play from zero: START at frame 0, then no pulse at frame 0.
        let (beats, pulse_beats, frames) = master.process(0, sample_rate, true, 120_000, &mut out);
        assert_eq!(beats, 0.0);
        assert_eq!(pulse_beats, 0.0);
        assert_eq!(frames, 0);
        assert_eq!(out.as_slice()[0].as_slice(), &[START]);

        // One second at 120 BPM is 2 beats = 48 pulses, one every 1000 frames.
        out.clear();
        let (beats, pulse_beats, frames) =
            master.process(48_000, sample_rate, true, 120_000, &mut out);
        assert_eq!(beats, 0.0);
        assert_eq!(pulse_beats, 0.0);
        assert_eq!(frames, 0);
        assert_eq!(out.len(), 48);
        assert_eq!(out.get(0).time, 999);
        assert_eq!(out.get(47).time, 47_999);
        assert!((master.beats_at(sample_rate) - 2.0).abs() < 1.0e-9);

        // The next block starts at the pulse-aligned beat 2.0, not a fraction.
        out.clear();
        let (_, pulse_beats, _) = master.process(0, sample_rate, true, 120_000, &mut out);
        assert_eq!(pulse_beats, 2.0);

        // Stop resets the position and emits STOP.
        out.clear();
        let (beats, pulse_beats, frames) = master.process(0, sample_rate, false, 120_000, &mut out);
        assert_eq!(beats, 0.0);
        assert_eq!(pulse_beats, 0.0);
        assert_eq!(frames, 0);
        assert_eq!(out.as_slice()[0].as_slice(), &[STOP]);
        assert_eq!(master.beats_at(sample_rate), 0.0);
        assert!(!master.is_playing());

        // Play again always starts from zero.
        out.clear();
        master.process(0, sample_rate, true, 120_000, &mut out);
        assert_eq!(out.as_slice()[0].as_slice(), &[START]);
    }

    #[test]
    fn master_clock_tracks_tempo_changes_without_drift() {
        let sample_rate = 48_000;
        let mut master = MasterClock::new();
        let mut out = MidiBlock::default();
        master.process(0, sample_rate, true, 120_000, &mut out);

        // 10 s at 120 BPM = 480 pulses.
        let mut pulses = 0;
        for _ in 0..10 {
            out.clear();
            master.process(48_000, sample_rate, true, 120_000, &mut out);
            pulses += out.len();
        }
        assert_eq!(pulses, 480);
        assert!((master.beats_at(sample_rate) - 20.0).abs() < 1.0e-9);

        // Half a second at 60 BPM adds 12 pulses (0.5 beat).
        out.clear();
        master.process(24_000, sample_rate, true, 60_000, &mut out);
        assert_eq!(out.len(), 12);
        assert!((master.beats_at(sample_rate) - 20.5).abs() < 1.0e-9);
    }

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
            let step = if i % 2 == 0 {
                interval + 20
            } else {
                interval - 20
            };
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
    fn tempo_recovers_cleanly_after_the_clock_drops_out() {
        let sample_rate = 48000.0f32;
        let mut clock = clock_at(120.0, sample_rate);
        // The clock stops for three seconds, then resumes at the same tempo.
        clock.process_block(&[], sample_rate as usize * 3);
        assert!(!clock.locked);
        let interval = (sample_rate as f64 * 60.0 / (120.0 * 24.0)).round() as usize;
        clock.process_block(&[MidiEvent::new(0, &[CLOCK])], interval);
        clock.process_block(&[MidiEvent::new(0, &[CLOCK])], interval);
        assert!((clock.bpm - 120.0).abs() < 1.0, "bpm={}", clock.bpm);
        assert_eq!(clock.glitches, 0);
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
