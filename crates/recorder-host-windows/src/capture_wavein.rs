//! WinMM **waveIn** capture (legacy Windows audio).

use std::mem::size_of;
use std::ptr::addr_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use recorder_core::buffer::AudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::{AudioFormat, SampleFormat as RSample};
use recorder_core::traits::{DeviceInfo, StreamHandle};
use windows::Win32::Foundation::{CloseHandle, BOOL, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    waveInAddBuffer, waveInClose, waveInGetDevCapsW, waveInGetNumDevs, waveInOpen,
    waveInPrepareHeader, waveInReset, waveInStart, waveInStop, waveInUnprepareHeader,
    CALLBACK_EVENT, WAVEFORMATEX, WAVEHDR, WAVEINCAPSW, WAVE_FORMAT_1S16, WAVE_FORMAT_2S16,
    WAVE_FORMAT_48S16, WAVE_FORMAT_4S16, WAVE_MAPPER, WHDR_DONE,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

fn pname_to_string(name: &[u16; 32]) -> String {
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

fn default_format_for_caps(caps: &WAVEINCAPSW) -> Option<AudioFormat> {
    let ch = caps.w_channels();
    let f = caps.dw_formats();
    let rate = if f & WAVE_FORMAT_48S16 != 0 {
        48_000
    } else if f & WAVE_FORMAT_4S16 != 0 {
        44_100
    } else if f & WAVE_FORMAT_2S16 != 0 {
        22_050
    } else if f & WAVE_FORMAT_1S16 != 0 {
        11_025
    } else {
        return None;
    };
    Some(AudioFormat::new(rate, ch, RSample::I16))
}

trait WaveInCapsSafe {
    fn w_channels(&self) -> u16;
    fn dw_formats(&self) -> u32;
}

impl WaveInCapsSafe for WAVEINCAPSW {
    fn w_channels(&self) -> u16 {
        unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(self.wChannels)) }
    }
    fn dw_formats(&self) -> u32 {
        unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(self.dwFormats)) }
    }
}

pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    let n = unsafe { waveInGetNumDevs() };
    for i in 0..n {
        let mut caps = WAVEINCAPSW::default();
        let r =
            unsafe { waveInGetDevCapsW(i as usize, &mut caps, size_of::<WAVEINCAPSW>() as u32) };
        if r != 0 {
            continue;
        }
        let name_arr: [u16; 32] =
            unsafe { (addr_of!(caps.szPname) as *const [u16; 32]).read_unaligned() };
        let name = pname_to_string(&name_arr);
        let default_format = default_format_for_caps(&caps).or_else(|| {
            Some(AudioFormat::new(
                44_100,
                caps.w_channels().clamp(1, 2),
                RSample::I16,
            ))
        });
        out.push(DeviceInfo {
            id: format!("wave:{i}"),
            name,
            default_format,
        });
    }
    out.push(DeviceInfo {
        id: "wave:mapper".into(),
        name: "Mapper (system default input)".into(),
        default_format: Some(AudioFormat::new(48_000, 2, RSample::I16)),
    });
    Ok(out)
}

fn parse_device_id(device_id: Option<&str>) -> Result<u32> {
    match device_id {
        None | Some("wave:mapper") => Ok(WAVE_MAPPER),
        Some(s) if let Some(rest) = s.strip_prefix("wave:") => {
            if rest == "mapper" {
                Ok(WAVE_MAPPER)
            } else {
                rest.parse::<u32>()
                    .map_err(|_| RecordingError::Device(format!("bad wave device id: {s}")))
            }
        }
        Some(s) => Err(RecordingError::Device(format!(
            "expected wave:N or wave:mapper, got {s}"
        ))),
    }
}

fn wave_format_pcm(af: AudioFormat) -> Result<WAVEFORMATEX> {
    if af.sample_format != RSample::I16 {
        return Err(RecordingError::Device(
            "WinMM waveIn path requires I16 session format (use device default from list)".into(),
        ));
    }
    let ch = af.channels as u32;
    let align = (ch * 2) as u16;
    Ok(WAVEFORMATEX {
        wFormatTag: 1,
        nChannels: af.channels,
        nSamplesPerSec: af.sample_rate_hz,
        nAvgBytesPerSec: af.sample_rate_hz * ch * 2,
        nBlockAlign: align,
        wBitsPerSample: 16,
        cbSize: 0,
    })
}

