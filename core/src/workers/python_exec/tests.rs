use super::*;

// `Net`/`Profile`/`ENV_LANDLOCK_RW`/`interpreter_extra_fs_read` are referenced
// bare on both platforms below. The #363 lift moved the entry builders (and
// these imports) into `entries`, so the parent no longer re-exports the
// sandbox/scheduler types — pull them in here.
use crate::tool_host::ENV_LANDLOCK_RW;
use kastellan_sandbox::{Net, Profile};
use super::entries::interpreter_extra_fs_read;
// The container/warm-idle entry-internals are exercised only by the
// `#[cfg(target_os = "macos")]` tests, so gate their imports to match (else
// they read as unused imports under `-D warnings` on Linux).
#[cfg(target_os = "macos")]
use crate::scheduler::ToolEntry;
#[cfg(target_os = "macos")]
use super::entries::{
    DEFAULT_MAX_AGE_SECONDS, DEFAULT_MAX_REQUESTS, IDLE_GRACE_SECONDS, IDLE_SECONDS_ENV,
    MAX_AGE_SECONDS_ENV, MAX_REQUESTS_ENV,
};

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

#[test]
fn python_exec_entry_opts_into_ephemeral_scratch() {
    let e = python_exec_entry(
        std::path::PathBuf::from("/bin/worker"),
        std::path::PathBuf::from("/usr/bin/python3"),
        vec![],
        None,
    );
    assert!(e.ephemeral_scratch, "python-exec must request per-spawn scratch");
}

#[test]
fn entry_injects_params_file_max_when_set() {
    let entry = super::python_exec_entry(
        std::path::PathBuf::from("/bin/worker"),
        std::path::PathBuf::from("/usr/bin/python3"),
        vec![],
        Some("250000".to_string()),
    );
    let got = entry
        .policy
        .env
        .iter()
        .find(|(k, _)| k == "KASTELLAN_PYTHON_PARAMS_FILE_MAX")
        .map(|(_, v)| v.as_str());
    assert_eq!(got, Some("250000"));
}

