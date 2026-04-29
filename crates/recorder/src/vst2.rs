//! VST2 (legacy 2.4 .dll) hosting on Windows via [`vst`](https://crates.io/crates/vst) (vst-rs).
//!
//! VST2 plugins are plain `.dll` files exporting either `VSTPluginMain` or `main` (legacy).
//! The Steinberg VST2 SDK is officially deprecated, but the format is still ubiquitous —
//! ReaPlugs and many free/open-source plugins ship VST2 only. The `vst-rs` crate provides
//! a clean-room re-implementation of the 2.4 ABI that works without the deprecated headers.
//!
//! Capture chain conversion: the recording stream is mono or stereo, so we feed the plugin's
//! plane 0 from input L (or mono) and plane 1 from input R (or mono duplicated). Any
//! additional input planes the plugin asks for (typical: a side-chain compressor reports 4
//! inputs) get fed silence — we don't have a side-chain bus in this app today. We then
//! read the plugin's first output plane back to L and the second to R (or duplicate the
//! first if the plugin only produces one), which matches what other VST2 hosts do for
//! plugins inserted on a stereo track.
//!
//! Thread-safety: `vst::host::PluginInstance` holds raw plugin pointers and is not `Send`
//! by itself, but the audio thread is the only caller while the chain is active. We wrap
//! the instance behind a `Mutex` and assert `Send` for the wrapper type.
//!
//! The whole `vst` crate is marked `#[deprecated]` because the upstream Steinberg VST2
//! SDK has been retired. We still depend on it because (a) the format itself is widely
//! deployed and our users have plugins in this format, and (b) `vst-rs` is a clean-room
//! reimplementation that doesn't need the deprecated SDK headers. The deprecation
//! warnings are noise here, so we silence them at module scope.
#![allow(deprecated)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use recorder_core::buffer::AudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::SampleFormat;
use recorder_core::traits::AudioProcessor;
use vst::host::{Host, HostBuffer, PluginInstance, PluginLoader};
use vst::plugin::Plugin;

/// Stripped-down host: VST2 plugins legitimately work without parameter automation, time
/// info, or event routing. Defaults from the trait are fine for capture-style hosting.
pub struct MinimalHost;

impl Host for MinimalHost {
    fn get_info(&self) -> (isize, String, String) {
        (1, "recorder".to_string(), "recorder".to_string())
    }
    fn get_block_size(&self) -> isize {
        // Plugins query this; matches the worst-case block we initialize with.
        4096
    }
    fn idle(&self) {}
    fn update_display(&self) {}
}

/// Catalog entry for a successfully probed VST2 plugin.
///
/// `vendor`, `inputs`, `outputs` aren't surfaced in the UI yet but are kept on the entry
/// so future diagnostics / parameter screens have them without re-probing the DLL.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Vst2PluginInfo {
    pub path: PathBuf,
    pub name: String,
    pub vendor: String,
    pub unique_id: i32,
    pub inputs: i32,
    pub outputs: i32,
}

impl Vst2PluginInfo {
    /// Stable `(path, unique_id)` key used for catalog dedup.
    pub fn dedup_key(&self) -> (PathBuf, i32) {
        (self.path.clone(), self.unique_id)
    }
}

