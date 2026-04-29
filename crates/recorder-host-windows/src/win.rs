use recorder_core::error::{RecordingError, Result};
use recorder_core::format::AudioFormat;
use recorder_core::traits::{
    AudioHost, CaptureSource, CaptureSourceKind, DeviceInfo, StreamHandle,
};
use std::sync::Arc;

use crate::audio_system::WindowsAudioSystem;
use crate::capture_cpal;
use crate::capture_dsound;
use crate::capture_dummy;
use crate::capture_wavein;

/// Windows capture host: WASAPI, optional ASIO (via cpal), DirectSound, WinMM waveIn, or dummy.
#[derive(Clone, Debug)]
pub struct WindowsHost {
    pub audio_system: WindowsAudioSystem,
}

impl Default for WindowsHost {
    fn default() -> Self {
        Self {
            audio_system: WindowsAudioSystem::Wasapi,
        }
    }
}

impl WindowsHost {
    pub fn new(audio_system: WindowsAudioSystem) -> Result<Self> {
        Ok(Self { audio_system })
    }
}

impl AudioHost for WindowsHost {
    fn list_input_devices(&self) -> Result<Vec<DeviceInfo>> {
        match self.audio_system {
            WindowsAudioSystem::Wasapi => {
                let host = capture_cpal::host_wasapi()?;
                capture_cpal::list_input_devices(&host)
            }
            WindowsAudioSystem::Asio => {
                let host = capture_cpal::host_asio()?;
                capture_cpal::list_input_devices(&host)
            }
            WindowsAudioSystem::DirectSound => capture_dsound::list_devices(),
            WindowsAudioSystem::WaveOut => capture_wavein::list_devices(),
            WindowsAudioSystem::Dummy => Ok(capture_dummy::list_devices()),
        }
    }

    fn start_input_stream(
        &self,
        device_id: Option<&str>,
        format: AudioFormat,
        on_buffer: Arc<dyn Fn(recorder_core::buffer::AudioBuffer) + Send + Sync>,
    ) -> Result<StreamHandle> {
        match self.audio_system {
            WindowsAudioSystem::Wasapi => {
                let host = capture_cpal::host_wasapi()?;
                capture_cpal::start_input_stream(&host, device_id, format, on_buffer)
            }
            WindowsAudioSystem::Asio => {
                let host = capture_cpal::host_asio()?;
                capture_cpal::start_input_stream(&host, device_id, format, on_buffer)
            }
            WindowsAudioSystem::DirectSound => {
                capture_dsound::start_stream(device_id, format, on_buffer)
            }
            WindowsAudioSystem::WaveOut => {
                capture_wavein::start_stream(device_id, format, on_buffer)
            }
            WindowsAudioSystem::Dummy => capture_dummy::start_stream(device_id, format, on_buffer),
        }
    }

    fn list_capture_sources(&self) -> Result<Vec<CaptureSource>> {
        let mut sources: Vec<CaptureSource> = self
            .list_input_devices()?
            .into_iter()
            .map(CaptureSource::from)
            .collect();
        // Only WASAPI exposes loopback render endpoints to cpal.
        if matches!(self.audio_system, WindowsAudioSystem::Wasapi) {
            let host = capture_cpal::host_wasapi()?;
            for d in capture_cpal::list_output_devices(&host)? {
                sources.push(CaptureSource {
                    id: d.id,
                    name: d.name,
                    default_format: d.default_format,
                    kind: CaptureSourceKind::Loopback,
                });
            }
        }
        Ok(sources)
    }

    fn start_capture(
        &self,
        source_id: Option<&str>,
        kind: CaptureSourceKind,
        format: AudioFormat,
        on_buffer: Arc<dyn Fn(recorder_core::buffer::AudioBuffer) + Send + Sync>,
    ) -> Result<StreamHandle> {
        match kind {
            CaptureSourceKind::Input => self.start_input_stream(source_id, format, on_buffer),
            CaptureSourceKind::Loopback => match self.audio_system {
                WindowsAudioSystem::Wasapi => {
                    let host = capture_cpal::host_wasapi()?;
                    capture_cpal::start_loopback_stream(&host, source_id, format, on_buffer)
                }
                other => Err(RecordingError::Config(format!(
                    "loopback capture is only supported by WASAPI on Windows; current audio system is {}",
                    other.label()
                ))),
            },
        }
    }

    fn list_output_devices(&self) -> Result<Vec<DeviceInfo>> {
        match self.audio_system {
            WindowsAudioSystem::Wasapi => {
                let host = capture_cpal::host_wasapi()?;
                capture_cpal::list_output_devices(&host)
            }
            WindowsAudioSystem::Asio => {
                let host = capture_cpal::host_asio()?;
                capture_cpal::list_output_devices(&host)
            }
            _ => Ok(vec![]),
        }
    }

    fn start_output_stream(
        &self,
        device_id: Option<&str>,
        format: AudioFormat,
        fill: Arc<dyn Fn(&mut [f32]) + Send + Sync>,
    ) -> Result<StreamHandle> {
        match self.audio_system {
            WindowsAudioSystem::Wasapi => {
                let host = capture_cpal::host_wasapi()?;
                capture_cpal::start_output_stream(&host, device_id, format, fill)
            }
            WindowsAudioSystem::Asio => {
                let host = capture_cpal::host_asio()?;
                capture_cpal::start_output_stream(&host, device_id, format, fill)
            }
            other => Err(RecordingError::Config(format!(
                "output streams are not supported for {}",
                other.label()
            ))),
        }
    }
}
