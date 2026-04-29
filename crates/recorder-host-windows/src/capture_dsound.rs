//! DirectSound **capture** (`dsound.dll`).

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use recorder_core::buffer::AudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::{AudioFormat, SampleFormat as RSample};
use recorder_core::traits::{DeviceInfo, StreamHandle};
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::DirectSound::{
    DirectSoundCaptureCreate8, DirectSoundCaptureEnumerateW, IDirectSoundCapture,
    IDirectSoundCaptureBuffer, IDirectSoundNotify, DSBPOSITIONNOTIFY, DSCBSTART_LOOPING,
    DSCBUFFERDESC, LPDSENUMCALLBACKW,
};
use windows::Win32::Media::Audio::WAVEFORMATEX;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForMultipleObjects};

fn win_dev_err(e: windows::core::Error) -> RecordingError {
    RecordingError::Device(e.to_string())
}

struct EnumCtx {
    devices: Vec<DeviceInfo>,
}

fn format_guid(g: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7],
    )
}

unsafe extern "system" fn ds_enum_capture_cb(
    guid: *mut GUID,
    _driver: PCWSTR,
    desc: PCWSTR,
    ctx: *mut c_void,
) -> BOOL {
    if ctx.is_null() {
        return BOOL::from(true);
    }
    let ctx = &mut *(ctx as *mut EnumCtx);
    let id = if guid.is_null() {
        "ds:default".to_string()
    } else {
        format!("ds:{}", format_guid(&*guid))
    };
    let name = pcw_to_string(desc);
    if name.is_empty() {
        return BOOL::from(true);
    }
    ctx.devices.push(DeviceInfo {
        id,
        name,
        default_format: Some(AudioFormat::new(44_100, 2, RSample::I16)),
    });
    BOOL::from(true)
}

fn pcw_to_string(p: PCWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut v = Vec::new();
    let mut i = 0usize;
    loop {
        let c = unsafe { p.0.add(i).read() };
        if c == 0 {
            break;
        }
        v.push(c);
        i += 1;
        if i > 4096 {
            break;
        }
    }
    String::from_utf16_lossy(&v)
}

pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let mut ctx = EnumCtx {
        devices: Vec::new(),
    };
    let cb: LPDSENUMCALLBACKW = Some(ds_enum_capture_cb);
    unsafe {
        DirectSoundCaptureEnumerateW(cb, Some(std::ptr::addr_of_mut!(ctx).cast::<c_void>()))
            .map_err(win_dev_err)?;
    }
    if ctx.devices.is_empty() {
        ctx.devices.push(DeviceInfo {
            id: "ds:default".into(),
            name: "DirectSound capture (default)".into(),
            default_format: Some(AudioFormat::new(44_100, 2, RSample::I16)),
        });
    }
    Ok(ctx.devices)
}

fn parse_guid(device_id: Option<&str>) -> Result<Option<GUID>> {
    match device_id {
        None | Some("ds:default") => Ok(None),
        Some(s) if let Some(rest) = s.strip_prefix("ds:") => {
            if rest == "default" {
                Ok(None)
            } else if rest.len() == 36 {
                Ok(Some(GUID::from(rest)))
            } else {
                Err(RecordingError::Device(format!(
                    "expected 36-char GUID after ds:, got {rest}"
                )))
            }
        }
        Some(s) => Err(RecordingError::Device(format!(
            "expected ds:default or ds:{{uuid}}, got {s}"
        ))),
    }
}

struct CoInit(bool);

