//! macOS VST native editor support.
//!
//! This module intentionally starts with editor hosting and catalog discovery for the
//! macOS bundle formats used by Open Mixer:
//!
//! - VST3: `.vst3` bundles, loaded through Steinberg's VST3 interfaces.
//! - VST2: legacy `.vst` bundles, loaded through `vst-rs`.
//!
//! The editor views are attached to standalone AppKit `NSWindow`s. The returned windows
//! are retained for the process lifetime for now; this keeps third-party plugin UI state
//! alive without requiring Open Mixer to own raw AppKit/VST pointers.

#![allow(deprecated)]
#![allow(unexpected_cfgs)]

use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};

use libloading::Library;
use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};
use rack::traits::PluginScanner;
use rack::vst3::Vst3Scanner;
use recorder_core::buffer::AudioBuffer;
use vst::host::{Host, HostBuffer, PluginLoader};
use vst::plugin::Plugin;
use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, BusDirections_, IAudioProcessor, IAudioProcessorTrait,
    IComponent, IComponentTrait, IConnectionPoint, IConnectionPointTrait, IEditController,
    IEditControllerTrait, MediaTypes_, ProcessData, ProcessModes_, ProcessSetup,
    SymbolicSampleSizes_,
};
use vst3::Steinberg::{
    kResultOk, IPlugViewTrait, IPluginBaseTrait, IPluginFactory, IPluginFactoryTrait,
};
use vst3::{ComPtr, Interface};

const K_AUDIO: i32 = MediaTypes_::kAudio as i32;
const K_EVENT: i32 = MediaTypes_::kEvent as i32;
const K_INPUT: i32 = BusDirections_::kInput as i32;
const K_OUTPUT: i32 = BusDirections_::kOutput as i32;

static VST3_EDITOR_VIEW_TYPE: &[u8] = b"editor\0";
static VST3_NS_VIEW_PLATFORM: &[u8] = b"NSView\0";

#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

struct MacVst3EditorState {
    _window: *mut Object,
    _view: ComPtr<vst3::Steinberg::IPlugView>,
    _controller: ComPtr<IEditController>,
    _component: ComPtr<IComponent>,
    _factory: ComPtr<IPluginFactory>,
    _library: Library,
    audio: Option<ComPtr<IAudioProcessor>>,
    audio_rx: Receiver<AudioBuffer>,
}

struct MacVst2EditorState {
    _window: *mut Object,
    _editor: Box<dyn vst::editor::Editor>,
    _plugin: vst::host::PluginInstance,
    _loader: PluginLoader<MacVst2Host>,
    audio_rx: Receiver<AudioBuffer>,
    host_buffer: HostBuffer<f32>,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
    input_channels: usize,
    output_channels: usize,
}

pub struct NativeVstEditorHandle {
    audio_tx: SyncSender<AudioBuffer>,
    state: NativeVstEditorState,
}

enum NativeVstEditorState {
    Vst3(Box<MacVst3EditorState>),
    Vst2(Box<MacVst2EditorState>),
}

impl NativeVstEditorHandle {
    pub fn audio_sender(&self) -> SyncSender<AudioBuffer> {
        self.audio_tx.clone()
    }

    pub fn poll_audio(&mut self) {
        match &mut self.state {
            NativeVstEditorState::Vst3(state) => unsafe {
                drain_vst3_editor_audio(state.audio.as_ref(), &state.audio_rx);
            },
            NativeVstEditorState::Vst2(state) => drain_vst2_editor_audio(state),
        }
    }
}

pub fn vst_search_directories() -> Vec<PathBuf> {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let mut dirs = vec![
        PathBuf::from("/Library/Audio/Plug-Ins/VST3"),
        PathBuf::from("/Library/Audio/Plug-Ins/VST"),
    ];
    if let Some(home) = home {
        dirs.push(home.join("Library/Audio/Plug-Ins/VST3"));
        dirs.push(home.join("Library/Audio/Plug-Ins/VST"));
    }
    dirs
}

