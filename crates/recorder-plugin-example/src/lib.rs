//! Example `cdylib`: applies 0.5 gain using [`recorder_plugin_api`] v1.

use std::slice;

use recorder_plugin_api::{RecorderAudioFrameV1, RecorderPluginV1, RECORDER_PLUGIN_ABI_VERSION};

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
    let st = &*(user as *const State);
    let f = &*frame;
    if f.data.is_null() {
        return -2;
    }
    let n = f.frames.saturating_mul(f.channels as usize);
    if n > out_len {
        return -3;
    }
    let inp = slice::from_raw_parts(f.data, n);
    let outp = slice::from_raw_parts_mut(out, n);
    for i in 0..n {
        outp[i] = (inp[i] * st.gain).clamp(-1.0, 1.0);
    }
    0
}

unsafe extern "C" fn destroy(user: *mut core::ffi::c_void) {
    if user.is_null() {
        return;
    }
    drop(Box::from_raw(user as *mut State));
}
