//! VST hosting on Windows. Supports both:
//!
//! * **VST3** via [`rack`](https://crates.io/crates/rack) for capture audio + the `vst3` crate
//!   for the vendor's native `IPlugView` editor (separate Win32 window).
//! * **VST2 (legacy 2.4 .dll)** via [`vst`](https://crates.io/crates/vst) (`vst-rs`)
//!   for capture audio and native editor hosting.
//!
//! Catalog and chain are format-agnostic: the UI sees a single list of `CatalogEntry`
//! values tagged with their format, and chain processors are dispatched to the right
//! backend at audio time via `LoadedPlugin`.

#[path = "vst_native_editor_win.rs"]
mod vst_native_editor_win;

#[path = "vst2.rs"]
pub mod vst2;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

pub use rack::plugin_info::PluginInfo as Vst3PluginInfo;

use egui;
use rack::traits::{PluginInstance, PluginScanner};
use rack::vst3::{Vst3Plugin, Vst3Scanner};
use recorder_core::buffer::AudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::SampleFormat;
use recorder_core::traits::AudioProcessor;

pub use vst2::{Vst2AudioProcessor, Vst2DiagCounters, Vst2HostedPlugin, Vst2PluginInfo};

/// Standard Windows VST2 / VST3 search locations.
///
/// Directory names suggesting VST3-only or VST2-only contents are no longer split — we
/// recurse into each root and probe every plugin-shaped file we find. Users routinely
/// keep mixed-format folders.
pub fn vst_search_directories() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(pf) = std::env::var("ProgramFiles") {
        v.push(PathBuf::from(&pf).join("Common Files").join("VST3"));
        v.push(PathBuf::from(&pf).join("Steinberg").join("VstPlugins"));
        v.push(PathBuf::from(&pf).join("VSTPlugins"));
        v.push(PathBuf::from(&pf).join("REAPER (x64)").join("Plugins"));
    }
    if let Ok(pfx86) = std::env::var("ProgramFiles(x86)") {
        v.push(PathBuf::from(&pfx86).join("Common Files").join("VST3"));
        v.push(PathBuf::from(&pfx86).join("Steinberg").join("VstPlugins"));
        v.push(PathBuf::from(&pfx86).join("VSTPlugins"));
        v.push(PathBuf::from(&pfx86).join("REAPER").join("Plugins"));
    }
    if let Ok(cf) = std::env::var("CommonProgramFiles") {
        v.push(PathBuf::from(&cf).join("VST3"));
    }
    if let Ok(la) = std::env::var("LocalAppData") {
        v.push(
            PathBuf::from(&la)
                .join("Programs")
                .join("Common")
                .join("VST3"),
        );
    }
    v
}

/// Cap recursion so a giant tree under a search root cannot stall the UI.
const SCAN_MAX_DEPTH: usize = 8;

/// Collect `root` and every subdirectory below it (depth-limited), skipping `.vst3` bundle dirs.
/// `.vst3` on Windows can be a single file or a bundle directory; in both cases the scanner
/// picks them up when given the *parent* directory, so we must not recurse into the bundle.
fn collect_scan_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        out.push(dir.clone());
        if depth >= SCAN_MAX_DEPTH {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let is_bundle = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("vst3"))
                .unwrap_or(false);
            if is_bundle {
                continue;
            }
            stack.push((path, depth + 1));
        }
    }
    out
}

/// Counts of plugin-shaped files directly in a single directory (non-recursive).
#[derive(Default, Clone)]
pub struct DirCounts {
    pub vst3_entries: usize,
    pub dll_entries: usize,
}

fn count_entries(dir: &Path) -> DirCounts {
    let mut c = DirCounts::default();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return c;
    };
    for entry in rd.flatten() {
        let Some(ext_owned) = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
        else {
            continue;
        };
        match ext_owned.as_str() {
            "vst3" => c.vst3_entries += 1,
            "dll" => c.dll_entries += 1,
            _ => {}
        }
    }
    c
}

#[derive(Clone)]
pub struct DirReport {
    pub dir: PathBuf,
    pub counts: DirCounts,
    pub vst3_found: usize,
    pub vst2_found: usize,
    pub error: Option<String>,
    /// Per-DLL probe failures, captured so users can see *why* a `.dll` was rejected.
    /// Most are benign (non-VST DLLs like helper libraries); a real plugin failing here
    /// usually indicates an architecture or runtime mismatch.
    pub vst2_failures: Vec<(PathBuf, String)>,
}

pub struct ScanReport {
    pub vst3_plugins: Vec<Vst3PluginInfo>,
    pub vst2_plugins: Vec<Vst2PluginInfo>,
    pub directories: Vec<DirReport>,
}

impl ScanReport {
    /// Total plugins of all formats.
    pub fn total_plugins(&self) -> usize {
        self.vst3_plugins.len() + self.vst2_plugins.len()
    }

