//! Loop recorder types and offline helpers shared by the engine and the GUI.
//!
//! The audio thread captures a loop into an interleaved stereo buffer. The
//! control thread rotates it (circular nudge), analyses it (peaks, transient),
//! and writes it to disk. Nothing here runs on the audio thread except the
//! plain data types.

use serde::{Deserialize, Serialize};
use realfft::RealFftPlanner;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderSourceKind {
    Strip,
    Bus,
}

/// Where a strip is tapped. `Output` is only meaningful for buses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderTap {
    /// After input gain, before the pre-fader insert chain.
    Input,
    /// After the pre-fader insert chain, before fader/pan.
    PreFader,
    /// After the post-fader insert chain and fader/pan.
    PostFader,
    /// Bus output (post plugins and fader).
    Output,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecorderSource {
    pub kind: RecorderSourceKind,
    pub id: u32,
    #[serde(default = "default_tap")]
    pub tap: RecorderTap,
}

fn default_tap() -> RecorderTap {
    RecorderTap::PostFader
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderSettings {
    /// Sources summed into each take.
    #[serde(default)]
    pub sources: Vec<RecorderSource>,
    /// WAV bit depth: 16, 24, or 32 (float).
    #[serde(default = "default_bit_depth")]
    pub bit_depth: u8,
    /// Folder for saved loops. Empty means `VIBEMIX_CONFIG_DIR/loops`.
    #[serde(default)]
    pub save_dir: String,
    /// Apply the automatic JACK/plugin latency offset.
    #[serde(default = "default_true")]
    pub auto_latency: bool,
    /// Refine the offset with transient detection.
    #[serde(default = "default_true")]
    pub transient: bool,
    /// Manual nudge in samples on top of the automatic offsets.
    #[serde(default)]
    pub nudge_samples: i64,
    /// Optional seam crossfade applied on playback/export.
    #[serde(default)]
    pub crossfade_ms: f32,
}

fn default_bit_depth() -> u8 {
    24
}

fn default_true() -> bool {
    true
}

impl Default for RecorderSettings {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            bit_depth: 24,
            save_dir: String::new(),
            auto_latency: true,
            transient: true,
            nudge_samples: 0,
            crossfade_ms: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopSlot {
    pub id: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub frames: u64,
    #[serde(default)]
    pub sample_rate: u32,
    #[serde(default)]
    pub loop_beats: f32,
    #[serde(default)]
    pub bpm: f32,
    /// Total applied rotation (positive = audio earlier).
    #[serde(default)]
    pub nudge_samples: i64,
    /// Automatic latency part of `nudge_samples`.
    #[serde(default)]
    pub base_nudge: i64,
    /// Transient-detection part of `nudge_samples`.
    #[serde(default)]
    pub transient_nudge: i64,
    /// User part of `nudge_samples`.
    #[serde(default)]
    pub manual_nudge: i64,
    /// Mono absolute peaks for the editor waveform.
    #[serde(default)]
    pub peaks: Vec<f32>,
    #[serde(default = "default_gain")]
    pub gain: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub playing: bool,
    #[serde(default)]
    pub saved: bool,
}

fn default_gain() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecorderStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub progress: f32,
    #[serde(default)]
    pub frames: u64,
    #[serde(default)]
    pub peak: f32,
    #[serde(default)]
    pub loop_beats: f32,
    /// Default loop folder, sent so the GUI can pre-select it.
    #[serde(default)]
    pub loops_dir: String,
    #[serde(default)]
    pub message: String,
}

impl Default for RecorderStatus {
    fn default() -> Self {
        Self {
            state: "idle".to_string(),
            progress: 0.0,
            frames: 0,
            peak: 0.0,
            loop_beats: 0.0,
            loops_dir: String::new(),
            message: String::new(),
        }
    }
}

/// Actions sent from the GUI to the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecorderAction {
    /// Arm and wait for the next bar 1.
    Arm,
    /// Disarm without recording.
    Cancel,
    /// Stop an in-progress take (it is discarded).
    Stop,
    /// Change the manual nudge of a slot, in samples.
    SetNudge {
        slot: u32,
        manual: i64,
    },
    SetLoopGain {
        slot: u32,
        gain: f32,
    },
    SetLoopMute {
        slot: u32,
        mute: bool,
    },
    LoopPlay {
        slot: u32,
        quantized: bool,
    },
    LoopStop {
        slot: u32,
    },
    LoopDelete {
        slot: u32,
    },
    /// Write the slot to disk. `path` defaults to the loops directory.
    LoopSave {
        slot: u32,
        #[serde(default)]
        path: Option<String>,
        #[serde(default = "default_bit_depth")]
        bit_depth: u8,
    },
    LoopRename {
        slot: u32,
        name: String,
    },
    /// Ask the engine for a waveform of a slot.
    ///
    /// `start`/`end` are frames in loop coordinates. `nudge` overrides the
    /// stored nudge for a drag preview (audio is not changed). `seq` is echoed
    /// so stale replies can be ignored.
    Waveform {
        slot: u32,
        #[serde(default)]
        start: u64,
        #[serde(default)]
        end: u64,
        #[serde(default)]
        nudge: Option<i64>,
        #[serde(default)]
        seq: u64,
        /// Number of peak buckets to return (0 = engine default).
        #[serde(default)]
        buckets: u32,
        /// Return a raw, unrotated whole-loop envelope for the editor fallback.
        #[serde(default)]
        base: bool,
    },
    /// Make a transient the loop start. `frame` is in raw (unrotated) frames.
    SnapTransient { slot: u32, frame: u64 },
    /// Shift the nudge to the nearest low-discontinuity point around the seam.
    SnapSeam { slot: u32 },
}

