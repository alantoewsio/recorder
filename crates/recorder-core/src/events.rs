use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};

use crate::control::{ParameterValue, PluginId};

/// Identifies where in the media pipeline an event was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioTap {
    Raw,
    Processed,
}

/// Time-aligned metadata emitted by analyzers and host automation.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaEvent {
    VoiceActivity {
        tap: AudioTap,
        start_frame: u64,
        end_frame: u64,
        active: bool,
        level: f32,
        confidence: f32,
    },
    SpeakerSegment {
        tap: AudioTap,
        start_frame: u64,
        end_frame: u64,
        speaker_id: String,
        confidence: f32,
    },
    TranscriptPartial {
        tap: AudioTap,
        stream_id: Option<String>,
        segment_id: u64,
        start_frame: u64,
        end_frame: u64,
        speaker_id: Option<String>,
        text: String,
        confidence: Option<f32>,
    },
    TranscriptFinal {
        tap: AudioTap,
        stream_id: Option<String>,
        segment_id: u64,
        start_frame: u64,
        end_frame: u64,
        speaker_id: Option<String>,
        text: String,
        confidence: Option<f32>,
    },
    AttributeDetected {
        tap: AudioTap,
        start_frame: u64,
        end_frame: u64,
        key: String,
        value: String,
        confidence: Option<f32>,
    },
    PluginParameterChanged {
        plugin_id: PluginId,
        parameter_id: String,
        value: ParameterValue,
        effective_frame: Option<u64>,
    },
}

#[derive(Clone)]
pub struct MediaEventSender {
    inner: Sender<MediaEvent>,
}

impl MediaEventSender {
    pub fn try_send(&self, event: MediaEvent) -> Result<(), TrySendError<MediaEvent>> {
        self.inner.try_send(event)
    }
}

#[derive(Clone)]
pub struct MediaEventReceiver {
    inner: Receiver<MediaEvent>,
}

impl MediaEventReceiver {
    pub fn try_iter(&self) -> crossbeam_channel::TryIter<'_, MediaEvent> {
        self.inner.try_iter()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

pub fn media_event_queue(capacity: usize) -> (MediaEventSender, MediaEventReceiver) {
    let (tx, rx) = bounded(capacity);
    (
        MediaEventSender { inner: tx },
        MediaEventReceiver { inner: rx },
    )
}
