use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::buffer::AudioBuffer;
use crate::error::Result;
use crate::events::MediaEventSender;
use crate::format::AudioFormat;
use crate::pipeline::{PipelineConfig, PipelineMetrics, StreamPipeline};
use crate::ring::frame_queue;
use crate::traits::{
    AudioAnalyzer, AudioHost, AudioProcessor, AudioSink, CaptureSourceKind, StreamHandle,
};

/// Global knobs for a recording session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub raw_queue_capacity: usize,
    pub processed_queue_capacity: usize,
    pub analyzer_queue_capacity: usize,
    pub plugin_budget_per_plugin: Option<Duration>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            raw_queue_capacity: 256,
            processed_queue_capacity: 256,
            analyzer_queue_capacity: 64,
            plugin_budget_per_plugin: Some(Duration::from_millis(5)),
        }
    }
}

/// Per-stream wiring for sinks and processors.
pub struct StreamOptions {
    pub raw_sink: Option<Box<dyn AudioSink>>,
    pub processed_sink: Option<Box<dyn AudioSink>>,
    pub processors: Vec<Box<dyn AudioProcessor + Send>>,
    pub analyzers: Vec<Box<dyn AudioAnalyzer + Send>>,
    pub event_tx: Option<MediaEventSender>,
    /// When set and `load(Ordering::Acquire) == true`, captured buffers are dropped before ingest
    /// (no disk growth; timeline compresses vs wall clock).
    pub pause_gate: Option<Arc<AtomicBool>>,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            raw_sink: None,
            processed_sink: None,
            processors: Vec::new(),
            analyzers: Vec::new(),
            event_tx: None,
            pause_gate: None,
        }
    }
}

/// Owns host stream stop and writer thread joins.
pub struct CaptureStream {
    host: StreamHandle,
    joins: Vec<JoinHandle<()>>,
    pipeline: Arc<StreamPipeline>,
}

impl CaptureStream {
    /// Stops device capture and closes frame queues; joins writer threads.
    pub fn stop(mut self) {
        self.host.stop();
        self.pipeline.close();
        for j in self.joins.drain(..) {
            let _ = j.join();
        }
    }

    pub fn metrics(&self) -> Arc<PipelineMetrics> {
        self.pipeline.metrics()
    }
}

/// Starts capture streams; does not select a host implementation (see OS crates).
pub struct RecordingSession {
    pub config: SessionConfig,
}

impl RecordingSession {
    pub fn new(config: SessionConfig) -> Self {
        Self { config }
    }

    /// Opens one input device stream and optional raw/processed sinks.
    ///
    /// Equivalent to [`RecordingSession::add_capture_stream`] with
    /// [`CaptureSourceKind::Input`].
    pub fn add_stream(
        &self,
        host: &dyn AudioHost,
        device_id: Option<&str>,
        format: AudioFormat,
        options: StreamOptions,
    ) -> Result<CaptureStream> {
        self.add_capture_stream(host, device_id, CaptureSourceKind::Input, format, options)
    }

    /// Opens a capture stream of any kind (microphone or speaker loopback) and wires the
    /// pipeline to optional raw/processed sinks, processors, and analyzers.
    pub fn add_capture_stream(
        &self,
        host: &dyn AudioHost,
        source_id: Option<&str>,
        kind: CaptureSourceKind,
        format: AudioFormat,
        options: StreamOptions,
    ) -> Result<CaptureStream> {
        let metrics = Arc::new(PipelineMetrics::default());

        let StreamOptions {
            raw_sink,
            processed_sink,
            processors,
            analyzers,
            event_tx,
            pause_gate,
        } = options;

        let has_processors = !processors.is_empty();
        let has_analyzers = !analyzers.is_empty();
        if raw_sink.is_none() && processed_sink.is_none() && !has_processors && !has_analyzers {
            return Err(crate::error::RecordingError::Config(
                "at least one sink, processor, or analyzer must be set".into(),
            ));
        }

        let (raw_tx, raw_rx) = if raw_sink.is_some() {
            let (t, r) = frame_queue(self.config.raw_queue_capacity);
            (Some(t), Some(r))
        } else {
            (None, None)
        };

        let (proc_tx, proc_rx) = if processed_sink.is_some() {
            let (t, r) = frame_queue(self.config.processed_queue_capacity);
            (Some(t), Some(r))
        } else {
            (None, None)
        };

        let mut analyzer_txs = Vec::with_capacity(analyzers.len());
        let mut analyzer_rxs = Vec::with_capacity(analyzers.len());
        for _ in 0..analyzers.len() {
            let (t, r) = frame_queue(self.config.analyzer_queue_capacity);
            analyzer_txs.push(t);
            analyzer_rxs.push(r);
        }

        let pipeline_config = PipelineConfig {
            format,
            raw_queue_capacity: self.config.raw_queue_capacity,
            processed_queue_capacity: self.config.processed_queue_capacity,
            analyzer_queue_capacity: self.config.analyzer_queue_capacity,
            plugin_budget_per_plugin: self.config.plugin_budget_per_plugin,
        };

        let pipeline = Arc::new(StreamPipeline::new(
            pipeline_config,
            raw_tx,
            proc_tx,
            analyzer_txs,
            processors,
            metrics.clone(),
        ));

        let mut joins = Vec::new();

        if let (Some(rx), Some(mut sink)) = (raw_rx, raw_sink) {
            joins.push(std::thread::spawn(move || {
                while let Ok(buf) = rx.inner.recv() {
                    if let Err(e) = sink.write_pcm_f32(&buf) {
                        tracing::error!("raw sink error: {e}");
                    }
                }
                let _ = sink.flush();
            }));
        }

        if let (Some(rx), Some(mut sink)) = (proc_rx, processed_sink) {
            joins.push(std::thread::spawn(move || {
                while let Ok(buf) = rx.inner.recv() {
                    if let Err(e) = sink.write_pcm_f32(&buf) {
                        tracing::error!("processed sink error: {e}");
                    }
                }
                let _ = sink.flush();
            }));
        }

        for (mut analyzer, rx) in analyzers.into_iter().zip(analyzer_rxs.into_iter()) {
            let event_tx = event_tx.clone();
            joins.push(std::thread::spawn(move || {
                while let Ok(buf) = rx.inner.recv() {
                    if let Err(e) = analyzer.accept_audio(&buf) {
                        tracing::error!("analyzer {} error: {e}", analyzer.name());
                    }
                    if let Some(tx) = event_tx.as_ref() {
                        for event in analyzer.drain_events() {
                            if tx.try_send(event).is_err() {
                                tracing::warn!("media event queue full; dropping analyzer event");
                            }
                        }
                    } else {
                        let _ = analyzer.drain_events();
                    }
                }
                if let Some(tx) = event_tx.as_ref() {
                    for event in analyzer.drain_events() {
                        let _ = tx.try_send(event);
                    }
                }
            }));
        }

        let pause_gate = pause_gate.clone();
        let pl = pipeline.clone();
        let callback = Arc::new(move |buf: AudioBuffer| {
            if let Some(g) = pause_gate.as_ref() {
                if g.load(Ordering::Acquire) {
                    return;
                }
            }
            pl.ingest(buf);
        });

        let host_handle = host.start_capture(source_id, kind, format, callback)?;

        Ok(CaptureStream {
            host: host_handle,
            joins,
            pipeline,
        })
    }
}
