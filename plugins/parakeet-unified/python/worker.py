#!/usr/bin/env python3
"""Persistent NeMo sidecar for recorder-plugin-parakeet.

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
import contextlib
import inspect
import numbers
import os
import sys
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
    """Load simple KEY=VALUE pairs from the nearest .env without printing secrets."""
    loaded_path = find_nearest_env()
    if loaded_path:
        try:
            for raw_line in loaded_path.read_text(encoding="utf-8").splitlines():
                apply_env_line(raw_line)
        except Exception as exc:
            print(f"STATUS\tCould not read .env at {loaded_path}: {clean_text(exc)}", flush=True)
            loaded_path = None

    token = (
        os.environ.get("HF_TOKEN")
        or os.environ.get("HUGGING_FACE_HUB_TOKEN")
        or os.environ.get("HUGGINGFACE_HUB_TOKEN")
    )
    if token:
        os.environ.setdefault("HF_TOKEN", token)
        os.environ.setdefault("HUGGING_FACE_HUB_TOKEN", token)
        print("STATUS\tHugging Face token is available for model download", flush=True)
    elif loaded_path:
        print(f"STATUS\tLoaded environment from {loaded_path}", flush=True)


def load_model(model_name: str):
    try:
        import nemo.collections.asr as nemo_asr
    except Exception as exc:  # pragma: no cover - depends on local Python env
        print(f"ERROR\tCould not import NeMo ASR. Install nemo_toolkit[asr]. Details: {exc}", flush=True)
        raise

    patch_nemo_compat()
    model = nemo_asr.models.ASRModel.from_pretrained(model_name=model_name)
    prepare_model_for_transcribe(model)
    move_model_to_best_device(model)
    return model


def move_model_to_best_device(model) -> None:
    """Move the model to GPU when available so transcribe() runs there too."""
    try:
        import torch
    except Exception as exc:
        print(f"STATUS\tCould not import torch to select device: {clean_text(exc)}", flush=True)
        return

    if torch.cuda.is_available():
        try:
            model.to("cuda")
            device_name = torch.cuda.get_device_name(0)
            print(f"STATUS\tUsing CUDA device: {device_name}", flush=True)
            return
        except Exception as exc:
            print(f"STATUS\tFailed to move model to CUDA: {clean_text(exc)}", flush=True)

    print(
        "STATUS\tUsing CPU (transcription will be slow; install a CUDA build of torch for real-time speed)",
        flush=True,
    )


def prepare_model_for_transcribe(model) -> None:
    """Fill optional config sections that NeMo's transcribe path assumes exist."""
    try:
        from omegaconf import OmegaConf, open_dict
    except Exception as exc:
        print(f"STATUS\tCould not patch transcribe config: {clean_text(exc)}", flush=True)
        return

    with open_dict(model.cfg):
        if model.cfg.get("validation_ds") is None:
            model.cfg.validation_ds = OmegaConf.create({})
            print("STATUS\tInitialized missing NeMo validation_ds config", flush=True)
        if model.cfg.get("test_ds") is None:
            model.cfg.test_ds = OmegaConf.create({})
            print("STATUS\tInitialized missing NeMo test_ds config", flush=True)


def patch_nemo_compat() -> None:
    """Handle model configs that are slightly ahead of the installed NeMo package.

    Parakeet Unified configs include streaming-specific ConformerEncoder keys that are not
    accepted by some NeMo 2.7.3 Windows/PyPI installs. The sidecar currently transcribes
    independent WAV chunks, so dropping unsupported streaming-only constructor keys is a
    pragmatic compatibility path. We log every dropped key so the UI shows what happened.
    """
    try:
        from nemo.collections.asr.modules.conformer_encoder import ConformerEncoder
    except Exception as exc:
        print(f"STATUS\tCould not apply NeMo compatibility patch: {clean_text(exc)}", flush=True)
        return

    original_init = ConformerEncoder.__init__
    supported = set(inspect.signature(original_init).parameters)

    def patched_init(self, *args, **kwargs):
        mapped = normalize_conformer_kwargs(kwargs)
        dropped = {}
        for key in list(kwargs.keys()):
            if key not in supported:
                dropped[key] = kwargs.pop(key)
        if mapped:
            print(f"STATUS\tMapped ConformerEncoder config: {', '.join(mapped)}", flush=True)
        if dropped:
            names = ", ".join(sorted(dropped.keys()))
            print(f"STATUS\tDropped unsupported ConformerEncoder config key(s): {names}", flush=True)
        return original_init(self, *args, **kwargs)

    ConformerEncoder.__init__ = patched_init


def normalize_conformer_kwargs(kwargs) -> list[str]:
    mapped = []
    if should_use_chunk_context(kwargs):
        kwargs["att_context_size"] = normalize_att_context_size(kwargs["att_chunk_context_size"])
        mapped.append(f"att_chunk_context_size -> att_context_size {kwargs['att_context_size']!r}")

    att_context_style = kwargs.get("att_context_style")
    if is_unknown_chunked_style(att_context_style):
        kwargs["att_context_style"] = "chunked_limited"
        mapped.append(f"att_context_style {att_context_style!r} -> 'chunked_limited'")

    return mapped


