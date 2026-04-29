use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use hound::{SampleFormat as HoundSampleFormat, WavSpec, WavWriter};

use crate::buffer::AudioBuffer;
use crate::error::Result;
use crate::format::{AudioFormat, SampleFormat};
use crate::traits::AudioSink;

/// Writes interleaved `f32` or `i16` PCM to a RIFF WAV file on a writer thread.
pub struct WavSink {
    writer: Option<WavWriter<BufWriter<File>>>,
    format: AudioFormat,
}

impl WavSink {
    pub fn create(path: impl AsRef<Path>, format: AudioFormat) -> Result<Self> {
        let sample_format = match format.sample_format {
            SampleFormat::F32 => HoundSampleFormat::Float,
            SampleFormat::I16 => HoundSampleFormat::Int,
        };
        let bits = match format.sample_format {
            SampleFormat::F32 => 32,
            SampleFormat::I16 => 16,
        };
        let spec = WavSpec {
            channels: format.channels,
            sample_rate: format.sample_rate_hz,
            bits_per_sample: bits,
            sample_format,
        };
        let writer = WavWriter::create(path.as_ref(), spec)?;
        Ok(Self {
            writer: Some(writer),
            format,
        })
    }
}

impl AudioSink for WavSink {
    fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> Result<()> {
        buffer.assert_format(self.format)?;
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        match self.format.sample_format {
            SampleFormat::F32 => {
                for &s in buffer.data.iter() {
                    writer.write_sample(s)?;
                }
            }
            SampleFormat::I16 => {
                for &s in buffer.data.iter() {
                    let v = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    writer.write_sample(v)?;
                }
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.take() {
            // `flush()` only pushes buffered bytes; `finalize()` also rewrites the RIFF
            // header with the final data length. Without this, short/empty-looking WAVs
            // can be produced even though sample writes succeeded on the writer thread.
            writer.finalize()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::format::AudioFormat;

    #[test]
    fn flush_finalizes_header_and_samples_are_readable() {
        let path =
            std::env::temp_dir().join(format!("recorder-core-wav-sink-{}.wav", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let format = AudioFormat::new(48_000, 2, SampleFormat::F32);
        let mut sink = WavSink::create(&path, format).expect("create wav sink");
        let buf = AudioBuffer::new(
            format,
            vec![0.25, -0.25, 0.5, -0.5].into(),
            2,
            Instant::now(),
            0,
        );

        sink.write_pcm_f32(&buf).expect("write samples");
        sink.flush().expect("finalize wav");

        let mut reader = hound::WavReader::open(&path).expect("open finalized wav");
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 48_000);
        assert_eq!(reader.duration(), 2);

        let samples: Vec<f32> = reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("read f32 samples");
        assert_eq!(samples.len(), 4);
        assert!((samples[0] - 0.25).abs() < f32::EPSILON);
        assert!((samples[3] + 0.5).abs() < f32::EPSILON);

        let _ = std::fs::remove_file(&path);
    }
}
