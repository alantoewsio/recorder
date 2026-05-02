use std::mem::{size_of, ManuallyDrop};
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use recorder_core::buffer::AudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::{AudioFormat, SampleFormat as RSample};
use recorder_core::traits::StreamHandle;
use windows::core::imp::{BLOB, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::core::{implement, ComObjectInner, Interface, HRESULT, PROPVARIANT};
use windows::Win32::Foundation::{
    CloseHandle, E_ILLEGAL_METHOD_CALL, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, IAgileObject, IAgileObject_Impl, COINIT_MULTITHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::VT_BLOB;

#[derive(Default)]
struct ActivationState {
    done: bool,
    client: Option<IAudioClient>,
    error: Option<HRESULT>,
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct ActivationHandler {
    state: Mutex<ActivationState>,
    cv: Condvar,
}

impl ActivationHandler {
    fn new() -> Self {
        Self {
            state: Mutex::new(ActivationState::default()),
            cv: Condvar::new(),
        }
    }
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        activateoperation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let mut state = self.state.lock().expect("activation state lock");
        state.done = true;
        if let Some(operation) = activateoperation {
            let mut activate_result = HRESULT(0);
            let mut activated = None;
            match unsafe { operation.GetActivateResult(&mut activate_result, &mut activated) } {
                Ok(()) if activate_result.is_ok() => match activated {
                    Some(unknown) => match unknown.cast::<IAudioClient>() {
                        Ok(client) => state.client = Some(client),
                        Err(err) => state.error = Some(err.code()),
                    },
                    None => state.error = Some(HRESULT(0x80004003u32 as i32)),
                },
                Ok(()) => state.error = Some(activate_result),
                Err(err) => state.error = Some(err.code()),
            }
        } else {
            state.error = Some(HRESULT(0x80004003u32 as i32));
        }
        self.cv.notify_all();
        Ok(())
    }
}

impl IAgileObject_Impl for ActivationHandler_Impl {}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn create_event() -> Result<Self> {
        let handle = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| RecordingError::Device(format!("CreateEventW failed: {e}")))?;
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn debug_app_output(message: &str) {
    if std::env::var_os("RECORDER_DEBUG_APP_OUTPUT").is_some() {
        eprintln!("[recorder app-output] {message}");
    }
}

fn wide_cstr_to_string(raw: &[u16]) -> String {
    let end = raw.iter().position(|ch| *ch == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

fn try_query_process_image(process_id: u32) -> Option<String> {
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let process = OwnedHandle(process);
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process.raw(),
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    if !ok {
        return None;
    }
    buf.truncate(len as usize);
    Some(String::from_utf16_lossy(&buf))
}

pub fn list_app_processes() -> Result<Vec<(u32, String, Option<String>)>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|e| RecordingError::Device(format!("CreateToolhelp32Snapshot failed: {e}")))?;
    let snapshot = OwnedHandle(snapshot);

    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut out = Vec::new();
    let mut has_entry = unsafe { Process32FirstW(snapshot.raw(), &mut entry) }.is_ok();
    while has_entry {
        let process_id = entry.th32ProcessID;
        if process_id > 0 {
            let exe_name = wide_cstr_to_string(&entry.szExeFile);
            if !exe_name.is_empty() {
                out.push((process_id, exe_name, try_query_process_image(process_id)));
            }
        }
        has_entry = unsafe { Process32NextW(snapshot.raw(), &mut entry) }.is_ok();
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

fn make_blob_propvariant(params: &AUDIOCLIENT_ACTIVATION_PARAMS) -> ManuallyDrop<PROPVARIANT> {
    ManuallyDrop::new(unsafe {
        PROPVARIANT::from_raw(windows::core::imp::PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: PROPVARIANT_0_0 {
                    vt: VT_BLOB.0,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: (params as *const AUDIOCLIENT_ACTIVATION_PARAMS)
                                .cast_mut()
                                .cast(),
                        },
                    },
                },
            },
        })
    })
}