/// Peak envelope for the loop editor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopWaveform {
    pub slot: u32,
    #[serde(default)]
    pub start: u64,
    #[serde(default)]
    pub end: u64,
    /// Nudge used to rotate the returned peaks.
    #[serde(default)]
    pub nudge: i64,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub peaks: Vec<f32>,
    /// Signed mono samples for a narrow window, so the editor can draw the
    /// actual waveform (and its high-frequency detail) when zoomed in.
    #[serde(default)]
    pub samples: Vec<f32>,
    /// High-pass (first-difference) energy envelope for the window.
    #[serde(default)]
    pub hf: Vec<f32>,
    /// Raw (unrotated) detected onsets; the GUI maps them with `nudge`.
    #[serde(default)]
    pub transients: Vec<Transient>,
    #[serde(default)]
    pub base: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transient {
    pub frame: u64,
    pub strength: f32,
}

/// Rotate an interleaved stereo buffer left by `nudge` samples.
///
/// Positive `nudge` makes the audio start earlier: `out[i] = src[i + nudge]`,
/// wrapping the skipped prefix to the end. Negative values rotate the other
/// way. The input must contain at least `frames * 2` samples.
pub fn rotate_interleaved(src: &[f32], frames: usize, nudge: i64) -> Vec<f32> {
    if frames == 0 {
        return Vec::new();
    }
    let n = frames as i64;
    let shift = nudge.rem_euclid(n) as usize;
    let mut out = vec![0.0f32; frames * 2];
    for i in 0..frames {
        let s = (i + shift) % frames;
        out[i * 2] = src[s * 2];
        out[i * 2 + 1] = src[s * 2 + 1];
    }
    out
}

/// Mono absolute-peak envelope with `count` buckets.
pub fn peaks(src: &[f32], frames: usize, count: usize) -> Vec<f32> {
    if frames == 0 || count == 0 {
        return Vec::new();
    }
    let mut out = vec![0.0f32; count];
    for (bucket, value) in out.iter_mut().enumerate() {
        let start = bucket * frames / count;
        let end = ((bucket + 1) * frames / count).max(start + 1).min(frames);
        let mut peak = 0.0f32;
        for i in start..end {
            let l = src[i * 2].abs();
            let r = src[i * 2 + 1].abs();
            peak = peak.max(l).max(r);
        }
        *value = peak;
    }
    out
}

