//! C ABI for embedding the recorder engine in non-Rust applications.
//!
//! This crate intentionally exposes opaque handles and C-compatible structs only. Rust
//! types such as `String`, `Vec`, `Box<dyn Trait>`, and `Result<T, E>` must not cross the
//! dynamic-library boundary.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::ptr;

use recorder_core::{
    AppCaptureBackend, AudioFormat, AudioHost, AudioSink, CaptureSourceKind, CaptureStream,
    RecordingError, RecordingSession, SampleFormat, SessionConfig, StreamOptions,
};

pub const RECORDER_SDK_OK: c_int = 0;
pub const RECORDER_SDK_NULL_POINTER: c_int = 1;
pub const RECORDER_SDK_INVALID_UTF8: c_int = 2;
pub const RECORDER_SDK_INVALID_ARGUMENT: c_int = 3;
pub const RECORDER_SDK_BUFFER_TOO_SMALL: c_int = 4;
pub const RECORDER_SDK_ERROR: c_int = 100;

const SAMPLE_FORMAT_DEFAULT: i32 = 0;
const SAMPLE_FORMAT_F32: i32 = 1;
const SAMPLE_FORMAT_I16: i32 = 2;

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").expect("empty CString"));
}

/// Opaque handle returned by `recorder_sdk_start_recording`.
///
/// Holds one capture stream for the microphone input plus, optionally, a second one for a
/// loopback (speaker output) source.
pub struct RecorderCapture {
    captures: Vec<CaptureStream>,
}

/// Start-recording configuration for the C ABI.
///
/// Null string pointers are treated as "not set". **Callers must zero-initialize the
/// struct** (e.g. `RecorderStartConfig cfg = {0};` in C) so trailing fields added in
/// future versions remain backward-compatible.
///
/// **Mixer graph:** this struct describes at most one microphone stream and one optional
/// loopback stream with separate file paths. Rust hosts can use the richer
/// `recorder_core::graph` / `BusMixer` APIs for multi-bus routing; a future C entry point
/// may expose a graph once that layout is stabilized (see `ARCHITECTURE.md`).
#[repr(C)]
pub struct RecorderStartConfig {
    /// Windows only: "wasapi", "asio", "directsound", "waveout", or "dummy".
    /// Ignored on macOS/Linux.
    pub audio_system: *const c_char,
    /// Optional device id from `recorder_sdk_list_devices_json`. Null uses default device.
    pub device_id: *const c_char,
    /// Optional pre-processed output path.
    pub raw_output_path: *const c_char,
    /// Optional post-processed output path. With no processors this is equivalent to raw.
    pub processed_output_path: *const c_char,
    /// "wav", "flac", or "mp3". Null/empty infers from the first output path, then defaults to "wav".
    pub output_format: *const c_char,
    /// 0 means use selected device default.
    pub sample_rate_hz: u32,
    /// 0 means use selected device default.
    pub channels: u16,
    /// 0 = use selected device default, 1 = f32, 2 = i16.
    pub sample_format: i32,
    /// Optional speaker-output (loopback) source id from `recorder_sdk_list_capture_sources_json`.
    /// Null disables loopback capture; non-null requires `loopback_output_path` too.
    pub loopback_source_id: *const c_char,
    /// Where to write the loopback recording. Required when `loopback_source_id` is set.
    /// Loopback streams use the source's native format and the same `output_format`
    /// (wav/flac/mp3) as the mic side.
    pub loopback_output_path: *const c_char,
}

fn set_last_error(message: impl AsRef<str>) {
    let sanitized = message.as_ref().replace('\0', "\\0");
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(sanitized).unwrap_or_else(|_| {
            CString::new("error message contained an interior nul").expect("static CString")
        });
    });
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new("").expect("empty CString");
    });
}

fn opt_cstr(ptr: *const c_char) -> Result<Option<String>, c_int> {
    if ptr.is_null() {
        return Ok(None);
    }
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|e| {
            set_last_error(format!("invalid UTF-8 string: {e}"));
            RECORDER_SDK_INVALID_UTF8
        })?
        .trim()
        .to_string();
    if s.is_empty() {
        Ok(None)
    } else {
        Ok(Some(s))
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn sample_format_label(sample_format: SampleFormat) -> &'static str {
    match sample_format {
        SampleFormat::F32 => "f32",
        SampleFormat::I16 => "i16",
    }
}

