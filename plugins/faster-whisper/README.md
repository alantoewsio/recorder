# Faster-Whisper Analyzer Plugin

This local Recorder plugin implements `AudioAnalyzer` for
[`SYSTRAN/faster-whisper`](https://github.com/SYSTRAN/faster-whisper).

The Rust analyzer receives enhanced/post-processor audio from Recorder, downmixes and resamples it
to 16 kHz mono WAV chunks, and sends those chunks to a persistent Python sidecar. The sidecar loads
the selected Whisper model once, auto-selects CPU/GPU by default, auto-downloads missing models,
and emits transcript events back to the demo app.

## Runtime Setup

Install the Python runtime expected by the sidecar:

```powershell
python -m pip install faster-whisper
```

Optional environment variables (used when no Faster-Whisper block is saved yet, or after
**Reset to defaults** in the demo app’s **Faster-Whisper properties** window):

- `RECORDER_FASTER_WHISPER_PYTHON`: Python executable to run. Defaults to the nearest project
  `.venv` Python when present, otherwise `python`.
- `RECORDER_FASTER_WHISPER_WORKER`: path to `worker.py`. Defaults to this crate's bundled script.
- `RECORDER_FASTER_WHISPER_MODEL`: model size/name. Defaults to `small`.
- `RECORDER_FASTER_WHISPER_CHUNK_SECONDS`: audio chunk duration. Defaults to `1.0`.
- `RECORDER_FASTER_WHISPER_SILENCE_RMS`: chunks with RMS below this are skipped without invoking
  the model. Defaults to `0.005`.

The **recorder-ui** demo persists Faster-Whisper settings in egui storage when you click **Apply**
in **Faster-Whisper properties**. Those values override the env defaults for future sessions.

GPU notes:

- `device_mode = auto` tries CUDA first when CTranslate2 reports an available CUDA device.
- `device_mode = cuda` requires CUDA support to be available or the worker fails with a clear error.
- `device_mode = cpu` forces CPU inference.

The sidecar lets `faster-whisper` download missing models into the default Hugging Face cache, or a
custom cache directory when one is configured in the properties window.
