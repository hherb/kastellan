//! Structural + behavioural pins on the shared guest-kernel fetch
//! (issues #471 and #479), and on the privileged installer that is the
//! only thing allowed to create the kernel.
//!
//! Movement-only split out of `microvm.rs`'s `mod tests`; the assertions,
//! their names and their doc comments are unchanged. These test *bash
//! scripts* rather than this crate's Rust, which is why they separate
//! cleanly from the image/launcher discovery tests next door.
//!
//! Host-independent by design: they run on macOS as well as the DGX, since
//! "does the integrity check reject a bad file" needs no VM.

use super::images::{GUEST_KERNEL_LIB, ROOTFS_IMAGES};
use super::repo_root;
use super::script_scan::code_body;

/// The pin is a *library*: sourcing it must define functions and
/// nothing else. If it ever grew a top-level side effect (a stray
/// `curl`, an `exit`), every one of these tests would break, which
/// is the intended alarm.
fn bash_with_pin(snippet: &str) -> std::process::Output {
    let lib = repo_root().join(GUEST_KERNEL_LIB);
    let script = format!("set -euo pipefail; source '{}'; {snippet}", lib.display());
    let mut cmd = std::process::Command::new("bash");
    cmd.arg("-c").arg(script);
    // Bounded (#690): every shell-out in this module answers to a budget, so a
    // snippet that hangs names itself instead of stalling the sweep.
    kastellan_sandbox::bounded_command::probe_output(
        &mut cmd,
        kastellan_sandbox::bounded_command::PROBE_BUDGET,
    )
    .unwrap_or_else(|e| panic!("the pin snippet did not answer: {e:?}"))
}

/// sha256 of the 5 bytes `hello`, from the standard test vectors.
///
/// Lets the accept/reject paths be exercised against a 5-byte file
/// instead of a 16 MB kernel, so these stay unit tests.
const HELLO_SHA256: &str =
    "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

