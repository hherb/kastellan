//! Preflight tests for the micro-VM e2e guards: the gate ordering, the
//! launcher probe order, the operator-facing hint wording, and #667's
//! image-freshness enforcement.
//!
//! Split out of `mod.rs` in the change that added the freshness gate — the
//! file was over the tree's 500-line guideline with the tests inline.

use super::*;

/// The regression this module was filed for: the launcher is probed
/// release-first, so every operator-facing hint must say `--release`.
/// Two of the 15 original copies said plain `cargo build -p …`,
/// which rebuilds debug and leaves a stale release binary running.
#[test]
fn release_is_probed_before_debug() {
    let candidates = launcher_candidates(Path::new("/ws/target"));
    assert_eq!(
        candidates,
        vec![
            PathBuf::from("/ws/target/release").join(LAUNCHER_BIN),
            PathBuf::from("/ws/target/debug").join(LAUNCHER_BIN),
        ]
    );
}

#[test]
fn launcher_hint_says_release() {
    let msg = launcher_skip_message();
    assert!(msg.contains("--release"), "hint must say --release, got: {msg}");
    assert!(msg.contains("[SKIP]"), "must be greppable as a skip: {msg}");
}

#[test]
fn probe_message_names_the_rootfs_and_its_build_script() {
    let msg = probe_skip_message("web-fetch.ext4", "no /dev/kvm");
    assert!(msg.contains("web-fetch.ext4"), "must name the image: {msg}");
    assert!(msg.contains("no /dev/kvm"), "must carry the cause: {msg}");
    assert!(msg.contains("build-web-fetch-rootfs.sh"), "must point at the builder: {msg}");
}

#[test]
fn probe_message_omits_the_build_hint_for_an_unknown_rootfs() {
    let msg = probe_skip_message("mystery.ext4", "boom");
    assert!(msg.contains("mystery.ext4"));
    assert!(!msg.contains("build the rootfs with"), "must not invent a script: {msg}");
}

/// The probe checks more than the three things the parenthetical names, so
/// the probe's OWN error must lead — a reader who stops at the parenthetical
/// would go hunting for a KVM problem that is really a missing `firecracker`.
#[test]
fn the_probe_reason_leads_with_the_actual_cause() {
    let reason = probe_reason("web-fetch.ext4", "firecracker not found on PATH");
    let cause = reason.find("firecracker not found on PATH").expect("carries the cause");
    let hedge = reason.find("needs").expect("carries the hedge");
    assert!(cause < hedge, "the real cause must precede the generic list: {reason}");
}

// ---------------------------------------------------------------
// The gate ORDERING (#680 review, finding 3).
//
// This used to live only inside `#[cfg(target_os = "linux")]`, where
// replacing the freshness call with `false` left every test green —
// and on macOS the mutation could not even be attempted, because the
// code compiles out. `preflight` is pure over injected closures, so
// the order is now provable on every host.
// ---------------------------------------------------------------

/// Records which preflight steps ran, in order.
#[derive(Default)]
struct Trace {
    steps: std::cell::RefCell<Vec<&'static str>>,
}

impl Trace {
    fn mark(&self, step: &'static str) {
        self.steps.borrow_mut().push(step);
    }
    fn steps(&self) -> Vec<&'static str> {
        self.steps.borrow().clone()
    }
}

/// A probe failure must short-circuit: a host with no KVM must not pay
/// for a launcher lookup or a digest it cannot use.
#[test]
fn a_failed_probe_skips_everything_after_it() {
    let t = Trace::default();
    let skipped = preflight(
        "kv-demo.ext4",
        || Err("no /dev/kvm".to_string()),
        || {
            t.mark("locate");
            None
        },
        |_| t.mark("path"),
        || {
            t.mark("stale");
            false
        },
        |_| {
            t.mark("unmet");
            true
        },
    );
    assert!(skipped, "an unmet precondition must skip");
    assert_eq!(t.steps(), vec!["unmet"], "nothing after the probe may run");
}

/// An unbuilt launcher must short-circuit before the freshness check,
/// and must NOT touch `PATH` — there is no directory to prepend.
#[test]
fn a_missing_launcher_skips_the_freshness_check() {
    let t = Trace::default();
    let skipped = preflight(
        "kv-demo.ext4",
        || Ok(()),
        || None,
        |_| t.mark("path"),
        || {
            t.mark("stale");
            false
        },
        |_| {
            t.mark("unmet");
            true
        },
    );
    assert!(skipped);
    assert_eq!(t.steps(), vec!["unmet"], "no PATH mutation, no digest");
}