#[test]
fn entry_omits_params_file_max_when_unset() {
    let entry = super::python_exec_entry(
        std::path::PathBuf::from("/bin/worker"),
        std::path::PathBuf::from("/usr/bin/python3"),
        vec![],
        None,
    );
    assert!(
        !entry
            .policy
            .env
            .iter()
            .any(|(k, _)| k == "KASTELLAN_PYTHON_PARAMS_FILE_MAX"),
        "unset → env must stay byte-identical (no file-max key)"
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
        None,
    );
    assert!(with
        .policy
        .fs_read
        .contains(&PathBuf::from("/opt/hb/gettext/lib")));

    let bare = python_exec_entry(
        PathBuf::from("/opt/python-exec"),
        PathBuf::from("/usr/local/bin/python3"),
        vec![],
        None,
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

// ---- container mode (macOS micro-VM) ----

/// Container-mode entry carries the Container backend tag + image, points
/// `binary` at the in-image worker, injects the in-image interpreter path,
/// preserves the strict policy, and binds NO host paths (code rides stdin,
/// scratch is the in-VM /tmp tmpfs).
#[cfg(target_os = "macos")]
#[test]
fn container_mode_entry_shape() {
    let entry = container_mode_entry(
        PathBuf::from(CONTAINER_WORKER_BIN),
        "kastellan/python-exec:dev".to_string(),
        None,
        crate::worker_lifecycle::Lifecycle::SingleUse,
    );
    assert_eq!(
        entry.sandbox_backend,
        Some(kastellan_sandbox::SandboxBackendKind::Container)
    );
    assert_eq!(
        entry.container_image.as_deref(),
        Some("kastellan/python-exec:dev")
    );
    assert_eq!(entry.binary, PathBuf::from(CONTAINER_WORKER_BIN));
    // Strict policy preserved.
    assert!(matches!(entry.policy.net, Net::Deny));
    assert_eq!(entry.policy.profile, Profile::WorkerStrict);
    assert_eq!(entry.policy.mem_mb, 512);
    assert_eq!(entry.policy.cpu_ms, 10_000);
    assert_eq!(entry.wall_clock_ms, Some(30_000));
    // No host binds in container mode.
    assert!(entry.policy.fs_read.is_empty(), "no host fs_read in container mode");
    assert!(entry.policy.fs_write.is_empty());
    // In-image interpreter injected; NO Landlock grant (Linux-prelude concept).
    assert!(entry
        .policy
        .env
        .contains(&(PYTHON_ENV.to_string(), CONTAINER_PYTHON.to_string())));
    assert!(!entry
        .policy
        .env
        .iter()
        .any(|(k, _)| k == ENV_LANDLOCK_RW));
    // No host scratch dir — the in-VM /tmp tmpfs serves params.json.
    assert!(!entry.ephemeral_scratch);
    assert!(matches!(
        entry.lifecycle,
        crate::worker_lifecycle::Lifecycle::SingleUse
    ));
}

/// The operator's params-file ceiling is forwarded into the jail only when set.
#[cfg(target_os = "macos")]
#[test]
fn container_mode_entry_forwards_params_file_max_only_when_set() {
    let without = container_mode_entry(
        PathBuf::from(CONTAINER_WORKER_BIN),
        "img".to_string(),
        None,
        crate::worker_lifecycle::Lifecycle::SingleUse,
    );
    assert!(!without
        .policy
        .env
        .iter()
        .any(|(k, _)| k == PARAMS_FILE_MAX_ENV));

    let with = container_mode_entry(
        PathBuf::from(CONTAINER_WORKER_BIN),
        "img".to_string(),
        Some("2097152".to_string()),
        crate::worker_lifecycle::Lifecycle::SingleUse,
    );
    assert!(with
        .policy
        .env
        .contains(&(PARAMS_FILE_MAX_ENV.to_string(), "2097152".to_string())));
}

// ---- warm/idle container lifecycle (macOS micro-VM) ----

#[cfg(target_os = "macos")]
#[test]
fn parse_idle_caps_unset_yields_single_use_defaults() {
    let (idle, max_req, max_age) = parse_idle_caps(|_| None);
    assert_eq!(idle, None, "no IDLE_SECONDS -> SingleUse");
    assert_eq!(max_req, DEFAULT_MAX_REQUESTS);
    assert_eq!(max_age, DEFAULT_MAX_AGE_SECONDS);
}

#[cfg(target_os = "macos")]
#[test]
fn parse_idle_caps_reads_idle_seconds_and_overrides() {
    let env = |k: &str| match k {
        IDLE_SECONDS_ENV => Some("120".to_string()),
        MAX_REQUESTS_ENV => Some("50".to_string()),
        MAX_AGE_SECONDS_ENV => Some("3600".to_string()),
        _ => None,
    };
    let (idle, max_req, max_age) = parse_idle_caps(env);
    assert_eq!(idle, Some(120));
    assert_eq!(max_req, 50);
    assert_eq!(max_age, 3600);
}

#[cfg(target_os = "macos")]
#[test]
fn parse_idle_caps_zero_and_garbage_fall_back_to_single_use() {
    assert_eq!(
        parse_idle_caps(|k| (k == IDLE_SECONDS_ENV).then(|| "0".to_string())).0,
        None
    );
    assert_eq!(
        parse_idle_caps(|k| (k == IDLE_SECONDS_ENV).then(|| "abc".to_string())).0,
        None
    );
    assert_eq!(
        parse_idle_caps(|k| (k == IDLE_SECONDS_ENV).then(String::new)).0,
        None
    );
}

#[cfg(target_os = "macos")]
#[test]
fn parse_idle_caps_garbage_overrides_use_defaults() {
    // A garbage max_requests must not panic — fall back to the default.
    let env = |k: &str| match k {
        IDLE_SECONDS_ENV => Some("60".to_string()),
        MAX_REQUESTS_ENV => Some("notnum".to_string()),
        _ => None,
    };
    let (idle, max_req, max_age) = parse_idle_caps(env);
    assert_eq!(idle, Some(60));
    assert_eq!(max_req, DEFAULT_MAX_REQUESTS);
    assert_eq!(max_age, DEFAULT_MAX_AGE_SECONDS);
}

#[cfg(target_os = "macos")]
#[test]
fn container_lifecycle_none_is_single_use() {
    assert!(matches!(
        container_lifecycle(None, 10_000, 86_400),
        crate::worker_lifecycle::Lifecycle::SingleUse
    ));
    assert!(matches!(
        container_lifecycle(Some(0), 10_000, 86_400),
        crate::worker_lifecycle::Lifecycle::SingleUse
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn container_lifecycle_positive_is_idle_timeout_with_caps() {
    match container_lifecycle(Some(120), 50, 3600) {
        crate::worker_lifecycle::Lifecycle::IdleTimeout { caps, contract } => {
            assert_eq!(caps.idle_seconds, 120);
            assert_eq!(caps.max_requests, 50);
            assert_eq!(caps.max_age_seconds, 3600);
            assert_eq!(caps.grace_period_seconds, IDLE_GRACE_SECONDS);
            assert!(contract.stateless);
        }
        other => panic!("expected IdleTimeout, got {other:?}"),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn resolve_container_entry_is_idle_timeout_when_idle_seconds_set() {
    let entry = resolve_container_entry_for_test(|k: &str| match k {
        ENABLE_ENV => Some("1".to_string()),
        USE_CONTAINER_ENV => Some("1".to_string()),
        IDLE_SECONDS_ENV => Some("120".to_string()),
        _ => None,
    });
    assert!(matches!(
        entry.lifecycle,
        crate::worker_lifecycle::Lifecycle::IdleTimeout { .. }
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn resolve_container_entry_is_single_use_without_idle_seconds() {
    let entry = resolve_container_entry_for_test(|k: &str| match k {
        ENABLE_ENV => Some("1".to_string()),
        USE_CONTAINER_ENV => Some("1".to_string()),
        _ => None,
    });
    assert!(matches!(
        entry.lifecycle,
        crate::worker_lifecycle::Lifecycle::SingleUse
    ));
}

/// Build the container-mode entry the resolver would register, from an env
/// closure. Mirrors the resolver's container short-circuit (image / params
/// ceiling / idle caps) so the lifecycle wiring can be asserted without a
/// full `ResolveCtx`.
#[cfg(target_os = "macos")]
fn resolve_container_entry_for_test(get_env: impl Fn(&str) -> Option<String>) -> ToolEntry {
    let image = get_env(IMAGE_ENV)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE.to_string());
    let params_file_max = get_env(PARAMS_FILE_MAX_ENV);
    let (idle, max_req, max_age) = parse_idle_caps(&get_env);
    container_mode_entry(
        PathBuf::from(CONTAINER_WORKER_BIN),
        image,
        params_file_max,
        container_lifecycle(idle, max_req, max_age),
    )
}

/// USE_CONTAINER=1 (macOS) routes the manifest to a Container-tagged entry,
/// with the default image when KASTELLAN_PYTHON_EXEC_IMAGE is unset.
#[cfg(target_os = "macos")]
#[test]
fn resolve_uses_container_backend_when_flag_set() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_USE_CONTAINER" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    // Only the worker binary needs to exist; NO host interpreter is probed
    // in container mode (the interpreter is in the image).
    let exists = |p: &Path| p == Path::new("/opt/python-exec");
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(
                entry.sandbox_backend,
                Some(kastellan_sandbox::SandboxBackendKind::Container)
            );
            assert_eq!(entry.container_image.as_deref(), Some(DEFAULT_IMAGE));
            assert_eq!(entry.binary, PathBuf::from(CONTAINER_WORKER_BIN));
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

/// An explicit KASTELLAN_PYTHON_EXEC_IMAGE override is honoured.
#[cfg(target_os = "macos")]
#[test]
fn resolve_container_honours_image_override() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_USE_CONTAINER" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_IMAGE" => Some("kastellan/python-exec:v9".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    let exists = |p: &Path| p == Path::new("/opt/python-exec");
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(
                entry.container_image.as_deref(),
                Some("kastellan/python-exec:v9")
            );
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

/// USE_CONTAINER unset (or != "1") stays in host mode: a host interpreter
/// IS probed and the entry carries no backend tag. (Runs on both OSes — on
/// Linux the flag is never even read.)
#[test]
fn resolve_stays_host_mode_without_use_container() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    let first = Path::new(PYTHON_CANDIDATES[0]);
    let exists = |p: &Path| p == Path::new("/opt/python-exec") || p == first;
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(entry.sandbox_backend, None, "host mode carries no backend tag");
            assert!(entry.container_image.is_none());
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

// ---- firecracker mode (Linux micro-VM) ----

/// `USE_MICROVM=1` (Linux) routes the manifest to a FirecrackerVm-tagged entry.
/// The resolver short-circuit skips host interpreter discovery entirely (the
/// interpreter is in the rootfs image). Mirrors the macOS container resolver test.
#[cfg(target_os = "linux")]
#[test]
fn resolver_registers_firecracker_when_use_microvm() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_USE_MICROVM" => Some("1".to_string()),
        _ => None,
    };
    // No host interpreter or binary needs to exist: USE_MICROVM short-circuits
    // before any host probing.
    let exists = |_p: &Path| false;
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(
                entry.sandbox_backend,
                Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
                "FirecrackerVm backend must be selected"
            );
            assert!(entry.container_image.is_none(), "no image tag in firecracker mode");
            assert_eq!(
                entry.binary,
                PathBuf::from("/usr/local/bin/kastellan-worker-python-exec")
            );
            // Strict policy preserved.
            assert!(matches!(entry.policy.net, Net::Deny));
            assert_eq!(entry.policy.profile, Profile::WorkerStrict);
            assert_eq!(entry.policy.mem_mb, 512);
            assert_eq!(entry.policy.cpu_ms, 10_000);
            assert_eq!(entry.wall_clock_ms, Some(30_000));
            // No host binds in firecracker mode.
            assert!(entry.policy.fs_read.is_empty(), "no host fs_read in firecracker mode");
            assert!(entry.policy.fs_write.is_empty());
            // Image dir defaults to /var/lib/kastellan/microvm when unset.
            assert!(
                entry
                    .policy
                    .env
                    .contains(&(
                        "KASTELLAN_MICROVM_DIR".to_string(),
                        "/var/lib/kastellan/microvm".to_string()
                    )),
                "KASTELLAN_MICROVM_DIR must default to /var/lib/kastellan/microvm"
            );
            // In-image interpreter injected — the rootfs reality (#360); the
            // value is forwarded into the guest, so it must match the rootfs
            // python the guest init bakes.
            assert!(entry
                .policy
                .env
                .contains(&(PYTHON_ENV.to_string(), "/usr/bin/python3".to_string())));
            // No host scratch — the in-VM /tmp tmpfs serves params.json.
            assert!(!entry.ephemeral_scratch);
            assert!(matches!(entry.lifecycle, crate::worker_lifecycle::Lifecycle::SingleUse));
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

/// KASTELLAN_MICROVM_DIR override is honoured (Linux).
#[cfg(target_os = "linux")]
#[test]
fn resolver_firecracker_honours_image_dir_override() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_USE_MICROVM" => Some("1".to_string()),
        "KASTELLAN_MICROVM_DIR" => Some("/opt/microvm".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| false;
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(
                entry
                    .policy
                    .env
                    .contains(&("KASTELLAN_MICROVM_DIR".to_string(), "/opt/microvm".to_string())),
                "KASTELLAN_MICROVM_DIR override must be forwarded"
            );
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

/// firecracker_mode_entry shape: FirecrackerVm backend, strict policy,
/// no host binds, in-image interpreter, correct lifecycle.
#[cfg(target_os = "linux")]
#[test]
fn firecracker_mode_entry_shape() {
    let entry = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        "/var/lib/kastellan/microvm".to_string(),
        None,
        crate::worker_lifecycle::Lifecycle::SingleUse,
    );
    assert_eq!(
        entry.sandbox_backend,
        Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
    );
    assert!(entry.container_image.is_none());
    assert!(matches!(entry.policy.net, Net::Deny));
    assert_eq!(entry.policy.profile, Profile::WorkerStrict);
    assert_eq!(entry.policy.mem_mb, 512);
    assert_eq!(entry.policy.cpu_ms, 10_000);
    assert_eq!(entry.wall_clock_ms, Some(30_000));
    assert!(entry.policy.fs_read.is_empty());
    assert!(entry.policy.fs_write.is_empty());
    assert!(!entry.ephemeral_scratch);
    // In-guest interpreter path = the rootfs reality (forwarded via cmdline,
    // #360); must match `build-rootfs.sh` + the guest init's baked fallback.
    assert!(entry
        .policy
        .env
        .contains(&(PYTHON_ENV.to_string(), "/usr/bin/python3".to_string())));
    assert!(!entry.policy.env.iter().any(|(k, _)| k == ENV_LANDLOCK_RW));
}

/// The params-file ceiling is forwarded into the VM only when set (Linux).
#[cfg(target_os = "linux")]
#[test]
fn firecracker_mode_entry_forwards_params_file_max_only_when_set() {
    let without = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        "/var/lib/kastellan/microvm".to_string(),
        None,
        crate::worker_lifecycle::Lifecycle::SingleUse,
    );
    assert!(!without.policy.env.iter().any(|(k, _)| k == PARAMS_FILE_MAX_ENV));

    let with = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        "/var/lib/kastellan/microvm".to_string(),
        Some("2097152".to_string()),
        crate::worker_lifecycle::Lifecycle::SingleUse,
    );
    assert!(with
        .policy
        .env
        .contains(&(PARAMS_FILE_MAX_ENV.to_string(), "2097152".to_string())));
}
