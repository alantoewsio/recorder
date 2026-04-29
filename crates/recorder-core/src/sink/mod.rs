mod null;
pub use null::NullSink;

#[cfg(feature = "wav")]
mod wav;
#[cfg(feature = "wav")]
pub use wav::WavSink;

#[cfg(feature = "flac")]
mod flac;
#[cfg(feature = "flac")]
pub use flac::FlacSink;

#[cfg(feature = "mp3")]
mod mp3;
#[cfg(feature = "mp3")]
pub use mp3::Mp3Sink;
