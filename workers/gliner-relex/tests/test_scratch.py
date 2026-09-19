"""Tests for `scratch`: pointing torch at the per-spawn scratch dir (issue #719).

Why this exists: torch 2.13 creates its compile-cache directory while
`import torch` is still running. On Linux that directory lands in bwrap's
per-spawn `/tmp` tmpfs. macOS Seatbelt has no tmpfs and grants no host `/tmp`,
so the host hands the worker a per-spawn directory in `KASTELLAN_WORKER_SCRATCH`
and the worker has to point torch (plus `TMPDIR` and `HOME`) into it. That only
works if it happens BEFORE torch is imported, which the last two tests pin:
importing the entry point must not load torch, and `main()` must apply the
redirect before `_serve()`, where everything that can load torch lives.

Nothing here needs torch or the weights: the helpers are pure, the order test
stubs both steps, and the import check shadows torch with an empty fake. So
these tests also run in CI's no-project job.
"""
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

from kastellan_worker_gliner_relex.errors import MODEL_LOAD_FAILED
from kastellan_worker_gliner_relex.scratch import (
    TORCH_CACHE_SUBDIR,
    WORKER_SCRATCH_ENV,
    apply_worker_scratch,
    scratch_overrides,
    scratch_problem,
)

SRC = Path(__file__).resolve().parent.parent / "src"

SCRATCH = "/var/folders/xy/T/pyexec-4242-0"


def test_no_scratch_env_means_no_overrides():
    # Linux: the host sets no scratch env, and the manifest's /tmp defaults stand.
    assert scratch_overrides({}) == {}


def test_blank_scratch_env_is_treated_as_unset():
    # Fail-safe: a blank value must not redirect everything to "" (the cwd).
    assert scratch_overrides({WORKER_SCRATCH_ENV: "   "}) == {}


def test_scratch_env_redirects_tmpdir_home_and_the_torch_cache():
    assert scratch_overrides({WORKER_SCRATCH_ENV: SCRATCH}) == {
        "TMPDIR": SCRATCH,
        "HOME": SCRATCH,
        "TORCHINDUCTOR_CACHE_DIR": os.path.join(SCRATCH, TORCH_CACHE_SUBDIR),
    }


def test_surrounding_whitespace_is_stripped_from_the_scratch_path():
    got = scratch_overrides({WORKER_SCRATCH_ENV: f"  {SCRATCH}\n"})
    assert got["TMPDIR"] == SCRATCH


def test_scratch_beats_the_manifests_linux_default():
    # The Rust manifest seeds TORCHINDUCTOR_CACHE_DIR=/tmp/torchinductor, which
    # is right on Linux and unwritable on macOS. When a scratch dir exists it
    # must win, or the worker dies at import exactly as #719 did.
    env = {
        WORKER_SCRATCH_ENV: SCRATCH,
        "TORCHINDUCTOR_CACHE_DIR": "/tmp/torchinductor",
    }
    apply_worker_scratch(env)
    assert env["TORCHINDUCTOR_CACHE_DIR"] == os.path.join(SCRATCH, TORCH_CACHE_SUBDIR)


def test_without_scratch_the_environment_is_left_alone():
    env = {"TORCHINDUCTOR_CACHE_DIR": "/tmp/torchinductor", "USER": "kastellan"}
    apply_worker_scratch(env)
    assert env == {"TORCHINDUCTOR_CACHE_DIR": "/tmp/torchinductor", "USER": "kastellan"}


def test_the_default_target_is_the_real_process_environment(monkeypatch, tmp_path):
    """Production calls `apply_worker_scratch()` with no argument.

    Every other test passes its own dict, so a default that redirected a COPY
    of `os.environ` would leave production un-redirected with all of them green.
    """
    # setenv/delenv first, so monkeypatch restores all three afterwards.
    monkeypatch.setenv(WORKER_SCRATCH_ENV, str(tmp_path))
    monkeypatch.setenv("TMPDIR", "/unchanged")
    monkeypatch.setenv("HOME", "/unchanged")
    monkeypatch.setenv("TORCHINDUCTOR_CACHE_DIR", "/tmp/torchinductor")
    apply_worker_scratch()
    assert os.environ["TORCHINDUCTOR_CACHE_DIR"] == os.path.join(str(tmp_path), TORCH_CACHE_SUBDIR)
    assert os.environ["TMPDIR"] == str(tmp_path)
    assert os.environ["HOME"] == str(tmp_path)


def test_an_unset_or_blank_scratch_dir_is_not_a_problem():
    assert scratch_problem({}) is None
    assert scratch_problem({WORKER_SCRATCH_ENV: "  "}) is None


def test_a_usable_scratch_dir_is_not_a_problem(tmp_path):
    assert scratch_problem({WORKER_SCRATCH_ENV: str(tmp_path)}) is None


def _a_file(path: Path) -> str:
    path.write_text("")
    return str(path)


@pytest.mark.parametrize(
    "make, why",
    [
        (lambda tmp: "relative/dir", "not an absolute path"),
        (lambda tmp: str(tmp / "missing"), "not an existing directory"),
        (lambda tmp: _a_file(tmp / "f"), "not an existing directory"),
    ],
    ids=["relative", "missing", "a-file"],
)
def test_an_unusable_scratch_dir_is_named(tmp_path, make, why):
    problem = scratch_problem({WORKER_SCRATCH_ENV: make(tmp_path)})
    assert problem is not None and WORKER_SCRATCH_ENV in problem and why in problem