/// The load-bearing ordering test. With both host gates passed, the
/// launcher is made reachable and THEN the image is checked — and the
/// freshness verdict is what the caller sees. Deleting the freshness
/// call (the mutation that silently restores #667) fails here.
#[test]
fn a_usable_host_prepends_path_then_checks_freshness() {
    let t = Trace::default();
    let seen = std::cell::RefCell::new(PathBuf::new());
    let skipped = preflight(
        "kv-demo.ext4",
        || Ok(()),
        || Some(PathBuf::from("/ws/target/release").join(LAUNCHER_BIN)),
        |bin| {
            t.mark("path");
            *seen.borrow_mut() = bin.to_path_buf();
        },
        || {
            t.mark("stale");
            true
        },
        |_| {
            t.mark("unmet");
            false
        },
    );
    assert_eq!(t.steps(), vec!["path", "stale"], "PATH first, then the image check");
    assert!(skipped, "the freshness verdict must be what the caller sees");
    assert_eq!(
        *seen.borrow(),
        PathBuf::from("/ws/target/release").join(LAUNCHER_BIN),
        "the located launcher must be the one made reachable"
    );
}

// ---------------------------------------------------------------
// The digest seams (#680 review, finding 4). Seven mutations to this
// layer used to survive the whole suite.
// ---------------------------------------------------------------

/// `KASTELLAN_MICROVM_DIR` decides which image is BOOTED and, since
/// #667, which image is HASHED. An empty or whitespace-only override
/// must fall back rather than resolve `"" + "/kv-demo.ext4"` — which
/// would hash a path in the filesystem root and report every image
/// unreadable.
#[test]
fn a_blank_image_dir_override_falls_back_to_the_default() {
    let _lock = crate::env_lock();
    for blank in ["", "   ", "\t"] {
        let _g = crate::EnvVarGuard::set("KASTELLAN_MICROVM_DIR", blank);
        assert_eq!(image_dir(), DEFAULT_IMAGE_DIR, "{blank:?} must fall back");
    }
    let _g = crate::EnvVarGuard::set("KASTELLAN_MICROVM_DIR", "/custom/dir");
    assert_eq!(image_dir(), "/custom/dir", "a real override must be honoured");
}

/// The reference is `target/release/`, because that is literally the
/// path every `build-*-rootfs.sh` copies out of. `target/debug` holds a
/// binary no image is ever built from, so comparing against it would
/// compare the image to something it could never contain.
#[test]
fn the_freshness_reference_is_the_release_profile() {
    let path = release_binary_path(Path::new("/ws/target"), GUEST_INIT_BIN);
    assert_eq!(path, PathBuf::from("/ws/target/release").join(GUEST_INIT_BIN));
}

/// The argv must read the IN-IMAGE path, not the target filename. The
/// two differ for exactly the binary present in all eight images — the
/// init is renamed to `/sbin/init` — so this mutation would send
/// `debugfs` hunting for a path no image contains and yield
/// `Indeterminate` forever: a check that silently stops checking.
#[test]
fn the_debugfs_argv_reads_the_in_image_path() {
    let argv = debugfs_argv(Path::new("/img/kv-demo.ext4"), GUEST_INIT_IN_IMAGE);
    assert_eq!(
        argv,
        vec![
            "-R".to_string(),
            format!("cat {GUEST_INIT_IN_IMAGE}"),
            "/img/kv-demo.ext4".to_string(),
        ]
    );
    assert!(!argv.iter().any(|a| a.contains(GUEST_INIT_BIN)), "must not use the target name");
}

/// `cat` writes to stdout, which is the whole reason there is no temp
/// file: the first version wrote `dump <path> <outfile>` into `$TMPDIR`,
/// and `debugfs` splits the `-R` string on whitespace, so a `$TMPDIR`
/// containing a space produced a malformed request and a permanent
/// `Indeterminate` for every image on that host.
#[test]
fn the_debugfs_argv_names_no_output_path() {
    let argv = debugfs_argv(Path::new("/img/kv-demo.ext4"), "/sbin/init");
    assert_eq!(argv[1], "cat /sbin/init", "one operand, so no path can need quoting: {argv:?}");
}

