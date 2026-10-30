//! Real-time loop capture and playback.
//!
//! The control thread allocates the capture buffer and sends it here through
//! the engine command queue. This module only copies samples and moves owned
//! buffers back to the control thread, so the audio callback never allocates.

use rtrb::Producer;
use vibemix_lv2::MidiEvent;

use crate::params::{RecorderState, SharedRecorder};

/// One finished take, handed to the control thread for trimming, rotation,
/// analysis and WAV writing.
pub struct RecordedTake {
    pub buffer: Box<[f32]>,
    /// Frames actually recorded (the loop length).
    pub frames: usize,
    pub sample_rate: u32,
    pub latency_frames: u32,
    pub loop_beats: f32,
}

pub const MAX_LOOPS: usize = 16;

const CLOCK: u8 = 0xF8;
const START: u8 = 0xFA;
const CONTINUE: u8 = 0xFB;
const STOP: u8 = 0xFC;
const RESET: u8 = 0xFF;

fn status(event: &MidiEvent) -> u8 {
    event.as_slice().first().copied().unwrap_or(0)
}

fn is_bar(beats: f64, beats_per_bar: f64) -> bool {
    if beats_per_bar <= 0.0 {
        return false;
    }
    let phase = (beats / beats_per_bar).fract();
    phase < 1.0e-6 || phase > 1.0 - 1.0e-6
}

