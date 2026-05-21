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
    """Resolve the operator-requested device string into a torch device.

    Cross-platform rules (post-macOS-slice 2026-05-21, per the MPS
    spike entry in `docs/devel/ROADMAP.md`):

    * `auto`:
        - **Linux**: probe `torch.cuda.is_available()` AND
          `torch.cuda.mem_get_info(0)` >= 3 GiB free; pick `cuda` if
          both pass, else fall through to `cpu`. The memory probe
          (spike correction #4) catches DGX-style cases where another
          process (vLLM) owns the unified-memory pool: `is_available()`
          returns True but `model.to("cuda")` would OOM. CPU is a
          first-class production posture (~157 ms p50 warm on DGX
          Spark CPU).
        - **darwin**: resolve directly to `cpu` *without* probing MPS.
          The macOS spike found MPS regresses ~5x vs CPU on realistic
          ~600-char paragraph input despite winning on a 33-char probe
          — and worst-case cold MPS dispatch is 4 s. Default safety
          first; operators who want MPS must opt in explicitly via
          `HHAGENT_GLINER_RELEX_DEVICE=mps`.

    * `cpu`: accepted on every platform.

    * `cuda`: accepted on non-darwin; rejected on darwin with
      `UNSUPPORTED_DEVICE` (Apple Silicon has no NVIDIA GPU; Intel
      Macs lost NVIDIA support around macOS 10.14).

    * `mps`: accepted on darwin iff `torch.backends.mps.is_available()`
      returns True (catches Intel Macs, pre-12.3 macOS, and PyTorch
      builds without MPS). Rejected on non-darwin with
      `UNSUPPORTED_DEVICE`.

    On every rejection path we exit with `UNSUPPORTED_DEVICE` (-32604)
    + a stderr line naming both the bad value and the legal set for
    this platform — operators see misconfig at daemon startup, not
    silently degraded behaviour.
    """
    is_darwin = sys.platform == "darwin"

    if requested == "auto":
        if is_darwin:
            # MPS regresses on realistic input per spike; opt-in only.
            return "cpu"
        # Linux/other: existing CUDA probe + cpu fallback.
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

    if requested == "cpu":
        return "cpu"

    if requested == "cuda":
        if is_darwin:
            _exit_with_error(
                UNSUPPORTED_DEVICE,
                "device=cuda not supported on darwin (Apple Silicon has no "
                "NVIDIA GPU); set HHAGENT_GLINER_RELEX_DEVICE to auto|cpu|mps",
                status=2,
            )
        return "cuda"

    if requested == "mps":
        if not is_darwin:
            _exit_with_error(
                UNSUPPORTED_DEVICE,
                "device=mps not supported on this platform (mps is darwin-only); "
                "set HHAGENT_GLINER_RELEX_DEVICE to auto|cuda|cpu",
                status=2,
            )
        try:
            import torch
            if torch.backends.mps.is_available():
                return "mps"
        except Exception:
            pass
        _exit_with_error(
            UNSUPPORTED_DEVICE,
            "device=mps requested but torch.backends.mps.is_available() is "
            "False (Intel Mac, macOS < 12.3, or PyTorch build without MPS); "
            "set HHAGENT_GLINER_RELEX_DEVICE to auto|cpu",
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