    /// Human-readable summary suitable for the scan status label.
    pub fn summary(&self) -> String {
        let scanned = self.directories.len();
        let scanned_with_content = self
            .directories
            .iter()
            .filter(|d| d.counts.vst3_entries + d.counts.dll_entries > 0)
            .count();
        let total_errors = self
            .directories
            .iter()
            .filter(|d| d.error.is_some())
            .count();
        let total_dll_failures: usize =
            self.directories.iter().map(|d| d.vst2_failures.len()).sum();
        let mut s = format!(
            "Scanned {scanned} folder(s) ({scanned_with_content} with audio binaries). \
             Found {} VST3 + {} VST2 = {} plugin(s).",
            self.vst3_plugins.len(),
            self.vst2_plugins.len(),
            self.total_plugins(),
        );
        if total_dll_failures > 0 {
            s.push_str(&format!(
                " {total_dll_failures} .dll(s) skipped (non-VST or load failed; see details)."
            ));
        }
        if total_errors > 0 {
            s.push_str(&format!(" {total_errors} folder(s) failed; see details."));
        }
        s
    }

    /// Multi-line per-folder detail, suitable for a collapsible panel.
    pub fn details(&self) -> String {
        let mut lines = Vec::new();
        for d in &self.directories {
            let err = d
                .error
                .as_ref()
                .map(|e| format!("  ERROR: {e}"))
                .unwrap_or_default();
            lines.push(format!(
                "{}  [vst3={}, dll={}, vst3_found={}, vst2_found={}]{}",
                d.dir.display(),
                d.counts.vst3_entries,
                d.counts.dll_entries,
                d.vst3_found,
                d.vst2_found,
                err
            ));
            for (path, e) in &d.vst2_failures {
                lines.push(format!("    .dll skipped: {} ({e})", path.display()));
            }
        }
        lines.join("\n")
    }
}

/// Walk every search directory and probe each one for both VST3 and VST2 plugins.
pub fn scan_all_plugins_with_report() -> std::result::Result<ScanReport, String> {
    let scanner = Vst3Scanner::new().map_err(|e| format!("{e:?}"))?;
    let mut seen_vst3: HashSet<(PathBuf, String)> = HashSet::new();
    let mut seen_vst2: HashSet<(PathBuf, i32)> = HashSet::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut report = ScanReport {
        vst3_plugins: Vec::new(),
        vst2_plugins: Vec::new(),
        directories: Vec::new(),
    };
    for root in vst_search_directories() {
        for dir in collect_scan_dirs(&root) {
            let canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if !visited.insert(canon) {
                continue;
            }
            let counts = count_entries(&dir);
            let mut vst3_found = 0usize;
            let mut vst2_found = 0usize;
            let mut err_msg: Option<String> = None;

            // VST3 scan via rack.
            match scanner.scan_path(&dir) {
                Ok(batch) => {
                    for p in batch {
                        let key = (p.path.clone(), p.unique_id.clone());
                        if seen_vst3.insert(key) {
                            report.vst3_plugins.push(p);
                            vst3_found += 1;
                        }
                    }
                }
                Err(e) => {
                    err_msg = Some(format!("vst3 scan: {e:?}"));
                }
            }

            // VST2 probe of every .dll in this dir (non-recursive; subdirs are handled
            // by the outer loop).
            let (vst2_plugins, vst2_failures) = vst2::scan_dir_for_vst2(&dir);
            for info in vst2_plugins {
                let key = info.dedup_key();
                if seen_vst2.insert(key) {
                    report.vst2_plugins.push(info);
                    vst2_found += 1;
                }
            }

            report.directories.push(DirReport {
                dir,
                counts,
                vst3_found,
                vst2_found,
                error: err_msg,
                vst2_failures,
            });
        }
    }
    Ok(report)
}

fn rack_err(e: rack::Error) -> RecordingError {
    RecordingError::Plugin(format!("{e:?}"))
}

pub fn load_and_init_vst3(
    info: &Vst3PluginInfo,
    sample_rate_hz: u32,
    max_block: usize,
) -> std::result::Result<Arc<Mutex<Vst3Plugin>>, String> {
    let scanner = Vst3Scanner::new().map_err(|e| format!("{e:?}"))?;
    let mut plugin = scanner.load(info).map_err(|e| format!("{e:?}"))?;
    plugin
        .initialize(sample_rate_hz as f64, max_block)
        .map_err(|e| format!("{e:?}"))?;
    Ok(Arc::new(Mutex::new(plugin)))
}

struct StereoScratch {
    il: Vec<f32>,
    ir: Vec<f32>,
    ol: Vec<f32>,
    or: Vec<f32>,
}

/// Stereo VST3 effect wrapper (mono capture is duplicated to L/R).
pub struct Vst3AudioProcessor {
    plugin: Arc<Mutex<Vst3Plugin>>,
    label: String,
    max_block: usize,
    scratch: Mutex<StereoScratch>,
    editor_audio_tx: Option<SyncSender<AudioBuffer>>,
}

#[allow(dead_code)]
impl Vst3AudioProcessor {
    pub fn new(
        info: &Vst3PluginInfo,
        sample_rate_hz: u32,
        max_block: usize,
    ) -> std::result::Result<Self, String> {
        let plugin = load_and_init_vst3(info, sample_rate_hz, max_block)?;
        Ok(Self {
            plugin,
            label: info.name.clone(),
            max_block,
            scratch: Mutex::new(StereoScratch {
                il: vec![0.0; max_block],
                ir: vec![0.0; max_block],
                ol: vec![0.0; max_block],
                or: vec![0.0; max_block],
            }),
            editor_audio_tx: None,
        })
    }

