# Recorder Plugin Development

This document explains how to add audio processing to Recorder-based applications.

There are currently several plugin/module paths:

1. **Rust in-process processors**: implement `recorder_core::AudioProcessor`. This is the recommended path for Rust applications embedding `recorder-core`.
2. **Rust in-process analyzers**: implement `recorder_core::AudioAnalyzer` to observe processed audio on a worker thread and emit typed `MediaEvent`s.
3. **Host-controllable plugins**: implement `recorder_core::ControllablePlugin` when the host or automation rules should be able to change parameters.
4. **Recorder C ABI plugins**: build a dynamic library that exports `recorder_plugin_entry_v1`. The ABI is defined in `recorder-plugin-api`, and `recorder-plugin-example` is a minimal gain plugin.
5. **VST2/VST3 plugins**: supported by the `recorder-ui` demo on Windows. VSTs fit the processor/control side of the framework; analyzer-style metadata is handled by Recorder-native analyzers.

Important current limitation: the binary SDK (`recorder-sdk`) exposes recording/device APIs, but it does **not** yet expose a `load_plugin(path)` function for Recorder C ABI plugins. Treat `recorder-plugin-api` as the stable plugin contract and `recorder-plugin-example` as the authoring template; the SDK loader API still needs to be added before non-Rust host apps can load these plugins dynamically.

## Rust AudioProcessor Plugins

Rust applications can pass processors directly through `StreamOptions.processors`.

Implement `AudioProcessor`:

```rust
use recorder_core::{AudioBuffer, AudioProcessor, RecordingError, Result};

pub struct Gain {
    pub amount: f32,
}

impl AudioProcessor for Gain {
    fn name(&self) -> &str {
        "gain"
    }

    fn reset(&mut self) {
        // Optional: clear delay lines, filter state, envelope followers, etc.
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
            .map(|sample| (sample * self.amount).clamp(-1.0, 1.0))
            .collect::<Vec<_>>()
            .into();

        Ok(())
    }
}
```

Attach it to a recording:

```rust
use recorder_core::{AudioProcessor, StreamOptions};

let processors: Vec<Box<dyn AudioProcessor + Send>> =
    vec![Box::new(Gain { amount: 0.5 })];

let options = StreamOptions {
    raw_sink: Some(raw_sink),
    processed_sink: Some(processed_sink),
    processors,
    analyzers: Vec::new(),
    event_tx: None,
    pause_gate: None,
};
```

### Processor Contract

- Input and output buffers are interleaved `f32` samples.
- `input.data.len()` should equal `input.frames * input.format.channels`.
- `output` is pre-sized by the pipeline. Write a full output buffer each call.
- Preserve `captured_at` and `frame_index` unless you intentionally change timing metadata.
- Return `Err(...)` instead of panicking. The pipeline will pass through the current buffer if a processor errors or exceeds its configured time budget.
- Keep processing realtime-friendly: no blocking I/O, no device enumeration, no network calls, and avoid heap allocation in the audio path where possible.
- Processors must be `Send`.

## Rust AudioAnalyzer Plugins

Analyzers observe audio and emit metadata instead of writing audio output. They are intended for VAD, ASR, diarization, language ID, keyword detection, and other model-driven or stateful analysis.

Analyzers are fed from bounded worker queues after the processor chain. That means an ASR analyzer can receive denoised/enhanced audio without running inference on the host audio callback.

```rust
use recorder_core::{AudioAnalyzer, AudioBuffer, MediaEvent, Result};

pub struct MyAnalyzer {
    pending: Vec<MediaEvent>,
}

impl AudioAnalyzer for MyAnalyzer {
    fn name(&self) -> &str {
        "my-analyzer"
    }

    fn accept_audio(&mut self, input: &AudioBuffer) -> Result<()> {
        // Copy, resample, window, or enqueue model input here. This runs on an analyzer worker.
        self.pending.push(MediaEvent::AttributeDetected {
            tap: recorder_core::AudioTap::Processed,
            start_frame: input.frame_index,
            end_frame: input.frame_index + input.frames as u64,
            key: "example".to_string(),
            value: "detected".to_string(),
            confidence: Some(1.0),
        });
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<MediaEvent> {
        std::mem::take(&mut self.pending)
    }
}
```

Wire analyzers with an event queue:

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

