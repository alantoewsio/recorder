use std::time::Duration;

/// PCM sample layout in host memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// Interleaved `f32` in range typically `[-1.0, 1.0]`.
    F32,
    /// Interleaved signed 16-bit little-endian.
    I16,
}

/// Describes a PCM stream layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl AudioFormat {
    pub const fn new(sample_rate_hz: u32, channels: u16, sample_format: SampleFormat) -> Self {
        Self {
            sample_rate_hz,
            channels,
            sample_format,
        }
    }

    pub fn samples_per_frame(&self, frames: usize) -> usize {
        frames * self.channels as usize
    }

    pub fn duration_for_frames(&self, frames: usize) -> Duration {
        let hz = self.sample_rate_hz as u128;
        if hz == 0 {
            return Duration::ZERO;
        }
        let ns = (frames as u128 * 1_000_000_000u128) / hz;
        Duration::from_nanos(ns.min(u64::MAX as u128) as u64)
    }
}
