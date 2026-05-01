use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, SampleFormat, StreamConfig};
use recorder_core::buffer::AudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::{AudioFormat, SampleFormat as RSample};
use recorder_core::traits::{
    AudioHost, CaptureSource, CaptureSourceKind, DeviceInfo, StreamHandle,
};

/// Linux capture host (via cpal; backend depends on ALSA/Pulse/Jack system configuration).
pub struct LinuxHost {
    host: Host,
}

impl Default for LinuxHost {
    fn default() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

/// PulseAudio and PipeWire publish `<sink>.monitor` (or "Monitor of <sink>") sources for
/// each output sink; cpal lists them alongside hardware inputs. Use this heuristic to
/// classify them.
fn is_monitor_source(name: &str) -> bool {
    name.ends_with(".monitor") || name.contains("Monitor of ") || name.contains("monitor of ")
}

impl LinuxHost {
    pub fn new() -> Result<Self> {
        Ok(Self::default())
    }

    /// Best-effort lookup of the default sink's monitor source id, intended for UIs that
    /// want to pre-select a sensible "speaker output" entry.
    pub fn default_loopback_source_id(&self) -> Option<String> {
        let output_name = self.host.default_output_device()?.name().ok()?;
        let inputs = self.list_input_devices().ok()?;
        // Try common naming patterns: `<sink>.monitor`, `Monitor of <sink>`.
        inputs.into_iter().find_map(|d| {
            if d.name == format!("{output_name}.monitor")
                || d.name == format!("Monitor of {output_name}")
                || (is_monitor_source(&d.name) && d.name.contains(&output_name))
            {
                Some(d.id)
            } else {
                None
            }
        })
    }

    fn resolve_device(&self, device_id: Option<&str>) -> Result<Device> {
        let inputs: Vec<_> = self
            .host
            .input_devices()
            .map_err(|e| RecordingError::Device(format!("enumerating input devices: {e}")))?
            .collect();
        match device_id {
            None => self
                .host
                .default_input_device()
                .ok_or_else(|| RecordingError::Device("no default input device".into())),
            Some(id) => {
                for d in inputs {
                    if let Ok(dev_id) = d.id() {
                        if dev_id.to_string() == *id {
                            return Ok(d);
                        }
                    }
                }
                Err(RecordingError::Device(format!(
                    "input device not found: {id}"
                )))
            }
        }
    }
}

impl AudioHost for LinuxHost {
    fn list_input_devices(&self) -> Result<Vec<DeviceInfo>> {
        let mut out = Vec::new();
        let inputs = self
            .host
            .input_devices()
            .map_err(|e| RecordingError::Device(format!("enumerating input devices: {e}")))?
            .collect::<Vec<_>>();
        for device in inputs {
            let id = device
                .id()
                .map_err(|e| RecordingError::Device(format!("device id: {e}")))?;
            let desc = device
                .description()
                .map_err(|e| RecordingError::Device(format!("device description: {e}")))?;
            let name = desc.name().to_string();
            let default_format = device.default_input_config().ok().map(|c| {
                let cfg = c.config();
                let sf = match c.sample_format() {
                    SampleFormat::F32 => RSample::F32,
                    SampleFormat::I16 => RSample::I16,
                    _ => RSample::F32,
                };
                AudioFormat::new(cfg.sample_rate, cfg.channels, sf)
            });
            out.push(DeviceInfo {
                id: id.to_string(),
                name,
                default_format,
            });
        }
        Ok(out)
    }

    fn start_input_stream(
        &self,
        device_id: Option<&str>,
        format: AudioFormat,
        on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
    ) -> Result<StreamHandle> {
        let device = self.resolve_device(device_id)?;
        let def = device
            .default_input_config()
            .map_err(|e| RecordingError::Device(format!("default input config: {e}")))?;
        let cfg: StreamConfig = def.config();
        if cfg.channels != format.channels {
            return Err(RecordingError::Device(format!(
                "device has {} channels; session requested {}",
                cfg.channels, format.channels
            )));
        }
        if cfg.sample_rate != format.sample_rate_hz {
            return Err(RecordingError::Device(format!(
                "device default rate is {} Hz; session requested {} Hz",
                cfg.sample_rate, format.sample_rate_hz
            )));
        }
        let ch = cfg.channels as usize;
        let ch_u16 = cfg.channels;
        let sr = cfg.sample_rate;
        let frame_counter = Arc::new(AtomicU64::new(0));
        let counter_cb = frame_counter.clone();

        let stream = match def.sample_format() {
            SampleFormat::F32 => device.build_input_stream(
                &cfg,
                move |data: &[f32], _| {
                    let frames = data.len() / ch;
                    let idx = counter_cb.fetch_add(frames as u64, Ordering::Relaxed);
                    let buf = AudioBuffer::new(
                        AudioFormat::new(sr, ch_u16, RSample::F32),
                        Arc::from(data.to_vec().into_boxed_slice()),
                        frames,
                        Instant::now(),
                        idx,
                    );
                    on_buffer(buf);
                },
                |e| tracing::error!("cpal stream error: {e:?}"),
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &cfg,
                move |data: &[i16], _| {
                    let frames = data.len() / ch;
                    let idx = counter_cb.fetch_add(frames as u64, Ordering::Relaxed);
                    let buf = AudioBuffer::from_interleaved_i16(
                        AudioFormat::new(sr, ch_u16, RSample::I16),
                        data,
                        frames,
                        Instant::now(),
                        idx,
                    );
                    on_buffer(buf);
                },
                |e| tracing::error!("cpal stream error: {e:?}"),
                None,
            ),
            other => {
                return Err(RecordingError::Device(format!(
                    "unsupported device sample format: {other:?}"
                )));
            }
        }
        .map_err(|e| RecordingError::Device(format!("build_input_stream: {e}")))?;

        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            if let Err(e) = stream.play() {
                tracing::error!("cpal play stream: {e}");
                return;
            }
            let _ = stop_rx.recv();
            let _ = stream.pause();
        });

        Ok(StreamHandle::new(move || {
            let _ = stop_tx.send(());
            let _ = join.join();
        }))
    }

    fn list_capture_sources(&self) -> Result<Vec<CaptureSource>> {
        let inputs = self.list_input_devices()?;
        Ok(inputs
            .into_iter()
            .map(|d| {
                let kind = if is_monitor_source(&d.name) {
                    CaptureSourceKind::Loopback
                } else {
                    CaptureSourceKind::Input
                };
                CaptureSource {
                    id: d.id,
                    name: d.name,
                    default_format: d.default_format,
                    kind,
                    app: None,
                }
            })
            .collect())
    }

    fn start_capture(
        &self,
        source_id: Option<&str>,
        // Loopback monitor sources are surfaced through the same input-device API on
        // PulseAudio/PipeWire, so the kind is informational only.
        _kind: CaptureSourceKind,
        format: AudioFormat,
        on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
    ) -> Result<StreamHandle> {
        if matches!(_kind, CaptureSourceKind::AppOutput) {
            return Err(RecordingError::Config(
                "app-output capture is not implemented for the Linux host yet".into(),
            ));
        }
        self.start_input_stream(source_id, format, on_buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::is_monitor_source;

    #[test]
    fn monitor_heuristic_matches_pulse_pipewire_names() {
        assert!(is_monitor_source(
            "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor"
        ));
        assert!(is_monitor_source("Monitor of Built-in Audio Analog Stereo"));
        assert!(!is_monitor_source(
            "alsa_input.pci-0000_00_1f.3.analog-stereo"
        ));
        assert!(!is_monitor_source("Built-in Microphone"));
    }
}