/// `debugfs` announces its version on stderr before saying anything
/// useful, so the banner must not become the operator's diagnosis.
#[test]
fn the_debugfs_complaint_drops_the_version_banner() {
    let stderr = b"debugfs 1.47.0 (5-Feb-2023)\n/sbin/nope: File not found by ext2_lookup \n";
    assert_eq!(debugfs_complaint(stderr), "/sbin/nope: File not found by ext2_lookup");
}

/// A reader that says nothing at all must still produce a sentence —
/// an empty detail would render as an empty parenthetical.
#[test]
fn the_debugfs_complaint_is_never_empty() {
    assert!(!debugfs_complaint(b"").is_empty());
    assert!(!debugfs_complaint(b"debugfs 1.47.0 (5-Feb-2023)\n").is_empty());
}

/// The banner test must not be pinned to a version, or the next
/// e2fsprogs release becomes somebody's diagnosis. `debugfs`'s own
/// complaints use a colon, so the space is what distinguishes them.
#[test]
fn the_debugfs_complaint_drops_a_future_version_banner_too() {
    let stderr = b"debugfs 2.0.1 (1-Jan-2030)\ncat: Filesystem not open\n";
    assert_eq!(debugfs_complaint(stderr), "cat: Filesystem not open");
    // ...but a real `debugfs:`-prefixed diagnostic must survive.
    assert_eq!(debugfs_complaint(b"debugfs: boom\n"), "debugfs: boom");
}

// ---------------------------------------------------------------
// #667: the freshness verdict must reach the operator as an ACTION,
// not merely as a correctly-worded string.
//
// Every test below drives `skip_if_image_stale_to` rather than
// asserting on `stale_reason` alone. That distinction is the whole
// point: a wording-only assertion leaves the `panic!` deletable with
// the suite still green, which is exactly the mutation that would
// silently restore #667.
// ---------------------------------------------------------------

/// A fully-verified image.
fn fresh() -> Freshness {
    Freshness::Fresh { unverified: vec![] }
}

/// An image that matched everywhere it could be checked, with one
/// binary left unchecked — the shape that used to pass in silence.
fn partly_verified() -> Freshness {
    Freshness::Fresh {
        unverified: vec![Unverified {
            binary: "kastellan-worker-kv-demo".to_string(),
            why: Missing::NotBuilt,
        }],
    }
}

/// An `Indeterminate` verdict with a realistic benign cause.
fn indeterminate() -> Freshness {
    Freshness::Indeterminate {
        unverified: vec![Unverified {
            binary: GUEST_INIT_BIN.to_string(),
            why: Missing::NotBuilt,
        }],
    }
}

/// Hold the env lock and force [`REQUIRE_ENV`] to a known state, so
/// these tests cannot race the ambient value on an operator's host.
fn with_require<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _lock = crate::env_lock();
    let _guard = match value {
        Some(v) => crate::EnvVarGuard::set(REQUIRE_ENV, v),
        None => crate::EnvVarGuard::unset(REQUIRE_ENV),
    };
    f()
}

/// Run `f`, returning the panic message.
fn panic_message(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let payload = std::panic::catch_unwind(f).expect_err("must panic");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_string())
}

#[test]
fn a_fresh_image_does_not_skip_and_says_nothing() {
    let mut out = Vec::new();
    let skipped =
        with_require(None, || skip_if_image_stale_to("kv-demo.ext4", fresh(), &mut out));
    assert!(!skipped, "a fresh image must run");
    assert!(out.is_empty(), "a fresh image must be silent, got {:?}", String::from_utf8(out));
}

/// The load-bearing branch. A stale image must STOP the run, not skip
/// it: a `[SKIP]` here would be #667 wearing a different hat, since a
/// skip reports green.
#[test]
#[should_panic(expected = "DIFFERS")]
fn a_stale_image_panics_rather_than_skipping() {
    let mut out = Vec::new();
    with_require(None, || {
        skip_if_image_stale_to(
            "kv-demo.ext4",
            Freshness::Stale { binary: GUEST_INIT_BIN.to_string() },
            &mut out,
        )
    });
}

