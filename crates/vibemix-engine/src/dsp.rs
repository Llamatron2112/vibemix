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

pub fn scale_into(dst: &mut [f32], src: &[f32], gain: f32) {
    for (out, input) in dst.iter_mut().zip(src.iter()) {
        *out = *input * gain;
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
}

impl DelayLine {
    pub fn new(max_samples: usize) -> Self {
        let len = max_samples + 4;
        Self {
            left: vec![0.0; len].into_boxed_slice(),
            right: vec![0.0; len].into_boxed_slice(),
            write: 0,
        }
    }

    pub fn max_samples(&self) -> usize {
        self.left.len() - 4
    }

    fn index(len: usize, value: isize) -> usize {
        let len = len as isize;
        (((value % len) + len) % len) as usize
    }

    /// Writes `input` into the line and reads it back delayed by `delay`
    /// samples (fractional, linearly interpolated) into `output`.
    pub fn process(
        &mut self,
        input_l: &[f32],
        input_r: &[f32],
        output_l: &mut [f32],
        output_r: &mut [f32],
        delay: f32,
    ) {
        let frames = input_l
            .len()
            .min(input_r.len())
            .min(output_l.len())
            .min(output_r.len());
        let len = self.left.len();
        let max = self.max_samples() as f32;
        let delay = delay.clamp(0.0, max);

        for frame in 0..frames {
            let write = self.write;
            self.left[write] = input_l[frame];
            self.right[write] = input_r[frame];

            let base = write as f32 - delay;
            let floor = base.floor() as isize;
            let frac = base - floor as f32;
            let i0 = Self::index(len, floor);
            let i1 = Self::index(len, floor + 1);
            output_l[frame] = self.left[i0] + (self.left[i1] - self.left[i0]) * frac;
            output_r[frame] = self.right[i0] + (self.right[i1] - self.right[i0]) * frac;

            self.write = (write + 1) % len;
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
