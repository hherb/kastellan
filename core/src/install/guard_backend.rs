//! Planning the supervised guard backend for `kastellan-cli install` (#609).
//!
//! # Why this exists
//!
//! The guard tier's backend (`llama-server` serving Shieldstral) used to be a
//! hand-started process. The daemon refuses to boot when a configured guard's
//! `/props` is unreachable (wiring-spec D6), so after a reboot the *whole
//! agent* stayed down until someone restarted the guard by hand — 1 500+
//! restarts on the DGX between 2026-10-19 and 2026-10-24.
//!
//! When the operator names the server binary and the model file, the
//! installer now adds a `kastellan-guard` unit to `kastellan.target`, started
//! before core, so it comes back with everything else.
//!
//! # One source of truth for the port and the alias
//!
//! The operator does **not** state the port or the model alias twice. Both
//! are read back out of the daemon's own guard settings —
//! `KASTELLAN_LLM_GUARD_URL` (the port) and `KASTELLAN_LLM_GUARD_MODEL` (the
//! alias) — so the unit serves exactly what the daemon will ask for. A
//! separate pair of settings would be a pair that can disagree, and the
//! disagreement would show up only as a boot refusal.
//!
//! The planning ([`plan_guard_backend`], [`add_to_specs`]) is **pure**:
//! folded `(key, value)` pairs in, a decision out. The thin IO half
//! ([`resolve_from_env_files`], [`remove_stale_unit`]) sits at the bottom so
//! `run.rs` (already over the file-size cap) only gains its call sites.

use std::path::{Path, PathBuf};

use kastellan_supervisor::env_file::fold_env_files;
use kastellan_supervisor::specs::{guard_service_spec, CORE_SERVICE_NAME, GUARD_SERVICE_NAME};
use kastellan_supervisor::{EnvFileRef, ServiceStatus, Supervisor};
use url::Url;

use super::plan::{InstallSpecs, Layout};

/// Absolute path to the operator's `llama-server` binary.
pub const GUARD_SERVER_BIN_KEY: &str = "KASTELLAN_GUARD_SERVER_BIN";
/// Absolute path to the guard model's GGUF file.
pub const GUARD_SERVER_MODEL_KEY: &str = "KASTELLAN_GUARD_SERVER_MODEL";

/// The daemon's own guard settings the unit is derived from (read by
/// `kastellan-llm-router`'s config).
const GUARD_URL_KEY: &str = "KASTELLAN_LLM_GUARD_URL";
const GUARD_MODEL_KEY: &str = "KASTELLAN_LLM_GUARD_MODEL";

/// Everything the guard unit needs, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
///
/// Fields are private so [`plan_guard_backend`] — absolute paths, a non-zero
/// loopback port, no systemd-expanded characters — is the only way to get one.
pub struct GuardBackend {
    server: PathBuf,
    model: PathBuf,
    /// The API model name; equals `KASTELLAN_LLM_GUARD_MODEL`.
    alias: String,
    /// The loopback port; the port of `KASTELLAN_LLM_GUARD_URL`.
    port: u16,
}

