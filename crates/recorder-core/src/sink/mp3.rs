use std::fs::File;
use std::io::{BufWriter, Write};
use std::mem::MaybeUninit;
use std::path::Path;

use mp3lame_encoder::{
    max_required_buffer_size, Bitrate, Builder, Encoder, FlushNoGap, InterleavedPcm, MonoPcm,
    Quality,
};

use crate::buffer::AudioBuffer;
use crate::error::{RecordingError, Result};
use crate::format::AudioFormat;
use crate::traits::AudioSink;

/// MP3 file sink (requires system LAME via `mp3lame-encoder`). Supports 1 or 2 channels.
pub struct Mp3Sink {
    encoder: Encoder,
    out: BufWriter<File>,
    scratch: Vec<MaybeUninit<u8>>,
    channels: u8,
}

impl Mp3Sink {
    pub fn create(path: impl AsRef<Path>, format: AudioFormat) -> Result<Self> {
        let ch = format.channels;
        if ch == 0 || ch > 2 {
            return Err(RecordingError::Config(
                "Mp3Sink supports only mono or stereo".into(),
            ));
        }
        let mut b = Builder::new().ok_or_else(|| {
            RecordingError::Plugin("failed to allocate LAME builder (is LAME installed?)".into())
        })?;
        b.set_num_channels(ch as u8)
            .map_err(|e| RecordingError::Plugin(format!("lame channels: {e:?}")))?;
        b.set_sample_rate(format.sample_rate_hz)
            .map_err(|e| RecordingError::Plugin(format!("lame sample rate: {e:?}")))?;
        b.set_brate(Bitrate::Kbps192)
            .map_err(|e| RecordingError::Plugin(format!("lame bitrate: {e:?}")))?;
        b.set_quality(Quality::Best)
            .map_err(|e| RecordingError::Plugin(format!("lame quality: {e:?}")))?;
        let encoder = b
            .build()
            .map_err(|e| RecordingError::Plugin(format!("lame build: {e:?}")))?;
        let out = BufWriter::new(File::create(path.as_ref())?);
        Ok(Self {
            encoder,
            out,
            scratch: vec![MaybeUninit::uninit(); 8192 + max_required_buffer_size(1152 * 2)],
            channels: ch as u8,
        })
    }

    fn ensure_scratch(&mut self, frames: usize) {
        // LAME docs: reserve using per-channel sample count.
        let need = 8192 + max_required_buffer_size(frames);
        if self.scratch.len() < need {
            self.scratch.resize(need, MaybeUninit::uninit());
        }
    }
}

impl AudioSink for Mp3Sink {
    fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> Result<()> {
        self.ensure_scratch(buffer.frames);
        let out_slice = &mut self.scratch[..];
        let written = match self.channels {
            1 => self
                .encoder
                .encode(MonoPcm(buffer.data.as_ref()), out_slice)
                .map_err(|e| RecordingError::Plugin(format!("lame encode: {e:?}")))?,
            2 => self
                .encoder
                .encode(InterleavedPcm(buffer.data.as_ref()), out_slice)
                .map_err(|e| RecordingError::Plugin(format!("lame encode: {e:?}")))?,
            _ => unreachable!(),
        };
        let bytes = unsafe { slice_assume_init(&out_slice[..written]) };
        self.out.write_all(bytes)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.ensure_scratch(1);
        let out_slice = &mut self.scratch[..];
        let written = self
            .encoder
            .flush::<FlushNoGap>(out_slice)
            .map_err(|e| RecordingError::Plugin(format!("lame flush: {e:?}")))?;
        let bytes = unsafe { slice_assume_init(&out_slice[..written]) };
        self.out.write_all(bytes)?;
        self.out.flush()?;
        Ok(())
    }
}

unsafe fn slice_assume_init(slice: &[MaybeUninit<u8>]) -> &[u8] {
    std::slice::from_raw_parts(slice.as_ptr() as *const u8, slice.len())
}
