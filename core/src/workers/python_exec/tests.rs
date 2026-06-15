use super::*;

fn ctx<'a>(
    get_env: &'a dyn Fn(&str) -> Option<String>,
    exists: &'a dyn Fn(&Path) -> bool,
) -> ResolveCtx<'a> {
    ResolveCtx {
        get_env,
        exists,
        is_dir: &|_p| false,
        exe_dir: None,
        canonicalize: &|_p| None,
        allowlist: &|_t| Vec::new(),
    }
}

#[test]
fn resolve_disabled_without_enable_gate() {
    let get_env = |k: &str| (k == BIN_ENV).then(|| "/opt/python-exec".to_string());
    let exists = |_p: &Path| true;
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Disabled { detail } => {
            assert!(detail.contains(ENABLE_ENV), "detail: {detail}");
        }
        other => panic!("expected Disabled, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_registers_with_strictest_policy() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    // Only the override binary + the first interpreter candidate exist
    // (the first candidate differs per OS — see PYTHON_CANDIDATES).
    let first = Path::new(PYTHON_CANDIDATES[0]);
    let exists = |p: &Path| p == Path::new("/opt/python-exec") || p == first;
    let c = ctx(&get_env, &exists);

    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(entry.binary, PathBuf::from("/opt/python-exec"));
            assert!(matches!(entry.policy.net, Net::Deny));
            assert_eq!(entry.policy.profile, Profile::WorkerStrict);
            assert_eq!(entry.policy.cpu_ms, 10_000);
            assert_eq!(entry.policy.mem_mb, 512);
            assert_eq!(entry.wall_clock_ms, Some(30_000));
            // No writable host path, ever.
            assert!(entry.policy.fs_write.is_empty());
            // fs_read: worker + interpreter + derived stdlib path
            // (value pins for the derivation live in the dedicated
            // interpreter_extra_fs_read tests below).
            assert!(entry.policy.fs_read.contains(&first.to_path_buf()));
            assert!(entry
                .policy
                .fs_read
                .contains(&interpreter_extra_fs_read(first).expect("candidate has bin parent")));
            // Env: interpreter for the worker's fail-closed startup +
            // the explicit Landlock /tmp grant (jail tmpfs scratch).
            assert!(entry
                .policy
                .env
                .contains(&(PYTHON_ENV.to_string(), PYTHON_CANDIDATES[0].to_string())));
            assert!(entry
                .policy
                .env
                .contains(&(ENV_LANDLOCK_RW.to_string(), r#"["/tmp"]"#.to_string())));
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn python_override_set_but_invalid_fails_closed() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_PYTHON" => Some("/opt/typo/python3".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    // The candidates DO exist — but the explicit override must not be
    // silently substituted.
    let exists = |p: &Path| p != Path::new("/opt/typo/python3");
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Misconfigured { detail } => {
            assert!(detail.contains("/opt/typo/python3"), "detail: {detail}");
            assert!(detail.contains("fail-closed"), "detail: {detail}");
        }
        other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
    }
}

#[test]
fn no_interpreter_anywhere_is_misconfigured() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    let exists = |p: &Path| p == Path::new("/opt/python-exec");
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Misconfigured { detail } => {
            assert!(detail.contains("no python3 interpreter"), "detail: {detail}");
        }
        other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
    }
}

#[test]
fn candidate_cascade_skips_missing_entries() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        _ => None,
    };
    // Host where only /usr/local/bin/python3 exists — the second
    // candidate on BOTH platforms, so this pins the skip-and-continue
    // behaviour portably.
    let exists = |p: &Path| p == Path::new("/usr/local/bin/python3");
    let python = resolve_env(get_env, |p: &Path| exists(p)).expect("resolves");
    assert_eq!(python, PathBuf::from("/usr/local/bin/python3"));
    // And the derived stdlib prefix follows the prefix, not /usr.
    assert_eq!(
        interpreter_extra_fs_read(&python),
        Some(PathBuf::from("/usr/local/lib"))
    );
}

