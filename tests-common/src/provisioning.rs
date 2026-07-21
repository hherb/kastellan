//! Integrity pins for the binaries our provisioning scripts download
//! (issue #386).
//!
//! # Why this module exists
//!
//! Three provisioning scripts fetch a third-party binary over the network
//! and then *install and run* it. If the download is not integrity-checked,
//! a compromised mirror, a TLS MITM, or plain wire corruption substitutes an
//! arbitrary binary that we then execute — as the worker user, or (worse) as
//! root under systemd.
//!
//! The guest **kernel** — the most dangerous of the three, because it boots
//! *inside* the containment boundary — was pinned in issues #471/#479 and is
//! covered by [`crate::microvm`]. This module covers the other two:
//!
//!   * `scripts/workers/microvm/install-firecracker.sh` — the Firecracker
//!     VMM release tarball from GitHub, installed to the worker user's
//!     `~/.local/bin/firecracker` and executed.
//!   * `scripts/matrix/vps/phase2-homeserver.sh` — the Continuwuity Matrix
//!     homeserver binary from a third-party Forgejo host, installed
//!     `root:root 0755` and run under systemd.
//!
//! # What these tests actually pin
//!
//! Two properties, structural rather than behavioural, because the scripts
//! themselves only ever run on a real provisioning host:
//!
//!   * **A well-formed sha256 is recorded** for each artefact (per
//!     architecture, where the script is multi-arch). A missing or malformed
//!     pin can only ever weaken the check, so it is worth failing the build
//!     over.
//!   * **Verification happens *before* the artefact is used** — extracted,
//!     installed, or executed. This is the load-bearing lesson from #479: a
//!     permission- or integrity-based control is only as good as the *order*
//!     of operations that establishes it, so a presence-only test (does the
//!     word `verify_sha256` appear anywhere?) is not enough. A verify that
//!     runs *after* the install has already happened protects nothing.
//!
//! To make those order assertions honest, every anchor below is matched
//! against a real **command** line, never a comment — the other #479 lesson,
//! where `contains("guest-kernel.sh")` was satisfied by a `# shellcheck
//! source=` comment sitting above the real `source`, so deleting the source
//! kept the test green. See [`first_command_line`].
//!
//! # Why no sums are duplicated into Rust here
//!
//! The guest-kernel pin lives in *both* bash and Rust because
//! `linux_firecracker.rs` re-checks the kernel at VM boot — there is a
//! genuine Rust consumer, so `rust_and_bash_kernel_pins_agree` guards the
//! drift. These two binaries have **no** runtime Rust consumer: they are
//! install-time only. Mirroring their sums into Rust would invent a drift
//! surface with nothing on the other side of it, so bash stays the single
//! source of truth and these tests only assert *structural* facts about it.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Output;

    /// The shared, pure sha256 verifier extracted from the guest-kernel pin
    /// so both installers and the kernel fetch can share one hasher.
    const VERIFY_LIB: &str = "scripts/workers/microvm/lib/verify.sh";

    /// The Firecracker VMM installer (worker-user, `~/.local/bin`).
    const FIRECRACKER_INSTALLER: &str = "scripts/workers/microvm/install-firecracker.sh";

    /// The Matrix homeserver install phase (root, systemd). Runs *standalone*
    /// on the VPS — the deployment copies only the phase scripts into `~/`,
    /// not the repo — so it cannot source [`VERIFY_LIB`] and must carry its
    /// own inline verifier.
    const MATRIX_PHASE2: &str = "scripts/matrix/vps/phase2-homeserver.sh";

    /// sha256 of the 5 bytes `hello`, from the standard test vectors — lets
    /// the accept/reject paths run against a 5-byte file instead of a real
    /// multi-megabyte binary, so these stay fast unit tests.
    const HELLO_SHA256: &str =
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    /// The repository root, derived from this crate's manifest dir.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tests-common has a workspace parent")
            .to_path_buf()
    }

    /// Read a repo-relative script to a string, panicking with a useful
    /// message if it is missing (a moved or renamed script should fail these
    /// tests loudly, not silently pass).
    fn read_script(rel: &str) -> String {
        let path = repo_root().join(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// Run `snippet` under `bash` with [`VERIFY_LIB`] already sourced.
    ///
    /// The lib is exactly that — a library: sourcing it must define functions
    /// and do nothing else. If it ever grew a top-level side effect (a stray
    /// `curl`, an `exit`), every test using this helper would break, which is
    /// the intended alarm.
    fn bash_with_verify_lib(snippet: &str) -> Output {
        let lib = repo_root().join(VERIFY_LIB);
        let script = format!("set -euo pipefail; source '{}'; {snippet}", lib.display());
        std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .output()
            .expect("bash is available on both dev hosts")
    }

    /// 0-based index of the first **command** line containing `needle`.
    ///
    /// Blank lines and lines whose first non-whitespace character is `#` are
    /// skipped, so a comment that merely *mentions* the needle cannot satisfy
    /// an ordering or presence check. Panics if no command line matches —
    /// a test that silently matches nothing is worse than no test (the #479
    /// lesson, baked into the helper so no individual test can forget it).
    fn first_command_line(body: &str, needle: &str) -> usize {
        body.lines()
            .enumerate()
            .find(|(_, line)| {
                let trimmed = line.trim_start();
                !trimmed.is_empty() && !trimmed.starts_with('#') && line.contains(needle)
            })
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| panic!("no command line contains {needle:?}"))
    }

    /// True iff some **command** line (not a comment) contains `needle`.
    fn has_command_line(body: &str, needle: &str) -> bool {
        body.lines().any(|line| {
            let trimmed = line.trim_start();
            !trimmed.is_empty() && !trimmed.starts_with('#') && line.contains(needle)
        })
    }

    /// Pull the value out of a `NAME="value"` assignment, matching the first
    /// **command** line that assigns `name`. Strict about the shape: panics
    /// rather than matching nothing, so a reformatted assignment fails the
    /// test loudly instead of letting a hex-shape check pass vacuously.
    fn assigned_value(body: &str, name: &str) -> String {
        let prefix = format!("{name}=\"");
        let line = body
            .lines()
            .map(str::trim_start)
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("no assignment `{prefix}…\"` found"));
        line[prefix.len()..]
            .strip_suffix('"')
            .unwrap_or_else(|| panic!("malformed assignment: {line}"))
            .to_string()
    }

    /// A recorded sum must be a 64-character lowercase-or-upper hex string;
    /// anything else can only weaken the check.
    fn assert_is_sha256(sum: &str, what: &str) {
        assert_eq!(sum.len(), 64, "{what} is not a sha256 (len {}): {sum:?}", sum.len());
        assert!(
            sum.chars().all(|c| c.is_ascii_hexdigit()),
            "{what} is not hex: {sum:?}"
        );
    }

    // ---------------------------------------------------------------
    // The shared verifier (lib/verify.sh) — behavioural.
    //
    // These run on macOS as well as the DGX: "does the integrity check
    // reject a bad file" needs no VM, and a check exercised on only one
    // host is a check that is half-verified.
    // ---------------------------------------------------------------

    #[test]
    fn verify_lib_sources_cleanly_and_defines_verify_sha256() {
        let lib = repo_root().join(VERIFY_LIB);
        assert!(lib.is_file(), "missing the shared verifier: {}", lib.display());
        // Sourcing must have no side effects, and must leave `verify_sha256`
        // callable.
        let out = bash_with_verify_lib("type -t verify_sha256");
        assert!(
            out.status.success(),
            "sourcing the verifier must define verify_sha256 with no side effects; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "function",
            "verify_sha256 must be a function after sourcing"
        );
    }

    #[test]
    fn verify_lib_accepts_content_matching_the_expected_sum() {
        let out = bash_with_verify_lib(&format!(
            "d=$(mktemp -d); printf 'hello' >\"$d/f\"; \
             verify_sha256 \"$d/f\" {HELLO_SHA256} && echo OK; rm -rf \"$d\""
        ));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("OK"),
            "verify_sha256 must accept a matching file; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn verify_lib_rejects_content_that_does_not_match() {
        // `&&` short-circuits: a non-zero verify means WRONGLY_ACCEPTED is
        // never printed. `|| true` keeps `set -e` from killing the shell so
        // we can observe the absence.
        let out = bash_with_verify_lib(&format!(
            "d=$(mktemp -d); printf 'HELLO' >\"$d/f\"; \
             (verify_sha256 \"$d/f\" {HELLO_SHA256} && echo WRONGLY_ACCEPTED) || true; rm -rf \"$d\""
        ));
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("WRONGLY_ACCEPTED"),
            "verify_sha256 must reject a file whose content does not match the sum"
        );
    }

    // ---------------------------------------------------------------
    // install-firecracker.sh — structural.
    // ---------------------------------------------------------------

    #[test]
    fn firecracker_installer_uses_the_shared_verify_lib() {
        // Rather than carry its own copy of the hasher (a second place to
        // rot), the installer sources the shared verifier.
        let body = read_script(FIRECRACKER_INSTALLER);
        assert!(
            has_command_line(&body, "verify.sh"),
            "install-firecracker.sh must source the shared {VERIFY_LIB}"
        );
    }

    #[test]
    fn firecracker_installer_pins_a_wellformed_sha256_per_arch() {
        // The installer runs on x86_64 or aarch64 (it selects FC_ARCH from
        // `uname -m`), so each arch needs its own recorded tarball sum —
        // exactly the sums upstream publishes as `.sha256.txt`.
        let body = read_script(FIRECRACKER_INSTALLER);
        assert_is_sha256(
            &assigned_value(&body, "FC_SHA256_X86_64"),
            "FC_SHA256_X86_64",
        );
        assert_is_sha256(
            &assigned_value(&body, "FC_SHA256_AARCH64"),
            "FC_SHA256_AARCH64",
        );
    }

    #[test]
    fn firecracker_installer_verifies_before_it_extracts_or_installs() {
        // The order is the whole point: a tarball verified only *after* it is
        // unpacked and the binary installed protects nothing.
        let body = read_script(FIRECRACKER_INSTALLER);
        let verify = first_command_line(&body, "verify_sha256");
        let extract = first_command_line(&body, "tar -xzf");
        let install = first_command_line(&body, "install -m 0755");
        assert!(
            verify < extract,
            "verify_sha256 (line {}) must precede `tar -xzf` (line {})",
            verify + 1,
            extract + 1
        );
        assert!(
            verify < install,
            "verify_sha256 (line {}) must precede `install -m 0755` (line {})",
            verify + 1,
            install + 1
        );
    }

    // ---------------------------------------------------------------
    // phase2-homeserver.sh — structural.
    // ---------------------------------------------------------------

    #[test]
    fn matrix_phase2_pins_a_wellformed_sha256() {
        // A single pin: the script hardcodes one binary variant
        // (haswell/amd64), so one recorded sum is correct.
        let body = read_script(MATRIX_PHASE2);
        assert_is_sha256(&assigned_value(&body, "BIN_SHA256"), "BIN_SHA256");
    }

    #[test]
    fn matrix_phase2_verifies_before_it_installs() {
        let body = read_script(MATRIX_PHASE2);
        let verify = first_command_line(&body, "verify_sha256");
        let install = first_command_line(&body, "install -m 0755");
        assert!(
            verify < install,
            "verify_sha256 (line {}) must precede `install -m 0755` (line {})",
            verify + 1,
            install + 1
        );
    }

    #[test]
    fn matrix_phase2_is_self_contained() {
        // The VPS deployment copies only the phase scripts into `~/`, never
        // the repo, so phase2 cannot `source` the shared verifier — it must
        // define its own. If someone later "DRYs" it by sourcing the repo
        // lib, the deployed script breaks on a box that has no such file.
        let body = read_script(MATRIX_PHASE2);
        // Forbid a *sourced* repo lib, not a mere mention — a comment may
        // legitimately explain why the verifier is duplicated. Anchoring on a
        // command line is the same #479 lesson this module preaches.
        assert!(
            !has_command_line(&body, "verify.sh") && !has_command_line(&body, "guest-kernel.sh"),
            "phase2-homeserver.sh must be self-contained (no repo lib to source on the VPS)"
        );
        assert!(
            has_command_line(&body, "verify_sha256()"),
            "phase2-homeserver.sh must define its own verify_sha256 (it runs standalone on the VPS)"
        );
    }
}