/// ...and it must panic even with the REQUIRE knob explicitly OFF.
/// Staleness is not an unmet precondition an operator may reasonably
/// lack; it is positive evidence that the run would prove nothing.
#[test]
#[should_panic(expected = "DIFFERS")]
fn a_stale_image_panics_even_when_require_is_off() {
    let mut out = Vec::new();
    with_require(Some("0"), || {
        skip_if_image_stale_to(
            "kv-demo.ext4",
            Freshness::Stale { binary: GUEST_INIT_BIN.to_string() },
            &mut out,
        )
    });
}

/// The panic must carry the rebuild command. An operator who has just
/// been told their gate is invalid wants the one line that fixes it,
/// and the two build-script directories are what makes assembling it
/// by hand error-prone.
#[test]
fn the_stale_panic_names_the_rebuild_command() {
    // Under the env lock like its siblings: the Stale arm is
    // env-independent, and that is exactly the invariant under test, so
    // it must not be assumed while proving it.
    let msg = with_require(None, || {
        panic_message(|| {
            let mut sink = Vec::new();
            skip_if_image_stale_to(
                "kv-demo.ext4",
                Freshness::Stale { binary: GUEST_INIT_BIN.to_string() },
                &mut sink,
            );
        })
    });
    assert!(msg.contains("build-kv-demo-rootfs.sh"), "must name this image's script: {msg}");
    assert!(msg.contains(REBUILD_ALL_SCRIPT), "must offer the rebuild-all route: {msg}");
}

/// An image a working reader cannot read is positive evidence about
/// THIS image, so it must stop the run like a stale one — the first
/// version warned and booted it (#680 review).
#[test]
fn an_unusable_image_panics_rather_than_running() {
    let msg = with_require(Some("0"), || {
        panic_message(|| {
            let mut sink = Vec::new();
            skip_if_image_stale_to(
                "kv-demo.ext4",
                Freshness::Unusable {
                    binary: GUEST_INIT_BIN.to_string(),
                    detail: "Filesystem not open".to_string(),
                },
                &mut sink,
            );
        })
    });
    assert!(msg.contains("Filesystem not open"), "must carry the reader's words: {msg}");
    assert!(!msg.contains("e2fsprogs"), "must not blame the wrong thing: {msg}");
    assert!(msg.contains("build-kv-demo-rootfs.sh"), "must give the remedy: {msg}");
}

/// A partly-verified image must WARN. It used to pass in silence, so a
/// matching init certified a worker nothing had compared — #667
/// restored for the worker half of every image.
#[test]
fn a_partly_verified_image_warns_and_still_runs() {
    let mut out = Vec::new();
    let skipped =
        with_require(None, || skip_if_image_stale_to("kv-demo.ext4", partly_verified(), &mut out));
    assert!(!skipped, "it matched everywhere it could — it must still run");
    let rendered = String::from_utf8(out).expect("utf8");
    assert!(rendered.contains("[WARN]"), "the caveat must be visible: {rendered}");
    assert!(rendered.contains("kastellan-worker-kv-demo"), "must name it: {rendered}");
    assert!(!rendered.contains("[SKIP]"), "a test that ran is not a skip: {rendered}");
}

/// ...and under REQUIRE it fails, because a partly-gated run is not the
/// fully-gated run the operator demanded.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn a_partly_verified_image_fails_under_require() {
    let mut out = Vec::new();
    with_require(Some("1"), || {
        skip_if_image_stale_to("kv-demo.ext4", partly_verified(), &mut out)
    });
}

/// Indeterminate must RUN, not skip: absence of a reference binary is
/// not evidence of staleness, and downgrading a real VM run to a skip
/// would lose coverage for nothing.
#[test]
fn an_indeterminate_verdict_warns_but_still_runs() {
    let mut out = Vec::new();
    let skipped =
        with_require(None, || skip_if_image_stale_to("kv-demo.ext4", indeterminate(), &mut out));
    assert!(!skipped, "must still run — the image may be perfectly current");
    let rendered = String::from_utf8(out).expect("utf8");
    assert!(rendered.contains("[WARN]"), "the caveat must be visible: {rendered}");
    assert!(
        !rendered.contains("[SKIP]"),
        "must NOT inflate the skip count for a test that ran: {rendered}"
    );
}

/// ...but an operator demanding a fully-gated run gets a failure
/// instead. This is the half that lets CI insist the check applied.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn an_indeterminate_verdict_fails_under_require() {
    let mut out = Vec::new();
    with_require(Some("1"), || {
        skip_if_image_stale_to("kv-demo.ext4", indeterminate(), &mut out)
    });
}