/// Look up `key` in folded env pairs. An empty value counts as unset, the
/// same rule the router's `read_env` applies to its own keys (no trim, so the
/// alias is byte-for-byte what the daemon sends).
fn lookup<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .rev() // a repeated key resolves to its last occurrence
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// Decide whether to supervise a guard backend, from the **folded** env files
/// (`kastellan.env` then `kastellan.env.local`, later winning).
///
/// - Neither `KASTELLAN_GUARD_SERVER_*` key set → `Ok(None)`: no unit. This is
///   the normal state for a host without a guard tier, *and* for one whose
///   operator still runs the guard by hand.
/// - Exactly one set → `Err`: half a configuration is a typo, not an opt-out.
/// - Both set → both must be absolute paths, and the daemon's guard URL and
///   model must be set, with the URL on loopback and an explicit port. None of
///   the three values reaching the command line may hold `%` or `$`, which
///   systemd would expand inside `ExecStart=`.
pub fn plan_guard_backend(env: &[(String, String)]) -> Result<Option<GuardBackend>, String> {
    let (server, model) = match (lookup(env, GUARD_SERVER_BIN_KEY), lookup(env, GUARD_SERVER_MODEL_KEY)) {
        (None, None) => return Ok(None),
        (Some(s), Some(m)) => (s, m),
        (Some(_), None) | (None, Some(_)) => {
            return Err(format!(
                "{GUARD_SERVER_BIN_KEY} and {GUARD_SERVER_MODEL_KEY} must be set together \
                 (the llama-server binary and the guard model file) — or neither, to leave \
                 the guard backend unsupervised"
            ))
        }
    };
    for (key, path) in [(GUARD_SERVER_BIN_KEY, server), (GUARD_SERVER_MODEL_KEY, model)] {
        if !Path::new(path).is_absolute() {
            return Err(format!(
                "{key} must be an absolute path (the unit runs with no PATH and no cwd), got {}",
                path
            ));
        }
    }
    let (Some(url), Some(alias)) = (lookup(env, GUARD_URL_KEY), lookup(env, GUARD_MODEL_KEY)) else {
        return Err(format!(
            "{GUARD_SERVER_BIN_KEY}/{GUARD_SERVER_MODEL_KEY} ask for a supervised guard backend, \
             but the daemon has no guard tier to use it: set {GUARD_URL_KEY} (e.g. \
             http://127.0.0.1:8081/v1), {GUARD_MODEL_KEY} and KASTELLAN_LLM_GUARD_TAU in \
             kastellan.env.local"
        ));
    };
    for (key, value) in [(GUARD_SERVER_BIN_KEY, server), (GUARD_SERVER_MODEL_KEY, model), (GUARD_MODEL_KEY, alias)] {
        if value.contains(['%', '$']) {
            return Err(format!(
                "{key}={value}: `%` and `$` are not allowed — systemd expands them inside the \
                 unit's ExecStart=, so the guard would run with a different argument"
            ));
        }
    }
    let port = loopback_port(url)?;
    Ok(Some(GuardBackend {
        server: PathBuf::from(server),
        model: PathBuf::from(model),
        alias: alias.to_string(),
        port,
    }))
}

/// The port of a loopback `http://` guard URL.
///
/// The unit binds `127.0.0.1` only, so a URL naming any other host (or
/// `[::1]`, which `127.0.0.1` does not answer) would have the daemon knocking
/// on a door the unit never opens. A guard URL with no port is almost certainly
/// an omission, so it is refused rather than read as 80 — and so is an
/// explicit `:80`, which the URL parser drops as http's default. Port 0 is
/// refused: the unit would bind a random port the daemon cannot know.
fn loopback_port(raw: &str) -> Result<u16, String> {
    let url = Url::parse(raw).map_err(|e| format!("{GUARD_URL_KEY}={raw} does not parse: {e}"))?;
    if url.scheme() != "http" {
        return Err(format!(
            "{GUARD_URL_KEY}={raw}: a supervised guard backend serves plain http on loopback"
        ));
    }
    match url.host_str() {
        Some("127.0.0.1") | Some("localhost") => {}
        _ => {
            return Err(format!(
                "{GUARD_URL_KEY}={raw}: a supervised guard backend listens on 127.0.0.1 only, so \
                 the URL must name 127.0.0.1 or localhost. Unset {GUARD_SERVER_BIN_KEY} and \
                 {GUARD_SERVER_MODEL_KEY} if the guard runs elsewhere."
            ))
        }
    }
    match url.port() {
        Some(0) => Err(format!(
            "{GUARD_URL_KEY}={raw}: port 0 would have the guard bind a random port the daemon \
             cannot know; name a fixed one (e.g. http://127.0.0.1:8081/v1)"
        )),
        Some(port) => Ok(port),
        None => Err(format!(
            "{GUARD_URL_KEY}={raw}: name the port explicitly, other than 80 (http's default, \
             which parses as no port at all) — e.g. http://127.0.0.1:8081/v1"
        )),
    }
}

