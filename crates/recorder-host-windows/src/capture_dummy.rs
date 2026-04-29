use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use recorder_core::buffer::AudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::AudioFormat;
use recorder_core::traits::{DeviceInfo, StreamHandle};

pub fn list_devices() -> Vec<DeviceInfo> {
    vec![DeviceInfo {
        id: "dummy:silence".into(),
        name: "Silence (no hardware)".into(),
        default_format: Some(AudioFormat::new(
            48_000,
            2,
            recorder_core::format::SampleFormat::F32,
        )),
    }]
}

pub fn start_stream(
    device_id: Option<&str>,
    format: AudioFormat,
    on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
) -> Result<StreamHandle> {
    if format.sample_format != recorder_core::format::SampleFormat::F32 {
        return Err(RecordingError::Device(
            "dummy backend only supports F32 session format".into(),
        ));
    }
    let id_ok = device_id.is_none() || device_id == Some("dummy:silence");
    if !id_ok {
        return Err(RecordingError::Device("unknown dummy device id".into()));
    }

    let frames_per_tick = (format.sample_rate_hz / 50).max(1) as usize;
    let period = Duration::from_millis(20);
    let counter = Arc::new(AtomicU64::new(0));
    let counter_thread = counter.clone();
    let ch = format.channels as usize;
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let join = std::thread::spawn(move || {
        while stop_rx.try_recv().is_err() {
            let start = Instant::now();
            let idx = counter_thread.fetch_add(frames_per_tick as u64, Ordering::Relaxed);
            let n = frames_per_tick * ch;
            let buf = AudioBuffer::new(
                format,
                vec![0.0f32; n].into(),
                frames_per_tick,
                Instant::now(),
                idx,
            );
            on_buffer(buf);
            let elapsed = start.elapsed();
            if elapsed < period {
                std::thread::sleep(period - elapsed);
            }
        }
    });

    Ok(StreamHandle::new(move || {
        let _ = stop_tx.send(());
        let _ = join.join();
    }))
}
