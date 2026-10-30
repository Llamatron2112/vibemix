pub fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

/// Enables flush-to-zero / denormals-are-zero on the current thread (x86_64).
/// Cheap and idempotent; intended to be called from the audio callback.
pub fn flush_denormals() {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::asm;
        const FTZ: u32 = 1 << 15;
        const DAZ: u32 = 1 << 6;
        let mut csr: u32 = 0;
        unsafe {
            asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack, preserves_flags));
            csr |= FTZ | DAZ;
            asm!("ldmxcsr [{}]", in(reg) &csr, options(nostack, preserves_flags));
        }
    }
}

/// Time constant for fader/pan/mute style parameter smoothing.
pub const GAIN_SMOOTH_MS: f32 = 10.0;
/// Time constant for send-delay smoothing (slower: moving a delay tap pitch
/// shifts the signal while it travels).
pub const DELAY_SMOOTH_MS: f32 = 40.0;

/// Smoothing coefficient for one block of `frames` samples with a one-pole
/// time constant of `tau_ms`. Independent of the host's block size.
pub fn smoothing_coeff(frames: usize, sample_rate: f32, tau_ms: f32) -> f32 {
    if sample_rate <= 0.0 || tau_ms <= 0.0 {
        return 1.0;
    }
    1.0 - (-(frames as f32) / (tau_ms * 0.001 * sample_rate)).exp()
}

/// Advances `current` towards `target` by one block and returns the
/// `(start, end)` values to ramp across the block. Snaps to the target once it
/// is close, so a muted or unity parameter reaches exactly 0 or 1.
pub fn smooth_step(current: &mut f32, target: f32, coeff: f32) -> (f32, f32) {
    let start = *current;
    let mut next = start + (target - start) * coeff;
    if (target - next).abs() <= 1.0e-5 * target.abs().max(1.0) {
        next = target;
    }
    *current = next;
    (start, next)
}

/// Linear ramp value at `index` of `frames` from `start` towards `end`.
#[inline]
fn ramp_at(start: f32, step: f32, index: usize) -> f32 {
    start + step * index as f32
}

pub fn apply_gain_ramp(signal: &mut [f32], start: f32, end: f32) {
    if start == end {
        apply_gain(signal, start);
        return;
    }
    let step = (end - start) / signal.len().max(1) as f32;
    for (index, sample) in signal.iter_mut().enumerate() {
        *sample *= ramp_at(start, step, index);
    }
}

pub fn scale_into_ramp(dst: &mut [f32], src: &[f32], start: f32, end: f32) {
    if start == end {
        scale_into(dst, src, start);
        return;
    }
    let step = (end - start) / src.len().max(1) as f32;
    for (index, (out, input)) in dst.iter_mut().zip(src.iter()).enumerate() {
        *out = *input * ramp_at(start, step, index);
    }
}

pub fn add_scaled_ramp(dst: &mut [f32], src: &[f32], start: f32, end: f32) {
    if start == end {
        add_scaled(dst, src, start);
        return;
    }
    let step = (end - start) / src.len().max(1) as f32;
    for (index, (out, input)) in dst.iter_mut().zip(src.iter()).enumerate() {
        *out += *input * ramp_at(start, step, index);
    }
}

pub fn scale_into(dst: &mut [f32], src: &[f32], gain: f32) {
    for (out, input) in dst.iter_mut().zip(src.iter()) {
        *out = *input * gain;
    }
}

/// Replaces NaN/Inf samples with silence so one misbehaving plugin cannot
/// poison the buses (and the hardware outputs) downstream of it.
pub fn sanitize(signal: &mut [f32]) {
    for sample in signal.iter_mut() {
        if !sample.is_finite() {
            *sample = 0.0;
        }
    }
}

pub fn apply_gain(signal: &mut [f32], gain: f32) {
    for sample in signal.iter_mut() {
        *sample *= gain;
    }
}

pub fn add_scaled(dst: &mut [f32], src: &[f32], gain: f32) {
    for (out, input) in dst.iter_mut().zip(src.iter()) {
        *out += *input * gain;
    }
}

pub fn peak(signal: &[f32]) -> f32 {
    signal
        .iter()
        .fold(0.0f32, |acc, sample| acc.max(sample.abs()))
}

