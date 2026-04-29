//! Native VST3 / VST2 editor host on Windows.
//!
//! Both formats end up presenting their UI on a top-level Win32 `HWND` we own, on a
//! dedicated thread that runs `GetMessageW` so the editor and the audio capture stay
//! decoupled. The window class, the wnd-proc, and the `WM_CLOSE`-from-anywhere shutdown
//! mechanism are shared between the two paths.
//!
//! VST3 (`rack`-based audio side):
//!   `rack` does not expose `IPlugView`, so we load a second in-process
//!   `IComponent` / `IEditController` from the same bundle for the vendor UI and mirror
//!   normalized parameters to the `rack::Vst3Plugin` used on the audio thread (see the
//!   module-level comment in `vst.rs`).
//!
//! VST2 (`vst-rs`-based audio side):
//!   VST2 plugins expose UI through the same `AEffect` dispatcher used for audio. There is
//!   no second instance: the spawned thread asks the existing
//!   `Arc<Mutex<Vst2HostedPlugin>>` for an `Editor` once, releases the audio-side mutex,
//!   and then drives `open(HWND)` / periodic `idle()` / `close()` directly. VST2 explicitly
//!   permits concurrent dispatcher and `process_replacing` calls; `vst-rs` reflects that by
//!   marking `PluginParametersInstance` as `Send + Sync`.

use std::ffi::{c_void, OsStr};
use std::mem::zeroed;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use libloading::Library;
use rack::traits::PluginInstance;
use rack::vst3::Vst3Plugin;
use recorder_core::buffer::AudioBuffer;
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
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
    LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW, SetTimer, SetWindowPos,
    ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, IDC_ARROW, MSG, SWP_NOMOVE, SWP_NOZORDER,
    SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_DESTROY, WM_TIMER, WNDCLASSEXW,
    WS_OVERLAPPEDWINDOW,
};

const K_AUDIO: i32 = MediaTypes_::kAudio as i32;
const K_EVENT: i32 = MediaTypes_::kEvent as i32;
const K_INPUT: i32 = BusDirections_::kInput as i32;
const K_OUTPUT: i32 = BusDirections_::kOutput as i32;

static VST3_EDITOR_VIEW_TYPE: &[u8] = b"editor\0";
const EDITOR_WINDOW_STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0);
const EDITOR_IDLE_TIMER_ID: usize = 1;
const EDITOR_IDLE_MS: u32 = 30;

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn vst3_bundle_binary_path(bundle: &Path) -> Result<PathBuf, String> {
    if bundle.is_file() && bundle.extension().and_then(|e| e.to_str()) == Some("vst3") {
        return Ok(bundle.to_path_buf());
    }
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64-win"
    } else {
        "x86-win"
    };
    let arch_dir = bundle.join("Contents").join(arch);
    let rd =
        std::fs::read_dir(&arch_dir).map_err(|e| format!("list {}: {e}", arch_dir.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("vst3") {
            return Ok(p);
        }
    }
    Err(format!("no .vst3 binary under {}", arch_dir.display()))
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

fn register_editor_window_class() -> Result<(), String> {
    let class_name = to_wide(EDITOR_CLASS);
    let hmod = unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(|e| e.to_string())?;
    let hinst = HINSTANCE(hmod.0);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(editor_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: Default::default(),
        hCursor: unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW) }
            .map_err(|e| e.to_string())?,
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        let err = windows::core::Error::from_win32();
        if err.code().0 as u32 != 1410 {
            return Err(err.to_string());
        }
    }
    Ok(())
}

