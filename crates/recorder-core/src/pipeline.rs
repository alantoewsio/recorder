use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::buffer::AudioBuffer;
use crate::error::{RecordingError, Result};
use crate::format::AudioFormat;
use crate::processing::run_processor_chain;
use crate::ring::FrameSender;
use crate::traits::AudioProcessor;

pub use crate::metrics::PipelineMetrics;

pub struct PipelineConfig {
    pub format: AudioFormat,
    /// Max number of pending PCM frames on the raw path.
    pub raw_queue_capacity: usize,
    /// Max number of pending PCM frames on the processed path.
    pub processed_queue_capacity: usize,
    /// Max number of pending PCM frames per analyzer tap.
    pub analyzer_queue_capacity: usize,
    /// Per-plugin wall-clock budget for a single `process` call (`None` = no limit).
    pub plugin_budget_per_plugin: Option<Duration>,
}

/// Per-source ingest: **always** pushes to the raw queue first (clone), then runs the plugin chain.
pub struct StreamPipeline {
    inner: Arc<PipelineInner>,
}

struct PipelineInner {
    config: PipelineConfig,
    raw_tx: Mutex<Option<FrameSender>>,
    proc_tx: Mutex<Option<FrameSender>>,
    analyzer_txs: Mutex<Vec<FrameSender>>,
    chain: Mutex<Vec<Box<dyn AudioProcessor + Send>>>,
    metrics: Arc<PipelineMetrics>,
}

impl StreamPipeline {
    pub fn new(
        config: PipelineConfig,
        raw_tx: Option<FrameSender>,
        proc_tx: Option<FrameSender>,
        analyzer_txs: Vec<FrameSender>,
        chain: Vec<Box<dyn AudioProcessor + Send>>,
        metrics: Arc<PipelineMetrics>,
    ) -> Self {
        Self {
            inner: Arc::new(PipelineInner {
                config,
                raw_tx: Mutex::new(raw_tx),
                proc_tx: Mutex::new(proc_tx),
                analyzer_txs: Mutex::new(analyzer_txs),
                chain: Mutex::new(chain),
                metrics,
            }),
        }
    }

    pub fn metrics(&self) -> Arc<PipelineMetrics> {
        self.inner.metrics.clone()
    }

    pub fn format(&self) -> AudioFormat {
        self.inner.config.format
    }

    /// Close queues so writer threads can exit after draining.
    pub fn close(&self) {
        *self.inner.raw_tx.lock().unwrap() = None;
        *self.inner.proc_tx.lock().unwrap() = None;
        self.inner.analyzer_txs.lock().unwrap().clear();
    }

    /// Invoked on the host audio thread: raw tap happens **before** any plugin.
    pub fn ingest(&self, buffer: AudioBuffer) {
        let frames = buffer.frames as u64;

        if let Some(tx) = self.inner.raw_tx.lock().unwrap().as_ref() {
            if tx.try_send(buffer.clone()).is_err() {
                self.inner
                    .metrics
                    .raw_frames_dropped
                    .fetch_add(frames, Ordering::Relaxed);
            }
        }

        let processed = {
            let mut chain = self.inner.chain.lock().unwrap();
            run_processor_chain(
                buffer,
                &mut chain,
                self.inner.config.plugin_budget_per_plugin,
                &self.inner.metrics,
            )
        };

        for tx in self.inner.analyzer_txs.lock().unwrap().iter() {
            if tx.try_send(processed.clone()).is_err() {
                self.inner
                    .metrics
                    .analyzer_frames_dropped
                    .fetch_add(frames, Ordering::Relaxed);
            }
        }

        if let Some(tx) = self.inner.proc_tx.lock().unwrap().as_ref() {
            if tx.try_send(processed).is_err() {
                self.inner
                    .metrics
                    .processed_frames_dropped
                    .fetch_add(frames, Ordering::Relaxed);
            }
        }
    }
}

/// Utility plugin that scales samples (useful in tests).
pub struct GainProcessor {
    pub gain: f32,
    name: String,
}

impl GainProcessor {
    pub fn new(gain: f32) -> Self {
        Self {
            gain,
            name: format!("gain({gain})"),
        }
    }
}

impl AudioProcessor for GainProcessor {
    fn name(&self) -> &str {
        &self.name
    }

    fn process(&mut self, input: &AudioBuffer, output: &mut AudioBuffer) -> Result<()> {
        if input.frames != output.frames || input.format != output.format {
            return Err(RecordingError::FormatMismatch {
                expected: input.format,
                got: output.format,
            });
        }
        output.captured_at = input.captured_at;
        output.frame_index = input.frame_index;
        let v: Vec<f32> = input
            .data
            .iter()
            .map(|s| (s * self.gain).clamp(-1.0, 1.0))
            .collect();
        output.data = v.into();
        Ok(())
    }
}

/// Plugin that busy-loops to simulate bad behaviour (tests only).
pub struct SpinProcessor {
    pub spin_for: Duration,
}

impl AudioProcessor for SpinProcessor {
    fn name(&self) -> &str {
        "spin"
    }

    fn process(&mut self, input: &AudioBuffer, output: &mut AudioBuffer) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() < self.spin_for {
            std::hint::spin_loop();
        }
        crate::processing::passthrough_copy(input, output);
        Ok(())
    }
}
