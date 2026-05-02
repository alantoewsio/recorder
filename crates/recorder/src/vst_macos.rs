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
use std::sync::{Arc, Mutex};

use libloading::Library;
use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};
use rack::traits::PluginScanner;
use rack::vst3::Vst3Scanner;
use vst::host::{Host, PluginLoader};
use vst::plugin::Plugin;
use vst3::Steinberg::Vst::{
    BusDirections_, IComponent, IComponentTrait, IConnectionPoint, IConnectionPointTrait,
    IEditController, IEditControllerTrait, MediaTypes_,
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
}

struct MacVst2EditorState {
    _window: *mut Object,
    _editor: Box<dyn vst::editor::Editor>,
    _plugin: vst::host::PluginInstance,
    _loader: PluginLoader<MacVst2Host>,
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
    let path = path.as_ref();
    let title = title.into();
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("vst3") => open_vst3_editor(path, &title),
        Some("vst") => open_vst2_editor(path, &title),
        other => Err(format!(
            "unsupported macOS VST bundle extension for {}: {:?}",
            path.display(),
            other
        )),
    }
}

pub fn open_vst3_editor(path: impl AsRef<Path>, title: &str) -> Result<(), String> {
    let bundle_path = path.as_ref().to_path_buf();
    let unique_id = find_vst3_unique_id(&bundle_path)?;
    unsafe { open_vst3_editor_inner(bundle_path, unique_id, title) }
}

pub fn open_vst2_editor(path: impl AsRef<Path>, title: &str) -> Result<(), String> {
    unsafe { open_vst2_editor_inner(path.as_ref().to_path_buf(), title) }
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
) -> Result<(), String> {
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
    });
    Box::leak(state);
    Ok(())
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

unsafe fn open_vst2_editor_inner(bundle_path: PathBuf, title: &str) -> Result<(), String> {
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
    let state = Box::new(MacVst2EditorState {
        _window: window,
        _editor: editor,
        _plugin: plugin,
        _loader: loader,
    });
    Box::leak(state);
    Ok(())
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
