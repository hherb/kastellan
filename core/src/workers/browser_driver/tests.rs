use super::*;

    /// No interpreter canonicalization in most tests — a self-contained venv.
    fn no_canon(_p: &Path) -> Option<PathBuf> {
        None
    }

    /// No interpreter deps in most tests.
    fn no_deps(_p: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    #[test]
    fn disabled_when_enable_not_set() {
        let env = |_k: &str| None;
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        assert!(matches!(
            resolve_env(env, is_dir, exists, no_canon, no_deps),
            Err(ResolveSkipReason::Disabled)
        ));
    }

    #[test]
    fn unresolvable_when_no_anchor() {
        let env = |k: &str| (k == "KASTELLAN_BROWSER_DRIVER_ENABLE").then(|| "1".to_string());
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        assert!(matches!(
            resolve_env(env, is_dir, exists, no_canon, no_deps),
            Err(ResolveSkipReason::VenvDirUnresolvable)
        ));
    }

    #[test]
    fn shim_missing_surfaces_path() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| false; // shim absent
        match resolve_env(env, is_dir, exists, no_canon, no_deps) {
            Err(ResolveSkipReason::ScriptShimMissing { path }) => {
                assert!(path.ends_with(SHIM_NAME), "path: {}", path.display());
            }
            other => panic!("expected ScriptShimMissing, got {other:?}"),
        }
    }

    #[test]
    fn resolves_when_enabled_and_shim_present() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        let out = resolve_env(env, is_dir, exists, no_canon, no_deps).expect("resolves");
        assert_eq!(out.venv_dir, PathBuf::from("/v"));
        assert!(out.script_path.ends_with(SHIM_NAME));
        // Self-contained (canonicalize → None) ⇒ no extra interpreter bind.
        assert_eq!(out.interpreter_root, None);
    }

    #[test]
    fn resolves_interpreter_root_for_external_venv() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        // The venv's python3 symlinks to an EXTERNAL interpreter (pyenv-style):
        // /home/u/.pyenv/versions/3.12.3/bin/python3.12 → prefix
        // /home/u/.pyenv/versions/3.12.3 must be bound.
        let canon = |p: &Path| {
            if p == Path::new("/v/bin/python3") {
                Some(PathBuf::from(
                    "/home/u/.pyenv/versions/3.12.3/bin/python3.12",
                ))
            } else {
                None
            }
        };
        let out = resolve_env(env, is_dir, exists, canon, no_deps).expect("resolves");
        assert_eq!(
            out.interpreter_root,
            Some(PathBuf::from("/home/u/.pyenv/versions/3.12.3"))
        );
        // And the entry binds that root read-only.
        let entry = browser_driver_entry(&out, &[]);
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/home/u/.pyenv/versions/3.12.3")));
    }

    #[test]
    fn extra_fs_read_env_is_parsed_and_bound() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            "KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ" => {
                Some(r#"["/opt/homebrew", "relative/dropped"]"#.to_string())
            }
            _ => None,
        };
        let out = resolve_env(env, |_p| true, |_p| true, no_canon, no_deps).expect("resolves");
        // Absolute entry kept; relative one dropped (policy needs absolute paths).
        assert_eq!(out.extra_fs_read, vec![PathBuf::from("/opt/homebrew")]);
        let entry = browser_driver_entry(&out, &[]);
        assert!(entry.policy.fs_read.contains(&PathBuf::from("/opt/homebrew")));
    }

    #[test]
    fn malformed_extra_fs_read_yields_no_extra_paths() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            "KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ" => Some("not json".to_string()),
            _ => None,
        };
        let out = resolve_env(env, |_p| true, |_p| true, no_canon, no_deps).expect("resolves");
        assert!(out.extra_fs_read.is_empty());
    }

    #[test]
    fn interpreter_under_venv_needs_no_extra_bind() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        // Self-contained venv: python3 resolves to within /v.
        let canon = |_p: &Path| Some(PathBuf::from("/v/bin/python3.12"));
        let out = resolve_env(env, is_dir, exists, canon, no_deps).expect("resolves");
        assert_eq!(
            out.interpreter_root, None,
            "interpreter already under venv_dir ⇒ no extra bind"
        );
    }

    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        allowlist: &'a dyn Fn(&str) -> Vec<String>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| true,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist,
        }
    }

    #[test]
    fn entry_has_browser_client_policy_and_operator_allowlist() {
        let env = BrowserDriverEnv {
            script_path: PathBuf::from("/v/bin/kastellan-worker-browser-driver"),
            venv_dir: PathBuf::from("/v"),
            interpreter_root: None,
            interpreter_lib_dirs: vec![],
            extra_fs_read: vec![],
        };
        let entry = browser_driver_entry(&env, &["example.com:443".to_string()]);
        assert_eq!(entry.binary, PathBuf::from("/v/bin/kastellan-worker-browser-driver"));
        // Phase 2: the browser-specific seccomp/Seatbelt profile.
        assert!(matches!(entry.policy.profile, Profile::WorkerBrowserClient));
        // Manifest leaves proxy_uds None; force-routing sets it at spawn.
        assert!(entry.policy.proxy_uds.is_none());
        match &entry.policy.net {
            Net::Allowlist(hosts) => assert_eq!(hosts, &vec!["example.com:443".to_string()]),
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        // venv mounted RO; resolver config present for in-jail DNS.
        assert!(entry.policy.fs_read.contains(&PathBuf::from("/v")));
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/etc/resolv.conf")));
        // operator allowlist injected as env JSON.
        let env_get = |key: &str| {
            entry
                .policy
                .env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            env_get("KASTELLAN_BROWSER_DRIVER_ALLOWLIST").as_deref(),
            Some(r#"["example.com:443"]"#)
        );
        // Browsers staged inside the (already-bound) venv; TMPDIR scratch wired.
        assert_eq!(
            env_get("PLAYWRIGHT_BROWSERS_PATH").as_deref(),
            Some("/v/browsers")
        );
        assert_eq!(env_get("TMPDIR").as_deref(), Some("/tmp"));
        // HOME must be set so Playwright's Node driver's uv_os_homedir() works
        // under bwrap's --clearenv (no /etc/passwd in the jail).
        assert_eq!(env_get("HOME").as_deref(), Some("/tmp"));
        assert_eq!(
            env_get(crate::tool_host::ENV_LANDLOCK_RW).as_deref(),
            Some(r#"["/tmp"]"#)
        );
        assert!(matches!(
            entry.lifecycle,
            crate::worker_lifecycle::Lifecycle::SingleUse
        ));
        // TasksMax must be raised above the default 64 — Chromium's process
        // tree needs it (DGX-confirmed).
        assert_eq!(entry.policy.tasks_max, Some(512));
    }

    #[test]
    fn manifest_registers_when_enabled() {
        let get_env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["example.com:443".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        assert_eq!(BrowserDriverManifest.name(), "browser-driver");
        assert_eq!(BrowserDriverManifest.allowlist_tool(), Some("browser-driver"));
        assert!(matches!(
            BrowserDriverManifest.resolve(&c),
            Resolution::Register(_)
        ));
    }

    #[test]
    fn manifest_disabled_by_default() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);
        assert!(matches!(
            BrowserDriverManifest.resolve(&c),
            Resolution::Disabled { .. }
        ));
    }

    #[test]
    fn interpreter_lib_dirs_are_bound_in_fs_read() {
        let env = BrowserDriverEnv {
            script_path: PathBuf::from("/v/bin/kastellan-worker-browser-driver"),
            venv_dir: PathBuf::from("/v"),
            interpreter_root: Some(PathBuf::from("/px")),
            interpreter_lib_dirs: vec![PathBuf::from("/opt/hb/gettext/lib")],
            extra_fs_read: vec![],
        };
        let entry = browser_driver_entry(&env, &[]);
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/opt/hb/gettext/lib")));
    }

    #[test]
    fn resolve_env_binds_out_of_prefix_interpreter_deps() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        // External pyenv interpreter at /px linking a Homebrew libintl.
        let canon = |p: &Path| match p.to_str() {
            Some("/v/bin/python3") => Some(PathBuf::from("/px/bin/python3.12")),
            _ => Some(p.to_path_buf()),
        };
        let deps = |p: &Path| {
            if p == Path::new("/px/bin/python3.12") {
                vec![PathBuf::from("/opt/hb/gettext/lib/libintl.8.dylib")]
            } else {
                vec![]
            }
        };
        let out = resolve_env(env, |_p| true, |_p| true, canon, deps).expect("resolves");
        assert_eq!(
            out.interpreter_lib_dirs,
            vec![PathBuf::from("/opt/hb/gettext/lib")]
        );
        let entry = browser_driver_entry(&out, &[]);
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/opt/hb/gettext/lib")));
    }
