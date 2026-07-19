# browser-driver micro-VM rootfs (slice 1) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce a `browser-driver.ext4` Firecracker rootfs that boots on the DGX, runs the existing unmodified Python browser-driver worker over the vsock stdio bridge, and successfully launches Chromium.

**Architecture:** A build-time-only Dockerfile stages Ubuntu + a venv + `playwright install --with-deps chromium`; `docker export` produces the staging tree, which then flows through the *same* `mkfs.ext4 -d` tail every existing rootfs script uses. Docker never appears at runtime. No production Rust changes except possibly a `microvm-init` mount fix, which Task 1 decides.

**Tech Stack:** Bash, Docker (build-time only), Ubuntu 24.04 arm64, Python 3.12 venv, Playwright 1.60.0 + Chromium, Rust (`kastellan-microvm-init`, `kastellan-sandbox`, `kastellan-core` integration tests), ext4.

**Spec:** `docs/superpowers/specs/2026-07-19-browser-driver-microvm-rootfs-design.md`

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible dependencies only.** Playwright is Apache-2.0, Chromium BSD, readability-lxml Apache-2.0, lxml BSD — all fine. Block CDDL / BUSL / SSPL / Elastic / "source-available".
- **Cross-platform discipline.** This slice is Linux-only by nature (Firecracker). It must not break the macOS build. Everything Linux-gated uses `#[cfg(target_os = "linux")]` or `#![cfg(target_os = "linux")]`.
- **Rust core, Python only inside sandboxed workers.** No PyO3, no in-process Python.
- **Every worker is sandboxed before it runs.** No "spawn unsandboxed" escape hatch.
- **In-rootfs binary path:** `/usr/local/bin/kastellan-worker-browser-driver`. Never a host `target/debug` path — see memory `vm-worker-in-rootfs-binary-path`.
- **Never bake content under a share anchor** (`/opt /data /srv /mnt /work /tmp`) — `apply_host_mounts` tmpfs-mounts each anchor and would shadow it (`sandbox/src/linux_firecracker/mounts.rs:44`).
- **Cmdline budget:** `MAX_CMDLINE_BYTES = 1920` (`sandbox/src/linux_firecracker/plan.rs:137`); env is hex-encoded so every env byte costs two.
- **Files under 500 lines** where feasible; pure functions in reusable modules preferred; inline documentation understandable by a junior contributor is mandatory.
- **All tests must pass before committing.**
- **Cargo needs sourcing** in non-interactive shells: `source "$HOME/.cargo/env"`.
- **DGX is the gate.** Drive it as exactly `ssh dgx '<cmd>'` (the allow-rule is a prefix match; flags before the hostname get denied). Iterate there, not on the Mac — the IDE's rust-analyzer holds `target/debug/.cargo-lock` (memory `mac-cargo-buildlock-prefer-dgx`). Write run logs to `~`, never `/tmp` (memory `dgx-run-logs-tmp-scrubbed`).
- **Baseline to hold:** DGX `cargo test --workspace` = **2584 / 0 / 47**, `clippy --workspace --all-targets -D warnings` clean.

---

## File Structure

| File | Responsibility | Task |
|---|---|---|
| `scripts/workers/microvm/Dockerfile.browser-driver` | **Create.** Build-time-only recipe: Ubuntu + venv + Playwright + Chromium + the worker package. Layer-ordered so worker-code edits rebuild only the last layer. | 2 |
| `scripts/workers/microvm/build-browser-driver-rootfs.sh` | **Create.** docker build → create → export → strip → install init → `mkfs.ext4 -d`. Mirrors `build-web-fetch-rootfs.sh`. | 2 |
| `workers/microvm-init/src/guest.rs:176-197` | **Modify, only if Task 1 says so.** `mount_pseudo_fs` gains `/dev` and/or `/dev/shm`. | 1, 3 |
| `core/tests/browser_driver_firecracker_e2e.rs` | **Create.** Hermetic launch-plan pin (always runs on Linux) + the `#[ignore]` DGX boot/render spike. | 4, 5 |
| `docs/superpowers/specs/2026-07-19-...-design.md` | **Modify.** Revision section recording Task 1's finding + two spec corrections. | 1, 6 |
| `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md` | **Modify.** Session close-out. | 6 |

---

## Task 1: Determine the guest `/dev` reality (decision gate)

This is a **measurement task**, not a code task. Its deliverable is a recorded
finding that selects among spec §4.3's options A–D. Do not write a mount change
before this task completes — option D means no change at all.

**Files:**
- Modify (temporarily, reverted in Step 6): `workers/microvm-run/src/main.rs:84-86`
- Modify (temporarily, reverted in Step 6): `workers/microvm-init/src/main.rs`
- Modify (permanent): `docs/superpowers/specs/2026-07-19-browser-driver-microvm-rootfs-design.md`

**Interfaces:**
- Consumes: nothing.
- Produces: a decision — **option A, B, C, or D** — consumed by Task 3. Also produces the boolean facts `devtmpfs_automounted` and `dev_shm_present`.

- [ ] **Step 1: Try the cheap check first — is the kernel config readable?**

```bash
ssh dgx 'cd ~/src/kastellan && ls -la /var/lib/kastellan/microvm/vmlinux && \
  (grep -a "CONFIG_DEVTMPFS" /var/lib/kastellan/microvm/vmlinux | head -5 || echo "no embedded config strings")'
```

Expected: either lines containing `CONFIG_DEVTMPFS_MOUNT=y` (conclusive — devtmpfs auto-mounts) or nothing (inconclusive; the kernel lacks `CONFIG_IKCONFIG`). If conclusive, skip to Step 5.

- [ ] **Step 2: Un-null the guest console so the guest can talk to us**

In `workers/microvm-run/src/main.rs`, the firecracker child currently discards
all output at lines 84-86:

```rust
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
```

Immediately **before** the statement containing that builder chain, open a log
file; then redirect only `stdout`/`stderr` into it (leave `stdin` null):

```rust
    // TEMPORARY DIAGNOSTIC (Task 1) — reverted in Step 6.
    // The guest console is normally discarded to keep our stdout pristine for
    // JSON-RPC. Route it to a file so the guest can report /proc/mounts.
    let console_log = std::fs::File::create(run_dir.join("guest-console.log"))
        .expect("create guest console log");
    let console_err = console_log.try_clone().expect("clone console log handle");
```

and change the three builder lines to:

```rust
        .stdin(Stdio::null())
        .stdout(Stdio::from(console_log))
        .stderr(Stdio::from(console_err))
```

