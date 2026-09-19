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
import os
import subprocess
import sys
from pathlib import Path

from kastellan_worker_gliner_relex.scratch import (
    TORCH_CACHE_SUBDIR,
    WORKER_SCRATCH_ENV,
    apply_worker_scratch,
    scratch_overrides,
)

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
    src = Path(__file__).resolve().parent.parent / "src"
    code = (
        "import sys\n"
        "import kastellan_worker_gliner_relex.__main__\n"
        "print('torch' in sys.modules)\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", code],
        env={**os.environ, "PYTHONPATH": os.pathsep.join([str(tmp_path), str(src)])},
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
