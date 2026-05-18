"""Entry point for `hhagent-worker-gliner-relex` (uv-generated shim).

Reads the required env vars (see the spec's "Manifest entry" section
for the canonical list), resolves the device, loads the model, and
hands off to Server.run(stdin, stdout).

Startup errors (`MODEL_LOAD_FAILED`, `UNSUPPORTED_DEVICE`) write one
JSON-encoded line to STDERR and exit with a non-zero status BEFORE
the stdio loop starts. The slice-2 crash classifier in the Rust side
maps these to `ClientError::EarlyExit` → "dead".
"""
import json
import os
import sys

from .errors import MODEL_LOAD_FAILED, UNSUPPORTED_DEVICE
from .model import GlinerModel
from .server import Server

# Spike correction #4: torch.cuda.is_available() returns True even when
# vLLM (or another process) owns the unified-memory pool on a DGX Spark
# and model.to("cuda") would OOM. Probe actual free memory and require
# >= 3 GiB before committing to cuda under "auto". 3 GiB covers
# multi-v1.0 (~2-3 GB resident at fp32) + transient activations.
_MIN_CUDA_FREE_BYTES = 3 * 1024 * 1024 * 1024


def _exit_with_error(code: int, message: str, status: int) -> None:
    """Write a structured stderr line and exit with the given status."""
    print(
        json.dumps({"level": "error", "code": code, "message": message}),
        file=sys.stderr,
        flush=True,
    )
    sys.exit(status)


def _resolve_device(requested: str) -> str:
    """Resolve `auto` to `cuda` (with memory probe) or `cpu`. Reject
    `mps` on Linux (the macOS follow-up will widen this).

    On `auto`: require torch.cuda.is_available() AND
    torch.cuda.mem_get_info(0) reporting >= 3 GiB free before
    selecting cuda. Otherwise fall back to cpu silently — CPU is a
    first-class production posture (~157 ms p50 warm on the DGX Spark
    spike). No warning here; the audit log shows which device a worker
    started under via the Rust side's startup row.
    """
    if requested == "auto":
        try:
            import torch
            if torch.cuda.is_available():
                try:
                    free, _total = torch.cuda.mem_get_info(0)
                    if free >= _MIN_CUDA_FREE_BYTES:
                        return "cuda"
                except Exception:
                    # mem_get_info can raise on misconfigured drivers;
                    # fall through to cpu rather than crash startup.
                    pass
        except Exception:
            pass
        return "cpu"
    if requested in ("cuda", "cpu"):
        return requested
    if requested == "mps":
        _exit_with_error(
            UNSUPPORTED_DEVICE,
            "device=mps not supported on this platform (Linux build); "
            "set HHAGENT_GLINER_RELEX_DEVICE to auto|cuda|cpu",
            status=2,
        )
    _exit_with_error(
        UNSUPPORTED_DEVICE,
        f"unknown device: {requested}",
        status=2,
    )
    # Unreachable; keep mypy/pyright happy.
    return requested


def main() -> None:
    weights_dir = os.environ.get("HHAGENT_GLINER_RELEX_WEIGHTS_DIR")
    model_id = os.environ.get("HHAGENT_GLINER_RELEX_MODEL")
    device_requested = os.environ.get("HHAGENT_GLINER_RELEX_DEVICE", "auto")

    if not weights_dir:
        _exit_with_error(
            MODEL_LOAD_FAILED,
            "HHAGENT_GLINER_RELEX_WEIGHTS_DIR is unset",
            status=1,
        )
    if not model_id:
        _exit_with_error(
            MODEL_LOAD_FAILED,
            "HHAGENT_GLINER_RELEX_MODEL is unset",
            status=1,
        )
    if not os.path.isdir(weights_dir):
        _exit_with_error(
            MODEL_LOAD_FAILED,
            f"weights directory missing: {weights_dir}",
            status=1,
        )

    device = _resolve_device(device_requested)

    try:
        model = GlinerModel.load(
            weights_dir=weights_dir,
            model_id=model_id,
            device=device,
        )
    except Exception as e:
        _exit_with_error(
            MODEL_LOAD_FAILED,
            f"GLiNER.from_pretrained failed: {e}",
            status=1,
        )

    server = Server(model=model)
    server.run(sys.stdin, sys.stdout)


if __name__ == "__main__":
    main()
