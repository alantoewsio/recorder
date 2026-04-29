use thiserror::Error;

/// Errors surfaced by the recording session and sinks.
#[derive(Debug, Error)]
pub enum RecordingError {
    #[error("device error: {0}")]
    Device(String),
    #[error("ring buffer full (raw path); frames dropped: {0}")]
    RawRingFull(u64),
    #[error("ring buffer full (processed path); frames dropped: {0}")]
    ProcessedRingFull(u64),
    #[error("encode / I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "wav")]
    #[error("wav: {0}")]
    Wav(#[from] hound::Error),
    #[error("format mismatch: expected {expected:?}, got {got:?}")]
    FormatMismatch {
        expected: crate::format::AudioFormat,
        got: crate::format::AudioFormat,
    },
    #[error("plugin processing failed: {0}")]
    Plugin(String),
    #[error("plugin time budget exceeded (frame bypassed)")]
    PluginTimeout,
    #[error("session already stopped")]
    AlreadyStopped,
    #[error("invalid configuration: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, RecordingError>;