pub fn open_native_vst_editor(
    path: impl AsRef<Path>,
    title: impl Into<String>,
) -> Result<(), String> {
    let mut handle = open_native_vst_editor_for_live_audio(path, title)?;
    handle.poll_audio();
    Box::leak(Box::new(handle));
    Ok(())
}

pub fn open_native_vst_editor_for_live_audio(
    path: impl AsRef<Path>,
    title: impl Into<String>,
) -> Result<NativeVstEditorHandle, String> {
    let path = path.as_ref();
    let title = title.into();
    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel(8);
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("vst3") => open_vst3_editor_handle(path, &title, audio_tx, audio_rx),
        Some("vst") => open_vst2_editor_handle(path, &title, audio_tx, audio_rx),
        other => Err(format!(
            "unsupported macOS VST bundle extension for {}: {:?}",
            path.display(),
            other
        )),
    }
}

pub fn open_vst3_editor(path: impl AsRef<Path>, title: &str) -> Result<(), String> {
    let mut handle = open_vst3_editor_for_live_audio(path, title)?;
    handle.poll_audio();
    Box::leak(Box::new(handle));
    Ok(())
}

pub fn open_vst3_editor_for_live_audio(
    path: impl AsRef<Path>,
    title: &str,
) -> Result<NativeVstEditorHandle, String> {
    let bundle_path = path.as_ref().to_path_buf();
    let unique_id = find_vst3_unique_id(&bundle_path)?;
    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel(8);
    unsafe { open_vst3_editor_inner(bundle_path, unique_id, title, audio_tx, audio_rx) }
}

pub fn open_vst2_editor(path: impl AsRef<Path>, title: &str) -> Result<(), String> {
    let mut handle = open_vst2_editor_for_live_audio(path, title)?;
    handle.poll_audio();
    Box::leak(Box::new(handle));
    Ok(())
}

pub fn open_vst2_editor_for_live_audio(
    path: impl AsRef<Path>,
    title: &str,
) -> Result<NativeVstEditorHandle, String> {
    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel(8);
    unsafe { open_vst2_editor_inner(path.as_ref().to_path_buf(), title, audio_tx, audio_rx) }
}

fn open_vst3_editor_handle(
    path: &Path,
    title: &str,
    audio_tx: SyncSender<AudioBuffer>,
    audio_rx: Receiver<AudioBuffer>,
) -> Result<NativeVstEditorHandle, String> {
    let bundle_path = path.to_path_buf();
    let unique_id = find_vst3_unique_id(&bundle_path)?;
    unsafe { open_vst3_editor_inner(bundle_path, unique_id, title, audio_tx, audio_rx) }
}

fn open_vst2_editor_handle(
    path: &Path,
    title: &str,
    audio_tx: SyncSender<AudioBuffer>,
    audio_rx: Receiver<AudioBuffer>,
) -> Result<NativeVstEditorHandle, String> {
    unsafe { open_vst2_editor_inner(path.to_path_buf(), title, audio_tx, audio_rx) }
}

fn find_vst3_unique_id(bundle_path: &Path) -> Result<String, String> {
    let scanner = Vst3Scanner::new().map_err(|e| format!("{e:?}"))?;
    let scan_root = bundle_path.parent().unwrap_or(bundle_path);
    let wanted = std::fs::canonicalize(bundle_path).unwrap_or_else(|_| bundle_path.to_path_buf());
    let plugins = scanner.scan_path(scan_root).map_err(|e| format!("{e:?}"))?;
    plugins
        .into_iter()
        .find(|plugin| {
            std::fs::canonicalize(&plugin.path).unwrap_or_else(|_| plugin.path.clone()) == wanted
        })
        .map(|plugin| plugin.unique_id)
        .ok_or_else(|| {
            format!(
                "VST3 plugin not found in scan results: {}",
                bundle_path.display()
            )
        })
}