@pytest.mark.skipif(hasattr(os, "geteuid") and os.geteuid() == 0, reason="root can write anywhere")
def test_a_read_only_scratch_dir_is_named(tmp_path):
    ro = tmp_path / "ro"
    ro.mkdir()
    ro.chmod(0o500)
    try:
        problem = scratch_problem({WORKER_SCRATCH_ENV: str(ro)})
    finally:
        ro.chmod(0o700)
    assert problem is not None and "not writable" in problem


def _stderr_error(stderr: str) -> dict:
    """The worker's one structured startup-error line (the last stderr line)."""
    return json.loads(stderr.strip().splitlines()[-1])


def test_an_unusable_scratch_dir_fails_startup_with_a_structured_error(tmp_path):
    result = subprocess.run(
        [sys.executable, "-c", "from kastellan_worker_gliner_relex.__main__ import main; main()"],
        env={
            **os.environ,
            "PYTHONPATH": str(SRC),
            WORKER_SCRATCH_ENV: str(tmp_path / "gone"),
        },
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 1, result.stderr
    err = _stderr_error(result.stderr)
    assert err["code"] == MODEL_LOAD_FAILED
    assert WORKER_SCRATCH_ENV in err["message"]


def test_a_failing_model_import_is_a_structured_error_not_a_traceback(tmp_path):
    """#719's shape on macOS: `auto` resolves to cpu without torch, so the model
    import is where torch first loads. If it fails there, the host must get a
    `MODEL_LOAD_FAILED` line, not a raw traceback and a contentless EarlyExit.

    A fake `gliner` that raises on import stands in for torch dying with EPERM.
    """
    fake = tmp_path / "fakes"
    (fake / "gliner").mkdir(parents=True)
    (fake / "gliner" / "__init__.py").write_text(
        "raise PermissionError(1, 'Operation not permitted', '/tmp/torchinductor')\n"
    )
    env = {k: v for k, v in os.environ.items() if k != WORKER_SCRATCH_ENV}
    env.update(
        PYTHONPATH=os.pathsep.join([str(fake), str(SRC)]),
        KASTELLAN_GLINER_RELEX_WEIGHTS_DIR=str(tmp_path),
        KASTELLAN_GLINER_RELEX_MODEL="unused",
        KASTELLAN_GLINER_RELEX_DEVICE="cpu",
    )
    result = subprocess.run(
        [sys.executable, "-c", "from kastellan_worker_gliner_relex.__main__ import main; main()"],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 1, result.stderr
    assert "Traceback" not in result.stderr, result.stderr
    err = _stderr_error(result.stderr)
    assert err["code"] == MODEL_LOAD_FAILED
    assert "Operation not permitted" in err["message"]


def test_a_failing_torch_import_under_linux_auto_is_reported_not_swallowed(monkeypatch, capsys):
    """Linux `auto` probes CUDA by importing torch. A failed import must name
    its cause, not fall back to cpu and let the model import fail again later
    with an error about a half-initialised module."""
    import kastellan_worker_gliner_relex.__main__ as entry

    monkeypatch.setattr(entry.sys, "platform", "linux")
    monkeypatch.setitem(sys.modules, "torch", None)  # makes `import torch` raise
    with pytest.raises(SystemExit) as exit_info:
        entry._resolve_device("auto")
    assert exit_info.value.code == 1
    err = _stderr_error(capsys.readouterr().err)
    assert err["code"] == MODEL_LOAD_FAILED
    assert "import torch failed" in err["message"]


def test_importing_the_entry_point_does_not_import_torch(tmp_path):
    """The redirect is useless if torch is already imported when it runs.

    `main()` applies it first and imports the model stack afterwards, so the
    entry-point module must not pull torch in at import time. Moving
    `from .model import GlinerModel` back to the top of `__main__.py` would
    silently undo the #719 fix on macOS; this test fails instead.

    Run in a subprocess, because this pytest session may already have torch
    loaded. An empty fake `torch` package goes first on `PYTHONPATH`, so any
    `import torch` succeeds and shows up in `sys.modules`, whether or not the
    host has the real one. Without the fake, a no-torch runner (CI) would
    report "torch not loaded" for a guarded `try: import torch` regression.
    """
    fake_torch = tmp_path / "torch"
    fake_torch.mkdir()
    (fake_torch / "__init__.py").write_text("")
    code = (
        "import sys\n"
        "import kastellan_worker_gliner_relex.__main__\n"
        "print('torch' in sys.modules)\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", code],
        env={**os.environ, "PYTHONPATH": os.pathsep.join([str(tmp_path), str(SRC)])},
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, f"importing __main__ failed:\n{result.stderr}"
    assert result.stdout.strip() == "False", (
        "importing kastellan_worker_gliner_relex.__main__ imported torch; the "
        "scratch redirect in main() would then run too late (issue #719)"
    )


def test_main_applies_the_scratch_redirect_before_serving(monkeypatch):
    """`main()` must redirect first: `_serve()` is where torch gets imported.

    `_serve()` resolves the device (which imports torch on Linux `auto` and on
    macOS `mps`) and loads the model. Swapping the two calls in `main()` would
    load torch before its cache dir is writable on macOS, and every other test
    here would still pass.
    """
    import kastellan_worker_gliner_relex.__main__ as entry

    calls = []
    monkeypatch.setattr(entry, "apply_worker_scratch", lambda: calls.append("scratch"))
    monkeypatch.setattr(entry, "_serve", lambda: calls.append("serve"))
    entry.main()
    assert calls == ["scratch", "serve"]
