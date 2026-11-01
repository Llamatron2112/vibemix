//! Background jobs for the control thread.
//!
//! Transient analysis, waveform scans and WAV/session writes can take tens to
//! hundreds of milliseconds. They run on dedicated workers so the control loop
//! keeps answering the GUI and the audio thread is never involved.

use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread;

use vibemix_core::model::MixerModel;
use vibemix_core::recorder::{
    detect_transient, detect_transients_with_sensitivity, peaks, LoopWaveform, Transient,
};

use super::loops::write_loop_wav;

/// Requested view for the loop editor. Kept so a finished analysis can refresh
/// the markers without waiting for another GUI gesture.
#[derive(Debug, Clone)]
pub(super) struct WaveformSpec {
    pub slot: u32,
    pub start: u64,
    pub end: u64,
    pub nudge: Option<i64>,
    pub buckets: u32,
    pub base: bool,
    pub seq: u64,
}

pub(super) struct TakeAnalysisJob {
    pub slot: u32,
    pub generation: u64,
    pub analysis_seq: u64,
    pub raw: Arc<[f32]>,
    pub frames: usize,
    pub sample_rate: u32,
    pub sensitivity: f32,
    pub auto_transient: bool,
    pub base: i64,
}

pub(super) struct SensitivityJob {
    pub slot: u32,
    pub generation: u64,
    pub analysis_seq: u64,
    pub raw: Arc<[f32]>,
    pub frames: usize,
    pub sample_rate: u32,
    pub sensitivity: f32,
}

pub(super) struct WaveformJob {
    pub address: PathBuf,
    pub slot: u32,
    pub generation: u64,
    pub raw: Arc<[f32]>,
    pub frames: usize,
    pub sample_rate: u32,
    pub start: usize,
    pub end: usize,
    pub nudge: usize,
    pub buckets: usize,
    pub base: bool,
    pub seq: u64,
    pub transients: Vec<Transient>,
}

pub(super) struct SaveLoopJob {
    pub slot: u32,
    pub generation: u64,
    pub raw: Arc<[f32]>,
    pub frames: usize,
    pub sample_rate: u32,
    pub path: PathBuf,
    pub bit_depth: u8,
    pub nudge: i64,
    pub normalize: bool,
}

pub(super) struct SaveSessionJob {
    pub model: Box<MixerModel>,
    pub path: String,
}

pub(super) struct AutosaveJob {
    pub model: Box<MixerModel>,
    pub path: PathBuf,
    pub previous: String,
}

pub(super) enum AnalysisJob {
    Take(TakeAnalysisJob),
    Sensitivity(SensitivityJob),
    Waveform(WaveformJob),
}

pub(super) enum IoJob {
    SaveLoop(SaveLoopJob),
    SaveSession(SaveSessionJob),
    Autosave(AutosaveJob),
}

pub(super) struct TakeAnalysis {
    pub slot: u32,
    pub generation: u64,
    pub analysis_seq: u64,
    pub transients: Vec<Transient>,
    pub peaks: Vec<f32>,
    pub transient: i64,
}

pub(super) struct SensitivityAnalysis {
    pub slot: u32,
    pub generation: u64,
    pub analysis_seq: u64,
    pub sensitivity: f32,
    pub transients: Vec<Transient>,
}

pub(super) struct LoopSaved {
    pub slot: u32,
    pub generation: u64,
    pub path: PathBuf,
    pub bit_depth: u8,
    pub nudge: i64,
    pub normalize: bool,
    pub result: Result<(), String>,
}

pub(super) enum WorkerOutput {
    Take(TakeAnalysis),
    Sensitivity(SensitivityAnalysis),
    Waveform {
        address: PathBuf,
        slot: u32,
        generation: u64,
        seq: u64,
        json: Option<String>,
    },
    LoopSaved(LoopSaved),
    SessionSaved {
        path: String,
        result: Result<(), String>,
    },
    Autosave {
        text: Option<String>,
        result: Result<(), String>,
    },
}

impl AnalysisJob {
    fn run(self) -> WorkerOutput {
        match self {
            AnalysisJob::Take(job) => WorkerOutput::Take(run_take(job)),
            AnalysisJob::Sensitivity(job) => WorkerOutput::Sensitivity(run_sensitivity(job)),
            AnalysisJob::Waveform(job) => run_waveform(job),
        }
    }
}

impl IoJob {
    fn run(self) -> WorkerOutput {
        match self {
            IoJob::SaveLoop(job) => WorkerOutput::LoopSaved(run_save_loop(job)),
            IoJob::SaveSession(job) => WorkerOutput::SessionSaved {
                path: job.path.clone(),
                result: super::state::save_session(&job.model, &job.path),
            },
            IoJob::Autosave(job) => run_autosave(job),
        }
    }
}

