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
        playing: bool,
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

        if self.state == State::Recording && !playing {
            // The transport stopped mid-take; discard it.
            self.cancel(garbage, shared);
            return;
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
                    if started {
                        aborted = true;
                        break;
                    }
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
                tracing::warn!(
                    needed = self.write + count,
                    capacity = self.capacity_frames,
                    "recorder buffer overflow; take discarded"
                );
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
    pub playing: bool,
    pub pending: bool,
    pub quantized: bool,
    pub gain: f32,
    pub mute: bool,
}

impl LoopPlayback {
    pub fn new(buffer: Box<[f32]>, frames: usize) -> Self {
        Self {
            buffer,
            frames,
            pos: 0,
            playing: false,
            pending: false,
            quantized: true,
            gain: 1.0,
            mute: false,
        }
    }

    /// Mix this loop into the master accumulator from `start` onwards.
    pub fn mix(&mut self, out_l: &mut [f32], out_r: &mut [f32], start: usize, frames: usize) {
        if !self.playing || self.mute || self.frames == 0 {
            return;
        }
        let gain = self.gain;
        for i in start.min(frames)..frames {
            let index = self.pos * 2;
            out_l[i] += self.buffer[index] * gain;
            out_r[i] += self.buffer[index + 1] * gain;
            self.pos += 1;
            if self.pos >= self.frames {
                self.pos = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::SharedRecorder;

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
            true,
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
