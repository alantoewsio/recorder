//! Multi-source audio recording: portable core with per-OS host crates.
//!
//! ## Raw vs processed isolation
//! [`StreamPipeline::ingest`](crate::pipeline::StreamPipeline::ingest) pushes a clone to the
//! raw queue **before** running any [`AudioProcessor`](crate::traits::AudioProcessor). Writer
//! threads never invoke plugins.

pub mod analyzer;
pub mod buffer;
pub mod composite;
pub mod control;
pub mod error;
pub mod events;
pub mod format;
pub mod metrics;
pub mod pipeline;
pub mod processing;
pub mod ring;
pub mod session;
pub mod synthetic;
pub mod traits;

#[cfg(any(feature = "wav", feature = "flac", feature = "mp3"))]
pub mod sink;

#[cfg(feature = "mixer")]
pub mod graph;
#[cfg(feature = "mixer")]
pub mod mixer;

pub use analyzer::{AudioAnalyzer, VoiceActivityAnalyzer};
pub use buffer::AudioBuffer;
pub use composite::{CompositeSink, TeeAudioSink};
pub use control::{ControllablePlugin, ParameterInfo, ParameterValue, PluginCommand, PluginId};
pub use error::{RecordingError, Result};
pub use events::{media_event_queue, AudioTap, MediaEvent, MediaEventReceiver, MediaEventSender};
pub use format::{AudioFormat, SampleFormat};
pub use metrics::PipelineMetrics;
pub use pipeline::{GainProcessor, PipelineConfig, SpinProcessor, StreamPipeline};
pub use session::{CaptureStream, RecordingSession, SessionConfig, StreamOptions};
pub use synthetic::SyntheticSource;
pub use traits::{
    AudioHost, AudioProcessor, AudioSink, CaptureSource, CaptureSourceKind, DeviceInfo,
    StreamHandle,
};

#[cfg(feature = "flac")]
pub use sink::FlacSink;
#[cfg(feature = "mp3")]
pub use sink::Mp3Sink;
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3"))]
pub use sink::NullSink;
#[cfg(feature = "wav")]
pub use sink::WavSink;

#[cfg(feature = "mixer")]
pub use graph::{spawn_single_bus_mixer, BusId, InputStripId, MixerGraph};
#[cfg(feature = "mixer")]
pub use mixer::{
    bus_mixer_legs, mixer_channels, BusLegConfig, BusMixer, BusMixerConfig, MixMode, MixerConfig,
    MixerInputSink, StreamMixer,
};