def normalize_att_context_size(value):
    """Convert newer chunk-context shapes to NeMo 2.7's [left, right] pair shape."""
    if value is None:
        return value

    value_list = list(value)
    if not value_list:
        return value

    if is_int_like(value_list[0]):
        return coerce_context_pair(value_list)

    return [coerce_context_pair(list(item)) for item in value_list]


def coerce_context_pair(item) -> list[int]:
    if is_int_like(item):
        return [int(item), 0]
    if len(item) >= 2:
        return normalize_chunked_pair(int(item[0]), int(item[1]))
    if len(item) == 1:
        return normalize_chunked_pair(int(item[0]), 0)
    return item


def normalize_chunked_pair(left: int, right: int) -> list[int]:
    right = max(right, 0)
    if left <= 0:
        return [left, right]

    divisor = right + 1
    remainder = left % divisor
    if remainder:
        left += divisor - remainder
    return [left, right]


def is_int_like(value) -> bool:
    return isinstance(value, numbers.Integral)


def should_use_chunk_context(kwargs) -> bool:
    if "att_chunk_context_size" not in kwargs:
        return False
    if kwargs.get("att_chunk_context_size") is None:
        return False
    style = kwargs.get("att_context_style")
    return is_unknown_chunked_style(style) or style == "chunked_limited"


def is_unknown_chunked_style(value) -> bool:
    return isinstance(value, str) and "chunk" in value and value != "chunked_limited"


def transcribe_one(model, wav_path: str) -> str:
    # NeMo writes tqdm progress bars and dataloader warnings to stderr on every call.
    # Keep the sidecar protocol/log readable; actual exceptions are still reported by caller.
    kwargs = transcribe_kwargs(model)
    with open(os.devnull, "w", encoding="utf-8") as devnull:
        with contextlib.redirect_stdout(devnull), contextlib.redirect_stderr(devnull):
            output = model.transcribe([wav_path], **kwargs)
    if not output:
        return ""

    first = output[0]
    text = getattr(first, "text", first)
    return clean_text(text)


def transcribe_kwargs(model) -> dict:
    """Select conservative transcribe options supported by this NeMo version."""
    desired = {
        "batch_size": 1,
        "num_workers": 0,
        "verbose": False,
    }
    try:
        signature = inspect.signature(model.transcribe)
    except Exception:
        return {}

    parameters = signature.parameters
    accepts_var_kwargs = any(
        parameter.kind == inspect.Parameter.VAR_KEYWORD for parameter in parameters.values()
    )
    return {
        key: value
        for key, value in desired.items()
        if accepts_var_kwargs or key in parameters
    }


def describe_transcribe_kwargs(kwargs: dict) -> str:
    return ", ".join(f"{key}={value}" for key, value in kwargs.items()) or "defaults"


def handle_transcribe_command(model, parts: list[str]) -> None:
    _, segment_id, start_frame, end_frame, rms, wav_path = parts
    try:
        kwargs = transcribe_kwargs(model)
        print(
            f"STATUS\tTranscribing segment {segment_id} rms={rms} ({describe_transcribe_kwargs(kwargs)})",
            flush=True,
        )
        text = transcribe_one(model, wav_path)
        if not text:
            print(f"STATUS\tSegment {segment_id} produced empty transcript", flush=True)
        print(f"FINAL\t{segment_id}\t{start_frame}\t{end_frame}\t{text}", flush=True)
    except Exception as exc:
        print(f"ERROR\tTranscription failed for segment {segment_id}: {exc}", flush=True)
    finally:
        try:
            Path(wav_path).unlink(missing_ok=True)
        except Exception:
            pass


def prewarm_model(model) -> None:
    """Run a single tiny transcribe so the first user chunk doesn't pay JIT/setup cost."""
    import tempfile
    import wave

    print("STATUS\tPre-warming model", flush=True)
    try:
        with tempfile.NamedTemporaryFile(prefix="parakeet-prewarm-", suffix=".wav", delete=False) as fh:
            warmup_path = fh.name
        try:
            with wave.open(warmup_path, "wb") as writer:
                writer.setnchannels(1)
                writer.setsampwidth(2)
                writer.setframerate(16_000)
                writer.writeframes(b"\x00\x00" * 16_000)
            transcribe_one(model, warmup_path)
        finally:
            try:
                Path(warmup_path).unlink(missing_ok=True)
            except Exception:
                pass
        print("STATUS\tModel warmed", flush=True)
    except Exception as exc:
        print(f"STATUS\tPre-warm failed (continuing): {clean_text(exc)}", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", default="nvidia/parakeet-unified-en-0.6b")
    args = parser.parse_args()

    load_repo_env()
    print(f"STATUS\tLoading model {args.model}", flush=True)

    try:
        model = load_model(args.model)
    except Exception:
        return 2

    prewarm_model(model)

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

        handle_transcribe_command(model, parts)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