Note: `run_dir` is the local holding the `--run-dir` argument. If the binding
has a different name in this function, use that name instead. If it is a `&str`
rather than a `Path`, use `std::path::Path::new(run_dir).join("guest-console.log")`.

- [ ] **Step 3: Make the guest dump what we need to know**

In `workers/microvm-init/src/main.rs`, immediately **after** `mount_pseudo_fs();` (around line 45) and **before** `apply_host_mounts(...)`, insert:

```rust
// TEMPORARY DIAGNOSTIC (Task 1) — reverted in Step 6.
// Answers: does the guest kernel auto-mount devtmpfs, and does /dev/shm exist?
eprintln!("=== KASTELLAN TASK1 BEGIN ===");
eprintln!(
    "TASK1 /proc/mounts:\n{}",
    std::fs::read_to_string("/proc/mounts").unwrap_or_else(|e| format!("<unreadable: {e}>"))
);
for p in ["/dev", "/dev/null", "/dev/urandom", "/dev/shm"] {
    eprintln!("TASK1 exists {p} = {}", std::path::Path::new(p).exists());
}
eprintln!("=== KASTELLAN TASK1 END ===");
```

- [ ] **Step 4: Rebuild and boot an existing VM, then read the console**

The web-fetch rootfs is the cheapest vehicle. Its init is baked into the image, so the rootfs must be rebuilt for the diagnostic to appear.

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && export PATH=$HOME/.local/bin:$PATH && \
  cargo build --release -p kastellan-microvm-run -p kastellan-microvm-init && \
  bash scripts/workers/microvm/build-web-fetch-rootfs.sh && \
  cargo test -p kastellan-core --test web_fetch_firecracker_egress_e2e -- --ignored --nocapture > ~/task1.log 2>&1; \
  echo "DONE_EXIT=$?"'
```

Then find and read the console log (the run dir is printed in the test output; the test may clean it up, so grep the test log first):

```bash
ssh dgx 'grep -A 40 "KASTELLAN TASK1 BEGIN" ~/task1.log || \
  (echo "--- not in test log; searching run dirs ---"; \
   find /tmp -name guest-console.log -newermt "-10 minutes" 2>/dev/null | head -3)'
```

Expected: a `/proc/mounts` dump plus four `TASK1 exists` lines.

**Read the two facts off it:**
- `devtmpfs_automounted` = does `/proc/mounts` contain a `devtmpfs /dev` line?
- `dev_shm_present` = is `TASK1 exists /dev/shm = true`?

If the rootfs rebuild fails because `firecracker` is missing from PATH, the test SKIP-as-passes silently and you will see no `TASK1` lines — re-check `export PATH=$HOME/.local/bin:$PATH` (memory `firecracker-e2e-stale-release-launcher`).

- [ ] **Step 5: Select the option and record the finding in the spec**

Append a Revision section to
`docs/superpowers/specs/2026-07-19-browser-driver-microvm-rootfs-design.md`:

```markdown
---

## 10. Revisions

### 10.1 Task 1 finding — the guest `/dev` reality (2026-07-19)

Measured by booting the web-fetch rootfs with the guest console un-nulled and
dumping `/proc/mounts` from `microvm-init` before `apply_host_mounts`.

- `devtmpfs` auto-mounted at `/dev`: **<YES|NO>**
- `/dev/shm` present: **<YES|NO>**

Raw `/proc/mounts`:

```
<paste the dump>
```

**Decision: option <A|B|C|D>.** <One sentence of reasoning tied to the two facts
above, using §4.3's option table.>
```

Selection rule, applied mechanically:
- devtmpfs auto-mounts **and** `/dev/shm` present → **option D** (no init change).
- devtmpfs auto-mounts, `/dev/shm` absent → a change is needed for `/dev/shm` only → **option A** (spec's stated preference), unless you judge the blast radius unacceptable, in which case **B**.
- devtmpfs does not auto-mount → both mounts needed → **option A**, same caveat.
- Choose **C** only if you have a concrete reason the mount must be opt-in; the spec argues against it.

- [ ] **Step 6: Revert both diagnostic patches**

```bash
git checkout -- workers/microvm-run/src/main.rs workers/microvm-init/src/main.rs
git diff --stat   # must show ONLY the spec file modified
```

Expected: only `docs/superpowers/specs/2026-07-19-browser-driver-microvm-rootfs-design.md` changed.

- [ ] **Step 7: Rebuild the web-fetch rootfs clean (undo the diagnostic image)**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && \
  cargo build --release -p kastellan-microvm-init && \
  bash scripts/workers/microvm/build-web-fetch-rootfs.sh'
```

Expected: `built /var/lib/kastellan/microvm/web-fetch.ext4 (+ shared .../vmlinux)`. This matters — leaving a diagnostic-laden image in the shared dir would pollute every later web-fetch run.

- [ ] **Step 8: Commit**

```bash
git add docs/superpowers/specs/2026-07-19-browser-driver-microvm-rootfs-design.md
git commit -m "docs(spec): record Task 1 finding — guest /dev reality, option <X> selected"
```

---

## Task 2: Dockerfile + rootfs build script

**Files:**
- Create: `scripts/workers/microvm/Dockerfile.browser-driver`
- Create: `scripts/workers/microvm/build-browser-driver-rootfs.sh`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `/var/lib/kastellan/microvm/browser-driver.ext4`, containing the worker at `/usr/local/bin/kastellan-worker-browser-driver`, browsers at `/usr/local/lib/kastellan-browser-driver/browsers`, and `microvm-init` at `/sbin/init`. Tasks 4 and 5 boot this image.

- [ ] **Step 1: Write the Dockerfile**

Create `scripts/workers/microvm/Dockerfile.browser-driver`:

```dockerfile
# Build-time-only recipe for the browser-driver micro-VM rootfs.
#
# Docker is NOT a runtime dependency. This image exists purely so
# `docker export` can hand us a staging tree; the tree then flows through the
# same `mkfs.ext4 -d` tail every other rootfs script uses
# (scripts/workers/microvm/build-*.sh).
#
# Why Docker at all: Chromium dlopen's NSS modules, fontconfig backends and
# SwiftShader at runtime. `ldd` cannot see any of those, so the from-scratch
# lib-closure pattern the other rootfs scripts use does not work here.
# `playwright install --with-deps chromium` resolves that closure for us,
# which is exactly the job it is maintained to do.
#
# Build context is `workers/browser-driver/` (NOT the repo root) so the whole
# `target/` tree is never sent to the daemon.
#
# Layer ordering is deliberate: apt, the venv, and the ~100 MB Chromium
# download all sit ABOVE the worker-source COPY, so editing worker code
# rebuilds only the final layer. Slices 2 and 3 iterate on this image.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
ENV KASTELLAN_BD_ROOT=/usr/local/lib/kastellan-browser-driver
ENV PLAYWRIGHT_BROWSERS_PATH=/usr/local/lib/kastellan-browser-driver/browsers

# 1. Interpreter. Use the system python3 + venv deliberately (NOT uv): a uv
#    venv symlinks to an external CPython, and we need the interpreter to live
#    inside the rootfs. Same reasoning as
#    scripts/workers/browser-driver/install.sh.
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 python3-venv ca-certificates \
 && rm -rf /var/lib/apt/lists/*

RUN python3 -m venv "$KASTELLAN_BD_ROOT/venv"

# 2. Third-party deps FIRST, pinned, so the expensive layers cache. These are
#    the same three declared in workers/browser-driver/pyproject.toml; the pin
#    matches workers/browser-driver/uv.lock.
RUN "$KASTELLAN_BD_ROOT/venv/bin/pip" install --no-cache-dir \
      "playwright==1.60.0" "readability-lxml>=0.8" "lxml>=5"

# 3. Chromium + its apt dependency closure. The expensive layer (~100 MB
#    download + ~250 MB of apt deps); kept above the source COPY on purpose.
RUN "$KASTELLAN_BD_ROOT/venv/bin/playwright" install --with-deps chromium \
 && rm -rf /var/lib/apt/lists/*

# 4. Build-time launch smoke. If the dlopen closure is incomplete, FAIL THE
#    BUILD HERE — loudly and locally — rather than during a VM boot where the
#    failure surfaces as an opaque render error. This does not prove the VM
#    case (different kernel and mounts), but it catches the missing-library
#    class, which is the main risk.
RUN set -eux; \
    shell_bin="$(find "$PLAYWRIGHT_BROWSERS_PATH" -name headless_shell -type f | head -1)"; \
    test -n "$shell_bin"; \
    "$shell_bin" --no-sandbox --dump-dom about:blank > /dev/null; \
    echo "chromium headless smoke OK: $shell_bin"

# 5. The worker package LAST, non-editable + --no-deps, so its source is copied
#    into site-packages and a code edit invalidates only this layer.
COPY . /src/browser-driver
RUN "$KASTELLAN_BD_ROOT/venv/bin/pip" install --no-cache-dir --no-deps \
      --force-reinstall /src/browser-driver \
 && rm -rf /src

# 6. Stable entrypoint path. The console script the venv generated lives in the
#    venv's bin/; symlink it to the path baked into the kernel cmdline
#    (MICROVM_WORKER_BIN, slice 2). Must NOT sit under a share anchor
#    (/opt /data /srv /mnt /work /tmp) — apply_host_mounts tmpfs-mounts those
#    and would shadow it.
RUN ln -sf "$KASTELLAN_BD_ROOT/venv/bin/kastellan-worker-browser-driver" \
           /usr/local/bin/kastellan-worker-browser-driver \
 && /usr/local/bin/kastellan-worker-browser-driver --help > /dev/null 2>&1 \
    || echo "note: worker has no --help; symlink presence verified by build script"
```

- [ ] **Step 2: Build the image on the DGX and confirm the smoke passed**

```bash
ssh dgx 'cd ~/src/kastellan && \
  docker build -f scripts/workers/microvm/Dockerfile.browser-driver \
    -t kastellan-browser-driver-rootfs:latest workers/browser-driver 2>&1 | tail -30'
```

Expected: `chromium headless smoke OK: /usr/local/lib/kastellan-browser-driver/browsers/.../headless_shell` and a successful build.

If the smoke fails with a missing `.so`, that is the real finding of this slice — add the package to the Step-1 `apt-get install` list, note which one and why in the Dockerfile, and rebuild.

- [ ] **Step 3: Measure the staging size before writing `ROOTFS_MIB`**

```bash
ssh dgx 'cid=$(docker create kastellan-browser-driver-rootfs:latest) && \
  mkdir -p ~/bd-stage && docker export "$cid" | tar -C ~/bd-stage -xf - && \
  docker rm "$cid" >/dev/null && \
  du -sh ~/bd-stage && du -sh ~/bd-stage/usr/local/lib/kastellan-browser-driver/browsers/* 2>/dev/null; \
  rm -rf ~/bd-stage'
```

Record the total. Per spec §4.2.1 expect ~800 MB pre-strip; commit `ROOTFS_MIB = ceil(measured_MiB × 1.2)`, expected to land at 768–1024.

- [ ] **Step 4: Write the build script**

Create `scripts/workers/microvm/build-browser-driver-rootfs.sh`. Set `ROOTFS_MIB` to the Step-3 measurement — **do not leave the 1536 placeholder**.