fn activate_process_loopback_client(process_id: u32) -> Result<IAudioClient> {
    debug_app_output(&format!("activate:start pid={process_id}"));
    let handler = ActivationHandler::new().into_object();
    let completion_handler: IActivateAudioInterfaceCompletionHandler = handler.to_interface();
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: process_id,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let propvariant = make_blob_propvariant(&params);
    let async_operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*propvariant as *const PROPVARIANT),
            &completion_handler,
        )
        .map_err(|e| RecordingError::Device(format!("ActivateAudioInterfaceAsync failed: {e}")))?
    };
    debug_app_output("activate:async-operation-created");
    let activation_deadline = Instant::now() + std::time::Duration::from_secs(5);
    let client = loop {
        let mut activate_result = HRESULT(0);
        let mut activated = None;
        match unsafe { async_operation.GetActivateResult(&mut activate_result, &mut activated) } {
            Ok(()) if activate_result.is_ok() => match activated {
                Some(unknown) => match unknown.cast::<IAudioClient>() {
                    Ok(client) => {
                        debug_app_output("activate:poll-complete");
                        break client;
                    }
                    Err(err) => {
                        return Err(RecordingError::Device(format!(
                            "ActivateAudioInterfaceAsync returned a non-IAudioClient: {err}"
                        )));
                    }
                },
                None => {
                    return Err(RecordingError::Device(
                        "ActivateAudioInterfaceAsync completed without an audio client".into(),
                    ));
                }
            },
            Ok(()) => {
                return Err(RecordingError::Device(format!(
                    "ActivateAudioInterfaceAsync failed: {activate_result:?}"
                )));
            }
            Err(err) if err.code() == E_ILLEGAL_METHOD_CALL => {
                if Instant::now() >= activation_deadline {
                    return Err(RecordingError::Device(
                        "ActivateAudioInterfaceAsync timed out waiting for process-loopback activation".into(),
                    ));
                }
                thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => {
                return Err(RecordingError::Device(format!(
                    "ActivateAudioInterfaceAsync result polling failed: {err}"
                )));
            }
        }
    };
    drop(async_operation);
    drop(completion_handler);
    drop(handler);
    debug_app_output("activate:done");
    Ok(client)
}

fn make_wave_format(format: AudioFormat) -> Result<WAVEFORMATEX> {
    let bits_per_sample = match format.sample_format {
        RSample::F32 => 32,
        RSample::I16 => 16,
    };
    let block_align = format
        .channels
        .checked_mul((bits_per_sample / 8) as u16)
        .ok_or_else(|| {
            RecordingError::Device("invalid channel count for app-output format".into())
        })?;
    Ok(WAVEFORMATEX {
        wFormatTag: match format.sample_format {
            RSample::F32 => WAVE_FORMAT_IEEE_FLOAT as u16,
            RSample::I16 => WAVE_FORMAT_PCM as u16,
        },
        nChannels: format.channels,
        nSamplesPerSec: format.sample_rate_hz,
        nAvgBytesPerSec: format.sample_rate_hz * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: bits_per_sample,
        cbSize: 0,
    })
}

pub fn start_process_loopback_stream(
    process_id: u32,
    format: AudioFormat,
    on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
) -> Result<StreamHandle> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (started_tx, started_rx) = mpsc::channel::<Result<()>>();

    let join = std::thread::spawn(move || {
        let run_result = run_capture_thread(process_id, format, on_buffer, &started_tx, &stop_rx);
        if let Err(err) = run_result {
            let _ = started_tx.send(Err(err));
        }
    });

    match started_rx.recv() {
        Ok(Ok(())) => Ok(StreamHandle::new(move || {
            let _ = stop_tx.send(());
            let _ = join.join();
        })),
        Ok(Err(err)) => {
            let _ = stop_tx.send(());
            let _ = join.join();
            Err(err)
        }
        Err(_) => {
            let _ = join.join();
            Err(RecordingError::Device(
                "app-output capture thread terminated before startup completed".into(),
            ))
        }
    }
}

