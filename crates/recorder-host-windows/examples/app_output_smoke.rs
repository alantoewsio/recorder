use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use recorder_core::{AudioFormat, AudioHost, CaptureSourceKind, SampleFormat};
use recorder_host_windows::{WindowsAudioSystem, WindowsHost};

fn spawn_audio_process() -> std::io::Result<Child> {
    Command::new("powershell")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "for (;;) { [System.Media.SystemSounds]::Asterisk.Play(); Start-Sleep -Milliseconds 250 }",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("step=spawn");
    let mut child = spawn_audio_process()?;
    let pid = child.id();
    eprintln!("child_pid={pid}");
    thread::sleep(Duration::from_secs(1));

    eprintln!("step=host");
    let host = WindowsHost::new(WindowsAudioSystem::Wasapi)?;
    eprintln!("step=list_sources");
    let sources = host.list_capture_sources()?;
    eprintln!("sources={}", sources.len());
    let source = sources
        .iter()
        .find(|source| {
            source.kind == CaptureSourceKind::AppOutput
                && source.app.as_ref().and_then(|app| app.process_id) == Some(pid)
        })
        .ok_or_else(|| format!("app-output source not found for pid {pid}"))?;

    eprintln!("source_id={}", source.id);
    let frames_seen = Arc::new(AtomicU64::new(0));
    let frames_seen_for_cb = Arc::clone(&frames_seen);
    eprintln!("step=start_capture");
    let handle = host.start_capture(
        Some(&source.id),
        CaptureSourceKind::AppOutput,
        AudioFormat::new(48_000, 2, SampleFormat::F32),
        Arc::new(move |buffer| {
            frames_seen_for_cb.fetch_add(buffer.frames as u64, Ordering::Relaxed);
        }),
    )?;

    eprintln!("step=capturing");
    thread::sleep(Duration::from_secs(3));
    eprintln!("step=stop");
    handle.stop();
    let _ = child.kill();
    let _ = child.wait();

    eprintln!("frames_seen={}", frames_seen.load(Ordering::Relaxed));
    Ok(())
}