```bash
#!/usr/bin/env bash
# Build the browser-driver micro-VM rootfs (ext4) into the SHARED image dir,
# beside python-exec.ext4 + the shared vmlinux.
#
# Unlike its sibling scripts, the staging tree comes from `docker export`
# rather than an `ldd` closure: Chromium dlopen's NSS/fontconfig/SwiftShader,
# which ldd cannot see. See scripts/workers/microvm/Dockerfile.browser-driver
# for the full rationale. Docker is build-time only — the runtime is pure
# Firecracker, exactly like every other worker.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh: ./scripts/workers/microvm/build-browser-driver-rootfs.sh" >&2
    exit 1
fi
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
IMAGE_TAG="kastellan-browser-driver-rootfs:latest"
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *)
        echo "Unsupported architecture '${HOST_ARCH}'. The pinned guest kernel is published for x86_64 and aarch64 only." >&2
        exit 1
        ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
# Measured from the docker-export staging tree, x1.2 headroom (spec S4.2.1).
ROOTFS_MIB=<MEASURED>

if ! command -v docker >/dev/null 2>&1; then
    echo "docker is required to build this rootfs (build-time only; not a runtime dep)." >&2
    exit 1
fi

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write the micro-VM image dir: $OUT_DIR" >&2
    echo "Run the one-time privileged setup first:" >&2
    echo "    sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
    echo "or build into a user-writable dir (set the same KASTELLAN_MICROVM_DIR in the service env):" >&2
    echo "    KASTELLAN_MICROVM_DIR=\"\$HOME/.local/share/kastellan/microvm\" ./scripts/workers/microvm/build-browser-driver-rootfs.sh" >&2
    exit 1
fi

# Shared guest kernel (pinned). Reused if another build-*-rootfs.sh fetched it.
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

# Guest PID1. Built on the host with cargo (native on the DGX aarch64), exactly
# as every sibling script does.
source "$HOME/.cargo/env"
cargo build --release -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT

# Staging tree from the container image. Context is workers/browser-driver so
# the repo's target/ tree is never sent to the daemon.
docker build -f "$REPO_ROOT/scripts/workers/microvm/Dockerfile.browser-driver" \
    -t "$IMAGE_TAG" "$REPO_ROOT/workers/browser-driver"
CID=$(docker create "$IMAGE_TAG")
trap 'rm -rf "$WORK"; docker rm -f "$CID" >/dev/null 2>&1 || true' EXIT
docker export "$CID" | tar -C "$WORK" -xf -

# Strip what a read-only render VM never reads. Keep fonts and NSS.
rm -rf "$WORK/var/lib/apt/lists" "$WORK/var/cache/apt" \
       "$WORK/usr/share/doc" "$WORK/usr/share/man" "$WORK/usr/share/locale" \
       "$WORK/root/.cache"
# ffmpeg is a Playwright video-capture helper; this worker never records.
find "$WORK/usr/local/lib/kastellan-browser-driver/browsers" -maxdepth 1 -name 'ffmpeg-*' -exec rm -rf {} + 2>/dev/null || true

# PID1. The worker itself is already staged by the image at
# /usr/local/bin/kastellan-worker-browser-driver (matches MICROVM_WORKER_BIN,
# slice 2). Fail closed if the image did not stage it.
install -D -m0755 "$REPO_ROOT/target/release/kastellan-microvm-init" "$WORK/sbin/init"
if [ ! -e "$WORK/usr/local/bin/kastellan-worker-browser-driver" ]; then
    echo "staging tree is missing /usr/local/bin/kastellan-worker-browser-driver" >&2
    exit 1
fi

# Pseudo-fs mountpoints + slice-3 host-dir-share anchors + slice-4a /run egress
# relay tmpfs mountpoint. Keep this anchor list in lockstep with
# mounts.rs::SHARE_ANCHORS (opt/data/srv/mnt/work/tmp) and the sibling scripts.
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"
mkdir -p "$WORK/run"

echo "staging size: $(du -sh "$WORK" | cut -f1) (image will be ${ROOTFS_MIB}M)"

# Journal-less ext4 (read-only at runtime, shared across concurrent VMs).
mkfs.ext4 -q -F -O ^has_journal -L browser-driver -d "$WORK" \
    "$OUT_DIR/browser-driver.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/browser-driver.ext4 (+ shared $OUT_DIR/vmlinux)"
```

- [ ] **Step 5: Make it executable and run it end to end**

```bash
ssh dgx 'cd ~/src/kastellan && chmod +x scripts/workers/microvm/build-browser-driver-rootfs.sh && \
  bash scripts/workers/microvm/build-browser-driver-rootfs.sh 2>&1 | tail -15'
```

Expected final line: `built /var/lib/kastellan/microvm/browser-driver.ext4 (+ shared .../vmlinux)`.

If `mkfs.ext4` fails with a space error, `ROOTFS_MIB` is too small — raise it to the reported requirement × 1.2 and rerun.

- [ ] **Step 6: Verify the image contents without booting**

```bash
ssh dgx 'debugfs -R "ls -l /usr/local/bin" /var/lib/kastellan/microvm/browser-driver.ext4 2>/dev/null | head; \
  debugfs -R "stat /sbin/init" /var/lib/kastellan/microvm/browser-driver.ext4 2>/dev/null | head -5; \
  ls -la /var/lib/kastellan/microvm/browser-driver.ext4'
```

Expected: `kastellan-worker-browser-driver` present under `/usr/local/bin`, `/sbin/init` a regular file, image size matching `ROOTFS_MIB`.

- [ ] **Step 7: Commit**

```bash
git add scripts/workers/microvm/Dockerfile.browser-driver \
        scripts/workers/microvm/build-browser-driver-rootfs.sh
git commit -m "feat(microvm): browser-driver rootfs via build-time Dockerfile + docker export

Chromium dlopen's NSS/fontconfig/SwiftShader, which the existing ldd-closure
rootfs pattern cannot discover. Stage via a build-time-only Dockerfile using
playwright install --with-deps, then export into the same mkfs.ext4 -d tail
every sibling script uses. Docker is build-time only; runtime is unchanged.

A build-time headless_shell smoke fails the image build if the dlopen closure
is incomplete, so that failure class surfaces loudly at build rather than as
an opaque render error during a VM boot."
```

---

## Task 3: Apply the Task 1 mount decision

**Skip this task entirely if Task 1 selected option D.** In that case tick every
box with a note "N/A — Task 1 selected option D (no init change needed)".

**Files:**
- Modify: `workers/microvm-init/src/guest.rs:176-197`
- Test: `workers/microvm-init/src/guest.rs` (inline `#[cfg(test)]` module) or the file's existing test module

**Interfaces:**
- Consumes: Task 1's option selection.
- Produces: a guest that mounts the pseudo-filesystems Chromium needs. Task 5's live boot depends on it.

- [ ] **Step 1: Write the failing test**

`mount_pseudo_fs` performs real syscalls, so the testable unit is the mount
**table**, not the mounting. Extract the table to a pure function first, then
test that. Add to `workers/microvm-init/src/guest.rs`:

```rust
/// The pseudo-filesystems PID1 mounts at boot, as pure data.
///
/// Split out from [`mount_pseudo_fs`] so the table is unit-testable without
/// performing real mounts: the syscall loop is untestable in-process, but a
/// silently-dropped entry is exactly the kind of regression worth pinning.
/// Each tuple is `(source, target, fstype)`.
pub(crate) fn pseudo_fs_table() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
        ("tmpfs", "/tmp", "tmpfs"),
        // Chromium needs POSIX shared memory. Playwright also passes
        // --disable-dev-shm-usage (which redirects to /tmp), but a Chromium
        // child touching /dev/shm directly would otherwise crash cryptically.
        ("tmpfs", "/dev/shm", "tmpfs"),
    ]
}
```

If Task 1 found devtmpfs is **not** auto-mounted, also add `("devtmpfs", "/dev", "devtmpfs")` **before** the `/dev/shm` entry — `/dev` must be mounted before something can be mounted beneath it.

Now the test:

```rust
#[test]
fn pseudo_fs_table_covers_dev_shm_for_chromium() {
    let table = pseudo_fs_table();
    let targets: Vec<&str> = table.iter().map(|(_, t, _)| *t).collect();

    // The pre-existing three must never be dropped.
    for required in ["/proc", "/sys", "/tmp"] {
        assert!(targets.contains(&required), "{required} missing from the pseudo-fs table");
    }
    // Chromium's shared memory (browser-driver micro-VM rootfs, slice 1).
    assert!(targets.contains(&"/dev/shm"), "/dev/shm missing from the pseudo-fs table");

    // /dev/shm can only be mounted after whatever provides /dev, so if /dev is
    // in the table at all it must come first.
    if let Some(dev_idx) = targets.iter().position(|t| *t == "/dev") {
        let shm_idx = targets.iter().position(|t| *t == "/dev/shm").unwrap();
        assert!(dev_idx < shm_idx, "/dev must be mounted before /dev/shm");
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && \
  cargo test -p kastellan-microvm-init pseudo_fs_table 2>&1 | tail -20'
```

Expected: FAIL — either `cannot find function pseudo_fs_table` (before Step 1's helper is added) or the `/dev/shm missing` assertion.

- [ ] **Step 3: Rewrite `mount_pseudo_fs` to consume the table**

```rust
pub(crate) fn mount_pseudo_fs() {
    for (src, target, fstype) in pseudo_fs_table() {
        // The mountpoint may not exist in the rootfs image (e.g. /dev/shm under
        // a freshly-mounted devtmpfs). Best-effort mkdir, then mount.
        let _ = std::fs::create_dir_all(target);
        let src = std::ffi::CString::new(*src).unwrap();
        let target = std::ffi::CString::new(*target).unwrap();
        let fstype = std::ffi::CString::new(*fstype).unwrap();
        // Ignore EBUSY (already mounted by the kernel or a prior call).
        unsafe {
            libc::mount(
                src.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                std::ptr::null(),
            );
        }
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && \
  cargo test -p kastellan-microvm-init 2>&1 | tail -20'
```

Expected: PASS, all tests in the crate green.

- [ ] **Step 5: Rebuild every rootfs — the init is baked into all of them**

This change affects all existing VM workers, which is precisely why Task 1
weighed the blast radius. Every image carries its own copy of `/sbin/init`.

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && \
  cargo build --release -p kastellan-microvm-init && \
  for s in build-rootfs build-web-fetch-rootfs build-web-research-rootfs \
           build-web-search-rootfs build-net-demo-rootfs build-matrix-rootfs; do \
    echo "== $s =="; bash scripts/workers/microvm/$s.sh 2>&1 | tail -2; done && \
  bash scripts/workers/kv-demo/build-kv-demo-rootfs.sh 2>&1 | tail -2'
```

Expected: a `built ...` line per script.

- [ ] **Step 6: Prove no existing VM worker regressed**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && export PATH=$HOME/.local/bin:$PATH && \
  setsid bash -lc "cargo test -p kastellan-core --test web_fetch_firecracker_egress_e2e \
    --test kv_demo_firecracker_persistent_e2e --test net_demo_firecracker_egress_e2e \
    -- --ignored --nocapture > ~/task3-vm.log 2>&1; echo DONE_EXIT=\$? >> ~/task3-vm.log" </dev/null & \
  sleep 5; echo started'
```

Poll with `ssh dgx 'tail -5 ~/task3-vm.log'` until `DONE_EXIT=` appears. Expected: `DONE_EXIT=0`, no new failures, and no unexpected `[SKIP]` lines.

- [ ] **Step 7: Commit**

```bash
git add workers/microvm-init/src/guest.rs
git commit -m "feat(microvm-init): mount /dev/shm for Chromium; extract pseudo-fs table

Extracts the pseudo-filesystem list from mount_pseudo_fs into a pure
pseudo_fs_table() so a silently-dropped entry is unit-testable without
performing real mounts. Adds /dev/shm, which Chromium needs; Playwright also
passes --disable-dev-shm-usage but a child touching /dev/shm directly would
crash cryptically.

Affects every VM worker (each image bakes its own /sbin/init), so all rootfs
images were rebuilt and the web-fetch / kv-demo / net-demo VM e2es re-run green."
```

---

## Task 4: Hermetic launch-plan pin

**Files:**
- Create: `core/tests/browser_driver_firecracker_e2e.rs`

**Interfaces:**
- Consumes: the in-rootfs path constant `/usr/local/bin/kastellan-worker-browser-driver` (staged by Task 2).
- Produces: `hex_decode` (a local test helper) reused by nothing else; the test file that Task 5 extends.

Note: `kastellan_sandbox::linux_firecracker` is `#[cfg(target_os = "linux")]`
(`sandbox/src/lib.rs:11-16`), so this pin runs **on Linux without KVM** — not on
the Mac. The spec's §7.1 claim of "Mac and Linux" is wrong and Task 6 corrects it.

- [ ] **Step 1: Write the failing test**

Create `core/tests/browser_driver_firecracker_e2e.rs`:

```rust
#![cfg(target_os = "linux")]
//! browser-driver × Firecracker micro-VM (slice 1: the rootfs).
//!
//! ## Tiers
//!
//! * `vm_policy_flows_through_plan_to_in_rootfs_guest_path` — hermetic, always
//!   runs on Linux (no KVM, no network). Feeds a browser-driver VM policy
//!   through the REAL `build_launch_plan` and pins that the guest execs the
//!   IN-ROOTFS worker path, not a host `target/` path. That failure mode is a
//!   guest PID1 panic -> boot loop -> dispatch hanging to wall-clock, which
//!   presents as a channel hang and has cost a debugging session before
//!   (memory: vm-worker-in-rootfs-binary-path). It also pins the cmdline
//!   budget, since env is hex-encoded and costs two bytes per byte.
//!
//! * The live boot/render tier is added in Task 5.
//!
//! Note `linux_firecracker` is Linux-gated, so none of this compiles on macOS.

use std::path::PathBuf;

use kastellan_sandbox::linux_firecracker::{build_launch_plan, FirecrackerImage};
use kastellan_sandbox::{Net, Profile, SandboxPolicy};

/// The worker path baked into the rootfs by
/// `scripts/workers/microvm/build-browser-driver-rootfs.sh`. Slice 2's
/// `MICROVM_WORKER_BIN` const must match this exactly.
const IN_ROOTFS_WORKER: &str = "/usr/local/bin/kastellan-worker-browser-driver";

/// Decode the lowercase-hex cmdline tokens `microvm-init` consumes.
///
/// `plan.rs::hex_encode` is `pub(super)`, so tests cannot call its inverse;
/// this is the minimal decoder needed to read a token back.
fn hex_decode(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "hex token has odd length: {s}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex byte"))
        .collect()
}

/// Build the VM policy slice 2's `browser_driver_firecracker_entry` will
/// produce. Constructed inline here because that production entry does not
/// exist yet — slice 1 is the rootfs only. Mirrors the shape of
/// `web_fetch_firecracker_entry` (empty fs_read, force-routed, VM backend).
fn browser_driver_vm_policy() -> SandboxPolicy {
    SandboxPolicy {
        // Empty: a VM shares no host paths in. The per-instance CA is appended
        // at spawn, and browser-driver runs no-MITM anyway.
        fs_read: vec![],
        fs_write: vec![],
        // Net::Allowlist WITH proxy_uds == force-routed. Without proxy_uds
        // build_launch_plan rejects it fail-closed (no virtio-net device).
        net: Net::Allowlist(vec!["example.org:443".to_string()]),
        cpu_ms: 30_000,
        // Chromium plus a RAM-backed /tmp tmpfs; see spec S6.
        mem_mb: 2048,
        profile: Profile::WorkerBrowserClient,
        cpu_quota_pct: None,
        tasks_max: Some(512),
        env: vec![
            (
                "KASTELLAN_BROWSER_DRIVER_ALLOWLIST".to_string(),
                r#"["example.org"]"#.to_string(),
            ),
            (
                "PLAYWRIGHT_BROWSERS_PATH".to_string(),
                "/usr/local/lib/kastellan-browser-driver/browsers".to_string(),
            ),
            ("TMPDIR".to_string(), "/tmp".to_string()),
            // Playwright's Node driver calls uv_os_homedir(); without HOME it
            // dies with "Connection closed while reading from the driver".
            ("HOME".to_string(), "/tmp".to_string()),
        ],
        proxy_uds: Some(PathBuf::from("/tmp/kastellan-egress.sock")),
        broker_uds: None,
        persistent_store: None,
    }
}

#[test]
fn vm_policy_flows_through_plan_to_in_rootfs_guest_path() {
    let image = FirecrackerImage {
        kernel_path: PathBuf::from("/var/lib/kastellan/microvm/vmlinux"),
        rootfs_path: PathBuf::from("/var/lib/kastellan/microvm/browser-driver.ext4"),
    };
    let policy = browser_driver_vm_policy();

    let plan = build_launch_plan(&policy, &image, IN_ROOTFS_WORKER, &[])
        .expect("browser-driver VM policy must produce a launch plan");

    // The guest execs the in-rootfs path, NOT a host target/ path.
    let token = plan
        .boot_args
        .split_whitespace()
        .find_map(|t| t.strip_prefix("kastellan.worker="))
        .expect("boot args carry a kastellan.worker= token");
    let decoded = String::from_utf8(hex_decode(token)).expect("worker token is utf8");
    assert_eq!(
        decoded, IN_ROOTFS_WORKER,
        "guest must exec the in-rootfs worker path; a host target/ path ENOENTs \
         inside the guest and boot-loops (memory: vm-worker-in-rootfs-binary-path)"
    );

    // Force-routed => no virtio-net device at all.
    assert!(
        !plan.net_enabled,
        "a force-routed VM worker must carry no NIC"
    );

    // Env is hex-encoded (two cmdline bytes per env byte), so the budget is the
    // real constraint on this entry. build_launch_plan fails closed above
    // MAX_CMDLINE_BYTES (1920, plan.rs:137); assert real headroom remains so a
    // slightly longer allowlist in production does not tip it over.
    assert!(
        plan.boot_args.len() < 1920,
        "cmdline {} bytes must stay under the 1920-byte cap",
        plan.boot_args.len()
    );
    assert!(
        plan.boot_args.len() < 1536,
        "cmdline {} bytes leaves too little headroom for a production-sized allowlist",
        plan.boot_args.len()
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && \
  cargo test -p kastellan-core --test browser_driver_firecracker_e2e 2>&1 | tail -25'
```

Expected: this test is written against existing production code, so it may pass
immediately. **That is acceptable here** — it is a characterization/regression
pin for a footgun, not a driver for new code. What must be verified is that it
*can* fail: temporarily change `IN_ROOTFS_WORKER` to
`/home/hherb/src/kastellan/target/debug/kastellan-worker-browser-driver`, re-run,
confirm the assertion fires with the boot-loop message, then change it back.

- [ ] **Step 3: Confirm the negative case, then restore**

```bash
# after temporarily editing IN_ROOTFS_WORKER as described above
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && \
  cargo test -p kastellan-core --test browser_driver_firecracker_e2e 2>&1 | grep -A3 "in-rootfs"'
```

Expected: the assertion message about boot-looping. Restore the constant afterwards.

- [ ] **Step 4: Run the test and clippy clean**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && \
  cargo test -p kastellan-core --test browser_driver_firecracker_e2e 2>&1 | tail -10 && \
  cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -10'
```

Expected: 1 passed; clippy clean. The DGX core-clippy gate is authoritative for
cfg-linux code — Mac clippy compiles this file out entirely (memory
`cfg-linux-e2e-deadcode-dgx-clippy`).

- [ ] **Step 5: Commit**

```bash
git add core/tests/browser_driver_firecracker_e2e.rs
git commit -m "test(browser-driver): hermetic VM launch-plan pin

Feeds a browser-driver VM policy through the real build_launch_plan and pins
that the guest execs the in-rootfs worker path rather than a host target/
path. That failure mode is a PID1 ENOENT -> boot loop -> dispatch hanging to
wall-clock, which presents as a channel hang. Also pins no-NIC force-routing
and cmdline-budget headroom (env is hex-encoded, two bytes per byte)."
```

---

## Task 5: DGX live boot + render spike

**Files:**
- Modify: `core/tests/browser_driver_firecracker_e2e.rs`

**Interfaces:**
- Consumes: `browser-driver.ext4` (Task 2), the mount fix (Task 3 if applicable), `browser_driver_vm_policy()` and `IN_ROOTFS_WORKER` (Task 4).
- Produces: the slice's acceptance evidence.

- [ ] **Step 1: Write the live test**

Append to `core/tests/browser_driver_firecracker_e2e.rs`. The stub proxy is the
key trick: it accepts the CONNECT and closes, so Chromium reports a
**navigation-class** `net::ERR_*`. Reaching that error at all proves Chromium
launched — which is the property this slice exists to establish — without
needing a real egress sidecar (that is slice 3).

```rust
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::sync::Arc;

use kastellan_core::tool_host::{dispatch, WorkerSpec};
use kastellan_core::secrets::Vault;
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};
use kastellan_sandbox::linux_firecracker::LinuxFirecracker;
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix,
};

