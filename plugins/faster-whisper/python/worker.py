#!/usr/bin/env python3
"""Persistent faster-whisper sidecar for recorder-plugin-faster-whisper.

Protocol on stdin:
  TRANSCRIBE<TAB>segment_id<TAB>start_frame<TAB>end_frame<TAB>rms<TAB>wav_path
  STOP

Protocol on stdout:
  READY
  STATUS<TAB>message
  FINAL<TAB>segment_id<TAB>start_frame<TAB>end_frame<TAB>text
  ERROR<TAB>message
"""

from __future__ import annotations

import argparse
import os
import sys
import tempfile
import wave
from pathlib import Path


def clean_text(text: str) -> str:
    return " ".join(str(text).replace("\t", " ").replace("\n", " ").split())


def parse_env_value(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
        return value[1:-1]
    return value


def find_nearest_env() -> Path | None:
    for start in [Path.cwd(), Path(__file__).resolve()]:
        for parent in [start, *start.parents]:
            env_path = parent / ".env"
            if env_path.is_file():
                return env_path
    return None


def apply_env_line(raw_line: str) -> None:
    line = raw_line.strip()
    if not line or line.startswith("#") or "=" not in line:
        return
    key, value = line.split("=", 1)
    key = key.strip()
    if key:
        os.environ.setdefault(key, parse_env_value(value))


def load_repo_env() -> None:
    loaded_path = find_nearest_env()
    if not loaded_path:
        return
    try:
        for raw_line in loaded_path.read_text(encoding="utf-8").splitlines():
            apply_env_line(raw_line)
        print(f"STATUS\tLoaded environment from {loaded_path}", flush=True)
    except Exception as exc:
        print(f"STATUS\tCould not read .env at {loaded_path}: {clean_text(exc)}", flush=True)


def parse_bool(value: str) -> bool:
    normalized = value.strip().lower()
    if normalized in {"1", "true", "yes", "on"}:
        return True
    if normalized in {"0", "false", "no", "off"}:
        return False
    raise ValueError(f"invalid boolean value: {value}")


def resolve_device(device_mode: str) -> str:
    import ctranslate2

    cuda_count = ctranslate2.get_cuda_device_count()
    if device_mode == "cpu":
        return "cpu"
    if device_mode == "cuda":
        if cuda_count <= 0:
            raise RuntimeError("CUDA was selected, but no CUDA device is available")
        return "cuda"
    if cuda_count > 0:
        return "cuda"
    return "cpu"


def select_compute_type(device: str) -> str:
    import ctranslate2

    supported = ctranslate2.get_supported_compute_types(device)
    if device == "cuda":
        preferred = ["int8_float16", "float16", "int8", "default", "float32"]
    else:
        preferred = ["int8", "int8_float32", "float32", "default"]
    for candidate in preferred:
        if candidate in supported:
            return candidate
    return "default"


def load_model(model_name: str, device_mode: str, cache_dir: str | None):
    try:
        from faster_whisper import WhisperModel
    except Exception as exc:
        print(
            "ERROR\tCould not import faster-whisper. Install it with `python -m pip install faster-whisper`. "
            f"Details: {clean_text(exc)}",
            flush=True,
        )
        raise

    device = resolve_device(device_mode)
    compute_type = select_compute_type(device)
    print(f"STATUS\tUsing {device.upper()} with compute type {compute_type}", flush=True)
    print(f"STATUS\tLoading model {model_name}", flush=True)
    kwargs = {
        "device": device,
        "compute_type": compute_type,
    }
    if cache_dir:
        kwargs["download_root"] = cache_dir
        print(f"STATUS\tModel cache directory: {cache_dir}", flush=True)
    model = WhisperModel(model_name, **kwargs)
    return model, device, compute_type


def transcribe_one(model, wav_path: str, task: str, beam_size: int, vad_filter: bool) -> tuple[str, str | None]:
    segments, info = model.transcribe(
        wav_path,
        task=task,
        beam_size=beam_size,
        vad_filter=vad_filter,
        condition_on_previous_text=False,
    )
    text = clean_text(" ".join(segment.text.strip() for segment in segments if segment.text.strip()))
    language = getattr(info, "language", None)
    return text, language


def prewarm_model(model, task: str, beam_size: int, vad_filter: bool) -> None:
    print("STATUS\tPre-warming model", flush=True)
    try:
        with tempfile.NamedTemporaryFile(prefix="faster-whisper-prewarm-", suffix=".wav", delete=False) as fh:
            warmup_path = fh.name
        try:
            with wave.open(warmup_path, "wb") as writer:
                writer.setnchannels(1)
                writer.setsampwidth(2)
                writer.setframerate(16_000)
                writer.writeframes(b"\x00\x00" * 16_000)
            transcribe_one(model, warmup_path, task, beam_size, vad_filter)
        finally:
            try:
                Path(warmup_path).unlink(missing_ok=True)
            except Exception:
                pass
        print("STATUS\tModel warmed", flush=True)
    except Exception as exc:
        print(f"STATUS\tPre-warm failed (continuing): {clean_text(exc)}", flush=True)


def handle_transcribe_command(model, task: str, beam_size: int, vad_filter: bool, parts: list[str]) -> None:
    _, segment_id, start_frame, end_frame, rms, wav_path = parts
    try:
        print(
            f"STATUS\tTranscribing segment {segment_id} rms={rms} task={task} beam={beam_size} vad={vad_filter}",
            flush=True,
        )
        text, language = transcribe_one(model, wav_path, task, beam_size, vad_filter)
        if language:
            print(f"STATUS\tDetected language for segment {segment_id}: {language}", flush=True)
        if not text:
            print(f"STATUS\tSegment {segment_id} produced empty transcript", flush=True)
        print(f"FINAL\t{segment_id}\t{start_frame}\t{end_frame}\t{text}", flush=True)
    except Exception as exc:
        print(f"ERROR\tTranscription failed for segment {segment_id}: {clean_text(exc)}", flush=True)
    finally:
        try:
            Path(wav_path).unlink(missing_ok=True)
        except Exception:
            pass


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", default="small")
    parser.add_argument("--device-mode", default="auto")
    parser.add_argument("--task", default="transcribe")
    parser.add_argument("--beam-size", type=int, default=1)
    parser.add_argument("--vad-filter", default="true")
    parser.add_argument("--cache-dir", default="")
    args = parser.parse_args()

    load_repo_env()

    try:
        vad_filter = parse_bool(args.vad_filter)
        model, _, _ = load_model(
            args.model,
            args.device_mode,
            args.cache_dir.strip() or None,
        )
    except Exception:
        return 2

    prewarm_model(model, args.task, args.beam_size, vad_filter)
    print("READY", flush=True)

    for raw_line in sys.stdin:
        line = raw_line.rstrip("\n")
        if line == "STOP":
            return 0
        if not line:
            continue

        parts = line.split("\t", 5)
        if len(parts) != 6 or parts[0] != "TRANSCRIBE":
            print(f"ERROR\tInvalid command: {clean_text(line)}", flush=True)
            continue

        handle_transcribe_command(model, args.task, args.beam_size, vad_filter, parts)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
