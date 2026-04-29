//! Bounded SPSC-style frame queues built on `crossbeam_channel`.
//! Raw and processed paths each get their own bounded queue so backpressure is isolated.

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};

use crate::buffer::AudioBuffer;

/// Producer handle for pushing captured frames.
#[derive(Clone)]
pub struct FrameSender {
    inner: Sender<AudioBuffer>,
}

impl FrameSender {
    pub fn try_send(&self, frame: AudioBuffer) -> Result<(), TrySendError<AudioBuffer>> {
        self.inner.try_send(frame)
    }
}

/// Consumer side used by writer threads.
pub struct FrameReceiver {
    pub inner: Receiver<AudioBuffer>,
}

pub fn frame_queue(capacity: usize) -> (FrameSender, FrameReceiver) {
    let (tx, rx) = bounded(capacity);
    (FrameSender { inner: tx }, FrameReceiver { inner: rx })
}