// The app/UI drains event_rx.try_iter() on its own thread.
```

Analyzer guidance:

- Emit frame-indexed events so the host can align transcripts, speaker labels, and routed clips with recorded audio.
- Treat queue overflow as acceptable loss for realtime capture; raw and processed recording should continue even if analysis falls behind.
- Use analyzers for metadata. Use processors for sample transforms.
- Keep long-running models inside the analyzer worker or an internal model thread, not inside `AudioProcessor::process`.

### Local Analyzer Plugin Crates

Recorder-native local plugins live under the workspace `plugins/` directory. They are ordinary Rust crates linked by host apps that want them. This keeps ASR/diarization-style plugins separate from VST and from the C ABI audio processor contract.

The included Parakeet plugin is in `plugins/parakeet-unified`:

- Rust crate: `recorder-plugin-parakeet`
- Analyzer: `ParakeetUnifiedAnalyzer`
- Model: `nvidia/parakeet-unified-en-0.6b`
- Runtime: persistent Python sidecar using NVIDIA NeMo

The Rust analyzer receives processed/enhanced audio, writes 16 kHz mono WAV chunks, and sends those chunks to `python/worker.py`. The sidecar loads the model once and emits `TranscriptFinal` events back to the host.

Demo app integration:

```rust
let analyzer = recorder_plugin_parakeet::create_analyzer(
    "nvidia.parakeet-unified-en-0.6b",
)?;
```

Runtime setup:

```powershell
python -m pip install "nemo_toolkit[asr]"
```

Environment overrides:

- `RECORDER_PARAKEET_PYTHON` (defaults to the nearest project `.venv` Python when present, otherwise `python`)
- `RECORDER_PARAKEET_WORKER`
- `RECORDER_PARAKEET_CHUNK_SECONDS`

## Host-Mediated Control

Plugins that expose parameters can implement `ControllablePlugin`. Analyzer output should not mutate another plugin directly. Instead, analyzers emit `MediaEvent`s, and the host or an automation rule maps those events to `PluginCommand`s.

Example flow:

```text
KeyDetector analyzer -> MediaEvent::AttributeDetected("key", "D minor")
Host automation rule -> PluginCommand::SetParameter(target_key = "D minor")
Pitch correction processor <- host applies command
```

This is the same interaction style used by DAWs, but the host remains in charge of ordering, thread boundaries, and parameter ownership.

### Channel Handling

Processors should support any channel count they receive from the selected device.

For channel-independent effects:

```rust
for sample in input.data.iter() {
    // Process each interleaved sample.
}
```

For per-frame effects:

```rust
let channels = input.format.channels as usize;
for frame in 0..input.frames {
    let base = frame * channels;
    let frame_samples = &input.data[base..base + channels];
    // Process one interleaved frame.
}
```

Do not assume stereo unless your host app validates and requests stereo-only devices.

## Recorder C ABI Plugins

The C ABI is defined by `crates/recorder-plugin-api`.

A plugin is a dynamic library that exports:

```c
int recorder_plugin_entry_v1(RecorderPluginV1* out);
```

The entry function fills a vtable:

```rust
#[repr(C)]
pub struct RecorderPluginV1 {
    pub abi_version: u32,
    pub user: *mut core::ffi::c_void,
    pub process: Option<RecorderPluginProcessV1>,
    pub destroy: Option<RecorderPluginDestroyV1>,
}
```

The process callback signature is:

```rust
pub type RecorderPluginProcessV1 = unsafe extern "C" fn(
    user: *mut core::ffi::c_void,
    frame: *const RecorderAudioFrameV1,
    out: *mut f32,
    out_len: usize,
) -> i32;
```

`RecorderAudioFrameV1` contains:

```rust
#[repr(C)]
pub struct RecorderAudioFrameV1 {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub _reserved: u16,
    pub frames: usize,
    pub data: *const f32,
}
```

`data` points to interleaved `frames * channels` `f32` samples. `out` points to caller-owned memory with `out_len` samples. Return `0` on success and non-zero on failure.

### Minimal Rust cdylib Plugin

Use `recorder-plugin-example` as the template.

`Cargo.toml`:

```toml
[package]
name = "my-recorder-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
recorder-plugin-api = { path = "../recorder/crates/recorder-plugin-api" }
```

`src/lib.rs`:

```rust
use std::slice;

use recorder_plugin_api::{
    RecorderAudioFrameV1, RecorderPluginV1, RECORDER_PLUGIN_ABI_VERSION,
};

struct State {
    gain: f32,
}

