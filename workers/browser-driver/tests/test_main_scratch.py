"""Tests for the per-spawn writable-scratch redirect (#283).

A worker with `ephemeral_scratch` is handed a unique per-spawn directory by the
host through `KASTELLAN_WORKER_SCRATCH` (macOS; on Linux the env is unset and
bwrap's per-spawn `/tmp` tmpfs is the scratch). `_apply_worker_scratch` points
Chromium's `--user-data-dir` (`$TMPDIR`) and Playwright's Node driver
(`uv_os_homedir()` -> `$HOME`) at it so each browser spawn is isolated to its own
directory instead of sharing the host `/tmp`.
"""
import os

from kastellan_worker_browser_driver.__main__ import (
    WORKER_SCRATCH_ENV,
    _apply_worker_scratch,
)


def test_scratch_redirects_tmpdir_and_home(monkeypatch):
    """When the host provides a scratch dir, TMPDIR and HOME both point at it."""
    monkeypatch.setenv("TMPDIR", "/tmp")
    monkeypatch.setenv("HOME", "/tmp")
    monkeypatch.setenv(WORKER_SCRATCH_ENV, "/var/folders/ab/pyexec-123-4")

    _apply_worker_scratch()

    assert os.environ["TMPDIR"] == "/var/folders/ab/pyexec-123-4"
    assert os.environ["HOME"] == "/var/folders/ab/pyexec-123-4"


def test_scratch_unset_leaves_env_untouched(monkeypatch):
    """Linux path: the env is unset, so TMPDIR/HOME (manifest-set /tmp) stand."""
    monkeypatch.setenv("TMPDIR", "/tmp")
    monkeypatch.setenv("HOME", "/tmp")
    monkeypatch.delenv(WORKER_SCRATCH_ENV, raising=False)

    _apply_worker_scratch()

    assert os.environ["TMPDIR"] == "/tmp"
    assert os.environ["HOME"] == "/tmp"


def test_scratch_blank_is_ignored(monkeypatch):
    """A blank/whitespace value is treated as unset (fail-safe to the /tmp default)."""
    monkeypatch.setenv("TMPDIR", "/tmp")
    monkeypatch.setenv("HOME", "/tmp")
    monkeypatch.setenv(WORKER_SCRATCH_ENV, "   ")

    _apply_worker_scratch()

    assert os.environ["TMPDIR"] == "/tmp"
    assert os.environ["HOME"] == "/tmp"