/// `/usr/bin/python3` on macOS is ALWAYS Apple's xcrun shim (SIP owns
/// `/usr/bin`), which cannot run inside the jail — it must never be a
/// candidate there. On Linux it is the primary distro interpreter.
#[test]
fn usr_bin_python_candidacy_is_platform_correct() {
    #[cfg(target_os = "macos")]
    assert!(!PYTHON_CANDIDATES.contains(&"/usr/bin/python3"));
    #[cfg(not(target_os = "macos"))]
    assert_eq!(PYTHON_CANDIDATES[0], "/usr/bin/python3");
}

#[test]
fn interpreter_symlink_is_canonicalized_into_policy_and_env() {
    // /usr/bin/python3 → /etc/alternatives/python3 → /usr/bin/python3.11
    // (update-alternatives layout). The jail binds /usr but NOT
    // /etc/alternatives, so the policy + injected env must carry the
    // canonical target, not the symlink. Exercised via the explicit
    // override so the test is independent of the per-OS candidate list
    // (canonicalization applies identically to both resolve paths).
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_PYTHON" => Some("/usr/bin/python3".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    let exists = |p: &Path| {
        p == Path::new("/opt/python-exec") || p == Path::new("/usr/bin/python3")
    };
    let canonicalize = |p: &Path| {
        (p == Path::new("/usr/bin/python3")).then(|| PathBuf::from("/usr/bin/python3.11"))
    };
    let c = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &|_p| false,
        exe_dir: None,
        canonicalize: &canonicalize,
        allowlist: &|_t| Vec::new(),
    };
    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(entry.policy.fs_read.contains(&PathBuf::from("/usr/bin/python3.11")));
            assert!(
                !entry.policy.fs_read.contains(&PathBuf::from("/usr/bin/python3")),
                "the symlink path must be replaced by its canonical target"
            );
            assert!(entry
                .policy
                .env
                .contains(&(PYTHON_ENV.to_string(), "/usr/bin/python3.11".to_string())));
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn missing_worker_binary_is_misconfigured() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        _ => None,
    };
    let exists = |p: &Path| p == Path::new(PYTHON_CANDIDATES[0]);
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Misconfigured { detail } => {
            assert!(detail.contains(DEFAULT_BIN_NAME), "detail: {detail}");
        }
        other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
    }
}

#[test]
fn interpreter_extra_fs_read_posix_prefix_grants_lib() {
    assert_eq!(
        interpreter_extra_fs_read(Path::new("/usr/bin/python3")),
        Some(PathBuf::from("/usr/lib"))
    );
    assert_eq!(interpreter_extra_fs_read(Path::new("/snap/python3")), None);
}

/// Framework pythons (what every macOS candidate canonicalizes into)
/// keep the interpreter dylib at `<version-root>/Python` — a sibling
/// of `bin/` and `lib/` — so the grant must be the version root.
#[test]
fn interpreter_extra_fs_read_framework_grants_version_root() {
    // python.org installer layout.
    assert_eq!(
        interpreter_extra_fs_read(Path::new(
            "/Library/Frameworks/Python.framework/Versions/3.13/bin/python3.13"
        )),
        Some(PathBuf::from("/Library/Frameworks/Python.framework/Versions/3.13"))
    );
    // Apple-Silicon Homebrew Cellar layout.
    assert_eq!(
        interpreter_extra_fs_read(Path::new(
            "/opt/homebrew/Cellar/python@3.14/3.14.5/Frameworks/Python.framework/Versions/3.14/bin/python3.14"
        )),
        Some(PathBuf::from(
            "/opt/homebrew/Cellar/python@3.14/3.14.5/Frameworks/Python.framework/Versions/3.14"
        ))
    );
    // Command Line Tools layout (note: Python3.framework).
    assert_eq!(
        interpreter_extra_fs_read(Path::new(
            "/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9/bin/python3.9"
        )),
        Some(PathBuf::from(
            "/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9"
        ))
    );
}

fn outcome_label(r: &Resolution) -> &'static str {
    match r {
        Resolution::Register(_) => "Register",
        Resolution::Disabled { .. } => "Disabled",
        Resolution::Misconfigured { .. } => "Misconfigured",
    }
}

// ---- issue #284: out-of-prefix interpreter shared-lib dirs ----