fn app_capture_backend_label(backend: AppCaptureBackend) -> &'static str {
    match backend {
        AppCaptureBackend::WindowsProcessLoopback => "windows-process-loopback",
        AppCaptureBackend::MacosScreenCaptureKit => "macos-screencapturekit",
        AppCaptureBackend::LinuxPipewireRoute => "linux-pipewire-route",
        AppCaptureBackend::Unsupported => "unsupported",
    }
}

#[cfg(windows)]
fn make_host(
    audio_system: Option<&str>,
) -> recorder_core::Result<Box<dyn AudioHost + Send + Sync>> {
    use recorder_host_windows::{WindowsAudioSystem, WindowsHost};

    let system = match audio_system
        .unwrap_or("wasapi")
        .to_ascii_lowercase()
        .as_str()
    {
        "wasapi" => WindowsAudioSystem::Wasapi,
        "asio" => WindowsAudioSystem::Asio,
        "directsound" | "dsound" => WindowsAudioSystem::DirectSound,
        "waveout" | "wavein" | "winmm" => WindowsAudioSystem::WaveOut,
        "dummy" => WindowsAudioSystem::Dummy,
        other => {
            return Err(RecordingError::Config(format!(
                "unknown Windows audio system: {other}"
            )));
        }
    };
    Ok(Box::new(WindowsHost::new(system)?))
}

#[cfg(target_os = "macos")]
fn make_host(
    _audio_system: Option<&str>,
) -> recorder_core::Result<Box<dyn AudioHost + Send + Sync>> {
    Ok(Box::new(recorder_host_macos::MacosHost::new()?))
}

#[cfg(target_os = "linux")]
fn make_host(
    _audio_system: Option<&str>,
) -> recorder_core::Result<Box<dyn AudioHost + Send + Sync>> {
    Ok(Box::new(recorder_host_linux::LinuxHost::new()?))
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn make_host(
    _audio_system: Option<&str>,
) -> recorder_core::Result<Box<dyn AudioHost + Send + Sync>> {
    Err(RecordingError::Config(
        "recorder-sdk is only supported on Windows, macOS, and Linux".into(),
    ))
}

fn devices_json(host: &dyn AudioHost) -> recorder_core::Result<String> {
    let devices = host.list_input_devices()?;
    let mut out = String::from("[");
    for (i, device) in devices.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"id\":\"");
        out.push_str(&json_escape(&device.id));
        out.push_str("\",\"name\":\"");
        out.push_str(&json_escape(&device.name));
        out.push_str("\",\"default_format\":");
        if let Some(format) = device.default_format {
            out.push_str(&format!(
                "{{\"sample_rate_hz\":{},\"channels\":{},\"sample_format\":\"{}\"}}",
                format.sample_rate_hz,
                format.channels,
                sample_format_label(format.sample_format)
            ));
        } else {
            out.push_str("null");
        }
        out.push('}');
    }
    out.push(']');
    Ok(out)
}

