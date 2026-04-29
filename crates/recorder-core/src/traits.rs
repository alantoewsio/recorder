use std::sync::Arc;

use crate::buffer::AudioBuffer;
use crate::error::{RecordingError, Result};
use crate::format::AudioFormat;

pub use crate::analyzer::AudioAnalyzer;

/// Receives encoded or raw PCM from writer threads (never called from plugin code).
pub trait AudioSink: Send + 'static {
    /// Called on the sink's dedicated writer thread.
    fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> Result<()>;
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// In-process plugin: transforms `input` into `output` (pre-sized by the pipeline).
pub trait AudioProcessor: Send {
    fn name(&self) -> &str {
        "plugin"
    }
    fn reset(&mut self) {}
    fn process(&mut self, input: &AudioBuffer, output: &mut AudioBuffer) -> Result<()>;
}

/// Type-erased processor for runtime chains.
pub type DynProcessor = dyn AudioProcessor + Send;

/// Describes an input device exposed by a host crate.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub default_format: Option<AudioFormat>,
}

/// Whether a capture source delivers microphone-style input or speaker-output loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureSourceKind {
    /// A regular input device such as a microphone or line-in.
    Input,
    /// A loopback / monitor source that delivers what the system is rendering to an output
    /// endpoint (Windows WASAPI loopback, PulseAudio/PipeWire `*.monitor`,
    /// BlackHole-style virtual devices on macOS, ScreenCaptureKit on macOS 13+).
    Loopback,
}

/// Describes one capture source (microphone or speaker loopback) exposed by a host crate.
#[derive(Debug, Clone)]
pub struct CaptureSource {
    pub id: String,
    pub name: String,
    pub default_format: Option<AudioFormat>,
    pub kind: CaptureSourceKind,
}

impl From<DeviceInfo> for CaptureSource {
    fn from(d: DeviceInfo) -> Self {
        Self {
            id: d.id,
            name: d.name,
            default_format: d.default_format,
            kind: CaptureSourceKind::Input,
        }
    }
}

/// Hosts enumerate devices and start capture into a pipeline ingest callback.
pub trait AudioHost: Send + Sync {
    /// Lists microphone-style input devices.
    ///
    /// Prefer [`AudioHost::list_capture_sources`], which also reports loopback sources
    /// where the host can capture them.
    fn list_input_devices(&self) -> Result<Vec<DeviceInfo>>;

    /// Opens a capture stream for a microphone-style input device; `on_buffer` is invoked
    /// on the host audio thread.
    ///
    /// Prefer [`AudioHost::start_capture`], which can also open loopback streams where the
    /// host supports them.
    fn start_input_stream(
        &self,
        device_id: Option<&str>,
        format: AudioFormat,
        on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
    ) -> Result<StreamHandle>;

    /// Lists every capture source the host can open, including microphone-style inputs and
    /// any loopback (system-audio) sources it can capture from.
    ///
    /// The default implementation reports only [`CaptureSourceKind::Input`] entries
    /// derived from [`AudioHost::list_input_devices`]; per-OS hosts override this to add
    /// loopback sources where they exist (WASAPI output endpoints, PulseAudio monitors,
    /// macOS virtual loopback devices, ScreenCaptureKit system audio).
    fn list_capture_sources(&self) -> Result<Vec<CaptureSource>> {
        Ok(self
            .list_input_devices()?
            .into_iter()
            .map(CaptureSource::from)
            .collect())
    }

    /// Opens a capture stream for any kind of source (microphone or loopback).
    ///
    /// The default implementation delegates microphone capture to
    /// [`AudioHost::start_input_stream`] and rejects loopback requests; per-OS hosts
    /// override this where they can serve loopback sources.
    fn start_capture(
        &self,
        source_id: Option<&str>,
        kind: CaptureSourceKind,
        format: AudioFormat,
        on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
    ) -> Result<StreamHandle> {
        match kind {
            CaptureSourceKind::Input => self.start_input_stream(source_id, format, on_buffer),
            CaptureSourceKind::Loopback => Err(RecordingError::Config(
                "loopback capture is not supported by this audio host".into(),
            )),
        }
    }

    /// Playback / render devices (for monitor bus → speakers). Default: none.
    fn list_output_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![])
    }

    /// Low-latency output: the device callback invokes `fill` with interleaved f32 samples
    /// (`frames * format.channels` elements). `format` must match the device's default
    /// output configuration. Default: unsupported.
    fn start_output_stream(
        &self,
        _device_id: Option<&str>,
        _format: AudioFormat,
        _fill: Arc<dyn Fn(&mut [f32]) + Send + Sync>,
    ) -> Result<StreamHandle> {
        Err(RecordingError::Config(
            "start_output_stream is not implemented for this host".into(),
        ))
    }
}

/// Opaque stop control for a running host stream.
pub struct StreamHandle {
    stop: Box<dyn FnOnce() + Send>,
}

impl StreamHandle {
    pub fn new(stop: impl FnOnce() + Send + 'static) -> Self {
        Self {
            stop: Box::new(stop),
        }
    }

    pub fn stop(self) {
        (self.stop)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InputOnlyHost;

    impl AudioHost for InputOnlyHost {
        fn list_input_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(vec![DeviceInfo {
                id: "mic-1".into(),
                name: "Mic".into(),
                default_format: None,
            }])
        }

        fn start_input_stream(
            &self,
            _device_id: Option<&str>,
            _format: AudioFormat,
            _on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
        ) -> Result<StreamHandle> {
            Ok(StreamHandle::new(|| {}))
        }
    }

    #[test]
    fn default_list_capture_sources_marks_inputs() {
        let sources = InputOnlyHost.list_capture_sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, CaptureSourceKind::Input);
        assert_eq!(sources[0].id, "mic-1");
    }

    #[test]
    fn default_start_capture_rejects_loopback() {
        let on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync> = Arc::new(|_| {});
        let result = InputOnlyHost.start_capture(
            None,
            CaptureSourceKind::Loopback,
            AudioFormat::new(48_000, 2, crate::format::SampleFormat::F32),
            on_buffer,
        );
        match result {
            Err(RecordingError::Config(msg)) => assert!(msg.contains("loopback")),
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected default loopback to error"),
        }
    }
}
