//! Windows host: input device enumeration and capture streams mapped to [`recorder_core::traits::AudioHost`].
//!
//! Backends (select with [`WindowsHost::audio_system`] / [`WindowsAudioSystem`]):
//! - **WASAPI** — [`cpal`] explicit WASAPI host (default).
//! - **ASIO** — [`cpal`] ASIO host when this crate is built with `--features asio` and the Steinberg ASIO SDK is available to `cpal`.
//! - **DirectSound** — `dsound.dll` capture via the `windows` crate.
//! - **WaveOut** — WinMM **waveIn** capture (label matches common host UIs).
//! - **Dummy** — synthetic silence.

#[cfg(windows)]
mod audio_system;
#[cfg(windows)]
mod capture_cpal;
#[cfg(windows)]
mod capture_dsound;
#[cfg(windows)]
mod capture_dummy;
#[cfg(windows)]
mod capture_process_loopback;
#[cfg(windows)]
mod capture_wavein;
#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use audio_system::WindowsAudioSystem;
#[cfg(windows)]
pub use win::WindowsHost;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::WindowsHost;