/// Live tier: boot browser-driver.ext4 and prove Chromium launches inside it.
///
/// Acceptance reasoning (spec S7.2): a browser-LAUNCH failure and a
/// NAVIGATION failure both surface as RENDER_FAILED (-32003), so the JSON-RPC
/// code cannot discriminate them — the message must. A stub proxy that accepts
/// then closes guarantees the navigation fails, so:
///
///   * a `net::ERR_*` message  => Chromium started and tried to navigate. PASS.
///   * a missing-executable or missing-shared-library message => the rootfs is
///     incomplete. FAIL, and that is exactly what this slice must catch.
///
/// The complementary, deterministic launch proof lives at Docker BUILD time:
/// Dockerfile.browser-driver runs headless_shell --dump-dom about:blank and
/// fails the build if the dlopen closure is incomplete.
#[test]
#[ignore = "DGX-only: real KVM + vsock + browser-driver rootfs (build-browser-driver-rootfs.sh)"]
fn vm_booted_browser_driver_launches_chromium() {
    skip_if_no_supervisor();
    skip_if_sandbox_unavailable();
    let Some(pg_bin) = pg_bin_dir_or_skip() else { return };

    let image = FirecrackerImage {
        kernel_path: PathBuf::from("/var/lib/kastellan/microvm/vmlinux"),
        rootfs_path: PathBuf::from("/var/lib/kastellan/microvm/browser-driver.ext4"),
    };
    if LinuxFirecracker::probe(&image).is_err() {
        eprintln!("[SKIP] firecracker/KVM/vsock or browser-driver.ext4 unavailable");
        return;
    }

    // Stub egress proxy: accept the CONNECT, read a little, drop. Chromium then
    // reports a proxy/connection net::ERR_*, which is our navigation-class signal.
    let suffix = unique_suffix();
    let uds_path = PathBuf::from(format!("/tmp/kastellan-bd-stub-{suffix}.sock"));
    let listener = UnixListener::bind(&uds_path).expect("bind stub proxy UDS");
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_w = Arc::clone(&seen);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut c) = conn else { continue };
            let mut buf = [0u8; 256];
            if let Ok(n) = c.read(&mut buf) {
                seen_w
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
            }
            drop(c); // close -> Chromium sees a proxy connection failure
        }
    });

    let mut policy = browser_driver_vm_policy();
    policy.proxy_uds = Some(uds_path.clone());
    // The guest reaches the proxy over the vsock relay; build_launch_plan
    // rewrites this env to the in-guest path (/run/kastellan-egress.sock).
    policy.env.push((
        "KASTELLAN_EGRESS_PROXY_UDS".to_string(),
        uds_path.display().to_string(),
    ));

    let _pg = bring_up_pg_cluster(&pg_bin, &format!("bd-vm-{suffix}"))
        .expect("bring up PG for the audit sink");

    let spec = WorkerSpec {
        program: PathBuf::from(IN_ROOTFS_WORKER),
        policy,
        backend: SandboxBackendKind::FirecrackerVm,
        ..Default::default()
    };

    let result = dispatch(
        &SandboxBackends::default(),
        &spec,
        "browser.render",
        serde_json::json!({ "url": "https://example.org/", "timeout_ms": 20000 }),
        &Vault::default(),
    );

    // Whatever happened, it must be a WELL-FORMED JSON-RPC reply over vsock:
    // that alone proves boot + worker + stdio bridge.
    let payload = format!("{result:?}");
    assert!(
        payload.contains("result") || payload.contains("error"),
        "expected a JSON-RPC reply from the VM-booted worker, got: {payload}"
    );

    // Chromium-launch discrimination (spec S7.2). Launch-class strings mean the
    // rootfs is incomplete.
    for launch_failure in [
        "Executable doesn't exist",
        "error while loading shared libraries",
        "cannot open shared object file",
    ] {
        assert!(
            !payload.contains(launch_failure),
            "Chromium failed to LAUNCH inside the VM ({launch_failure}) — the rootfs \
             dlopen/lib closure is incomplete: {payload}"
        );
    }

    eprintln!("stub proxy saw {} connection(s): {:?}", seen.lock().unwrap().len(), seen.lock().unwrap());
    let _ = std::fs::remove_file(&uds_path);
}
```

**Signatures: verify before writing.** `WorkerSpec`'s field set, whether it
implements `Default`, and `dispatch`'s exact parameter list are all assumed
above and may not match. `core/tests/web_fetch_firecracker_egress_e2e.rs`
performs the same spawn against the same backend and is the authoritative local
example — read its spawn block first and mirror it exactly, adjusting only the
policy, the program path, the method name, and the params. Do not fight the
compiler against the sketch above; the sketch shows intent, that file shows the
API.

- [ ] **Step 2: Build the release launcher, then run it**

`locate_microvm_run()` prefers `target/release`; a stale binary silently runs OLD
launcher code (memory `firecracker-e2e-stale-release-launcher`). And
`firecracker` is off the non-interactive SSH PATH — without it the test
SKIP-as-passes and proves nothing.

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && export PATH=$HOME/.local/bin:$PATH && \
  cargo build --release -p kastellan-microvm-run && \
  setsid bash -lc "cargo test -p kastellan-core --test browser_driver_firecracker_e2e \
    -- --ignored --nocapture > ~/task5.log 2>&1; echo DONE_EXIT=\$? >> ~/task5.log" </dev/null & \
  sleep 5; echo started'
```