    pub fn from_arc(
        plugin: Arc<Mutex<Vst3Plugin>>,
        label: String,
        max_block: usize,
        editor_audio_tx: Option<SyncSender<AudioBuffer>>,
    ) -> Self {
        Self {
            plugin,
            label,
            max_block,
            scratch: Mutex::new(StereoScratch {
                il: vec![0.0; max_block],
                ir: vec![0.0; max_block],
                ol: vec![0.0; max_block],
                or: vec![0.0; max_block],
            }),
            editor_audio_tx,
        }
    }

    pub fn plugin_arc(&self) -> Arc<Mutex<Vst3Plugin>> {
        self.plugin.clone()
    }

    pub fn display_label(&self) -> &str {
        &self.label
    }
}

impl AudioProcessor for Vst3AudioProcessor {
    fn name(&self) -> &str {
        &self.label
    }

    fn reset(&mut self) {
        if let Ok(mut g) = self.plugin.lock() {
            let _ = PluginInstance::reset(&mut *g);
        }
    }

    fn process(&mut self, input: &AudioBuffer, output: &mut AudioBuffer) -> Result<()> {
        if input.format.sample_format != SampleFormat::F32 {
            return Err(RecordingError::Plugin(
                "VST3 path expects F32 buffers".into(),
            ));
        }
        let frames = input.frames;
        if frames == 0 {
            *output = AudioBuffer::silent(input.format, 0, input.captured_at, input.frame_index);
            return Ok(());
        }
        if frames > self.max_block {
            return Err(RecordingError::Plugin(format!(
                "block {} exceeds VST init {}",
                frames, self.max_block
            )));
        }

        let ch = input.format.channels as usize;
        let mut sc = self
            .scratch
            .lock()
            .map_err(|_| RecordingError::Plugin("VST scratch mutex poisoned".into()))?;
        let data = &input.data;
        let StereoScratch {
            il: ref mut sil,
            ir: ref mut sir,
            ol: ref mut sol,
            or: ref mut sor,
        } = &mut *sc;
        let il = &mut sil[..frames];
        let ir = &mut sir[..frames];
        let ol = &mut sol[..frames];
        let or = &mut sor[..frames];
        match ch {
            2 => {
                for f in 0..frames {
                    il[f] = data[f * 2];
                    ir[f] = data[f * 2 + 1];
                }
            }
            1 => {
                for f in 0..frames {
                    let m = data[f];
                    il[f] = m;
                    ir[f] = m;
                }
            }
            _ => {
                return Err(RecordingError::Plugin(format!(
                    "VST demo supports 1–2 input channels; got {ch}"
                )));
            }
        }

        let mut plug = self
            .plugin
            .lock()
            .map_err(|_| RecordingError::Plugin("VST3 plugin mutex poisoned".into()))?;

        PluginInstance::process(&mut *plug, &[il, ir], &mut [ol, or], frames).map_err(rack_err)?;
        if let Some(tx) = &self.editor_audio_tx {
            let _ = tx.try_send(input.clone());
        }

        let out_fmt = input.format;
        *output = AudioBuffer::new(
            out_fmt,
            match ch {
                2 => {
                    let mut v = Vec::with_capacity(frames * 2);
                    for f in 0..frames {
                        v.push(ol[f]);
                        v.push(or[f]);
                    }
                    v.into()
                }
                1 => {
                    let mut v = Vec::with_capacity(frames);
                    for f in 0..frames {
                        v.push(0.5 * (ol[f] + or[f]));
                    }
                    v.into()
                }
                _ => unreachable!(),
            },
            frames,
            input.captured_at,
            input.frame_index,
        );
        Ok(())
    }
}

/// Re-initialize all plugin instances when sample rate or block size changes.
///
/// Takes `&mut [ChainEntry]` so we can recover from a panic-induced `Mutex` poisoning by
/// reloading the affected VST2 plugin in place. Without this, a single panic inside
/// `vst-rs::process` would permanently brick recording for that chain entry — every
/// subsequent `arc.lock()` would surface `PoisonError` and the user would have to remove
/// and re-add the plugin to recover.
pub fn reinit_plugin_chain(
    chain: &mut [ChainEntry],
    sample_rate_hz: u32,
    max_block: usize,
) -> std::result::Result<(), String> {
    // Step 1: detect any poisoned VST2 instances and reload them from the catalog. We do
    // this before collecting the audio-side `Arc<Mutex<...>>` references so the rebuilt
    // chain points at fresh, healthy plugins.
    for entry in chain.iter_mut() {
        let LoadedPlugin::Vst2(arc) = &entry.loaded else {
            continue;
        };
        if !arc.is_poisoned() {
            continue;
        }
        let CatalogEntry::Vst2(info) = &entry.catalog else {
            continue; // catalog/loaded mismatch shouldn't happen, but skip silently
        };
        eprintln!(
            "VST2 mutex for {:?} poisoned (likely panicked mid-process); reloading from {}",
            info.name,
            info.path.display()
        );
        let fresh = vst2::load_and_init_vst2(info, sample_rate_hz, max_block)
            .map_err(|e| format!("reload {}: {}", info.name, e))?;
        entry.loaded = LoadedPlugin::Vst2(fresh);
    }

    let mut vst3_plugins: Vec<Arc<Mutex<Vst3Plugin>>> = Vec::new();
    let mut vst2_plugins: Vec<Arc<Mutex<Vst2HostedPlugin>>> = Vec::new();
    for entry in chain.iter() {
        match &entry.loaded {
            LoadedPlugin::Vst3(arc) => vst3_plugins.push(arc.clone()),
            LoadedPlugin::Vst2(arc) => vst2_plugins.push(arc.clone()),
        }
    }
    for p in &vst3_plugins {
        let mut g = p.lock().map_err(|e| format!("{e:?}"))?;
        PluginInstance::initialize(&mut *g, sample_rate_hz as f64, max_block)
            .map_err(|e| format!("{e:?}"))?;
    }
    vst2::reinit_vst2(&vst2_plugins, sample_rate_hz, max_block)?;
    Ok(())
}