fn bundle_binary_path(bundle: &Path) -> Result<PathBuf, String> {
    if bundle.is_file() {
        return Ok(bundle.to_path_buf());
    }
    let macos_dir = bundle.join("Contents").join("MacOS");
    let rd =
        std::fs::read_dir(&macos_dir).map_err(|e| format!("list {}: {e}", macos_dir.display()))?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(format!("no plugin binary under {}", macos_dir.display()))
}

fn parse_class_id(uid: &str) -> Result<[i8; 16], String> {
    let hex: String = uid.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return Err(format!(
            "plugin UID must be 32 hex digits (got {} from {:?})",
            hex.len(),
            uid
        ));
    }
    let mut out = [0i8; 16];
    for i in 0..16 {
        let b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("invalid hex in UID {:?}", uid))?;
        out[i] = b as i8;
    }
    Ok(out)
}

unsafe fn get_factory(lib: &Library) -> Result<ComPtr<IPluginFactory>, String> {
    let get_factory: libloading::Symbol<unsafe extern "C" fn() -> *mut IPluginFactory> = lib
        .get(b"GetPluginFactory\0")
        .map_err(|e| format!("GetPluginFactory: {e}"))?;
    let fp = get_factory();
    if fp.is_null() {
        return Err("GetPluginFactory returned null".into());
    }
    ComPtr::from_raw(fp).ok_or_else(|| "invalid IPluginFactory pointer".to_string())
}

unsafe fn get_or_create_controller(
    component: &ComPtr<IComponent>,
    factory: &ComPtr<IPluginFactory>,
) -> Result<Option<ComPtr<IEditController>>, String> {
    if let Some(ctrl) = component.cast::<IEditController>() {
        return Ok(Some(ctrl));
    }
    let mut controller_cid = [0i8; 16];
    if component.getControllerClassId(&mut controller_cid) != kResultOk {
        return Ok(None);
    }
    let mut controller_ptr: *mut IEditController = ptr::null_mut();
    let r = factory.createInstance(
        controller_cid.as_ptr() as *const i8,
        IEditController::IID.as_ptr() as *const i8,
        &mut controller_ptr as *mut _ as *mut _,
    );
    if r != kResultOk || controller_ptr.is_null() {
        return Ok(None);
    }
    let ctrl = ComPtr::from_raw(controller_ptr).ok_or("IEditController wrap failed")?;
    if ctrl.initialize(ptr::null_mut()) != kResultOk {
        return Err("IEditController::initialize failed".into());
    }
    Ok(Some(ctrl))
}

unsafe fn try_connect(component: &ComPtr<IComponent>, controller: &ComPtr<IEditController>) {
    let Some(comp_cp) = component.cast::<IConnectionPoint>() else {
        return;
    };
    let Some(ctrl_cp) = controller.cast::<IConnectionPoint>() else {
        return;
    };
    let _ = comp_cp.connect(ctrl_cp.as_ptr());
    let _ = ctrl_cp.connect(comp_cp.as_ptr());
}

unsafe fn activate_all_buses(component: &ComPtr<IComponent>, media: i32, dir: i32) {
    let n = component.getBusCount(media, dir);
    for i in 0..n {
        let _ = component.activateBus(media, dir, i, 1);
    }
}

