use crate::buffer::AudioBuffer;
use crate::error::Result;
use crate::traits::AudioSink;

/// Discards all PCM (for optional processed path when no file is needed).
#[derive(Debug, Default)]
pub struct NullSink;

impl NullSink {
    pub fn new() -> Self {
        Self
    }
}

impl AudioSink for NullSink {
    fn write_pcm_f32(&mut self, _buffer: &AudioBuffer) -> Result<()> {
        Ok(())
    }
}