Poll with `ssh dgx 'tail -30 ~/task5.log'` until `DONE_EXIT=` appears.

Expected: `DONE_EXIT=0`, and **no `[SKIP]` line** — a `[SKIP]` here means the
test proved nothing.

- [ ] **Step 3: Diagnose a launch failure, if one occurs**

If the launch-class assertion fires, the missing library is named in the
message. Add its apt package to `Dockerfile.browser-driver` Step 1's
`apt-get install` list with a comment saying which Chromium subsystem needs it,
then re-run Task 2 Steps 2–5 and this task's Step 2.

If instead the test hangs to wall-clock with no reply, suspect the guest exec
path: re-read Task 4's pin and confirm the rootfs really staged
`/usr/local/bin/kastellan-worker-browser-driver` (Task 2 Step 6).

- [ ] **Step 4: Commit**

```bash
git add core/tests/browser_driver_firecracker_e2e.rs
git commit -m "test(browser-driver): DGX live VM boot + Chromium launch spike

Boots browser-driver.ext4 and dispatches one browser.render through a stub
egress proxy that accepts then closes. Because a launch failure and a
navigation failure both surface as RENDER_FAILED (-32003), the assertion
discriminates on message: reaching a net::ERR_* proves Chromium started, while
a missing-executable/shared-library string fails the test."
```