/// Probe a single `.dll` to determine whether it is a hostable VST2 plugin and read its info.
///
/// Loading a third-party DLL can crash if architecture/CRT mismatches; `catch_unwind` keeps
/// the rest of the scan running. The returned info is the plugin's own self-description
/// after a fresh `init()`.
pub fn probe_vst2(path: &Path) -> std::result::Result<Vst2PluginInfo, String> {
    let path = path.to_path_buf();
    let host = Arc::new(Mutex::new(MinimalHost));
    let result = catch_unwind(AssertUnwindSafe(|| -> std::result::Result<_, String> {
        let mut loader = PluginLoader::load(&path, host.clone())
            .map_err(|e| format!("PluginLoader::load failed: {e:?}"))?;
        let mut instance = loader
            .instance()
            .map_err(|e| format!("loader.instance() failed: {e:?}"))?;
        instance.init();
        let info = instance.get_info();
        Ok(info)
    }));
    let info = match result {
        Ok(Ok(info)) => info,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("plugin panicked during load/init".to_string()),
    };
    if info.name.is_empty() && info.unique_id == 0 {
        return Err("plugin returned empty Info (probably not a VST2 plugin)".to_string());
    }
    Ok(Vst2PluginInfo {
        path,
        name: if info.name.trim().is_empty() {
            "(unnamed VST2)".to_string()
        } else {
            info.name
        },
        vendor: info.vendor,
        unique_id: info.unique_id,
        inputs: info.inputs,
        outputs: info.outputs,
    })
}

/// Scan a single directory's `.dll` files (non-recursive) and return all hostable VST2 plugins.
///
/// Per-DLL errors are collected so callers can surface them without aborting the whole scan.
pub fn scan_dir_for_vst2(dir: &Path) -> (Vec<Vst2PluginInfo>, Vec<(PathBuf, String)>) {
    let mut plugins = Vec::new();
    let mut errors = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (plugins, errors);
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_dll = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dll"))
            .unwrap_or(false);
        if !is_dll {
            continue;
        }
        match probe_vst2(&path) {
            Ok(info) => plugins.push(info),
            Err(e) => errors.push((path, e)),
        }
    }
    (plugins, errors)
}

/// A loaded VST2 plugin retained for the lifetime of a chain entry.
///
/// Field order matters for `Drop`: `instance` must run its destructor before `_loader` drops
/// the underlying `libloading::Library`, otherwise we'd be calling into freed code.
///
/// `info`, `in_channels`, `out_channels` are kept for upcoming UI/automation work even
/// though the audio path doesn't read them today.
#[allow(dead_code)]
pub struct Vst2HostedPlugin {
    // Crate-visible so `vst_native_editor_win` can call `instance.get_editor()` etc.
    // without going through extra accessor plumbing for every Editor trait method.
    pub(crate) instance: PluginInstance,
    _loader: PluginLoader<MinimalHost>,
    pub info: Vst2PluginInfo,
    pub in_channels: usize,
    pub out_channels: usize,
}

// SAFETY: `PluginInstance`/`PluginLoader` hold raw pointers into the loaded plugin and are
// not auto-`Send`. We never call into the plugin from multiple threads concurrently — every
// access goes through a `Mutex<Vst2HostedPlugin>` owned by `Vst2AudioProcessor`. Moving the
// wrapper between threads (e.g. UI thread -> audio thread) is therefore safe.
unsafe impl Send for Vst2HostedPlugin {}

pub fn load_and_init_vst2(
    info: &Vst2PluginInfo,
    sample_rate_hz: u32,
    max_block: usize,
) -> std::result::Result<Arc<Mutex<Vst2HostedPlugin>>, String> {
    let host = Arc::new(Mutex::new(MinimalHost));
    let mut loader = PluginLoader::load(&info.path, host)
        .map_err(|e| format!("PluginLoader::load failed: {e:?}"))?;
    let mut instance = loader
        .instance()
        .map_err(|e| format!("loader.instance() failed: {e:?}"))?;
    instance.init();
    instance.set_sample_rate(sample_rate_hz as f32);
    instance.set_block_size(max_block as i64);
    instance.resume();
    instance.start_process();
    let probed = instance.get_info();
    let in_channels = probed.inputs.max(0) as usize;
    let out_channels = probed.outputs.max(0) as usize;
    Ok(Arc::new(Mutex::new(Vst2HostedPlugin {
        instance,
        _loader: loader,
        info: info.clone(),
        in_channels,
        out_channels,
    })))
}

impl Drop for Vst2HostedPlugin {
    fn drop(&mut self) {
        self.instance.stop_process();
        self.instance.suspend();
    }
}