/// Add the guard unit to an install: a target member started **before** core,
/// and an `After=` on core, so systemd starts the guard first. Pure.
///
/// `After=` orders the *start*, not readiness: `llama-server` needs a few
/// seconds to load its model, and core's boot rides that out with a bounded
/// `/props` retry (`guard_model::tier::props_wait`). launchd ignores `after`
/// altogether, and the same retry — plus core's `KeepAlive` — covers it there.
pub fn add_to_specs(specs: &mut InstallSpecs, guard: &GuardBackend, log_dir: &Path) {
    let member_at = specs
        .members
        .iter()
        .position(|s| s.name == CORE_SERVICE_NAME)
        .unwrap_or(specs.members.len());
    specs.members.insert(
        member_at,
        guard_service_spec(&guard.server, &guard.model, &guard.alias, guard.port, log_dir),
    );
    let target_at = specs
        .target
        .members
        .iter()
        .position(|n| n == CORE_SERVICE_NAME)
        .unwrap_or(specs.target.members.len());
    specs.target.members.insert(target_at, GUARD_SERVICE_NAME.to_string());
    if let Some(core) = specs.members.iter_mut().find(|s| s.name == CORE_SERVICE_NAME) {
        core.after.push(GUARD_SERVICE_NAME.to_string());
    }
}

// ----- IO half -----

/// Read the env files the core unit reads (`kastellan.env`, then the optional
/// `kastellan.env.local`), plan the guard backend from them, and check that
/// the two files it names exist. Call it **after** `kastellan.env` has been
/// regenerated, so it sees what the daemon will see.
pub fn resolve_from_env_files(layout: &Layout) -> Result<Option<GuardBackend>, String> {
    let mut env = Vec::new();
    fold_env_files(
        &mut env,
        &[
            EnvFileRef { path: layout.env_file.clone(), optional: false },
            EnvFileRef { path: layout.env_local_file.clone(), optional: true },
        ],
    )
    .map_err(|e| format!("reading the env files for the guard backend: {e}"))?;
    let Some(guard) = plan_guard_backend(&env)? else {
        // Not an error — the operator may run the guard by hand — but a reboot
        // then leaves the whole agent down until they do (#609), so say so.
        if let Some(url) = lookup(&env, GUARD_URL_KEY) {
            eprintln!(
                "note: the guard tier is configured ({GUARD_URL_KEY}={url}) but its backend is \
                 not supervised, so it will not come back after a reboot; set \
                 {GUARD_SERVER_BIN_KEY} and {GUARD_SERVER_MODEL_KEY} in kastellan.env.local to \
                 have install supervise it"
            );
        }
        return Ok(None);
    };
    for (key, path) in [(GUARD_SERVER_BIN_KEY, &guard.server), (GUARD_SERVER_MODEL_KEY, &guard.model)] {
        if !path.is_file() {
            return Err(format!("{key}={}: no such file", path.display()));
        }
    }
    Ok(Some(guard))
}

