//! Direct-to-disk session capture.
//!
//! The audio thread pushes interleaved source samples into a lock-free ring;
//! this module drains that ring on a dedicated thread and streams one WAV per
//! source. Disk work never runs on the audio thread, and a full ring simply
//! drops a block (counted by [`SharedCapture`]) instead of stalling audio.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::params::SharedCapture;

/// Everything the writer thread needs to stream to disk.
pub(super) struct CaptureJob {
    pub consumer: rtrb::Consumer<f32>,
    pub channels: usize,
    pub names: Vec<String>,
    pub sample_rate: u32,
    pub bit_depth: u8,
    pub dir: PathBuf,
    pub base: String,
    pub shared: Arc<SharedCapture>,
}

/// A spawned writer thread and the shared status it updates.
pub(super) struct CaptureHandle {
    pub join: thread::JoinHandle<Result<Vec<PathBuf>, String>>,
}

/// Spawn the writer thread for a freshly created ring.
pub(super) fn spawn(job: CaptureJob) -> CaptureHandle {
    let join = thread::Builder::new()
        .name("vibemix-capture".to_string())
        .spawn(move || run(job))
        .expect("spawn capture writer");
    CaptureHandle { join }
}

fn run(mut job: CaptureJob) -> Result<Vec<PathBuf>, String> {
    if job.channels == 0 || job.channels % 2 != 0 || job.names.len() * 2 != job.channels {
        return Err("invalid capture source layout".to_string());
    }
    let depth = match job.bit_depth {
        16 | 24 | 32 => job.bit_depth,
        _ => 24,
    };
    fs::create_dir_all(&job.dir).map_err(|error| error.to_string())?;
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: job.sample_rate,
        bits_per_sample: depth as u16,
        sample_format: if depth == 32 {
            hound::SampleFormat::Float
        } else {
            hound::SampleFormat::Int
        },
    };

    let mut paths = Vec::with_capacity(job.names.len());
    let mut writers = Vec::with_capacity(job.names.len());
    for (index, name) in job.names.iter().enumerate() {
        let path = job.dir.join(format!(
            "{}-{:02}-{}.wav",
            job.base,
            index + 1,
            sanitize(name)
        ));
        let writer = hound::WavWriter::create(&path, spec).map_err(|error| error.to_string())?;
        writers.push(writer);
        paths.push(path);
    }

    let mut frame = vec![0.0f32; job.channels];
    let mut error: Option<String> = None;
    loop {
        while job.consumer.slots() >= job.channels {
            for sample in frame.iter_mut() {
                match job.consumer.pop() {
                    Ok(value) => *sample = value,
                    Err(_) => break,
                }
            }
            for (source, writer) in writers.iter_mut().enumerate() {
                let left = frame[source * 2];
                let right = frame[source * 2 + 1];
                if let Err(failure) = write_sample(writer, depth, left)
                    .and_then(|()| write_sample(writer, depth, right))
                {
                    error = Some(failure.to_string());
                    break;
                }
            }
            if error.is_some() {
                break;
            }
        }
        if error.is_some() {
            break;
        }
        // Once the audio thread has stopped and the ring is drained, finalize.
        if job.shared.stopped() && job.consumer.slots() < job.channels {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }

    for writer in writers {
        if error.is_some() {
            break;
        }
        if let Err(failure) = writer.finalize() {
            error = Some(failure.to_string());
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(paths)
}

fn write_sample<W: Write + std::io::Seek>(
    writer: &mut hound::WavWriter<W>,
    depth: u8,
    sample: f32,
) -> Result<(), hound::Error> {
    match depth {
        16 => writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16),
        24 => writer.write_sample((sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32),
        _ => writer.write_sample(sample),
    }
}

fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("source");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_streams_one_wav_per_source() {
        let dir = std::env::temp_dir().join(format!("vibemix-capture-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(1024);
        let shared = Arc::new(SharedCapture::new());
        let job = CaptureJob {
            consumer,
            channels: 4,
            names: vec!["kick".to_string(), "snare".to_string()],
            sample_rate: 48_000,
            bit_depth: 32,
            dir: dir.clone(),
            base: "test".to_string(),
            shared: Arc::clone(&shared),
        };
        // Two frames: (kick L/R, snare L/R).
        for value in [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8] {
            producer.push(value).unwrap();
        }
        let handle = spawn(job);
        // Give the writer a moment to drain, then signal stop.
        thread::sleep(Duration::from_millis(20));
        shared.set_stopped();
        let paths = handle.join.join().expect("writer").expect("capture");
        assert_eq!(paths.len(), 2);
        for path in &paths {
            assert!(path.exists(), "{}", path.display());
        }
        let reader = hound::WavReader::open(&paths[0]).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 48_000);
        assert_eq!(reader.duration(), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
