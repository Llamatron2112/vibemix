//! Split out of the controller; see `mod.rs`.

use super::*;

pub(super) fn loops_dir() -> PathBuf {
    config_dir().join("loops")
}

pub(super) fn timestamp_slug() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    if now == -1 {
        return "0".to_string();
    }
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return now.to_string();
    }
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn pending_from_spec(spec: &WaveformSpec) -> PendingWaveform {
    PendingWaveform {
        slot: spec.slot,
        start: spec.start,
        end: spec.end,
        nudge: spec.nudge,
        seq: spec.seq,
        buckets: spec.buckets,
        base: spec.base,
    }
}

pub(super) fn sanitize_file_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        "loop".to_string()
    } else {
        out
    }
}

/// Write an interleaved stereo loop, applying the circular nudge first.
///
/// When `normalize` is set, the rotated buffer is scaled so its peak sits at
/// -1 dBFS (0.891251). Silent buffers are left alone.
pub(super) fn write_loop_wav(
    path: &Path,
    raw: &[f32],
    frames: usize,
    sample_rate: u32,
    bit_depth: u8,
    nudge: i64,
    normalize: bool,
) -> anyhow::Result<()> {
    // Same rule as sessions: never replace an existing file that is not a WAV.
    if let Ok(mut existing) = fs::File::open(path) {
        use std::io::Read;
        let mut magic = [0u8; 4];
        if existing.read(&mut magic).unwrap_or(0) > 0 && &magic != b"RIFF" {
            anyhow::bail!("refusing to overwrite {}: not a WAV file", path.display());
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let depth = match bit_depth {
        16 | 24 | 32 => bit_depth,
        _ => 24,
    };
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: depth as u16,
        sample_format: if depth == 32 {
            hound::SampleFormat::Float
        } else {
            hound::SampleFormat::Int
        },
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    let mut rotated = rotate_interleaved(raw, frames, nudge);
    if normalize {
        let peak = rotated
            .iter()
            .copied()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        // About -60 dBFS: treating near-silence as normalizable would just
        // amplify noise.
        if peak > 1.0e-3 {
            let gain = 0.891_250_9 / peak;
            for sample in &mut rotated {
                *sample *= gain;
            }
        }
    }
    for sample in rotated {
        match depth {
            16 => writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)?,
            24 => writer.write_sample((sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32)?,
            _ => writer.write_sample(sample)?,
        }
    }
    writer.finalize()?;
    Ok(())
}

/// Read a stereo WAV as interleaved f32. Keeps the engine sample rate fixed.
pub(super) fn read_loop_wav(path: &Path, expected_rate: u32) -> anyhow::Result<(Vec<f32>, usize)> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    if spec.channels != 2 {
        anyhow::bail!("expected a stereo WAV, found {} channels", spec.channels);
    }
    if spec.sample_rate != expected_rate {
        anyhow::bail!(
            "loop is {} Hz but the engine runs at {} Hz",
            spec.sample_rate,
            expected_rate
        );
    }
    const MAX_LOOP_SECONDS: u64 = 300;
    let max_frames = expected_rate as u64 * MAX_LOOP_SECONDS;
    if reader.len() as u64 > max_frames {
        anyhow::bail!("loop is longer than {} minutes", MAX_LOOP_SECONDS / 60);
    }

    let mut out = Vec::new();
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => {
            for sample in reader.samples::<f32>() {
                out.push(sample?);
            }
        }
        (hound::SampleFormat::Int, 16) => {
            for sample in reader.samples::<i16>() {
                out.push(sample? as f32 / 32_768.0);
            }
        }
        (hound::SampleFormat::Int, 24) => {
            for sample in reader.samples::<i32>() {
                out.push(sample? as f32 / 8_388_608.0);
            }
        }
        (hound::SampleFormat::Int, 32) => {
            for sample in reader.samples::<i32>() {
                out.push(sample? as f32 / 2_147_483_648.0);
            }
        }
        (format, bits) => anyhow::bail!("unsupported WAV format: {format:?}, {bits} bits"),
    }
    if out.len() % 2 != 0 {
        anyhow::bail!("WAV has an odd number of samples");
    }
    let frames = out.len() / 2;
    if frames == 0 {
        anyhow::bail!("WAV is empty");
    }
    Ok((out, frames))
}