/// A POSIX interpreter (`/px/bin/python3.12`, so `interpreter_extra_fs_read`
/// ⇒ `/px/lib`) that links a Homebrew `libintl` outside `/px/lib` must have
/// that dir surfaced for binding.
#[test]
fn interpreter_extra_lib_dirs_binds_out_of_prefix_dep() {
    let exists = |_p: &Path| false; // no libpython seed on disk
    let canon = |p: &Path| Some(p.to_path_buf());
    let deps = |p: &Path| {
        if p == Path::new("/px/bin/python3.12") {
            vec![PathBuf::from("/opt/hb/gettext/lib/libintl.8.dylib")]
        } else {
            vec![]
        }
    };
    let dirs =
        interpreter_extra_lib_dirs(Path::new("/px/bin/python3.12"), &exists, &canon, &deps);
    assert_eq!(dirs, vec![PathBuf::from("/opt/hb/gettext/lib")]);
}

/// A dep that lives under the already-bound `interpreter_extra_fs_read`
/// region (`/px/lib`) is NOT re-bound — it's reachable in-jail already.
#[test]
fn interpreter_extra_lib_dirs_skips_in_prefix_dep() {
    let exists = |_p: &Path| false;
    let canon = |p: &Path| Some(p.to_path_buf());
    let deps = |p: &Path| {
        if p == Path::new("/px/bin/python3.12") {
            vec![PathBuf::from("/px/lib/libpython3.12.dylib")]
        } else {
            vec![]
        }
    };
    let dirs =
        interpreter_extra_lib_dirs(Path::new("/px/bin/python3.12"), &exists, &canon, &deps);
    assert!(dirs.is_empty(), "in-prefix (/px/lib) deps must not be bound, got {dirs:?}");
}

/// `python_exec_entry` binds the supplied lib dirs in `fs_read`; an empty vec
/// is byte-identical to the pre-#284 behaviour (worker + interpreter + stdlib
/// only).
#[test]
fn entry_binds_interpreter_lib_dirs() {
    let with = python_exec_entry(
        PathBuf::from("/opt/python-exec"),
        PathBuf::from("/usr/local/bin/python3"),
        vec![PathBuf::from("/opt/hb/gettext/lib")],
    );
    assert!(with
        .policy
        .fs_read
        .contains(&PathBuf::from("/opt/hb/gettext/lib")));

    let bare = python_exec_entry(
        PathBuf::from("/opt/python-exec"),
        PathBuf::from("/usr/local/bin/python3"),
        vec![],
    );
    assert!(!bare
        .policy
        .fs_read
        .contains(&PathBuf::from("/opt/hb/gettext/lib")));
    // Worker + interpreter + derived stdlib still bound on the empty path.
    assert!(bare
        .policy
        .fs_read
        .contains(&PathBuf::from("/usr/local/bin/python3")));
    assert!(bare.policy.fs_read.contains(&PathBuf::from("/usr/local/lib")));
}

/// An interpreter with no derivable prefix region (`interpreter_extra_fs_read`
/// ⇒ `None`, e.g. a bare `/snap/python3` with no `bin/` parent) falls back to
/// the binary path as the walk-prefix. Nothing lies *under* a file path, so
/// every non-system dep is bound — the safe over-approximation the doc comment
/// promises.
#[test]
fn interpreter_extra_lib_dirs_no_prefix_falls_back_to_binary_path() {
    // `/snap/python3` has no `bin/` parent ⇒ interpreter_extra_fs_read = None.
    assert_eq!(interpreter_extra_fs_read(Path::new("/snap/python3")), None);
    let exists = |_p: &Path| false;
    let canon = |p: &Path| Some(p.to_path_buf());
    let deps = |p: &Path| {
        if p == Path::new("/snap/python3") {
            vec![PathBuf::from("/snap/lib/libintl.8.dylib")]
        } else {
            vec![]
        }
    };
    let dirs = interpreter_extra_lib_dirs(Path::new("/snap/python3"), &exists, &canon, &deps);
    assert_eq!(dirs, vec![PathBuf::from("/snap/lib")]);
}
