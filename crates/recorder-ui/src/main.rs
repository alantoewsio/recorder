//! Minimal desktop UI to exercise `recorder-core` + OS host crates.

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn main() {
    eprintln!("recorder-ui is only supported on Windows, macOS, and Linux.");
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn main() -> eframe::Result<()> {
    #[cfg(windows)]
    enable_per_monitor_dpi_awareness();

    // Renderer choice varies by Windows GPU/driver combo: Wgpu (DX12) presents black on some setups,
    // Glow (OpenGL) presents black on others. Default to Wgpu (works on the dev machines tried so far),
    // and let the user override at runtime via RECORDER_RENDERER=glow|wgpu without recompiling.
    let renderer_env = std::env::var("RECORDER_RENDERER")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let renderer = match renderer_env.as_str() {
        "glow" => eframe::Renderer::Glow,
        "wgpu" | "" => eframe::Renderer::Wgpu,
        other => {
            eprintln!("RECORDER_RENDERER='{other}' is not 'glow' or 'wgpu'; using wgpu.");
            eframe::Renderer::Wgpu
        }
    };
    eprintln!(
        "recorder-ui: renderer={} (set RECORDER_RENDERER=glow|wgpu to override)",
        match renderer {
            eframe::Renderer::Glow => "glow",
            eframe::Renderer::Wgpu => "wgpu",
        }
    );

    #[cfg(windows)]
    install_wgpu_device_lost_panic_hook();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]),
        persist_window: false,
        renderer,
        ..Default::default()
    };
    eframe::run_native(
        "Recorder demo",
        options,
        Box::new(|cc| {
            #[cfg(windows)]
            install_dpi_change_hook(cc);
            Ok(Box::new(RecorderApp::new(cc)) as Box<dyn eframe::App>)
        }),
    )
}

#[cfg(windows)]
fn enable_per_monitor_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };

    // Must run before eframe/winit creates the HWND. If Windows already fixed the process
    // awareness from a manifest or parent process, failure is harmless.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// wgpu 22 (the version eframe 0.29 ships with) panics out of `Queue::submit` when the
/// underlying D3D12 device is reset, instead of returning an error we could catch. The
/// device-lost panic surfaces most often when a sibling top-level window — the VST plugin
/// editor — is destroyed while the eframe surface is mid-frame, because Windows
/// renegotiates the swap chain between the two windows. Until eframe upgrades to a wgpu
/// version with `OnSubmittedWorkDone`/lost-device callbacks, the only way out is to
/// replace the unhelpful wgpu backtrace with an actionable hint and exit cleanly.
#[cfg(windows)]
fn install_wgpu_device_lost_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            String::new()
        };
        if msg.contains("device is lost")
            || msg.contains("Device is lost")
            || msg.contains("Parent device is lost")
        {
            eprintln!(
                "\nrecorder-ui: the GPU device backing the wgpu surface was reset \
                 (most often after closing a VST plugin editor window). \
                 Restart with `$env:RECORDER_RENDERER='glow'` (PowerShell) or \
                 `set RECORDER_RENDERER=glow` (cmd) to use the OpenGL backend, \
                 which is not affected by this Windows D3D12 issue."
            );
            std::process::exit(2);
        }
        prev(info);
    }));
}