/// Find the first significant onset within `radius_ms` of `center`.
///
/// Returns an absolute sample index, or `None` when there is no clear onset.
/// The detector uses a short-window RMS and a threshold relative to the
/// loudest window in the search range.
pub fn detect_transient(
    src: &[f32],
    frames: usize,
    sample_rate: u32,
    center: i64,
    radius_ms: f32,
) -> Option<i64> {
    if frames == 0 || sample_rate == 0 {
        return None;
    }
    let radius = (sample_rate as f32 * radius_ms / 1000.0).round() as i64;
    let start = (center - radius).max(0) as usize;
    let end = ((center + radius) as usize).min(frames);
    if end <= start + 8 {
        return None;
    }

    let window = 32usize.min((end - start).max(1));
    let step = (window / 4).max(1);
    let mut best = 0.0f32;
    let mut env = Vec::new();
    let mut pos = start;
    while pos + window <= end {
        let mut sum = 0.0f32;
        for i in pos..pos + window {
            let mono = (src[i * 2] + src[i * 2 + 1]) * 0.5;
            sum += mono * mono;
        }
        let rms = (sum / window as f32).sqrt();
        best = best.max(rms);
        env.push((pos, rms));
        pos += step;
    }
    if best <= 1.0e-5 {
        return None;
    }

    let threshold = best * 0.25;
    let (onset, _) = env.into_iter().find(|(_, rms)| *rms >= threshold)?;
    // Refine to the first sample above half the threshold in that window.
    let refine_end = (onset + window).min(end);
    for i in onset..refine_end {
        let mono = (src[i * 2] + src[i * 2 + 1]) * 0.5;
        if mono.abs() >= threshold * 0.5 {
            return Some(i as i64);
        }
    }
    Some(onset as i64)
}

