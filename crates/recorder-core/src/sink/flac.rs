use std::fs::File;
use std::io::Write;
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::component::Stream;
use flacenc::config::Encoder;
use flacenc::encode_fixed_size_frame;
use flacenc::error::Verify;
use flacenc::source::Fill;
use flacenc::source::FrameBuf;

use crate::buffer::AudioBuffer;
use crate::error::{RecordingError, Result};
use crate::format::AudioFormat;
use crate::traits::AudioSink;

const BLOCK: usize = 1024;
const BITS: usize = 16;

/// Streaming FLAC writer using `flacenc` (16-bit PCM derived from `f32` input).
pub struct FlacSink {
    stream: Stream,
    encoder: flacenc::error::Verified<Encoder>,
    pending: Vec<i32>,
    channels: usize,
    frame_index: usize,
    out: File,
}

impl FlacSink {
    pub fn create(path: impl AsRef<Path>, format: AudioFormat) -> Result<Self> {
        if format.channels == 0 {
            return Err(RecordingError::Config("channels must be > 0".into()));
        }
        let channels = format.channels as usize;
        let sample_rate_hz = format.sample_rate_hz as usize;
        let stream = Stream::new(sample_rate_hz, channels, BITS)
            .map_err(|e| RecordingError::Plugin(format!("flac stream: {e:?}")))?;
        let encoder = Encoder::default()
            .into_verified()
            .map_err(|e| RecordingError::Plugin(format!("flac encoder config: {e:?}")))?;
        let out = File::create(path.as_ref())?;
        Ok(Self {
            stream,
            encoder,
            pending: Vec::new(),
            channels,
            frame_index: 0,
            out,
        })
    }

    fn push_interleaved_i32(&mut self, interleaved: &[i32]) -> Result<()> {
        self.pending.extend_from_slice(interleaved);
        let ch = self.channels;
        while self.pending.len() >= ch * BLOCK {
            let chunk: Vec<i32> = self.pending.drain(0..ch * BLOCK).collect();
            self.encode_block(&chunk, BLOCK)?;
        }
        Ok(())
    }

    fn encode_block(&mut self, interleaved: &[i32], block_size: usize) -> Result<()> {
        let mut fb = FrameBuf::with_size(self.channels, block_size)
            .map_err(|e| RecordingError::Plugin(format!("framebuf: {e:?}")))?;
        fb.fill_interleaved(interleaved)
            .map_err(|e| RecordingError::Plugin(format!("fill_interleaved: {e:?}")))?;
        let frame = encode_fixed_size_frame(
            &self.encoder,
            &fb,
            self.frame_index,
            self.stream.stream_info(),
        )
        .map_err(|e| RecordingError::Plugin(format!("encode frame: {e:?}")))?;
        self.stream.add_frame(frame);
        self.frame_index += 1;
        Ok(())
    }

    fn f32_to_i16(s: f32) -> i32 {
        let v = (s * 32767.0).clamp(-32768.0, 32767.0);
        v as i32
    }
}

impl AudioSink for FlacSink {
    fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> Result<()> {
        let interleaved: Vec<i32> = buffer.data.iter().map(|&s| Self::f32_to_i16(s)).collect();
        self.push_interleaved_i32(&interleaved)
    }

    fn flush(&mut self) -> Result<()> {
        let ch = self.channels;
        if !self.pending.is_empty() {
            let n = self.pending.len() / ch;
            if n > 0 {
                let tail: Vec<i32> = self.pending.drain(..).collect();
                self.encode_block(&tail, n)?;
            }
        }
        let mut sink = ByteSink::new();
        self.stream
            .write(&mut sink)
            .map_err(|e| RecordingError::Plugin(format!("write stream: {e:?}")))?;
        self.out
            .write_all(sink.as_slice())
            .map_err(RecordingError::from)?;
        self.out.flush()?;
        Ok(())
    }
}
