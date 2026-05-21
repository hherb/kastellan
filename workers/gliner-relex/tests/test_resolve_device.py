"""Tests for `__main__._resolve_device`.

Why a dedicated test file:

The `_resolve_device` helper used to be a trivial wrapper around
`torch.cuda.is_available()` + a `mps` rejection. The macOS slice
(2026-05-21, per `docs/devel/ROADMAP.md`'s "GLiNER-Relex worker —
macOS MPS spike" entry) widens it with a darwin branch:

  * On Linux: behaviour unchanged. `mps` rejected with
    `UNSUPPORTED_DEVICE`; `auto` probes CUDA memory.
  * On darwin:
      - `auto` resolves to `cpu` (not `mps`) — the spike found MPS
        regresses ~5x vs CPU on realistic ~600-char paragraph input
        despite winning on a 33-char probe. Default safety-first.
      - `mps` is accepted as an explicit operator opt-in IFF
        `torch.backends.mps.is_available()` returns True; otherwise
        rejected with `UNSUPPORTED_DEVICE` (parallel to the Linux
        cuda-not-available case which falls through to cpu under
        `auto` but rejects an explicit `device=cuda` at model load).
      - `cuda` rejected with `UNSUPPORTED_DEVICE` (M3 Max has no NVIDIA).

Tests use `monkeypatch` to swap `sys.platform` and patch the `torch`
module's `backends.mps.is_available` / `cuda.is_available` /
`cuda.mem_get_info` so each branch is exercised deterministically
without requiring the real platform / hardware.
"""
import sys

import pytest

from hhagent_worker_gliner_relex.__main__ import _resolve_device
from hhagent_worker_gliner_relex.errors import UNSUPPORTED_DEVICE


def _fake_torch(
    cuda_available: bool = False,
    cuda_free_bytes: int = 0,
    mps_available: bool = False,
):
    """Build a stand-in `torch` module surfacing only the attributes
    `_resolve_device` consults: `cuda.is_available()`, `cuda.mem_get_info(0)`,
    `backends.mps.is_available()`.

    Real torch is ~600 MB import-time on Apple Silicon; stubbing it out
    keeps the unit-test loop fast and removes a hardware dependency.
    """
    import types

    torch = types.SimpleNamespace()
    torch.cuda = types.SimpleNamespace(
        is_available=lambda: cuda_available,
        mem_get_info=lambda _i: (cuda_free_bytes, cuda_free_bytes),
    )
    torch.backends = types.SimpleNamespace(
        mps=types.SimpleNamespace(is_available=lambda: mps_available),
    )
    return torch


@pytest.fixture
def patched_torch_cpu(monkeypatch):
    """Stub `torch` such that nothing is available (cuda and mps both off).

    `_resolve_device("auto")` should resolve to `cpu` on every platform.
    """
    fake = _fake_torch(cuda_available=False, mps_available=False)
    monkeypatch.setitem(sys.modules, "torch", fake)
    return fake


# ------------- darwin branch (new behaviour) -------------

def test_darwin_auto_resolves_to_cpu_even_when_mps_available(monkeypatch):
    """The spike found MPS regresses ~5x vs CPU on realistic input. So
    `auto` on darwin must NEVER pick mps — operators who want mps must
    set `HHAGENT_GLINER_RELEX_DEVICE=mps` explicitly.
    """
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setitem(
        sys.modules,
        "torch",
        _fake_torch(mps_available=True),
    )
    assert _resolve_device("auto") == "cpu"


def test_darwin_explicit_mps_is_accepted_when_torch_reports_available(monkeypatch):
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setitem(
        sys.modules,
        "torch",
        _fake_torch(mps_available=True),
    )
    assert _resolve_device("mps") == "mps"


def test_darwin_explicit_mps_is_rejected_when_torch_reports_unavailable(
    monkeypatch, capsys
):
    """`device=mps` on darwin without working MPS support (e.g. Intel
    Mac, or some PyTorch builds) should fail loud at startup with
    `UNSUPPORTED_DEVICE` rather than silently falling through to cpu
    — operators who explicitly asked for mps need to see the misconfig.
    """
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setitem(
        sys.modules,
        "torch",
        _fake_torch(mps_available=False),
    )
    with pytest.raises(SystemExit) as excinfo:
        _resolve_device("mps")
    assert excinfo.value.code == 2
    err = capsys.readouterr().err
    assert f'"code": {UNSUPPORTED_DEVICE}' in err
    assert "mps" in err


def test_darwin_explicit_cpu_is_accepted(monkeypatch):
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setitem(sys.modules, "torch", _fake_torch())
    assert _resolve_device("cpu") == "cpu"


def test_darwin_explicit_cuda_is_rejected(monkeypatch, capsys):
    """Apple Silicon has no NVIDIA GPU; `device=cuda` on darwin must
    fail loud. (Even Intel Macs with discrete NVIDIA cards stopped
    being supported around macOS 10.14.)
    """
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setitem(sys.modules, "torch", _fake_torch())
    with pytest.raises(SystemExit) as excinfo:
        _resolve_device("cuda")
    assert excinfo.value.code == 2
    err = capsys.readouterr().err
    assert f'"code": {UNSUPPORTED_DEVICE}' in err
    assert "cuda" in err


# ------------- linux branch (existing behaviour preserved) -------------

def test_linux_explicit_mps_is_rejected(monkeypatch, capsys):
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setitem(sys.modules, "torch", _fake_torch())
    with pytest.raises(SystemExit) as excinfo:
        _resolve_device("mps")
    assert excinfo.value.code == 2
    err = capsys.readouterr().err
    assert f'"code": {UNSUPPORTED_DEVICE}' in err
    assert "mps" in err


def test_linux_auto_picks_cuda_when_available_with_enough_free_memory(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    # 4 GiB free, comfortably above the 3 GiB threshold.
    four_gib = 4 * 1024 * 1024 * 1024
    monkeypatch.setitem(
        sys.modules,
        "torch",
        _fake_torch(cuda_available=True, cuda_free_bytes=four_gib),
    )
    assert _resolve_device("auto") == "cuda"


def test_linux_auto_falls_back_to_cpu_when_cuda_unavailable(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setitem(sys.modules, "torch", _fake_torch(cuda_available=False))
    assert _resolve_device("auto") == "cpu"


def test_linux_auto_falls_back_to_cpu_when_cuda_low_on_memory(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    # 1 GiB free, well below the 3 GiB threshold (spike correction #4).
    one_gib = 1 * 1024 * 1024 * 1024
    monkeypatch.setitem(
        sys.modules,
        "torch",
        _fake_torch(cuda_available=True, cuda_free_bytes=one_gib),
    )
    assert _resolve_device("auto") == "cpu"


# ------------- cross-platform branches (unchanged) -------------

def test_explicit_cpu_is_passed_through(patched_torch_cpu):
    assert _resolve_device("cpu") == "cpu"


def test_unknown_device_string_is_rejected_with_unsupported_device(
    monkeypatch, capsys, patched_torch_cpu
):
    monkeypatch.setattr(sys, "platform", "linux")
    with pytest.raises(SystemExit) as excinfo:
        _resolve_device("tpu")
    assert excinfo.value.code == 2
    err = capsys.readouterr().err
    assert f'"code": {UNSUPPORTED_DEVICE}' in err
    assert "tpu" in err