fn capture_sources_json(host: &dyn AudioHost) -> recorder_core::Result<String> {
    let sources = host.list_capture_sources()?;
    let mut out = String::from("[");
    for (i, source) in sources.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let kind = match source.kind {
            CaptureSourceKind::Input => "input",
            CaptureSourceKind::Loopback => "loopback",
            CaptureSourceKind::AppOutput => "app-output",
        };
        out.push_str("{\"id\":\"");
        out.push_str(&json_escape(&source.id));
        out.push_str("\",\"name\":\"");
        out.push_str(&json_escape(&source.name));
        out.push_str("\",\"kind\":\"");
        out.push_str(kind);
        out.push_str("\",\"default_format\":");
        if let Some(format) = source.default_format {
            out.push_str(&format!(
                "{{\"sample_rate_hz\":{},\"channels\":{},\"sample_format\":\"{}\"}}",
                format.sample_rate_hz,
                format.channels,
                sample_format_label(format.sample_format)
            ));
        } else {
            out.push_str("null");
        }
        out.push_str(",\"app\":");
        if let Some(app) = source.app.as_ref() {
            out.push_str("{\"backend\":\"");
            out.push_str(app_capture_backend_label(app.backend));
            out.push_str("\",\"app_name\":\"");
            out.push_str(&json_escape(&app.app_name));
            out.push_str("\",\"app_id\":");
            if let Some(app_id) = app.app_id.as_ref() {
                out.push('"');
                out.push_str(&json_escape(app_id));
                out.push('"');
            } else {
                out.push_str("null");
            }
            out.push_str(",\"process_id\":");
            if let Some(process_id) = app.process_id {
                out.push_str(&process_id.to_string());
            } else {
                out.push_str("null");
            }
            out.push_str(",\"instance_id\":");
            if let Some(instance_id) = app.instance_id.as_ref() {
                out.push('"');
                out.push_str(&json_escape(instance_id));
                out.push('"');
            } else {
                out.push_str("null");
            }
            out.push_str(",\"supports_multi_select\":");
            out.push_str(if app.supports_multi_select {
                "true"
            } else {
                "false"
            });
            out.push_str(",\"requires_system_permission\":");
            out.push_str(if app.requires_system_permission {
                "true"
            } else {
                "false"
            });
            out.push('}');
        } else {
            out.push_str("null");
        }
        out.push('}');
    }
    out.push(']');
    Ok(out)
}

fn infer_output_format(
    explicit: Option<String>,
    raw: Option<&str>,
    processed: Option<&str>,
) -> String {
    if let Some(fmt) = explicit {
        return fmt.to_ascii_lowercase();
    }
    let path = raw.or(processed).unwrap_or("");
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "wav".to_string())
}

fn build_sink(
    path: &str,
    output_format: &str,
    format: AudioFormat,
) -> recorder_core::Result<Box<dyn AudioSink>> {
    match output_format {
        "wav" => {
            #[cfg(feature = "wav")]
            {
                Ok(Box::new(recorder_core::WavSink::create(path, format)?))
            }
            #[cfg(not(feature = "wav"))]
            {
                let _ = (path, format);
                Err(RecordingError::Config(
                    "recorder-sdk was built without the wav feature".into(),
                ))
            }
        }
        "flac" => {
            #[cfg(feature = "flac")]
            {
                Ok(Box::new(recorder_core::FlacSink::create(path, format)?))
            }
            #[cfg(not(feature = "flac"))]
            {
                let _ = (path, format);
                Err(RecordingError::Config(
                    "recorder-sdk was built without the flac feature".into(),
                ))
            }
        }
        "mp3" => {
            #[cfg(feature = "mp3")]
            {
                Ok(Box::new(recorder_core::Mp3Sink::create(path, format)?))
            }
            #[cfg(not(feature = "mp3"))]
            {
                let _ = (path, format);
                Err(RecordingError::Config(
                    "recorder-sdk was built without the mp3 feature".into(),
                ))
            }
        }
        other => Err(RecordingError::Config(format!(
            "unsupported output format: {other}; expected wav, flac, or mp3"
        ))),
    }
}

fn requested_format(
    config: &RecorderStartConfig,
    default: AudioFormat,
) -> recorder_core::Result<AudioFormat> {
    let sample_format = match config.sample_format {
        SAMPLE_FORMAT_DEFAULT => default.sample_format,
        SAMPLE_FORMAT_F32 => SampleFormat::F32,
        SAMPLE_FORMAT_I16 => SampleFormat::I16,
        other => {
            return Err(RecordingError::Config(format!(
                "invalid sample_format {other}; expected 0, 1, or 2"
            )));
        }
    };
    Ok(AudioFormat::new(
        if config.sample_rate_hz == 0 {
            default.sample_rate_hz
        } else {
            config.sample_rate_hz
        },
        if config.channels == 0 {
            default.channels
        } else {
            config.channels
        },
        sample_format,
    ))
}

