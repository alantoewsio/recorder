# Recorder

Rust workspace for multi-source audio capture: a portable **recorder-core** pipeline (WAV / FLAC / MP3 sinks), per-OS host crates, optional C ABI plugins, and a small **recorder-ui** demo.

See [ARCHITECTURE.md](ARCHITECTURE.md) for crate layout and data flow.

## Quick start

```bash
cargo run -p recorder-ui
```

On **Windows**, choose an **Audio system** (WASAPI, ASIO if built with `--features asio`, DirectSound, WaveOut / WinMM waveIn, or Dummy), then an input device for that stack. Set the output path and format (WAV, FLAC, or MP3), then record, pause, or stop. Playback of the last file uses **rodio** and **Symphonia**.

### Recording speaker output (loopback)

The recorder can capture system audio output alongside (or instead of) the microphone. In the demo UI, choose a **Speaker output** mode:

- **Off** — mic-only (the default).
- **Separate files** — mic and speaker each go to their own files (`*_mic_*.wav` / `*_speaker_*.wav`), pre/post stages preserved per source.
- **Stereo split** — single stereo file: mic on the left channel, speaker on the right.
- **Mixed** — single stereo file with both signals summed and soft-limited.

Per-OS loopback support:

| OS | Source | Setup |
|----|--------|-------|
| Windows | **WASAPI loopback** | Built-in. Loopback capture is only available with the WASAPI audio system; the UI disables the mode for DirectSound, WaveOut, ASIO, and Dummy. |
| Linux | **PulseAudio / PipeWire monitor sources** | Built-in. Pick the `Monitor of <sink>` source matching your output device. |
| macOS | **ScreenCaptureKit** (macOS 13+) | Default-on via the `screencapturekit` feature on `recorder-host-macos`. First use prompts for **Screen Recording** permission in System Settings → Privacy & Security. Falls back to virtual drivers (BlackHole, Loopback, Soundflower, VB-Cable) when SCK is unavailable. |

The UI shows a **● Speaker output is being recorded** indicator while a loopback stream is live, and an inline reminder under the source picker that loopback captures everything the system is rendering — including conference calls and notifications. Recording with anyone else in earshot is your responsibility; mention it before you start, and check local recording-consent laws.

## Release Builds

This repo includes a GitHub Actions workflow at [`.github/workflows/release.yml`](.github/workflows/release.yml) that builds native release artifacts on the correct operating system runners:

- `recorder-ui-windows-x64.zip`
- `recorder-ui-macos-x64.tar.gz`
- `recorder-ui-linux-x64.tar.gz`
- `recorder-sdk-windows-x64.zip`
- `recorder-sdk-macos-x64.tar.gz`
- `recorder-sdk-linux-x64.tar.gz`

