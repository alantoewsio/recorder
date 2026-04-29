use std::sync::Arc;
use std::time::Instant;

use crate::format::{AudioFormat, SampleFormat};

/// One contiguous block of interleaved PCM owned by the producer.
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    pub format: AudioFormat,
    /// Interleaved samples: `frames * channels` elements.
    pub data: Arc<[f32]>,
    pub frames: usize,
    /// Host-provided or synthetic capture instant (wall clock).
    pub captured_at: Instant,
    /// Monotonic frame index for this logical stream (optional ordering aid).
    pub frame_index: u64,
}

impl AudioBuffer {
    pub fn new(
        format: AudioFormat,
        data: Arc<[f32]>,
        frames: usize,
        captured_at: Instant,
        frame_index: u64,
    ) -> Self {
        debug_assert_eq!(data.len(), format.samples_per_frame(frames));
        Self {
            format,
            data,
            frames,
            captured_at,
            frame_index,
        }
    }

    /// Convert interleaved `i16` PCM to `f32` and build a buffer.
    pub fn from_interleaved_i16(
        format: AudioFormat,
        interleaved: &[i16],
        frames: usize,
        captured_at: Instant,
        frame_index: u64,
    ) -> Self {
        let expected = format.samples_per_frame(frames);
        let mut out = Vec::with_capacity(expected);
        for &s in interleaved.iter().take(expected) {
            out.push(s as f32 / 32768.0);
        }
        out.resize(expected, 0.0);
        Self::new(
            AudioFormat {
                sample_format: SampleFormat::F32,
                ..format
            },
            out.into(),
            frames,
            captured_at,
            frame_index,
        )
    }

    pub fn silent(
        format: AudioFormat,
        frames: usize,
        captured_at: Instant,
        frame_index: u64,
    ) -> Self {
        let n = format.samples_per_frame(frames);
        Self::new(
            format,
            vec![0.0f32; n].into(),
            frames,
            captured_at,
            frame_index,
        )
    }

    pub fn assert_format(&self, expected: AudioFormat) -> crate::error::Result<()> {
        if self.format != expected {
            return Err(crate::error::RecordingError::FormatMismatch {
                expected,
                got: self.format,
            });
        }
        Ok(())
    }
}