pub fn reinit_vst2(
    plugins: &[Arc<Mutex<Vst2HostedPlugin>>],
    sample_rate_hz: u32,
    max_block: usize,
) -> std::result::Result<(), String> {
    for arc in plugins {
        let mut g = arc.lock().map_err(|e| format!("{e:?}"))?;
        g.instance.stop_process();
        g.instance.suspend();
        g.instance.set_sample_rate(sample_rate_hz as f32);
        g.instance.set_block_size(max_block as i64);
        g.instance.resume();
        g.instance.start_process();
    }
    Ok(())
}

/// Per-channel planar scratch sized to the plugin's actual `(inputs, outputs)` count.
///
/// VST2 plugins routinely report more than 2 of either: side-chain compressors expose 4
/// inputs, multi-out routers expose 8+ outputs, etc. `vst-rs::PluginInstance::process`
/// panics if `HostBuffer` was constructed with fewer planes than the plugin advertises, so
/// we size to the plugin's reported counts and pad / truncate in `process()`.
struct PlanarScratch {
    /// Plane storage; `inputs.len() == plugin.in_channels`, each `Vec<f32>` length `max_block`.
    inputs: Vec<Vec<f32>>,
    /// Same for outputs; `outputs.len() == plugin.out_channels`.
    outputs: Vec<Vec<f32>>,
}

/// Live counters surfaced to the demo UI so users can see whether buffers are actually
/// reaching the plugin. Cheap to update from the audio thread (relaxed atomics) and read
/// from the UI thread.
#[derive(Default)]
pub struct Vst2DiagCounters {
    /// Number of `process()` calls that successfully ran the plugin's `processReplacing`.
    pub processed: AtomicU64,
    /// Number of `process()` calls that returned `Err` before invoking the plugin
    /// (sample-format mismatch, channel count, scratch lock, etc.).
    pub errored: AtomicU64,
    /// Most recent peak of the input buffer the plugin was about to process, scaled by
    /// `f32::to_bits` so we can store it in an atomic. `0` until the first call.
    pub last_peak_bits: AtomicU64,
}

/// VST2 effect wrapper sized to the plugin's reported channel layout.
pub struct Vst2AudioProcessor {
    plugin: Arc<Mutex<Vst2HostedPlugin>>,
    label: String,
    max_block: usize,
    /// Cached so `process()` doesn't have to re-lock the plugin to know the layout. The
    /// input count isn't read in the audio loop today (we always pad/clear the planes by
    /// iterating the scratch directly), but it's kept symmetric with the output count for
    /// future diagnostics / the parameter UI.
    #[allow(dead_code)]
    plugin_in_channels: usize,
    plugin_out_channels: usize,
    scratch: Mutex<PlanarScratch>,
    host_buf: Mutex<HostBuffer<f32>>,
    diag: Arc<Vst2DiagCounters>,
}

// SAFETY: `HostBuffer<f32>` internally holds `Vec<*const f32>` / `Vec<*mut f32>` scratch
// arrays which the auto-trait analysis treats as `!Send`. Those vectors are populated and
// consumed entirely inside `process()` while holding `host_buf`'s `Mutex`; pointers never
// escape and are never observed by another thread. Same logic as the VST3 processor: the
// audio thread is the only caller during recording, and the wrapper is moved (not shared)
// when handed off as a `Box<dyn AudioProcessor + Send>`.
unsafe impl Send for Vst2AudioProcessor {}

#[allow(dead_code)]
impl Vst2AudioProcessor {
    pub fn new(
        info: &Vst2PluginInfo,
        sample_rate_hz: u32,
        max_block: usize,
    ) -> std::result::Result<Self, String> {
        let plugin = load_and_init_vst2(info, sample_rate_hz, max_block)?;
        Ok(Self::from_arc_with_diag(
            plugin,
            info.name.clone(),
            max_block,
            Arc::new(Vst2DiagCounters::default()),
        ))
    }

