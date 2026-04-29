//! User-selectable Windows audio stacks (matches common host / DAW menus).

/// Audio API used for capture on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum WindowsAudioSystem {
    /// Windows Audio Session API (modern default).
    #[default]
    Wasapi,
    /// Steinberg ASIO (requires `recorder-host-windows` built with `--features asio` and the ASIO SDK for `cpal`).
    Asio,
    /// Microsoft DirectSound capture (`dsound.dll`).
    DirectSound,
    /// WinMM **waveIn** capture (legacy stack; many UIs label this driver family “WaveOut”).
    WaveOut,
    /// No hardware: silence at the requested format (for tests / offline pipeline).
    Dummy,
}

impl WindowsAudioSystem {
    pub const ALL: &[WindowsAudioSystem] = &[
        WindowsAudioSystem::Wasapi,
        WindowsAudioSystem::Asio,
        WindowsAudioSystem::DirectSound,
        WindowsAudioSystem::WaveOut,
        WindowsAudioSystem::Dummy,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            WindowsAudioSystem::Wasapi => "WASAPI",
            WindowsAudioSystem::Asio => "ASIO",
            WindowsAudioSystem::DirectSound => "DirectSound",
            WindowsAudioSystem::WaveOut => "WaveOut",
            WindowsAudioSystem::Dummy => "Dummy Audio",
        }
    }
}