impl Controller {
    pub(super) fn send_recorder(&self, address: &Path) {
        if let Ok(json) = serde_json::to_string(&self.recorder_status) {
            self.send_message(&ControlMessage::RecorderStatus { json }, address);
        }
    }

    pub(super) fn send_loops(&self, address: &Path) {
        if let Ok(json) = serde_json::to_string(&self.model.loops) {
            self.send_message(&ControlMessage::LoopList { json }, address);
        }
    }

    /// Queue a high-resolution envelope for the editor. The scan runs on the
    /// analysis worker; the result is sent when it comes back.
    pub(super) fn submit_waveform(&mut self, address: &Path, request: PendingWaveform) {
        let Some(runtime) = self
            .loops
            .get(request.slot as usize)
            .and_then(Option::as_ref)
        else {
            return;
        };
        let frames = runtime.slot.frames as usize;
        if frames == 0 || runtime.raw.len() < frames * 2 {
            return;
        }
        let (start, end) = if request.base {
            (0usize, frames)
        } else {
            let start = (request.start as usize).min(frames - 1);
            let end = (request.end as usize).min(frames);
            if end <= start {
                (0, frames)
            } else {
                (start, end)
            }
        };
        let nudge = if request.base {
            0
        } else {
            request
                .nudge
                .unwrap_or(runtime.slot.nudge_samples)
                .rem_euclid(frames as i64) as usize
        };
        let buckets = if request.buckets == 0 {
            LOOP_WAVEFORM_BUCKETS
        } else {
            (request.buckets as usize).clamp(512, 32_768)
        };
        let spec = WaveformSpec {
            slot: request.slot,
            start: start as u64,
            end: end as u64,
            nudge: request.nudge,
            buckets: buckets as u32,
            base: request.base,
            seq: request.seq,
        };
        self.waveform_seq = request.seq;
        self.last_waveform = Some((address.to_path_buf(), spec));
        self.workers
            .submit_analysis(AnalysisJob::Waveform(WaveformJob {
                address: address.to_path_buf(),
                slot: request.slot,
                generation: runtime.generation,
                raw: Arc::clone(&runtime.raw),
                frames,
                sample_rate: runtime.slot.sample_rate,
                start,
                end,
                nudge,
                buckets,
                base: request.base,
                seq: request.seq,
                transients: runtime.transients.clone(),
            }));
    }

    /// Re-run the last editor view after transient analysis changed. Reusing
    /// the sequence number lets the open editor accept the refreshed data.
    pub(super) fn refresh_waveform(&mut self, slot: u32) {
        let Some((address, spec)) = self.last_waveform.clone() else {
            return;
        };
        if spec.slot != slot {
            return;
        }
        self.submit_waveform(&address, pending_from_spec(&spec));
    }