    pub fn from_arc(plugin: Arc<Mutex<Vst2HostedPlugin>>, label: String, max_block: usize) -> Self {
        Self::from_arc_with_diag(
            plugin,
            label,
            max_block,
            Arc::new(Vst2DiagCounters::default()),
        )
    }

    pub fn from_arc_with_diag(
        plugin: Arc<Mutex<Vst2HostedPlugin>>,
        label: String,
        max_block: usize,
        diag: Arc<Vst2DiagCounters>,
    ) -> Self {
        // Read the plugin's reported channel layout *once* here. We tolerate a poisoned
        // mutex (e.g. a previous run panicked inside `process`) by reading through
        // `into_inner` — the channel-count fields are POD set at load time, so even on
        // poison they are still valid. The audio path itself will surface a clean
        // `RecordingError::Plugin` instead of panicking again, since the HostBuffer is now
        // sized to those very counts.
        let (in_ch, out_ch) = match plugin.lock() {
            Ok(g) => (g.in_channels, g.out_channels),
            Err(p) => {
                let g = p.into_inner();
                (g.in_channels, g.out_channels)
            }
        };
        let host_buf = HostBuffer::<f32>::new(in_ch, out_ch);
        let inputs: Vec<Vec<f32>> = (0..in_ch).map(|_| vec![0.0f32; max_block]).collect();
        let outputs: Vec<Vec<f32>> = (0..out_ch).map(|_| vec![0.0f32; max_block]).collect();
        Self {
            plugin,
            label,
            max_block,
            plugin_in_channels: in_ch,
            plugin_out_channels: out_ch,
            scratch: Mutex::new(PlanarScratch { inputs, outputs }),
            host_buf: Mutex::new(host_buf),
            diag,
        }
    }

    pub fn plugin_arc(&self) -> Arc<Mutex<Vst2HostedPlugin>> {
        self.plugin.clone()
    }

    pub fn display_label(&self) -> &str {
        &self.label
    }

    pub fn diag(&self) -> Arc<Vst2DiagCounters> {
        self.diag.clone()
    }
}

impl AudioProcessor for Vst2AudioProcessor {
    fn name(&self) -> &str {
        &self.label
    }

    fn reset(&mut self) {
        if let Ok(mut g) = self.plugin.lock() {
            g.instance.suspend();
            g.instance.resume();
        }
    }

