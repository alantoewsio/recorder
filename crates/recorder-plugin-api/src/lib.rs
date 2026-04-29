//! # Recorder plugin ABI v1
//!
//! Plugins are dynamic libraries exporting `recorder_plugin_entry_v1`.
//! The host loads that symbol, obtains a [`RecorderPluginV1`] vtable, then invokes `process` for each PCM frame.
//!
//! ## Versioning
//! - Bump `RECORDER_PLUGIN_ABI_VERSION` when breaking the C layout or calling convention.
//! - Non-breaking additions use new struct fields only at the end with reserved padding, or a v2 entry symbol.

/// Increment when `RecorderPluginV1` or frame layout changes incompatibly.
pub const RECORDER_PLUGIN_ABI_VERSION: u32 = 1;

/// PCM frame description (read-only for `process`).
#[repr(C)]
pub struct RecorderAudioFrameV1 {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub _reserved: u16,
    pub frames: usize,
    /// Interleaved `frames * channels` samples.
    pub data: *const f32,
}

pub type RecorderPluginProcessV1 = unsafe extern "C" fn(
    user: *mut core::ffi::c_void,
    frame: *const RecorderAudioFrameV1,
    out: *mut f32,
    out_len: usize,
) -> i32;

pub type RecorderPluginDestroyV1 = unsafe extern "C" fn(user: *mut core::ffi::c_void);

/// Plugin vtable supplied by the plugin.
#[repr(C)]
pub struct RecorderPluginV1 {
    pub abi_version: u32,
    /// Opaque instance pointer passed to `process` / `destroy`.
    pub user: *mut core::ffi::c_void,
    pub process: Option<RecorderPluginProcessV1>,
    pub destroy: Option<RecorderPluginDestroyV1>,
}

/// Return 0 on success; non-zero aborts loading.
pub type RecorderPluginCreateFnV1 = unsafe extern "C" fn(out: *mut RecorderPluginV1) -> i32;

/// Well-known export name for the v1 loader.
pub const RECORDER_PLUGIN_ENTRY_V1_SYM: &[u8] = b"recorder_plugin_entry_v1\0";