fn run_capture_thread(
    process_id: u32,
    format: AudioFormat,
    on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
    started_tx: &mpsc::Sender<Result<()>>,
    stop_rx: &mpsc::Receiver<()>,
) -> Result<()> {
    debug_app_output(&format!("capture-thread:start pid={process_id}"));
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| RecordingError::Device(format!("CoInitializeEx failed: {e}")))?;
    }
    let run_result = (|| {
        let client = activate_process_loopback_client(process_id)?;
        debug_app_output("capture-thread:activated");
        let wave_format = make_wave_format(format)?;
        unsafe {
            debug_app_output("capture-thread:initialize");
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    0,
                    0,
                    &wave_format,
                    None,
                )
                .map_err(|e| {
                    RecordingError::Device(format!("IAudioClient::Initialize failed: {e}"))
                })?;
        }
        debug_app_output("capture-thread:get-service");
        let capture_client: IAudioCaptureClient = unsafe {
            client.GetService().map_err(|e| {
                RecordingError::Device(format!("IAudioClient::GetService failed: {e}"))
            })?
        };
        let sample_ready = OwnedHandle::create_event()?;
        unsafe {
            debug_app_output("capture-thread:set-event");
            client.SetEventHandle(sample_ready.raw()).map_err(|e| {
                RecordingError::Device(format!("IAudioClient::SetEventHandle failed: {e}"))
            })?;
            debug_app_output("capture-thread:start-client");
            client
                .Start()
                .map_err(|e| RecordingError::Device(format!("IAudioClient::Start failed: {e}")))?;
        }
        debug_app_output("capture-thread:running");
        let _ = started_tx.send(Ok(()));
        let frame_counter = AtomicU64::new(0);
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            let wait = unsafe { WaitForSingleObject(sample_ready.raw(), 100) };
            if wait == WAIT_TIMEOUT {
                continue;
            }
            if wait != WAIT_OBJECT_0 {
                return Err(RecordingError::Device(format!(
                    "WaitForSingleObject failed for app-output capture: {wait:?}"
                )));
            }
            loop {
                let frames_available = unsafe {
                    capture_client.GetNextPacketSize().map_err(|e| {
                        RecordingError::Device(format!("GetNextPacketSize failed: {e}"))
                    })?
                };
                if frames_available == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut num_frames = 0u32;
                let mut flags = 0u32;
                unsafe {
                    capture_client
                        .GetBuffer(&mut data, &mut num_frames, &mut flags, None, None)
                        .map_err(|e| RecordingError::Device(format!("GetBuffer failed: {e}")))?;
                }
                let frames = num_frames as usize;
                let idx = frame_counter.fetch_add(frames as u64, Ordering::Relaxed);
                let sample_count = frames * format.channels as usize;
                let captured_at = Instant::now();
                let buffer = match format.sample_format {
                    RSample::F32 => {
                        let samples = if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                            vec![0.0; sample_count]
                        } else {
                            unsafe { slice::from_raw_parts(data.cast::<f32>(), sample_count) }
                                .to_vec()
                        };
                        AudioBuffer::new(
                            format,
                            Arc::from(samples.into_boxed_slice()),
                            frames,
                            captured_at,
                            idx,
                        )
                    }
                    RSample::I16 => {
                        if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                            let zeros = vec![0i16; sample_count];
                            AudioBuffer::from_interleaved_i16(
                                format,
                                &zeros,
                                frames,
                                captured_at,
                                idx,
                            )
                        } else {
                            let samples =
                                unsafe { slice::from_raw_parts(data.cast::<i16>(), sample_count) };
                            AudioBuffer::from_interleaved_i16(
                                format,
                                samples,
                                frames,
                                captured_at,
                                idx,
                            )
                        }
                    }
                };
                unsafe {
                    capture_client.ReleaseBuffer(num_frames).map_err(|e| {
                        RecordingError::Device(format!("ReleaseBuffer failed: {e}"))
                    })?;
                }
                on_buffer(buffer);
            }
        }
        unsafe {
            let _ = client.Stop();
        }
        Ok(())
    })();
    unsafe {
        CoUninitialize();
    }
    run_result
}