    fn process(&mut self, input: &AudioBuffer, output: &mut AudioBuffer) -> Result<()> {
        let bump_err = |e: RecordingError| -> Result<()> {
            self.diag.errored.fetch_add(1, Ordering::Relaxed);
            Err(e)
        };
        if input.format.sample_format != SampleFormat::F32 {
            return bump_err(RecordingError::Plugin(format!(
                "VST2 path expects F32 buffers (got {:?}, ch={}, sr={})",
                input.format.sample_format, input.format.channels, input.format.sample_rate_hz
            )));
        }
        let frames = input.frames;
        if frames == 0 {
            *output = AudioBuffer::silent(input.format, 0, input.captured_at, input.frame_index);
            return Ok(());
        }
        if frames > self.max_block {
            return bump_err(RecordingError::Plugin(format!(
                "block {} exceeds VST2 init {}",
                frames, self.max_block
            )));
        }

        let ch = input.format.channels as usize;
        if ch != 1 && ch != 2 {
            return bump_err(RecordingError::Plugin(format!(
                "VST2 path supports 1–2 input channels; got {ch}"
            )));
        }

        let peak = input
            .data
            .iter()
            .copied()
            .fold(0.0f32, |max, s| max.max(s.abs()));
        self.diag
            .last_peak_bits
            .store(peak.to_bits() as u64, Ordering::Relaxed);

        let mut sc = self
            .scratch
            .lock()
            .map_err(|_| RecordingError::Plugin("VST2 scratch mutex poisoned".into()))?;
        let mut hb = self
            .host_buf
            .lock()
            .map_err(|_| RecordingError::Plugin("VST2 host buffer mutex poisoned".into()))?;
        let mut plug = self
            .plugin
            .lock()
            .map_err(|_| RecordingError::Plugin("VST2 plugin mutex poisoned".into()))?;

        // Plane 0 := L (or mono). Plane 1 (if present) := R, or mono duplicated. Any
        // remaining input planes (e.g. side-chain inputs on plane 2/3 of a compressor)
        // get silence — we don't expose a side-chain bus.
        let data = &input.data;
        if let Some(plane) = sc.inputs.get_mut(0) {
            match ch {
                2 => {
                    for f in 0..frames {
                        plane[f] = data[f * 2];
                    }
                }
                1 => {
                    plane[..frames].copy_from_slice(&data[..frames]);
                }
                _ => unreachable!(),
            }
        }
        if let Some(plane) = sc.inputs.get_mut(1) {
            match ch {
                2 => {
                    for f in 0..frames {
                        plane[f] = data[f * 2 + 1];
                    }
                }
                1 => {
                    plane[..frames].copy_from_slice(&data[..frames]);
                }
                _ => unreachable!(),
            }
        }
        for plane in sc.inputs.iter_mut().skip(2) {
            // Side-chain / aux inputs: silence each block. Plugins are spec'd to not
            // mutate input planes, so a single zero-fill at construction would suffice in
            // theory, but a misbehaving plugin could leave non-zero residue between blocks.
            for x in plane[..frames].iter_mut() {
                *x = 0.0;
            }
        }
        // Pre-zero outputs: VST2 `processReplacing` is supposed to fully overwrite, but a
        // plugin returning garbage on planes it doesn't drive (multi-out routers, etc.)
        // should at least produce silence on the unused channels.
        for plane in sc.outputs.iter_mut() {
            for x in plane[..frames].iter_mut() {
                *x = 0.0;
            }
        }

        {
            let PlanarScratch { inputs, outputs } = &mut *sc;
            // `bind` panics on size mismatch with the HostBuffer it was constructed with;
            // we collect exactly `plugin_in_channels` / `plugin_out_channels` slices,
            // matching the HostBuffer::new arguments in `from_arc`.
            let in_slices: Vec<&[f32]> = inputs.iter().map(|v| &v[..frames]).collect();
            let mut out_slices: Vec<&mut [f32]> =
                outputs.iter_mut().map(|v| &mut v[..frames]).collect();
            let mut audio_buf = hb.bind(&in_slices, &mut out_slices);
            plug.instance.process(&mut audio_buf);
        }
        self.diag.processed.fetch_add(1, Ordering::Relaxed);
        drop(plug);
        drop(hb);

        // Read first 1-2 output planes back to the recorder's PCM stream. Plugins with no
        // outputs (rare — MIDI tools) produce silence; mono plugins (out_channels == 1)
        // duplicate to L/R when the recorder is stereo.
        let n_out_planes = self.plugin_out_channels.min(sc.outputs.len());
        let take_l = n_out_planes >= 1;
        let take_r_from_plane = if n_out_planes >= 2 { Some(1) } else { None };
        let out_fmt = input.format;
        let mut v = Vec::with_capacity(frames * ch);
        for f in 0..frames {
            let l = if take_l { sc.outputs[0][f] } else { 0.0 };
            let r = match take_r_from_plane {
                Some(idx) => sc.outputs[idx][f],
                None => l,
            };
            match ch {
                2 => {
                    v.push(l);
                    v.push(r);
                }
                1 => {
                    v.push(0.5 * (l + r));
                }
                _ => unreachable!(),
            }
        }
        *output = AudioBuffer::new(
            out_fmt,
            v.into(),
            frames,
            input.captured_at,
            input.frame_index,
        );
        Ok(())
    }
}
