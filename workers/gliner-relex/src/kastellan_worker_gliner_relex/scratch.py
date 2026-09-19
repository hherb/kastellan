"""Point torch and the temp/home dirs at the host's per-spawn scratch dir.

Issue #719. torch 2.13 creates its compile-cache directory while `import torch`
is still running (`torch._dynamo.package` builds its disk cache at module load).
The worker must therefore have a writable directory before it imports anything
heavy:

* **Linux:** bwrap gives every spawn a private `/tmp` tmpfs. The Rust manifest
  sets `TORCHINDUCTOR_CACHE_DIR=/tmp/torchinductor`, and that works as is.
* **macOS:** Seatbelt has no tmpfs and the policy grants no host `/tmp`, so
  that same path is unwritable and the import fails with EPERM. Instead the
  host creates a unique directory per spawn, grants it, and names it in
  `KASTELLAN_WORKER_SCRATCH` (the Rust side is
  `kastellan_core::tool_host::prepare_ephemeral_scratch`). This module points
  torch's cache, `TMPDIR` and `HOME` into it.

The browser-driver worker does the same for Chromium; see its `__main__.py`.

`main()` must call `apply_worker_scratch()` before importing the model, which
is why `__main__.py` imports the model inside `main()` rather than at the top.
"""
import os
from typing import Dict, Mapping, MutableMapping

# Per-spawn scratch dir the host grants on macOS. Keep in sync with the Rust
# constant `kastellan_core::tool_host::ENV_WORKER_SCRATCH`.
WORKER_SCRATCH_ENV = "KASTELLAN_WORKER_SCRATCH"

# Subdirectory of the scratch dir that holds torch's compile cache. torch
# creates it itself; we only name it.
TORCH_CACHE_SUBDIR = "torchinductor"


def scratch_overrides(environ: Mapping[str, str]) -> Dict[str, str]:
    """Return the environment variables to override, or `{}` if there is no scratch dir.

    Pure: reads only `environ`. A missing or blank `KASTELLAN_WORKER_SCRATCH`
    means "no scratch dir" (the Linux case), so nothing is overridden and the
    manifest's `/tmp` defaults stand. Blank counts as unset on purpose: an
    empty path would send every write to the current directory.
    """
    scratch = environ.get(WORKER_SCRATCH_ENV, "").strip()
    if not scratch:
        return {}
    return {
        "TMPDIR": scratch,
        "HOME": scratch,
        "TORCHINDUCTOR_CACHE_DIR": os.path.join(scratch, TORCH_CACHE_SUBDIR),
    }


def apply_worker_scratch(environ: MutableMapping[str, str] = os.environ) -> None:
    """Apply `scratch_overrides` to `environ` (the real process env by default).

    Call this before importing torch: torch reads `TORCHINDUCTOR_CACHE_DIR`
    while it is being imported.
    """
    environ.update(scratch_overrides(environ))