fn start_recording_inner(config: &RecorderStartConfig) -> Result<*mut RecorderCapture, c_int> {
    let audio_system = opt_cstr(config.audio_system)?;
    let device_id = opt_cstr(config.device_id)?;
    let raw_path = opt_cstr(config.raw_output_path)?;
    let processed_path = opt_cstr(config.processed_output_path)?;
    let loopback_source_id = opt_cstr(config.loopback_source_id)?;
    let loopback_output_path = opt_cstr(config.loopback_output_path)?;
    let output_format = infer_output_format(
        opt_cstr(config.output_format)?,
        raw_path.as_deref(),
        processed_path.as_deref(),
    );

    if raw_path.is_none() && processed_path.is_none() {
        set_last_error("at least one output path is required");
        return Err(RECORDER_SDK_INVALID_ARGUMENT);
    }

    if loopback_source_id.is_some() && loopback_output_path.is_none() {
        set_last_error("loopback_output_path is required when loopback_source_id is set");
        return Err(RECORDER_SDK_INVALID_ARGUMENT);
    }
    if loopback_output_path.is_some() && loopback_source_id.is_none() {
        set_last_error("loopback_source_id is required when loopback_output_path is set");
        return Err(RECORDER_SDK_INVALID_ARGUMENT);
    }

    let host = make_host(audio_system.as_deref()).map_err(|e| {
        set_last_error(e.to_string());
        RECORDER_SDK_ERROR
    })?;
    let devices = host.list_input_devices().map_err(|e| {
        set_last_error(e.to_string());
        RECORDER_SDK_ERROR
    })?;

    let selected = match device_id.as_deref() {
        Some(id) => devices
            .iter()
            .find(|d| d.id == id)
            .or_else(|| devices.first()),
        None => devices.first(),
    }
    .ok_or_else(|| {
        set_last_error("no input devices found");
        RECORDER_SDK_ERROR
    })?;
    if device_id.is_some() && selected.id != device_id.as_deref().unwrap_or_default() {
        set_last_error("requested device id was not found");
        return Err(RECORDER_SDK_INVALID_ARGUMENT);
    }

    let default_format =
        selected
            .default_format
            .unwrap_or(AudioFormat::new(48_000, 2, SampleFormat::F32));
    let format = requested_format(config, default_format).map_err(|e| {
        set_last_error(e.to_string());
        RECORDER_SDK_INVALID_ARGUMENT
    })?;

    let raw_sink = raw_path
        .as_deref()
        .map(|p| build_sink(p, &output_format, format))
        .transpose()
        .map_err(|e| {
            set_last_error(e.to_string());
            RECORDER_SDK_ERROR
        })?;
    let processed_sink = processed_path
        .as_deref()
        .map(|p| build_sink(p, &output_format, format))
        .transpose()
        .map_err(|e| {
            set_last_error(e.to_string());
            RECORDER_SDK_ERROR
        })?;

    let session = RecordingSession::new(SessionConfig::default());
    let mic_capture = session
        .add_capture_stream(
            host.as_ref(),
            device_id.as_deref(),
            CaptureSourceKind::Input,
            format,
            StreamOptions {
                raw_sink,
                processed_sink,
                processors: Vec::new(),
                analyzers: Vec::new(),
                event_tx: None,
                pause_gate: None,
            },
        )
        .map_err(|e| {
            set_last_error(e.to_string());
            RECORDER_SDK_ERROR
        })?;

    let mut captures = vec![mic_capture];

    if let (Some(src_id), Some(out_path)) = (
        loopback_source_id.as_deref(),
        loopback_output_path.as_deref(),
    ) {
        let sources = host.list_capture_sources().map_err(|e| {
            set_last_error(e.to_string());
            RECORDER_SDK_ERROR
        })?;
        let loopback_source = sources
            .iter()
            .find(|s| s.id == src_id && s.kind == CaptureSourceKind::Loopback)
            .ok_or_else(|| {
                set_last_error(format!("loopback source not found: {src_id}"));
                for c in captures.drain(..) {
                    c.stop();
                }
                RECORDER_SDK_INVALID_ARGUMENT
            })?;
        let loopback_format = loopback_source.default_format.unwrap_or(AudioFormat::new(
            48_000,
            2,
            SampleFormat::F32,
        ));
        let loopback_sink = match build_sink(out_path, &output_format, loopback_format) {
            Ok(sink) => sink,
            Err(e) => {
                set_last_error(e.to_string());
                for c in captures.drain(..) {
                    c.stop();
                }
                return Err(RECORDER_SDK_ERROR);
            }
        };
        match session.add_capture_stream(
            host.as_ref(),
            Some(src_id),
            CaptureSourceKind::Loopback,
            loopback_format,
            StreamOptions {
                raw_sink: None,
                processed_sink: Some(loopback_sink),
                processors: Vec::new(),
                analyzers: Vec::new(),
                event_tx: None,
                pause_gate: None,
            },
        ) {
            Ok(capture) => captures.push(capture),
            Err(e) => {
                set_last_error(e.to_string());
                for c in captures.drain(..) {
                    c.stop();
                }
                return Err(RECORDER_SDK_ERROR);
            }
        }
    }

    Ok(Box::into_raw(Box::new(RecorderCapture { captures })))
}