Pushing a `v*` tag (for example `v0.9.0`) runs this workflow and then **creates a [GitHub Release](https://github.com/alantoewsio/recorder/releases)** for that tag, with the zip/tar.gz files attached as downloadable assets (not only workflow artifacts).

Run it manually from GitHub Actions (`Release Builds` → `Run workflow`) on a branch to build artifacts without publishing a Release. To publish from an existing tag after workflow changes, either select that tag as the run ref if GitHub offers it, or move the tag (`git push origin :refs/tags/vX.Y.Z` then re-tag and push) or publish a new patch tag.

```bash
git tag v0.9.0
git push origin v0.9.0
```

The Windows artifact is built with `--features vst` and includes `recorder-ui.exe` plus `libmp3lame.dll` when present in `target/release`. macOS and Linux artifacts build the portable UI without the Windows-only VST feature.

Local Windows release build:

```powershell
cargo build -p recorder-ui --features vst --release
```

Local SDK binary-library build:

```powershell
pwsh scripts/build-recorder-sdk-release.ps1
```

On macOS/Linux:

```bash
bash scripts/build-recorder-sdk-release.sh
```

The SDK packages include:

- `include/recorder_sdk.h`
- `bin/recorder_sdk.dll` on Windows, `bin/librecorder_sdk.dylib` on macOS, or `bin/librecorder_sdk.so` on Linux
- static/import libraries in `lib/` when produced by the platform toolchain
- `bin/libmp3lame.dll` in the Windows SDK package for MP3 runtime support

Cross-building macOS/Linux release binaries from Windows is not currently supported by this workspace because the UI/audio stack depends on native SDKs and linkers (`pkg-config`, ALSA, Apple SDK/linker, etc.). Use the release workflow or build on the target OS.

## Using Recorder As A Library

If you want another application to use this project as a recording component, integrate the crates directly. Do **not** shell out to `recorder-ui.exe`, and do **not** treat the demo app's egui state as the API boundary. `recorder-ui` is a reference desktop shell: device picker, path text box, VST catalog UI, native editor windows, and playback controls.

For processor/plugin authoring, see [Recorder Plugin Development](docs/PLUGIN_DEVELOPMENT.md). That guide covers Rust `AudioProcessor`s, the current Recorder C ABI plugin contract, the `recorder-plugin-example` crate, and the current VST packaging limitation.

Use these crates as the integration boundary:

| Crate | Purpose | Integration status |
|-------|---------|--------------------|
| `recorder-core` | Stable core recording pipeline, sessions, buffers, processors, sinks, errors | Use this directly |
| `recorder` | Convenience meta-crate that re-exports `recorder-core`; optional Windows VST host behind `vst` | Prefer this for Rust apps that use VST hosting/editor support |
| `recorder-host-windows` | Windows input-device enumeration and capture backends implementing `AudioHost` | Use this directly on Windows |
| `recorder-host-macos` | macOS input capture via `cpal` | Use this directly on macOS |
| `recorder-host-linux` | Linux input capture via `cpal` | Use this directly on Linux |
| `recorder-sdk` | C ABI dynamic/static library for non-Rust apps | Use this when the consuming app should not require Rust/Cargo |
| `recorder-ui` | egui demo application | Use as reference UI code, not as a component boundary |
| `plugins/parakeet-unified` | Local `AudioAnalyzer` plugin for NVIDIA Parakeet Unified ASR via NeMo | Optional local transcription plugin for Rust/demo use |

### Dependency Setup

For an app in the same workspace or a sibling repo, add path dependencies like this:

```toml
[dependencies]
recorder-core = { path = "../recorder/crates/recorder-core", features = ["wav", "flac", "mp3"] }
# Optional on Windows when you need reusable VST scanning/processing/native editor hosting.
recorder = { path = "../recorder/crates/recorder", features = ["vst"] }

[target.'cfg(windows)'.dependencies]
recorder-host-windows = { path = "../recorder/crates/recorder-host-windows" }

[target.'cfg(target_os = "macos")'.dependencies]
recorder-host-macos = { path = "../recorder/crates/recorder-host-macos" }

[target.'cfg(target_os = "linux")'.dependencies]
recorder-host-linux = { path = "../recorder/crates/recorder-host-linux" }
```

For a non-Rust app, use the binary SDK package instead:

```c
#include "recorder_sdk.h"

size_t required = 0;
int code = recorder_sdk_list_devices_json("wasapi", NULL, 0, &required);
char* json = malloc(required);
code = recorder_sdk_list_devices_json("wasapi", json, required, &required);

RecorderStartConfig cfg = {0};
cfg.audio_system = "wasapi";          /* Windows only; ignored on macOS/Linux */
cfg.raw_output_path = "recording.wav";
cfg.output_format = "wav";

RecorderCapture* capture = NULL;
code = recorder_sdk_start_recording(&cfg, &capture);
if (code != RECORDER_SDK_OK) {
    fprintf(stderr, "%s\n", recorder_sdk_last_error());
}

/* ...record... */

recorder_sdk_capture_stop(capture);
recorder_sdk_capture_free(capture);
free(json);
```

To also record speaker output, enumerate every capture source (inputs **and** loopback) with `recorder_sdk_list_capture_sources_json`, pick a loopback entry, and set the loopback fields on `RecorderStartConfig`:

```c
RecorderStartConfig cfg = {0};
cfg.audio_system = "wasapi";
cfg.raw_output_path = "mic.wav";
cfg.output_format = "wav";
cfg.loopback_source_id = "<id from list_capture_sources_json with kind=loopback>";
cfg.loopback_output_path = "speaker.wav";

RecorderCapture* capture = NULL;
recorder_sdk_start_recording(&cfg, &capture);
/* ... */
recorder_sdk_capture_stop(capture); /* joins both mic and speaker streams */
recorder_sdk_capture_free(capture);
```

**Always zero-initialize `RecorderStartConfig`** (`= {0}` in C, `memset` in C++); the struct is grown additively in the C ABI and stale stack values in trailing fields would otherwise be interpreted as set.

The C ABI intentionally uses opaque handles and C-compatible structs only. Do not pass Rust-owned memory, Rust trait objects, or Rust collections across the SDK boundary.

Choose only the sink features you need:

```toml
recorder-core = { path = "../recorder/crates/recorder-core", default-features = false, features = ["wav"] }
```

Available `recorder-core` features:

- `wav` enables `WavSink`
- `flac` enables `FlacSink`
- `mp3` enables `Mp3Sink`
- `mixer` enables `StreamMixer` (rubato-based two-source mixer for combining mic + speaker into one file)

### Minimal Recording Flow

A host app is responsible for UI, permissions, output-path selection, and lifetime management. The recorder component only needs:

- an `AudioHost`
- a selected `DeviceInfo`
- an `AudioFormat`
- one or more `AudioSink`s
- optional `AudioProcessor`s
- optional `AudioAnalyzer`s and a `MediaEvent` queue for transcripts, speaker labels, or other metadata

Windows example:

```rust
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use recorder_core::{
    AudioFormat, AudioHost, AudioProcessor, RecordingSession, SampleFormat, SessionConfig,
    StreamOptions, WavSink,
};
use recorder_host_windows::{WindowsAudioSystem, WindowsHost};

fn start_wav_recording(path: &Path) -> recorder_core::Result<recorder_core::CaptureStream> {
    let host = WindowsHost::new(WindowsAudioSystem::Wasapi)?;
    let devices = host.list_input_devices()?;
    let device = devices
        .first()
        .ok_or_else(|| recorder_core::RecordingError::Device("no input devices".into()))?;

    let format = device
        .default_format
        .unwrap_or(AudioFormat::new(48_000, 2, SampleFormat::F32));

    let raw_sink = Box::new(WavSink::create(path, format)?);
    let pause_gate = Arc::new(AtomicBool::new(false));

    let session = RecordingSession::new(SessionConfig::default());
    session.add_stream(
        &host,
        Some(device.id.as_str()),
        format,
        StreamOptions {
            raw_sink: Some(raw_sink),
            processed_sink: None,
            processors: Vec::<Box<dyn AudioProcessor + Send>>::new(),
            analyzers: Vec::new(),
            event_tx: None,
            pause_gate: Some(pause_gate),
        },
    )
}
```

Keep the returned `CaptureStream` alive for as long as recording should continue. To stop and finalize output files:

```rust
capture.stop();
```

`CaptureStream::stop()` stops the host stream, closes internal queues, joins writer threads, and flushes/finalizes sinks. This is required for file formats with final headers or trailers, especially WAV, FLAC, and MP3.

### Recording Raw And Processed Files

`StreamOptions` can write two independent outputs from one capture stream:

- `raw_sink`: receives captured input before processors
- `processed_sink`: receives output after the processor chain

Use different file paths for the two sinks:

```rust
use recorder_core::{RecordingSession, SessionConfig, StreamOptions, WavSink};

let raw_sink = Box::new(WavSink::create("recording_pre.wav", format)?);
let processed_sink = Box::new(WavSink::create("recording_post.wav", format)?);

let capture = RecordingSession::new(SessionConfig::default()).add_stream(
    host.as_ref(),
    Some(device.id.as_str()),
    format,
    StreamOptions {
        raw_sink: Some(raw_sink),
        processed_sink: Some(processed_sink),
        processors,
        analyzers: Vec::new(),
        event_tx: None,
        pause_gate: None,
    },
)?;
```

The raw queue is populated before any plugin or processor runs. A failing processor should not prevent raw capture from being written.

### Implementing Processors

Processors implement `AudioProcessor` and operate on interleaved `f32` buffers. The pipeline gives each processor an input `AudioBuffer` and a mutable output `AudioBuffer`.

```rust
use recorder_core::{AudioBuffer, AudioProcessor, RecordingError, Result};

struct Gain {
    amount: f32,
}

impl AudioProcessor for Gain {
    fn name(&self) -> &str {
        "gain"
    }

    fn process(&mut self, input: &AudioBuffer, output: &mut AudioBuffer) -> Result<()> {
        if input.format != output.format || input.frames != output.frames {
            return Err(RecordingError::FormatMismatch {
                expected: input.format,
                got: output.format,
            });
        }

        output.captured_at = input.captured_at;
        output.frame_index = input.frame_index;
        output.data = input
            .data
            .iter()
            .map(|s| (s * self.amount).clamp(-1.0, 1.0))
            .collect::<Vec<_>>()
            .into();
        Ok(())
    }
}
```

Add processors in `StreamOptions.processors`:

```rust
let processors: Vec<Box<dyn AudioProcessor + Send>> =
    vec![Box::new(Gain { amount: 0.5 })];
```

### Analyzers And Media Events

Analyzers observe processed audio on worker threads and emit typed `MediaEvent`s through a bounded queue. This is the preferred path for realtime transcription, speaker identification, VAD, keyword detection, and other metadata-producing modules.

```rust
use recorder_core::{media_event_queue, StreamOptions, VoiceActivityAnalyzer};

let (event_tx, event_rx) = media_event_queue(1024);

let options = StreamOptions {
    raw_sink: Some(raw_sink),
    processed_sink: Some(processed_sink),
    processors,
    analyzers: vec![Box::new(VoiceActivityAnalyzer::default())],
    event_tx: Some(event_tx),
    pause_gate: None,
};

for event in event_rx.try_iter() {
    println!("{event:?}");
}
```

Analyzers receive the post-processor buffer, so VST/DSP enhancement can run before ASR. If analyzer queues overflow, analysis input is dropped and recording continues.

### Local Analyzer Plugins

The workspace has a `plugins/` directory for Recorder-native local plugins. These are Rust crates that can implement `AudioAnalyzer`, `AudioProcessor`, or future plugin capabilities without going through VST.

The demo app currently links `plugins/parakeet-unified`, which exposes `NVIDIA Parakeet Unified EN 0.6B` as a selectable local analyzer plugin. It is disabled by default. When enabled, enhanced audio is chunked, downmixed/resampled to 16 kHz mono WAV, and sent to a persistent Python/NeMo sidecar. The sidecar loads `nvidia/parakeet-unified-en-0.6b` and returns transcript events for the demo's analyzer output panel.

Runtime setup:

```powershell
python -m pip install "nemo_toolkit[asr]"
```

Optional environment variables:

- `RECORDER_PARAKEET_PYTHON`: Python executable to run. Defaults to the nearest project `.venv` Python when present, otherwise `python`.
- `RECORDER_PARAKEET_WORKER`: path to the sidecar script. Defaults to the bundled `plugins/parakeet-unified/python/worker.py`.
- `RECORDER_PARAKEET_CHUNK_SECONDS`: chunk length sent to the model. Defaults to `1.5`.
- `RECORDER_PARAKEET_SILENCE_RMS`: chunks with RMS below this are skipped without invoking the model. Defaults to `0.005`.

The model card lists NeMo 2.7.3, Linux, and NVIDIA GPU hardware as the supported runtime. The Rust plugin compiles without those Python dependencies; transcription starts only when the local analyzer is enabled and the sidecar can load the model.

### Device And Format Selection

The host crate exposes input devices through `AudioHost::list_input_devices()`. Each `DeviceInfo` may include a `default_format`.

```rust
let devices = host.list_input_devices()?;
for device in &devices {
    println!("{}: {:?}", device.name, device.default_format);
}
```

Use the device's default format unless your app has already verified the backend supports a different one. Current host implementations are conservative and may reject mismatched sample rates, channel counts, or sample formats.

### Pause / Resume

Set `StreamOptions.pause_gate` to an `Arc<AtomicBool>`. While it is `true`, the capture callback drops incoming buffers before writing either output. This keeps the device stream alive but compresses the recorded timeline.

```rust
pause_gate.store(true, std::sync::atomic::Ordering::Release);  // pause
pause_gate.store(false, std::sync::atomic::Ordering::Release); // resume
```

### Status And Error Reporting

`RecordingSession::add_stream` returns startup errors synchronously: missing device, unsupported format, sink open failure, etc.

After startup, capture callbacks and writer threads run asynchronously. The current public API exposes `CaptureStream::metrics()` for dropped-frame and processor-timeout counters:

```rust
let metrics = capture.metrics();
let raw_dropped = metrics.raw_frames_dropped.load(std::sync::atomic::Ordering::Relaxed);
let processed_dropped = metrics.processed_frames_dropped.load(std::sync::atomic::Ordering::Relaxed);
let analyzer_dropped = metrics.analyzer_frames_dropped.load(std::sync::atomic::Ordering::Relaxed);
let plugin_timeouts = metrics.plugin_timeouts.load(std::sync::atomic::Ordering::Relaxed);
```

If your application needs durable background error reporting from writer threads, add a small adapter around `AudioSink` or extend `recorder-core` with an error callback/channel before treating errors as user-visible UI state.

### MP3 Runtime Packaging

On Windows, applications using `Mp3Sink` must ship `libmp3lame.dll` next to their executable or otherwise ensure it is discoverable by the dynamic loader.

This repo's `recorder-ui/build.rs` copies the vendored DLL into `target/<profile>/` for the demo app. Another executable should either copy:

```text
third_party/lame/windows-x64/libmp3lame.dll
```

next to its `.exe`, or copy the same `build.rs` pattern into that executable crate.

On macOS and Linux, install a system `libmp3lame`.

### VST Integration Status

VST2/VST3 support lives in the root `recorder` crate behind the Windows-only `vst` feature. It exposes reusable APIs for scanning plugin paths, loading VST2/VST3 plugins, building `Vec<Box<dyn AudioProcessor + Send>>`, and opening/closing native editor windows. The `recorder-ui` demo consumes that library API instead of carrying its own VST host implementation.

The native vendor editor is hosted in a separate Win32 `HWND`; the optional egui companion panel in `VstUiState::draw_native_editor_ui` only explains/controls that window.

Windows VST build notes:

- VST3 audio uses `rack` and requires CMake plus a C++ toolchain.
- VST3 native editor UI uses `vst3` and Win32 `HWND` hosting.
- VST2 audio/editor support uses the vendored `vst-rs` patch in `vendor/vst-rs`.
- The workspace root patches `rack` and `vst` via `[patch.crates-io]`; preserve those patches if you split crates into another workspace.

### Integration Checklist For LLMs And Humans

When integrating this recorder into another app:

1. Depend on `recorder-core` and one host crate; avoid depending on `recorder-ui`.
2. Choose sink features deliberately (`wav`, `flac`, `mp3`) to avoid unnecessary runtime dependencies.
3. Enumerate devices with the selected host and use `DeviceInfo.default_format` unless you validate another format.
4. Create distinct paths for raw and processed sinks.
5. Use analyzers plus `MediaEvent`s for ASR, diarization, and other metadata instead of blocking inside processors.
6. Hold `CaptureStream` for the recording lifetime.
7. Always call `CaptureStream::stop()` before assuming files are finalized.
8. Keep processors `Send`, non-blocking, and tolerant of the stream's channel count.
9. If using MP3 on Windows, package `libmp3lame.dll` next to the final executable.
10. If using VST on Windows, depend on `recorder` with `features = ["vst"]` and use `recorder::vst`.
11. Surface `CaptureStream::metrics()` and any sink/analyzer errors in your application's own status/logging system.

### MP3 (LAME) on Windows

MP3 encoding uses the **LAME** library via [`mp3lame-encoder`](https://crates.io/crates/mp3lame-encoder). This repository vendors a 64-bit Windows build at `third_party/lame/windows-x64/libmp3lame.dll` (see [third_party/lame/README.md](third_party/lame/README.md)).

`recorder-ui`’s `build.rs` copies that DLL into `target/<profile>/` next to the executable when you build on Windows. For other binaries that enable the `mp3` feature, copy the DLL yourself or extend the same pattern.

### MP3 on macOS and Linux

Install a system **libmp3lame** (package names vary by distro; on macOS, **Homebrew** `lame` is typical). The Rust crate loads the native library at runtime.

### Re-vendoring LAME files

```powershell
pwsh scripts/vendor_lame.ps1
```

Re-downloads the official LAME 3.100 source tarball (for `LICENSE` / `COPYING`) and the RareWares Windows x64 ZIP (for `libmp3lame.dll`).

## License: this project

Workspace crates are licensed under **MIT OR Apache-2.0** (see each `Cargo.toml` and `LICENSE-*` files if you add them at the repo root).

## Third-party components

| Component | Use | License / notes |
|-----------|-----|-----------------|
| **LAME** (`libmp3lame`) | MP3 encoding (native, runtime-loaded) | **LGPL-2.0** (GNU Library GPL v2). Texts in [third_party/lame/LICENSE](third_party/lame/LICENSE) and [third_party/lame/COPYING](third_party/lame/COPYING). Not the same license as this repo’s Rust sources. |
| **RareWares** Windows build | Prebuilt `libmp3lame.dll` 3.100 x64 | Same LAME license applies to the DLL; provenance in [third_party/lame/README.md](third_party/lame/README.md). |
| [`mp3lame-encoder`](https://crates.io/crates/mp3lame-encoder) | Rust bindings to LAME | **MIT** (the crate; you still comply with LGPL for the native library). |
| [`hound`](https://crates.io/crates/hound) | WAV I/O | **Apache-2.0 OR MIT** |
| [`flacenc`](https://crates.io/crates/flacenc) | FLAC encoding | **MIT** |
| [`cpal`](https://crates.io/crates/cpal) (via host crates) | Cross-platform audio I/O | **Apache-2.0** |
| [`rodio`](https://crates.io/crates/rodio) / [`symphonia`](https://crates.io/crates/symphonia) | Demo playback | **MIT** (rodio) / **MPL-2.0** (Symphonia; see crate metadata) |
| [`egui`](https://crates.io/crates/egui) / [`eframe`](https://crates.io/crates/eframe) | Demo UI | **MIT OR Apache-2.0** |

MP3 may be subject to **patents** in some jurisdictions; LGPL addresses copyright, not patents. This README is not legal advice.

### LAME acknowledgment

This software uses **LAME**, licensed under the LGPL. See [https://lame.sourceforge.io/](https://lame.sourceforge.io/) (historically [www.mp3dev.org](http://www.mp3dev.org/)).