impl Drop for CoInit {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

pub fn start_stream(
    device_id: Option<&str>,
    format: AudioFormat,
    on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
) -> Result<StreamHandle> {
    if format.sample_format != RSample::I16 {
        return Err(RecordingError::Device(
            "DirectSound capture requires I16 session format (see device defaults)".into(),
        ));
    }
    let guid = parse_guid(device_id)?;
    let ch = format.channels.max(1);
    if ch > 2 {
        return Err(RecordingError::Device(
            "DirectSound demo supports up to 2 channels".into(),
        ));
    }
    let wfx = WAVEFORMATEX {
        wFormatTag: 1,
        nChannels: ch,
        nSamplesPerSec: format.sample_rate_hz,
        nAvgBytesPerSec: format.sample_rate_hz * ch as u32 * 2,
        nBlockAlign: (ch as u16) * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };

    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let join = std::thread::spawn(move || {
        let run = || -> Result<()> {
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if !hr.is_ok() {
                return Err(RecordingError::Device(format!(
                    "CoInitializeEx failed ({hr:?})"
                )));
            }
            let _co = CoInit(true);

            let mut dsc: Option<IDirectSoundCapture> = None;
            unsafe {
                DirectSoundCaptureCreate8(guid.as_ref().map(|g| g as *const GUID), &mut dsc, None)
                    .map_err(win_dev_err)?;
            }
            let dsc = dsc.ok_or_else(|| {
                RecordingError::Device("DirectSoundCaptureCreate8 returned null".into())
            })?;

            let buffer_bytes = (format.sample_rate_hz / 10).max(1) * ch as u32 * 2;
            let half = buffer_bytes / 2;
            if half == 0 || buffer_bytes < 4 {
                return Err(RecordingError::Device("buffer too small".into()));
            }

            let mut wfx_box = Box::new(wfx);
            let desc = DSCBUFFERDESC {
                dwSize: size_of::<DSCBUFFERDESC>() as u32,
                dwFlags: 0,
                dwBufferBytes: buffer_bytes,
                dwReserved: 0,
                lpwfxFormat: wfx_box.as_mut() as *mut WAVEFORMATEX,
                dwFXCount: 0,
                lpDSCFXDesc: std::ptr::null_mut(),
            };

            let mut cap_buf: Option<IDirectSoundCaptureBuffer> = None;
            unsafe {
                dsc.CreateCaptureBuffer(&desc, &mut cap_buf, None)
                    .map_err(win_dev_err)?;
            }

            let cap_buf = cap_buf.ok_or_else(|| {
                RecordingError::Device("CreateCaptureBuffer returned null".into())
            })?;

            let notify: IDirectSoundNotify = cap_buf.cast().map_err(win_dev_err)?;

            let ev0 = unsafe { CreateEventW(None, BOOL(0), BOOL(0), None) }.map_err(win_dev_err)?;
            let ev1 = unsafe { CreateEventW(None, BOOL(0), BOOL(0), None) }.map_err(win_dev_err)?;

            let notifies = [
                DSBPOSITIONNOTIFY {
                    dwOffset: half.saturating_sub(1),
                    hEventNotify: ev0,
                },
                DSBPOSITIONNOTIFY {
                    dwOffset: buffer_bytes.saturating_sub(1),
                    hEventNotify: ev1,
                },
            ];
            unsafe { notify.SetNotificationPositions(&notifies) }.map_err(win_dev_err)?;

            unsafe { cap_buf.Start(DSCBSTART_LOOPING) }.map_err(win_dev_err)?;

            let counter = AtomicU64::new(0);
            let frame_bytes = (ch as usize) * 2;

            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                let handles = [ev0, ev1];
                let w: WAIT_EVENT = unsafe { WaitForMultipleObjects(&handles, BOOL(0), 250) };
                if w == WAIT_TIMEOUT {
                    continue;
                }
                if w.0 >= WAIT_OBJECT_0.0 + 2 {
                    continue;
                }
                let idx = w.0 - WAIT_OBJECT_0.0;
                let offset = if idx == 0 { 0 } else { half };
                let len = if idx == 0 { half } else { buffer_bytes - half };

                let mut ptr1 = std::ptr::null_mut();
                let mut bytes1 = 0u32;
                unsafe {
                    cap_buf
                        .Lock(offset, len, &mut ptr1, &mut bytes1, None, None, 0)
                        .map_err(win_dev_err)?;
                }
                if bytes1 > 0 && !ptr1.is_null() {
                    let b = bytes1 as usize;
                    let frames = b / frame_bytes;
                    if frames > 0 {
                        let slice = unsafe { std::slice::from_raw_parts(ptr1 as *const u8, b) };
                        let samples: &[i16] = bytemuck::cast_slice(slice);
                        let fi = counter.fetch_add(frames as u64, Ordering::Relaxed);
                        let buf = AudioBuffer::from_interleaved_i16(
                            AudioFormat::new(format.sample_rate_hz, ch, RSample::I16),
                            samples,
                            frames,
                            Instant::now(),
                            fi,
                        );
                        on_buffer(buf);
                    }
                    unsafe {
                        cap_buf.Unlock(ptr1, bytes1, None, 0).map_err(win_dev_err)?;
                    }
                } else {
                    unsafe {
                        cap_buf.Unlock(ptr1, bytes1, None, 0).map_err(win_dev_err)?;
                    }
                }

                if idx == 0 {
                    let _ = unsafe { ResetEvent(ev0) };
                } else {
                    let _ = unsafe { ResetEvent(ev1) };
                }
            }

            unsafe {
                let _ = cap_buf.Stop();
                let _ = CloseHandle(ev0);
                let _ = CloseHandle(ev1);
            }
            Ok(())
        };

        if let Err(e) = run() {
            tracing::error!("DirectSound capture: {e}");
        }
    });

    Ok(StreamHandle::new(move || {
        let _ = stop_tx.send(());
        let _ = join.join();
    }))
}