/// Conservative upper bound for host callback sizes (Windows backends vary).
pub fn max_block_for_sample_rate(sample_rate_hz: u32) -> usize {
    ((sample_rate_hz as usize).saturating_mul(3) / 10)
        .max(4096)
        .min(65_536)
}

pub const DEFAULT_INIT_SR: u32 = 48_000;

/// A single catalog entry covers either format. `dedup_key()` is responsible for global
/// uniqueness (so the same `.dll` cannot appear twice across roots).
#[derive(Debug, Clone)]
pub enum CatalogEntry {
    Vst3(Vst3PluginInfo),
    Vst2(Vst2PluginInfo),
}

impl CatalogEntry {
    pub fn name(&self) -> &str {
        match self {
            CatalogEntry::Vst3(p) => p.name.as_str(),
            CatalogEntry::Vst2(p) => p.name.as_str(),
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            CatalogEntry::Vst3(p) => p.path.as_path(),
            CatalogEntry::Vst2(p) => p.path.as_path(),
        }
    }

    pub fn format_label(&self) -> &'static str {
        match self {
            CatalogEntry::Vst3(_) => "VST3",
            CatalogEntry::Vst2(_) => "VST2",
        }
    }

    /// Whether this entry's vendor UI can be hosted in a separate window.
    ///
    /// Both VST3 (`IPlugView`) and VST2 (`effEditOpen`) editors are supported. The
    /// per-entry runtime check (does the plugin actually advertise an editor?) lives in
    /// `start_native_editor` since `Vst2PluginInfo` doesn't surface that flag at scan time
    /// and we'd rather attempt-and-recover than gate the button optimistically.
    pub fn supports_native_editor(&self) -> bool {
        matches!(self, CatalogEntry::Vst3(_) | CatalogEntry::Vst2(_))
    }
}

/// A loaded plugin retained by the UI. `Arc<Mutex<...>>` is shared with capture
/// `AudioProcessor`s so that parameter mirroring (VST3) and stop-time teardown work.
#[derive(Clone)]
pub enum LoadedPlugin {
    Vst3(Arc<Mutex<Vst3Plugin>>),
    Vst2(Arc<Mutex<Vst2HostedPlugin>>),
}

#[derive(Clone)]
pub struct ChainEntry {
    pub catalog: CatalogEntry,
    pub loaded: LoadedPlugin,
    pub bypassed: bool,
    pub vst3_editor_audio_tx: Option<SyncSender<AudioBuffer>>,
    /// Live diagnostics shared with the chain's `Vst2AudioProcessor` so the UI can show
    /// whether the audio thread is actually calling the plugin and how many buffers it has
    /// processed. Always present (default zeros) for VST2 entries; left at default for
    /// VST3 entries since they have a separate editor-side processor.
    pub vst2_diag: Arc<Vst2DiagCounters>,
}

/// UI state: scanned catalog and loaded plugin chain (shared `Arc` with processors while recording).
#[derive(Default)]
pub struct VstUiState {
    pub catalog: Vec<CatalogEntry>,
    pub chain: Vec<ChainEntry>,
    pub catalog_pick: usize,
    pub scan_error: Option<String>,
    /// One-line scan summary (always populated after a scan succeeds).
    pub scan_summary: Option<String>,
    /// Per-folder details for the collapsible diagnostics panel.
    pub scan_details: Option<String>,
    pub show_scan_details: bool,
    pub editor_open: Vec<bool>,
    /// HWND of the native editor (0 if none), for posting `WM_CLOSE`.
    pub native_hwnd: Vec<Arc<AtomicIsize>>,
    pub native_threads: Vec<Option<std::thread::JoinHandle<()>>>,
}

impl VstUiState {
    fn trim_editor_flags(&mut self) {
        self.editor_open.resize(self.chain.len(), false);
        self.native_hwnd.truncate(self.chain.len());
        self.native_threads.truncate(self.chain.len());
    }

    pub fn build_processor_chain(&self, max_block: usize) -> Vec<Box<dyn AudioProcessor + Send>> {
        self.chain
            .iter()
            .filter(|entry| !entry.bypassed)
            .map(|entry| match &entry.loaded {
                LoadedPlugin::Vst3(arc) => Box::new(Vst3AudioProcessor::from_arc(
                    arc.clone(),
                    entry.catalog.name().to_string(),
                    max_block,
                    entry.vst3_editor_audio_tx.clone(),
                )) as Box<dyn AudioProcessor + Send>,
                LoadedPlugin::Vst2(arc) => Box::new(Vst2AudioProcessor::from_arc_with_diag(
                    arc.clone(),
                    entry.catalog.name().to_string(),
                    max_block,
                    entry.vst2_diag.clone(),
                )) as Box<dyn AudioProcessor + Send>,
            })
            .collect()
    }

