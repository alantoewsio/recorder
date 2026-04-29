//! Deterministic in-memory source for tests and benchmarks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::buffer::AudioBuffer;
use crate::format::AudioFormat;
use crate::pipeline::StreamPipeline;
use crate::traits::StreamHandle;

/// Feeds synthetic sine PCM into a [`StreamPipeline`] from a background thread.
pub struct SyntheticSource {
    format: AudioFormat,
    frequency_hz: f32,
    frame_counter: AtomicU64,
}

impl SyntheticSource {
    pub fn new(format: AudioFormat, frequency_hz: f32) -> Self {
        Self {
            format,
            frequency_hz,
            frame_counter: AtomicU64::new(0),
        }
    }

    /// Runs until `stop` is set; `frames_per_chunk` per pushed buffer.
    pub fn run(
        self: Arc<Self>,
        pipeline: Arc<StreamPipeline>,
        frames_per_chunk: usize,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let channels = self.format.channels as usize;
            let mut phase: f32 = 0.0;
            let two_pi = std::f32::consts::TAU;
            let sr = self.format.sample_rate_hz as f32;
            let inc = self.frequency_hz * two_pi / sr;

            while !stop.load(Ordering::Relaxed) {
                let idx = self
                    .frame_counter
                    .fetch_add(frames_per_chunk as u64, Ordering::Relaxed);
                let mut data = vec![0.0f32; frames_per_chunk * channels];
                for f in 0..frames_per_chunk {
                    let s = phase.sin() * 0.2;
                    phase += inc;
                    for c in 0..channels {
                        data[f * channels + c] = s;
                    }
                }
                let buf = AudioBuffer::new(
                    self.format,
                    data.into(),
                    frames_per_chunk,
                    Instant::now(),
                    idx,
                );
                pipeline.ingest(buf);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    }
}

/// No-op host stop for synthetic runs.
pub fn synthetic_stop_handle() -> StreamHandle {
    StreamHandle::new(|| {})
}