/// Returns a static version string.
#[no_mangle]
pub extern "C" fn recorder_sdk_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Returns the last error message for the calling thread. Pointer remains valid until the
/// next SDK call on the same thread.
#[no_mangle]
pub extern "C" fn recorder_sdk_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Enumerates input devices as JSON.
///
/// `required_len_out`, when non-null, receives the required byte count including the NUL
/// terminator. If `out_json` is null or `out_json_len` is too small, returns
/// `RECORDER_SDK_BUFFER_TOO_SMALL`.
#[no_mangle]
pub extern "C" fn recorder_sdk_list_devices_json(
    audio_system: *const c_char,
    out_json: *mut c_char,
    out_json_len: usize,
    required_len_out: *mut usize,
) -> c_int {
    clear_last_error();
    let audio_system = match opt_cstr(audio_system) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let host = match make_host(audio_system.as_deref()) {
        Ok(host) => host,
        Err(e) => {
            set_last_error(e.to_string());
            return RECORDER_SDK_ERROR;
        }
    };
    let json = match devices_json(host.as_ref()) {
        Ok(json) => json,
        Err(e) => {
            set_last_error(e.to_string());
            return RECORDER_SDK_ERROR;
        }
    };
    write_json_to_caller_buffer(json, out_json, out_json_len, required_len_out)
}

/// Enumerates every capture source (microphone inputs, speaker-output loopback sources,
/// and optional app-output sources) as JSON. Each entry carries a `"kind"` field and
/// optional `"app"` metadata for app-bound sources.
///
/// Buffer-size protocol matches `recorder_sdk_list_devices_json`.
#[no_mangle]
pub extern "C" fn recorder_sdk_list_capture_sources_json(
    audio_system: *const c_char,
    out_json: *mut c_char,
    out_json_len: usize,
    required_len_out: *mut usize,
) -> c_int {
    clear_last_error();
    let audio_system = match opt_cstr(audio_system) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let host = match make_host(audio_system.as_deref()) {
        Ok(host) => host,
        Err(e) => {
            set_last_error(e.to_string());
            return RECORDER_SDK_ERROR;
        }
    };
    let json = match capture_sources_json(host.as_ref()) {
        Ok(json) => json,
        Err(e) => {
            set_last_error(e.to_string());
            return RECORDER_SDK_ERROR;
        }
    };
    write_json_to_caller_buffer(json, out_json, out_json_len, required_len_out)
}

fn write_json_to_caller_buffer(
    json: String,
    out_json: *mut c_char,
    out_json_len: usize,
    required_len_out: *mut usize,
) -> c_int {
    let required = json.len() + 1;
    if !required_len_out.is_null() {
        unsafe {
            *required_len_out = required;
        }
    }
    if out_json.is_null() || out_json_len < required {
        set_last_error(format!(
            "output buffer too small; required {required} bytes"
        ));
        return RECORDER_SDK_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(json.as_ptr(), out_json as *mut u8, json.len());
        *out_json.add(json.len()) = 0;
    }
    RECORDER_SDK_OK
}