    pub fn scan_catalog(&mut self) {
        self.scan_error = None;
        self.scan_summary = None;
        self.scan_details = None;
        match scan_all_plugins_with_report() {
            Ok(report) => {
                self.scan_summary = Some(report.summary());
                self.scan_details = Some(report.details());
                let mut catalog: Vec<CatalogEntry> = Vec::new();
                catalog.extend(report.vst3_plugins.into_iter().map(CatalogEntry::Vst3));
                catalog.extend(report.vst2_plugins.into_iter().map(CatalogEntry::Vst2));
                // Sort by name for predictable display, with format as tiebreaker.
                catalog.sort_by(|a, b| {
                    a.name()
                        .to_ascii_lowercase()
                        .cmp(&b.name().to_ascii_lowercase())
                        .then(a.format_label().cmp(b.format_label()))
                });
                self.catalog = catalog;
                if self.catalog_pick >= self.catalog.len() {
                    self.catalog_pick = 0;
                }
            }
            Err(e) => self.scan_error = Some(e),
        }
    }

    pub fn add_pick_to_chain(&mut self) -> std::result::Result<(), String> {
        let Some(entry) = self.catalog.get(self.catalog_pick).cloned() else {
            return Err("No plugin selected.".into());
        };
        let max_block = max_block_for_sample_rate(DEFAULT_INIT_SR);
        let loaded = match &entry {
            CatalogEntry::Vst3(info) => {
                let arc = load_and_init_vst3(info, DEFAULT_INIT_SR, max_block)?;
                LoadedPlugin::Vst3(arc)
            }
            CatalogEntry::Vst2(info) => {
                let arc = vst2::load_and_init_vst2(info, DEFAULT_INIT_SR, max_block)?;
                LoadedPlugin::Vst2(arc)
            }
        };
        self.chain.push(ChainEntry {
            catalog: entry,
            loaded,
            bypassed: false,
            vst3_editor_audio_tx: None,
            vst2_diag: Arc::new(Vst2DiagCounters::default()),
        });
        self.native_hwnd.push(Arc::new(AtomicIsize::new(0)));
        self.native_threads.push(None);
        self.trim_editor_flags();
        Ok(())
    }

    pub fn remove_chain(&mut self, index: usize) {
        if index < self.chain.len() {
            self.stop_native_editor(index);
            self.chain.remove(index);
            if index < self.editor_open.len() {
                self.editor_open.remove(index);
            }
            if index < self.native_hwnd.len() {
                self.native_hwnd.remove(index);
            }
            if index < self.native_threads.len() {
                self.native_threads.remove(index);
            }
            self.trim_editor_flags();
        }
    }

    pub fn move_chain(&mut self, index: usize, delta: isize) {
        let j = index as isize + delta;
        if j < 0 || j >= self.chain.len() as isize {
            return;
        }
        let j = j as usize;
        self.chain.swap(index, j);
        if index < self.editor_open.len() && j < self.editor_open.len() {
            self.editor_open.swap(index, j);
        }
        if index < self.native_hwnd.len() && j < self.native_hwnd.len() {
            self.native_hwnd.swap(index, j);
        }
        if index < self.native_threads.len() && j < self.native_threads.len() {
            self.native_threads.swap(index, j);
        }
        self.trim_editor_flags();
    }

    pub fn move_chain_to(&mut self, from: usize, to: usize) {
        if from >= self.chain.len() || to >= self.chain.len() || from == to {
            return;
        }
        let entry = self.chain.remove(from);
        self.chain.insert(to, entry);
        if from < self.editor_open.len() {
            let open = self.editor_open.remove(from);
            self.editor_open.insert(to, open);
        }
        if from < self.native_hwnd.len() {
            let hwnd = self.native_hwnd.remove(from);
            self.native_hwnd.insert(to, hwnd);
        }
        if from < self.native_threads.len() {
            let thread = self.native_threads.remove(from);
            self.native_threads.insert(to, thread);
        }
        self.trim_editor_flags();
    }

    pub fn toggle_bypass(&mut self, index: usize) {
        if let Some(entry) = self.chain.get_mut(index) {
            entry.bypassed = !entry.bypassed;
        }
    }

    pub fn toggle_native_editor(&mut self, index: usize) {
        let Some(entry) = self.chain.get(index) else {
            return;
        };
        if !entry.catalog.supports_native_editor() {
            return;
        }
        let cur = self.editor_open.get(index).copied().unwrap_or(false);
        if cur {
            self.stop_native_editor(index);
            if let Some(v) = self.editor_open.get_mut(index) {
                *v = false;
            }
        } else {
            self.start_native_editor(index);
            if let Some(v) = self.editor_open.get_mut(index) {
                *v = true;
            }
        }
    }