/// Detect onsets with spectral flux, returned in raw frame coordinates.
///
/// Amplitude-only detection fails on compressed or saturated material where
/// the envelope is nearly flat. This uses a short-time FFT and sums positive
/// log-magnitude changes, weighted towards the higher bins, then picks local
/// maxima against a local adaptive threshold. The result is not capped at a
/// small marker count, so the whole loop is scanned.
pub fn detect_transients(src: &[f32], frames: usize, sample_rate: u32) -> Vec<Transient> {
    if frames < 256 || sample_rate == 0 {
        return Vec::new();
    }
    let fft_size = if frames >= 2048 {
        1024
    } else {
        (frames / 2).next_power_of_two().clamp(128, 1024)
    };
    let hop = (fft_size / 8).max(32);
    if frames < fft_size + hop {
        return Vec::new();
    }
    let mono: Vec<f32> = (0..frames)
        .map(|i| (src[i * 2] + src[i * 2 + 1]) * 0.5)
        .collect();
    let hann: Vec<f32> = (0..fft_size)
        .map(|i| {
            let phase = std::f32::consts::TAU * i as f32 / (fft_size - 1) as f32;
            0.5 - 0.5 * phase.cos()
        })
        .collect();

    let mut planner = RealFftPlanner::<f32>::new();
    let r2c = planner.plan_fft_forward(fft_size);
    let mut input = r2c.make_input_vec();
    let mut spectrum = r2c.make_output_vec();
    let mut previous = vec![0.0f32; spectrum.len()];
    let mut flux = Vec::new();
    let mut starts = Vec::new();
    let mut pos = 0;
    while pos + fft_size <= frames {
        for i in 0..fft_size {
            input[i] = mono[pos + i] * hann[i];
        }
        if r2c.process(&mut input, &mut spectrum).is_err() {
            break;
        }
        let mut value = 0.0f32;
        for (bin, coefficient) in spectrum.iter().enumerate() {
            let magnitude = coefficient.norm();
            let log_magnitude = (1.0 + magnitude * 100.0).ln();
            let difference = (log_magnitude - previous[bin]).max(0.0);
            let weight = 1.0 + 2.0 * bin as f32 / spectrum.len() as f32;
            value += difference * weight;
            previous[bin] = log_magnitude;
        }
        flux.push(value);
        starts.push(pos);
        pos += hop;
    }
    if flux.len() < 3 {
        return Vec::new();
    }

    let peak = flux.iter().copied().fold(0.0f32, f32::max);
    if peak <= 1.0e-6 {
        return Vec::new();
    }
    let normalized: Vec<f32> = flux.iter().map(|value| value / peak).collect();

    // Local mean/std over roughly one second, using prefix sums.
    let local = ((sample_rate as f32 * 0.5 / hop as f32) as usize).max(4);
    let mut prefix = Vec::with_capacity(normalized.len() + 1);
    let mut prefix_sq = Vec::with_capacity(normalized.len() + 1);
    prefix.push(0.0f64);
    prefix_sq.push(0.0f64);
    for value in &normalized {
        let v = *value as f64;
        prefix.push(prefix.last().copied().unwrap_or(0.0) + v);
        prefix_sq.push(prefix_sq.last().copied().unwrap_or(0.0) + v * v);
    }
    let threshold_at = |index: usize| -> f32 {
        let from = index.saturating_sub(local);
        let to = (index + local + 1).min(normalized.len());
        let count = (to - from).max(1) as f64;
        let sum = prefix[to] - prefix[from];
        let sum_sq = prefix_sq[to] - prefix_sq[from];
        let mean = sum / count;
        let variance = (sum_sq / count - mean * mean).max(0.0);
        (mean + 1.5 * variance.sqrt()) as f32
    };

    let min_gap = (sample_rate as usize * 30 / 1000).max(hop);
    let mut out = Vec::new();
    let mut last = usize::MAX;
    for i in 0..normalized.len() {
        let value = normalized[i];
        if value < 0.02 || value < threshold_at(i) {
            continue;
        }
        let from = i.saturating_sub(2);
        let to = (i + 2).min(normalized.len() - 1);
        if normalized[from..=to].iter().any(|other| *other > value) {
            continue;
        }
        let approx = starts[i];
        if last != usize::MAX && approx.saturating_sub(last) < min_gap {
            continue;
        }
        let search_from = approx.saturating_sub(hop);
        let search_to = (approx + fft_size).min(frames);
        let mut local_peak = 0.0f32;
        for j in search_from..search_to {
            let previous = if j > 0 { mono[j - 1] } else { 0.0 };
            local_peak = local_peak.max((mono[j] - previous).abs());
        }
        let refined = if local_peak > 1.0e-6 {
            let threshold = local_peak * 0.2;
            (search_from..search_to)
                .find(|j| {
                    let previous = if *j > 0 { mono[j - 1] } else { 0.0 };
                    (mono[*j] - previous).abs() >= threshold
                })
                .unwrap_or(approx)
        } else {
            approx
        };
        out.push(Transient {
            frame: refined as u64,
            strength: value.clamp(0.0, 1.0),
        });
        last = refined;
        if out.len() >= 4096 {
            break;
        }
    }
    out
}