fn run_autosave(job: AutosaveJob) -> WorkerOutput {
    let text = match toml::to_string_pretty(&job.model) {
        Ok(text) => text,
        Err(error) => {
            return WorkerOutput::Autosave {
                text: None,
                result: Err(error.to_string()),
            };
        }
    };
    if text == job.previous {
        return WorkerOutput::Autosave {
            text: Some(text),
            result: Ok(()),
        };
    }
    let result =
        vibemix_core::config::write_atomic(&job.path, &text).map_err(|error| error.to_string());
    WorkerOutput::Autosave {
        text: Some(text),
        result,
    }
}

fn run_take(job: TakeAnalysisJob) -> TakeAnalysis {
    let transients =
        detect_transients_with_sensitivity(&job.raw, job.frames, job.sample_rate, job.sensitivity);
    let transient = if job.auto_transient {
        let window = job.sample_rate as i64 / 10;
        transients
            .iter()
            .find(|transient| (transient.frame as i64 - job.base).abs() <= window)
            .map(|transient| transient.frame as i64 - job.base)
            .unwrap_or_else(|| {
                detect_transient(&job.raw, job.frames, job.sample_rate, job.base, 100.0)
                    .map(|onset| onset - job.base)
                    .unwrap_or(0)
            })
    } else {
        0
    };
    let peaks = peaks(&job.raw, job.frames, super::LOOP_PEAK_BUCKETS);
    TakeAnalysis {
        slot: job.slot,
        generation: job.generation,
        analysis_seq: job.analysis_seq,
        transients,
        peaks,
        transient,
    }
}

fn run_sensitivity(job: SensitivityJob) -> SensitivityAnalysis {
    let transients =
        detect_transients_with_sensitivity(&job.raw, job.frames, job.sample_rate, job.sensitivity);
    SensitivityAnalysis {
        slot: job.slot,
        generation: job.generation,
        analysis_seq: job.analysis_seq,
        sensitivity: job.sensitivity,
        transients,
    }
}

fn run_waveform(job: WaveformJob) -> WorkerOutput {
    let frames = job.frames;
    if frames == 0 || job.raw.len() < frames * 2 {
        return WorkerOutput::Waveform {
            address: job.address,
            slot: job.slot,
            generation: job.generation,
            seq: job.seq,
            json: None,
        };
    }
    let start = job.start.min(frames);
    let end = job.end.min(frames).max(start);
    let span = end - start;
    let nudge = job.nudge % frames.max(1);
    const RAW_SAMPLE_LIMIT: usize = 32_768;
    let samples = if !job.base && span <= RAW_SAMPLE_LIMIT {
        (start..end)
            .map(|frame| {
                let raw = (nudge + frame) % frames;
                (job.raw[raw * 2] + job.raw[raw * 2 + 1]) * 0.5
            })
            .collect::<Vec<f32>>()
    } else {
        Vec::new()
    };

    let buckets = job.buckets.clamp(512, 32_768);
    let peaks = {
        let mut peaks = vec![0.0f32; buckets];
        for (bucket, peak) in peaks.iter_mut().enumerate() {
            let from = start + span * bucket / buckets;
            let to = (start + span * (bucket + 1) / buckets)
                .max(from + 1)
                .min(end);
            for frame in from..to {
                let raw = (nudge + frame) % frames;
                let index = raw * 2;
                *peak = peak.max(job.raw[index].abs()).max(job.raw[index + 1].abs());
            }
        }
        peaks
    };

    // High-pass energy (first difference) for the visual HF strip. Long
    // windows are skipped; zooming in brings it back.
    const HF_BUCKETS: usize = 2048;
    let hf = if !job.base && span <= job.sample_rate as usize * 10 {
        let mut hf = vec![0.0f32; HF_BUCKETS];
        for (bucket, value) in hf.iter_mut().enumerate() {
            let from = start + span * bucket / HF_BUCKETS;
            let to = (start + span * (bucket + 1) / HF_BUCKETS)
                .max(from + 1)
                .min(end);
            for frame in from..to {
                let raw = (nudge + frame) % frames;
                let previous = (raw + frames - 1) % frames;
                let mono = (job.raw[raw * 2] + job.raw[raw * 2 + 1]) * 0.5;
                let previous = (job.raw[previous * 2] + job.raw[previous * 2 + 1]) * 0.5;
                *value = value.max((mono - previous).abs());
            }
        }
        let peak = hf.iter().copied().fold(0.0f32, f32::max);
        if peak > 1.0e-6 {
            for value in &mut hf {
                *value /= peak;
            }
        }
        hf
    } else {
        Vec::new()
    };

    let payload = LoopWaveform {
        slot: job.slot,
        start: start as u64,
        end: end as u64,
        nudge: nudge as i64,
        seq: job.seq,
        peaks,
        samples,
        hf,
        transients: job.transients,
        base: job.base,
    };
    WorkerOutput::Waveform {
        address: job.address,
        slot: job.slot,
        generation: job.generation,
        seq: job.seq,
        json: serde_json::to_string(&payload).ok(),
    }
}

fn run_save_loop(job: SaveLoopJob) -> LoopSaved {
    let result = write_loop_wav(
        &job.path,
        &job.raw,
        job.frames,
        job.sample_rate,
        job.bit_depth,
        job.nudge,
        job.normalize,
    )
    .map_err(|error| error.to_string());
    LoopSaved {
        slot: job.slot,
        generation: job.generation,
        path: job.path,
        bit_depth: job.bit_depth,
        nudge: job.nudge,
        normalize: job.normalize,
        result,
    }
}

