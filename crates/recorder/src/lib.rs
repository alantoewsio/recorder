//! Convenience re-exports for [`recorder_core`].
//!
//! Add a host crate to your binary (`recorder-host-windows`, `recorder-host-macos`, or
//! `recorder-host-linux`) depending on target OS.

#[cfg(all(windows, feature = "vst"))]
pub mod vst;

pub use recorder_core::*;
