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
//! `scripts/linux/install-firecracker-vsock.sh`: the image dir is
//! `root:<worker-group>` mode `1775` and `vmlinux` is `root:root`, so on a
//! correctly provisioned host the agent cannot unlink, rename or overwrite
//! it. Note that **root must own the directory and its parent too**, not
//! just the kernel — `unlink(2)` exempts the directory's owner as well as
//! the file's, and permission to rename the image dir itself comes from
//! its parent.
//!
//! The two halves are complementary and neither is sufficient alone. This
//! one is what still holds wherever the ownership half does not reach, and
//! those cases are real rather than hypothetical:
//!
//!   * an install predating that change, or one whose installer was never
//!     re-run;
//!   * `KASTELLAN_MICROVM_DIR` pointed at a directory root does not manage
//!     — including `~/.local/share/kastellan/microvm`, which the build
//!     scripts themselves document as a supported layout and which carries
//!     **no** ownership protection at all;
//!   * anything that puts an agent-owned `vmlinux` back into the protected
//!     directory.
//!
//! So: do not read the ownership half as making this check redundant.
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
    let expected =
        expected_sha256(arch).ok_or_else(|| KernelPinError::UnknownArch(arch.to_string()))?;
    verify_kernel(path, expected)
}

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

    /// The two sizes where a chunked read loop classically goes wrong:
    /// nothing to read at all, and a length that is an exact multiple of
    /// the buffer (so the final `read` returns 0 rather than a short
    /// count). Both must still produce the true digest — an empty file
    /// in particular must be *rejected*, never waved through.
    #[test]
    fn verify_kernel_handles_empty_and_exact_multiple_files() {
        let dir = temp_dir("edges");

        let empty = write_file(&dir, "empty", b"");
        assert!(
            verify_kernel(&empty, HELLO_SHA256).is_err(),
            "a 0-byte kernel must never pass"
        );
        assert!(verify_kernel(&empty, &sha256_hex_of_slice(b"")).is_ok());

        let exact = vec![0x5Au8; HASH_CHUNK_BYTES * 2];
        let path = write_file(&dir, "exact", &exact);
        assert!(verify_kernel(&path, &sha256_hex_of_slice(&exact)).is_ok());

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