    fn start_native_editor(&mut self, index: usize) {
        if self
            .native_threads
            .get(index)
            .and_then(|t| t.as_ref())
            .is_some()
        {
            return;
        }
        let entry = self.chain[index].clone();
        while self.native_hwnd.len() <= index {
            self.native_hwnd.push(Arc::new(AtomicIsize::new(0)));
        }
        while self.native_threads.len() <= index {
            self.native_threads.push(None);
        }
        let hwnd_slot = self.native_hwnd[index].clone();
        let handle = match (&entry.catalog, &entry.loaded) {
            (CatalogEntry::Vst3(info), LoadedPlugin::Vst3(arc)) => {
                let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel(4);
                if let Some(entry) = self.chain.get_mut(index) {
                    entry.vst3_editor_audio_tx = Some(audio_tx);
                }
                let bundle = info.path.clone();
                let uid = info.unique_id.clone();
                let title = format!("{} — VST3", info.name);
                let rack = arc.clone();
                std::thread::spawn(move || {
                    vst_native_editor_win::run_vst3_editor_thread(
                        bundle, uid, title, rack, hwnd_slot, audio_rx,
                    );
                })
            }
            (CatalogEntry::Vst2(info), LoadedPlugin::Vst2(arc)) => {
                let title = format!("{} — VST2", info.name);
                let plugin = arc.clone();
                std::thread::spawn(move || {
                    vst_native_editor_win::run_vst2_editor_thread(plugin, title, hwnd_slot);
                })
            }
            // Catalog/loaded mismatch should never happen — `add_pick_to_chain` builds them
            // from the same `CatalogEntry`. Nothing to spawn if the invariant is ever broken.
            _ => return,
        };
        self.native_threads[index] = Some(handle);
    }

    pub fn stop_native_editor(&mut self, index: usize) {
        if let Some(entry) = self.chain.get_mut(index) {
            entry.vst3_editor_audio_tx = None;
        }
        if let Some(slot) = self.native_hwnd.get(index) {
            let v = slot.load(Ordering::Acquire);
            vst_native_editor_win::post_close_native_editor(v);
        }
        if let Some(Some(h)) = self.native_threads.get_mut(index).map(|t| t.take()) {
            let _ = h.join();
        }
    }

    /// If the spawned editor thread has already exited (e.g. a VST2 plugin that turned out
    /// to have no editor, or the user closed the vendor window directly), reap it and clear
    /// the matching `editor_open` flag so the companion egui window doesn't get stuck.
    fn reap_finished_editors(&mut self) {
        for i in 0..self.chain.len() {
            let finished = self
                .native_threads
                .get(i)
                .and_then(|t| t.as_ref())
                .map(|t| t.is_finished())
                .unwrap_or(false);
            if finished {
                if let Some(Some(h)) = self.native_threads.get_mut(i).map(|t| t.take()) {
                    let _ = h.join();
                }
                if let Some(v) = self.editor_open.get_mut(i) {
                    *v = false;
                }
                if let Some(slot) = self.native_hwnd.get(i) {
                    slot.store(0, Ordering::Release);
                }
            }
        }
    }

    /// Companion panel: explains native window + offers close (vendor UI is not embedded in egui).
    pub fn draw_native_editor_ui(&mut self, ctx: &egui::Context) {
        self.trim_editor_flags();
        self.reap_finished_editors();
        for i in 0..self.chain.len() {
            if !self.editor_open.get(i).copied().unwrap_or(false) {
                continue;
            }
            let label = self.chain[i].catalog.name().to_string();
            let fmt = self.chain[i].catalog.format_label();
            let mut win_open = true;
            let close_click = egui::Window::new(format!("{label} — {fmt}"))
                .id(egui::Id::new(("vst_native_editor", i)))
                .open(&mut win_open)
                .resizable(true)
                .default_width(420.0)
                .show(ctx, |ui| {
                    ui.label(
                        "The plugin's own editor is shown in the separate Windows window that opened.",
                    );
                    match fmt {
                        "VST3" => {
                            ui.label(
                                "Changes are applied to the recording chain by mirroring parameters to the rack instance about every 50 ms.",
                            );
                        }
                        "VST2" => {
                            ui.label(
                                "The editor and the audio chain share the same plugin instance, so parameter changes take effect immediately on the next audio block.",
                            );
                        }
                        _ => {}
                    }
                    ui.button("Close vendor window").clicked()
                })
                .map_or(false, |r| r.inner.unwrap_or(false));
            if close_click || !win_open {
                self.stop_native_editor(i);
                if let Some(v) = self.editor_open.get_mut(i) {
                    *v = false;
                }
            }
        }
    }
}

#[cfg(all(test, windows, feature = "vst"))]
mod reaeq_probe_tests {
    use super::*;
    use recorder_core::buffer::AudioBuffer;
    use recorder_core::format::{AudioFormat, SampleFormat};
    use recorder_core::pipeline::{PipelineConfig, PipelineMetrics, StreamPipeline};
    use recorder_core::traits::AudioProcessor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn reaeq_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("REAEQ_DLL") {
            return Some(PathBuf::from(path));
        }
        [
            r"C:\Program Files\VSTPlugins\ReaPlugs\reaeq-standalone.dll",
            r"C:\Program Files\REAPER (x64)\Plugins\FX\reaeq.dll",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    }

