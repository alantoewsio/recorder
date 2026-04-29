# NVIDIA Parakeet Unified Analyzer Plugin

This local Recorder plugin implements `AudioAnalyzer` for
`nvidia/parakeet-unified-en-0.6b`.

The Rust analyzer receives enhanced/post-processor audio from Recorder, downmixes and resamples it
to 16 kHz mono WAV chunks, and sends those chunks to a persistent Python sidecar. The sidecar loads
the NVIDIA NeMo model once and emits transcript events back to the demo app.

## Runtime Setup

Install the Python runtime expected by the model card:

```powershell
python -m pip install "nemo_toolkit[asr]"
```

Optional environment variables (used when no Parakeet block is saved yet, or after **Reset to defaults** in the demo app’s **Parakeet properties** window):

- `RECORDER_PARAKEET_PYTHON`: Python executable to run. Defaults to the nearest project `.venv` Python when present, otherwise `python`.
- `RECORDER_PARAKEET_WORKER`: path to `worker.py`. Defaults to this crate's bundled script.
- `RECORDER_PARAKEET_CHUNK_SECONDS`: audio chunk duration. Defaults to `1.5`. Lower values reduce time-to-first-word but increase per-chunk overhead.
- `RECORDER_PARAKEET_SILENCE_RMS`: chunks with RMS below this are skipped without invoking the model. Defaults to `0.005`.

The **recorder-ui** demo persists Parakeet settings (chunk length, silence threshold, model path, Python, etc.) in egui storage when you click **Apply** in **Parakeet properties** (open it by clicking the Parakeet strip tile in the FX chain). Those values override the env defaults for future sessions.

The sidecar moves the model to a CUDA device when one is available; otherwise it runs on CPU and reports `Using CPU (transcription will be slow; install a CUDA build of torch for real-time speed)`. After loading, it runs a tiny silent pre-warm transcribe so the first user chunk doesn't pay the cold-start latency.

The model card lists NeMo 2.7.3 and NVIDIA GPU hardware as the supported runtime. The model can
take a while to download and initialize. During that time the Rust analyzer keeps only a bounded
amount of recent audio and starts sending chunks after the sidecar reports `model ready`.
