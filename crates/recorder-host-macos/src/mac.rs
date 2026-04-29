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

/// macOS Core Audio doesn't expose system-audio loopback natively, so users typically
/// install a virtual loopback driver. Match the common ones by name.
fn is_virtual_loopback_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("blackhole")
        // Rogue Amoeba's Loopback creates devices named e.g. "Loopback Audio".
        || lower.contains("loopback")
        || lower.contains("soundflower")
        || lower.contains("vb-cable")
        || lower.contains("vb cable")
}

/// macOS Core Audio capture host (via cpal).
pub struct MacosHost {
    host: Host,
}

impl Default for MacosHost {
    fn default() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

impl MacosHost {
    pub fn new() -> Result<Self> {
        Ok(Self::default())
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

impl AudioHost for MacosHost {
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
        let mut sources: Vec<CaptureSource> = inputs
            .into_iter()
            .map(|d| {
                let kind = if is_virtual_loopback_name(&d.name) {
                    CaptureSourceKind::Loopback
                } else {
                    CaptureSourceKind::Input
                };
                CaptureSource {
                    id: d.id,
                    name: d.name,
                    default_format: d.default_format,
                    kind,
                }
            })
            .collect();
        // Prepend ScreenCaptureKit's synthetic system-audio source when available so it
        // appears at the head of the loopback list (preferred over BlackHole/etc.).
        #[cfg(feature = "screencapturekit")]
        if crate::screen_capture::is_available() {
            sources.insert(0, crate::screen_capture::synthetic_source());
        }
        Ok(sources)
    }

    fn start_capture(
        &self,
        source_id: Option<&str>,
        kind: CaptureSourceKind,
        format: AudioFormat,
        on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
    ) -> Result<StreamHandle> {
        // Route ScreenCaptureKit's synthetic system-audio source to the SCK backend.
        #[cfg(feature = "screencapturekit")]
        if matches!(kind, CaptureSourceKind::Loopback)
            && source_id == Some(crate::screen_capture::SOURCE_ID)
        {
            let _ = format; // SCK uses its negotiated 48 kHz / 2-ch mix.
            return crate::screen_capture::start(on_buffer);
        }
        // Otherwise virtual loopback drivers (BlackHole, Loopback, Soundflower, VB-Cable)
        // appear as ordinary input devices to Core Audio, so loopback capture goes
        // through the standard input stream path.
        let _ = kind;
        self.start_input_stream(source_id, format, on_buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::is_virtual_loopback_name;

    #[test]
    fn known_virtual_drivers_are_recognized() {
        assert!(is_virtual_loopback_name("BlackHole 2ch"));
        assert!(is_virtual_loopback_name("blackhole 16ch"));
        assert!(is_virtual_loopback_name("Loopback Audio"));
        assert!(is_virtual_loopback_name("Soundflower (2ch)"));
        assert!(is_virtual_loopback_name("VB-Cable"));
        assert!(is_virtual_loopback_name("VB Cable"));
    }

    #[test]
    fn regular_inputs_are_not_loopback() {
        assert!(!is_virtual_loopback_name("MacBook Pro Microphone"));
        assert!(!is_virtual_loopback_name("USB Audio Device"));
    }
}
