//! Fan-out [`AudioSink`] for driving multiple outputs from one bus.

use crossbeam_channel::Sender;

use crate::buffer::AudioBuffer;
use crate::error::Result;
use crate::traits::AudioSink;

/// Forwards every buffer to each child sink in order. If one child returns an error, that
/// error propagates and later children are skipped for that buffer.
pub struct CompositeSink {
    children: Vec<Box<dyn AudioSink>>,
}

impl CompositeSink {
    pub fn new(children: Vec<Box<dyn AudioSink>>) -> Self {
        Self { children }
    }

    pub fn push(&mut self, sink: Box<dyn AudioSink>) {
        self.children.push(sink);
    }

    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }
}

impl AudioSink for CompositeSink {
    fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> Result<()> {
        for s in self.children.iter_mut() {
            s.write_pcm_f32(buffer)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        for s in self.children.iter_mut() {
            s.flush()?;
        }
        Ok(())
    }
}

/// Clone each buffer to multiple bus queues (strip → multiple buses). Uses `try_send` like
/// [`MixerInputSink`]: full queues drop silently.
pub struct TeeAudioSink {
    txs: Vec<Sender<AudioBuffer>>,
}

impl TeeAudioSink {
    pub fn new(txs: Vec<Sender<AudioBuffer>>) -> Self {
        Self { txs }
    }
}

impl AudioSink for TeeAudioSink {
    fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> Result<()> {
        for tx in &self.txs {
            let _ = tx.try_send(buffer.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct VecSink(Arc<Mutex<Vec<AudioBuffer>>>);
    impl AudioSink for VecSink {
        fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> Result<()> {
            self.0.lock().unwrap().push(buffer.clone());
            Ok(())
        }
    }

    #[test]
    fn composite_fanout() {
        let a = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let b = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let mut c = CompositeSink::new(vec![
            Box::new(VecSink(a.clone())),
            Box::new(VecSink(b.clone())),
        ]);
        let f = crate::format::AudioFormat::new(48_000, 1, crate::format::SampleFormat::F32);
        let buf = AudioBuffer::silent(f, 4, std::time::Instant::now(), 0);
        c.write_pcm_f32(&buf).unwrap();
        assert_eq!(a.lock().unwrap().len(), 1);
        assert_eq!(b.lock().unwrap().len(), 1);
    }
}