/// The knob reads the one project flag dialect, not a strict
/// `Some("1")` — the operator-facing skew #654 was filed about. A
/// `kastellan.env` saying `=true` must demand, not silently skip.
#[test]
fn the_require_knob_speaks_the_project_flag_dialect() {
    use crate::gliner_e2e::UnmetAction;
    for truthy in ["1", "true", "TRUE", "yes", "on", "  true  "] {
        assert_eq!(
            with_require(Some(truthy), require_action),
            UnmetAction::Fail,
            "{truthy:?} must demand a real run"
        );
    }
    for falsey in ["0", "false", "no", "off", ""] {
        assert_eq!(
            with_require(Some(falsey), require_action),
            UnmetAction::Skip,
            "{falsey:?} must stay an opt-out"
        );
    }
    assert_eq!(with_require(None, require_action), UnmetAction::Skip, "unset is the default");
}

/// A value OUTSIDE the dialect must say so. It degrades to Skip, which
/// hands back exactly the green run the operator was trying to rule
/// out — the #654 skew, silently, on the knob whose own doc cites #654
/// (#680 review).
#[test]
fn an_out_of_dialect_require_value_warns() {
    let mut out = Vec::new();
    let action = with_require(Some("y"), || require_action_to(&mut out));
    assert_eq!(action, crate::gliner_e2e::UnmetAction::Skip, "out of dialect is not truthy");
    let rendered = String::from_utf8(out).expect("utf8");
    assert!(rendered.contains("[WARN]"), "the degradation must be visible: {rendered}");
    assert!(rendered.contains(REQUIRE_ENV), "must name the knob: {rendered}");
}

/// ...while a value IN the dialect stays silent, or every run would
/// carry a warning nobody needs.
#[test]
fn an_in_dialect_require_value_is_silent() {
    for quiet in ["1", "true", "0", "off"] {
        let mut out = Vec::new();
        with_require(Some(quiet), || require_action_to(&mut out));
        assert!(out.is_empty(), "{quiet:?} must not warn, got {:?}", String::from_utf8(out));
    }
    let mut out = Vec::new();
    with_require(None, || require_action_to(&mut out));
    assert!(out.is_empty(), "unset must not warn, got {:?}", String::from_utf8(out));
}

/// An unmet precondition still skips-as-passes by default, which is
/// what keeps a plain `cargo test` green on a host with no KVM. The
/// line must actually be EMITTED, not merely renderable.
#[test]
fn an_unmet_precondition_emits_a_skip_line_by_default() {
    let mut out = Vec::new();
    let skipped =
        with_require(None, || report_unmet_microvm_to("no /dev/kvm on this host", &mut out));
    assert!(skipped, "the caller must return");
    let rendered = String::from_utf8(out).expect("utf8");
    assert!(rendered.contains("[SKIP]"), "must be greppable: {rendered}");
    assert!(rendered.contains("no /dev/kvm"), "must carry the reason: {rendered}");
}

/// The documented false green this knob exists for: `firecracker` off
/// the non-interactive ssh PATH made the whole suite skip-as-pass.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn an_unmet_precondition_fails_under_require() {
    let mut out = Vec::new();
    with_require(Some("1"), || report_unmet_microvm_to("firecracker not on PATH", &mut out));
}

/// The two panicking-under-REQUIRE call sites used to spell the same
/// sentence out twice, with only the env-var name pinned — so the rest
/// could drift silently. One renderer now, and this is what says so.
#[test]
fn both_require_failures_render_the_same_sentence() {
    let from_unmet = with_require(Some("1"), || {
        panic_message(|| {
            let mut sink = Vec::new();
            report_unmet_microvm_to("a reason", &mut sink);
        })
    });
    let direct = panic_message(|| require_panic("a reason"));
    assert_eq!(from_unmet, direct, "one renderer, one sentence");
}

// ---------------------------------------------------------------
// The registry is only a reference if the images the suites actually
// boot are IN it (#680 review, finding 5).
// ---------------------------------------------------------------