unsafe fn open_vst3_editor_inner(
    bundle_path: PathBuf,
    class_id_hex: String,
    title: &str,
    audio_tx: SyncSender<AudioBuffer>,
    audio_rx: Receiver<AudioBuffer>,
) -> Result<NativeVstEditorHandle, String> {
    let binary_path = bundle_binary_path(&bundle_path)?;
    let lib =
        Library::new(&binary_path).map_err(|e| format!("load {}: {e}", binary_path.display()))?;
    let factory = get_factory(&lib)?;
    let cid = parse_class_id(&class_id_hex)?;

    let mut comp_ptr: *mut IComponent = ptr::null_mut();
    let cr = factory.createInstance(
        cid.as_ptr() as *const i8,
        IComponent::IID.as_ptr() as *const i8,
        &mut comp_ptr as *mut _ as *mut _,
    );
    if cr != kResultOk || comp_ptr.is_null() {
        return Err(format!("createInstance IComponent: {cr:#x}"));
    }
    let component = ComPtr::from_raw(comp_ptr).ok_or("IComponent null")?;
    if component.initialize(ptr::null_mut()) != kResultOk {
        return Err("IComponent::initialize failed".into());
    }

    activate_all_buses(&component, K_EVENT, K_INPUT);
    activate_all_buses(&component, K_EVENT, K_OUTPUT);
    activate_all_buses(&component, K_AUDIO, K_INPUT);
    activate_all_buses(&component, K_AUDIO, K_OUTPUT);
    let editor_audio = setup_vst3_editor_audio_processor(&component, 48_000, 4096);

    let controller = get_or_create_controller(&component, &factory)?
        .ok_or_else(|| "no IEditController".to_string())?;
    try_connect(&component, &controller);
    if component.setActive(1) != kResultOk {
        return Err("IComponent::setActive(1) failed".into());
    }

    let view_ptr = controller.createView(VST3_EDITOR_VIEW_TYPE.as_ptr() as *const i8);
    if view_ptr.is_null() {
        return Err("createView(editor) returned null".into());
    }
    let view = ComPtr::from_raw(view_ptr).ok_or("IPlugView wrap failed")?;
    if view.isPlatformTypeSupported(VST3_NS_VIEW_PLATFORM.as_ptr() as *const i8) != kResultOk {
        return Err("plugin editor does not support NSView".into());
    }

    let (width, height) = vst3_view_size(&view);
    let window = create_editor_window(title, width, height)?;
    let content_view: *mut Object = msg_send![window, contentView];
    if content_view.is_null() {
        return Err("NSWindow contentView is null".into());
    }

    let attach = view.attached(
        content_view as *mut c_void,
        VST3_NS_VIEW_PLATFORM.as_ptr() as *const i8,
    );
    if attach != kResultOk {
        return Err(format!("IPlugView::attached failed: {attach:#x}"));
    }

    show_window(window);
    let state = Box::new(MacVst3EditorState {
        _window: window,
        _view: view,
        _controller: controller,
        _component: component,
        _factory: factory,
        _library: lib,
        audio: editor_audio,
        audio_rx,
    });
    Ok(NativeVstEditorHandle {
        audio_tx,
        state: NativeVstEditorState::Vst3(state),
    })
}

unsafe fn setup_vst3_editor_audio_processor(
    component: &ComPtr<IComponent>,
    sample_rate_hz: u32,
    max_block: usize,
) -> Option<ComPtr<IAudioProcessor>> {
    let audio = component.cast::<IAudioProcessor>()?;
    if audio.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as i32) != kResultOk {
        return None;
    }
    let mut setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime as i32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
        maxSamplesPerBlock: max_block as i32,
        sampleRate: sample_rate_hz as f64,
    };
    if audio.setupProcessing(&mut setup) != kResultOk {
        return None;
    }
    if audio.setProcessing(1) != kResultOk {
        return None;
    }
    Some(audio)
}