#[cfg(windows)]
fn install_dpi_change_hook(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, GetWindowRect, SetWindowLongPtrW, GWLP_WNDPROC, WM_DPICHANGED, WNDPROC,
    };

    static PREV_WNDPROC: AtomicIsize = AtomicIsize::new(0);

    unsafe extern "system" fn dpi_stable_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_DPICHANGED {
            let suggested = lparam.0 as *mut RECT;
            if !suggested.is_null() {
                let mut current = RECT::default();
                if unsafe { GetWindowRect(hwnd, &mut current) }.is_ok() {
                    let width = current.right - current.left;
                    let height = current.bottom - current.top;
                    let suggested = unsafe { &mut *suggested };
                    suggested.right = suggested.left + width;
                    suggested.bottom = suggested.top + height;
                }
            }
        }

        let prev = PREV_WNDPROC.load(Ordering::Acquire);
        let prev: WNDPROC = unsafe { std::mem::transmute(prev) };
        unsafe { CallWindowProcW(prev, hwnd, msg, wparam, lparam) }
    }

    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);

    if PREV_WNDPROC.load(Ordering::Acquire) != 0 {
        return;
    }
    let prev =
        unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, dpi_stable_wndproc as *const () as isize) };
    if prev != 0 {
        PREV_WNDPROC.store(prev, Ordering::Release);
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use std::io::Write;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use crossbeam_channel;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use eframe::egui;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use recorder_core::error::RecordingError;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use recorder_core::format::{AudioFormat, SampleFormat};
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use recorder_core::traits::{AudioAnalyzer, AudioHost, AudioProcessor, AudioSink};
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use recorder_core::{
    media_event_queue, spawn_single_bus_mixer, AudioBuffer, BusMixer, BusMixerConfig,
    CaptureSource, CaptureSourceKind,
    CaptureStream, DeviceInfo, FlacSink, MediaEvent, MediaEventReceiver, MediaEventSender,
    MixMode, MixerConfig, Mp3Sink, RecordingSession, SessionConfig,
    StreamOptions, VoiceActivityAnalyzer, WavSink,
};
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use recorder_plugin_parakeet as parakeet_plugin;

#[cfg(all(windows, feature = "vst"))]
use recorder::vst;

#[cfg(all(windows, feature = "vst", test))]
mod vst_scan_smoke {
    //! Real-machine smoke test: invokes the catalog scanner and prints the diagnostics
    //! report. This is intentionally `#[ignore]` so `cargo test` defaults stay quiet —
    //! run with `cargo test --bin recorder-ui --features vst -- --ignored --nocapture`
    //! to see what the UI's "Scan for VST plugins" button would surface.

    #[test]
    #[ignore]
    fn print_scan_report() {
        let report = super::vst::scan_all_plugins_with_report()
            .expect("scan should not error fatally; per-folder failures are captured inline");
        println!("---- summary ----");
        println!("{}", report.summary());
        println!("---- details ----");
        println!("{}", report.details());
        println!("---- vst3 ({}) ----", report.vst3_plugins.len());
        for p in &report.vst3_plugins {
            println!("  {} @ {}", p.name, p.path.display());
        }
        println!("---- vst2 ({}) ----", report.vst2_plugins.len());
        for p in &report.vst2_plugins {
            println!(
                "  [{:08x}] {} ({}) in={} out={} @ {}",
                p.unique_id as u32,
                p.name,
                p.vendor,
                p.inputs,
                p.outputs,
                p.path.display()
            );
        }
    }
}
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use rodio::{Decoder, OutputStream, Sink, Source};

#[cfg(windows)]
fn make_host_windows(
    system: recorder_host_windows::WindowsAudioSystem,
) -> Box<dyn AudioHost + Send + Sync> {
    Box::new(recorder_host_windows::WindowsHost::new(system).expect("WindowsHost::new"))
}

#[cfg(all(not(windows), any(target_os = "macos", target_os = "linux")))]
fn make_host() -> Box<dyn AudioHost + Send + Sync> {
    #[cfg(target_os = "macos")]
    {
        Box::new(recorder_host_macos::MacosHost::default())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(recorder_host_linux::LinuxHost::default())
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
enum OutFormat {
    #[default]
    Wav,
    Flac,
    Mp3,
}

/// How to combine mic + speaker streams when both are recorded.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
enum SpeakerMode {
    /// Don't record speaker output.
    #[default]
    Off,
    /// Mic and speaker are written to two distinct files (`_mic_*` / `_speaker_*`).
    Separate,
    /// One stereo file: mic on the left channel, speaker on the right.
    StereoSplit,
    /// One file (mono or stereo), mic and speaker summed with a soft limiter.
    Mixed,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
impl SpeakerMode {
    fn label(self) -> &'static str {
        match self {
            SpeakerMode::Off => "Off",
            SpeakerMode::Separate => "Separate files",
            SpeakerMode::StereoSplit => "Stereo split (mic L / speaker R)",
            SpeakerMode::Mixed => "Mixed (sum)",
        }
    }

    fn is_active(self) -> bool {
        !matches!(self, SpeakerMode::Off)
    }

    /// Whether the mode produces a single combined output file via [`BusMixer`].
    fn uses_mixer(self) -> bool {
        matches!(self, SpeakerMode::StereoSplit | SpeakerMode::Mixed)
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct PersistedVstEntry {
    format: String,
    path: String,
    name: String,
    bypassed: bool,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct PersistedSettings {
    path: String,
    format: OutFormat,
    record_pre: bool,
    record_post: bool,
    live_enabled: bool,
    channel_gain: f32,
    input_channel: usize,
    /// Channel 2 (speaker loopback) trim; default 1.0 when absent from older saves.
    #[serde(default = "default_unit_gain")]
    channel2_gain: f32,
    #[serde(default)]
    channel2_input_channel: usize,
    selected_device_id: Option<String>,
    local_analyzer_order: Vec<String>,
    enabled_analyzers: Vec<String>,
    vst_chain: Vec<PersistedVstEntry>,
    #[serde(default)]
    vst_chain_ch2: Vec<PersistedVstEntry>,
    /// Default to off so loopback is opt-in; the UI shows a banner whenever it is on.
    /// Old settings persisted only this bool; the new `speaker_mode` field supersedes it
    /// but we still accept it for backward compatibility (true → `Separate`).
    #[serde(default)]
    record_speaker: bool,
    #[serde(default)]
    speaker_mode: Option<SpeakerMode>,
    #[serde(default)]
    selected_speaker_source_id: Option<String>,
    /// Parakeet NeMo sidecar settings (`None` = use env + built-in defaults on first launch).
    #[serde(default)]
    parakeet: Option<parakeet_plugin::ParakeetConfig>,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn default_unit_gain() -> f32 {
    1.0
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            path: "recording".to_string(),
            format: OutFormat::Wav,
            record_pre: true,
            record_post: false,
            live_enabled: true,
            channel_gain: 1.0,
            input_channel: 0,
            channel2_gain: 1.0,
            channel2_input_channel: 0,
            selected_device_id: None,
            local_analyzer_order: Vec::new(),
            enabled_analyzers: Vec::new(),
            vst_chain: Vec::new(),
            vst_chain_ch2: Vec::new(),
            record_speaker: false,
            speaker_mode: None,
            selected_speaker_source_id: None,
            parakeet: None,
        }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const SETTINGS_KEY: &str = "recorder-ui.channel-strip";
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const PARTIAL_PREFIX: &str = "… ";

/// Vertical gain slider height; must match the meter row below each strip.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const CHANNEL_STRIP_SLIDER_HEIGHT: f32 = 140.0;
/// One channel column: `draw_log_meter` (64) + gap + vertical slider — all columns use this width.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const CHANNEL_STRIP_COL_WIDTH: f32 = 64.0 + 6.0 + CHANNEL_STRIP_SLIDER_HEIGHT;
/// Side panel width: three columns + vertical separators + frame horizontal padding.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const CHANNEL_STRIP_PANEL_WIDTH: f32 = CHANNEL_STRIP_COL_WIDTH * 3.0 + 24.0;
/// Max height for the VST / internal plugin list — same in all three columns.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const CHANNEL_STRIP_FX_SCROLL_MAX_HEIGHT: f32 = 200.0;
/// Fixed height for the bottom "Live · …" block so Ch1 / Ch2 / Main align.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const CHANNEL_STRIP_LIVE_SECTION_HEIGHT: f32 = 152.0;

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl OutFormat {
    fn ext(self) -> &'static str {
        match self {
            OutFormat::Wav => "wav",
            OutFormat::Flac => "flac",
            OutFormat::Mp3 => "mp3",
        }
    }

    fn label(self) -> &'static str {
        match self {
            OutFormat::Wav => "WAV",
            OutFormat::Flac => "FLAC",
            OutFormat::Mp3 => "MP3 (needs LAME)",
        }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn build_sink(
    path: &Path,
    fmt: OutFormat,
    af: AudioFormat,
) -> std::result::Result<Box<dyn recorder_core::traits::AudioSink>, RecordingError> {
    match fmt {
        OutFormat::Wav => Ok(Box::new(WavSink::create(path, af)?)),
        OutFormat::Flac => Ok(Box::new(FlacSink::create(path, af)?)),
        OutFormat::Mp3 => Ok(Box::new(Mp3Sink::create(path, af)?)),
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn describe_media_event(event: &MediaEvent) -> String {
    match event {
        MediaEvent::VoiceActivity {
            start_frame,
            end_frame,
            active,
            level,
            confidence,
            ..
        } => format!(
            "Voice {} frames {start_frame}-{end_frame} level {:.3} conf {:.0}%",
            if *active { "active" } else { "inactive" },
            level,
            confidence * 100.0
        ),
        MediaEvent::SpeakerSegment {
            start_frame,
            end_frame,
            speaker_id,
            confidence,
            ..
        } => format!(
            "Speaker {speaker_id} frames {start_frame}-{end_frame} conf {:.0}%",
            confidence * 100.0
        ),
        MediaEvent::TranscriptPartial {
            start_frame,
            end_frame,
            speaker_id,
            text,
            ..
        } => format!(
            "Partial {}frames {start_frame}-{end_frame}: {text}",
            speaker_id
                .as_ref()
                .map(|s| format!("[{s}] "))
                .unwrap_or_default()
        ),
        MediaEvent::TranscriptFinal {
            start_frame,
            end_frame,
            speaker_id,
            text,
            ..
        } => format!(
            "Final {}frames {start_frame}-{end_frame}: {text}",
            speaker_id
                .as_ref()
                .map(|s| format!("[{s}] "))
                .unwrap_or_default()
        ),
        MediaEvent::AttributeDetected {
            start_frame,
            end_frame,
            key,
            value,
            confidence,
            ..
        } => {
            let conf = confidence
                .map(|c| format!(" conf {:.0}%", c * 100.0))
                .unwrap_or_default();
            if start_frame == end_frame && *start_frame == 0 {
                format!("{key}: {value}{conf}")
            } else {
                format!("{key}: {value} frames {start_frame}-{end_frame}{conf}")
            }
        }
        MediaEvent::PluginParameterChanged {
            plugin_id,
            parameter_id,
            value,
            ..
        } => format!("Plugin {plugin_id} parameter {parameter_id} changed to {value:?}"),
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn analyzer_status_log_path() -> PathBuf {
    PathBuf::from("analyzer-status.log")
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn reset_analyzer_status_log() {
    let path = analyzer_status_log_path();
    let _ = std::fs::File::create(&path).and_then(|mut file| {
        writeln!(
            file,
            "Recorder analyzer status log started at {:?}",
            std::time::SystemTime::now()
        )
    });
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn append_analyzer_status_log(message: &str) {
    let path = analyzer_status_log_path();
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{:?}\t{}", std::time::SystemTime::now(), message));
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn default_local_analyzers() -> Vec<LocalAnalyzerSelection> {
    let mut plugins = vec![LocalAnalyzerSelection {
        id: "builtin.voice-activity".to_string(),
        name: "Voice activity".to_string(),
        description: "Built-in RMS threshold analyzer for proving analyzer events.".to_string(),
        enabled: false,
    }];

    for descriptor in parakeet_plugin::descriptors() {
        plugins.push(LocalAnalyzerSelection {
            id: descriptor.id.to_string(),
            name: descriptor.name.to_string(),
            description: descriptor.description.to_string(),
            enabled: false,
        });
    }

    plugins
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn normalize_path(path: &str, fmt: OutFormat) -> PathBuf {
    let mut p = PathBuf::from(path.trim());
    if p.extension().is_none() || p.extension().and_then(|e| e.to_str()).is_none() {
        p.set_extension(fmt.ext());
    }
    p
}

/// When both pre- and post-processed files are written, disambiguate with `_pre` / `_post`.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn marked_output_path(path: &str, infix: &str, fmt: OutFormat) -> PathBuf {
    let mut p = normalize_path(path, fmt);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "recording".to_string());
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or(fmt.ext());
    p.set_file_name(format!("{stem}{infix}.{ext}"));
    p
}

/// Compose the file infix for a given `(source, stage)` pair.
///
/// - `_speaker` is added whenever the speaker stream is being recorded.
/// - `_mic` is added only when both mic and speaker are recorded (so single-source mic
///   recordings keep the legacy filenames intact).
/// - `_pre` / `_post` are added only when both stages are being recorded for that source.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn output_infix(
    record_mic: bool,
    record_speaker: bool,
    source: SourceTag,
    record_pre: bool,
    record_post: bool,
    stage: StageTag,
) -> String {
    let want_source = match source {
        SourceTag::Mic => record_mic && record_speaker,
        SourceTag::Speaker => record_speaker,
    };
    let want_stage = record_pre && record_post;
    let mut s = String::new();
    if want_source {
        s.push('_');
        s.push_str(source.label());
    }
    if want_stage {
        s.push('_');
        s.push_str(stage.label());
    }
    s
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum SourceTag {
    Mic,
    Speaker,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl SourceTag {
    fn label(self) -> &'static str {
        match self {
            SourceTag::Mic => "mic",
            SourceTag::Speaker => "speaker",
        }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy)]
enum StageTag {
    Pre,
    Post,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl StageTag {
    fn label(self) -> &'static str {
        match self {
            StageTag::Pre => "pre",
            StageTag::Post => "post",
        }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[derive(Default)]
struct AudioMeter {
    level_bits: AtomicU32,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl AudioMeter {
    fn set_level(&self, level: f32) {
        self.level_bits
            .store(level.clamp(0.0, 1.0).to_bits(), Ordering::Release);
    }

    fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Acquire)).clamp(0.0, 1.0)
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct MeterProcessor {
    meter: Arc<AudioMeter>,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl MeterProcessor {
    fn new(meter: Arc<AudioMeter>) -> Self {
        Self { meter }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl AudioProcessor for MeterProcessor {
    fn name(&self) -> &str {
        "meter"
    }

    fn process(
        &mut self,
        input: &AudioBuffer,
        output: &mut AudioBuffer,
    ) -> recorder_core::Result<()> {
        let peak = input
            .data
            .iter()
            .copied()
            .fold(0.0f32, |max, sample| max.max(sample.abs()));
        self.meter.set_level(peak);
        output.format = input.format;
        output.frames = input.frames;
        output.captured_at = input.captured_at;
        output.frame_index = input.frame_index;
        output.data = input.data.clone();
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct ChannelMapProcessor {
    channel: usize,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl ChannelMapProcessor {
    fn new(channel: usize) -> Self {
        Self { channel }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl AudioProcessor for ChannelMapProcessor {
    fn name(&self) -> &str {
        "channel-map"
    }

    fn process(
        &mut self,
        input: &AudioBuffer,
        output: &mut AudioBuffer,
    ) -> recorder_core::Result<()> {
        output.format = input.format;
        output.frames = input.frames;
        output.captured_at = input.captured_at;
        output.frame_index = input.frame_index;
        let channels = usize::from(input.format.channels.max(1));
        let selected = self.channel.min(channels.saturating_sub(1));
        if channels <= 1 || selected == 0 {
            output.data = input.data.clone();
            return Ok(());
        }

        let mut mapped = input.data.to_vec();
        for frame in 0..input.frames {
            let base = frame * channels;
            let Some(sample) = input.data.get(base + selected).copied() else {
                continue;
            };
            for ch in 0..channels {
                if let Some(out) = mapped.get_mut(base + ch) {
                    *out = sample;
                }
            }
        }
        output.data = mapped.into();
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct DemoGainProcessor {
    gain: f32,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl DemoGainProcessor {
    fn new(gain: f32) -> Self {
        Self { gain }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl AudioProcessor for DemoGainProcessor {
    fn name(&self) -> &str {
        "channel-gain"
    }

    fn process(
        &mut self,
        input: &AudioBuffer,
        output: &mut AudioBuffer,
    ) -> recorder_core::Result<()> {
        output.format = input.format;
        output.frames = input.frames;
        output.captured_at = input.captured_at;
        output.frame_index = input.frame_index;
        output.data = input
            .data
            .iter()
            .map(|sample| (sample * self.gain).clamp(-1.0, 1.0))
            .collect();
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn buffer_peak_f32(buf: &AudioBuffer) -> f32 {
    buf.data
        .iter()
        .copied()
        .fold(0.0f32, |max, sample| max.max(sample.abs()))
}

/// Optional file sink plus a tap for the Main bus (meter + analyzer queue).
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct FileAndMainBusSink {
    file: Option<Box<dyn AudioSink>>,
    bus_tx: crossbeam_channel::Sender<AudioBuffer>,
    meter: Arc<AudioMeter>,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl AudioSink for FileAndMainBusSink {
    fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> recorder_core::Result<()> {
        self.meter.set_level(buffer_peak_f32(buffer));
        let _ = self.bus_tx.try_send(buffer.clone());
        if let Some(file) = self.file.as_mut() {
            file.write_pcm_f32(buffer)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> recorder_core::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn spawn_main_analyzer_worker(
    main_rx: crossbeam_channel::Receiver<AudioBuffer>,
    mut analyzers: Vec<Box<dyn AudioAnalyzer + Send>>,
    event_tx: Option<MediaEventSender>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("recorder-ui-main-bus-analyzers".into())
        .spawn(move || {
            while let Ok(buf) = main_rx.recv() {
                for analyzer in analyzers.iter_mut() {
                    if let Err(e) = analyzer.accept_audio(&buf) {
                        eprintln!("analyzer error: {e}");
                    }
                    if let Some(tx) = event_tx.as_ref() {
                        for event in analyzer.drain_events() {
                            if tx.try_send(event).is_err() {
                                eprintln!("media event queue full; dropping analyzer event");
                            }
                        }
                    } else {
                        let _ = analyzer.drain_events();
                    }
                }
            }
            for analyzer in analyzers.iter_mut() {
                if let Some(tx) = event_tx.as_ref() {
                    for event in analyzer.drain_events() {
                        let _ = tx.try_send(event);
                    }
                }
            }
        })
        .expect("spawn main analyzer worker")
}

#[cfg(test)]
mod output_path_tests {
    use super::*;

    #[test]
    fn pre_and_post_paths_are_distinct_for_explicit_wav_name() {
        assert_eq!(
            marked_output_path("recording.wav", "_pre", OutFormat::Wav),
            PathBuf::from("recording_pre.wav")
        );
        assert_eq!(
            marked_output_path("recording.wav", "_post", OutFormat::Wav),
            PathBuf::from("recording_post.wav")
        );
    }

    #[test]
    fn pre_and_post_paths_are_distinct_when_extension_is_missing() {
        assert_eq!(
            marked_output_path("recording", "_pre", OutFormat::Wav),
            PathBuf::from("recording_pre.wav")
        );
        assert_eq!(
            marked_output_path("recording", "_post", OutFormat::Wav),
            PathBuf::from("recording_post.wav")
        );
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct RecordingState {
    captures: Vec<CaptureStream>,
    mixer: Option<BusMixer>,
    /// When the Main bus feeds analyzers off-mic (dual capture + mixer), join this after captures.
    main_analyzer: Option<std::thread::JoinHandle<()>>,
    paused: Arc<AtomicBool>,
    /// True when a loopback stream is recording; drives the privacy banner.
    speaker_active: bool,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[derive(Clone, PartialEq)]
struct LiveFingerprint {
    ch1_device: String,
    ch1_format: AudioFormat,
    ch1_gain: f32,
    ch1_input_channel: usize,
    ch2: Option<(String, AudioFormat)>,
    ch2_gain: f32,
    ch2_input_channel: usize,
    main_mix: SpeakerMode,
    live_enabled: bool,
    local_analyzer_sig: String,
    parakeet_sig: String,
    #[cfg(all(windows, feature = "vst"))]
    vst_ch1_sig: String,
    #[cfg(all(windows, feature = "vst"))]
    vst_ch2_sig: String,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
struct LiveInputState {
    ch1: CaptureStream,
    ch1_device_id: String,
    ch1_format: AudioFormat,
    ch2: Option<CaptureStream>,
    ch2_source_id: Option<String>,
    ch2_format: Option<AudioFormat>,
    mixer: Option<BusMixer>,
    main_analyzer: Option<std::thread::JoinHandle<()>>,
    fingerprint: LiveFingerprint,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum PluginPickerTarget {
    /// Built-in analyzers on the Main strip (Parakeet, voice activity, …).
    #[default]
    MainInternal,
    Ch1Vst,
    Ch2Vst,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct LocalAnalyzerSelection {
    id: String,
    name: String,
    description: String,
    enabled: bool,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
struct RecorderApp {
    #[cfg(windows)]
    audio_system: recorder_host_windows::WindowsAudioSystem,
    host: Box<dyn AudioHost + Send + Sync>,
    devices: Vec<DeviceInfo>,
    device_index: Option<usize>,
    /// Loopback (speaker output) capture sources, populated from `list_capture_sources`.
    speaker_sources: Vec<CaptureSource>,
    speaker_index: Option<usize>,
    speaker_mode: SpeakerMode,
    path: String,
    format: OutFormat,
    /// Capture from the device before the plugin chain.
    record_pre: bool,
    /// Capture after the plugin chain (dry pass-through if the chain is empty).
    record_post: bool,
    live_enabled: bool,
    /// Channel 1 (mic) trim gain.
    channel_gain: f32,
    /// Channel 1 input map (1-based index in UI is this + 1).
    input_channel: usize,
    /// Channel 2 (loopback) trim gain.
    channel2_gain: f32,
    channel2_input_channel: usize,
    dragged_chain_index: Option<usize>,
    dragged_chain_index_ch2: Option<usize>,
    dragged_internal_index: Option<usize>,
    plugin_picker_open: bool,
    plugin_picker_target: PluginPickerTarget,
    status: String,
    recording: Option<RecordingState>,
    last_file: Option<PathBuf>,
    last_pre_file: Option<PathBuf>,
    last_post_file: Option<PathBuf>,
    last_speaker_pre_file: Option<PathBuf>,
    last_speaker_post_file: Option<PathBuf>,
    last_main_post_file: Option<PathBuf>,
    playback: Option<std::thread::JoinHandle<()>>,
    meter_ch1: Arc<AudioMeter>,
    meter_ch2: Arc<AudioMeter>,
    meter_main: Arc<AudioMeter>,
    event_tx: MediaEventSender,
    event_rx: MediaEventReceiver,
    analysis_events: Vec<String>,
    transcript_lines: Vec<String>,
    local_analyzers: Vec<LocalAnalyzerSelection>,
    /// Active Parakeet analyzer configuration (persisted).
    parakeet_config: parakeet_plugin::ParakeetConfig,
    /// Editable copy while the properties window is open.
    parakeet_props_draft: parakeet_plugin::ParakeetConfig,
    parakeet_props_open: bool,
    live_input: Option<LiveInputState>,
    #[cfg(all(windows, feature = "vst"))]
    vst: vst::VstUiState,
    #[cfg(all(windows, feature = "vst"))]
    vst_ch2: vst::VstUiState,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl RecorderApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = cc
            .storage
            .and_then(|storage| eframe::get_value::<PersistedSettings>(storage, SETTINGS_KEY))
            .unwrap_or_default();
        #[cfg(all(windows, feature = "vst"))]
        let persisted_vst_chain = settings.vst_chain.clone();
        #[cfg(windows)]
        let audio_system = recorder_host_windows::WindowsAudioSystem::default();
        #[cfg(windows)]
        let host = make_host_windows(audio_system);
        #[cfg(not(windows))]
        let host = make_host();
        let all_sources = host.list_capture_sources().unwrap_or_default();
        let devices: Vec<DeviceInfo> = all_sources
            .iter()
            .filter(|s| s.kind == CaptureSourceKind::Input)
            .map(|s| DeviceInfo {
                id: s.id.clone(),
                name: s.name.clone(),
                default_format: s.default_format,
            })
            .collect();
        let speaker_sources: Vec<CaptureSource> = all_sources
            .into_iter()
            .filter(|s| s.kind == CaptureSourceKind::Loopback)
            .collect();
        let device_index = settings
            .selected_device_id
            .as_ref()
            .and_then(|id| devices.iter().position(|device| &device.id == id))
            .or_else(|| (!devices.is_empty()).then_some(0));
        let speaker_index = settings
            .selected_speaker_source_id
            .as_ref()
            .and_then(|id| speaker_sources.iter().position(|s| &s.id == id));
        let (event_tx, event_rx) = media_event_queue(1024);
        reset_analyzer_status_log();
        let mut local_analyzers = default_local_analyzers();
        if !settings.local_analyzer_order.is_empty() {
            let mut ordered = Vec::with_capacity(local_analyzers.len());
            for id in &settings.local_analyzer_order {
                if let Some(index) = local_analyzers.iter().position(|plugin| &plugin.id == id) {
                    ordered.push(local_analyzers.remove(index));
                }
            }
            ordered.extend(local_analyzers);
            local_analyzers = ordered;
        }
        for plugin in &mut local_analyzers {
            plugin.enabled = settings
                .enabled_analyzers
                .iter()
                .any(|enabled_id| enabled_id == &plugin.id);
        }
        let mut speaker_mode = settings.speaker_mode.unwrap_or_else(|| {
            // Migrate from the older bool-only persisted settings.
            if settings.record_speaker {
                SpeakerMode::Separate
            } else {
                SpeakerMode::Off
            }
        });
        if matches!(speaker_mode, SpeakerMode::Separate) {
            // Older "separate post files" mode is replaced by a single Main post file; default mix.
            speaker_mode = SpeakerMode::Mixed;
        }
        #[cfg(all(windows, feature = "vst"))]
        let persisted_vst_chain_ch2 = settings.vst_chain_ch2.clone();
        let parakeet_config = settings
            .parakeet
            .clone()
            .unwrap_or_else(parakeet_plugin::ParakeetConfig::default);
        let parakeet_props_draft = parakeet_config.clone();
        let mut app = Self {
            #[cfg(windows)]
            audio_system,
            host,
            devices,
            device_index,
            speaker_sources,
            speaker_index,
            speaker_mode,
            path: settings.path,
            format: settings.format,
            record_pre: settings.record_pre,
            record_post: settings.record_post,
            live_enabled: settings.live_enabled,
            channel_gain: settings.channel_gain.clamp(0.0, 4.0),
            input_channel: settings.input_channel,
            channel2_gain: settings.channel2_gain.clamp(0.0, 4.0),
            channel2_input_channel: settings.channel2_input_channel,
            dragged_chain_index: None,
            dragged_chain_index_ch2: None,
            dragged_internal_index: None,
            plugin_picker_open: false,
            plugin_picker_target: PluginPickerTarget::default(),
            status: "Ready.".to_string(),
            recording: None,
            last_file: None,
            last_pre_file: None,
            last_post_file: None,
            last_speaker_pre_file: None,
            last_speaker_post_file: None,
            last_main_post_file: None,
            playback: None,
            meter_ch1: Arc::new(AudioMeter::default()),
            meter_ch2: Arc::new(AudioMeter::default()),
            meter_main: Arc::new(AudioMeter::default()),
            event_tx,
            event_rx,
            analysis_events: Vec::new(),
            transcript_lines: Vec::new(),
            local_analyzers,
            parakeet_config,
            parakeet_props_draft,
            parakeet_props_open: false,
            live_input: None,
            #[cfg(all(windows, feature = "vst"))]
            vst: vst::VstUiState::default(),
            #[cfg(all(windows, feature = "vst"))]
            vst_ch2: vst::VstUiState::default(),
        };
        #[cfg(all(windows, feature = "vst"))]
        app.restore_persisted_vst_chain(&persisted_vst_chain);
        #[cfg(all(windows, feature = "vst"))]
        app.restore_persisted_vst_chain_ch2(&persisted_vst_chain_ch2);
        app.start_live_input_stream();
        app
    }

    fn stop_live_input_stream(&mut self) {
        if let Some(live) = self.live_input.take() {
            let LiveInputState {
                ch1,
                ch2,
                mixer,
                main_analyzer,
                ..
            } = live;
            ch1.stop();
            if let Some(c2) = ch2 {
                c2.stop();
            }
            if let Some(m) = mixer {
                m.stop();
            }
            if let Some(j) = main_analyzer {
                let _ = j.join();
            }
        }
        self.meter_ch1.set_level(0.0);
        self.meter_ch2.set_level(0.0);
        self.meter_main.set_level(0.0);
    }

    fn persisted_settings(&self) -> PersistedSettings {
        PersistedSettings {
            path: self.path.clone(),
            format: self.format,
            record_pre: self.record_pre,
            record_post: self.record_post,
            live_enabled: self.live_enabled,
            channel_gain: self.channel_gain,
            input_channel: self.input_channel,
            channel2_gain: self.channel2_gain,
            channel2_input_channel: self.channel2_input_channel,
            selected_device_id: self
                .device_index
                .and_then(|i| self.devices.get(i).map(|device| device.id.clone())),
            // Persist the legacy bool too so older binaries migrate cleanly when downgrading.
            record_speaker: self.ch2_selected(),
            speaker_mode: Some(self.speaker_mode),
            selected_speaker_source_id: self
                .speaker_index
                .and_then(|i| self.speaker_sources.get(i).map(|s| s.id.clone())),
            local_analyzer_order: self
                .local_analyzers
                .iter()
                .map(|plugin| plugin.id.clone())
                .collect(),
            enabled_analyzers: self
                .local_analyzers
                .iter()
                .filter(|plugin| plugin.enabled)
                .map(|plugin| plugin.id.clone())
                .collect(),
            vst_chain: {
                #[cfg(all(windows, feature = "vst"))]
                {
                    self.vst
                        .chain
                        .iter()
                        .map(|entry| PersistedVstEntry {
                            format: entry.catalog.format_label().to_string(),
                            path: entry.catalog.path().display().to_string(),
                            name: entry.catalog.name().to_string(),
                            bypassed: entry.bypassed,
                        })
                        .collect()
                }
                #[cfg(not(all(windows, feature = "vst")))]
                {
                    Vec::new()
                }
            },
            vst_chain_ch2: {
                #[cfg(all(windows, feature = "vst"))]
                {
                    self.vst_ch2
                        .chain
                        .iter()
                        .map(|entry| PersistedVstEntry {
                            format: entry.catalog.format_label().to_string(),
                            path: entry.catalog.path().display().to_string(),
                            name: entry.catalog.name().to_string(),
                            bypassed: entry.bypassed,
                        })
                        .collect()
                }
                #[cfg(not(all(windows, feature = "vst")))]
                {
                    Vec::new()
                }
            },
            parakeet: Some(self.parakeet_config.clone()),
        }
    }

    /// Channel 2 (loopback) is active when the user picked a speaker source and the host can capture it.
    fn ch2_selected(&self) -> bool {
        self.speaker_index.is_some()
            && self
                .speaker_index
                .and_then(|i| self.speaker_sources.get(i))
                .is_some()
    }

    fn selected_device_for_live_input(&self) -> Option<DeviceInfo> {
        self.device_index.and_then(|i| self.devices.get(i).cloned())
    }

    fn restart_live_input_stream(&mut self) {
        self.stop_live_input_stream();
        self.start_live_input_stream();
    }

    #[cfg_attr(not(all(windows, feature = "vst")), allow(dead_code))]
    fn ensure_live_input_stream(&mut self) {
        if self.recording.is_some() {
            self.stop_live_input_stream();
            return;
        }
        if !self.live_enabled {
            self.stop_live_input_stream();
            return;
        }
        let Some(dev) = self.selected_device_for_live_input() else {
            self.stop_live_input_stream();
            return;
        };
        let af = dev
            .default_format
            .unwrap_or_else(|| AudioFormat::new(48_000, 2, SampleFormat::F32));
        let fp = self.compute_live_fingerprint(&dev.id, af);
        let already_running = self
            .live_input
            .as_ref()
            .map(|live| live.fingerprint == fp)
            .unwrap_or(false);
        if !already_running {
            self.restart_live_input_stream();
        }
    }

    fn selected_loopback_pair(&self) -> Option<(CaptureSource, AudioFormat)> {
        if !self.loopback_supported() {
            return None;
        }
        let i = self.speaker_index?;
        let src = self.speaker_sources.get(i)?.clone();
        let af = src
            .default_format
            .unwrap_or_else(|| AudioFormat::new(48_000, 2, SampleFormat::F32));
        Some((src, af))
    }

    fn compute_live_fingerprint(&self, ch1_device_id: &str, ch1_format: AudioFormat) -> LiveFingerprint {
        let ch2 = self
            .selected_loopback_pair()
            .map(|(s, f)| (s.id, f));
        LiveFingerprint {
            ch1_device: ch1_device_id.to_string(),
            ch1_format,
            ch1_gain: self.channel_gain,
            ch1_input_channel: self.input_channel,
            ch2,
            ch2_gain: self.channel2_gain,
            ch2_input_channel: self.channel2_input_channel,
            main_mix: self.speaker_mode,
            live_enabled: self.live_enabled,
            local_analyzer_sig: self
                .local_analyzers
                .iter()
                .filter(|p| p.enabled)
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(","),
            parakeet_sig: format!("{:?}", self.parakeet_config),
            #[cfg(all(windows, feature = "vst"))]
            vst_ch1_sig: self
                .vst
                .chain
                .iter()
                .map(|e| e.catalog.path().display().to_string())
                .collect::<Vec<_>>()
                .join("|"),
            #[cfg(all(windows, feature = "vst"))]
            vst_ch2_sig: self
                .vst_ch2
                .chain
                .iter()
                .map(|e| e.catalog.path().display().to_string())
                .collect::<Vec<_>>()
                .join("|"),
        }
    }

    #[cfg_attr(not(all(windows, feature = "vst")), allow(unused_variables))]
    fn build_ch1_processors(
        &mut self,
        af: AudioFormat,
    ) -> std::result::Result<Vec<Box<dyn AudioProcessor + Send>>, String> {
        #[cfg_attr(
            not(all(windows, feature = "vst")),
            allow(unused_mut, unused_variables)
        )]
        let mut processors: Vec<Box<dyn AudioProcessor + Send>> =
            vec![Box::new(ChannelMapProcessor::new(self.input_channel))];
        processors.push(Box::new(MeterProcessor::new(self.meter_ch1.clone())));
        if (self.channel_gain - 1.0).abs() > f32::EPSILON {
            processors.push(Box::new(DemoGainProcessor::new(self.channel_gain)));
        }

        #[cfg(all(windows, feature = "vst"))]
        if !self.vst.chain.is_empty() {
            let max_block = vst::max_block_for_sample_rate(af.sample_rate_hz);
            vst::reinit_plugin_chain(&mut self.vst.chain, af.sample_rate_hz, max_block)?;
            processors.extend(self.vst.build_processor_chain(max_block));
        }

        Ok(processors)
    }

    #[cfg_attr(not(all(windows, feature = "vst")), allow(unused_variables))]
    fn build_ch2_processors(
        &mut self,
        af: AudioFormat,
    ) -> std::result::Result<Vec<Box<dyn AudioProcessor + Send>>, String> {
        #[cfg_attr(
            not(all(windows, feature = "vst")),
            allow(unused_mut, unused_variables)
        )]
        let mut processors: Vec<Box<dyn AudioProcessor + Send>> =
            vec![Box::new(ChannelMapProcessor::new(self.channel2_input_channel))];
        processors.push(Box::new(MeterProcessor::new(self.meter_ch2.clone())));
        if (self.channel2_gain - 1.0).abs() > f32::EPSILON {
            processors.push(Box::new(DemoGainProcessor::new(self.channel2_gain)));
        }

        #[cfg(all(windows, feature = "vst"))]
        if !self.vst_ch2.chain.is_empty() {
            let max_block = vst::max_block_for_sample_rate(af.sample_rate_hz);
            vst::reinit_plugin_chain(&mut self.vst_ch2.chain, af.sample_rate_hz, max_block)?;
            processors.extend(self.vst_ch2.build_processor_chain(max_block));
        }

        Ok(processors)
    }

    fn build_analyzers_for_format(
        &self,
        _af: AudioFormat,
    ) -> std::result::Result<Vec<Box<dyn AudioAnalyzer + Send>>, String> {
        let mut analyzers: Vec<Box<dyn AudioAnalyzer + Send>> = Vec::new();
        for plugin in self.local_analyzers.iter().filter(|plugin| plugin.enabled) {
            match plugin.id.as_str() {
                "builtin.voice-activity" => {
                    analyzers.push(Box::new(VoiceActivityAnalyzer::default()))
                }
                id if id == parakeet_plugin::PARAKEET_PLUGIN_ID => analyzers.push(
                    parakeet_plugin::create_analyzer_with_config(id, self.parakeet_config.clone())?,
                ),
                id => {
                    return Err(format!("unknown local analyzer plugin: {id}"));
                }
            }
        }
        Ok(analyzers)
    }

    fn drain_media_events(&mut self) {
        let events: Vec<MediaEvent> = self.event_rx.try_iter().collect();
        for event in events {
            match &event {
                MediaEvent::TranscriptPartial { text, .. } => {
                    let trimmed = text.trim();
                    let last_is_partial = self
                        .transcript_lines
                        .last()
                        .is_some_and(|line| line.starts_with(PARTIAL_PREFIX));
                    if trimmed.is_empty() {
                        if last_is_partial {
                            self.transcript_lines.pop();
                        }
                    } else {
                        let formatted = format!("{PARTIAL_PREFIX}{trimmed}");
                        if last_is_partial {
                            if let Some(last) = self.transcript_lines.last_mut() {
                                *last = formatted;
                            }
                        } else {
                            self.transcript_lines.push(formatted);
                        }
                    }
                }
                MediaEvent::TranscriptFinal { text, .. } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let last_is_partial = self
                        .transcript_lines
                        .last()
                        .is_some_and(|line| line.starts_with(PARTIAL_PREFIX));
                    if last_is_partial {
                        if let Some(last) = self.transcript_lines.last_mut() {
                            *last = trimmed.to_string();
                        }
                    } else {
                        self.transcript_lines.push(trimmed.to_string());
                    }
                }
                _ => {
                    let message = describe_media_event(&event);
                    append_analyzer_status_log(&message);
                    self.analysis_events.push(message);
                }
            }
        }
        const MAX_EVENTS: usize = 100;
        if self.analysis_events.len() > MAX_EVENTS {
            let remove = self.analysis_events.len() - MAX_EVENTS;
            self.analysis_events.drain(0..remove);
        }
        const MAX_TRANSCRIPT_LINES: usize = 200;
        if self.transcript_lines.len() > MAX_TRANSCRIPT_LINES {
            let remove = self.transcript_lines.len() - MAX_TRANSCRIPT_LINES;
            self.transcript_lines.drain(0..remove);
        }
    }

    fn start_live_input_stream(&mut self) {
        if self.recording.is_some() {
            return;
        }
        if !self.live_enabled {
            self.meter_ch1.set_level(0.0);
            self.meter_ch2.set_level(0.0);
            self.meter_main.set_level(0.0);
            return;
        }
        let Some(dev) = self.selected_device_for_live_input() else {
            return;
        };
        let af = dev
            .default_format
            .unwrap_or_else(|| AudioFormat::new(48_000, 2, SampleFormat::F32));
        let fingerprint = self.compute_live_fingerprint(&dev.id, af);

        let use_main_bus = self.ch2_selected() && self.loopback_supported();

        if !use_main_bus {
            let processors = match self.build_ch1_processors(af) {
                Ok(p) => p,
                Err(e) => {
                    self.status = format!("Live input setup failed: {e}");
                    return;
                }
            };
            let analyzers = match self.build_analyzers_for_format(af) {
                Ok(a) => a,
                Err(e) => {
                    self.status = format!("Analyzer setup failed: {e}");
                    return;
                }
            };
            let session = RecordingSession::new(SessionConfig::default());
            let opts = StreamOptions {
                raw_sink: None,
                processed_sink: None,
                processors,
                analyzers,
                event_tx: Some(self.event_tx.clone()),
                pause_gate: None,
            };
            match session.add_capture_stream(
                self.host.as_ref(),
                Some(dev.id.as_str()),
                CaptureSourceKind::Input,
                af,
                opts,
            ) {
                Ok(ch1) => {
                    self.live_input = Some(LiveInputState {
                        ch1,
                        ch1_device_id: dev.id,
                        ch1_format: af,
                        ch2: None,
                        ch2_source_id: None,
                        ch2_format: None,
                        mixer: None,
                        main_analyzer: None,
                        fingerprint,
                    });
                }
                Err(e) => {
                    self.status = format!("Live input stream failed: {e}");
                }
            }
            return;
        }

        let Some((spk_src, saf)) = self.selected_loopback_pair() else {
            self.status =
                "Select a speaker (loopback) source on channel 2 to preview system audio."
                    .to_string();
            return;
        };

        let mix_mode = match self.speaker_mode {
            SpeakerMode::StereoSplit => MixMode::SplitStereo,
            SpeakerMode::Mixed | SpeakerMode::Separate | SpeakerMode::Off => MixMode::SumStereo,
        };

        let ch1_processors = match self.build_ch1_processors(af) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Live channel 1 setup failed: {e}");
                return;
            }
        };
        let ch2_processors = match self.build_ch2_processors(saf) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Live channel 2 setup failed: {e}");
                return;
            }
        };

        let analyzers = match self.build_analyzers_for_format(af) {
            Ok(a) => a,
            Err(e) => {
                self.status = format!("Analyzer setup failed: {e}");
                return;
            }
        };

        let (main_tx, main_rx) = crossbeam_channel::bounded::<AudioBuffer>(64);
        let main_analyzer = spawn_main_analyzer_worker(
            main_rx,
            analyzers,
            Some(self.event_tx.clone()),
        );

        let cfg = MixerConfig {
            mode: mix_mode,
            mic_format: af,
            speaker_format: saf,
            jitter_window: std::time::Duration::from_millis(200),
        };
        let bus_cfg = BusMixerConfig::from(cfg);
        let out_sink: Box<dyn AudioSink> = Box::new(FileAndMainBusSink {
            file: None,
            bus_tx: main_tx,
            meter: self.meter_main.clone(),
        });
        let (mut leg_sinks, mixer) = match spawn_single_bus_mixer(64, bus_cfg, out_sink) {
            Ok(x) => x,
            Err(e) => {
                self.status = format!("Live mixer failed: {e}");
                let _ = main_analyzer.join();
                return;
            }
        };
        let mic_in = leg_sinks.remove(0);
        let spk_in = leg_sinks.remove(0);

        let session = RecordingSession::new(SessionConfig::default());

        let ch1_opts = StreamOptions {
            raw_sink: None,
            processed_sink: Some(Box::new(mic_in)),
            processors: ch1_processors,
            analyzers: Vec::new(),
            event_tx: None,
            pause_gate: None,
        };
        let ch1 = match session.add_capture_stream(
            self.host.as_ref(),
            Some(dev.id.as_str()),
            CaptureSourceKind::Input,
            af,
            ch1_opts,
        ) {
            Ok(c) => c,
            Err(e) => {
                self.status = format!("Live mic stream failed: {e}");
                mixer.stop();
                let _ = main_analyzer.join();
                return;
            }
        };

        let ch2_opts = StreamOptions {
            raw_sink: None,
            processed_sink: Some(Box::new(spk_in)),
            processors: ch2_processors,
            analyzers: Vec::new(),
            event_tx: None,
            pause_gate: None,
        };
        let ch2 = match session.add_capture_stream(
            self.host.as_ref(),
            Some(spk_src.id.as_str()),
            CaptureSourceKind::Loopback,
            saf,
            ch2_opts,
        ) {
            Ok(c) => c,
            Err(e) => {
                self.status = format!("Live speaker stream failed: {e}");
                ch1.stop();
                mixer.stop();
                let _ = main_analyzer.join();
                return;
            }
        };

        self.live_input = Some(LiveInputState {
            ch1,
            ch1_device_id: dev.id,
            ch1_format: af,
            ch2: Some(ch2),
            ch2_source_id: Some(spk_src.id),
            ch2_format: Some(saf),
            mixer: Some(mixer),
            main_analyzer: Some(main_analyzer),
            fingerprint,
        });
    }

    #[cfg(windows)]
    fn set_audio_system(&mut self, system: recorder_host_windows::WindowsAudioSystem) {
        if self.audio_system == system {
            return;
        }
        self.stop_live_input_stream();
        self.audio_system = system;
        self.host = make_host_windows(system);
        self.refresh_devices();
    }

    fn refresh_devices(&mut self) {
        let prev_speaker_id = self.speaker_index.and_then(|i| {
            self.speaker_sources
                .get(i)
                .map(|s| s.id.clone())
        });
        match self.host.list_capture_sources() {
            Ok(list) => {
                self.devices = list
                    .iter()
                    .filter(|s| s.kind == CaptureSourceKind::Input)
                    .map(|s| DeviceInfo {
                        id: s.id.clone(),
                        name: s.name.clone(),
                        default_format: s.default_format,
                    })
                    .collect();
                self.speaker_sources = list
                    .into_iter()
                    .filter(|s| s.kind == CaptureSourceKind::Loopback)
                    .collect();
                self.device_index = if self.devices.is_empty() {
                    None
                } else {
                    Some(
                        self.device_index
                            .unwrap_or(0)
                            .min(self.devices.len().saturating_sub(1)),
                    )
                };
                self.speaker_index = if self.speaker_sources.is_empty() {
                    None
                } else {
                    prev_speaker_id
                        .as_ref()
                        .and_then(|id| {
                            self.speaker_sources
                                .iter()
                                .position(|s| &s.id == id)
                        })
                };
                self.status = format!(
                    "Found {} input device(s) and {} loopback source(s).",
                    self.devices.len(),
                    self.speaker_sources.len()
                );
            }
            Err(e) => {
                self.status = format!("Device list error: {e}");
                self.devices.clear();
                self.device_index = None;
                self.speaker_sources.clear();
                self.speaker_index = None;
            }
        }
        self.restart_live_input_stream();
    }

    /// Whether the current host backend can capture system-audio loopback. Drives the UI
    /// gating for the "Record speaker output" controls.
    fn loopback_supported(&self) -> bool {
        #[cfg(windows)]
        {
            self.audio_system == recorder_host_windows::WindowsAudioSystem::Wasapi
        }
        #[cfg(not(windows))]
        {
            !self.speaker_sources.is_empty()
        }
    }

    fn loopback_unavailable_hint(&self) -> &'static str {
        #[cfg(windows)]
        {
            "Loopback capture requires the WASAPI audio system."
        }
        #[cfg(target_os = "macos")]
        {
            "macOS needs a virtual loopback driver (BlackHole, Loopback, etc.) for speaker capture."
        }
        #[cfg(target_os = "linux")]
        {
            "No PulseAudio/PipeWire monitor source was found for any output sink."
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            "Loopback is not supported on this platform."
        }
    }

    fn drain_playback(&mut self) {
        if let Some(h) = self.playback.take() {
            let _ = h.join();
        }
    }

    fn start_recording(&mut self) {
        self.drain_playback();
        let ch2_active = self.ch2_selected() && self.loopback_supported();

        if !self.record_pre && !self.record_post {
            self.status = "Enable at least one of “Record pre” or “Record post”.".to_string();
            return;
        }

        let Some(i) = self.device_index else {
            self.status = "Select an input device.".to_string();
            return;
        };
        let dev = self.devices[i].clone();
        let af = dev
            .default_format
            .unwrap_or_else(|| AudioFormat::new(48_000, 2, SampleFormat::F32));

        let speaker = if ch2_active {
            match self.selected_loopback_pair() {
                Some(s) => Some(s),
                None => {
                    self.status = "Select a speaker (loopback) source on channel 2.".to_string();
                    return;
                }
            }
        } else {
            None
        };

        self.stop_live_input_stream();

        let ch1_processors = match self.build_ch1_processors(af) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Channel 1 processor setup failed: {e}");
                self.restart_live_input_stream();
                return;
            }
        };

        let ch2_processors = if let Some((_, saf)) = speaker.as_ref() {
            match self.build_ch2_processors(*saf) {
                Ok(p) => Some(p),
                Err(e) => {
                    self.status = format!("Channel 2 processor setup failed: {e}");
                    self.restart_live_input_stream();
                    return;
                }
            }
        } else {
            None
        };

        let use_main_bus = ch2_active;
        let mic_analyzers = if use_main_bus {
            Vec::new()
        } else {
            match self.build_analyzers_for_format(af) {
                Ok(a) => a,
                Err(e) => {
                    self.status = format!("Analyzer setup failed: {e}");
                    self.restart_live_input_stream();
                    return;
                }
            }
        };
        let main_analyzers = if use_main_bus {
            match self.build_analyzers_for_format(af) {
                Ok(a) => a,
                Err(e) => {
                    self.status = format!("Analyzer setup failed: {e}");
                    self.restart_live_input_stream();
                    return;
                }
            }
        } else {
            Vec::new()
        };

        // ----- output paths -----
        let ch1_pre_path = if self.record_pre {
            if ch2_active {
                Some(marked_output_path(&self.path, "_ch1_pre", self.format))
            } else {
                let infix = output_infix(
                    true,
                    false,
                    SourceTag::Mic,
                    self.record_pre,
                    self.record_post,
                    StageTag::Pre,
                );
                Some(if infix.is_empty() {
                    normalize_path(&self.path, self.format)
                } else {
                    marked_output_path(&self.path, &infix, self.format)
                })
            }
        } else {
            None
        };

        let ch2_pre_path =
            (ch2_active && self.record_pre).then(|| marked_output_path(&self.path, "_ch2_pre", self.format));

        let main_post_path = if ch2_active && self.record_post {
            Some(marked_output_path(&self.path, "_main_post", self.format))
        } else if !ch2_active && self.record_post {
            let infix = output_infix(
                true,
                false,
                SourceTag::Mic,
                self.record_pre,
                self.record_post,
                StageTag::Post,
            );
            Some(if infix.is_empty() {
                normalize_path(&self.path, self.format)
            } else {
                marked_output_path(&self.path, &infix, self.format)
            })
        } else {
            None
        };

        let mic_raw_sink: Option<Box<dyn AudioSink>> = if let Some(p) = ch1_pre_path.as_ref() {
            match build_sink(p, self.format, af) {
                Ok(s) => Some(s),
                Err(e) => {
                    self.status = format!("Could not open channel 1 pre output: {e}");
                    self.restart_live_input_stream();
                    return;
                }
            }
        } else {
            None
        };

        let ch2_raw_sink: Option<Box<dyn AudioSink>> =
            if let Some(p) = ch2_pre_path.as_ref() {
                let saf = speaker.as_ref().unwrap().1;
                match build_sink(p, self.format, saf) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        self.status = format!("Could not open channel 2 pre output: {e}");
                        self.restart_live_input_stream();
                        return;
                    }
                }
            } else {
                None
            };

        let paused = Arc::new(AtomicBool::new(false));

        let mut main_analyzer_join: Option<std::thread::JoinHandle<()>> = None;
        let mut pending_main_bus_mixer: Option<BusMixer> = None;
        let mut mic_post_sink: Option<Box<dyn AudioSink>> = None;
        let mut spk_post_sink: Option<Box<dyn AudioSink>> = None;

        if use_main_bus {
            let (_src, saf) = speaker.as_ref().expect("ch2");
            let mix_mode = match self.speaker_mode {
                SpeakerMode::StereoSplit => MixMode::SplitStereo,
                SpeakerMode::Mixed | SpeakerMode::Separate | SpeakerMode::Off => MixMode::SumStereo,
            };
            let cfg = MixerConfig {
                mode: mix_mode,
                mic_format: af,
                speaker_format: *saf,
                jitter_window: std::time::Duration::from_millis(200),
            };

            let (main_tx, main_rx) = crossbeam_channel::bounded::<AudioBuffer>(64);
            main_analyzer_join = Some(spawn_main_analyzer_worker(
                main_rx,
                main_analyzers,
                Some(self.event_tx.clone()),
            ));

            let main_file: Option<Box<dyn AudioSink>> = if self.record_post {
                let p = main_post_path.as_ref().expect("main post path");
                match build_sink(p, self.format, cfg.output_format()) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        self.status = format!("Could not open main post output: {e}");
                        if let Some(j) = main_analyzer_join.take() {
                            drop(main_tx);
                            let _ = j.join();
                        }
                        self.restart_live_input_stream();
                        return;
                    }
                }
            } else {
                None
            };

            let out_sink: Box<dyn AudioSink> = Box::new(FileAndMainBusSink {
                file: main_file,
                bus_tx: main_tx,
                meter: self.meter_main.clone(),
            });

            let bus_cfg = BusMixerConfig::from(cfg);
            match spawn_single_bus_mixer(64, bus_cfg, out_sink) {
                Ok((mut legs, m)) => {
                    let mic_in = legs.remove(0);
                    let spk_in = legs.remove(0);
                    mic_post_sink = Some(Box::new(mic_in));
                    spk_post_sink = Some(Box::new(spk_in));
                    pending_main_bus_mixer = Some(m);
                }
                Err(e) => {
                    self.status = format!("Mixer start failed: {e}");
                    if let Some(j) = main_analyzer_join.take() {
                        let _ = j.join();
                    }
                    self.restart_live_input_stream();
                    return;
                }
            }
        } else if self.record_post {
            let p = main_post_path.as_ref().expect("mic post path");
            match build_sink(p, self.format, af) {
                Ok(s) => {
                    mic_post_sink = Some(s);
                }
                Err(e) => {
                    self.status = format!("Could not open post output: {e}");
                    self.restart_live_input_stream();
                    return;
                }
            }
        }

        let session = RecordingSession::new(SessionConfig::default());
        let mic_opts = StreamOptions {
            raw_sink: mic_raw_sink,
            processed_sink: mic_post_sink,
            processors: ch1_processors,
            analyzers: mic_analyzers,
            event_tx: if use_main_bus {
                None
            } else {
                Some(self.event_tx.clone())
            },
            pause_gate: Some(paused.clone()),
        };
        let mic_capture = match session.add_capture_stream(
            self.host.as_ref(),
            Some(dev.id.as_str()),
            CaptureSourceKind::Input,
            af,
            mic_opts,
        ) {
            Ok(capture) => capture,
            Err(e) => {
                self.status = format!("Mic start failed: {e}");
                if let Some(m) = pending_main_bus_mixer.take() {
                    m.stop();
                }
                if let Some(j) = main_analyzer_join.take() {
                    let _ = j.join();
                }
                self.restart_live_input_stream();
                return;
            }
        };

        let mut captures = vec![mic_capture];
        let mut speaker_active = false;

        if let Some((src, saf)) = speaker {
            let procs = ch2_processors.unwrap_or_default();
            let speaker_opts = StreamOptions {
                raw_sink: ch2_raw_sink,
                processed_sink: spk_post_sink,
                processors: procs,
                analyzers: Vec::new(),
                event_tx: None,
                pause_gate: Some(paused.clone()),
            };
            match session.add_capture_stream(
                self.host.as_ref(),
                Some(src.id.as_str()),
                CaptureSourceKind::Loopback,
                saf,
                speaker_opts,
            ) {
                Ok(speaker_capture) => {
                    captures.push(speaker_capture);
                    speaker_active = true;
                }
                Err(e) => {
                    self.status = format!("Speaker start failed: {e}");
                    for c in captures.drain(..) {
                        c.stop();
                    }
                    if let Some(m) = pending_main_bus_mixer.take() {
                        m.stop();
                    }
                    if let Some(j) = main_analyzer_join.take() {
                        let _ = j.join();
                    }
                    self.restart_live_input_stream();
                    return;
                }
            }
        }

        let mixer = pending_main_bus_mixer;

        self.last_pre_file = ch1_pre_path.clone();
        self.last_speaker_pre_file = ch2_pre_path.clone();
        self.last_main_post_file = main_post_path.clone();
        self.last_post_file = main_post_path.clone();
        self.last_speaker_post_file = None;
        self.last_file = self
            .last_main_post_file
            .clone()
            .or_else(|| self.last_post_file.clone())
            .or_else(|| self.last_pre_file.clone())
            .or_else(|| self.last_speaker_pre_file.clone());

        self.recording = Some(RecordingState {
            captures,
            mixer,
            main_analyzer: main_analyzer_join,
            paused,
            speaker_active,
        });
        self.status = if speaker_active {
            "Recording channel 1 + channel 2 (+ Main when post is enabled)… (pause skips writing; stop finalizes files.)".to_string()
        } else {
            "Recording… (pause skips writing; stop finalizes files.)".to_string()
        };
    }

    fn toggle_pause(&mut self) {
        let Some(rec) = &self.recording else {
            return;
        };
        let v = !rec.paused.load(Ordering::Acquire);
        rec.paused.store(v, Ordering::Release);
        self.status = if v {
            "Paused (audio not written).".to_string()
        } else {
            "Recording…".to_string()
        };
    }

    fn stop_recording(&mut self) {
        if let Some(rec) = self.recording.take() {
            for capture in rec.captures {
                capture.stop();
            }
            // Stopping the captures drops the per-stream writer threads, which closes the
            // mixer's input channels so the mixer thread finishes naturally.
            if let Some(mixer) = rec.mixer {
                mixer.stop();
            }
            if let Some(j) = rec.main_analyzer {
                let _ = j.join();
            }
            let ch1_pre = self
                .last_pre_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "—".to_string());
            let ch2_pre = self
                .last_speaker_pre_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "—".to_string());
            let main_post = self
                .last_main_post_file
                .as_ref()
                .or(self.last_post_file.as_ref())
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "—".to_string());
            if rec.speaker_active {
                self.status = format!(
                    "Stopped. Ch1 pre: {ch1_pre}  Ch2 pre: {ch2_pre}  Main post: {main_post}"
                );
            } else {
                self.status =
                    format!("Stopped. Pre: {ch1_pre}  Post: {main_post}");
            }
            self.restart_live_input_stream();
        }
    }

    fn play_last(&mut self) {
        let path = self
            .last_main_post_file
            .clone()
            .or_else(|| self.last_post_file.clone())
            .or_else(|| self.last_pre_file.clone())
            .or_else(|| self.last_file.clone());
        let Some(path) = path else {
            self.status = "No file to play yet.".to_string();
            return;
        };
        if !path.exists() {
            self.status = format!("Missing file: {}", path.display());
            return;
        }
        self.drain_playback();
        let path_clone = path.clone();
        self.playback = Some(std::thread::spawn(move || {
            let play = || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                let (_stream, handle) = OutputStream::try_default()?;
                let sink = Sink::try_new(&handle)?;
                let file = std::fs::File::open(&path_clone)?;
                let decoder = Decoder::new(std::io::BufReader::new(file))?;
                sink.append(decoder.convert_samples::<f32>());
                sink.sleep_until_end();
                Ok(())
            };
            if let Err(e) = play() {
                eprintln!("playback error: {e}");
            }
        }));
        self.status = format!("Playing {} …", path.display());
    }

    fn draw_log_meter(ui: &mut egui::Ui, level: f32, height: f32) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(64.0, height), egui::Sense::hover());
        let painter = ui.painter();
        let meter_rect = egui::Rect::from_min_max(
            egui::pos2(rect.right() - 28.0, rect.top()),
            egui::pos2(rect.right(), rect.bottom()),
        );
        painter.rect_filled(meter_rect, 2.0, egui::Color32::from_rgb(18, 22, 20));
        painter.rect_stroke(
            meter_rect,
            2.0,
            egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
        );

        let db = 20.0 * level.max(0.000_001).log10();
        let normalized = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let fill = egui::Rect::from_min_max(
            egui::pos2(
                meter_rect.left() + 3.0,
                meter_rect.bottom() - meter_rect.height() * normalized,
            ),
            egui::pos2(meter_rect.right() - 3.0, meter_rect.bottom() - 3.0),
        );
        let color = if db > -6.0 {
            egui::Color32::from_rgb(230, 64, 30)
        } else if db > -18.0 {
            egui::Color32::from_rgb(220, 180, 40)
        } else {
            egui::Color32::from_rgb(46, 180, 120)
        };
        painter.rect_filled(fill, 1.0, color);

        let font = egui::FontId::monospace(9.0);
        for db_mark in [0.0, -6.0, -18.0, -30.0, -42.0, -54.0, -60.0] {
            let y = meter_rect.bottom() - meter_rect.height() * ((db_mark + 60.0) / 60.0);
            painter.line_segment(
                [
                    egui::pos2(meter_rect.left(), y),
                    egui::pos2(meter_rect.right(), y),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_gray(60)),
            );
            let label = if db_mark == 0.0 {
                "0".to_string()
            } else {
                format!("{db_mark:.0}")
            };
            painter.text(
                egui::pos2(meter_rect.left() - 4.0, y),
                egui::Align2::RIGHT_CENTER,
                label,
                font.clone(),
                egui::Color32::from_rgb(190, 190, 190),
            );
        }

        painter.text(
            egui::pos2(rect.left(), rect.top()),
            egui::Align2::LEFT_TOP,
            "dB",
            font,
            egui::Color32::from_rgb(150, 150, 150),
        );
    }

    fn selected_device_channel_count(&self) -> usize {
        self.device_index
            .and_then(|i| self.devices.get(i))
            .and_then(|device| device.default_format)
            .map(|format| usize::from(format.channels.max(1)))
            .unwrap_or(1)
    }

    fn selected_speaker_channel_count(&self) -> usize {
        self.speaker_index
            .and_then(|i| self.speaker_sources.get(i))
            .and_then(|s| s.default_format)
            .map(|format| usize::from(format.channels.max(1)))
            .unwrap_or(1)
    }

    fn move_internal_plugin_to(&mut self, from: usize, to: usize) {
        if from >= self.local_analyzers.len() || to >= self.local_analyzers.len() || from == to {
            return;
        }
        let plugin = self.local_analyzers.remove(from);
        self.local_analyzers.insert(to, plugin);
        self.analysis_events.clear();
        self.transcript_lines.clear();
        self.restart_live_input_stream();
    }

    #[cfg(all(windows, feature = "vst"))]
    fn restore_persisted_vst_chain(&mut self, saved_chain: &[PersistedVstEntry]) {
        if saved_chain.is_empty() {
            return;
        }

        self.vst.scan_catalog();
        if self.vst.catalog.is_empty() {
            self.status = "Saved VST chain could not be restored: no plugins found.".to_string();
            return;
        }

        let mut restored = 0usize;
        for saved in saved_chain {
            let Some(index) = self.vst.catalog.iter().position(|plugin| {
                plugin.format_label() == saved.format
                    && plugin.path().display().to_string() == saved.path
            }) else {
                continue;
            };
            self.vst.catalog_pick = index;
            if self.vst.add_pick_to_chain().is_ok() {
                if let Some(entry) = self.vst.chain.last_mut() {
                    entry.bypassed = saved.bypassed;
                }
                restored += 1;
            }
        }

        if restored > 0 {
            self.status = format!("Restored {restored} saved VST plugin(s).");
        } else {
            self.status = "Saved VST chain did not match the current scan results.".to_string();
        }
    }

    #[cfg(all(windows, feature = "vst"))]
    fn restore_persisted_vst_chain_ch2(&mut self, saved_chain: &[PersistedVstEntry]) {
        if saved_chain.is_empty() {
            return;
        }

        self.vst_ch2.scan_catalog();
        if self.vst_ch2.catalog.is_empty() {
            self.status =
                "Saved VST chain (Ch2) could not be restored: no plugins found.".to_string();
            return;
        }

        let mut restored = 0usize;
        for saved in saved_chain {
            let Some(index) = self.vst_ch2.catalog.iter().position(|plugin| {
                plugin.format_label() == saved.format
                    && plugin.path().display().to_string() == saved.path
            }) else {
                continue;
            };
            self.vst_ch2.catalog_pick = index;
            if self.vst_ch2.add_pick_to_chain().is_ok() {
                if let Some(entry) = self.vst_ch2.chain.last_mut() {
                    entry.bypassed = saved.bypassed;
                }
                restored += 1;
            }
        }

        if restored > 0 {
            self.status = format!("Restored {restored} channel-2 VST plugin(s).");
        }
    }

    #[cfg(all(windows, feature = "vst"))]
    fn toggle_vst_editor_from_strip_ch2(&mut self, index: usize) {
        self.ensure_live_input_stream();
        let needs_pipeline_restart = self
            .vst_ch2
            .chain
            .get(index)
            .map(|entry| matches!(entry.catalog, vst::CatalogEntry::Vst3(_)))
            .unwrap_or(false);
        self.vst_ch2.toggle_native_editor(index);
        if needs_pipeline_restart {
            self.restart_live_input_stream();
        }
    }

    #[cfg(all(windows, feature = "vst"))]
    fn toggle_vst_editor_from_strip(&mut self, index: usize) {
        self.ensure_live_input_stream();
        let needs_pipeline_restart = self
            .vst
            .chain
            .get(index)
            .map(|entry| matches!(entry.catalog, vst::CatalogEntry::Vst3(_)))
            .unwrap_or(false);
        self.vst.toggle_native_editor(index);
        if needs_pipeline_restart {
            self.restart_live_input_stream();
        }
    }

    fn draw_plugin_picker(&mut self, ctx: &egui::Context) {
        if !self.plugin_picker_open {
            return;
        }

        let mut open = self.plugin_picker_open;
        let mut picked_internal: Option<usize> = None;
        #[cfg(all(windows, feature = "vst"))]
        let mut picked_vst: Option<usize> = None;

        let title = match self.plugin_picker_target {
            PluginPickerTarget::MainInternal => "Add internal plugin",
            PluginPickerTarget::Ch1Vst => "Add plugin (channel 1)",
            PluginPickerTarget::Ch2Vst => "Add plugin (channel 2)",
        };
        egui::Window::new(title)
            .open(&mut open)
            .resizable(true)
            .show(ctx, |ui| {
                match self.plugin_picker_target {
                    PluginPickerTarget::MainInternal => {
                        ui.label("Built-in analyzers on the Main bus");
                        for (i, plugin) in self.local_analyzers.iter().enumerate() {
                            let suffix = if plugin.enabled { " (in chain)" } else { "" };
                            if ui
                                .selectable_label(false, format!("{}{}", plugin.name, suffix))
                                .on_hover_text(&plugin.description)
                                .clicked()
                            {
                                picked_internal = Some(i);
                            }
                        }
                    }
                    #[cfg(all(windows, feature = "vst"))]
                    PluginPickerTarget::Ch1Vst | PluginPickerTarget::Ch2Vst => {
                        if self.vst.catalog.is_empty()
                            && self.vst.scan_error.is_none()
                            && self.vst.scan_summary.is_none()
                        {
                            self.vst.scan_catalog();
                        }
                        ui.label(self.vst.scan_summary.clone().unwrap_or_else(|| {
                            "Scanning known VST locations on first open.".to_string()
                        }));
                        if let Some(err) = &self.vst.scan_error {
                            ui.colored_label(egui::Color32::RED, err);
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("channel_strip_plugin_picker")
                            .max_height(260.0)
                            .show(ui, |ui| {
                                for (i, plugin) in self.vst.catalog.iter().enumerate() {
                                    let label =
                                        format!("[{}] {}", plugin.format_label(), plugin.name());
                                    if ui
                                        .selectable_label(false, label)
                                        .on_hover_text(plugin.path().display().to_string())
                                        .clicked()
                                    {
                                        picked_vst = Some(i);
                                    }
                                }
                            });
                    }
                    #[cfg(not(all(windows, feature = "vst")))]
                    PluginPickerTarget::Ch1Vst | PluginPickerTarget::Ch2Vst => {
                        ui.label("VST: rebuild with --features vst.");
                    }
                }
            });
        self.plugin_picker_open = open;

        if let Some(i) = picked_internal {
            if let Some(plugin) = self.local_analyzers.get_mut(i) {
                plugin.enabled = true;
                self.status = format!("Added internal plugin: {}", plugin.name);
                self.analysis_events.clear();
                self.transcript_lines.clear();
                self.restart_live_input_stream();
                self.plugin_picker_open = false;
                self.plugin_picker_target = PluginPickerTarget::MainInternal;
            }
        }

        #[cfg(all(windows, feature = "vst"))]
        if let Some(i) = picked_vst {
            match self.plugin_picker_target {
                PluginPickerTarget::Ch1Vst => {
                    self.vst.catalog_pick = i;
                    match self.vst.add_pick_to_chain() {
                        Ok(()) => {
                            self.status = "Plugin added to channel 1.".to_string();
                            self.restart_live_input_stream();
                            self.plugin_picker_open = false;
                        }
                        Err(e) => self.status = format!("Add plugin: {e}"),
                    }
                }
                PluginPickerTarget::Ch2Vst => {
                    self.vst_ch2.catalog_pick = i;
                    match self.vst_ch2.add_pick_to_chain() {
                        Ok(()) => {
                            self.status = "Plugin added to channel 2.".to_string();
                            self.restart_live_input_stream();
                            self.plugin_picker_open = false;
                        }
                        Err(e) => self.status = format!("Add plugin (Ch2): {e}"),
                    }
                }
                PluginPickerTarget::MainInternal => {}
            }
        }
    }

    /// Parakeet NeMo sidecar tuning: chunk length, silence gate, paths, model id.
    fn draw_parakeet_properties_window(&mut self, ctx: &egui::Context) {
        if !self.parakeet_props_open {
            return;
        }
        let mut open = self.parakeet_props_open;
        egui::Window::new("Parakeet properties")
            .open(&mut open)
            .resizable(true)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.label(
                    "NeMo sidecar and chunking. Apply commits settings, saves with the rest of the demo app, and restarts the live analyzer stream.",
                );
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.label("Chunk length (s):");
                    ui.add(
                        egui::DragValue::new(&mut self.parakeet_props_draft.chunk_seconds)
                            .speed(0.05)
                            .range(0.25..=60.0),
                    );
                });
                ui.small("Larger chunks often improve accuracy; smaller chunks reduce latency.");

                ui.horizontal(|ui| {
                    ui.label("Silence RMS threshold:");
                    ui.add(
                        egui::DragValue::new(&mut self.parakeet_props_draft.silence_rms_threshold)
                            .speed(0.0005)
                            .range(0.0..=1.0),
                    );
                });
                ui.small("Chunks below this RMS skip the model. Lower if quiet speech is dropped; raise if noise becomes transcript.");

                ui.horizontal(|ui| {
                    ui.label("Pre-model buffer (s):");
                    ui.add(
                        egui::DragValue::new(&mut self.parakeet_props_draft.pre_ready_buffer_seconds)
                            .speed(0.5)
                            .range(1.0..=120.0),
                    );
                });
                ui.small("Audio kept while the model loads before the first transcript.");

                ui.horizontal(|ui| {
                    ui.label("Sample rate (Hz):");
                    ui.add(
                        egui::DragValue::new(&mut self.parakeet_props_draft.sample_rate_hz)
                            .speed(500)
                            .range(8000..=48_000),
                    );
                });
                ui.small("Parakeet is trained for 16 kHz mono; leave at 16000 unless you know otherwise.");

                ui.horizontal(|ui| {
                    ui.label("Model name:");
                    ui.text_edit_singleline(&mut self.parakeet_props_draft.model_name);
                });

                ui.horizontal(|ui| {
                    ui.label("Python:");
                    ui.text_edit_singleline(&mut self.parakeet_props_draft.python);
                });

                ui.horizontal(|ui| {
                    ui.label("Worker script:");
                    let mut path_str = self
                        .parakeet_props_draft
                        .worker_script
                        .display()
                        .to_string();
                    if ui.text_edit_singleline(&mut path_str).changed() {
                        self.parakeet_props_draft.worker_script = PathBuf::from(path_str);
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Stream id:");
                    let mut sid = self
                        .parakeet_props_draft
                        .stream_id
                        .clone()
                        .unwrap_or_default();
                    if ui.text_edit_singleline(&mut sid).changed() {
                        self.parakeet_props_draft.stream_id = if sid.is_empty() {
                            None
                        } else {
                            Some(sid)
                        };
                    }
                });

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Reset to defaults").clicked() {
                        self.parakeet_props_draft = parakeet_plugin::ParakeetConfig::default();
                    }
                    if ui.button("Apply").clicked() {
                        self.parakeet_config = self.parakeet_props_draft.clone();
                        self.status = "Parakeet settings applied.".to_string();
                        self.analysis_events.clear();
                        self.transcript_lines.clear();
                        self.restart_live_input_stream();
                    }
                });
            });
        self.parakeet_props_open = open;
    }

    fn draw_channel_strip(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.draw_plugin_picker(ctx);
        self.draw_parakeet_properties_window(ctx);

        egui::Frame::none()
            .fill(egui::Color32::from_rgb(30, 32, 32))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(55)))
            .inner_margin(egui::Margin::symmetric(6.0, 8.0))
            .show(ui, |ui| {
                let col_w = CHANNEL_STRIP_COL_WIDTH;
                ui.set_min_height(ui.available_height());

                #[allow(dead_code)]
                enum StripAction {
                    ToggleEditor1(usize),
                    ToggleEditor2(usize),
                    RemoveVst1(usize),
                    RemoveVst2(usize),
                    ToggleBypass1(usize),
                    ToggleBypass2(usize),
                    MoveVst1 { from: usize, to: usize },
                    MoveVst2 { from: usize, to: usize },
                    RemoveInternal(usize),
                    MoveInternal { from: usize, to: usize },
                    OpenInternalProperties(usize),
                }
                let mut pending: Option<StripAction> = None;

                ui.horizontal(|ui| {
                    // ---- Channel 1 (mic) ----
                    ui.vertical(|ui| {
                        ui.set_min_width(col_w);
                        ui.set_max_width(col_w);
                        ui.vertical_centered(|ui| {
                            ui.strong("Channel 1");
                            ui.small("Microphone + VST");
                        });
                        ui.add_space(6.0);
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(22, 24, 24))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(48)))
                            .inner_margin(egui::Margin::same(6.0))
                            .show(ui, |ui| {
                                ui.label("VST / FX");
                                egui::ScrollArea::vertical()
                                    .id_salt("ch1_fx")
                                    .max_height(CHANNEL_STRIP_FX_SCROLL_MAX_HEIGHT)
                                    .show(ui, |ui| {
                                        #[cfg(all(windows, feature = "vst"))]
                                        {
                                            let len = self.vst.chain.len();
                                            for i in 0..len {
                                                let Some(entry) = self.vst.chain.get(i) else {
                                                    continue;
                                                };
                                                let label = format!(
                                                    "{} {}",
                                                    entry.catalog.format_label(),
                                                    entry.catalog.name()
                                                );
                                                let fill = if entry.bypassed {
                                                    egui::Color32::from_rgb(86, 75, 34)
                                                } else if self
                                                    .vst
                                                    .editor_open
                                                    .get(i)
                                                    .copied()
                                                    .unwrap_or(false)
                                                {
                                                    egui::Color32::from_rgb(52, 92, 82)
                                                } else {
                                                    egui::Color32::from_rgb(58, 64, 64)
                                                };
                                                let response = ui.add_sized(
                                                    [ui.available_width(), 26.0],
                                                    egui::Button::new(label)
                                                        .fill(fill)
                                                        .sense(egui::Sense::click_and_drag()),
                                                );
                                                if response.drag_started() {
                                                    self.dragged_chain_index = Some(i);
                                                }
                                                if response.hovered() {
                                                    if let Some(from) = self.dragged_chain_index {
                                                        let primary_down = ui
                                                            .input(|input| input.pointer.primary_down());
                                                        if primary_down && from != i {
                                                            pending = Some(StripAction::MoveVst1 {
                                                                from,
                                                                to: i,
                                                            });
                                                        }
                                                    }
                                                }
                                                if response.drag_stopped() {
                                                    self.dragged_chain_index = None;
                                                }
                                                if response.clicked() {
                                                    let modifiers =
                                                        ui.input(|input| input.modifiers);
                                                    if modifiers.alt {
                                                        pending = Some(StripAction::RemoveVst1(i));
                                                    } else if modifiers.shift {
                                                        pending = Some(StripAction::ToggleBypass1(i));
                                                    } else {
                                                        pending = Some(StripAction::ToggleEditor1(i));
                                                    }
                                                }
                                            }
                                        }
                                        let empty_rows = 6usize.saturating_sub({
                                            #[cfg(all(windows, feature = "vst"))]
                                            {
                                                self.vst.chain.len()
                                            }
                                            #[cfg(not(all(windows, feature = "vst")))]
                                            {
                                                0usize
                                            }
                                        });
                                        for _ in 0..empty_rows.max(1) {
                                            let response = ui.add_sized(
                                                [ui.available_width(), 26.0],
                                                egui::Button::new("+ add VST")
                                                    .fill(egui::Color32::from_rgb(38, 40, 40)),
                                            );
                                            if response.clicked() {
                                                self.plugin_picker_target = PluginPickerTarget::Ch1Vst;
                                                self.plugin_picker_open = true;
                                            }
                                        }
                                    });
                            });
                        ui.add_space(6.0);
                        let h = CHANNEL_STRIP_SLIDER_HEIGHT;
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(22, 24, 24))
                            .inner_margin(egui::Margin::same(6.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    Self::draw_log_meter(ui, self.meter_ch1.level(), h);
                                    ui.add_space(6.0);
                                    let old = ui.spacing().slider_width;
                                    ui.spacing_mut().slider_width = h;
                                    let g = ui
                                        .add(
                                            egui::Slider::new(&mut self.channel_gain, 0.0..=4.0)
                                                .vertical()
                                                .text("gain"),
                                        )
                                        .changed();
                                    ui.spacing_mut().slider_width = old;
                                    if g {
                                        self.restart_live_input_stream();
                                    }
                                });
                            });
                        ui.add_space(6.0);
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(42, 48, 48))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(65)))
                            .inner_margin(egui::Margin::same(6.0))
                            .show(ui, |ui| {
                                let w = ui.available_width();
                                ui.allocate_ui_with_layout(
                                    egui::vec2(w, CHANNEL_STRIP_LIVE_SECTION_HEIGHT),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        ui.label("Live · mic");
                                        let mut device_changed = false;
                                        let selected_device = self
                                            .device_index
                                            .and_then(|i| self.devices.get(i).map(|d| d.name.as_str()))
                                            .unwrap_or("(none)");
                                        egui::ComboBox::from_id_salt("strip_device_ch1")
                                            .width(ui.available_width())
                                            .selected_text(selected_device)
                                            .show_ui(ui, |ui| {
                                                for (i, device) in self.devices.iter().enumerate() {
                                                    if ui
                                                        .selectable_label(
                                                            self.device_index == Some(i),
                                                            &device.name,
                                                        )
                                                        .clicked()
                                                    {
                                                        self.device_index = Some(i);
                                                        device_changed = true;
                                                    }
                                                }
                                            });
                                        if device_changed {
                                            let channels = self.selected_device_channel_count();
                                            self.input_channel = self
                                                .input_channel
                                                .min(channels.saturating_sub(1));
                                            self.restart_live_input_stream();
                                        }
                                        let channels = self.selected_device_channel_count();
                                        egui::ComboBox::from_id_salt("strip_input_ch1")
                                            .width(ui.available_width())
                                            .selected_text(format!("Input {}", self.input_channel + 1))
                                            .show_ui(ui, |ui| {
                                                for channel in 0..channels {
                                                    if ui
                                                        .selectable_label(
                                                            self.input_channel == channel,
                                                            format!("Input {}", channel + 1),
                                                        )
                                                        .clicked()
                                                    {
                                                        self.input_channel = channel;
                                                        self.restart_live_input_stream();
                                                    }
                                                }
                                            });
                                    },
                                );
                            });
                    });

                    ui.separator();

                    // ---- Channel 2 (loopback) ----
                    ui.vertical(|ui| {
                        ui.set_min_width(col_w);
                        ui.set_max_width(col_w);
                        ui.vertical_centered(|ui| {
                            ui.strong("Channel 2");
                            ui.small("Speaker / loopback + VST");
                        });
                        ui.add_space(6.0);
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(22, 24, 24))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(48)))
                            .inner_margin(egui::Margin::same(6.0))
                            .show(ui, |ui| {
                                ui.label("VST / FX");
                                egui::ScrollArea::vertical()
                                    .id_salt("ch2_fx")
                                    .max_height(CHANNEL_STRIP_FX_SCROLL_MAX_HEIGHT)
                                    .show(ui, |ui| {
                                        #[cfg(all(windows, feature = "vst"))]
                                        {
                                            let len = self.vst_ch2.chain.len();
                                            for i in 0..len {
                                                let Some(entry) = self.vst_ch2.chain.get(i) else {
                                                    continue;
                                                };
                                                let label = format!(
                                                    "{} {}",
                                                    entry.catalog.format_label(),
                                                    entry.catalog.name()
                                                );
                                                let fill = if entry.bypassed {
                                                    egui::Color32::from_rgb(86, 75, 34)
                                                } else if self
                                                    .vst_ch2
                                                    .editor_open
                                                    .get(i)
                                                    .copied()
                                                    .unwrap_or(false)
                                                {
                                                    egui::Color32::from_rgb(52, 92, 82)
                                                } else {
                                                    egui::Color32::from_rgb(58, 64, 64)
                                                };
                                                let response = ui.add_sized(
                                                    [ui.available_width(), 26.0],
                                                    egui::Button::new(label)
                                                        .fill(fill)
                                                        .sense(egui::Sense::click_and_drag()),
                                                );
                                                if response.drag_started() {
                                                    self.dragged_chain_index_ch2 = Some(i);
                                                }
                                                if response.hovered() {
                                                    if let Some(from) = self.dragged_chain_index_ch2 {
                                                        let primary_down = ui
                                                            .input(|input| input.pointer.primary_down());
                                                        if primary_down && from != i {
                                                            pending = Some(StripAction::MoveVst2 {
                                                                from,
                                                                to: i,
                                                            });
                                                        }
                                                    }
                                                }
                                                if response.drag_stopped() {
                                                    self.dragged_chain_index_ch2 = None;
                                                }
                                                if response.clicked() {
                                                    let modifiers =
                                                        ui.input(|input| input.modifiers);
                                                    if modifiers.alt {
                                                        pending = Some(StripAction::RemoveVst2(i));
                                                    } else if modifiers.shift {
                                                        pending = Some(StripAction::ToggleBypass2(i));
                                                    } else {
                                                        pending = Some(StripAction::ToggleEditor2(i));
                                                    }
                                                }
                                            }
                                        }
                                        let empty_rows = 6usize.saturating_sub({
                                            #[cfg(all(windows, feature = "vst"))]
                                            {
                                                self.vst_ch2.chain.len()
                                            }
                                            #[cfg(not(all(windows, feature = "vst")))]
                                            {
                                                0usize
                                            }
                                        });
                                        for _ in 0..empty_rows.max(1) {
                                            let response = ui.add_sized(
                                                [ui.available_width(), 26.0],
                                                egui::Button::new("+ add VST")
                                                    .fill(egui::Color32::from_rgb(38, 40, 40)),
                                            );
                                            if response.clicked() {
                                                self.plugin_picker_target = PluginPickerTarget::Ch2Vst;
                                                self.plugin_picker_open = true;
                                            }
                                        }
                                    });
                            });
                        ui.add_space(6.0);
                        let h = CHANNEL_STRIP_SLIDER_HEIGHT;
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(22, 24, 24))
                            .inner_margin(egui::Margin::same(6.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    Self::draw_log_meter(ui, self.meter_ch2.level(), h);
                                    ui.add_space(6.0);
                                    let old = ui.spacing().slider_width;
                                    ui.spacing_mut().slider_width = h;
                                    let g = ui
                                        .add(
                                            egui::Slider::new(&mut self.channel2_gain, 0.0..=4.0)
                                                .vertical()
                                                .text("gain"),
                                        )
                                        .changed();
                                    ui.spacing_mut().slider_width = old;
                                    if g {
                                        self.restart_live_input_stream();
                                    }
                                });
                            });
                        ui.add_space(6.0);
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(42, 48, 48))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(65)))
                            .inner_margin(egui::Margin::same(6.0))
                            .show(ui, |ui| {
                                let w = ui.available_width();
                                ui.allocate_ui_with_layout(
                                    egui::vec2(w, CHANNEL_STRIP_LIVE_SECTION_HEIGHT),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        ui.label("Live · speaker");
                                        let loop_ok = self.loopback_supported();
                                        let selected_text = self
                                            .speaker_index
                                            .and_then(|i| self.speaker_sources.get(i))
                                            .map(|s| s.name.as_str())
                                            .unwrap_or("(off — pick to enable Ch2)");
                                        egui::ComboBox::from_id_salt("strip_speaker_ch2")
                                            .width(ui.available_width())
                                            .selected_text(selected_text)
                                            .show_ui(ui, |ui| {
                                                let mut picked: Option<Option<usize>> = None;
                                                if ui
                                                    .selectable_label(self.speaker_index.is_none(), "(off)")
                                                    .clicked()
                                                {
                                                    picked = Some(None);
                                                }
                                                if loop_ok {
                                                    for (i, source) in self.speaker_sources.iter().enumerate() {
                                                        if ui
                                                            .selectable_label(
                                                                self.speaker_index == Some(i),
                                                                &source.name,
                                                            )
                                                            .clicked()
                                                        {
                                                            picked = Some(Some(i));
                                                        }
                                                    }
                                                }
                                                if let Some(p) = picked {
                                                    self.speaker_index = p;
                                                    if self.speaker_index.is_some()
                                                        && matches!(
                                                            self.speaker_mode,
                                                            SpeakerMode::Off | SpeakerMode::Separate
                                                        )
                                                    {
                                                        self.speaker_mode = SpeakerMode::Mixed;
                                                    }
                                                    self.restart_live_input_stream();
                                                }
                                            });
                                        if !loop_ok {
                                            ui.small(self.loopback_unavailable_hint());
                                        } else {
                                            ui.small(
                                                "Captures system playback. Analyzers use Main (Ch1+Ch2) when live.",
                                            );
                                        }
                                        let ch2_ch = self.selected_speaker_channel_count();
                                        self.channel2_input_channel = self
                                            .channel2_input_channel
                                            .min(ch2_ch.saturating_sub(1));
                                        egui::ComboBox::from_id_salt("strip_input_ch2")
                                            .width(ui.available_width())
                                            .selected_text(format!(
                                                "Input {}",
                                                self.channel2_input_channel + 1
                                            ))
                                            .show_ui(ui, |ui| {
                                                for channel in 0..ch2_ch {
                                                    if ui
                                                        .selectable_label(
                                                            self.channel2_input_channel == channel,
                                                            format!("Input {}", channel + 1),
                                                        )
                                                        .clicked()
                                                    {
                                                        self.channel2_input_channel = channel;
                                                        self.restart_live_input_stream();
                                                    }
                                                }
                                            });
                                    },
                                );
                            });
                    });

                    ui.separator();

                    // ---- Main (analyzers + live) ----
                    ui.vertical(|ui| {
                        ui.set_min_width(col_w);
                        ui.set_max_width(col_w);
                        ui.vertical_centered(|ui| {
                            ui.strong("Main");
                            ui.small("Analyzers · level after Ch1+Ch2 mix");
                        });
                        ui.add_space(6.0);
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(22, 24, 24))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(48)))
                            .inner_margin(egui::Margin::same(6.0))
                            .show(ui, |ui| {
                                ui.label("Internal plugins");
                                egui::ScrollArea::vertical()
                                    .id_salt("main_internal")
                                    .max_height(CHANNEL_STRIP_FX_SCROLL_MAX_HEIGHT)
                                    .show(ui, |ui| {
                                        for i in 0..self.local_analyzers.len() {
                                            let Some(plugin) = self.local_analyzers.get(i) else {
                                                continue;
                                            };
                                            if !plugin.enabled {
                                                continue;
                                            }
                                            let hover = if plugin.id
                                                == parakeet_plugin::PARAKEET_PLUGIN_ID
                                            {
                                                "Click: Parakeet settings · Alt+click: remove"
                                            } else {
                                                "Alt+click: remove"
                                            };
                                            let response = ui
                                                .add_sized(
                                                    [ui.available_width(), 26.0],
                                                    egui::Button::new(format!("INT {}", plugin.name))
                                                        .fill(egui::Color32::from_rgb(45, 66, 82))
                                                        .sense(egui::Sense::click_and_drag()),
                                                )
                                                .on_hover_text(hover);
                                            if response.drag_started() {
                                                self.dragged_internal_index = Some(i);
                                            }
                                            if response.hovered() {
                                                if let Some(from) = self.dragged_internal_index {
                                                    let primary_down = ui
                                                        .input(|input| input.pointer.primary_down());
                                                    if primary_down && from != i {
                                                        pending = Some(StripAction::MoveInternal {
                                                            from,
                                                            to: i,
                                                        });
                                                    }
                                                }
                                            }
                                            if response.drag_stopped() {
                                                self.dragged_internal_index = None;
                                            }
                                            if response.clicked() {
                                                let modifiers =
                                                    ui.input(|input| input.modifiers);
                                                if modifiers.alt {
                                                    pending = Some(StripAction::RemoveInternal(i));
                                                } else {
                                                    pending =
                                                        Some(StripAction::OpenInternalProperties(i));
                                                }
                                            }
                                        }
                                        let empty_rows = 6usize.saturating_sub(
                                            self.local_analyzers.iter().filter(|p| p.enabled).count(),
                                        );
                                        for _ in 0..empty_rows.max(1) {
                                            let response = ui.add_sized(
                                                [ui.available_width(), 26.0],
                                                egui::Button::new("+ add internal")
                                                    .fill(egui::Color32::from_rgb(38, 40, 40)),
                                            );
                                            if response.clicked() {
                                                self.plugin_picker_target =
                                                    PluginPickerTarget::MainInternal;
                                                self.plugin_picker_open = true;
                                            }
                                        }
                                    });
                            });
                        ui.add_space(6.0);
                        let h = CHANNEL_STRIP_SLIDER_HEIGHT;
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(22, 24, 24))
                            .inner_margin(egui::Margin::same(6.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    Self::draw_log_meter(ui, self.meter_main.level(), h);
                                    ui.add_space(6.0);
                                    let (_, slot) = ui.allocate_exact_size(
                                        egui::vec2(h, h),
                                        egui::Sense::hover(),
                                    );
                                    slot.on_hover_text(
                                        "Main level after Ch1+Ch2 gains and mix (read-only meter).",
                                    );
                                });
                            });
                        ui.add_space(6.0);
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(42, 48, 48))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(65)))
                            .inner_margin(egui::Margin::same(6.0))
                            .show(ui, |ui| {
                                let w = ui.available_width();
                                ui.allocate_ui_with_layout(
                                    egui::vec2(w, CHANNEL_STRIP_LIVE_SECTION_HEIGHT),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        ui.label("Live · monitoring");
                                        let live_response = ui.add_sized(
                                            [ui.available_width(), 30.0],
                                            egui::Button::new(if self.live_enabled {
                                                "LIVE"
                                            } else {
                                                "live"
                                            })
                                            .fill(if self.live_enabled {
                                                egui::Color32::from_rgb(190, 20, 70)
                                            } else {
                                                egui::Color32::from_rgb(55, 55, 55)
                                            }),
                                        );
                                        if live_response.clicked() {
                                            self.live_enabled = !self.live_enabled;
                                            if self.live_enabled {
                                                self.restart_live_input_stream();
                                            } else {
                                                self.stop_live_input_stream();
                                            }
                                        }
                                        ui.small(if self.ch2_selected() && self.loopback_supported() {
                                            "With Ch2: analyzers see the mixed Main bus."
                                        } else {
                                            "Ch2 off: analyzers follow the mic path only."
                                        });
                                    },
                                );
                            });
                    });
                });

                match pending {
                    #[cfg(all(windows, feature = "vst"))]
                    Some(StripAction::ToggleEditor1(i)) => self.toggle_vst_editor_from_strip(i),
                    #[cfg(all(windows, feature = "vst"))]
                    Some(StripAction::ToggleEditor2(i)) => self.toggle_vst_editor_from_strip_ch2(i),
                    #[cfg(all(windows, feature = "vst"))]
                    Some(StripAction::RemoveVst1(i)) => {
                        self.vst.remove_chain(i);
                        self.restart_live_input_stream();
                    }
                    #[cfg(all(windows, feature = "vst"))]
                    Some(StripAction::RemoveVst2(i)) => {
                        self.vst_ch2.remove_chain(i);
                        self.restart_live_input_stream();
                    }
                    #[cfg(all(windows, feature = "vst"))]
                    Some(StripAction::ToggleBypass1(i)) => {
                        self.vst.toggle_bypass(i);
                        self.restart_live_input_stream();
                    }
                    #[cfg(all(windows, feature = "vst"))]
                    Some(StripAction::ToggleBypass2(i)) => {
                        self.vst_ch2.toggle_bypass(i);
                        self.restart_live_input_stream();
                    }
                    #[cfg(all(windows, feature = "vst"))]
                    Some(StripAction::MoveVst1 { from, to }) => {
                        self.vst.move_chain_to(from, to);
                        self.dragged_chain_index = Some(to);
                        self.restart_live_input_stream();
                    }
                    #[cfg(all(windows, feature = "vst"))]
                    Some(StripAction::MoveVst2 { from, to }) => {
                        self.vst_ch2.move_chain_to(from, to);
                        self.dragged_chain_index_ch2 = Some(to);
                        self.restart_live_input_stream();
                    }
                    Some(StripAction::RemoveInternal(i)) => {
                        if let Some(plugin) = self.local_analyzers.get_mut(i) {
                            plugin.enabled = false;
                            self.analysis_events.clear();
                            self.transcript_lines.clear();
                            self.restart_live_input_stream();
                        }
                    }
                    Some(StripAction::MoveInternal { from, to }) => {
                        self.move_internal_plugin_to(from, to);
                        self.dragged_internal_index = Some(to);
                    }
                    Some(StripAction::OpenInternalProperties(i)) => {
                        if let Some(plugin) = self.local_analyzers.get(i) {
                            if plugin.id == parakeet_plugin::PARAKEET_PLUGIN_ID {
                                self.parakeet_props_draft = self.parakeet_config.clone();
                                self.parakeet_props_open = true;
                            } else {
                                self.status = format!(
                                    "{} has no properties panel (only Parakeet does today).",
                                    plugin.name
                                );
                            }
                        }
                    }
                    _ => {}
                }

                if ui.ctx().input(|input| input.pointer.any_released()) {
                    self.dragged_chain_index = None;
                    self.dragged_chain_index_ch2 = None;
                    self.dragged_internal_index = None;
                }
            });
    }

    fn draw_right_side(&mut self, ui: &mut egui::Ui) {
        ui.set_max_width(820.0);
        ui.heading("Recorder demo");
        ui.label(&self.status);
        ui.add_space(8.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Audio Device");
            #[cfg(windows)]
            {
                ui.horizontal(|ui| {
                    ui.label("Audio system:");
                    egui::ComboBox::from_id_salt("audio_system")
                        .selected_text(self.audio_system.label())
                        .show_ui(ui, |ui| {
                            for &sys in recorder_host_windows::WindowsAudioSystem::ALL {
                                if ui
                                    .selectable_label(self.audio_system == sys, sys.label())
                                    .clicked()
                                {
                                    self.set_audio_system(sys);
                                }
                            }
                        });
                    if ui.button("Refresh devices").clicked() {
                        self.refresh_devices();
                    }
                });
            }

            if let Some(i) = self.device_index {
                if let Some(device) = self.devices.get(i) {
                    if let Some(af) = device.default_format {
                        ui.label(format!(
                            "Selected source: {} ({} Hz, {} ch, {:?})",
                            device.name, af.sample_rate_hz, af.channels, af.sample_format
                        ));
                    }
                }
            } else {
                ui.label("No input source selected.");
            }
        });

        ui.add_space(8.0);
        #[cfg(all(windows, feature = "vst"))]
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Plugin Scan");
            if let Some(err) = &self.vst.scan_error {
                ui.colored_label(egui::Color32::RED, err);
            } else if let Some(summary) = &self.vst.scan_summary {
                ui.label(summary);
            } else {
                ui.label("Open an empty FX row to scan and add external plugins.");
            }
            if self.vst.scan_details.is_some() {
                if ui
                    .small_button(if self.vst.show_scan_details {
                        "Hide scan details"
                    } else {
                        "Show scan details"
                    })
                    .clicked()
                {
                    self.vst.show_scan_details = !self.vst.show_scan_details;
                }
                if self.vst.show_scan_details {
                    if let Some(d) = &self.vst.scan_details {
                        egui::ScrollArea::vertical()
                            .id_salt("vst_scan_details_scroll")
                            .max_height(120.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut d.as_str())
                                        .font(egui::TextStyle::Monospace)
                                        .desired_rows(6)
                                        .desired_width(f32::INFINITY),
                                );
                            });
                    }
                }
            }
        });

        #[cfg(all(windows, not(feature = "vst")))]
        ui.label("VST: rebuild with --features vst to enable external plugin hosting.");

        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Recording");
            ui.label("Output file (extension added if missing):");
            ui.text_edit_singleline(&mut self.path);
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.record_pre, "Record pre-processed");
                ui.checkbox(&mut self.record_post, "Record post-processed");
            });

            // Speaker / loopback is configured on channel 2 in the strip. When a loopback
            // source is selected, Main post uses a mixer; these radios pick the mix shape.
            let loopback_supported = self.loopback_supported();
            if self.ch2_selected() && loopback_supported {
                ui.label("Main bus mix (after Ch1 + Ch2 gains):");
                ui.horizontal_wrapped(|ui| {
                    ui.radio_value(&mut self.speaker_mode, SpeakerMode::Mixed, "Sum (stereo)");
                    ui.radio_value(
                        &mut self.speaker_mode,
                        SpeakerMode::StereoSplit,
                        "Split (mic L / speaker R)",
                    );
                });
                ui.small("Pre files: channel 1 + channel 2 raw; post file: Main mix. Privacy: see strip.");
            } else if !loopback_supported {
                ui.small(self.loopback_unavailable_hint());
            }

            ui.horizontal_wrapped(|ui| {
                ui.label("Format:");
                for format in [OutFormat::Wav, OutFormat::Flac, OutFormat::Mp3] {
                    ui.radio_value(&mut self.format, format, format.label());
                }
            });

            // Live privacy banner while a loopback stream is actually capturing audio.
            if self
                .recording
                .as_ref()
                .map(|r| r.speaker_active)
                .unwrap_or(false)
            {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 64, 30),
                    "● Speaker output is being recorded.",
                );
            }

            ui.horizontal_wrapped(|ui| {
                let rec_active = self.recording.is_some();
                if ui
                    .add_enabled(!rec_active, egui::Button::new("Record"))
                    .clicked()
                {
                    self.start_recording();
                }
                let pause_label = self
                    .recording
                    .as_ref()
                    .map(|r| {
                        if r.paused.load(Ordering::Acquire) {
                            "Resume"
                        } else {
                            "Pause"
                        }
                    })
                    .unwrap_or("Pause");
                if ui
                    .add_enabled(rec_active, egui::Button::new(pause_label))
                    .clicked()
                {
                    self.toggle_pause();
                }
                if ui
                    .add_enabled(rec_active, egui::Button::new("Stop"))
                    .clicked()
                {
                    self.stop_recording();
                }
                if ui
                    .add_enabled(self.last_file.is_some(), egui::Button::new("Play last"))
                    .clicked()
                {
                    self.play_last();
                }
            });
        });

        ui.add_space(8.0);
        let output_height = ui.available_height().max(260.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), output_height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(output_height);
                    egui::CollapsingHeader::new("Internal plugin output")
                        .default_open(self.local_analyzers.iter().any(|plugin| plugin.enabled))
                        .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Clear output").clicked() {
                            self.analysis_events.clear();
                            self.transcript_lines.clear();
                        }
                        ui.label("Internal analyzers are added from the channel strip.");
                    });
                    if self.local_analyzers.iter().any(|plugin| plugin.enabled) {
                        ui.label(if self.live_input.is_some() || self.recording.is_some() {
                            "Analyzer stream is active. Parakeet may take a while to load before the first transcript."
                        } else {
                            "Analyzer selected, but no stream is active. Turn LIVE on or start recording."
                        });
                    } else {
                        ui.label("No internal analyzer plugin is selected.");
                    }
                    ui.separator();
                    ui.label("Analyzer status:");
                    if self.analysis_events.is_empty() {
                        ui.label("(no analyzer status yet)");
                    } else {
                        let status_height = (ui.available_height() * 0.35).clamp(90.0, 180.0);
                        egui::ScrollArea::vertical()
                            .id_salt("analysis_events_scroll")
                            .max_height(status_height)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                for event in &self.analysis_events {
                                    ui.label(event);
                                }
                            });
                    }
                    ui.separator();
                    ui.label("Live transcription:");
                    if self.transcript_lines.is_empty() {
                        ui.label("(no transcript yet)");
                    } else {
                        let transcript_height = ui.available_height().max(120.0);
                        egui::ScrollArea::vertical()
                            .id_salt("transcript_scroll")
                            .max_height(transcript_height)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                for line in &self.transcript_lines {
                                    ui.label(line);
                                }
                            });
                    }
                });
                });
            },
        );
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl Drop for RecorderApp {
    fn drop(&mut self) {
        self.stop_live_input_stream();
        if let Some(rec) = self.recording.take() {
            for capture in rec.captures {
                capture.stop();
            }
            if let Some(mixer) = rec.mixer {
                mixer.stop();
            }
        }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl eframe::App for RecorderApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.persisted_settings());
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_media_events();
        if let Some(h) = &self.playback {
            if h.is_finished() {
                self.playback = None;
                self.status = "Playback finished.".to_string();
            }
        }

        egui::SidePanel::left("channel_strip_panel")
            .resizable(false)
            .exact_width(CHANNEL_STRIP_PANEL_WIDTH)
            .show(ctx, |ui| {
                self.draw_channel_strip(ui, ctx);
            });

        egui::CentralPanel::default().show(ctx, |ui| self.draw_right_side(ui));
        #[cfg(all(windows, feature = "vst"))]
        {
            self.vst.draw_native_editor_ui(ctx);
            self.vst_ch2.draw_native_editor_ui(ctx);
            if self.vst.editor_open.iter().any(|&o| o)
                || self.vst_ch2.editor_open.iter().any(|&o| o)
            {
                ctx.request_repaint();
            } else {
                let delay = if self.recording.is_some() || self.live_input.is_some() {
                    std::time::Duration::from_millis(33)
                } else {
                    std::time::Duration::from_millis(250)
                };
                ctx.request_repaint_after(delay);
            }
        }
        #[cfg(not(all(windows, feature = "vst")))]
        {
            let delay = if self.recording.is_some() || self.live_input.is_some() {
                std::time::Duration::from_millis(33)
            } else {
                std::time::Duration::from_millis(250)
            };
            ctx.request_repaint_after(delay);
        }
    }
}
