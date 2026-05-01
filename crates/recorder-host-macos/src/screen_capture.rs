//! Native macOS system-audio loopback via ScreenCaptureKit (macOS 13+).
//!
//! ScreenCaptureKit doesn't expose pure-audio sources; we attach an SCStream to the first
//! display, configure it to capture audio (no video frames), and forward every audio
//! `CMSampleBuffer` it delivers as a `recorder-core` `AudioBuffer`. Audio is delivered as
//! deinterleaved 32-bit float per channel at the sample rate negotiated through
//! `SCStreamConfiguration::with_sample_rate`.
//!
//! ## Permissions
//!
//! On first use the OS prompts the user for **Screen Recording** permission. If the user
//! declines, `start` returns `RecordingError::Device` with a message guiding them to
//! System Settings → Privacy & Security → Screen Recording.
//!
//! ## OS gating
//!
//! [`is_available`] runs `SCShareableContent::get` on a probe call; if SCK is unavailable
//! (macOS < 13 or the framework declines for some other reason) we fall back to the
//! virtual-device loopback path in `mac.rs`.

#![cfg(all(target_os = "macos", feature = "screencapturekit"))]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use recorder_core::buffer::AudioBuffer as RAudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::{AudioFormat, SampleFormat as RSample};
use recorder_core::traits::{CaptureSource, CaptureSourceKind, StreamHandle};

use screencapturekit::cm::CMSampleBuffer;
use screencapturekit::prelude::*;

/// Synthetic source id used for the SCK-backed system-audio source. Stable across
/// `list_capture_sources` calls so UIs can persist the user's selection.
pub const SOURCE_ID: &str = "scl:system-audio";
const SOURCE_NAME: &str = "System Audio (ScreenCaptureKit)";
const TARGET_SAMPLE_RATE: u32 = 48_000;
const TARGET_CHANNELS: u16 = 2;

/// Probe whether ScreenCaptureKit is usable on this OS. Currently treats any successful
/// `SCShareableContent::get` as "available", which holds on macOS 12.3+ with audio
/// support officially landing in macOS 13.
pub fn is_available() -> bool {
    SCShareableContent::get().is_ok()
}

/// The synthetic `CaptureSource` shown in `list_capture_sources` when SCK is available.
pub fn synthetic_source() -> CaptureSource {
    CaptureSource {
        id: SOURCE_ID.into(),
        name: SOURCE_NAME.into(),
        default_format: Some(AudioFormat::new(
            TARGET_SAMPLE_RATE,
            TARGET_CHANNELS,
            RSample::F32,
        )),
        kind: CaptureSourceKind::Loopback,
        app: None,
    }
}

struct AudioHandler {
    on_buffer: Arc<dyn Fn(RAudioBuffer) + Send + Sync>,
    sample_rate: u32,
    frame_index: AtomicU64,
}

impl SCStreamOutputTrait for AudioHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }
        let Some(list) = sample.audio_buffer_list() else {
            return;
        };
        let frames = sample.num_samples();
        if frames == 0 {
            return;
        }
        // SCK delivers planar f32: one AudioBuffer per channel. Interleave into the shape
        // the rest of the pipeline expects (`AudioFormat::F32` interleaved).
        let n_channels = list.num_buffers();
        if n_channels == 0 {
            return;
        }
        let channels = n_channels as u16;
        let mut planar: Vec<&[u8]> = Vec::with_capacity(n_channels);
        for i in 0..n_channels {
            if let Some(buf) = list.get(i) {
                planar.push(buf.data());
            } else {
                return;
            }
        }
        let mut interleaved = Vec::with_capacity(frames * n_channels);
        for f in 0..frames {
            for ch_bytes in planar.iter() {
                interleaved.push(read_f32_at(ch_bytes, f).unwrap_or(0.0));
            }
        }
        let format = AudioFormat::new(self.sample_rate, channels, RSample::F32);
        let frame_index = self.frame_index.fetch_add(frames as u64, Ordering::Relaxed);
        let buf = RAudioBuffer::new(
            format,
            Arc::from(interleaved.into_boxed_slice()),
            frames,
            // CMSampleBuffer presentation timestamps use the host time clock; using
            // `Instant::now()` here keeps the cross-stream `captured_at` anchor consistent
            // with what other host crates emit.
            Instant::now(),
            frame_index,
        );
        (self.on_buffer)(buf);
    }
}

fn read_f32_at(bytes: &[u8], frame: usize) -> Option<f32> {
    let offset = frame.checked_mul(4)?;
    let end = offset.checked_add(4)?;
    if end > bytes.len() {
        return None;
    }
    let arr: [u8; 4] = bytes[offset..end].try_into().ok()?;
    Some(f32::from_ne_bytes(arr))
}

/// Open a ScreenCaptureKit system-audio stream. Pumps audio sample buffers into
/// `on_buffer` until [`StreamHandle::stop`] is called.
pub fn start(on_buffer: Arc<dyn Fn(RAudioBuffer) + Send + Sync>) -> Result<StreamHandle> {
    let content = SCShareableContent::get().map_err(|e| {
        RecordingError::Device(format!(
            "ScreenCaptureKit unavailable (grant Screen Recording permission?): {e}"
        ))
    })?;
    let displays = content.displays();
    let display = displays
        .first()
        .ok_or_else(|| RecordingError::Device("ScreenCaptureKit reported no displays".into()))?;

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();
    let config = SCStreamConfiguration::new()
        .with_captures_audio(true)
        .with_sample_rate(TARGET_SAMPLE_RATE as i32)
        .with_channel_count(TARGET_CHANNELS as i32);

    let mut stream = SCStream::new(&filter, &config);
    let handler = AudioHandler {
        on_buffer,
        sample_rate: TARGET_SAMPLE_RATE,
        frame_index: AtomicU64::new(0),
    };
    stream.add_output_handler(handler, SCStreamOutputType::Audio);
    stream.start_capture().map_err(|e| {
        RecordingError::Device(format!(
            "ScreenCaptureKit start_capture failed: {e} (grant Screen Recording permission \
             in System Settings → Privacy & Security)"
        ))
    })?;

    Ok(StreamHandle::new(move || {
        if let Err(e) = stream.stop_capture() {
            tracing::warn!("ScreenCaptureKit stop_capture: {e}");
        }
    }))
}