/// A stereo delay line with a fractional (linearly interpolated) read tap.
///
/// The buffer is allocated once at construction; `process` performs no
/// allocation and is safe to call on the audio thread.
pub struct DelayLine {
    left: Box<[f32]>,
    right: Box<[f32]>,
    write: usize,
    /// Whether the ring holds a continuous recent history. A line that is not
    /// being used (zero delay, or its send is off) is neither written nor
    /// read, and is cleared before its next use so stale audio never replays.
    primed: bool,
    /// Whether the ring has ever been written. A never-used line is still the
    /// zeroed allocation it started as, so priming it need not touch its pages
    /// (which would fault them all in at once on the audio thread).
    dirty: bool,
}

impl DelayLine {
    pub fn new(max_samples: usize) -> Self {
        let len = max_samples + 4;
        Self {
            left: vec![0.0; len].into_boxed_slice(),
            right: vec![0.0; len].into_boxed_slice(),
            write: 0,
            primed: false,
            dirty: false,
        }
    }

    pub fn max_samples(&self) -> usize {
        self.left.len() - 4
    }

    /// Marks the ring stale (the send feeding it is idle).
    pub fn invalidate(&mut self) {
        self.primed = false;
    }

    fn prime(&mut self) {
        if self.dirty {
            self.left.fill(0.0);
            self.right.fill(0.0);
            self.dirty = false;
        }
        self.write = 0;
        self.primed = true;
    }

    /// Writes `input` into the line and reads it back delayed by `delay`
    /// samples (fractional, linearly interpolated) into `output`.
    #[cfg(test)]
    pub fn process(
        &mut self,
        input_l: &[f32],
        input_r: &[f32],
        output_l: &mut [f32],
        output_r: &mut [f32],
        delay: f32,
    ) {
        self.process_ramp(input_l, input_r, output_l, output_r, delay, delay);
    }