    #[test]
    #[ignore = "manual native UI probe; set REAEQ_DLL to override the detected ReaEQ path"]
    fn reaeq_editor_receives_sine_tone() {
        let path = reaeq_path().expect("ReaEQ DLL not found");
        eprintln!("loading ReaEQ from {}", path.display());
        let info = vst2::probe_vst2(&path).expect("probe ReaEQ");
        let sample_rate = 48_000u32;
        let block = 512usize;
        let plugin = vst2::load_and_init_vst2(&info, sample_rate, block).expect("load ReaEQ");
        let hwnd_slot = Arc::new(AtomicIsize::new(0));
        let editor_plugin = plugin.clone();
        let editor_hwnd = hwnd_slot.clone();
        let editor = std::thread::spawn(move || {
            vst_native_editor_win::run_vst2_editor_thread(
                editor_plugin,
                "ReaEQ sine probe".to_string(),
                editor_hwnd,
            );
        });

        let wait_start = Instant::now();
        while hwnd_slot.load(Ordering::Acquire) == 0
            && wait_start.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let hwnd = hwnd_slot.load(Ordering::Acquire);
        eprintln!("ReaEQ editor HWND: {hwnd}");
        assert_ne!(hwnd, 0, "editor window did not open");

        let mut processor = vst2::Vst2AudioProcessor::from_arc(plugin, info.name.clone(), block);
        let format = AudioFormat::new(sample_rate, 2, SampleFormat::F32);
        let mut phase = 0.0f32;
        let phase_inc = 440.0f32 * std::f32::consts::TAU / sample_rate as f32;
        let run_until = Instant::now() + Duration::from_secs(30);
        let mut frame_index = 0u64;
        while Instant::now() < run_until {
            let mut data = Vec::with_capacity(block * 2);
            for _ in 0..block {
                let sample = phase.sin() * 0.5;
                phase = (phase + phase_inc) % std::f32::consts::TAU;
                data.push(sample);
                data.push(sample);
            }
            let input = AudioBuffer::new(format, data.into(), block, Instant::now(), frame_index);
            let mut output = AudioBuffer::silent(format, block, Instant::now(), frame_index);
            processor
                .process(&input, &mut output)
                .expect("process ReaEQ");
            frame_index += block as u64;
            std::thread::sleep(Duration::from_secs_f64(block as f64 / sample_rate as f64));
        }

        vst_native_editor_win::post_close_native_editor(hwnd_slot.load(Ordering::Acquire));
        let _ = editor.join();
    }

    /// Reproduce the original UI bug: open the ReaEQ editor and then immediately call the
    /// VST2 reinit cycle (suspend → set rate → resume → start_process) on the same plugin
    /// instance, mirroring what the demo's `restart_live_input_stream()` was doing on every
    /// editor toggle. Several stock analyzers (ReaEQ, ReaXcomp, etc.) freeze their display
    /// once you suspend the plugin while the editor is attached, so this is the regression
    /// the demo's editor-toggle path was hitting.
    #[test]
    #[ignore = "manual: regression for editor-toggle reinit cycle freezing ReaEQ analyzer"]
    fn reaeq_editor_survives_reinit_cycle_while_open() {
        let path = reaeq_path().expect("ReaEQ DLL not found");
        eprintln!("loading ReaEQ from {}", path.display());
        let info = vst2::probe_vst2(&path).expect("probe ReaEQ");
        let sample_rate = 48_000u32;
        let block = 512usize;
        let plugin = vst2::load_and_init_vst2(&info, sample_rate, block).expect("load ReaEQ");
        let hwnd_slot = Arc::new(AtomicIsize::new(0));
        let editor_plugin = plugin.clone();
        let editor_hwnd = hwnd_slot.clone();
        let editor = std::thread::spawn(move || {
            vst_native_editor_win::run_vst2_editor_thread(
                editor_plugin,
                "ReaEQ reinit-while-open probe".to_string(),
                editor_hwnd,
            );
        });
        let wait_start = Instant::now();
        while hwnd_slot.load(Ordering::Acquire) == 0
            && wait_start.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let hwnd = hwnd_slot.load(Ordering::Acquire);
        eprintln!("ReaEQ editor HWND: {hwnd}");
        assert_ne!(hwnd, 0, "editor window did not open");

        let mut processor =
            vst2::Vst2AudioProcessor::from_arc(plugin.clone(), info.name.clone(), block);
        let format = AudioFormat::new(sample_rate, 2, SampleFormat::F32);
        let phase_inc = 440.0f32 * std::f32::consts::TAU / sample_rate as f32;
        let mut phase = 0.0f32;
        let mut frame_index = 0u64;

        let drive_for = |processor: &mut vst2::Vst2AudioProcessor,
                         phase: &mut f32,
                         frame_index: &mut u64,
                         dur: Duration| {
            let until = Instant::now() + dur;
            while Instant::now() < until {
                let mut data = Vec::with_capacity(block * 2);
                for _ in 0..block {
                    let sample = phase.sin() * 0.5;
                    *phase = (*phase + phase_inc) % std::f32::consts::TAU;
                    data.push(sample);
                    data.push(sample);
                }
                let input =
                    AudioBuffer::new(format, data.into(), block, Instant::now(), *frame_index);
                let mut output = AudioBuffer::silent(format, block, Instant::now(), *frame_index);
                processor
                    .process(&input, &mut output)
                    .expect("process ReaEQ");
                *frame_index += block as u64;
                std::thread::sleep(Duration::from_secs_f64(block as f64 / sample_rate as f64));
            }
        };

        eprintln!("phase 1: feeding tone before reinit (analyzer should show signal)");
        drive_for(
            &mut processor,
            &mut phase,
            &mut frame_index,
            Duration::from_secs(8),
        );

        eprintln!(
            "phase 2: triggering vst2::reinit_vst2 with editor open (matches demo's restart_live_input_stream after toggle_native_editor)"
        );
        vst2::reinit_vst2(&[plugin.clone()], sample_rate, block).expect("reinit ReaEQ");

        eprintln!("phase 3: feeding tone after reinit (this is where the demo bug shows up)");
        drive_for(
            &mut processor,
            &mut phase,
            &mut frame_index,
            Duration::from_secs(12),
        );

        vst_native_editor_win::post_close_native_editor(hwnd_slot.load(Ordering::Acquire));
        let _ = editor.join();
    }

