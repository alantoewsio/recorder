//! Shared realtime processor chain (strip pipeline and bus post-mix).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::buffer::AudioBuffer;
use crate::metrics::PipelineMetrics;
use crate::traits::AudioProcessor;

/// Run `processors` in order with the same timeout / passthrough semantics as
/// [`crate::pipeline::StreamPipeline`].
pub fn run_processor_chain(
    mut current: AudioBuffer,
    chain: &mut [Box<dyn AudioProcessor + Send>],
    budget: Option<Duration>,
    metrics: &PipelineMetrics,
) -> AudioBuffer {
    if chain.is_empty() {
        return current;
    }

    let mut scratch = AudioBuffer::silent(
        current.format,
        current.frames,
        current.captured_at,
        current.frame_index,
    );

    for plugin in chain.iter_mut() {
        let start = Instant::now();
        let mut timed_out = false;

        let res = plugin.process(&current, &mut scratch);

        if let Some(b) = budget {
            if start.elapsed() > b {
                timed_out = true;
            }
        }

        if res.is_err() || timed_out {
            metrics.plugin_timeouts.fetch_add(1, Ordering::Relaxed);
            passthrough_copy(&current, &mut scratch);
        }

        std::mem::swap(&mut current, &mut scratch);
    }

    current
}

pub(crate) fn passthrough_copy(from: &AudioBuffer, to: &mut AudioBuffer) {
    to.format = from.format;
    to.frames = from.frames;
    to.captured_at = from.captured_at;
    to.frame_index = from.frame_index;
    to.data = from.data.clone();
}