    /// Poll the background workers and apply their results.
    pub(super) fn poll_workers(&mut self) {
        while let Some(output) = self.workers.try_recv() {
            match output {
                WorkerOutput::Take(result) => {
                    let Some(runtime) = self
                        .loops
                        .get_mut(result.slot as usize)
                        .and_then(Option::as_mut)
                    else {
                        continue;
                    };
                    if runtime.generation != result.generation
                        || runtime.analysis_seq != result.analysis_seq
                    {
                        continue;
                    }
                    runtime.transients = result.transients;
                    runtime.slot.peaks = result.peaks;
                    if self.model.recorder.transient {
                        runtime.slot.transient_nudge = result.transient;
                    }
                    let total = runtime.slot.base_nudge
                        + runtime.slot.transient_nudge
                        + runtime.slot.manual_nudge;
                    self.set_loop_total(result.slot, total);
                    self.loops_dirty = true;
                    self.refresh_waveform(result.slot);
                }
                WorkerOutput::Sensitivity(result) => {
                    let Some(runtime) = self
                        .loops
                        .get_mut(result.slot as usize)
                        .and_then(Option::as_mut)
                    else {
                        continue;
                    };
                    if runtime.generation != result.generation
                        || runtime.analysis_seq != result.analysis_seq
                        || runtime.slot.transient_sensitivity != result.sensitivity
                    {
                        continue;
                    }
                    runtime.transients = result.transients;
                    self.recorder_dirty = true;
                    self.refresh_waveform(result.slot);
                }
                WorkerOutput::Waveform {
                    address,
                    slot,
                    generation,
                    seq,
                    json,
                } => {
                    if seq != self.waveform_seq {
                        continue;
                    }
                    if self
                        .loops
                        .get(slot as usize)
                        .and_then(Option::as_ref)
                        .is_none_or(|runtime| runtime.generation != generation)
                    {
                        continue;
                    }
                    if let Some(json) = json {
                        self.send_message(&ControlMessage::LoopWaveform { json }, &address);
                    }
                }
                WorkerOutput::LoopSaved(result) => {
                    let Some(runtime) = self
                        .loops
                        .get_mut(result.slot as usize)
                        .and_then(Option::as_mut)
                    else {
                        continue;
                    };
                    if runtime.generation != result.generation {
                        continue;
                    }
                    match result.result {
                        Ok(()) => {
                            let saved_dir = result
                                .path
                                .parent()
                                .map(|parent| parent.to_string_lossy().into_owned());
                            runtime.slot.path = Some(result.path.to_string_lossy().into_owned());
                            // If the loop moved while the worker was writing,
                            // the file is from the old rotation.
                            runtime.slot.saved = runtime.slot.nudge_samples == result.nudge;
                            runtime.slot.normalize = result.normalize;
                            if let Some(dir) = saved_dir {
                                self.model.recorder.save_dir = dir;
                            }
                            self.model.recorder.bit_depth = result.bit_depth;
                            self.rebuild_model_loops();
                            self.loops_dirty = true;
                            self.recorder_status.message =
                                format!("saved {}", result.path.display());
                            self.recorder_dirty = true;
                        }
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                path = %result.path.display(),
                                "failed to save loop"
                            );
                            self.recorder_status.message = error;
                            self.recorder_dirty = true;
                        }
                    }
                }
                WorkerOutput::SessionSaved { path, result } => {
                    self.session_saving = false;
                    self.pending_session = Some(match result {
                        Ok(()) => (true, path, String::new()),
                        Err(error) => (false, path, error),
                    });
                }
                WorkerOutput::Autosave { text, result } => {
                    self.autosave_saving = false;
                    match result {
                        Ok(()) => {
                            if let Some(text) = text {
                                self.last_autosave_text = text;
                            }
                        }
                        Err(error) => tracing::warn!(%error, "autosave failed"),
                    }
                }
            }
        }
    }

    /// Poll the audio thread for a finished take and refresh the status.
    pub(super) fn poll_recorder(&mut self) {
        while let Some(take) = self.engine.take_recording() {
            self.finish_take(take);
        }

        match self.engine.recorder().take_discard() {
            crate::params::DISCARD_LOST_CLOCK => {
                tracing::warn!("recorder lost the MIDI clock; take discarded")
            }
            crate::params::DISCARD_OVERFLOW => {
                tracing::warn!("recorder buffer overflow; take discarded")
            }
            crate::params::DISCARD_TAKE_QUEUE => {
                tracing::warn!("finished takes arrived faster than they could be processed; one take was discarded");
                self.recorder_status.message =
                    "loop capture dropped: processing could not keep up".to_string();
                self.recorder_dirty = true;
            }
            _ => {}
        }

        let snapshot = self.engine.recorder().snapshot();
        let total = snapshot.total;
        let clock = self
            .engine
            .clock()
            .snapshot(self.engine.sample_rate() as f32);
        let message = if snapshot.state == crate::params::RecorderState::Armed && !clock.locked {
            "waiting for MIDI clock".to_string()
        } else {
            String::new()
        };
        let status = RecorderStatus {
            state: snapshot.state.as_str().to_string(),
            progress: if total > 0 {
                (snapshot.frames as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            },
            frames: snapshot.frames,
            peak: snapshot.peak,
            loop_beats: snapshot.loop_beats,
            loops_dir: loops_dir().to_string_lossy().into_owned(),
            message,
        };
        if status != self.recorder_status {
            self.recorder_status = status;
            self.recorder_dirty = true;
        }
    }

    /// Trim a captured take, apply the automatic/transient/manual offsets, and
    /// install it in the next free launcher slot.
    pub(super) fn finish_take(&mut self, take: crate::recorder::RecordedTake) {
        let crate::recorder::RecordedTake {
            buffer,
            frames,
            sample_rate,
            latency_frames,
            loop_beats,
        } = take;
        if frames == 0 {
            return;
        }
        let Some(slot_index) = self.loops.iter().position(Option::is_none) else {
            self.recorder_status.message = "loop slots are full".to_string();
            self.recorder_dirty = true;
            return;
        };
        let base = if self.model.recorder.auto_latency {
            latency_frames as i64
        } else {
            0
        };
        let manual = self.model.recorder.nudge_samples;
        let total = base + manual;
        let sensitivity = self.model.recorder.transient_sensitivity;
        let auto_transient = self.model.recorder.transient;
        let generation = self.next_loop_generation;
        self.next_loop_generation = self.next_loop_generation.wrapping_add(1);
        let raw: Arc<[f32]> = Arc::from(&buffer[..frames * 2]);
        let bpm = self
            .engine
            .clock()
            .snapshot(self.engine.sample_rate() as f32)
            .bpm;
        let slot = LoopSlot {
            id: slot_index as u32,
            name: format!("loop {}", slot_index + 1),
            path: None,
            frames: frames as u64,
            sample_rate,
            loop_beats,
            bpm,
            nudge_samples: total,
            base_nudge: base,
            transient_nudge: 0,
            manual_nudge: manual,
            transient_sensitivity: sensitivity,
            normalize: false,
            peaks: Vec::new(),
            gain: 1.0,
            mute: false,
            playing: false,
            saved: false,
        };
        if let Err(error) = self.engine.set_loop(slot_index, buffer, frames, total) {
            tracing::warn!(%error, "failed to install recorded loop");
            self.recorder_status.message = error.to_string();
            self.recorder_dirty = true;
            return;
        }
        self.loops[slot_index] = Some(LoopRuntime {
            slot,
            raw: Arc::clone(&raw),
            transients: Vec::new(),
            generation,
            analysis_seq: 1,
        });
        self.workers
            .submit_analysis(AnalysisJob::Take(TakeAnalysisJob {
                slot: slot_index as u32,
                generation,
                analysis_seq: 1,
                raw,
                frames,
                sample_rate,
                sensitivity,
                auto_transient,
                base,
            }));
        self.rebuild_model_loops();
        self.loops_dirty = true;
        self.recorder_status = RecorderStatus::default();
        self.recorder_dirty = true;
    }

    pub(super) fn arm_recorder(&mut self) {
        let sources = self.model.recorder.sources.clone();
        if sources.is_empty() {
            self.recorder_status.message = "select at least one source".to_string();
            self.recorder_dirty = true;
            return;
        }
        self.engine.recorder().set_sources(&sources);

        let length = self.model.transport_ui.loop_length.max(1) as f32;
        let loop_beats = if self.model.transport_ui.loop_bars {
            length * self.model.beats_per_bar
        } else {
            length
        };
        let clock = self
            .engine
            .clock()
            .snapshot(self.engine.sample_rate() as f32);
        let bpm = if clock.bpm > 1.0 { clock.bpm } else { 120.0 };
        let sample_rate = self.engine.sample_rate() as usize;
        let expected =
            ((loop_beats as f64 * 60.0 / bpm as f64) * sample_rate as f64).ceil() as usize;
        // Allocate for a slow (30 BPM) take so a tempo lower than the current
        // estimate cannot overflow the buffer; the audio thread discards the
        // take if it still runs out.
        let capacity_frames =
            ((loop_beats as f64 * 60.0 / 30.0) * sample_rate as f64).ceil() as usize;
        let cap = sample_rate * 300;
        if expected == 0 {
            self.recorder_status.message = "loop length is zero".to_string();
            self.recorder_dirty = true;
            return;
        }
        if capacity_frames > cap {
            self.recorder_status.message = "loop is longer than five minutes".to_string();
            self.recorder_dirty = true;
            return;
        }
        let capacity = (capacity_frames + sample_rate).min(cap);
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(capacity * 2).is_err() {
            self.recorder_status.message = "not enough memory for the loop buffer".to_string();
            self.recorder_dirty = true;
            return;
        }
        buffer.resize(capacity * 2, 0.0f32);
        let buffer = buffer.into_boxed_slice();
        let loop_pulses = (loop_beats * 24.0).round().max(1.0) as u32;

        match self
            .engine
            .arm_recorder(buffer, capacity, expected, loop_pulses, loop_beats)
        {
            Ok(()) => {
                self.recorder_status = RecorderStatus {
                    state: "armed".to_string(),
                    message: if clock.locked {
                        String::new()
                    } else {
                        "waiting for MIDI clock".to_string()
                    },
                    ..RecorderStatus::default()
                };
                self.recorder_dirty = true;
            }
            Err(error) => {
                self.recorder_status.message = error.to_string();
                self.recorder_dirty = true;
            }
        }
    }

    pub(super) fn handle_recorder_action(&mut self, action: RecorderAction) -> bool {
        let snapshot = matches!(
            action,
            RecorderAction::LoopSave { .. } | RecorderAction::SetSensitivity { .. }
        );
        match action {
            RecorderAction::Arm => self.arm_recorder(),
            RecorderAction::Cancel | RecorderAction::Stop => {
                let _ = self.engine.cancel_recorder();
                self.recorder_status = RecorderStatus::default();
                self.recorder_dirty = true;
            }
            RecorderAction::SetNudge { slot, manual } => self.set_loop_nudge(slot, manual),
            RecorderAction::SetLoopGain { slot, gain } => {
                let gain = gain.clamp(0.0, 4.0);
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.gain = gain;
                    let _ = self.engine.loop_gain(slot as usize, gain);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::SetLoopMute { slot, mute } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.mute = mute;
                    let _ = self.engine.loop_mute(slot as usize, mute);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::LoopPlay { slot, quantized } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.playing = true;
                    let _ = self.engine.loop_play(slot as usize, quantized);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::LoopStop { slot } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.playing = false;
                    let _ = self.engine.loop_stop(slot as usize);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::LoopDelete { slot } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::take) {
                    let _ = runtime;
                    let _ = self.engine.clear_loop(slot as usize);
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::LoopRename { slot, name } => {
                if let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) {
                    runtime.slot.name = name;
                    self.rebuild_model_loops();
                    self.loops_dirty = true;
                }
            }
            RecorderAction::Waveform {
                slot,
                start,
                end,
                nudge,
                seq,
                buckets,
                base,
            } => {
                self.pending_waveform = Some(PendingWaveform {
                    slot,
                    start,
                    end,
                    nudge,
                    seq,
                    buckets,
                    base,
                });
            }
            RecorderAction::SnapTransient { slot, frame } => {
                self.snap_transient(slot, frame);
            }
            RecorderAction::SnapSeam { slot } => self.snap_loop_seam(slot),
            RecorderAction::SetSensitivity { slot, sensitivity } => {
                self.set_loop_sensitivity(slot, sensitivity)
            }
            RecorderAction::LoopSave {
                slot,
                path,
                bit_depth,
                normalize,
            } => self.save_loop(slot, path, bit_depth, normalize),
        }
        snapshot
    }

    /// Change a loop's transient sensitivity and re-run the detector.
    pub(super) fn set_loop_sensitivity(&mut self, slot: u32, sensitivity: f32) {
        let sensitivity = if sensitivity.is_finite() {
            sensitivity.clamp(0.0, 1.0)
        } else {
            vibemix_core::recorder::default_sensitivity()
        };
        let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) else {
            return;
        };
        let frames = runtime.slot.frames as usize;
        let sample_rate = runtime.slot.sample_rate;
        runtime.slot.transient_sensitivity = sensitivity;
        runtime.analysis_seq = runtime.analysis_seq.wrapping_add(1);
        let analysis_seq = runtime.analysis_seq;
        let job = SensitivityJob {
            slot,
            generation: runtime.generation,
            analysis_seq,
            raw: Arc::clone(&runtime.raw),
            frames,
            sample_rate,
            sensitivity,
        };
        // New takes start from the sensitivity the user last settled on.
        self.model.recorder.transient_sensitivity = sensitivity;
        self.rebuild_model_loops();
        self.loops_dirty = true;
        self.recorder_dirty = true;
        self.workers.submit_analysis(AnalysisJob::Sensitivity(job));
    }

    pub(super) fn set_loop_nudge(&mut self, slot: u32, manual: i64) {
        let Some(total) = self
            .loops
            .get(slot as usize)
            .and_then(Option::as_ref)
            .map(|runtime| runtime.slot.base_nudge + runtime.slot.transient_nudge + manual)
        else {
            return;
        };
        self.set_loop_total(slot, total);
    }

    /// Set the total rotation without interrupting playback.
    pub(super) fn set_loop_total(&mut self, slot: u32, total: i64) {
        let Some(runtime) = self.loops.get_mut(slot as usize).and_then(Option::as_mut) else {
            return;
        };
        runtime.slot.nudge_samples = total;
        runtime.slot.manual_nudge = total - runtime.slot.base_nudge - runtime.slot.transient_nudge;
        runtime.slot.saved = false;
        // Update the playback rotation in place; the loop keeps playing.
        let _ = self.engine.loop_offset(slot as usize, total);
        self.rebuild_model_loops();
        self.loops_dirty = true;
    }

    pub(super) fn snap_transient(&mut self, slot: u32, frame: u64) {
        let Some(runtime) = self.loops.get(slot as usize).and_then(Option::as_ref) else {
            return;
        };
        let frames = runtime.slot.frames as i64;
        if frames <= 0 {
            return;
        }
        let total = (frame as i64).rem_euclid(frames);
        self.set_loop_total(slot, total);
        self.recorder_status.message = format!("snapped transient at {total} samples");
        self.recorder_dirty = true;
    }

    pub(super) fn snap_loop_seam(&mut self, slot: u32) {
        let Some(runtime) = self.loops.get(slot as usize).and_then(Option::as_ref) else {
            return;
        };
        let frames = runtime.slot.frames as usize;
        if frames == 0 {
            return;
        }
        let current = runtime.slot.nudge_samples;
        let sample_rate = runtime.slot.sample_rate;
        let total = snap_seam(&runtime.raw, frames, current, sample_rate, 2.0);
        let delta = total - current;
        self.set_loop_total(slot, total);
        self.recorder_status.message = if delta == 0 {
            "seam already aligned".to_string()
        } else {
            format!("seam snapped by {delta} samples")
        };
        self.recorder_dirty = true;
    }

    pub(super) fn save_loop(
        &mut self,
        slot: u32,
        path: Option<String>,
        bit_depth: u8,
        normalize: bool,
    ) {
        let save_dir = self.model.recorder.save_dir.clone();
        let Some(runtime) = self.loops.get(slot as usize).and_then(Option::as_ref) else {
            return;
        };
        let frames = runtime.slot.frames as usize;
        let sample_rate = runtime.slot.sample_rate;
        let name = sanitize_file_name(&runtime.slot.name);
        let path = path
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let dir = if save_dir.trim().is_empty() {
                    loops_dir()
                } else {
                    PathBuf::from(&save_dir)
                };
                let stamp = timestamp_slug();
                let mut candidate = dir.join(format!("{name}-{stamp}.wav"));
                let mut suffix = 2;
                while candidate.exists() {
                    candidate = dir.join(format!("{name}-{stamp}-{suffix}.wav"));
                    suffix += 1;
                }
                candidate
            });
        self.recorder_status.message = format!("saving {}…", path.display());
        self.recorder_dirty = true;
        let job = IoJob::SaveLoop(SaveLoopJob {
            slot,
            generation: runtime.generation,
            raw: Arc::clone(&runtime.raw),
            frames,
            sample_rate,
            path,
            bit_depth,
            nudge: runtime.slot.nudge_samples,
            normalize,
        });
        if !self.workers.submit_io(job) {
            self.recorder_status.message = "file-I/O worker is unavailable".to_string();
            self.recorder_dirty = true;
        }
    }

    pub(super) fn rebuild_model_loops(&mut self) {
        self.model.loops = self
            .loops
            .iter()
            .flatten()
            .map(|runtime| runtime.slot.clone())
            .collect();
    }

    pub(super) fn clear_loops(&mut self) {
        for index in 0..MAX_LOOPS {
            let _ = self.engine.clear_loop(index);
            self.loops[index] = None;
        }
        self.model.loops.clear();
        self.loops_dirty = true;
    }

    pub(super) fn load_loops_from_model(&mut self) {
        let saved = std::mem::take(&mut self.model.loops);
        for index in 0..MAX_LOOPS {
            let _ = self.engine.clear_loop(index);
            self.loops[index] = None;
        }
        let sample_rate = self.engine.sample_rate();
        for mut slot in saved {
            let Some(path) = slot.path.clone() else {
                continue;
            };
            match read_loop_wav(Path::new(&path), sample_rate) {
                Ok((raw, frames)) => {
                    let Some(index) = self.loops.iter().position(Option::is_none) else {
                        break;
                    };
                    slot.id = index as u32;
                    slot.frames = frames as u64;
                    slot.sample_rate = sample_rate;
                    slot.nudge_samples = 0;
                    slot.base_nudge = 0;
                    slot.transient_nudge = 0;
                    slot.manual_nudge = 0;
                    slot.playing = false;
                    let raw: Arc<[f32]> = Arc::from(raw);
                    slot.peaks = peaks(&raw, frames, LOOP_PEAK_BUCKETS);
                    if self
                        .engine
                        .set_loop(
                            index,
                            raw.to_vec().into_boxed_slice(),
                            frames,
                            slot.nudge_samples,
                        )
                        .is_ok()
                    {
                        let transients = detect_transients_with_sensitivity(
                            &raw,
                            frames,
                            sample_rate,
                            slot.transient_sensitivity,
                        );
                        let generation = self.next_loop_generation;
                        self.next_loop_generation = self.next_loop_generation.wrapping_add(1);
                        self.loops[index] = Some(LoopRuntime {
                            slot,
                            raw,
                            transients,
                            generation,
                            analysis_seq: 1,
                        });
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, path, "failed to load loop");
                }
            }
        }
        self.rebuild_model_loops();
        self.loops_dirty = true;
    }
}

