//! Loop recorder types and offline helpers shared by the engine and the GUI.
//!
//! The audio thread captures a loop into an interleaved stereo buffer. The
//! control thread rotates it (circular nudge), analyses it (peaks, transient),
//! and writes it to disk. Nothing here runs on the audio thread except the
//! plain data types.

use serde::{Deserialize, Serialize};

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
    /// `start`/`end` are frames in loop coordinates (after the current nudge).
    /// `preview` requests the whole-loop envelope used while dragging; otherwise
    /// the engine returns a high-resolution window for the editor.
    Waveform {
        slot: u32,
        #[serde(default)]
        start: u64,
        #[serde(default)]
        end: u64,
        #[serde(default)]
        preview: bool,
    },
}

/// Peak envelope for the loop editor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopWaveform {
    pub slot: u32,
    #[serde(default)]
    pub start: u64,
    #[serde(default)]
    pub end: u64,
    #[serde(default)]
    pub preview: bool,
    #[serde(default)]
    pub peaks: Vec<f32>,
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