/// Starts a recording and returns an opaque capture handle.
#[no_mangle]
pub extern "C" fn recorder_sdk_start_recording(
    config: *const RecorderStartConfig,
    out_capture: *mut *mut RecorderCapture,
) -> c_int {
    clear_last_error();
    if config.is_null() || out_capture.is_null() {
        set_last_error("config and out_capture must be non-null");
        return RECORDER_SDK_NULL_POINTER;
    }
    unsafe {
        *out_capture = ptr::null_mut();
    }
    match start_recording_inner(unsafe { &*config }) {
        Ok(handle) => {
            unsafe {
                *out_capture = handle;
            }
            RECORDER_SDK_OK
        }
        Err(code) => code,
    }
}

/// Stops a recording (joining all underlying capture streams). The handle remains valid
/// and must still be passed to `recorder_sdk_capture_free`.
#[no_mangle]
pub extern "C" fn recorder_sdk_capture_stop(capture: *mut RecorderCapture) -> c_int {
    clear_last_error();
    if capture.is_null() {
        set_last_error("capture must be non-null");
        return RECORDER_SDK_NULL_POINTER;
    }
    let capture = unsafe { &mut *capture };
    for stream in capture.captures.drain(..) {
        stream.stop();
    }
    RECORDER_SDK_OK
}

/// Stops, if needed, and frees a capture handle.
#[no_mangle]
pub extern "C" fn recorder_sdk_capture_free(capture: *mut RecorderCapture) {
    if capture.is_null() {
        return;
    }
    let mut capture = unsafe { Box::from_raw(capture) };
    for stream in capture.captures.drain(..) {
        stream.stop();
    }
}

#[cfg(test)]
mod tests {
    use recorder_core::{
        AppCaptureBackend, AppCaptureDescriptor, AudioFormat, CaptureSource, SampleFormat,
    };

    use super::{capture_sources_json, json_escape, AudioHost, CaptureSourceKind};

    struct FakeHost {
        sources: Vec<CaptureSource>,
    }

    impl AudioHost for FakeHost {
        fn list_input_devices(&self) -> recorder_core::Result<Vec<recorder_core::DeviceInfo>> {
            Ok(vec![])
        }

        fn start_input_stream(
            &self,
            _device_id: Option<&str>,
            _format: AudioFormat,
            _on_buffer: std::sync::Arc<dyn Fn(recorder_core::AudioBuffer) + Send + Sync>,
        ) -> recorder_core::Result<recorder_core::StreamHandle> {
            unreachable!("not needed for json tests")
        }

        fn list_capture_sources(&self) -> recorder_core::Result<Vec<CaptureSource>> {
            Ok(self.sources.clone())
        }
    }

    #[test]
    fn json_escape_handles_common_special_chars() {
        assert_eq!(json_escape("a\"b\\c\n"), "a\\\"b\\\\c\\n");
    }

    #[test]
    fn capture_sources_json_includes_app_output_metadata() {
        let host = FakeHost {
            sources: vec![CaptureSource {
                id: "app:42".into(),
                name: "Music Player".into(),
                default_format: Some(AudioFormat::new(48_000, 2, SampleFormat::F32)),
                kind: CaptureSourceKind::AppOutput,
                app: Some(AppCaptureDescriptor {
                    backend: AppCaptureBackend::WindowsProcessLoopback,
                    app_name: "Music Player".into(),
                    app_id: Some("com.example.player".into()),
                    process_id: Some(42),
                    instance_id: Some("session-42".into()),
                    supports_multi_select: true,
                    requires_system_permission: false,
                }),
            }],
        };

        let json = capture_sources_json(&host).expect("json");
        assert!(json.contains("\"kind\":\"app-output\""));
        assert!(json.contains("\"backend\":\"windows-process-loopback\""));
        assert!(json.contains("\"process_id\":42"));
        assert!(json.contains("\"supports_multi_select\":true"));
    }
}