/// Offset of the first bar boundary in this block, if any.
///
/// `START` is a bar 1 by definition; otherwise a `CLOCK` landing on a multiple
/// of `beats_per_bar` marks the boundary. Used to quantize loop launches.
pub fn bar_boundary_offset(
    events: &[MidiEvent],
    beats_before: f64,
    beats_per_bar: f32,
) -> Option<usize> {
    let bpb = (beats_per_bar as f64).max(1.0);
    let mut beats = beats_before;
    for event in events {
        match status(event) {
            START => return Some(event.time.max(0) as usize),
            CLOCK => {
                beats += 1.0 / 24.0;
                if is_bar(beats, bpb) {
                    return Some(event.time.max(0) as usize);
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Armed,
    Recording,
}

/// Audio-thread recorder state.
pub struct Recorder {
    state: State,
    buffer: Option<Box<[f32]>>,
    capacity_frames: usize,
    write: usize,
    loop_pulses: u32,
    pulses: u32,
    expected_frames: u64,
    latency_frames: u32,
    loop_beats: f32,
    peak: f32,
    since_pulse: usize,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            buffer: None,
            capacity_frames: 0,
            write: 0,
            loop_pulses: 0,
            pulses: 0,
            expected_frames: 0,
            latency_frames: 0,
            loop_beats: 4.0,
            peak: 0.0,
            since_pulse: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn arm(
        &mut self,
        buffer: Box<[f32]>,
        capacity_frames: usize,
        expected_frames: usize,
        loop_pulses: u32,
        latency_frames: u32,
        loop_beats: f32,
        shared: &SharedRecorder,
    ) {
        self.state = State::Armed;
        self.buffer = Some(buffer);
        self.capacity_frames = capacity_frames;
        self.write = 0;
        self.loop_pulses = loop_pulses;
        self.pulses = 0;
        self.expected_frames = expected_frames as u64;
        self.latency_frames = latency_frames;
        self.loop_beats = loop_beats;
        self.peak = 0.0;
        self.since_pulse = 0;
        shared.set_loop_beats(loop_beats);
        shared.set_status(0, 0, 0.0);
        shared.set_state(RecorderState::Armed);
    }

    pub fn cancel(&mut self, garbage: &mut Producer<Box<[f32]>>, shared: &SharedRecorder) {
        if let Some(buffer) = self.buffer.take() {
            if let Err(buffer) = garbage.push(buffer) {
                std::mem::forget(buffer);
            }
        }
        self.state = State::Idle;
        self.write = 0;
        self.since_pulse = 0;
        shared.set_state(RecorderState::Idle);
        shared.set_status(0, 0, 0.0);
    }

    /// Copy one block of the summed source mix and finish the take when the
    /// loop boundary is reached.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        frames: usize,
        events: &[MidiEvent],
        beats_before: f64,
        beats_per_bar: f32,
        left: &[f32],
        right: &[f32],
        sample_rate: u32,
        shared: &SharedRecorder,
        take_tx: &mut Producer<RecordedTake>,
        garbage: &mut Producer<Box<[f32]>>,
    ) {
        if self.state == State::Idle {
            return;
        }

        // Some clock sources send 24 PPQN without a START. Treat clock pulses
        // as the transport reference; only an explicit STOP aborts a take.
        let saw_clock = events.iter().any(|event| status(event) == CLOCK);
        if self.state == State::Recording {
            if saw_clock {
                self.since_pulse = 0;
            } else {
                self.since_pulse += frames;
                if self.since_pulse > sample_rate as usize {
                    shared.note_discard(crate::params::DISCARD_LOST_CLOCK);
                    self.cancel(garbage, shared);
                    return;
                }
            }
        }

        let bpb = (beats_per_bar as f64).max(1.0);
        let mut beats = beats_before;
        let mut started = self.state == State::Recording;
        let mut start_off: Option<usize> = None;
        let mut end_off: Option<usize> = None;
        let mut pulses = self.pulses;
        let mut aborted = false;

        for event in events {
            let offset = event.time.max(0) as usize;
            match status(event) {
                START => {
                    beats = 0.0;
                    if !started {
                        start_off = Some(offset);
                        started = true;
                        pulses = 0;
                    }
                }
                CLOCK => {
                    beats += 1.0 / 24.0;
                    if !started {
                        if is_bar(beats, bpb) {
                            start_off = Some(offset);
                            started = true;
                            pulses = 0;
                        }
                    } else if start_off.map(|s| offset > s).unwrap_or(true) {
                        pulses += 1;
                        if pulses >= self.loop_pulses {
                            end_off = Some(offset);
                            break;
                        }
                    }
                }
                CONTINUE => {
                    if !started {
                        // Wait for the next bar boundary; a continue does not
                        // reset the bar position.
                    }
                }
                STOP => {
                    aborted = true;
                    break;
                }
                RESET => {
                    beats = 0.0;
                }
                _ => {}
            }
        }

        if aborted {
            self.cancel(garbage, shared);
            return;
        }

        if let Some(offset) = start_off {
            // A new take starts in this block.
            self.state = State::Recording;
            self.write = 0;
            self.pulses = 0;
            self.peak = 0.0;
            shared.set_state(RecorderState::Recording);
            shared.set_status(0, self.expected_frames, 0.0);
            if offset >= frames {
                return;
            }
        }

        if self.state == State::Armed {
            return;
        }

        let from = start_off.unwrap_or(0).min(frames);
        let to = end_off.unwrap_or(frames).min(frames);
        if to > from {
            let count = to - from;
            if self.write + count > self.capacity_frames {
                shared.note_discard(crate::params::DISCARD_OVERFLOW);
                self.cancel(garbage, shared);
                return;
            }
            if let Some(buffer) = self.buffer.as_mut() {
                for i in from..to {
                    let l = left[i];
                    let r = right[i];
                    let index = self.write * 2;
                    buffer[index] = l;
                    buffer[index + 1] = r;
                    self.write += 1;
                    self.peak = self.peak.max(l.abs()).max(r.abs());
                }
            }
        }
        self.pulses = pulses;

        shared.set_status(self.write as u64, self.expected_frames, self.peak);

        if end_off.is_some() || (self.loop_pulses > 0 && self.pulses >= self.loop_pulses) {
            let frames = self.write;
            self.state = State::Idle;
            self.write = 0;
            self.pulses = 0;
            shared.set_state(RecorderState::Idle);
            shared.set_status(0, 0, 0.0);
            if frames == 0 {
                if let Some(buffer) = self.buffer.take() {
                    if let Err(buffer) = garbage.push(buffer) {
                        std::mem::forget(buffer);
                    }
                }
                return;
            }
            let Some(buffer) = self.buffer.take() else {
                return;
            };
            let take = RecordedTake {
                buffer,
                frames,
                sample_rate,
                latency_frames: self.latency_frames,
                loop_beats: self.loop_beats,
            };
            if let Err(take) = take_tx.push(take) {
                let rtrb::PushError::Full(take) = take;
                if let Err(buffer) = garbage.push(take.buffer) {
                    std::mem::forget(buffer);
                }
            }
        }
    }
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

/// A loaded loop buffer and its playback position.
pub struct LoopPlayback {
    pub buffer: Box<[f32]>,
    pub frames: usize,
    pub pos: usize,
    /// Circular read offset: positive values start earlier in the raw buffer.
    pub offset: usize,
    pub playing: bool,
    pub pending: bool,
    pub quantized: bool,
    pub gain: f32,
    pub mute: bool,
    /// Fading out before it stops, so a stop does not click.
    pub stopping: bool,
    /// Smoothed output level (follows `gain`, `mute` and `stopping`).
    level: f32,
}

impl LoopPlayback {
    pub fn new(buffer: Box<[f32]>, frames: usize) -> Self {
        Self {
            buffer,
            frames,
            pos: 0,
            offset: 0,
            playing: false,
            pending: false,
            quantized: true,
            gain: 1.0,
            mute: false,
            stopping: false,
            level: 1.0,
        }
    }

    /// Change the rotation without touching the buffer or playback state.
    pub fn set_offset(&mut self, offset: i64) {
        if self.frames == 0 {
            self.offset = 0;
        } else {
            self.offset = offset.rem_euclid(self.frames as i64) as usize;
        }
    }

    /// Starts playback from the top at full level (a quantized launch should
    /// hit the downbeat, not fade in).
    pub fn start(&mut self) {
        self.pos = 0;
        self.playing = true;
        self.pending = false;
        self.stopping = false;
        self.level = self.target();
    }

    /// Asks the loop to stop; it fades out over a few ms and then goes idle.
    pub fn stop(&mut self) {
        self.pending = false;
        if self.playing {
            self.stopping = true;
        }
    }

    fn target(&self) -> f32 {
        if self.mute || self.stopping {
            0.0
        } else {
            self.gain
        }
    }

    /// Mix this loop into the master accumulator from `start` onwards. `coeff`
    /// is the per-block smoothing coefficient for gain/mute/stop changes. A
    /// muted loop keeps advancing so it stays on the bar grid.
    pub fn mix(
        &mut self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        start: usize,
        frames: usize,
        coeff: f32,
    ) {
        if !self.playing || self.frames == 0 {
            return;
        }
        let from = start.min(frames);
        let count = frames - from;
        let (level_start, level_end) = {
            let target = self.target();
            crate::dsp::smooth_step(&mut self.level, target, coeff)
        };
        if level_start == 0.0 && level_end == 0.0 {
            self.pos = (self.pos + count) % self.frames;
        } else {
            let step = (level_end - level_start) / count.max(1) as f32;
            for (n, i) in (from..frames).enumerate() {
                let level = level_start + step * n as f32;
                let mut read = self.pos + self.offset;
                if read >= self.frames {
                    read -= self.frames;
                }
                let index = read * 2;
                out_l[i] += self.buffer[index] * level;
                out_r[i] += self.buffer[index + 1] * level;
                self.pos += 1;
                if self.pos >= self.frames {
                    self.pos = 0;
                }
            }
        }
        if self.stopping && self.level == 0.0 {
            self.playing = false;
            self.stopping = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::SharedRecorder;

    fn loop_with(samples: &[f32]) -> LoopPlayback {
        let stereo: Vec<f32> = samples.iter().flat_map(|s| [*s, *s]).collect();
        let mut playback = LoopPlayback::new(stereo.into_boxed_slice(), samples.len());
        playback.start();
        playback
    }

    #[test]
    fn muted_loop_stays_on_the_bar_grid() {
        let mut playback = loop_with(&[1.0; 8]);
        playback.mute = true;
        playback.level = 0.0;
        let (mut l, mut r) = ([0.0f32; 6], [0.0f32; 6]);
        playback.mix(&mut l, &mut r, 0, 6, 1.0);
        assert!(l.iter().all(|s| *s == 0.0));
        assert_eq!(playback.pos, 6);
        playback.mix(&mut l, &mut r, 0, 6, 1.0);
        assert_eq!(playback.pos, 4);
    }

    #[test]
    fn offset_rotates_without_stopping() {
        let mut playback = loop_with(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        playback.set_offset(3);
        assert!(playback.playing);
        let (mut l, mut r) = ([0.0f32; 4], [0.0f32; 4]);
        playback.mix(&mut l, &mut r, 0, 4, 1.0);
        assert_eq!(l, [3.0, 4.0, 5.0, 6.0]);
        assert!(playback.playing);

        // Changing the rotation keeps the phase and the playback state.
        playback.set_offset(5);
        assert!(playback.playing);
        l.fill(0.0);
        r.fill(0.0);
        playback.mix(&mut l, &mut r, 0, 4, 1.0);
        assert_eq!(l, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(playback.pos, 0);
    }

    #[test]
    fn stopping_fades_out_then_goes_idle() {
        let mut playback = loop_with(&[1.0; 64]);
        playback.stop();
        let (mut l, mut r) = ([0.0f32; 16], [0.0f32; 16]);
        // Half-way smoothing per block: the tail is audible but shrinking.
        playback.mix(&mut l, &mut r, 0, 16, 0.5);
        assert!(l[0] > l[15] && l[15] > 0.0);
        assert!(playback.playing);
        for _ in 0..40 {
            playback.mix(&mut l, &mut r, 0, 16, 0.5);
        }
        assert!(!playback.playing);
    }

    #[test]
    fn bar_boundary_finds_start_and_clock() {
        let events = [MidiEvent::new(2, &[START])];
        assert_eq!(bar_boundary_offset(&events, 0.0, 4.0), Some(2));

        // One pulse before a 4/4 bar boundary: 95 pulses = 3.9583 beats.
        let events = [MidiEvent::new(7, &[CLOCK])];
        assert_eq!(bar_boundary_offset(&events, 95.0 / 24.0, 4.0), Some(7));
        assert_eq!(bar_boundary_offset(&events, 0.0, 4.0), None);
    }

    #[test]
    fn captures_from_clock_without_start() {
        let shared = SharedRecorder::new();
        let (mut take_tx, mut take_rx) = rtrb::RingBuffer::<RecordedTake>::new(2);
        let (mut garbage_tx, _garbage_rx) = rtrb::RingBuffer::<Box<[f32]>>::new(2);
        let mut recorder = Recorder::new();
        recorder.arm(
            vec![0.0; 8 * 2].into_boxed_slice(),
            8,
            4,
            2,
            0,
            1.0,
            &shared,
        );

        let left = [0.1, 0.2, 0.3, 0.4];
        let right = [1.0, 2.0, 3.0, 4.0];
        // 95 pulses = 3.9583 beats; the first CLOCK lands on the 4/4 bar.
        let events = [
            MidiEvent::new(0, &[CLOCK]),
            MidiEvent::new(1, &[CLOCK]),
            MidiEvent::new(2, &[CLOCK]),
        ];
        recorder.process(
            4,
            &events,
            95.0 / 24.0,
            4.0,
            &left,
            &right,
            48_000,
            &shared,
            &mut take_tx,
            &mut garbage_tx,
        );

        let take = take_rx.pop().expect("clock-only take");
        assert_eq!(take.frames, 2);
        assert!((take.buffer[0] - 0.1).abs() < 1e-6);
        assert_eq!(shared.state(), RecorderState::Idle);
    }

    #[test]
    fn captures_from_start_to_loop_boundary() {
        let shared = SharedRecorder::new();
        let (mut take_tx, mut take_rx) = rtrb::RingBuffer::<RecordedTake>::new(2);
        let (mut garbage_tx, _garbage_rx) = rtrb::RingBuffer::<Box<[f32]>>::new(2);
        let mut recorder = Recorder::new();
        recorder.arm(
            vec![0.0; 8 * 2].into_boxed_slice(),
            8,
            4,
            2,
            0,
            1.0,
            &shared,
        );

        let left = [0.1, 0.2, 0.3, 0.4];
        let right = [1.0, 2.0, 3.0, 4.0];
        let events = [
            MidiEvent::new(0, &[START]),
            MidiEvent::new(2, &[CLOCK]),
            MidiEvent::new(3, &[CLOCK]),
        ];
        recorder.process(
            4,
            &events,
            0.0,
            4.0,
            &left,
            &right,
            48_000,
            &shared,
            &mut take_tx,
            &mut garbage_tx,
        );

        let take = take_rx.pop().expect("take");
        assert_eq!(take.frames, 3);
        assert!((take.buffer[0] - 0.1).abs() < 1e-6);
        assert!((take.buffer[1] - 1.0).abs() < 1e-6);
        assert!((take.buffer[4] - 0.3).abs() < 1e-6);
        assert!((take.buffer[5] - 3.0).abs() < 1e-6);
        assert_eq!(shared.state(), RecorderState::Idle);
    }
}