unsafe fn process_vst3_editor_audio_buffer(audio: &ComPtr<IAudioProcessor>, buffer: &AudioBuffer) {
    let frames = buffer.frames;
    if frames == 0 || frames > i32::MAX as usize {
        return;
    }
    let channels = buffer.format.channels as usize;
    if channels != 1 && channels != 2 {
        return;
    }

    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    match channels {
        1 => {
            left.copy_from_slice(&buffer.data[..frames]);
            right.copy_from_slice(&buffer.data[..frames]);
        }
        2 => {
            for frame in 0..frames {
                left[frame] = buffer.data[frame * 2];
                right[frame] = buffer.data[frame * 2 + 1];
            }
        }
        _ => unreachable!(),
    }

    let mut out_left = vec![0.0f32; frames];
    let mut out_right = vec![0.0f32; frames];
    let mut input_ptrs = vec![left.as_mut_ptr(), right.as_mut_ptr()];
    let mut output_ptrs = vec![out_left.as_mut_ptr(), out_right.as_mut_ptr()];
    let mut inputs = [AudioBusBuffers {
        numChannels: 2,
        silenceFlags: 0,
        __field0: AudioBusBuffers__type0 {
            channelBuffers32: input_ptrs.as_mut_ptr(),
        },
    }];
    let mut outputs = [AudioBusBuffers {
        numChannels: 2,
        silenceFlags: 0,
        __field0: AudioBusBuffers__type0 {
            channelBuffers32: output_ptrs.as_mut_ptr(),
        },
    }];
    let mut data = ProcessData {
        processMode: ProcessModes_::kRealtime as i32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
        numSamples: frames as i32,
        numInputs: 1,
        numOutputs: 1,
        inputs: inputs.as_mut_ptr(),
        outputs: outputs.as_mut_ptr(),
        inputParameterChanges: ptr::null_mut(),
        outputParameterChanges: ptr::null_mut(),
        inputEvents: ptr::null_mut(),
        outputEvents: ptr::null_mut(),
        processContext: ptr::null_mut(),
    };
    let _ = audio.process(&mut data);
}