#[no_mangle]
pub extern "C" fn recorder_plugin_entry_v1(out: *mut RecorderPluginV1) -> i32 {
    if out.is_null() {
        return -1;
    }

    let user = Box::into_raw(Box::new(State { gain: 0.5 })) as *mut core::ffi::c_void;

    unsafe {
        std::ptr::write(
            out,
            RecorderPluginV1 {
                abi_version: RECORDER_PLUGIN_ABI_VERSION,
                user,
                process: Some(process),
                destroy: Some(destroy),
            },
        );
    }

    0
}

unsafe extern "C" fn process(
    user: *mut core::ffi::c_void,
    frame: *const RecorderAudioFrameV1,
    out: *mut f32,
    out_len: usize,
) -> i32 {
    if user.is_null() || frame.is_null() || out.is_null() {
        return -1;
    }

    let state = &*(user as *const State);
    let frame = &*frame;

    if frame.data.is_null() {
        return -2;
    }

    let sample_count = frame.frames.saturating_mul(frame.channels as usize);
    if sample_count > out_len {
        return -3;
    }

    let input = slice::from_raw_parts(frame.data, sample_count);
    let output = slice::from_raw_parts_mut(out, sample_count);

    for i in 0..sample_count {
        output[i] = (input[i] * state.gain).clamp(-1.0, 1.0);
    }

    0
}

unsafe extern "C" fn destroy(user: *mut core::ffi::c_void) {
    if !user.is_null() {
        drop(Box::from_raw(user as *mut State));
    }
}
```

Build:

```bash
cargo build --release
```

Output library names by platform:

- Windows: `my_recorder_plugin.dll`
- macOS: `libmy_recorder_plugin.dylib`
- Linux: `libmy_recorder_plugin.so`

### ABI Rules

- Export exactly `recorder_plugin_entry_v1`.
- Set `abi_version` to `RECORDER_PLUGIN_ABI_VERSION`.
- Use `#[repr(C)]` types only across the boundary.
- Do not let Rust panics unwind across the ABI boundary. Catch failures and return non-zero.
- Allocate plugin state in `recorder_plugin_entry_v1` and release it in `destroy`.
- Treat `frame.data` as read-only.
- Write exactly `frames * channels` samples to `out` on success.
- Validate all pointers and lengths.
- Keep `process` realtime-friendly.
- Do not retain `frame.data` or `out` after `process` returns.
- Return `0` on success. Return a plugin-defined non-zero error code on failure.

### C/C++ Plugin Authors

You can implement the same ABI from C or C++ by mirroring the structs in `recorder-plugin-api`.

The Rust definitions are the source of truth for v1 layout:

- `RECORDER_PLUGIN_ABI_VERSION = 1`
- `RecorderAudioFrameV1`
- `RecorderPluginV1`
- `recorder_plugin_entry_v1`

If you write a C/C++ header for plugins, keep field order, integer widths, pointer types, and calling convention aligned with the Rust `extern "C"` definitions.

## Testing Plugins

Recommended checks:

1. Build the plugin in release mode.
2. Run unit tests for DSP math using ordinary Rust tests where possible.
3. Test mono and stereo buffers.
4. Test unusual block sizes, including very small buffers.
5. Test analyzer event emission and queue-overflow behavior if the module implements `AudioAnalyzer`.
6. Test null pointer / short output error paths for C ABI plugins.
7. Verify the plugin does not allocate heavily or block in `process`.

For Rust processors and analyzers, add tests against `AudioBuffer` directly. For C ABI plugins, write a small loader test or use the future SDK loader once available.

## VST Plugins

VST2/VST3 plugin hosting is separate from the Recorder C ABI plugin system.

Current state:

- The `recorder` crate can scan/load VST2 and VST3 plugins on Windows behind the `vst` feature, and `recorder-ui` demonstrates that API.
- VST2 hosting uses the vendored `vst-rs` patch in `vendor/vst-rs`.
- VST3 hosting uses `rack` for audio and `vst3` for native editor windows.
- VSTs currently adapt into the `AudioProcessor` chain. Analyzer-style metadata should use Recorder-native `AudioAnalyzer`s and `MediaEvent`s.
- VST support is not exposed through `recorder-sdk`.

If your application needs VST support through the binary SDK, add explicit SDK calls for scanning, chain management, editor windows, and parameter control.

## Versioning

The current Recorder C ABI is v1.

Breaking changes require one of:

- incrementing `RECORDER_PLUGIN_ABI_VERSION`
- adding a new entry symbol such as `recorder_plugin_entry_v2`

Non-breaking additions should append fields to new structs or use a new vtable while keeping v1 loadable.
