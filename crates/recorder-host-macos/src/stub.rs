use std::sync::Arc;

use recorder_core::buffer::AudioBuffer;
use recorder_core::error::{RecordingError, Result};
use recorder_core::format::AudioFormat;
use recorder_core::traits::{
    AudioHost, CaptureSource, CaptureSourceKind, DeviceInfo, StreamHandle,
};

#[derive(Debug, Default, Clone)]
pub struct MacosHost;

impl AudioHost for MacosHost {
    fn list_input_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(Vec::new())
    }

    fn start_input_stream(
        &self,
        _device_id: Option<&str>,
        _format: AudioFormat,
        _on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
    ) -> Result<StreamHandle> {
        Err(RecordingError::Device(
            "recorder-host-macos is only functional on macOS targets".into(),
        ))
    }

    fn list_capture_sources(&self) -> Result<Vec<CaptureSource>> {
        Ok(Vec::new())
    }

    fn start_capture(
        &self,
        _source_id: Option<&str>,
        _kind: CaptureSourceKind,
        _format: AudioFormat,
        _on_buffer: Arc<dyn Fn(AudioBuffer) + Send + Sync>,
    ) -> Result<StreamHandle> {
        Err(RecordingError::Device(
            "recorder-host-macos is only functional on macOS targets".into(),
        ))
    }
}
