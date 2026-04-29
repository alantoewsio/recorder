//! Linux host: capture via **cpal** (typically PipeWire or PulseAudio depending on system).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::LinuxHost;

#[cfg(not(target_os = "linux"))]
mod stub;
#[cfg(not(target_os = "linux"))]
pub use stub::LinuxHost;