unsafe fn get_factory(lib: &Library) -> Result<ComPtr<IPluginFactory>, String> {
    let get_factory: libloading::Symbol<unsafe extern "system" fn() -> *mut IPluginFactory> = lib
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

unsafe fn push_rack_params_to_controller(rack: &mut Vst3Plugin, ctrl: &ComPtr<IEditController>) {
    let n_rack = PluginInstance::parameter_count(rack);
    let n_ctrl = ctrl.getParameterCount() as usize;
    let limit = n_rack.min(n_ctrl);
    for i in 0..limit {
        let Ok(norm) = PluginInstance::get_parameter(rack, i) else {
            continue;
        };
        let mut pi = zeroed();
        if ctrl.getParameterInfo(i as i32, &mut pi) != kResultOk {
            continue;
        }
        let _ = ctrl.setParamNormalized(pi.id, norm as f64);
    }
}

unsafe fn pull_controller_params_to_rack(ctrl: &ComPtr<IEditController>, rack: &mut Vst3Plugin) {
    let n_rack = PluginInstance::parameter_count(rack);
    for i in 0..n_rack {
        let mut pi = zeroed();
        if ctrl.getParameterInfo(i as i32, &mut pi) != kResultOk {
            continue;
        }
        let v = ctrl.getParamNormalized(pi.id) as f32;
        let _ = PluginInstance::set_parameter(rack, i, v);
    }
}

unsafe fn setup_editor_audio_processor(
    component: &ComPtr<IComponent>,
    sample_rate_hz: u32,
    max_block: usize,
) -> Option<ComPtr<IAudioProcessor>> {
    let audio = component.cast::<IAudioProcessor>()?;
    if audio.canProcessSampleSize(SymbolicSampleSizes_::kSample32) != kResultOk {
        return None;
    }
    let mut setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32,
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

unsafe fn process_editor_audio_buffer(audio: &ComPtr<IAudioProcessor>, buffer: &AudioBuffer) {
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
        processMode: ProcessModes_::kRealtime,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32,
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

unsafe fn drain_editor_audio(
    audio: Option<&ComPtr<IAudioProcessor>>,
    audio_rx: &Receiver<AudioBuffer>,
) {
    let Some(audio) = audio else {
        while audio_rx.try_recv().is_ok() {}
        return;
    };
    loop {
        match audio_rx.try_recv() {
            Ok(buffer) => process_editor_audio_buffer(audio, &buffer),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
}

static EDITOR_CLASS_ONCE: std::sync::Once = std::sync::Once::new();
const EDITOR_CLASS: &str = "RecorderVstNativeEditorHost";

unsafe extern "system" fn editor_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLOSE => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn create_host_window(title: &str, width: i32, height: i32) -> Result<HWND, String> {
    EDITOR_CLASS_ONCE.call_once(|| {
        if let Err(e) = register_editor_window_class() {
            eprintln!("RegisterClassExW: {e}");
        }
    });
    let class_name = to_wide(EDITOR_CLASS);
    let title_w = to_wide(title);
    let hmod = unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(|e| e.to_string())?;
    let hinst = HINSTANCE(hmod.0);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            EDITOR_WINDOW_STYLE,
            windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
            windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
            width,
            height,
            HWND::default(),
            None,
            hinst,
            None,
        )
    }
    .map_err(|e| e.to_string())?;
    // Once the HWND exists, Windows knows which monitor/DPI applies. Resize immediately so
    // the plugin receives a client area matching its editor size on scaled displays.
    unsafe { resize_host_window_to_client(hwnd, width, height)? };
    Ok(hwnd)
}

unsafe fn window_size_for_client(
    hwnd: HWND,
    client_width: i32,
    client_height: i32,
) -> Result<(i32, i32), String> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: client_width,
        bottom: client_height,
    };
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    unsafe {
        AdjustWindowRectExForDpi(
            &mut rect,
            EDITOR_WINDOW_STYLE,
            false,
            WINDOW_EX_STYLE::default(),
            dpi,
        )
    }
    .map_err(|e| e.to_string())?;
    Ok((rect.right - rect.left, rect.bottom - rect.top))
}

unsafe fn resize_host_window_to_client(
    hwnd: HWND,
    client_width: i32,
    client_height: i32,
) -> Result<(), String> {
    let (outer_width, outer_height) =
        unsafe { window_size_for_client(hwnd, client_width, client_height)? };
    unsafe {
        SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            outer_width,
            outer_height,
            SWP_NOMOVE | SWP_NOZORDER,
        )
    }
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Dedicated thread: vendor UI + Win32 message loop; mirrors parameters to `rack`.
pub fn run_vst3_editor_thread(
    bundle_path: PathBuf,
    class_id_hex: String,
    plugin_title: String,
    rack: Arc<Mutex<Vst3Plugin>>,
    hwnd_signal: Arc<AtomicIsize>,
    audio_rx: Receiver<AudioBuffer>,
) {
    let r = unsafe {
        run_vst3_editor_inner(
            bundle_path,
            class_id_hex,
            plugin_title,
            rack,
            hwnd_signal.clone(),
            audio_rx,
        )
    };
    if let Err(e) = r {
        eprintln!("native VST3 editor: {e}");
    }
    hwnd_signal.store(0, Ordering::Release);
}

unsafe fn run_vst3_editor_inner(
    bundle_path: PathBuf,
    class_id_hex: String,
    plugin_title: String,
    rack: Arc<Mutex<Vst3Plugin>>,
    hwnd_signal: Arc<AtomicIsize>,
    audio_rx: Receiver<AudioBuffer>,
) -> Result<(), String> {
    let dll_path = vst3_bundle_binary_path(&bundle_path)?;
    let lib = Library::new(&dll_path).map_err(|e| format!("load {}: {e}", dll_path.display()))?;
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
    let editor_audio = setup_editor_audio_processor(&component, 48_000, 4096);

    let controller = get_or_create_controller(&component, &factory)?
        .ok_or_else(|| "no IEditController".to_string())?;
    try_connect(&component, &controller);

    if component.setActive(1) != kResultOk {
        return Err("IComponent::setActive(1) failed".into());
    }

    {
        let mut g = rack.lock().map_err(|_| "rack mutex poisoned")?;
        push_rack_params_to_controller(&mut *g, &controller);
    }

    let view_ptr = controller.createView(VST3_EDITOR_VIEW_TYPE.as_ptr() as *const i8);
    if view_ptr.is_null() {
        component.setActive(0);
        component.terminate();
        return Err("createView(editor) returned null".into());
    }
    let view = ComPtr::from_raw(view_ptr).ok_or("IPlugView wrap failed")?;

    let mut rect = vst3::Steinberg::ViewRect {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    let _ = view.getSize(&mut rect);
    let mut width = rect.right - rect.left;
    let mut height = rect.bottom - rect.top;
    if width <= 0 {
        width = 800;
    }
    if height <= 0 {
        height = 600;
    }

    let hwnd = unsafe { create_host_window(&plugin_title, width, height)? };
    hwnd_signal.store(hwnd.0.addr() as isize, Ordering::Release);

    let platform = b"HWND\0".as_ptr() as *const i8;
    if view.isPlatformTypeSupported(platform) != kResultOk {
        let _ = DestroyWindow(hwnd);
        hwnd_signal.store(0, Ordering::Release);
        component.setActive(0);
        component.terminate();
        return Err("plugin editor does not support HWND".into());
    }

    let attach = view.attached(hwnd.0 as *mut c_void, platform);
    if attach != kResultOk {
        let _ = DestroyWindow(hwnd);
        hwnd_signal.store(0, Ordering::Release);
        component.setActive(0);
        component.terminate();
        return Err(format!("IPlugView::attached failed: {attach:#x}"));
    }

    let _ = ShowWindow(hwnd, SW_SHOW);
    let timer_id = unsafe { SetTimer(hwnd, EDITOR_IDLE_TIMER_ID, EDITOR_IDLE_MS, None) };

    let mut last_sync = std::time::Instant::now();
    let mut msg = MSG::default();
    loop {
        let gm = GetMessageW(&mut msg, HWND::default(), 0, 0);
        if gm.0 == 0 {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
        drain_editor_audio(editor_audio.as_ref(), &audio_rx);

        if last_sync.elapsed().as_millis() >= 50 {
            last_sync = std::time::Instant::now();
            if let Ok(mut g) = rack.try_lock() {
                pull_controller_params_to_rack(&controller, &mut *g);
            }
        }
    }
    if timer_id != 0 {
        let _ = unsafe { KillTimer(hwnd, EDITOR_IDLE_TIMER_ID) };
    }

    let _ = view.removed();
    if let Some(audio) = &editor_audio {
        let _ = audio.setProcessing(0);
    }
    controller.terminate();
    let _ = component.setActive(0);
    component.terminate();

    drop(view);
    drop(controller);
    drop(component);
    drop(factory);
    drop(lib);
    Ok(())
}

/// Ask the native editor thread to shut down (safe from any thread).
pub fn post_close_native_editor(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let p = ptr::with_exposed_provenance_mut::<c_void>(hwnd as usize);
        let _ = PostMessageW(HWND(p), WM_CLOSE, WPARAM::default(), LPARAM::default());
    }
}

// ---------------------------------------------------------------------------
// VST2 editor host
// ---------------------------------------------------------------------------

use super::vst2::Vst2HostedPlugin;
// `vst-rs` ships its public surface with `#[deprecated]` because Steinberg retired the VST 2.4
// SDK; we depend on it intentionally (see the explanation in `vst2.rs`). Silence the noise
// for the editor-host code below.
#[allow(deprecated)]
use vst::plugin::Plugin as Vst2PluginTrait;

/// Dedicated thread: hosts the VST2 plugin's editor in a Win32 window and pumps `idle()`.
///
/// The audio chain keeps using the same `Arc<Mutex<Vst2HostedPlugin>>` — the editor
/// dispatcher path inside `vst-rs` (`PluginParametersInstance`) is `Send + Sync`, so editor
/// events and `process_replacing` may run concurrently as VST 2.4 itself permits.
pub fn run_vst2_editor_thread(
    plugin: Arc<Mutex<Vst2HostedPlugin>>,
    plugin_title: String,
    hwnd_signal: Arc<AtomicIsize>,
) {
    let r = unsafe { run_vst2_editor_inner(plugin, plugin_title, hwnd_signal.clone()) };
    if let Err(e) = r {
        eprintln!("native VST2 editor: {e}");
    }
    hwnd_signal.store(0, Ordering::Release);
}

/// SAFETY: All raw HWND / `Box<dyn Editor>` access happens on this single thread, never
/// crossing `.send()`. The editor's internal dispatcher (`PluginParametersInstance`) is
/// `Send + Sync` per upstream `vst-rs`, so concurrent calls from the audio thread are
/// allowed; we never share the editor box itself between threads.
#[allow(deprecated)]
unsafe fn run_vst2_editor_inner(
    plugin: Arc<Mutex<Vst2HostedPlugin>>,
    plugin_title: String,
    hwnd_signal: Arc<AtomicIsize>,
) -> Result<(), String> {
    // Take ownership of the editor under the audio mutex, then release the lock so the
    // capture path is unblocked while the editor runs.
    let mut editor = {
        let mut g = plugin
            .lock()
            .map_err(|_| "vst2 plugin mutex poisoned".to_string())?;
        g.instance
            .get_editor()
            .ok_or_else(|| "plugin reports no editor".to_string())?
    };

    // Ask the plugin for its preferred dimensions before opening so the host window is the
    // right size on first paint. Some plugins return `(0, 0)` until `open()` is called;
    // fall back to a sensible default and let the user resize.
    let (mut w, mut h) = editor.size();
    if w <= 0 {
        w = 760;
    }
    if h <= 0 {
        h = 520;
    }

    let hwnd = unsafe { create_host_window(&plugin_title, w, h)? };
    hwnd_signal.store(hwnd.0.addr() as isize, Ordering::Release);

    if !editor.open(hwnd.0 as *mut c_void) {
        let _ = DestroyWindow(hwnd);
        hwnd_signal.store(0, Ordering::Release);
        return Err("Editor::open returned false (plugin refused parent HWND)".into());
    }

    // Some plugins finalize their layout inside `open()` — query size again and resize the
    // host window if it changed materially.
    let (rw, rh) = editor.size();
    if rw > 0 && rh > 0 && (rw != w || rh != h) {
        unsafe { resize_host_window_to_client(hwnd, rw, rh)? };
    }

    let _ = ShowWindow(hwnd, SW_SHOW);
    let timer_id = unsafe { SetTimer(hwnd, EDITOR_IDLE_TIMER_ID, EDITOR_IDLE_MS, None) };

    // VST 2.4 expects the host to call `effEditIdle` periodically (~30 Hz is the de-facto
    // norm) so plugins that don't run their own UI timer keep animating, repaint after
    // parameter changes, etc.
    let mut msg = MSG::default();
    loop {
        let gm = GetMessageW(&mut msg, HWND::default(), 0, 0);
        if gm.0 == 0 {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);

        if msg.message == WM_TIMER && msg.wParam.0 == EDITOR_IDLE_TIMER_ID {
            editor.idle();
        }
    }
    if timer_id != 0 {
        let _ = unsafe { KillTimer(hwnd, EDITOR_IDLE_TIMER_ID) };
    }

    editor.close();
    drop(editor);
    Ok(())
}