/// Uninstall a `kastellan-guard` unit an earlier install left behind, unless
/// this install `planned` one (then it was just installed and is not stale).
/// `uninstall` passes `None`: it removes the unit whatever the env files say.
///
/// The rewritten target no longer `Wants=` the unit, so nothing would start it
/// again — but the old process would keep running, and its unit file would sit
/// in the units dir looking live. `uninstall` stops it and removes the file. A
/// no-op when no unit is installed, which is the common case.
pub fn remove_stale_unit(sup: &dyn Supervisor, planned: Option<&GuardBackend>) -> Result<(), String> {
    if planned.is_some() {
        return Ok(());
    }
    let status = sup
        .status(GUARD_SERVICE_NAME)
        .map_err(|e| format!("cannot query {GUARD_SERVICE_NAME} via the service manager: {e}"))?;
    if status == ServiceStatus::NotInstalled {
        return Ok(());
    }
    sup.uninstall(GUARD_SERVICE_NAME)
        .map_err(|e| format!("uninstall the stale {GUARD_SERVICE_NAME} unit: {e}"))?;
    eprintln!("removed the {GUARD_SERVICE_NAME} unit (the guard backend is no longer supervised)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::plan::{build_specs, resolve_layout};

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const BIN: (&str, &str) = (GUARD_SERVER_BIN_KEY, "/home/u/.local/bin/llama-server");
    const MODEL: (&str, &str) = (GUARD_SERVER_MODEL_KEY, "/home/u/models/shield.gguf");
    const URL: (&str, &str) = (GUARD_URL_KEY, "http://127.0.0.1:8081/v1");
    const ALIAS: (&str, &str) = (GUARD_MODEL_KEY, "shieldstral");

    #[test]
    fn no_server_keys_means_no_unit_even_with_a_guard_tier_configured() {
        assert_eq!(plan_guard_backend(&env(&[URL, ALIAS])), Ok(None));
        assert_eq!(plan_guard_backend(&[]), Ok(None));
    }

    #[test]
    fn empty_values_count_as_unset() {
        let e = env(&[(GUARD_SERVER_BIN_KEY, ""), (GUARD_SERVER_MODEL_KEY, ""), URL, ALIAS]);
        assert_eq!(plan_guard_backend(&e), Ok(None));
    }

    /// No trim, like the router: the unit's `--alias` is exactly what the
    /// daemon sends, padding included.
    #[test]
    fn the_alias_is_taken_verbatim() {
        let e = env(&[BIN, MODEL, URL, (GUARD_MODEL_KEY, " shieldstral")]);
        assert_eq!(plan_guard_backend(&e).unwrap().unwrap().alias, " shieldstral");
    }

    #[test]
    fn systemd_specifier_and_variable_characters_are_refused() {
        for e in [
            env(&[(GUARD_SERVER_BIN_KEY, "/opt/llama%h/llama-server"), MODEL, URL, ALIAS]),
            env(&[BIN, (GUARD_SERVER_MODEL_KEY, "/models/$HOME.gguf"), URL, ALIAS]),
            env(&[BIN, MODEL, URL, (GUARD_MODEL_KEY, "shield%n")]),
        ] {
            let err = plan_guard_backend(&e).unwrap_err();
            assert!(err.contains("not allowed"), "{err}");
        }
    }

    #[test]
    fn full_config_derives_port_and_alias_from_the_daemons_guard_settings() {
        let got = plan_guard_backend(&env(&[BIN, MODEL, URL, ALIAS])).unwrap().unwrap();
        assert_eq!(
            got,
            GuardBackend {
                server: PathBuf::from(BIN.1),
                model: PathBuf::from(MODEL.1),
                alias: "shieldstral".into(),
                port: 8081,
            }
        );
    }

    /// `.local` is folded after `kastellan.env`; a repeated key's last value wins.
    #[test]
    fn a_repeated_key_resolves_to_its_last_value() {
        let e = env(&[BIN, MODEL, URL, ALIAS, (GUARD_URL_KEY, "http://localhost:9090/v1")]);
        assert_eq!(plan_guard_backend(&e).unwrap().unwrap().port, 9090);
    }

    #[test]
    fn half_a_server_config_is_refused() {
        for e in [env(&[BIN, URL, ALIAS]), env(&[MODEL, URL, ALIAS])] {
            let err = plan_guard_backend(&e).unwrap_err();
            assert!(err.contains("must be set together"), "{err}");
        }
    }

    #[test]
    fn relative_paths_are_refused() {
        let e = env(&[(GUARD_SERVER_BIN_KEY, "llama-server"), MODEL, URL, ALIAS]);
        let err = plan_guard_backend(&e).unwrap_err();
        assert!(err.contains(GUARD_SERVER_BIN_KEY) && err.contains("absolute"), "{err}");
        let e = env(&[BIN, (GUARD_SERVER_MODEL_KEY, "models/x.gguf"), URL, ALIAS]);
        assert!(plan_guard_backend(&e).unwrap_err().contains(GUARD_SERVER_MODEL_KEY));
    }

    #[test]
    fn a_server_without_a_guard_tier_is_refused() {
        for e in [env(&[BIN, MODEL]), env(&[BIN, MODEL, URL]), env(&[BIN, MODEL, ALIAS])] {
            let err = plan_guard_backend(&e).unwrap_err();
            assert!(err.contains("no guard tier"), "{err}");
        }
    }

    #[test]
    fn the_url_must_be_loopback_http_with_an_explicit_port() {
        for (url, needle) in [
            ("http://10.0.0.3:8081/v1", "127.0.0.1 only"),
            ("http://[::1]:8081/v1", "127.0.0.1 only"),
            ("https://127.0.0.1:8081/v1", "plain http"),
            ("http://127.0.0.1/v1", "port explicitly"),
            ("http://127.0.0.1:80/v1", "other than 80"),
            ("http://127.0.0.1:0/v1", "random port"),
            ("not a url", "does not parse"),
        ] {
            let e = env(&[BIN, MODEL, (GUARD_URL_KEY, url), ALIAS]);
            let err = plan_guard_backend(&e).unwrap_err();
            assert!(err.contains(needle), "{url}: {err}");
        }
    }

    #[test]
    fn localhost_is_accepted() {
        let e = env(&[BIN, MODEL, (GUARD_URL_KEY, "http://localhost:8081/v1"), ALIAS]);
        assert_eq!(plan_guard_backend(&e).unwrap().unwrap().port, 8081);
    }

    #[test]
    fn add_to_specs_starts_the_guard_after_postgres_and_before_core() {
        let layout = resolve_layout(Path::new("/home/u"), "u");
        let mut specs = build_specs(&layout, Path::new("/usr/lib/postgresql/18/bin/postgres"));
        let guard = plan_guard_backend(&env(&[BIN, MODEL, URL, ALIAS])).unwrap().unwrap();
        add_to_specs(&mut specs, &guard, &layout.log_dir);

        let order = ["kastellan-postgres", "kastellan-guard", "kastellan-core"];
        let names: Vec<&str> = specs.members.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, order, "members are installed in this order");
        assert_eq!(specs.target.members, order, "and the target starts them in it");

        let core = specs.members.iter().find(|s| s.name == "kastellan-core").unwrap();
        assert_eq!(core.after, ["kastellan-postgres", "kastellan-guard"]);
        let unit = specs.members.iter().find(|s| s.name == "kastellan-guard").unwrap();
        assert_eq!(unit.program, PathBuf::from(BIN.1));
        assert!(unit.args.windows(2).any(|w| w == ["--port", "8081"]));
        assert!(unit.args.windows(2).any(|w| w == ["--alias", "shieldstral"]));
        assert_eq!(unit.stderr_log, Some(layout.log_dir.join("kastellan-guard.err")));
    }

    /// The unit's context must clear the daemon's own D8 floor, or a
    /// supervised guard is one the daemon refuses to boot against.
    #[test]
    fn the_units_context_clears_the_daemons_guard_floor() {
        use crate::cassandra::guard_model::context_pin::REQUIRED_GUARD_N_CTX;
        assert!(
            u64::from(kastellan_supervisor::specs::GUARD_CONTEXT_TOKENS) >= REQUIRED_GUARD_N_CTX
        );
    }

    /// A fake service manager: answers `status` with a fixed value and
    /// records every `uninstall`.
    struct FakeSupervisor {
        status: ServiceStatus,
        uninstalled: std::cell::RefCell<Vec<String>>,
    }

    impl Supervisor for FakeSupervisor {
        fn install(&self, _: &kastellan_supervisor::ServiceSpec) -> Result<(), kastellan_supervisor::SupervisorError> {
            unreachable!("remove_stale_unit never installs")
        }
        fn start(&self, _: &str) -> Result<(), kastellan_supervisor::SupervisorError> {
            unreachable!("remove_stale_unit never starts")
        }
        fn stop(&self, _: &str) -> Result<(), kastellan_supervisor::SupervisorError> {
            unreachable!("uninstall stops the unit itself")
        }
        fn uninstall(&self, name: &str) -> Result<(), kastellan_supervisor::SupervisorError> {
            self.uninstalled.borrow_mut().push(name.to_string());
            Ok(())
        }
        fn status_with_word(
            &self,
            _: &str,
        ) -> Result<(ServiceStatus, Option<String>), kastellan_supervisor::SupervisorError> {
            Ok((self.status, None))
        }
    }

    fn fake(status: ServiceStatus) -> FakeSupervisor {
        FakeSupervisor { status, uninstalled: Default::default() }
    }

    #[test]
    fn remove_stale_unit_is_a_no_op_when_no_unit_is_installed() {
        let sup = fake(ServiceStatus::NotInstalled);
        remove_stale_unit(&sup, None).unwrap();
        assert!(sup.uninstalled.borrow().is_empty());
    }

    #[test]
    fn remove_stale_unit_uninstalls_a_left_over_unit_whatever_its_state() {
        for status in [ServiceStatus::Active, ServiceStatus::Inactive, ServiceStatus::Transitioning] {
            let sup = fake(status);
            remove_stale_unit(&sup, None).unwrap();
            assert_eq!(*sup.uninstalled.borrow(), ["kastellan-guard"], "{status:?}");
        }
    }

    /// The unit this install just planned (and installed) is not stale.
    #[test]
    fn remove_stale_unit_keeps_a_planned_unit() {
        let guard = plan_guard_backend(&env(&[BIN, MODEL, URL, ALIAS])).unwrap().unwrap();
        let sup = fake(ServiceStatus::Active);
        remove_stale_unit(&sup, Some(&guard)).unwrap();
        assert!(sup.uninstalled.borrow().is_empty());
    }

    /// A tempdir home with `kastellan.env` (and optionally the `.local`
    /// overlay) written, plus real files for the binary and the model.
    fn home_with(env_body: &str, local_body: Option<&str>) -> (tempfile::TempDir, Layout) {
        let home = tempfile::tempdir().unwrap();
        let layout = resolve_layout(home.path(), "u");
        std::fs::create_dir_all(layout.env_file.parent().unwrap()).unwrap();
        std::fs::write(&layout.env_file, env_body).unwrap();
        if let Some(body) = local_body {
            std::fs::create_dir_all(layout.env_local_file.parent().unwrap()).unwrap();
            std::fs::write(&layout.env_local_file, body).unwrap();
        }
        (home, layout)
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"").unwrap();
        p
    }

    #[test]
    fn resolve_reads_the_overlay_after_kastellan_env_and_may_be_without_it() {
        let files = tempfile::tempdir().unwrap();
        let (bin, model) = (touch(files.path(), "llama-server"), touch(files.path(), "g.gguf"));
        let base = format!(
            "{GUARD_SERVER_BIN_KEY}={}\n{GUARD_SERVER_MODEL_KEY}={}\n{GUARD_URL_KEY}=http://127.0.0.1:8081/v1\n{GUARD_MODEL_KEY}=shieldstral\n",
            bin.display(),
            model.display()
        );

        let (_home, layout) = home_with(&base, None);
        assert_eq!(resolve_from_env_files(&layout).unwrap().unwrap().port, 8081, "no overlay");

        let (_home, layout) = home_with(&base, Some(&format!("{GUARD_URL_KEY}=http://127.0.0.1:9090/v1\n")));
        assert_eq!(resolve_from_env_files(&layout).unwrap().unwrap().port, 9090, "the overlay wins");
    }

    /// A typo'd model path must fail the install, not install a unit that
    /// crash-loops on a file that is not there.
    #[test]
    fn resolve_refuses_a_missing_model_file() {
        let files = tempfile::tempdir().unwrap();
        let bin = touch(files.path(), "llama-server");
        let body = format!(
            "{GUARD_SERVER_BIN_KEY}={}\n{GUARD_SERVER_MODEL_KEY}={}\n{GUARD_URL_KEY}=http://127.0.0.1:8081/v1\n{GUARD_MODEL_KEY}=shieldstral\n",
            bin.display(),
            files.path().join("typo.gguf").display()
        );
        let (_home, layout) = home_with(&body, None);
        let err = resolve_from_env_files(&layout).unwrap_err();
        assert!(err.contains(GUARD_SERVER_MODEL_KEY) && err.contains("no such file"), "{err}");
    }
}