    /// Same plugin path, but ingest through `StreamPipeline` so the buffer flow matches the
    /// demo (Meter ➜ VST chain ➜ pipeline timeouts) instead of calling `Vst2AudioProcessor`
    /// directly.
    #[test]
    #[ignore = "manual: drives ReaEQ through StreamPipeline like the demo app"]
    fn reaeq_editor_receives_signal_through_pipeline() {
        let path = reaeq_path().expect("ReaEQ DLL not found");
        eprintln!("loading ReaEQ from {}", path.display());
        let info = vst2::probe_vst2(&path).expect("probe ReaEQ");
        let sample_rate = 48_000u32;
        let block = 512usize;
        let plugin = vst2::load_and_init_vst2(&info, sample_rate, block).expect("load ReaEQ");
        let hwnd_slot = Arc::new(AtomicIsize::new(0));
        let editor_plugin = plugin.clone();
        let editor_hwnd = hwnd_slot.clone();
        let editor = std::thread::spawn(move || {
            vst_native_editor_win::run_vst2_editor_thread(
                editor_plugin,
                "ReaEQ pipeline probe".to_string(),
                editor_hwnd,
            );
        });
        let wait_start = Instant::now();
        while hwnd_slot.load(Ordering::Acquire) == 0
            && wait_start.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let hwnd = hwnd_slot.load(Ordering::Acquire);
        eprintln!("ReaEQ editor HWND: {hwnd}");
        assert_ne!(hwnd, 0, "editor window did not open");

        struct PeakProbe(Arc<AtomicU32>);
        impl AudioProcessor for PeakProbe {
            fn name(&self) -> &str {
                "peak-probe"
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
                    .fold(0.0f32, |max, s| max.max(s.abs()));
                self.0.store(peak.to_bits(), Ordering::Release);
                output.format = input.format;
                output.frames = input.frames;
                output.captured_at = input.captured_at;
                output.frame_index = input.frame_index;
                output.data = input.data.clone();
                Ok(())
            }
        }

        let format = AudioFormat::new(sample_rate, 2, SampleFormat::F32);
        let pre_peak = Arc::new(AtomicU32::new(0));
        let post_peak = Arc::new(AtomicU32::new(0));
        let processors: Vec<Box<dyn AudioProcessor + Send>> = vec![
            Box::new(PeakProbe(pre_peak.clone())),
            Box::new(vst2::Vst2AudioProcessor::from_arc(
                plugin,
                info.name.clone(),
                block,
            )),
            Box::new(PeakProbe(post_peak.clone())),
        ];
        let metrics = Arc::new(PipelineMetrics::default());
        let pipeline = StreamPipeline::new(
            PipelineConfig {
                format,
                raw_queue_capacity: 16,
                processed_queue_capacity: 16,
                analyzer_queue_capacity: 16,
                plugin_budget_per_plugin: Some(Duration::from_millis(5)),
            },
            None,
            None,
            Vec::new(),
            processors,
            metrics.clone(),
        );

        let mut phase = 0.0f32;
        let phase_inc = 440.0f32 * std::f32::consts::TAU / sample_rate as f32;
        let run_until = Instant::now() + Duration::from_secs(20);
        let mut frame_index = 0u64;
        while Instant::now() < run_until {
            let mut data = Vec::with_capacity(block * 2);
            for _ in 0..block {
                let sample = phase.sin() * 0.5;
                phase = (phase + phase_inc) % std::f32::consts::TAU;
                data.push(sample);
                data.push(sample);
            }
            let buf = AudioBuffer::new(format, data.into(), block, Instant::now(), frame_index);
            pipeline.ingest(buf);
            frame_index += block as u64;
            std::thread::sleep(Duration::from_secs_f64(block as f64 / sample_rate as f64));
        }
        eprintln!(
            "pipeline pre={:.4} post={:.4} timeouts={}",
            f32::from_bits(pre_peak.load(Ordering::Acquire)),
            f32::from_bits(post_peak.load(Ordering::Acquire)),
            metrics.plugin_timeouts.load(Ordering::Acquire),
        );

        vst_native_editor_win::post_close_native_editor(hwnd_slot.load(Ordering::Acquire));
        let _ = editor.join();
    }
}
