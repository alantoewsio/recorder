//! macOS host: Core Audio input via **cpal**, plus optional ScreenCaptureKit-backed
//! system-audio loopback (macOS 13+) under the `screencapturekit` feature.

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::MacosHost;

#[cfg(all(target_os = "macos", feature = "screencapturekit"))]
mod screen_capture;

#[cfg(not(target_os = "macos"))]
mod stub;
#[cfg(not(target_os = "macos"))]
pub use stub::MacosHost;