#[test]
fn kernel_pin_exists_and_sources_cleanly() {
    let lib = repo_root().join(GUEST_KERNEL_LIB);
    assert!(lib.is_file(), "missing the shared kernel pin: {}", lib.display());
    let out = bash_with_pin("true");
    assert!(
        out.status.success(),
        "sourcing the pin must have no side effects; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A recorded sum per supported arch, and an explicit refusal for
/// anything else — never a silently unverified fetch.
#[test]
fn kernel_pin_records_a_sha256_for_both_supported_arches() {
    for arch in ["x86_64", "aarch64"] {
        let out = bash_with_pin(&format!("guest_kernel_sha256 {arch}"));
        let sum = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(out.status.success(), "no recorded sum for {arch}");
        assert_eq!(sum.len(), 64, "{arch} sum is not a sha256: {sum:?}");
        assert!(
            sum.chars().all(|c| c.is_ascii_hexdigit()),
            "{arch} sum is not hex: {sum:?}"
        );
    }
    let out = bash_with_pin("guest_kernel_sha256 mips64 || echo REFUSED");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("REFUSED"),
        "an unsupported arch must refuse, not return an empty sum"
    );
}

#[test]
fn verify_accepts_content_matching_the_expected_sum() {
    let out = bash_with_pin(&format!(
        "d=$(mktemp -d); printf hello >\"$d/f\"; \
         verify_sha256 \"$d/f\" {HELLO_SHA256} && echo OK; rm -rf \"$d\""
    ));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("OK"),
        "a matching file must verify; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The load-bearing negative case: a byte that does not match the
/// recorded sum must fail, and the failure must be loud.
#[test]
fn verify_rejects_content_that_does_not_match() {
    let out = bash_with_pin(&format!(
        "d=$(mktemp -d); printf tampered >\"$d/f\"; \
         verify_sha256 \"$d/f\" {HELLO_SHA256} && echo WRONGLY_ACCEPTED; rm -rf \"$d\""
    ));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("WRONGLY_ACCEPTED"), "tampered content was accepted");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("sha256 mismatch"),
        "a mismatch must say so on stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The gap issue #471 was actually filed for: the old scripts did
/// `[ -f vmlinux ] || curl …`, so a kernel already on disk was
/// reused **unchecked** forever. A pre-existing bad file must now be
/// caught, quarantined, and the build stopped.
///
/// Runs without network: the file exists, so the fetch never starts.
#[test]
fn a_pre_existing_tampered_kernel_is_quarantined_and_fails_closed() {
    let out = bash_with_pin(
        "d=$(mktemp -d); printf 'not a kernel' >\"$d/vmlinux\"; \
         fetch_guest_kernel \"$d\" aarch64 && echo WRONGLY_ACCEPTED; \
         echo \"present=$([ -f \"$d/vmlinux\" ] && echo yes || echo no)\"; \
         echo \"quarantined=$(ls \"$d\"/vmlinux.rejected.* 2>/dev/null | wc -l | tr -d ' ')\"; \
         rm -rf \"$d\"",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("WRONGLY_ACCEPTED"), "a tampered kernel was accepted: {stdout}");
    assert!(
        stdout.contains("present=no"),
        "the rejected kernel must not stay in place for the next build: {stdout}"
    );
    assert!(
        stdout.contains("quarantined=1"),
        "the rejected kernel must be kept aside as evidence: {stdout}"
    );
}

/// Evidence is named by content, so a second bad kernel cannot
/// overwrite what the first one left behind. "What did we almost
/// boot?" is worth much less if only the latest attempt survives.
#[test]
fn a_second_distinct_bad_kernel_does_not_clobber_the_first_as_evidence() {
    let out = bash_with_pin(
        "d=$(mktemp -d); \
         printf 'bad kernel one' >\"$d/vmlinux\"; fetch_guest_kernel \"$d\" aarch64 || true; \
         printf 'bad kernel two' >\"$d/vmlinux\"; fetch_guest_kernel \"$d\" aarch64 || true; \
         echo \"kept=$(ls \"$d\"/vmlinux.rejected.* 2>/dev/null | wc -l | tr -d ' ')\"; \
         rm -rf \"$d\"",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("kept=2"),
        "both rejected kernels must survive as separate evidence: {stdout}"
    );
}

/// Re-running the build against the *same* bad file must not pile up
/// near-identical corpses — content-addressed naming makes the
/// quarantine idempotent.
#[test]
fn re_rejecting_identical_bytes_is_idempotent() {
    let out = bash_with_pin(
        "d=$(mktemp -d); \
         printf 'same bad bytes' >\"$d/vmlinux\"; fetch_guest_kernel \"$d\" aarch64 || true; \
         printf 'same bad bytes' >\"$d/vmlinux\"; fetch_guest_kernel \"$d\" aarch64 || true; \
         echo \"kept=$(ls \"$d\"/vmlinux.rejected.* 2>/dev/null | wc -l | tr -d ' ')\"; \
         rm -rf \"$d\"",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("kept=1"),
        "identical rejected bytes must collapse to one evidence file: {stdout}"
    );
}

/// If the quarantine move itself fails, the function must not claim
/// to have preserved the bytes. It still fails closed either way —
/// the point is that the operator-facing report stays truthful, so
/// nobody goes looking for evidence that was never written.
///
/// Skips under uid 0, where a read-only directory does not actually
/// stop the move. Announced via `eprintln!` rather than silently, in
/// the same spirit as the `[SKIP]` convention the micro-VM e2es use:
/// a check that quietly does nothing is worse than one that fails.
#[test]
fn a_failed_quarantine_is_reported_rather_than_claimed() {
    let out = bash_with_pin(
        "if [ \"$(id -u)\" = 0 ]; then echo ROOT_SKIP; exit 0; fi; \
         d=$(mktemp -d); printf 'not a kernel' >\"$d/vmlinux\"; chmod 500 \"$d\"; \
         fetch_guest_kernel \"$d\" aarch64 && echo WRONGLY_ACCEPTED; \
         chmod 700 \"$d\"; rm -rf \"$d\"",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.contains("ROOT_SKIP") {
        eprintln!(
            "[SKIP] a_failed_quarantine_is_reported_rather_than_claimed: \
             running as root, a read-only dir does not block the move"
        );
        return;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains("WRONGLY_ACCEPTED"), "a tampered kernel was accepted: {stdout}");
    assert!(
        stderr.contains("Could not quarantine"),
        "a failed quarantine must say so, got: {stderr}"
    );
    assert!(
        !stderr.contains("  quarantined:"),
        "must not claim to have quarantined when the move failed: {stderr}"
    );
}

/// Structural pin: the URL lives in the shared file and nowhere
/// else. Eight scripts each holding their own copy is what #475
/// showed goes wrong — and here the drift would be a *silently
/// unverified* download rather than a bad hint.
#[test]
fn kernel_pin_is_the_only_place_the_kernel_url_appears() {
    let root = repo_root();
    for &super::images::RootfsImage { image: rootfs, build_script: script, .. } in ROOTFS_IMAGES {
        let raw = std::fs::read_to_string(root.join(script))
            .unwrap_or_else(|e| panic!("read {script}: {e}"));
        let body = code_body(&raw);
        assert!(
            !body.contains("spec.ccfc.min"),
            "{script} (for {rootfs}) declares its own kernel URL; \
             it must source {GUEST_KERNEL_LIB} instead"
        );
    }
}

/// Every build script must actually *use* the pin. Without this a
/// script could drop its URL (satisfying the test above) and simply
/// stop fetching the kernel at all.
#[test]
fn every_build_script_fetches_through_the_pin() {
    let root = repo_root();
    for &super::images::RootfsImage { image: rootfs, build_script: script, .. } in ROOTFS_IMAGES {
        let raw = std::fs::read_to_string(root.join(script))
            .unwrap_or_else(|e| panic!("read {script}: {e}"));
        // Prose must not satisfy a PRESENCE check: a script that only
        // MENTIONS the pin in a comment while no longer sourcing it would
        // download an unverified kernel and pass (#471, and the
        // [[guard-shares-the-census-blind-spot]] lesson).
        let body = code_body(&raw);
        assert!(
            body.contains("guest-kernel.sh"),
            "{script} (for {rootfs}) does not source {GUEST_KERNEL_LIB}"
        );
        assert!(
            body.contains("require_guest_kernel"),
            "{script} (for {rootfs}) sources the pin but never calls require_guest_kernel"
        );
    }
}

// --- #479: the boot-time pin must not drift from the build-time one ---

/// Pull `NAME="value"` out of the shared bash pin.
///
/// Deliberately strict about the shape: if the assignment is
/// reformatted, this panics rather than quietly matching nothing and
/// letting the comparison below pass vacuously — a test that can
/// silently stop testing is worse than no test.
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

/// #479's other half: the privileged installer must leave the guest
/// kernel where the agent's own OS user cannot replace it.
///
/// Three properties, and the first is the one that was got WRONG on
/// the first attempt — which is exactly why it is asserted rather
/// than commented.
///
/// `unlink(2)` refuses removal from a sticky directory only when the
/// process's UID "is neither the UID of the file to be deleted nor
/// that of the directory containing it". There are **two**
/// exemptions, not one. The original version of this change chowned
/// the directory to the worker user and root-owned only `vmlinux`,
/// which satisfies the *directory-owner* exemption: the agent could
/// still `rm` the kernel and drop in its own, and the whole ownership
/// half was void while looking correct. So the directory `chown` must
/// name **root**, and asserting `chown root:root` somewhere in the
/// file is not enough — the earlier bug passed exactly that check.
#[test]
fn installer_root_owns_the_kernel_in_a_sticky_dir() {
    let script = "scripts/linux/install-firecracker-vsock.sh";
    let body = std::fs::read_to_string(repo_root().join(script))
        .unwrap_or_else(|e| panic!("read {script}: {e}"));

    // Every assertion below anchors to a real command line, not to a
    // bare substring: the version of this test that shipped the
    // original bug used `contains("chown root:root")`, which the
    // BUGGY script also satisfied. A comment must never be able to
    // satisfy a security assertion.
    let cmd = |pred: &dyn Fn(&str) -> bool| -> Option<String> {
        body.lines().map(str::trim).find(|l| !l.starts_with('#') && pred(l)).map(str::to_string)
    };

    // 1. The image dir must be owned by ROOT. unlink(2) exempts the
    //    DIRECTORY's owner as well as the file's, so naming
    //    TARGET_USER here re-opens the hole however vmlinux is owned.
    let dir_chown = cmd(&|l| l.starts_with("chown ") && l.ends_with("\"${MICROVM_DIR}\""))
        .unwrap_or_else(|| panic!("{script} never chowns ${{MICROVM_DIR}} itself"));
    assert!(
        dir_chown.starts_with("chown \"root:") || dir_chown.starts_with("chown root:"),
        "the micro-VM image dir must be owned by ROOT, not the worker. Found: {dir_chown}"
    );

    // 2. And so must its PARENT — unlink/rename permission on the
    //    image dir is governed by the parent, so an agent-owned
    //    /var/lib/kastellan lets the agent swap the whole directory.
    assert!(
        cmd(&|l| l.starts_with("chown root:root") && l.ends_with("\"${MICROVM_PARENT}\""))
            .is_some(),
        "{script} must root-own the PARENT of the image dir too"
    );

    // 3. Sticky + group-writable exactly. 1777 would be world-writable
    //    and is NOT what this ships; accepting it would let a later
    //    edit weaken the dir while keeping this test green.
    assert!(
        cmd(&|l| l.starts_with("chmod 1775") && l.ends_with("\"${MICROVM_DIR}\"")).is_some(),
        "{script} must chmod the image dir 1775 (sticky + group write)"
    );

    // 4. vmlinux itself root-owned, and the pin actually SOURCED —
    //    `contains(\"guest-kernel.sh\")` alone is satisfied by the
    //    `# shellcheck source=...` comment sitting right above it.
    assert!(
        cmd(&|l| l.starts_with("chown root:root") && l.contains("/vmlinux")).is_some(),
        "{script} must leave vmlinux itself root-owned"
    );

    // 5. And root-owned is only half of it: the kernel's MODE is
    //    asserted for the same reason the directory's is. A root-owned
    //    `vmlinux` at 0664 or 0666 can be overwritten IN PLACE — no
    //    unlink, no rename, so neither the sticky bit nor either
    //    ownership assertion above notices — and the ownership half of
    //    #479 is void while this test stays green. That is exactly the
    //    shape of the four bugs this branch already fixed, so read the
    //    bit rather than assuming it.
    assert!(
        cmd(&|l| l.starts_with("chmod 0644") && l.contains("/vmlinux")).is_some(),
        "{script} must chmod vmlinux 0644 — a group/world-writable kernel is \
         replaceable in place however it is owned"
    );

    assert!(
        cmd(&|l| l.starts_with("source ") && l.contains("guest-kernel.sh")).is_some(),
        "{script} must actually source {GUEST_KERNEL_LIB}, not merely mention it"
    );
    assert!(
        cmd(&|l| l.starts_with("fetch_guest_kernel ")).is_some(),
        "{script} is the only thing that may CREATE the kernel — builds only verify"
    );

    // 6. The post-install verification must READ BACK what it just
    //    set, both bits of it, rather than reporting success on the
    //    strength of having run chown/chmod. `stat` here must NOT be
    //    given `-L`: on a symlink planted in the window between the
    //    pre-fetch `[ -L ]` check and the fetch itself, an
    //    undereferenced `%u` reports the agent-owned LINK, which is
    //    what catches that race. `stat -Lc` would follow to the
    //    root-owned target and report success.
    assert!(
        cmd(&|l| l.contains("stat -c '%u'") && l.contains("/vmlinux")).is_some(),
        "{script} must read back the kernel's owner with a NON-dereferencing \
         stat (no -L, or a planted symlink reports its target's uid)"
    );
    assert!(
        cmd(&|l| l.contains("stat -c '%a'") && l.contains("/vmlinux")).is_some(),
        "{script} must read back the kernel's mode after setting it"
    );
    assert!(
        !body.lines().map(str::trim).any(|l| !l.starts_with('#') && l.contains("stat -L")),
        "{script} must not dereference symlinks when reading back the kernel's \
         owner — that is what catches a link planted during the fetch window"
    );

    // 7. ORDER, not just presence. `chown` and `chmod` follow symlinks,
    //    so the post-fetch `[ -L ]` check must sit BETWEEN the fetch and
    //    the chown — otherwise root follows an agent-planted link out of
    //    this directory before anything notices. The first version of
    //    this very fix got that wrong (the check was added *after* the
    //    chown), which is the branch's own signature bug recurring
    //    inside its remedy: reading a permission property instead of
    //    ordering the operations that establish it. A presence-only
    //    assertion would have stayed green through it, so assert the
    //    sequence.
    let line_of = |pred: &dyn Fn(&str) -> bool| -> Option<usize> {
        body.lines().map(str::trim).position(|l| !l.starts_with('#') && pred(l))
    };
    let fetch_at = line_of(&|l| l.starts_with("fetch_guest_kernel "))
        .unwrap_or_else(|| panic!("{script} never calls fetch_guest_kernel"));
    let chown_at = line_of(&|l| l.starts_with("chown root:root") && l.contains("/vmlinux"))
        .unwrap_or_else(|| panic!("{script} never chowns vmlinux"));
    let recheck_at = body
        .lines()
        .map(str::trim)
        .enumerate()
        .find(|(i, l)| *i > fetch_at && !l.starts_with('#') && l.contains("[ -L "))
        .map(|(i, _)| i)
        .unwrap_or_else(|| {
            panic!("{script} never re-checks for a symlink after fetch_guest_kernel")
        });
    assert!(
        fetch_at < recheck_at && recheck_at < chown_at,
        "{script}: the post-fetch symlink re-check must sit between the fetch \
         (line {fetch_at}) and the chown (line {chown_at}), but is at line \
         {recheck_at}. chown/chmod FOLLOW symlinks — checking after them means \
         root has already followed the link out of the image dir."
    );
}

/// #479: a build script must never be able to create the guest
/// kernel.
///
/// The image dir is group-writable so builds can manage their own
/// `*.ext4`, which also means a build CAN create a new entry. So if
/// `vmlinux` were ever absent, a build calling `fetch_guest_kernel`
/// would rename its download into place and leave an **agent-owned**
/// kernel — no unlink of root's file needed, nothing failing, and the
/// ownership half of #479 silently gone from then on. Builds verify
/// (`require_guest_kernel`); only the privileged installer creates.
#[test]
fn build_scripts_verify_the_kernel_but_never_create_it() {
    let root = repo_root();
    for &super::images::RootfsImage { image: rootfs, build_script: script, .. } in ROOTFS_IMAGES {
        let raw = std::fs::read_to_string(root.join(script))
            .unwrap_or_else(|e| panic!("read {script}: {e}"));
        // The same shared comment rule as every other scanner over these
        // files, rather than a fourth private notion of "ignore the prose".
        let body = code_body(&raw);
        let calls = |name: &str| body.lines().map(str::trim).any(|l| l.starts_with(name));
        assert!(
            calls("require_guest_kernel"),
            "{script} (for {rootfs}) must call require_guest_kernel"
        );
        assert!(
            !calls("fetch_guest_kernel"),
            "{script} (for {rootfs}) calls fetch_guest_kernel — an unprivileged build that \
             can CREATE the kernel can create an agent-owned one, voiding #479"
        );
    }
}