#[cfg(test)]
mod wav_tests {
    use super::*;

    #[test]
    fn wav_roundtrip_for_each_depth() {
        let dir = std::env::temp_dir().join(format!("vibemix-wav-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let raw = vec![0.0, 0.5, -0.25, 0.125, 0.75, -0.75, 0.0, 0.0];
        for depth in [16u8, 24, 32] {
            let path = dir.join(format!("loop-{depth}.wav"));
            write_loop_wav(&path, &raw, 4, 48_000, depth, 0, false).unwrap();
            let (read, frames) = read_loop_wav(&path, 48_000).unwrap();
            assert_eq!(frames, 4);
            assert_eq!(read.len(), raw.len());
            let tolerance = if depth == 16 { 1.0e-3 } else { 1.0e-4 };
            for (a, b) in read.iter().zip(raw.iter()) {
                assert!((a - b).abs() <= tolerance, "{depth}: {a} vs {b}");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn over_long_wav_is_rejected_before_decoding() {
        let dir = std::env::temp_dir().join(format!("vibemix-wav-long-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("too-long.wav");
        let frames = 48_000u64 * 300 + 1;
        let data_len = frames * 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&192_000u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        // Make the file length match the declared sparse data chunk.
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(44 + data_len).unwrap();
        drop(file);

        let error = read_loop_wav(&path, 48_000).unwrap_err().to_string();
        assert!(error.contains("longer than"), "{error}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn normalize_reaches_minus_one_dbfs() {
        let dir = std::env::temp_dir().join(format!("vibemix-wav-norm-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let raw = vec![0.0, 0.1, 0.2, -0.05, 0.0, 0.0, 0.0, 0.0];
        let path = dir.join("norm.wav");
        write_loop_wav(&path, &raw, 4, 48_000, 32, 0, true).unwrap();
        let (read, _) = read_loop_wav(&path, 48_000).unwrap();
        let peak = read
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!((peak - 0.891_250_9).abs() < 1.0e-4, "peak={peak}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