unsafe fn drain_vst3_editor_audio(
    audio: Option<&ComPtr<IAudioProcessor>>,
    audio_rx: &Receiver<AudioBuffer>,
) {
    let Some(audio) = audio else {
        while audio_rx.try_recv().is_ok() {}
        return;
    };
    loop {
        match audio_rx.try_recv() {
            Ok(buffer) => process_vst3_editor_audio_buffer(audio, &buffer),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
}

unsafe fn vst3_view_size(view: &ComPtr<vst3::Steinberg::IPlugView>) -> (f64, f64) {
    let mut rect = vst3::Steinberg::ViewRect {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    let _ = view.getSize(&mut rect);
    let width = (rect.right - rect.left).max(320) as f64;
    let height = (rect.bottom - rect.top).max(240) as f64;
    (width, height)
}

struct MacVst2Host;

impl Host for MacVst2Host {
    fn get_info(&self) -> (isize, String, String) {
        (1, "recorder".to_string(), "recorder".to_string())
    }

    fn idle(&self) {}
    fn update_display(&self) {}
}

unsafe fn open_vst2_editor_inner(
    bundle_path: PathBuf,
    title: &str,
    audio_tx: SyncSender<AudioBuffer>,
    audio_rx: Receiver<AudioBuffer>,
) -> Result<NativeVstEditorHandle, String> {
    let binary_path = bundle_binary_path(&bundle_path)?;
    let host = Arc::new(Mutex::new(MacVst2Host));
    let mut loader = PluginLoader::load(&binary_path, host)
        .map_err(|e| format!("PluginLoader::load failed: {e:?}"))?;
    let mut plugin = loader
        .instance()
        .map_err(|e| format!("loader.instance() failed: {e:?}"))?;
    plugin.init();
    plugin.set_sample_rate(48_000.0);
    plugin.set_block_size(4096);
    plugin.resume();
    plugin.start_process();
    let info = plugin.get_info();
    let input_channels = info.inputs.max(0) as usize;
    let output_channels = info.outputs.max(0) as usize;
    let mut editor = plugin
        .get_editor()
        .ok_or_else(|| "plugin reports no editor".to_string())?;
    let (mut width, mut height) = editor.size();
    if width <= 0 {
        width = 760;
    }
    if height <= 0 {
        height = 520;
    }
    let window = create_editor_window(title, width as f64, height as f64)?;
    let content_view: *mut Object = msg_send![window, contentView];
    if content_view.is_null() {
        return Err("NSWindow contentView is null".into());
    }
    if !editor.open(content_view as *mut c_void) {
        return Err("Editor::open returned false (plugin refused parent NSView)".into());
    }
    show_window(window);
    let host_buffer = HostBuffer::<f32>::new(input_channels, output_channels);
    let inputs = (0..input_channels).map(|_| vec![0.0f32; 4096]).collect();
    let outputs = (0..output_channels).map(|_| vec![0.0f32; 4096]).collect();
    let state = Box::new(MacVst2EditorState {
        _window: window,
        _editor: editor,
        _plugin: plugin,
        _loader: loader,
        audio_rx,
        host_buffer,
        inputs,
        outputs,
        input_channels,
        output_channels,
    });
    Ok(NativeVstEditorHandle {
        audio_tx,
        state: NativeVstEditorState::Vst2(state),
    })
}

fn drain_vst2_editor_audio(state: &mut MacVst2EditorState) {
    loop {
        match state.audio_rx.try_recv() {
            Ok(buffer) => process_vst2_editor_audio_buffer(state, &buffer),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    state._editor.idle();
}

fn process_vst2_editor_audio_buffer(state: &mut MacVst2EditorState, buffer: &AudioBuffer) {
    let frames = buffer.frames.min(4096);
    if frames == 0 {
        return;
    }
    let channels = buffer.format.channels as usize;
    if channels != 1 && channels != 2 {
        return;
    }

    if let Some(plane) = state.inputs.get_mut(0) {
        match channels {
            2 => {
                for frame in 0..frames {
                    plane[frame] = buffer.data[frame * 2];
                }
            }
            1 => plane[..frames].copy_from_slice(&buffer.data[..frames]),
            _ => unreachable!(),
        }
    }
    if let Some(plane) = state.inputs.get_mut(1) {
        match channels {
            2 => {
                for frame in 0..frames {
                    plane[frame] = buffer.data[frame * 2 + 1];
                }
            }
            1 => plane[..frames].copy_from_slice(&buffer.data[..frames]),
            _ => unreachable!(),
        }
    }
    for plane in state.inputs.iter_mut().skip(2) {
        plane[..frames].fill(0.0);
    }
    for plane in &mut state.outputs {
        plane[..frames].fill(0.0);
    }

    let in_slices: Vec<&[f32]> = state.inputs.iter().map(|v| &v[..frames]).collect();
    let mut out_slices: Vec<&mut [f32]> =
        state.outputs.iter_mut().map(|v| &mut v[..frames]).collect();
    if in_slices.len() != state.input_channels || out_slices.len() != state.output_channels {
        return;
    }
    let mut audio_buf = state.host_buffer.bind(&in_slices, &mut out_slices);
    state._plugin.process(&mut audio_buf);
}

unsafe fn ns_string(s: &str) -> Result<*mut Object, String> {
    let c = CString::new(s).map_err(|_| "string contains null byte".to_string())?;
    let ns: *mut Object = msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()];
    if ns.is_null() {
        return Err("NSString allocation failed".into());
    }
    Ok(ns)
}

unsafe fn create_editor_window(
    title: &str,
    width: f64,
    height: f64,
) -> Result<*mut Object, String> {
    let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
    let _: () = msg_send![app, setActivationPolicy: 0isize];

    let style_mask: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3);
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize { width, height },
    };
    let window: *mut Object = msg_send![class!(NSWindow), alloc];
    let window: *mut Object = msg_send![
        window,
        initWithContentRect: rect
        styleMask: style_mask
        backing: 2u64
        defer: false
    ];
    if window.is_null() {
        return Err("NSWindow allocation failed".into());
    }
    let title = ns_string(title)?;
    let _: () = msg_send![window, setTitle: title];
    let _: () = msg_send![window, setReleasedWhenClosed: false];
    let _: *mut Object = msg_send![window, retain];
    Ok(window)
}

unsafe fn show_window(window: *mut Object) {
    let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
    let _: () = msg_send![window, center];
    let _: () = msg_send![window, makeKeyAndOrderFront: ptr::null_mut::<Object>()];
    let _: () = msg_send![app, activateIgnoringOtherApps: true];
}
