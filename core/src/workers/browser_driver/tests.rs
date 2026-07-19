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
    fn enable_accepts_non_one_truthy_flag() {
        // #459: the ENABLE gate now goes through the unified `env_flag_enabled`
        // dialect, so a non-"1" truthy value (`true`) is NOT treated as off — it
        // passes the gate (then fails later for the missing anchor, not Disabled).
        let env = |k: &str| (k == "KASTELLAN_BROWSER_DRIVER_ENABLE").then(|| "true".to_string());
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        assert!(
            !matches!(
                resolve_env(env, is_dir, exists, no_canon, no_deps),
                Err(ResolveSkipReason::Disabled)
            ),
            "ENABLE=true must pass the opt-in gate (not read as off)"
        );
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
        let entry = browser_driver_entry(&out, &[], None);
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
        let entry = browser_driver_entry(&out, &[], None);
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
        // Rows are bare hosts — `db::tool_allowlists::validate_domain` forbids an
        // embedded port, so a `tool_allowlists` row can never carry one.
        let entry = browser_driver_entry(
            &env,
            &["example.com".to_string(), ".wiki.example.org".to_string()],
            None,
        );
        assert_eq!(entry.binary, PathBuf::from("/v/bin/kastellan-worker-browser-driver"));
        // Phase 2: the browser-specific seccomp/Seatbelt profile.
        assert!(matches!(entry.policy.profile, Profile::WorkerBrowserClient));
        // Manifest leaves proxy_uds None; force-routing sets it at spawn.
        assert!(entry.policy.proxy_uds.is_none());
        // Net::Allowlist is PORT-SCOPED to 443, not the verbatim rows: a
        // bare-host entry is an all-port grant at the egress proxy, so
        // `example.com:22` must not be reachable from an `example.com` row.
        // The wildcard dot survives the mapping (proxy suffix match).
        match &entry.policy.net {
            Net::Allowlist(hosts) => assert_eq!(
                hosts,
                &vec![
                    "example.com:443".to_string(),
                    ".wiki.example.org:443".to_string()
                ]
            ),
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
        // The worker's own check still gets the VERBATIM rows (wildcard intact).
        assert_eq!(
            env_get("KASTELLAN_BROWSER_DRIVER_ALLOWLIST").as_deref(),
            Some(r#"["example.com",".wiki.example.org"]"#)
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

    /// #283: macOS browser-driver no longer grants the shared host `/tmp`. The
    /// manifest leaves `fs_write` empty and opts into `ephemeral_scratch`, so the
    /// cold-spawn path mints a unique per-spawn dir (added to `fs_write` at spawn
    /// by `prepare_ephemeral_scratch`) and the worker writes only there. Holds on
    /// both platforms (Linux already had an empty `fs_write` — its scratch is the
    /// bwrap per-spawn `/tmp` tmpfs; `ephemeral_scratch` is a no-op there).
    #[test]
    fn entry_uses_per_spawn_ephemeral_scratch_not_shared_tmp() {
        let env = BrowserDriverEnv {
            script_path: PathBuf::from("/v/bin/kastellan-worker-browser-driver"),
            venv_dir: PathBuf::from("/v"),
            interpreter_root: None,
            interpreter_lib_dirs: vec![],
            extra_fs_read: vec![],
        };
        let entry = browser_driver_entry(&env, &[], None);
        assert!(
            entry.ephemeral_scratch,
            "browser-driver must opt into the per-spawn scratch mechanism (#283)"
        );
        assert!(
            entry.policy.fs_write.is_empty(),
            "manifest must not pre-grant a writable dir; the per-spawn scratch is \
             added at spawn, never the shared host /tmp ({:?})",
            entry.policy.fs_write,
        );
    }

    #[test]
    fn manifest_registers_when_enabled() {
        let get_env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            // On Linux, resolve() fail-closes unless the lockdown-exec shim is
            // discoverable; point the override at a (test-runnable) path so the
            // manifest registers. Ignored on macOS (Seatbelt — no shim there).
            "KASTELLAN_LOCKDOWN_EXEC_BIN" => {
                Some("/usr/bin/kastellan-worker-lockdown-exec".to_string())
            }
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["example.com".to_string()];
        // is_dir=false so the shim override path counts as a runnable file
        // (discover_binary requires exists && !is_dir).
        let c = ResolveCtx {
            get_env: &get_env,
            exists: &exists,
            is_dir: &|_p| false,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist: &allowlist,
        };
        assert_eq!(BrowserDriverManifest.name(), "browser-driver");
        assert_eq!(BrowserDriverManifest.allowlist_tool(), Some("browser-driver"));
        assert!(matches!(
            BrowserDriverManifest.resolve(&c),
            Resolution::Register(_)
        ));
    }

    /// Linux fail-closed: enabled + venv present but NO discoverable
    /// lockdown-exec shim ⇒ Misconfigured (never register an unfilterable
    /// browser). Linux-only — on macOS the shim isn't used.
    #[cfg(target_os = "linux")]
    #[test]
    fn manifest_misconfigured_when_shim_missing_on_linux() {
        let get_env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            // No KASTELLAN_LOCKDOWN_EXEC_BIN override and no exe_dir ⇒ shim
            // undiscoverable.
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["example.com".to_string()];
        let c = ctx(&get_env, &exists, &allowlist); // exe_dir: None
        assert!(
            matches!(
                BrowserDriverManifest.resolve(&c),
                Resolution::Misconfigured { .. }
            ),
            "Linux browser-driver must fail closed when the lockdown-exec shim is missing"
        );
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
        let entry = browser_driver_entry(&env, &[], None);
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/opt/hb/gettext/lib")));
    }

    /// cfg-split: only one arm runs per platform. The Linux arm asserts the
    /// shim is set AND that no `KASTELLAN_LANDLOCK_PROFILE` env is emitted —
    /// since #281's Landlock follow-up, browser-driver runs with Landlock
    /// **active** (the shim applies the ruleset; absence of the env = the
    /// default on-path). The macOS arm asserts the shim/env are both absent
    /// (Seatbelt from the parent). The Linux arm is exercised by CI/DGX.
    #[test]
    fn entry_sets_lockdown_shim_and_landlock_active_on_linux() {
        let env = BrowserDriverEnv {
            script_path: PathBuf::from("/v/bin/kastellan-worker-browser-driver"),
            venv_dir: PathBuf::from("/v"),
            interpreter_root: None,
            interpreter_lib_dirs: vec![],
            extra_fs_read: vec![],
        };
        let allow = vec!["example.com".to_string()];
        #[cfg(target_os = "linux")]
        {
            let shim = std::path::PathBuf::from("/opt/kastellan/kastellan-worker-lockdown-exec");
            let entry = browser_driver_entry(&env, &allow, Some(shim.clone()));
            assert_eq!(entry.lockdown_shim.as_deref(), Some(shim.as_path()));
            assert!(
                entry.policy.fs_read.contains(&shim),
                "shim must be bound RO into the jail so bwrap can exec it (the DGX bug)"
            );
            assert!(
                !entry.policy.env.iter().any(|(k, _)| k == "KASTELLAN_LANDLOCK_PROFILE"),
                "Linux browser-driver must NOT disable Landlock — the shim applies the ruleset (#281 follow-up)"
            );
            // The writable scratch grant is now load-bearing under Landlock
            // (Chromium's --user-data-dir lives under /tmp).
            assert!(
                entry.policy.env.iter().any(
                    |(k, v)| k == crate::tool_host::ENV_LANDLOCK_RW && v == r#"["/tmp"]"#
                ),
                "Landlock RW must grant the /tmp scratch the browser writes to"
            );
        }
        #[cfg(not(target_os = "linux"))]
        {
            let entry = browser_driver_entry(&env, &allow, None);
            assert!(entry.lockdown_shim.is_none());
            assert!(
                !entry.policy.env.iter().any(|(k, _)| k == "KASTELLAN_LANDLOCK_PROFILE"),
                "macOS browser-driver must not add the Landlock-profile env"
            );
        }
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
        let entry = browser_driver_entry(&out, &[], None);
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/opt/hb/gettext/lib")));
    }

    // ---- Firecracker micro-VM entry (slice 2) --------------------------------
    //
    // `Resolution` deliberately derives no `Debug` (it holds a large
    // `ToolEntry`), so a failing match cannot be `{:?}`-printed. Name the arm
    // instead — the same helper web-fetch's tests use. `cfg`-gated with its only
    // callers so macOS clippy does not see it as dead code.

    #[cfg(target_os = "linux")]
    fn outcome_label(r: &Resolution) -> &'static str {
        match r {
            Resolution::Register(_) => "Register",
            Resolution::Disabled { .. } => "Disabled",
            Resolution::Misconfigured { .. } => "Misconfigured",
        }
    }
    //
    // Linux-only: `browser_driver_firecracker_entry` and the `FirecrackerVm`
    // backend variant are `cfg(target_os = "linux")`, so macOS compiles this
    // block out entirely. The DGX `clippy -p kastellan-core --all-targets
    // -D warnings` gate is the authoritative check for it (memory:
    // cfg-linux-e2e-deadcode-dgx-clippy).

    /// The VM entry's shape: VM backend, no host paths shared in, no lockdown
    /// shim, port-scoped allowlist, and the in-rootfs browser tree.
    #[cfg(target_os = "linux")]
    #[test]
    fn firecracker_entry_is_vm_backed_with_no_host_binds_or_shim() {
        let allowlist = vec!["example.com".to_string(), ".wiki.example.org".to_string()];
        let entry = browser_driver_firecracker_entry(
            PathBuf::from("/usr/local/bin/kastellan-worker-browser-driver"),
            "/var/lib/kastellan/microvm".to_string(),
            &allowlist,
        );

        assert!(
            matches!(
                entry.sandbox_backend,
                Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
            ),
            "VM mode must select the Firecracker backend"
        );
        // A VM shares no host paths in: the venv, interpreter and browser tree
        // all live inside the rootfs. The per-instance CA would be appended at
        // spawn — browser-driver does not even get one (no-MITM tunnel).
        assert!(
            entry.policy.fs_read.is_empty(),
            "VM fs_read must be empty (no host FS, no NIC, no local DNS)"
        );
        assert!(entry.policy.fs_write.is_empty());
        // The lockdown-exec shim is a HOST-mode mechanism (#281): it is not
        // staged in the rootfs, so requiring it would be a boot failure. In VM
        // mode the isolation boundary is the VM itself.
        assert!(
            entry.lockdown_shim.is_none(),
            "VM mode must not require the host lockdown-exec shim"
        );
        assert!(
            !entry
                .policy
                .env
                .iter()
                .any(|(k, _)| k == crate::tool_host::ENV_LANDLOCK_RW),
            "Landlock RW is a host-mode (bwrap+shim) grant; the VM has no shim to honour it"
        );
        // proxy_uds stays None in the manifest; force-routing sets it at spawn.
        assert!(entry.policy.proxy_uds.is_none());
        assert!(matches!(entry.policy.profile, Profile::WorkerBrowserClient));
        assert!(matches!(
            entry.lifecycle,
            crate::worker_lifecycle::Lifecycle::SingleUse
        ));
        // Chromium's process tree still needs the raised task cap.
        assert_eq!(entry.policy.tasks_max, Some(512));
        // Firecracker ENFORCES mem_mb as total guest RAM, and it must cover
        // Chromium plus the /tmp tmpfs that --disable-dev-shm-usage redirects
        // shared memory into (spec §6/§10.4) — hence 2048, not host mode's 1024.
        assert_eq!(entry.policy.mem_mb, 2048);

        // Same dual-allowlist shape as host mode: port-scoped for the proxy
        // (a bare host would be an all-port grant), verbatim for the worker.
        match &entry.policy.net {
            Net::Allowlist(hosts) => assert_eq!(
                hosts,
                &vec![
                    "example.com:443".to_string(),
                    ".wiki.example.org:443".to_string()
                ]
            ),
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        let env_get = |key: &str| {
            entry
                .policy
                .env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            env_get("KASTELLAN_BROWSER_DRIVER_ALLOWLIST"),
            Some(r#"["example.com",".wiki.example.org"]"#)
        );
        // In-rootfs browser tree (Dockerfile.browser-driver's
        // ENV PLAYWRIGHT_BROWSERS_PATH), NOT a host venv subdir.
        assert_eq!(
            env_get("PLAYWRIGHT_BROWSERS_PATH"),
            Some("/usr/local/lib/kastellan-browser-driver/browsers")
        );
        // Playwright's Node driver calls uv_os_homedir() at startup.
        assert_eq!(env_get("TMPDIR"), Some("/tmp"));
        assert_eq!(env_get("HOME"), Some("/tmp"));
        // Backend image coordinates (stripped before the guest env is encoded).
        assert_eq!(env_get("KASTELLAN_MICROVM_DIR"), Some("/var/lib/kastellan/microvm"));
        assert_eq!(env_get("KASTELLAN_MICROVM_ROOTFS"), Some("browser-driver.ext4"));
    }

    /// `USE_MICROVM=1` registers the VM entry **without** any host venv,
    /// interpreter or lockdown shim on disk.
    ///
    /// This is the property that makes slice 2 more than a copy of web-fetch's
    /// branch: host mode fail-closes with `Misconfigured` when the venv shim or
    /// the lockdown-exec shim is missing, and on a VM-only deployment neither
    /// exists. `exists`/`canonicalize` here return "nothing on this host", which
    /// would make host mode `Misconfigured`.
    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_uses_microvm_entry_without_any_host_venv_or_shim() {
        let get_env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_USE_MICROVM" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| false; // no venv shim, no lockdown-exec shim
        let allowlist = |_t: &str| vec!["example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match BrowserDriverManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                // THE PIN that matters: the guest execs the in-rootfs path, not
                // a host build-output path. A host path ENOENTs inside the
                // guest, panics PID1 and boot-loops, and the dispatch hangs to
                // wall-clock naming nothing (memory:
                // vm-worker-in-rootfs-binary-path). Must match the symlink baked
                // by build-browser-driver-rootfs.sh byte for byte.
                assert_eq!(
                    entry.binary,
                    PathBuf::from("/usr/local/bin/kastellan-worker-browser-driver")
                );
                // Default image dir when KASTELLAN_MICROVM_DIR is unset.
                assert!(entry.policy.env.iter().any(|(k, v)| k
                    == "KASTELLAN_MICROVM_DIR"
                    && v == "/var/lib/kastellan/microvm"));
            }
            other => panic!("expected Register(VM entry), got {}", outcome_label(&other)),
        }
    }

    /// An operator-set `KASTELLAN_MICROVM_DIR` overrides the default; a blank
    /// one falls back (mirrors web-fetch's `.filter(|v| !v.trim().is_empty())`).
    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_microvm_honors_image_dir_override_and_ignores_blank() {
        let dir_of = |entry: &crate::scheduler::ToolEntry| {
            entry
                .policy
                .env
                .iter()
                .find(|(k, _)| k == "KASTELLAN_MICROVM_DIR")
                .map(|(_, v)| v.clone())
                .expect("VM entry carries an image dir")
        };
        for (set, want) in [
            (Some("/srv/images"), "/srv/images"),
            (Some("   "), "/var/lib/kastellan/microvm"),
        ] {
            let get_env = move |k: &str| match k {
                "KASTELLAN_BROWSER_DRIVER_ENABLE" | "KASTELLAN_BROWSER_DRIVER_USE_MICROVM" => {
                    Some("1".to_string())
                }
                "KASTELLAN_MICROVM_DIR" => set.map(|s| s.to_string()),
                _ => None,
            };
            let exists = |_p: &Path| false;
            let allowlist = |_t: &str| vec![];
            let c = ctx(&get_env, &exists, &allowlist);
            match BrowserDriverManifest.resolve(&c) {
                Resolution::Register(entry) => assert_eq!(dir_of(&entry), want, "set={set:?}"),
                other => panic!("expected Register, got {}", outcome_label(&other)),
            }
        }
    }

    /// `USE_MICROVM=1` with the worker itself disabled must stay `Disabled` —
    /// the VM flag is a backend choice, never an implicit opt-in.
    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_microvm_without_enable_stays_disabled() {
        let get_env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_USE_MICROVM" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec![];
        let c = ctx(&get_env, &exists, &allowlist);
        match BrowserDriverManifest.resolve(&c) {
            Resolution::Disabled { .. } => {}
            other => panic!("expected Disabled, got {}", outcome_label(&other)),
        }
    }

    /// The VM flag honours the unified truthiness dialect (#459 residual #2), so
    /// `=true` cannot silently read as off next to a `FORCE_ROUTING=true`.
    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_microvm_flag_honors_unified_truthiness_dialect() {
        for (raw, want_vm) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("", false),
        ] {
            let get_env = move |k: &str| match k {
                "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
                "KASTELLAN_BROWSER_DRIVER_USE_MICROVM" => Some(raw.to_string()),
                _ => None,
            };
            // Host mode would be Misconfigured here (nothing on disk), so the
            // falsy arm is distinguishable from the VM arm without a venv.
            let exists = |_p: &Path| false;
            let allowlist = |_t: &str| vec![];
            let c = ctx(&get_env, &exists, &allowlist);
            let got_vm = matches!(
                BrowserDriverManifest.resolve(&c),
                Resolution::Register(ref e)
                    if matches!(
                        e.sandbox_backend,
                        Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                    )
            );
            assert_eq!(got_vm, want_vm, "USE_MICROVM={raw:?}");
        }
    }
