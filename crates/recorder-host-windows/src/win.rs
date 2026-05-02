use recorder_core::error::{RecordingError, Result};
use recorder_core::format::AudioFormat;
use recorder_core::traits::{
    AppCaptureBackend, AppCaptureDescriptor, AudioHost, CaptureSource, CaptureSourceKind,
    DeviceInfo, StreamHandle,
};
use std::sync::Arc;

use crate::audio_system::WindowsAudioSystem;
use crate::capture_cpal;
use crate::capture_dsound;
use crate::capture_dummy;
use crate::capture_process_loopback;
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

fn parse_app_source_id(source_id: &str) -> Result<u32> {
    source_id
        .strip_prefix("app:")
        .ok_or_else(|| {
            RecordingError::Config(format!("invalid app-output source id: {source_id}"))
        })?
        .parse::<u32>()
        .map_err(|_| RecordingError::Config(format!("invalid app-output source id: {source_id}")))
}

fn app_process_source(process_id: u32, app_name: &str, app_id: Option<&str>) -> CaptureSource {
    CaptureSource {
        id: format!("app:{process_id}"),
        name: app_name.to_string(),
        default_format: Some(AudioFormat::new(
            48_000,
            2,
            recorder_core::format::SampleFormat::F32,
        )),
        kind: CaptureSourceKind::AppOutput,
        app: Some(AppCaptureDescriptor {
            backend: AppCaptureBackend::WindowsProcessLoopback,
            app_name: app_name.to_string(),
            app_id: app_id.map(str::to_string),
            process_id: Some(process_id),
            instance_id: Some(process_id.to_string()),
            supports_multi_select: true,
            requires_system_permission: false,
        }),
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
                    app: None,
                });
            }
            for (process_id, app_name, app_id) in capture_process_loopback::list_app_processes()? {
                sources.push(app_process_source(process_id, &app_name, app_id.as_deref()));
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
            CaptureSourceKind::AppOutput => match self.audio_system {
                WindowsAudioSystem::Wasapi => {
                    let source_id = source_id.ok_or_else(|| {
                        RecordingError::Config(
                            "app-output capture requires a selected app source id".into(),
                        )
                    })?;
                    let process_id = parse_app_source_id(source_id)?;
                    capture_process_loopback::start_process_loopback_stream(
                        process_id,
                        format,
                        on_buffer,
                    )
                }
                other => Err(RecordingError::Config(format!(
                    "app-output capture is only supported by WASAPI on Windows; current audio system is {}",
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

#[cfg(test)]
mod tests {
    use recorder_core::traits::AppCaptureBackend;

    use super::*;

    #[test]
    fn parse_app_source_id_accepts_pid_ids() {
        assert_eq!(parse_app_source_id("app:42").unwrap(), 42);
    }

    #[test]
    fn parse_app_source_id_rejects_non_app_ids() {
        assert!(parse_app_source_id("loopback:42").is_err());
        assert!(parse_app_source_id("app:not-a-number").is_err());
    }

    #[test]
    fn app_process_becomes_app_output_source() {
        let source = app_process_source(4242, "Spotify.exe", Some("C:\\Apps\\Spotify.exe"));
        assert_eq!(source.id, "app:4242");
        assert_eq!(source.kind, CaptureSourceKind::AppOutput);
        let app = source.app.expect("app metadata");
        assert_eq!(app.backend, AppCaptureBackend::WindowsProcessLoopback);
        assert_eq!(app.process_id, Some(4242));
        assert_eq!(app.app_name, "Spotify.exe");
        assert_eq!(app.app_id.as_deref(), Some("C:\\Apps\\Spotify.exe"));
        assert!(app.supports_multi_select);
        assert!(!app.requires_system_permission);
    }
}
