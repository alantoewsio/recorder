use crate::buffer::AudioBuffer;
use crate::events::{AudioTap, MediaEvent};
use crate::Result;

/// Observes audio and emits time-aligned metadata events.
///
/// Implementations run on analyzer worker threads fed by bounded queues, not directly on the
/// host audio callback. `accept_audio` can do heavier work than an `AudioProcessor`, but should
/// still avoid unbounded buffering so stale analysis does not accumulate indefinitely.
pub trait AudioAnalyzer: Send {
    fn name(&self) -> &str;
    fn accept_audio(&mut self, input: &AudioBuffer) -> Result<()>;
    fn drain_events(&mut self) -> Vec<MediaEvent>;
}

/// Simple analyzer used to prove event plumbing before integrating ASR/diarization models.
pub struct VoiceActivityAnalyzer {
    threshold: f32,
    tap: AudioTap,
    was_active: Option<bool>,
    pending: Vec<MediaEvent>,
}

impl VoiceActivityAnalyzer {
    pub fn new(threshold: f32) -> Self {
        Self {
            threshold,
            tap: AudioTap::Processed,
            was_active: None,
            pending: Vec::new(),
        }
    }

    pub fn with_tap(mut self, tap: AudioTap) -> Self {
        self.tap = tap;
        self
    }
}

impl Default for VoiceActivityAnalyzer {
    fn default() -> Self {
        Self::new(0.02)
    }
}

impl AudioAnalyzer for VoiceActivityAnalyzer {
    fn name(&self) -> &str {
        "voice-activity"
    }

    fn accept_audio(&mut self, input: &AudioBuffer) -> Result<()> {
        let sample_count = input.data.len().max(1);
        let sum_squares = input.data.iter().map(|sample| sample * sample).sum::<f32>();
        let rms = (sum_squares / sample_count as f32).sqrt();
        let active = rms >= self.threshold;

        if self.was_active != Some(active) {
            self.pending.push(MediaEvent::VoiceActivity {
                tap: self.tap,
                start_frame: input.frame_index,
                end_frame: input.frame_index + input.frames as u64,
                active,
                level: rms,
                confidence: if self.threshold > 0.0 {
                    (rms / self.threshold).clamp(0.0, 1.0)
                } else {
                    1.0
                },
            });
            self.was_active = Some(active);
        }

        Ok(())
    }

    fn drain_events(&mut self) -> Vec<MediaEvent> {
        std::mem::take(&mut self.pending)
    }
}