/// Find a nudge near `nudge` that minimises the seam discontinuity.
///
/// The loop starts at `nudge` and ends at `nudge + frames - 1`; the score
/// combines both edge amplitudes and their difference. The search is limited
/// to `window_ms` so the musical position does not drift.
pub fn snap_seam(
    src: &[f32],
    frames: usize,
    nudge: i64,
    sample_rate: u32,
    window_ms: f32,
) -> i64 {
    if frames == 0 || sample_rate == 0 {
        return nudge;
    }
    let radius = (sample_rate as f32 * window_ms / 1000.0).round().max(1.0) as i64;
    let score_at = |candidate: i64| {
        let start = candidate.rem_euclid(frames as i64) as usize;
        let last = (start + frames - 1) % frames;
        let first = (src[start * 2] + src[start * 2 + 1]) * 0.5;
        let end = (src[last * 2] + src[last * 2 + 1]) * 0.5;
        first.abs() + end.abs() + 0.5 * (first - end).abs()
    };
    let current = score_at(nudge);
    if current <= 0.005 {
        return nudge;
    }
    let mut best = nudge;
    let mut best_score = current;
    for offset in -radius..=radius {
        let candidate = nudge + offset;
        let score = score_at(candidate);
        if score < best_score {
            best_score = score;
            best = candidate;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_wraps_in_both_directions() {
        // Two frames: L/R pairs (1,2) and (3,4).
        let src = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(rotate_interleaved(&src, 2, 0), vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(rotate_interleaved(&src, 2, 1), vec![3.0, 4.0, 1.0, 2.0]);
        assert_eq!(rotate_interleaved(&src, 2, -1), vec![3.0, 4.0, 1.0, 2.0]);
        assert_eq!(rotate_interleaved(&src, 2, 3), vec![3.0, 4.0, 1.0, 2.0]);
    }

    #[test]
    fn peaks_split_the_buffer() {
        let src = [0.0, 0.0, 0.5, -0.5, 1.0, 0.0, 0.0, 0.25];
        let p = peaks(&src, 4, 2);
        assert_eq!(p.len(), 2);
        assert!((p[0] - 0.5).abs() < 1e-6);
        assert!((p[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn detects_multiple_onsets() {
        let frames = 48_000;
        let mut src = vec![0.0f32; frames * 2];
        for onset in [4_800usize, 9_600, 14_400] {
            for i in onset..onset + 32 {
                src[i * 2] = 0.8;
                src[i * 2 + 1] = 0.8;
            }
        }
        let found = detect_transients(&src, frames, 48_000);
        assert!(found.len() >= 3, "found {}", found.len());
        for expected in [4_800u64, 9_600, 14_400] {
            assert!(
                found.iter().any(|t| t.frame.abs_diff(expected) < 256),
                "missing onset {expected}: {:?}",
                found.iter().map(|t| t.frame).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn seam_snap_moves_off_a_peak() {
        let frames = 480;
        let mut src = vec![0.0f32; frames * 2];
        for i in 0..frames {
            let value = (i as f32 * std::f32::consts::TAU / 48.0).sin();
            src[i * 2] = value;
            src[i * 2 + 1] = value;
        }
        // Frame 12 is a peak; a nearby zero crossing should score better.
        let snapped = snap_seam(&src, frames, 12, 48_000, 5.0);
        assert_ne!(snapped, 12);
        let start = snapped.rem_euclid(frames as i64) as usize;
        let first = (src[start * 2] + src[start * 2 + 1]) * 0.5;
        assert!(first.abs() < 0.35, "first={first}");
    }

    #[test]
    fn detects_high_frequency_bursts_on_a_saturated_tone() {
        let sample_rate = 48_000u32;
        let frames = sample_rate as usize * 2;
        let mut src = vec![0.0f32; frames * 2];
        let onsets = [9_600usize, 28_800, 48_000];
        for i in 0..frames {
            let low = (i as f32 * std::f32::consts::TAU * 100.0 / sample_rate as f32).sin() * 0.9;
            let mut value = low;
            for onset in onsets {
                if i >= onset && i < onset + 96 {
                    value += (i as f32 * std::f32::consts::TAU * 8000.0 / sample_rate as f32).sin() * 0.3;
                }
            }
            value = value.clamp(-0.9, 0.9);
            src[i * 2] = value;
            src[i * 2 + 1] = value;
        }
        let found = detect_transients(&src, frames, sample_rate);
        for expected in onsets {
            assert!(
                found.iter().any(|t| t.frame.abs_diff(expected as u64) < 512),
                "missing burst {expected}: {:?}",
                found.iter().map(|t| t.frame).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn seam_snap_moves_before_an_impulse() {
        let frames = 48_000;
        let mut src = vec![0.0f32; frames * 2];
        for i in 4_800..4_832 {
            src[i * 2] = 0.8;
            src[i * 2 + 1] = 0.8;
        }
        let snapped = snap_seam(&src, frames, 4_800, 48_000, 2.0);
        assert!(snapped < 4_800, "snapped={snapped}");
        assert!(snapped >= 4_800 - 96, "snapped={snapped}");
    }

    #[test]
    fn transient_finds_a_late_click() {
        let frames = 4800;
        let mut src = vec![0.0f32; frames * 2];
        for i in 300..360 {
            src[i * 2] = 0.8;
            src[i * 2 + 1] = 0.8;
        }
        let found = detect_transient(&src, frames, 48_000, 0, 50.0).unwrap();
        assert!((found - 300).abs() <= 8, "found {found}");
    }
}