/// Owns the worker threads and their result channel.
pub(super) struct WorkerPool {
    analysis: mpsc::Sender<AnalysisJob>,
    io: mpsc::Sender<IoJob>,
    results: mpsc::Receiver<WorkerOutput>,
}

impl WorkerPool {
    pub fn new() -> Self {
        let (analysis_tx, analysis_rx) = mpsc::channel::<AnalysisJob>();
        let (io_tx, io_rx) = mpsc::channel::<IoJob>();
        let (result_tx, result_rx) = mpsc::channel::<WorkerOutput>();

        let analysis_results = result_tx.clone();
        if let Err(error) = thread::Builder::new()
            .name("vibemix-analysis".to_string())
            .spawn(move || {
                while let Ok(job) = analysis_rx.recv() {
                    if analysis_results.send(job.run()).is_err() {
                        break;
                    }
                }
            })
        {
            tracing::warn!(%error, "failed to start the loop-analysis worker");
        }

        if let Err(error) = thread::Builder::new()
            .name("vibemix-io".to_string())
            .spawn(move || {
                while let Ok(job) = io_rx.recv() {
                    if result_tx.send(job.run()).is_err() {
                        break;
                    }
                }
            })
        {
            tracing::warn!(%error, "failed to start the file-I/O worker");
        }

        Self {
            analysis: analysis_tx,
            io: io_tx,
            results: result_rx,
        }
    }

    pub fn submit_analysis(&self, job: AnalysisJob) -> bool {
        self.analysis.send(job).is_ok()
    }

    pub fn submit_io(&self, job: IoJob) -> bool {
        self.io.send(job).is_ok()
    }

    pub fn try_recv(&self) -> Option<WorkerOutput> {
        self.results.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn take_job(raw: Vec<f32>, frames: usize) -> TakeAnalysisJob {
        TakeAnalysisJob {
            slot: 0,
            generation: 1,
            analysis_seq: 1,
            raw: Arc::from(raw),
            frames,
            sample_rate: 48_000,
            sensitivity: 0.5,
            auto_transient: true,
            base: 0,
        }
    }

    #[test]
    fn take_analysis_returns_peaks_and_onsets() {
        let frames = 4_800;
        let mut raw = vec![0.0f32; frames * 2];
        raw[100] = 1.0;
        raw[101] = 1.0;
        raw[2_000] = 0.5;
        raw[2_001] = 0.5;
        let result = run_take(take_job(raw, frames));
        assert_eq!(result.slot, 0);
        assert_eq!(result.peaks.len(), super::super::LOOP_PEAK_BUCKETS);
        assert!(!result.transients.is_empty());
        assert!(result.peaks.iter().any(|peak| *peak >= 0.9));
    }

    #[test]
    fn waveform_job_serializes_a_full_view() {
        let frames = 100;
        let raw: Arc<[f32]> = Arc::from(vec![0.25f32; frames * 2]);
        let output = run_waveform(WaveformJob {
            address: PathBuf::from("/tmp/vibemix-test.sock"),
            slot: 2,
            generation: 3,
            raw,
            frames,
            sample_rate: 48_000,
            start: 0,
            end: frames,
            nudge: 0,
            buckets: 512,
            base: true,
            seq: 7,
            transients: Vec::new(),
        });
        let WorkerOutput::Waveform {
            slot,
            generation,
            seq,
            json,
            ..
        } = output
        else {
            panic!("expected waveform");
        };
        assert_eq!((slot, generation, seq), (2, 3, 7));
        let payload: LoopWaveform =
            serde_json::from_str(json.as_deref().expect("json")).expect("parse");
        assert_eq!(payload.slot, 2);
        assert_eq!(payload.peaks.len(), 512);
        assert_eq!(payload.seq, 7);
    }

    #[test]
    fn save_loop_job_writes_the_file() {
        let dir = std::env::temp_dir().join(format!("vibemix-worker-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("loop.wav");
        let frames = 8;
        let raw = vec![0.25f32; frames * 2];
        let result = run_save_loop(SaveLoopJob {
            slot: 0,
            generation: 1,
            raw: Arc::from(raw),
            frames,
            sample_rate: 48_000,
            path: path.clone(),
            bit_depth: 24,
            nudge: 0,
            normalize: false,
        });
        result.result.expect("save");
        let (read, read_frames) = super::super::loops::read_loop_wav(&path, 48_000).unwrap();
        assert_eq!(read_frames, frames);
        assert_eq!(read.len(), frames * 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_pool_delivers_results() {
        let pool = WorkerPool::new();
        assert!(pool.submit_analysis(AnalysisJob::Take(take_job(vec![0.0; 960 * 2], 960))));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(WorkerOutput::Take(result)) = pool.try_recv() {
                assert_eq!(result.slot, 0);
                return;
            }
            assert!(Instant::now() < deadline, "worker did not answer");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
