pub fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
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
}