/// Every rootfs an e2e boots must be registered, or its freshness check
/// silently does nothing: an unregistered image has no baked-binary
/// list, so the verdict is `Indeterminate` — a `[WARN]` on a run that
/// proceeds ungated. The newest suite, the one most likely to be gating
/// a fresh change, would be the one running unchecked.
#[test]
fn every_rootfs_an_e2e_boots_is_registered() {
    let dir = repo_root().join("core/tests");
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).expect("core/tests must exist") {
        let path = entry.expect("dirent").path();
        if path.extension().map_or(true, |e| e != "rs") {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("read test source");
        for (i, m) in body.match_indices("VM_ROOTFS: &str = \"") {
            let rest = &body[i + m.len()..];
            let name = &rest[..rest.find('"').expect("closing quote")];
            assert!(
                image_entry(name).is_some(),
                "{} boots {name}, which no registry entry knows — its freshness check \
                 would silently do nothing (#667)",
                path.display()
            );
            checked += 1;
        }
    }
    // A scan that matched nothing would pass vacuously, which is the
    // failure mode this whole module exists to refuse.
    assert!(checked >= 8, "expected to find the micro-VM suites, matched only {checked}");
}

/// `image` is resolved against `image_dir()` at runtime and
/// `build_script` against the repo root; nothing in their shared type
/// says so. A directory component in `image` would compile, then miss
/// the exact-string lookup and yield `Indeterminate` forever.
#[test]
fn an_image_is_a_bare_filename_and_a_script_is_repo_relative() {
    for entry in ROOTFS_IMAGES {
        assert!(
            !entry.image.contains('/'),
            "{} must be a bare filename — it is joined onto image_dir()",
            entry.image
        );
        assert!(
            entry.build_script.contains('/'),
            "{} must be repo-relative — it is joined onto the repo root",
            entry.build_script
        );
    }
}

/// The rebuild-all script every staleness message points at must
/// exist, for the same reason `every_build_script_exists` pins the
/// per-image ones: a hint naming a nonexistent path is worse than no
/// hint, and this one is quoted on the failure path an operator only
/// ever reads when something is already wrong.
#[test]
fn the_rebuild_all_script_exists_and_is_executable() {
    let path = repo_root().join(REBUILD_ALL_SCRIPT);
    assert!(path.is_file(), "missing {REBUILD_ALL_SCRIPT}: {}", path.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert!(mode & 0o111 != 0, "{REBUILD_ALL_SCRIPT} is not executable (mode {mode:o})");
    }
}

/// The rebuild-all script must actually drive every image in the
/// registry, and it must drive them from its `IMAGES` table rather than
/// merely mentioning them: a `body.contains` over the whole file is
/// satisfied by a path surviving only in a comment after its entry was
/// deleted.
#[test]
fn the_rebuild_all_script_covers_every_registered_image() {
    let body = std::fs::read_to_string(repo_root().join(REBUILD_ALL_SCRIPT))
        .unwrap_or_else(|e| panic!("read {REBUILD_ALL_SCRIPT}: {e}"));
    let table = images_table(&body);
    for entry in ROOTFS_IMAGES {
        let stem = entry.image.strip_suffix(".ext4").expect("images are .ext4");
        assert!(
            table.iter().any(|(s, script)| s == stem && script == entry.build_script),
            "{REBUILD_ALL_SCRIPT}'s IMAGES table has no {stem}:{} entry (for {})",
            entry.build_script,
            entry.image
        );
    }
    // And the reverse: an entry the registry does not know would be
    // rebuilt by a command the freshness check never consults.
    for (stem, script) in &table {
        let image = format!("{stem}.ext4");
        let known = image_entry(&image)
            .unwrap_or_else(|| panic!("{REBUILD_ALL_SCRIPT} builds {image}, which is not registered"));
        assert_eq!(known.build_script, *script, "the two tables disagree for {image}");
    }
}

/// Parse the `IMAGES=( "stem:script" … )` array out of the rebuild-all
/// script. Deliberately strict: a shape it cannot parse yields an empty
/// table, and the caller's `assert!` then fails loudly rather than
/// passing vacuously.
fn images_table(body: &str) -> Vec<(String, String)> {
    let start = body.find("IMAGES=(").expect("IMAGES array must exist");
    let rest = &body[start..];
    let end = rest.find("\n)").expect("IMAGES array must be closed");
    rest[..end]
        .lines()
        .filter_map(|l| l.trim().strip_prefix('"')?.strip_suffix('"'))
        .filter_map(|e| e.split_once(':'))
        .map(|(s, script)| (s.to_string(), script.to_string()))
        .collect()
}
