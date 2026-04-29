use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, SampleFormat, StreamConfig};
use recorder_core::buffer::AudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::{AudioFormat, SampleFormat as RSample};
use recorder_core::traits::{DeviceInfo, StreamHandle};

pub fn host_wasapi() -> Result<Host> {
    cpal::host_from_id(cpal::HostId::Wasapi)
        .map_err(|e| RecordingError::Device(format!("WASAPI host unavailable: {e}")))
}

#[cfg(feature = "asio")]
pub fn host_asio() -> Result<Host> {
    if !cpal::available_hosts().contains(&cpal::HostId::Asio) {
        return Err(RecordingError::Device(
            "ASIO host not available (no ASIO driver or cpal ASIO build)".into(),
        ));
    }
    cpal::host_from_id(cpal::HostId::Asio)
        .map_err(|e| RecordingError::Device(format!("ASIO host unavailable: {e}")))
}

#[cfg(not(feature = "asio"))]
pub fn host_asio() -> Result<Host> {
    Err(RecordingError::Device(
        "ASIO is disabled in this build: enable the `asio` feature on `recorder-host-windows` and install the Steinberg ASIO SDK for cpal (see cpal README).".into(),
    ))
}

pub fn list_input_devices(host: &Host) -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    let inputs = host
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

/// Enumerate WASAPI output endpoints; cpal reports the endpoint's default render mix
/// format, which is also the format delivered by the loopback capture stream.
pub fn list_output_devices(host: &Host) -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    let outputs = host
        .output_devices()
        .map_err(|e| RecordingError::Device(format!("enumerating output devices: {e}")))?
        .collect::<Vec<_>>();
    for device in outputs {
        let id = device
            .id()
            .map_err(|e| RecordingError::Device(format!("device id: {e}")))?;
        let desc = device
            .description()
            .map_err(|e| RecordingError::Device(format!("device description: {e}")))?;
        let name = desc.name().to_string();
        let default_format = device.default_output_config().ok().map(|c| {
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

fn resolve_device(host: &Host, device_id: Option<&str>) -> Result<Device> {
    let inputs: Vec<_> = host
        .input_devices()
        .map_err(|e| RecordingError::Device(format!("enumerating input devices: {e}")))?
        .collect();
    match device_id {
        None => host
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

fn resolve_output_device(host: &Host, device_id: Option<&str>) -> Result<Device> {
    let outputs: Vec<_> = host
        .output_devices()
        .map_err(|e| RecordingError::Device(format!("enumerating output devices: {e}")))?
        .collect();
    match device_id {
        None => host
            .default_output_device()
            .ok_or_else(|| RecordingError::Device("no default output device".into())),
        Some(id) => {
            for d in outputs {
                if let Ok(dev_id) = d.id() {
                    if dev_id.to_string() == *id {
                        return Ok(d);
                    }
                }
            }
            Err(RecordingError::Device(format!(
                "output device not found: {id}"
            )))
        }
    }
}

pub fn start_input_stream(
    host: &Host,
    device_id: Option<&str>,
    format: AudioFormat,
    on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
) -> Result<StreamHandle> {
    let device = resolve_device(host, device_id)?;
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

/// Open a WASAPI loopback capture stream against a render endpoint. cpal sets
/// `AUDCLNT_STREAMFLAGS_LOOPBACK` automatically when `build_input_stream` is called on an
/// output device, which makes the endpoint deliver whatever it is currently rendering.
///
/// The endpoint format is fixed (the device's render mix format); callers must pass the
/// matching `AudioFormat` from [`list_output_devices`]. Use [`recorder_core::CaptureSource`]
/// `default_format` for that.
pub fn start_loopback_stream(
    host: &Host,
    device_id: Option<&str>,
    format: AudioFormat,
    on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
) -> Result<StreamHandle> {
    let device = resolve_output_device(host, device_id)?;
    let def = device
        .default_output_config()
        .map_err(|e| RecordingError::Device(format!("default output config: {e}")))?;
    let cfg: StreamConfig = def.config();
    if cfg.channels != format.channels {
        return Err(RecordingError::Device(format!(
            "loopback endpoint has {} channels; session requested {}",
            cfg.channels, format.channels
        )));
    }
    if cfg.sample_rate != format.sample_rate_hz {
        return Err(RecordingError::Device(format!(
            "loopback endpoint runs at {} Hz; session requested {} Hz",
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
                let frames = data.len() / ch.max(1);
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
            |e| tracing::error!("cpal loopback stream error: {e:?}"),
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &cfg,
            move |data: &[i16], _| {
                let frames = data.len() / ch.max(1);
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
            |e| tracing::error!("cpal loopback stream error: {e:?}"),
            None,
        ),
        other => {
            return Err(RecordingError::Device(format!(
                "unsupported loopback sample format: {other:?}"
            )));
        }
    }
    .map_err(|e| RecordingError::Device(format!("build_input_stream (loopback): {e}")))?;

    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let join = std::thread::spawn(move || {
        if let Err(e) = stream.play() {
            tracing::error!("cpal loopback play stream: {e}");
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

/// Low-latency playback on a render device. `fill` supplies interleaved f32 PCM each
/// callback; length is `frames * channels` for the stream's `format`.
pub fn start_output_stream(
    host: &Host,
    device_id: Option<&str>,
    format: AudioFormat,
    fill: Arc<dyn Fn(&mut [f32]) + Send + Sync>,
) -> Result<StreamHandle> {
    let device = resolve_output_device(host, device_id)?;
    let def = device
        .default_output_config()
        .map_err(|e| RecordingError::Device(format!("default output config: {e}")))?;
    let cfg: StreamConfig = def.config();
    if cfg.channels != format.channels {
        return Err(RecordingError::Device(format!(
            "output device has {} channels; session requested {}",
            cfg.channels, format.channels
        )));
    }
    if cfg.sample_rate != format.sample_rate_hz {
        return Err(RecordingError::Device(format!(
            "output device runs at {} Hz; session requested {} Hz",
            cfg.sample_rate, format.sample_rate_hz
        )));
    }

    let fill_c = fill.clone();
    let stream = match def.sample_format() {
        SampleFormat::F32 => device
            .build_output_stream(
                &cfg,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    fill_c(data);
                },
                |e| tracing::error!("cpal output stream error: {e:?}"),
                None,
            )
            .map_err(|e| RecordingError::Device(format!("build_output_stream: {e}")))?,
        other => {
            return Err(RecordingError::Device(format!(
                "start_output_stream requires F32 output; got {other:?}"
            )));
        }
    };

    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let join = std::thread::spawn(move || {
        if let Err(e) = stream.play() {
            tracing::error!("cpal output play stream: {e}");
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