pub fn start_stream(
    device_id: Option<&str>,
    format: AudioFormat,
    on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
) -> Result<StreamHandle> {
    let dev = parse_device_id(device_id)?;
    let wfx = wave_format_pcm(format)?;
    let ch = format.channels as usize;
    let frames_per_buffer = (format.sample_rate_hz / 20).max(1) as usize;
    let bytes = frames_per_buffer * ch * 2;
    let counter = Arc::new(AtomicU64::new(0));

    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let join = std::thread::spawn(move || {
        let event = match unsafe { CreateEventW(None, BOOL(0), BOOL(0), None) } {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("CreateEventW: {e}");
                return;
            }
        };
        let mut hwi = unsafe { std::mem::zeroed() };
        let r = unsafe {
            waveInOpen(
                Some(std::ptr::addr_of_mut!(hwi)),
                dev,
                &wfx,
                event.0 as usize,
                0,
                CALLBACK_EVENT,
            )
        };
        if r != 0 {
            tracing::error!("waveInOpen failed: MMSYSERR {r}");
            unsafe {
                let _ = CloseHandle(event);
            }
            return;
        }

        let mut buffer = vec![0u8; bytes];
        let mut hdr = WAVEHDR {
            lpData: windows::core::PSTR(buffer.as_mut_ptr()),
            dwBufferLength: bytes as u32,
            dwBytesRecorded: 0,
            dwUser: 0,
            dwFlags: 0,
            dwLoops: 0,
            lpNext: std::ptr::null_mut(),
            reserved: 0,
        };

        let mut run = || -> Result<()> {
            unsafe {
                if waveInPrepareHeader(
                    hwi,
                    std::ptr::addr_of_mut!(hdr),
                    size_of::<WAVEHDR>() as u32,
                ) != 0
                {
                    return Err(RecordingError::Device("waveInPrepareHeader failed".into()));
                }
                if waveInAddBuffer(
                    hwi,
                    std::ptr::addr_of_mut!(hdr),
                    size_of::<WAVEHDR>() as u32,
                ) != 0
                {
                    let _ = waveInUnprepareHeader(
                        hwi,
                        std::ptr::addr_of_mut!(hdr),
                        size_of::<WAVEHDR>() as u32,
                    );
                    return Err(RecordingError::Device("waveInAddBuffer failed".into()));
                }
                if waveInStart(hwi) != 0 {
                    let _ = waveInUnprepareHeader(
                        hwi,
                        std::ptr::addr_of_mut!(hdr),
                        size_of::<WAVEHDR>() as u32,
                    );
                    return Err(RecordingError::Device("waveInStart failed".into()));
                }
            }

            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                let w = unsafe { WaitForSingleObject(event, 200) };
                if w != WAIT_OBJECT_0 {
                    continue;
                }
                if hdr.dwFlags & WHDR_DONE == 0 {
                    continue;
                }
                let recorded = hdr.dwBytesRecorded as usize;
                let frame_bytes = ch * 2;
                let frames = recorded / frame_bytes;
                if frames > 0 {
                    let idx = counter.fetch_add(frames as u64, Ordering::Relaxed);
                    let take = frames * frame_bytes;
                    let samples: &[i16] = bytemuck::cast_slice(&buffer[..take]);
                    let buf = AudioBuffer::from_interleaved_i16(
                        AudioFormat::new(format.sample_rate_hz, format.channels, RSample::I16),
                        samples,
                        frames,
                        Instant::now(),
                        idx,
                    );
                    on_buffer(buf);
                }
                unsafe {
                    if waveInUnprepareHeader(
                        hwi,
                        std::ptr::addr_of_mut!(hdr),
                        size_of::<WAVEHDR>() as u32,
                    ) != 0
                    {
                        break;
                    }
                    hdr.dwBufferLength = bytes as u32;
                    hdr.dwBytesRecorded = 0;
                    hdr.dwFlags = 0;
                    if waveInPrepareHeader(
                        hwi,
                        std::ptr::addr_of_mut!(hdr),
                        size_of::<WAVEHDR>() as u32,
                    ) != 0
                    {
                        break;
                    }
                    if waveInAddBuffer(
                        hwi,
                        std::ptr::addr_of_mut!(hdr),
                        size_of::<WAVEHDR>() as u32,
                    ) != 0
                    {
                        break;
                    }
                }
            }

            unsafe {
                let _ = waveInStop(hwi);
                let _ = waveInReset(hwi);
                let _ = waveInUnprepareHeader(
                    hwi,
                    std::ptr::addr_of_mut!(hdr),
                    size_of::<WAVEHDR>() as u32,
                );
                let _ = waveInClose(hwi);
                let _ = CloseHandle(event);
            }
            Ok(())
        };

        if let Err(e) = run() {
            tracing::error!("waveIn capture: {e}");
            unsafe {
                let _ = waveInClose(hwi);
                let _ = CloseHandle(event);
            }
        }
    });

    Ok(StreamHandle::new(move || {
        let _ = stop_tx.send(());
        let _ = join.join();
    }))
}