    /// Like [`process`](Self::process), moving the delay linearly from
    /// `delay_start` to `delay_end` across the block so changes are click-free.
    pub fn process_ramp(
        &mut self,
        input_l: &[f32],
        input_r: &[f32],
        output_l: &mut [f32],
        output_r: &mut [f32],
        delay_start: f32,
        delay_end: f32,
    ) {
        let frames = input_l
            .len()
            .min(input_r.len())
            .min(output_l.len())
            .min(output_r.len());
        let len = self.left.len();
        let max = self.max_samples() as f32;
        let delay_start = delay_start.clamp(0.0, max);
        let delay_end = delay_end.clamp(0.0, max);
        if delay_start == 0.0 && delay_end == 0.0 {
            // No delay: a straight copy, without touching the ring at all.
            output_l[..frames].copy_from_slice(&input_l[..frames]);
            output_r[..frames].copy_from_slice(&input_r[..frames]);
            self.primed = false;
            return;
        }
        if !self.primed {
            self.prime();
        }
        self.dirty = true;
        let step = (delay_end - delay_start) / frames.max(1) as f32;

        for frame in 0..frames {
            let write = self.write;
            self.left[write] = input_l[frame];
            self.right[write] = input_r[frame];

            let delay = delay_start + step * frame as f32;
            let base = write as f32 - delay;
            let floor = base.floor() as isize;
            let frac = base - floor as f32;
            // `delay <= max < len`, so the read position wraps at most once.
            let i0 = if floor < 0 {
                (floor + len as isize) as usize
            } else {
                floor as usize
            };
            let i1 = if i0 + 1 == len { 0 } else { i0 + 1 };
            output_l[frame] = self.left[i0] + (self.left[i1] - self.left[i0]) * frac;
            output_r[frame] = self.right[i0] + (self.right[i1] - self.right[i0]) * frac;

            self.write = if write + 1 == len { 0 } else { write + 1 };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pan_center_is_equal_power() {
        let (left, right) = pan_gains(0.0);
        assert!((left - right).abs() < 1.0e-6);
        assert!((left * left + right * right - 1.0).abs() < 1.0e-5);
    }

    #[test]
    fn pan_hard_left_silences_right() {
        let (left, right) = pan_gains(-1.0);
        assert!((left - 1.0).abs() < 1.0e-6);
        assert!(right.abs() < 1.0e-6);
    }

    #[test]
    fn scale_into_applies_gain() {
        let input = [1.0f32, -0.5, 0.25];
        let mut output = [0.0f32; 3];
        scale_into(&mut output, &input, 0.5);
        assert_eq!(output, [0.5, -0.25, 0.125]);
    }

    #[test]
    fn scale_into_zero_is_silence() {
        let input = [1.0f32, -0.5, 0.25];
        let mut output = [1.0f32; 3];
        scale_into(&mut output, &input, 0.0);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn add_scaled_accumulates() {
        let mut destination = [0.5f32, 0.5, 0.5];
        add_scaled(&mut destination, &[1.0, 0.0, -1.0], 2.0);
        assert_eq!(destination, [2.5, 0.5, -1.5]);
    }

    #[test]
    fn peak_finds_maximum() {
        assert_eq!(peak(&[0.0, -0.75, 0.5]), 0.75);
    }

    #[test]
    fn smoothing_is_block_size_independent() {
        let rate = 48_000.0;
        let run = |block: usize| {
            let mut value = 1.0f32;
            let coeff = smoothing_coeff(block, rate, GAIN_SMOOTH_MS);
            for _ in 0..(rate as usize / block) {
                smooth_step(&mut value, 0.0, coeff);
            }
            value
        };
        // One second of smoothing is far past the 10 ms time constant at any
        // block size, so a mute is complete and exact.
        assert_eq!(run(64), 0.0);
        assert_eq!(run(1024), 0.0);
    }

    #[test]
    fn smooth_step_reaches_target_within_tens_of_ms() {
        let mut value = 1.0f32;
        let coeff = smoothing_coeff(256, 48_000.0, GAIN_SMOOTH_MS);
        // 256 frames is 5.3 ms; 20 blocks is about 107 ms.
        for _ in 0..20 {
            smooth_step(&mut value, 0.0, coeff);
        }
        assert!(value < 1.0e-3);
    }

    #[test]
    fn sanitize_zeroes_non_finite_samples() {
        let mut signal = [0.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.25];
        sanitize(&mut signal);
        assert_eq!(signal, [0.5, 0.0, 0.0, 0.0, -0.25]);
    }

    #[test]
    fn ramp_covers_start_to_end() {
        let mut signal = [1.0f32; 4];
        apply_gain_ramp(&mut signal, 0.0, 1.0);
        assert_eq!(signal, [0.0, 0.25, 0.5, 0.75]);
    }

    #[test]
    fn delay_ramp_matches_fixed_delay_when_flat() {
        let input = [1.0, 0.5, 0.25, 0.0, 0.0, 0.0];
        let (mut a_l, mut a_r) = ([0.0f32; 6], [0.0f32; 6]);
        let (mut b_l, mut b_r) = ([0.0f32; 6], [0.0f32; 6]);
        DelayLine::new(16).process(&input, &input, &mut a_l, &mut a_r, 2.0);
        DelayLine::new(16).process_ramp(&input, &input, &mut b_l, &mut b_r, 2.0, 2.0);
        assert_eq!(a_l, b_l);
    }

    #[test]
    fn idle_line_does_not_replay_stale_audio() {
        let mut line = DelayLine::new(16);
        let loud = [1.0f32; 8];
        let (mut out_l, mut out_r) = ([0.0f32; 8], [0.0f32; 8]);
        line.process(&loud, &loud, &mut out_l, &mut out_r, 4.0);
        // The send goes idle, then comes back with a delay: the old audio must
        // not be heard again.
        line.invalidate();
        let silence = [0.0f32; 8];
        line.process(&silence, &silence, &mut out_l, &mut out_r, 4.0);
        assert!(out_l.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn delay_zero_is_passthrough() {
        let mut line = DelayLine::new(16);
        let input = [0.25, -0.5, 1.0, 0.0];
        let (mut out_l, mut out_r) = ([0.0f32; 4], [0.0f32; 4]);
        line.process(&input, &input, &mut out_l, &mut out_r, 0.0);
        assert_eq!(out_l, input);
        assert_eq!(out_r, input);
    }

    #[test]
    fn delay_integer_shifts_impulse() {
        let mut line = DelayLine::new(16);
        let input = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let (mut out_l, mut out_r) = ([0.0f32; 8], [0.0f32; 8]);
        line.process(&input, &input, &mut out_l, &mut out_r, 3.0);
        assert_eq!(out_l, [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn delay_fractional_interpolates() {
        let mut line = DelayLine::new(16);
        let input = [1.0, 0.0, 0.0, 0.0];
        let (mut out_l, mut out_r) = ([0.0f32; 4], [0.0f32; 4]);
        line.process(&input, &input, &mut out_l, &mut out_r, 2.5);
        assert!((out_l[2] - 0.5).abs() < 1.0e-6);
        assert!((out_l[3] - 0.5).abs() < 1.0e-6);
    }
}