---

## Task 6: Spec corrections, docs close-out, PR

**Files:**
- Modify: `docs/superpowers/specs/2026-07-19-browser-driver-microvm-rootfs-design.md`
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

**Interfaces:**
- Consumes: every prior task's outcome (measured `ROOTFS_MIB`, the Task 1 option, the test counts).
- Produces: the merged slice.

- [ ] **Step 1: Correct the two spec inaccuracies found during implementation**

In §7.1, the claim that the hermetic pin "always runs, Mac and Linux" is wrong:
`kastellan_sandbox::linux_firecracker` is `#[cfg(target_os = "linux")]`
(`sandbox/src/lib.rs:11-16`), so the pin is compiled out on macOS. Change it to
"always runs on Linux (no KVM needed); compiled out on macOS".

In §7.2, add that the deterministic launch proof landed at **Docker build time**
(`Dockerfile.browser-driver` runs `headless_shell --dump-dom about:blank`)
rather than as a separate in-guest smoke — the guest execs exactly one program,
so a second in-guest binary would have needed console plumbing for no extra
signal.

- [ ] **Step 2: Run the full DGX gate**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && export PATH=$HOME/.local/bin:$PATH && \
  setsid bash -lc "cargo build --workspace && \
    cargo test --workspace -- --nocapture > ~/task6-gate.log 2>&1; \
    cargo clippy --workspace --all-targets -- -D warnings >> ~/task6-gate.log 2>&1; \
    echo DONE_EXIT=\$? >> ~/task6-gate.log" </dev/null & \
  sleep 5; echo started'
```

Poll until `DONE_EXIT=`. Then extract the counts and the skip lines:

```bash
ssh dgx 'grep -E "^test result:" ~/task6-gate.log | \
  awk "{p+=\$4; f+=\$6; i+=\$8} END {print \"passed=\"p, \"failed=\"f, \"ignored=\"i}"; \
  echo "--- SKIP lines ---"; grep -c "\[SKIP\]" ~/task6-gate.log; \
  grep "\[SKIP\]" ~/task6-gate.log | sort -u'
```

Expected: `failed=0`; passed = **2584 + the new hermetic pin** (2585) if Task 3
was skipped as option D, or **2586** if Task 3 added its table test. Clippy
clean. `[SKIP]` lines should be only the 4 known opt-in `KASTELLAN_GLINER_RELEX_ENABLE`
gated e2e — any containment skip (bwrap/userns/PG) is a false green.

Record the exact numbers; they become the new baseline.

- [ ] **Step 3: Update HANDOVER.md**

Replace the "IN PROGRESS" bullet added earlier with a completion entry stating:
slice 1 shipped; the rootfs recipe and why Docker is build-time-only; the
measured `ROOTFS_MIB`; the Task 1 `/dev` finding and which option was taken; the
new DGX baseline; and that slices 2 (VM entry + `resolve()` branch, which must
short-circuit the Linux lockdown-shim `Misconfigured` arm at
`browser_driver.rs:399-402`) and 3 (live render through a real sidecar) remain.

Update the "Last updated", "Current state", and toolchain-baseline lines to the
Step-2 numbers. Keep HANDOVER under 500 lines — prune the oldest merged-session
paragraph if needed.

- [ ] **Step 4: Update ROADMAP.md**

Add a terse one-line `[x]` entry in the micro-VM phase with the PR number, per
the file's own "How to update" instruction: condense to one line, note that
pure refactors are not recorded there.

- [ ] **Step 5: Commit and open the PR**

```bash
git add docs/
git commit -m "docs: browser-driver micro-VM rootfs slice 1 close-out"
git push -u origin feat/browser-driver-microvm-rootfs
gh pr create --base main --title "browser-driver micro-VM rootfs (slice 1 of the VM-entry arc)" --body "$(cat <<'EOF'
## What

Slice 1 of giving browser-driver a Firecracker micro-VM mode: the rootfs.
Produces `browser-driver.ext4` containing Chromium, Playwright, a Python venv,
and the existing unmodified worker.

## Why a Dockerfile

Chromium `dlopen`s NSS modules, fontconfig backends and SwiftShader at runtime.
The existing rootfs pattern discovers libraries with `ldd`, which cannot see any
of them. `playwright install --with-deps chromium` resolves that closure
instead. Docker is **build-time only** — the runtime remains pure Firecracker
with no new dependency, and the staging tree flows through the same
`mkfs.ext4 -d` tail every sibling script uses.

## Verification

- DGX `cargo test --workspace`: <counts>, clippy `-D warnings` clean
- Live DGX boot: a VM-booted browser-driver launches Chromium and reaches a
  navigation-class `net::ERR_*` through a stub proxy
- Build-time smoke: the image build fails if the dlopen closure is incomplete

## Notes for reviewers

- The spec corrects an earlier HANDOVER claim that this fixes macOS #286. It
  does not — Firecracker is Linux-only; #286's named fix is the `MacosContainer`
  VM-netns backend (#55).
- <If Task 3 applied: the microvm-init mount change affects every VM worker;
  all rootfs images were rebuilt and the web-fetch / kv-demo / net-demo VM e2es
  re-run green.>

Spec: `docs/superpowers/specs/2026-07-19-browser-driver-microvm-rootfs-design.md`
Plan: `docs/superpowers/plans/2026-07-19-browser-driver-microvm-rootfs.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Notes for the implementer

- **Run cargo in the FOREGROUND** for short commands. For anything long
  (`--workspace`, rootfs rebuild loops), use the `setsid … > ~/log 2>&1 &` +
  poll-for-`DONE_EXIT` pattern shown above. Never pipe a background cargo run
  through `| tail` — it masks the exit code and buffers output.
- **Never `git add -A`.** Stage the specific files each step names. Untracked
  `docs/essay-medium-draft.md` and `.claude/scheduled_tasks.lock` must stay out.
- **Iterate on the DGX, not the Mac.** The IDE's rust-analyzer holds
  `target/debug/.cargo-lock`.
- **A green run with `[SKIP]` lines is not a green run** for anything
  containment-related. Always read `--nocapture` output.
