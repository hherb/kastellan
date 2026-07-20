# Guest-kernel verification at VM boot — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the micro-VM guest kernel unforgeable between builds — sha256-verified on every VM boot, and owned by root in a sticky image directory so the agent's own OS user cannot replace it.

**Architecture:** Two independent halves. (1) A new dual-platform `sandbox/src/guest_kernel_pin.rs` splits a pure arch→sum table from a single IO function that hashes the kernel; `LinuxFirecracker::spawn_under_policy` calls it right after `resolve_image` and fails closed. (2) `install-firecracker-vsock.sh` sets mode `1755` on `/var/lib/kastellan/microvm/` and installs `vmlinux` as `root:root 0644`, so the write primitive is gone rather than merely detected.

**Tech Stack:** Rust (workspace `sha2 = "0.10"`, MIT/Apache-2.0), bash provisioning scripts, existing `kastellan-tests-common` test harness.

**Spec:** `docs/superpowers/specs/2026-07-20-microvm-guest-kernel-boot-verification-design.md`
**Issue:** [#479](https://github.com/hherb/kastellan/issues/479)
**Branch:** `fix/479-verify-guest-kernel-at-boot` (already exists, spec committed as `2028c6d6`)

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible dependencies only.** `sha2` is MIT OR Apache-2.0 — fine. Add no other dependency.
- **Cross-platform: Linux + macOS first-class.** `guest_kernel_pin.rs` must **not** be `#[cfg(target_os = "linux")]`-gated. Its tests must run on the dev Mac. Only the call site inside `linux_firecracker.rs` is Linux-gated.
- **No bypass escape hatch.** Do not add any environment variable that skips verification. Fail closed on mismatch, unreadable file, or unknown architecture.
- **Cargo is not on the default non-interactive `PATH`:** every cargo command must be preceded by `source "$HOME/.cargo/env"`.
- **Run cargo in the FOREGROUND.** Never background a `cargo test`/`clippy` and poll it.
- **Commit specific files** — `git add <paths>`, never `git add -A` (untracked `docs/essay-medium-draft.md` and `.claude/scheduled_tasks.lock` must stay out of history).
- **Files under 500 lines** where feasible.
- **Inline documentation aimed at a junior contributor is mandatory**, including *why*, not just *what*.
- The two pinned sums, copied verbatim from `scripts/workers/microvm/lib/guest-kernel.sh`:
  - x86_64: `49ba99a5299444ac59dda2efc3569cc2d58a5d72ea6475a6bfc37aa0bf322e54`
  - aarch64: `bb1f50912d63a8ca5e92d488984875e1177eb9283050ffa592a8cb455cada52d`

## File Structure

| File | Responsibility |
|---|---|
| `sandbox/src/guest_kernel_pin.rs` (**create**, ~200 lines incl. tests) | The pin: arch→sum table, `verify_kernel`, `verify_pinned_kernel`, `KernelPinError`. Dual-platform. Owns its own unit tests. |
| `sandbox/src/lib.rs` (**modify**, line 12 area) | Declare `pub mod guest_kernel_pin;` ungated. |
| `sandbox/Cargo.toml` (**modify**) | Add `sha2 = { workspace = true }`. |
| `sandbox/src/linux_firecracker.rs` (**modify**, `spawn_under_policy` ~line 182) | Call the check after `resolve_image`; two new `cfg(linux)` tests. |
| `tests-common/src/microvm.rs` (**modify**, tests module) | Cross-check test: Rust consts vs `guest-kernel.sh`; install-script property test. |
| `scripts/linux/install-firecracker-vsock.sh` (**modify**, section 4 ~line 92) | Sticky dir + root-owned kernel install. |

**Not modified:** the eight `build-*-rootfs.sh`. See Task 4's note — `fetch_guest_kernel` already behaves correctly against a root-owned kernel.

---

### Task 1: The pin module (pure table + hashing), dual-platform

**Files:**
- Create: `sandbox/src/guest_kernel_pin.rs`
- Modify: `sandbox/src/lib.rs` (add module declaration next to line 12)
- Modify: `sandbox/Cargo.toml` (add `sha2`)

**Interfaces:**
- Consumes: nothing (first task).
- Produces, relied on by Tasks 2 and 3:
  - `pub const GUEST_KERNEL_SHA256_X86_64: &str`
  - `pub const GUEST_KERNEL_SHA256_AARCH64: &str`
  - `pub fn expected_sha256(arch: &str) -> Option<&'static str>`
  - `pub fn verify_kernel(path: &Path, expected: &str) -> Result<(), KernelPinError>`
  - `pub fn verify_pinned_kernel(path: &Path, arch: &str) -> Result<(), KernelPinError>`
  - `pub enum KernelPinError` implementing `std::fmt::Display` + `std::error::Error`

- [ ] **Step 1: Write the failing tests**

Create `sandbox/src/guest_kernel_pin.rs` containing **only** the test module for now (the implementation lands in Step 3, so the file must not yet declare the functions):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// sha256 of the 5 bytes `hello`, from the standard test vectors.
    /// Lets the accept/reject paths be exercised against a 5-byte file
    /// instead of a 16 MB kernel, so these stay unit tests. Same constant
    /// the bash-side tests in `tests-common` use, deliberately.
    const HELLO_SHA256: &str =
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    /// A unique temp dir, using `std` only — `kastellan-sandbox` has no
    /// `tempfile` dependency and this module must not add one.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("kastellan-kernelpin-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).expect("create test file");
        f.write_all(bytes).expect("write test file");
        path
    }

    #[test]
    fn expected_sha256_knows_both_published_arches() {
        assert_eq!(expected_sha256("x86_64"), Some(GUEST_KERNEL_SHA256_X86_64));
        assert_eq!(expected_sha256("aarch64"), Some(GUEST_KERNEL_SHA256_AARCH64));
    }

    /// Fail closed on anything else. Returning `None` rather than a
    /// default is the whole point: an unrecognised architecture must
    /// never degrade into an unverified boot.
    #[test]
    fn expected_sha256_refuses_unknown_arch() {
        assert_eq!(expected_sha256("riscv64"), None);
        assert_eq!(expected_sha256(""), None);
        assert_eq!(expected_sha256("arm64"), None, "macOS spelling is not a Linux guest arch");
    }

    #[test]
    fn verify_kernel_accepts_matching_content() {
        let dir = temp_dir("accept");
        let path = write_file(&dir, "vmlinux", b"hello");
        assert!(verify_kernel(&path, HELLO_SHA256).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_kernel_rejects_altered_content() {
        let dir = temp_dir("reject");
        let path = write_file(&dir, "vmlinux", b"hellp");
        let err = verify_kernel(&path, HELLO_SHA256).expect_err("altered bytes must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("does not match the pinned sha256"), "unhelpful message: {msg}");
        // Both sums must be printed, so an operator can tell a truncated
        // file from a different artefact without re-running anything.
        assert!(msg.contains(HELLO_SHA256), "expected sum missing from: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_kernel_rejects_a_missing_file() {
        let dir = temp_dir("missing");
        let err = verify_kernel(&dir.join("vmlinux"), HELLO_SHA256)
            .expect_err("a missing kernel must fail, never pass");
        assert!(err.to_string().contains("cannot read"), "unhelpful message: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_pinned_kernel_refuses_unknown_arch_without_reading_the_file() {
        let dir = temp_dir("unknown-arch");
        let path = write_file(&dir, "vmlinux", b"hello");
        let err = verify_pinned_kernel(&path, "riscv64")
            .expect_err("an unrecorded arch must fail closed");
        assert!(err.to_string().contains("no recorded guest-kernel sha256"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Chunked hashing must give the same answer as a single read. Pins
    /// the streaming loop against an off-by-one in the buffer handling —
    /// the file is 40 MB in production and cannot be slurped in tests.
    #[test]
    fn verify_kernel_hashes_content_larger_than_its_read_buffer() {
        let dir = temp_dir("large");
        let bytes = vec![0xABu8; 200_000];
        let path = write_file(&dir, "vmlinux", &bytes);
        let expected = sha256_hex_of_slice(&bytes);
        assert!(verify_kernel(&path, &expected).is_ok());
        assert!(verify_kernel(&path, HELLO_SHA256).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test-local reference implementation, deliberately not the one
    /// under test: hashing the whole slice at once is what the streaming
    /// loop is being checked against.
    fn sha256_hex_of_slice(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-sandbox guest_kernel_pin 2>&1 | tail -20
```

Expected: compile error — `cannot find function `expected_sha256` in this scope` (and similar for `verify_kernel`, `verify_pinned_kernel`, `GUEST_KERNEL_SHA256_*`). The module is not yet declared in `lib.rs`, so it may report "file not found for module" first; that is also an acceptable red.

- [ ] **Step 3: Add the dependency and declare the module**

In `sandbox/Cargo.toml`, in the `[dependencies]` block after `libc`, add:

```toml
# sha2: #479 — verify the pinned micro-VM guest kernel at VM boot, not
# just at rootfs-build time. MIT OR Apache-2.0, AGPL-compatible,
# pure-Rust with no system deps. Already a workspace dependency (the
# audit-log payload fingerprint uses it).
sha2       = { workspace = true }
```

In `sandbox/src/lib.rs`, immediately **above** line 12's `pub mod linux_bwrap;` (and above its `#[cfg(target_os = "linux")]` attribute), add:

```rust
// Deliberately NOT cfg(linux)-gated even though its only caller is the
// Linux Firecracker backend. Everything here — the arch table, hashing a
// file, the verdict — needs no KVM and no Linux, and a fail-closed check
// exercised on only one host is half-verified (issue #471's own lesson).
pub mod guest_kernel_pin;
```

- [ ] **Step 4: Write the implementation**

Prepend to `sandbox/src/guest_kernel_pin.rs`, **above** the existing `#[cfg(test)] mod tests`:

```rust
//! The pinned micro-VM guest kernel, verified at VM boot (issue #479).
//!
//! # Why this exists
//!
//! Issue #471 made every `build-*-rootfs.sh` sha256-verify the guest
//! `vmlinux` it fetches, including a copy already on disk. That closed
//! the download-time gap. It did **not** constrain the file afterwards:
//! the kernel was verified during *some* build, then booted — possibly
//! months later, hundreds of times — with nothing re-checking it.
//!
//! That window matters because `/var/lib/kastellan/microvm/` is reachable
//! by the agent's own OS user, which is exactly what
//! `docs/threat-model.md` assumes a worst-case compromise reaches. The
//! micro-VM *is* the containment boundary and the guest kernel is what
//! enforces it, so a kernel an attacker chose is close to the worst
//! possible file to boot on trust.
//!
//! # Honest limitation: this check is TOCTOU
//!
//! We hash the file, then Firecracker opens it separately a moment later.
//! An attacker who can write it *between* those two events still wins.
//! What this buys is shrinking the exposure from months and hundreds of
//! boots down to microseconds — real, but not a closed hole.
//!
//! What actually closes it is the other half of #479, in
//! `scripts/linux/install-firecracker-vsock.sh`: the image dir is sticky
//! (`1755`) and `vmlinux` is `root:root`, so the agent user has no write
//! primitive on it at all. The two halves are complementary and neither
//! is sufficient alone — this one is what still holds when ownership was
//! never applied (a pre-existing install, or an operator pointing
//! `KASTELLAN_MICROVM_DIR` at a directory they control).
//!
//! Closing the TOCTOU properly would mean hashing through an already-open
//! fd and handing that same fd to Firecracker, which its config-file
//! interface does not accommodate. Not attempted.
//!
//! # Keeping the sums in step with bash
//!
//! `scripts/workers/microvm/lib/guest-kernel.sh` holds the same two sums
//! for the build-time check, and stays the human-facing place an operator
//! edits on a kernel version bump. The duplication is deliberate — the
//! alternatives were rejected in the design doc (`kastellan-sandbox` is
//! published to crates.io, so it cannot `include_str!` a file outside its
//! own directory) — and it is **CI-enforced**, not hoped-for:
//! `kastellan-tests-common`'s `rust_and_bash_kernel_pins_agree` fails the
//! PR if the two ever drift.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// sha256 of `vmlinux-6.1.102` for x86_64.
///
/// Must equal `KASTELLAN_GUEST_KERNEL_SHA256_X86_64` in
/// `scripts/workers/microvm/lib/guest-kernel.sh`. Bump both together, in
/// the same deliberate step as the kernel version — and never "fix" a
/// mismatch by pasting in whatever a failure printed.
pub const GUEST_KERNEL_SHA256_X86_64: &str =
    "49ba99a5299444ac59dda2efc3569cc2d58a5d72ea6475a6bfc37aa0bf322e54";

/// sha256 of `vmlinux-6.1.102` for aarch64. See
/// [`GUEST_KERNEL_SHA256_X86_64`] for the bump rule.
pub const GUEST_KERNEL_SHA256_AARCH64: &str =
    "bb1f50912d63a8ca5e92d488984875e1177eb9283050ffa592a8cb455cada52d";

/// Read this many bytes at a time while hashing.
///
/// The kernel is ~16 MB (aarch64) / ~40 MB (x86_64); streaming it keeps
/// peak memory flat instead of scaling with the file.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// Why a guest kernel was refused. Every variant is fatal — there is no
/// "warn and continue" here, by design.
#[derive(Debug)]
pub enum KernelPinError {
    /// No recorded sum for this architecture. Fails rather than
    /// defaulting, so an unexpected host can never boot unverified.
    UnknownArch(String),
    /// The kernel could not be read at all (missing, unreadable).
    Unreadable(PathBuf, std::io::Error),
    /// The kernel exists but is not the pinned one.
    Mismatch { path: PathBuf, expected: String, actual: String },
}

impl fmt::Display for KernelPinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArch(arch) => write!(
                f,
                "no recorded guest-kernel sha256 for architecture '{arch}'; \
                 refusing to boot a micro-VM on an unverified kernel"
            ),
            Self::Unreadable(path, e) => write!(
                f,
                "cannot read the micro-VM guest kernel at {}: {e} \
                 (build it with scripts/workers/microvm/build-rootfs.sh, or run \
                 sudo scripts/linux/install-firecracker-vsock.sh)",
                path.display()
            ),
            Self::Mismatch { path, expected, actual } => write!(
                f,
                "the micro-VM guest kernel at {} does not match the pinned sha256 — \
                 refusing to boot it.\n  expected: {expected}\n  actual:   {actual}\n\
                 If the kernel version was intentionally changed, update \
                 GUEST_KERNEL_SHA256_* in sandbox/src/guest_kernel_pin.rs AND \
                 scripts/workers/microvm/lib/guest-kernel.sh together. If it was not, \
                 this file has been replaced since it was built — treat it as an incident.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for KernelPinError {}

/// The recorded sum for a supported architecture, or `None`.
///
/// Pure — no filesystem access, unit-testable on any host. Takes the arch
/// as a parameter rather than reading [`std::env::consts::ARCH`] itself
/// so both arms can be tested from either dev box.
///
/// Spellings are Rust's (`std::env::consts::ARCH`), which match `uname
/// -m` on Linux. macOS's `arm64` is deliberately **not** accepted: these
/// kernels boot a Linux guest, and silently mapping it would trade a
/// clear error for a confusing failure later.
pub fn expected_sha256(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some(GUEST_KERNEL_SHA256_X86_64),
        "aarch64" => Some(GUEST_KERNEL_SHA256_AARCH64),
        _ => None,
    }
}

/// Hash `path` and compare against `expected` (bare lowercase hex).
///
/// The only IO in this module. Streams the file in
/// [`HASH_CHUNK_BYTES`] chunks so a 40 MB kernel does not become a 40 MB
/// allocation.
pub fn verify_kernel(path: &Path, expected: &str) -> Result<(), KernelPinError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| KernelPinError::Unreadable(path.to_path_buf(), e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK_BYTES];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| KernelPinError::Unreadable(path.to_path_buf(), e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        return Err(KernelPinError::Mismatch {
            path: path.to_path_buf(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Verify `path` against the sum recorded for `arch`.
///
/// The arch lookup happens *first*, so an unrecorded architecture fails
/// without reading the file — there is no sum to compare against, and
/// "we hashed it and had nothing to check it with" is not a pass.
pub fn verify_pinned_kernel(path: &Path, arch: &str) -> Result<(), KernelPinError> {
    let expected = expected_sha256(arch).ok_or_else(|| KernelPinError::UnknownArch(arch.to_string()))?;
    verify_kernel(path, expected)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-sandbox guest_kernel_pin 2>&1 | tail -20
```

Expected: `test result: ok. 7 passed; 0 failed`.

- [ ] **Step 6: Clippy clean**

```sh
source "$HOME/.cargo/env"
cargo clippy -p kastellan-sandbox --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: no warnings, exit 0.

- [ ] **Step 7: Commit**

```bash
git add sandbox/src/guest_kernel_pin.rs sandbox/src/lib.rs sandbox/Cargo.toml Cargo.lock
git commit -m "sandbox: pin the micro-VM guest kernel sha256 in a dual-platform module

The table and the hashing are deliberately NOT cfg(linux)-gated, even
though the only caller is: a fail-closed check exercised on one host is
half-verified (#471's lesson). Fails closed on unknown arch without
reading the file.

Refs #479

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Enforce the pin on the Firecracker boot path

**Files:**
- Modify: `sandbox/src/linux_firecracker.rs` (`spawn_under_policy`, immediately after the `resolve_image` call ~line 182; tests appended to the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::guest_kernel_pin::verify_pinned_kernel` from Task 1.
- Produces: no new public API. Behaviour: `LinuxFirecracker::spawn_under_policy` returns `SandboxError::Backend` when the kernel is missing or unpinned.

> **Note for the implementer:** these tests are `cfg(linux)` (the whole module is) but need **no KVM, no Firecracker binary and no root** — the check runs before any VM work, so a bogus image dir short-circuits. They run on the DGX in the ordinary `cargo test --workspace`. On the dev Mac they compile out entirely; that is expected, and Task 3 is what the Mac exercises.

- [ ] **Step 1: Write the failing tests**

Append to the existing `#[cfg(test)] mod tests` block in `sandbox/src/linux_firecracker.rs`:

```rust
    /// A unique temp dir holding a fake image set, using `std` only.
    fn fake_image_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("kastellan-fcpin-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fake image dir");
        dir
    }

    fn policy_pointing_at(dir: &std::path::Path) -> SandboxPolicy {
        let mut policy = SandboxPolicy::default();
        policy.env = vec![(
            "KASTELLAN_MICROVM_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        )];
        policy
    }

    /// The #479 gate. A `vmlinux` that is not the pinned one must stop
    /// the spawn outright — no VM, no run dir, no launcher.
    #[test]
    fn spawn_refuses_a_guest_kernel_that_does_not_match_the_pin() {
        let dir = fake_image_dir("bad-kernel");
        std::fs::write(dir.join("vmlinux"), b"not the pinned kernel").expect("write fake kernel");
        std::fs::write(dir.join("python-exec.ext4"), b"fake rootfs").expect("write fake rootfs");

        let err = LinuxFirecracker::new()
            .spawn_under_policy(&policy_pointing_at(&dir), "/bin/true", &[])
            .expect_err("an unpinned kernel must never boot");

        let msg = err.to_string();
        assert!(
            msg.contains("does not match the pinned sha256"),
            "the failure must name the pin, not some downstream symptom: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing kernel must fail on the pin check with an actionable
    /// message, rather than surfacing later as an opaque launcher error.
    #[test]
    fn spawn_refuses_a_missing_guest_kernel() {
        let dir = fake_image_dir("no-kernel");
        let err = LinuxFirecracker::new()
            .spawn_under_policy(&policy_pointing_at(&dir), "/bin/true", &[])
            .expect_err("a missing kernel must never reach the launcher");
        assert!(
            err.to_string().contains("cannot read the micro-VM guest kernel"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

On the DGX (`ssh dgx '<cmd>'`), or on any Linux host:

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-sandbox guest_kernel 2>&1 | tail -20
```

Expected: both new tests FAIL. `spawn_refuses_a_guest_kernel_that_does_not_match_the_pin` fails because the spawn proceeds past the (absent) check and errors on something else — most likely a launcher/`firecracker` discovery message — so the `does not match the pinned sha256` assertion trips. **Read the actual failure message and confirm it is the missing-check symptom, not a test bug.**

- [ ] **Step 3: Add the check to `spawn_under_policy`**

In `sandbox/src/linux_firecracker.rs`, find:

```rust
        let image = resolve_image(&policy.env);
        let mut plan = build_launch_plan(policy, &image, program, args)?;
```

Replace with:

```rust
        let image = resolve_image(&policy.env);
        // #479: verify the guest kernel is the pinned one, on EVERY boot.
        //
        // #471 verifies it at rootfs-build time, which does not constrain
        // the file afterwards — it is booted for months without another
        // check. The image dir is reachable by the agent's own OS user,
        // exactly what the threat model assumes a compromise reaches, and
        // the guest kernel is what enforces the containment boundary the
        // rest of the model rests on.
        //
        // Deliberately the first thing after resolving the paths: a bad
        // kernel costs no run dir, no images and no launcher. Fails closed
        // with no bypass env var — that would be the "spawn unsandboxed"
        // escape hatch CLAUDE.md forbids, on the one file that defines the
        // boundary. See `guest_kernel_pin` for the TOCTOU caveat and for
        // why the ownership half of #479 is the part that closes it.
        crate::guest_kernel_pin::verify_pinned_kernel(&image.kernel_path, std::env::consts::ARCH)
            .map_err(|e| SandboxError::Backend(e.to_string()))?;
        let mut plan = build_launch_plan(policy, &image, program, args)?;
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-sandbox 2>&1 | tail -20
cargo clippy -p kastellan-sandbox --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: all `kastellan-sandbox` tests pass (the two new ones included); clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker.rs
git commit -m "sandbox: verify the guest kernel on every micro-VM boot (#479)

Checked immediately after resolve_image, before any run dir, image or
launcher work, so a bad kernel costs nothing. Fails closed; no bypass
env var, by design.

Refs #479

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: CI-enforce that the Rust and bash pins agree

**Files:**
- Modify: `tests-common/src/microvm.rs` (append to the existing `#[cfg(test)] mod tests`)
- Modify: `tests-common/Cargo.toml` (add a dev-dependency on `kastellan-sandbox`)

**Interfaces:**
- Consumes: `kastellan_sandbox::guest_kernel_pin::{GUEST_KERNEL_SHA256_X86_64, GUEST_KERNEL_SHA256_AARCH64}` from Task 1; the existing `GUEST_KERNEL_LIB` const and `repo_root()` helper already in this file.
- Produces: nothing consumed by later tasks.

> **Why here and not in `sandbox`:** `kastellan-tests-common` is `publish = false`, so it may freely read repo-relative script paths; `kastellan-sandbox` is published to crates.io and must not. This file already owns `GUEST_KERNEL_LIB` and the #478 drift tests, so the pin-integrity tests all live together. `linux-check.yml` runs this crate's tests on every PR.

- [ ] **Step 1: Add the dev-dependency**

In `tests-common/Cargo.toml`, under `[dev-dependencies]` (create the section if absent, after `[dependencies]`):

```toml
[dev-dependencies]
# #479: the pin-agreement test compares this crate's view of
# scripts/workers/microvm/lib/guest-kernel.sh against the Rust consts.
# A dev-dependency, not a dependency: no shipped code here uses it.
kastellan-sandbox = { path = "../sandbox" }
```

- [ ] **Step 2: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests` block in `tests-common/src/microvm.rs`:

```rust
    /// Pull `NAME="value"` out of the shared bash pin.
    ///
    /// Deliberately strict about the shape: if the assignment is
    /// reformatted, this fails loudly rather than quietly matching
    /// nothing and letting the comparison below pass vacuously — a test
    /// that can silently stop testing is worse than no test.
    fn bash_pin_value(body: &str, name: &str) -> String {
        let prefix = format!("{name}=\"");
        let line = body
            .lines()
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("{GUEST_KERNEL_LIB} has no line starting `{prefix}`"));
        line[prefix.len()..]
            .strip_suffix('"')
            .unwrap_or_else(|| panic!("malformed assignment in {GUEST_KERNEL_LIB}: {line}"))
            .to_string()
    }

    /// #479: the boot-time check (Rust) and the build-time check (bash)
    /// must agree, or one of them is verifying against a stale sum.
    ///
    /// The duplication is deliberate — `kastellan-sandbox` is published
    /// to crates.io and cannot `include_str!` a path outside its own
    /// directory — so this test is what makes it safe. It runs on every
    /// PR via `linux-check.yml`, because a version bump that updates one
    /// side and not the other is exactly the drift least likely to be
    /// caught by an operator's occasional DGX run.
    #[test]
    fn rust_and_bash_kernel_pins_agree() {
        use kastellan_sandbox::guest_kernel_pin::{
            GUEST_KERNEL_SHA256_AARCH64, GUEST_KERNEL_SHA256_X86_64,
        };
        let body = std::fs::read_to_string(repo_root().join(GUEST_KERNEL_LIB))
            .unwrap_or_else(|e| panic!("read {GUEST_KERNEL_LIB}: {e}"));

        assert_eq!(
            bash_pin_value(&body, "KASTELLAN_GUEST_KERNEL_SHA256_X86_64"),
            GUEST_KERNEL_SHA256_X86_64,
            "x86_64 pin drifted between {GUEST_KERNEL_LIB} and \
             sandbox/src/guest_kernel_pin.rs — bump both together"
        );
        assert_eq!(
            bash_pin_value(&body, "KASTELLAN_GUEST_KERNEL_SHA256_AARCH64"),
            GUEST_KERNEL_SHA256_AARCH64,
            "aarch64 pin drifted between {GUEST_KERNEL_LIB} and \
             sandbox/src/guest_kernel_pin.rs — bump both together"
        );
    }

    /// #479's other half: the privileged installer is the only thing that
    /// puts the kernel in place root-owned, and the image dir must be
    /// sticky.
    ///
    /// The sticky bit is load-bearing, not cosmetic: POSIX directory
    /// write permission alone permits `unlink()` of any entry regardless
    /// of that file's owner, so a root-owned `vmlinux` in a
    /// worker-writable dir could simply be removed and replaced. If a
    /// later edit drops the `chmod 1755`, the ownership half of #479 is
    /// silently void — hence a test rather than a comment.
    #[test]
    fn installer_root_owns_the_kernel_in_a_sticky_dir() {
        let script = "scripts/linux/install-firecracker-vsock.sh";
        let body = std::fs::read_to_string(repo_root().join(script))
            .unwrap_or_else(|e| panic!("read {script}: {e}"));

        assert!(
            body.contains("chmod 1755"),
            "{script} must make the micro-VM image dir sticky (1755); without +t the \
             agent user can unlink root's vmlinux and the ownership half of #479 is void"
        );
        assert!(
            body.contains("guest-kernel.sh"),
            "{script} must source {GUEST_KERNEL_LIB} rather than fetching the kernel itself"
        );
        assert!(
            body.contains("chown root:root"),
            "{script} must leave vmlinux root-owned"
        );
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common 2>&1 | tail -25
```

Expected: `rust_and_bash_kernel_pins_agree` **PASSES** already (Task 1 copied the sums verbatim — that is the intended steady state, and the test's value is in future bumps). `installer_root_owns_the_kernel_in_a_sticky_dir` **FAILS** with "must make the micro-VM image dir sticky (1755)" — Task 4 fixes it.

> If `rust_and_bash_kernel_pins_agree` fails here, do **not** edit either sum to match: one of them was mistyped in Task 1. Re-copy from `guest-kernel.sh`.

- [ ] **Step 4: Commit the passing half now**

Commit only the pin-agreement work; the installer test stays red until Task 4 and is committed there.

```bash
git add tests-common/src/microvm.rs tests-common/Cargo.toml Cargo.lock
git commit -m "test-infra: CI-enforce that the Rust and bash guest-kernel pins agree

Publishing constraints force the sum to be written in two places
(kastellan-sandbox is on crates.io and cannot include_str! outside its
own dir). This makes the duplication safe: linux-check.yml runs these
tests on every PR, and a bump that updates one side fails there.

Also pins #479's ownership half structurally: installer_root_owns_the_
kernel_in_a_sticky_dir is red until the next commit.

Refs #479

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Sticky image dir + root-owned kernel in the installer

**Files:**
- Modify: `scripts/linux/install-firecracker-vsock.sh` (header comment ~line 17; section 4 ~lines 92-99)
- Modify: `scripts/workers/microvm/lib/guest-kernel.sh` (quarantine failure message, ~line 150)

**Interfaces:**
- Consumes: `fetch_guest_kernel` from `guest-kernel.sh` (unchanged behaviour); the test from Task 3.
- Produces: `/var/lib/kastellan/microvm/` at mode `1755` with `vmlinux` as `root:root 0644`.

> **Why the eight `build-*-rootfs.sh` scripts are NOT changed.** `fetch_guest_kernel` already behaves correctly against a root-owned kernel: present-and-good → it early-returns without writing, so root ownership is no obstacle; present-and-bad → the quarantine `mv` fails on a root-owned file under `+t` and the build stops; a custom user-owned `KASTELLAN_MICROVM_DIR` → it fetches as before. Leaving them untouched also keeps #478's `every_build_script_fetches_through_the_pin` valid. Step 4 below improves the one message this makes reachable.

- [ ] **Step 1: Confirm the Task 3 test is red**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common installer_root_owns 2>&1 | tail -15
```

Expected: FAIL, "must make the micro-VM image dir sticky (1755)".

- [ ] **Step 2: Update the installer**

In `scripts/linux/install-firecracker-vsock.sh`, replace the whole of section 4 — these five lines:

```bash
# 4. Provision the micro-VM image dir, owned by the worker user. build-rootfs.sh
#    and the FirecrackerVm backend both default to /var/lib/kastellan/microvm;
#    creating it here (the one privileged step) lets the unprivileged rootfs
#    build + the per-user service write it without further root.
MICROVM_DIR="/var/lib/kastellan/microvm"
mkdir -p "${MICROVM_DIR}"
chown "${TARGET_USER}:$(id -gn "${TARGET_USER}")" "${MICROVM_DIR}"
echo "Provisioned ${MICROVM_DIR} (owner ${TARGET_USER})"
```

with:

```bash
# 4. Provision the micro-VM image dir + install the pinned guest kernel.
#
#    The dir stays owned by the worker user so the eight unprivileged
#    build-*-rootfs.sh scripts keep working without root. Issue #479 adds two
#    things on top of that:
#
#      * mode 1755 — the STICKY bit, and it is load-bearing rather than
#        belt-and-braces. POSIX directory write permission on its own permits
#        unlink() and rename() of ANY entry, regardless of that entry's owner
#        and mode. So root-owning vmlinux inside a worker-writable dir would
#        stop nothing at all: the agent could remove it and drop in its own.
#        With +t, removal and rename are restricted to the entry's owner. The
#        agent still freely creates and replaces its own *.ext4 images (it
#        owns those); it cannot touch root's kernel.
#
#      * the guest kernel is fetched HERE, as root, and left root:root 0644.
#        docs/threat-model.md assumes a worst-case compromise reaches the
#        agent's own OS user — and the micro-VM is the containment boundary
#        while the guest kernel is what enforces it. It is the one artefact in
#        that directory an attacker at that level must not be able to rewrite.
#        (kastellan also re-verifies it at every VM boot; see
#        sandbox/src/guest_kernel_pin.rs. That check is TOCTOU-limited, which
#        is why this ownership step is the half that actually closes it.)
#
#    The rootfs images are deliberately NOT protected this way: a tampered
#    rootfs is not an escalation — the guest userland is already assumed
#    hostile and the VM is what contains it.
MICROVM_DIR="/var/lib/kastellan/microvm"
mkdir -p "${MICROVM_DIR}"
chown "${TARGET_USER}:$(id -gn "${TARGET_USER}")" "${MICROVM_DIR}"
chmod 1755 "${MICROVM_DIR}"
echo "Provisioned ${MICROVM_DIR} (owner ${TARGET_USER}, sticky)"

# The same pinned+verified fetch the rootfs builds use — one place the URL,
# the arch table and the sums are written down (issue #471).
# shellcheck source=../workers/microvm/lib/guest-kernel.sh
source "$(dirname "${BASH_SOURCE[0]}")/../workers/microvm/lib/guest-kernel.sh"
fetch_guest_kernel "${MICROVM_DIR}"
chown root:root "${MICROVM_DIR}/vmlinux"
chmod 0644 "${MICROVM_DIR}/vmlinux"
echo "Installed pinned guest kernel root-owned at ${MICROVM_DIR}/vmlinux"
```

Then update the header comment. Replace lines 17-19:

```bash
# It also provisions the micro-VM image dir /var/lib/kastellan/microvm (owned
# by the worker user) so the unprivileged build-rootfs.sh + the per-user
# service can write it without further root.
```

with:

```bash
# It also provisions the micro-VM image dir /var/lib/kastellan/microvm (owned
# by the worker user, mode 1755) so the unprivileged build-rootfs.sh + the
# per-user service can write it without further root, and installs the pinned
# guest kernel there as root:root 0644 — the sticky bit is what stops the
# worker user from unlinking and replacing it (issue #479). Re-run this script
# after bumping the pinned kernel version.
```

- [ ] **Step 3: Run the test to verify it passes**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common 2>&1 | tail -20
```

Expected: all pass, including `installer_root_owns_the_kernel_in_a_sticky_dir`.

- [ ] **Step 4: Make the newly-reachable quarantine message actionable**

A build run by the agent user now hits the "cannot quarantine" path whenever a root-owned kernel fails to verify, which previously could not happen. In `scripts/workers/microvm/lib/guest-kernel.sh`, in `_kastellan_quarantine`, replace:

```bash
        echo "Could not quarantine $file (tried: $target)." >&2
        echo "Remove it by hand; the build will keep refusing until you do." >&2
```

with:

```bash
        echo "Could not quarantine $file (tried: $target)." >&2
        echo "If it is root-owned (the default since #479), re-run the privileged" >&2
        echo "installer, which owns the kernel and can replace it:" >&2
        echo "    sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
        echo "Otherwise remove it by hand; the build will keep refusing until you do." >&2
```

- [ ] **Step 5: Re-run the bash-driving tests**

The existing #478 tests assert on this function's stderr.

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common 2>&1 | tail -20
```

Expected: all pass. If a test asserting `!stderr.contains("  quarantined:")` now fails, the new lines have broken its expectations — re-read that test and adjust the *message*, never the assertion.

- [ ] **Step 6: Shell syntax check**

```sh
bash -n scripts/linux/install-firecracker-vsock.sh && echo "syntax OK"
bash -n scripts/workers/microvm/lib/guest-kernel.sh && echo "syntax OK"
```

Expected: `syntax OK` twice.

- [ ] **Step 7: Commit**

```bash
git add scripts/linux/install-firecracker-vsock.sh scripts/workers/microvm/lib/guest-kernel.sh
git commit -m "microvm: root-own the guest kernel in a sticky image dir (#479)

The sticky bit is the load-bearing part: POSIX directory write
permission alone permits unlink() of any entry regardless of its owner,
so root-owning vmlinux in a worker-writable dir would stop nothing. With
+t the agent still creates and replaces its own *.ext4 images but cannot
touch root's kernel.

The eight build-*-rootfs.sh are unchanged: fetch_guest_kernel already
early-returns on a verified kernel without writing, so root ownership is
no obstacle to an unprivileged build.

Refs #479

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: DGX acceptance — cost measurement, negative case, live verification

**Files:** none modified unless a defect is found.

**Interfaces:** consumes everything from Tasks 1-4.

> All commands run via `ssh dgx '<command>'` **exactly in that form** — the `Bash(ssh dgx *)` allow rule is a prefix match, so flags before the hostname are denied. Write long-run logs to `~`, never `/tmp` (a workspace test run scrubs `/tmp` mid-run).

- [ ] **Step 1: Push the branch and sync the DGX**

```bash
git push -u origin fix/479-verify-guest-kernel-at-boot
ssh dgx 'cd ~/src/kastellan && git fetch origin && git checkout fix/479-verify-guest-kernel-at-boot && git pull'
```

- [ ] **Step 2: Measure the per-boot hash cost**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && cargo build --release -p kastellan-sandbox 2>&1 | tail -3'
ssh dgx 'for i in 1 2 3; do /usr/bin/time -f "%e s" sha256sum /var/lib/kastellan/microvm/vmlinux >/dev/null; done'
```

Expected: three timings, each well under 0.1 s on aarch64 (ARM crypto extensions). **Record the numbers.**

Decision gate: if the median exceeds **~50 ms**, stop and report to the operator rather than adding an mtime cache — a cache reintroduces the trust-what-is-on-disk pattern this work removes.

- [ ] **Step 3: Apply the installer and verify the resulting permissions**

```bash
ssh dgx 'cd ~/src/kastellan && sudo ./scripts/linux/install-firecracker-vsock.sh 2>&1 | tail -10'
ssh dgx 'ls -ld /var/lib/kastellan/microvm && ls -l /var/lib/kastellan/microvm/vmlinux'
```

Expected: dir `drwxr-xr-t` (the `t`), `vmlinux` owned `root root` mode `-rw-r--r--`.

- [ ] **Step 4: Prove the write primitive is actually gone**

This is the assertion the whole ownership half rests on; verify it rather than assuming it.

```bash
ssh dgx 'rm -f /var/lib/kastellan/microvm/vmlinux; echo "rm exit=$?"'
ssh dgx 'ls -l /var/lib/kastellan/microvm/vmlinux'
```

Expected: `rm` fails with "Operation not permitted" and a non-zero exit; the kernel is still there, still `root root`. **If `rm` succeeds, the sticky bit is not doing its job — stop and investigate before going further.**

- [ ] **Step 5: Confirm an unprivileged rootfs build still works**

```bash
ssh dgx 'cd ~/src/kastellan && ./scripts/workers/microvm/build-rootfs.sh 2>&1 | tail -5'
```

Expected: succeeds as the ordinary user, and the "Fetching pinned guest kernel" line does **not** appear (the root-owned kernel verifies and is reused).

- [ ] **Step 6: Full workspace gate**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && setsid bash -lc "export PATH=\$HOME/.local/bin:\$PATH; cargo build --workspace && cargo test --workspace; echo DONE_EXIT=\$?" > ~/dgx-479.log 2>&1 < /dev/null &'
```

Poll with `ssh dgx 'tail -5 ~/dgx-479.log'` until `DONE_EXIT=` appears.

Expected: `DONE_EXIT=0`, and **2607 + the new tests passed / 0 failed / 50 ignored** (the `main` baseline is 2607/0/50 at `dd10bd68`). Confirm the delta equals exactly the tests added by Tasks 1-3.

- [ ] **Step 7: Clippy gate**

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5'
```

Expected: exit 0, no warnings. This gate is authoritative for `cfg(linux)` code the Mac compiles out.

- [ ] **Step 8: Negative case — prove the check is load-bearing**

A fail-closed check never observed to fail is not yet known to work.

```bash
ssh dgx 'cd ~/src/kastellan && sed -i "s/bb1f50912d63a8ca5e92d488984875e1177eb9283050ffa592a8cb455cada52d/bb1f50912d63a8ca5e92d488984875e1177eb9283050ffa592a8cb455cada52e/" sandbox/src/guest_kernel_pin.rs'
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && cargo test -p kastellan-tests-common rust_and_bash_kernel_pins_agree 2>&1 | tail -10'
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && cargo test -p kastellan-core --test python_exec_firecracker_e2e 2>&1 | tail -15'
ssh dgx 'cd ~/src/kastellan && git checkout sandbox/src/guest_kernel_pin.rs && git status --short'
```

Expected, in order: the drift test FAILS naming the x86_64/aarch64 disagreement; the live micro-VM e2e FAILS with "does not match the pinned sha256" (proving the boot gate is real and not bypassed); `git status` clean after the revert. **Record both failure messages.**

- [ ] **Step 9: Confirm the Mac side too**

The dual-platform requirement means the pin module's tests must run on macOS.

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-sandbox guest_kernel_pin 2>&1 | tail -10
cargo test -p kastellan-tests-common 2>&1 | tail -10
cargo clippy -p kastellan-sandbox -p kastellan-tests-common --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: all pass on macOS, clippy exit 0. Per `cfg-linux-e2e-deadcode-dgx-clippy`, the DGX gate is blind to unused-import/dead-code in items that exist on both platforms, so this Mac pass is not redundant.

- [ ] **Step 10: Commit any fixes, then update the handover docs**

If Steps 1-9 surfaced defects, fix and re-run the affected gate. Then update `docs/devel/handovers/HANDOVER.md` and `docs/devel/ROADMAP.md`:

- HANDOVER: a new `✅ MERGED`-style header entry for #479 (recording the measured hash cost from Step 2, the new test count from Step 6, and the Step 4 + Step 8 negative-case results); update the "Current state" HEAD, the test baseline, and the "Next TODO" entry that currently lists #479 as an open follow-on.
- ROADMAP: mark #479 done under the same section as #471's line 491 entry, carrying forward #386 as the remaining sibling.
- Prune both to stay under 500 lines.

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: #479 guest-kernel boot verification — session handover

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git push
```

- [ ] **Step 11: Open the PR**

```bash
gh pr create --base main --title "security: verify the micro-VM guest kernel at every VM boot (closes #479)" --body "$(cat <<'EOF'
Closes #479. Follow-up to #471 / PR #478.

## The gap

#478 sha256-verifies the guest `vmlinux` at rootfs-build time. That does not
constrain the file afterwards: it is then booted, possibly for months and
hundreds of times, with nothing re-checking it — and
`/var/lib/kastellan/microvm/` was owned and writable by the agent's own OS
user, which `docs/threat-model.md` assumes a worst-case compromise reaches.
The micro-VM is the containment boundary and the guest kernel is what
enforces it.

## Two halves, deliberately

**Ownership removes the write primitive.** The image dir becomes mode `1755`
and `vmlinux` becomes `root:root 0644`. The sticky bit is load-bearing, not
decoration: POSIX directory write permission alone permits `unlink()` of any
entry regardless of its owner, so root-owning the kernel in a worker-writable
dir would stop nothing. Verified on the DGX — `rm` as the agent user fails
with EPERM.

**Boot-time hashing catches what ownership misses** — a pre-existing install,
or `KASTELLAN_MICROVM_DIR` pointed at an operator-writable dir. Documented as
TOCTOU-limited rather than sold as sufficient alone; ownership is what closes
the hole.

## Notes for reviewers

- `guest_kernel_pin.rs` is deliberately **not** `cfg(linux)`-gated though its
  only caller is: a fail-closed check exercised on one host is half-verified.
  Its tests run on both dev boxes.
- The sum is necessarily written twice (`kastellan-sandbox` is published to
  crates.io and cannot `include_str!` outside its own dir). The duplication is
  CI-enforced by `rust_and_bash_kernel_pins_agree`, which runs on every PR.
- No bypass env var — that would be the "spawn unsandboxed" escape hatch
  CLAUDE.md forbids, on the one file that defines the boundary.
- The eight `build-*-rootfs.sh` are unchanged: `fetch_guest_kernel` already
  early-returns on a verified kernel without writing.
- Rootfs images are deliberately **not** protected — a tampered rootfs is not
  an escalation, since the guest userland is already assumed hostile.
- Still open, same family: #386 (Firecracker binary + Matrix homeserver binary
  fetched unchecked), which can reuse `verify_sha256`.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-Review

**Spec coverage:** §3 boundary → Task 4. §4.1 dual-platform module → Task 1 (+ Mac gate, Task 5 Step 9). §4.2 pure/IO split → Task 1. §4.3 TOCTOU documented → Task 1 module doc. §4.4 fail closed, no bypass → Task 1 + Task 2. §5 sum source of truth → Task 3. §6 testing, all five kinds → Tasks 1-3 + Task 5 Steps 6/8. §7 cost gate → Task 5 Step 2, with the escalate-don't-cache rule. §8 out of scope → carried into the PR body.

**Deviation from spec §3.3, flagged to the operator before writing:** the spec introduced `require_guest_kernel` so builds would stop fetching. Planning showed it unnecessary — `fetch_guest_kernel` already early-returns on a verified kernel without writing, so a root-owned kernel is no obstacle. Dropping it also keeps #478's `every_build_script_fetches_through_the_pin` valid. Task 4's note records the reasoning.

**Type consistency:** `expected_sha256`, `verify_kernel`, `verify_pinned_kernel`, `KernelPinError`, `GUEST_KERNEL_SHA256_{X86_64,AARCH64}` are named identically in Tasks 1, 2 and 3. `repo_root()` and `GUEST_KERNEL_LIB` in Task 3 are pre-existing in `tests-common/src/microvm.rs`.
