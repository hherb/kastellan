# `kastellan-cli install` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `kastellan-cli install`/`uninstall` that takes a freshly-built tree to a running, supervised, per-user Kastellan (Postgres + daemon under `systemd --user`) by copying binaries + assets to a stable `~/.local` prefix, initializing the cluster, writing a tunable EnvironmentFile, installing the systemd target, and verifying it's up.

**Architecture:** Pure planning (`kastellan_core::install::plan`) computes the layout, the EnvironmentFile contents, the required/optional binary lists, and the `ServiceSpec`s; an IO layer (`kastellan_core::install`) copies files, shells out to `kastellan-db-init`, installs the supervisor target, enables linger, starts, and verifies; the CLI binary (`kastellan-cli install`) is a thin arg-parse + call wrapper. A small additive `ServiceSpec.environment_file` field carries the EnvironmentFile into the unit.

**Tech Stack:** Rust, `kastellan-supervisor` (systemd `--user` / launchd via the `Supervisor` trait), `kastellan-db` (`kastellan-db-init`, `find_pg_bin_dir`), std `process::Command`/`fs`.

## Global Constraints

- AGPL-3.0; AGPL-compatible deps only. No new external deps in this feature.
- Per-user, no root. Flat binary prefix `~/.local/lib/kastellan/` (the daemon finds workers via `current_exe()`-relative discovery — "flat install").
- Cross-platform via the `Supervisor` trait (`SystemdUser`/`LaunchAgents`) + a `--pg-bin-dir` override (macOS/Postgres.app). Linger is Linux-only. No platform-only code without a trait-provided counterpart.
- Idempotent re-runs; fail-closed with actionable error messages; verify the service is actually `active` before declaring success.
- `extra` env-var names are exact: `KASTELLAN_LLM_LOCAL_URL`, `KASTELLAN_LLM_LOCAL_MODEL`, `KASTELLAN_LLM_EMBEDDING_URL`, `KASTELLAN_LLM_EMBEDDING_MODEL`, `KASTELLAN_PROMPTS_DIR`, `KASTELLAN_L0_RULES_FILE`, `KASTELLAN_DATA_DIR`, `KASTELLAN_EGRESS_FORCE_ROUTING`.
- `cargo clippy --workspace --all-targets -D warnings` clean after every task. Files < 500 LOC. Cargo not on PATH → prefix commands with `source "$HOME/.cargo/env" && …`.

---

### Task 1: Supervisor — `ServiceSpec.environment_file` + `EnvironmentFile=` rendering

**Files:**
- Modify: `supervisor/src/lib.rs` (add field to `ServiceSpec`)
- Modify: `supervisor/src/systemd_user/builder.rs` (render `EnvironmentFile=`)
- Modify: `supervisor/src/specs.rs` (set `environment_file: None` in the 2 prod spec literals)
- Modify (add `environment_file: None` to each `ServiceSpec` literal): `supervisor/src/lib.rs` tests, `supervisor/src/systemd_user/tests.rs`, `supervisor/src/systemd_user/builder/tests.rs`, `supervisor/src/launchd_agents/tests.rs`, `supervisor/src/launchd_agents/builders/tests.rs`, `supervisor/tests/systemd_user_smoke.rs`, `supervisor/tests/target_smoke.rs`, `supervisor/tests/launchd_agents_smoke.rs`
- Test: `supervisor/src/systemd_user/builder/tests.rs`

**Interfaces:**
- Produces: `ServiceSpec` gains `pub environment_file: Option<PathBuf>` (`#[serde(default)]`); `build_unit_file` emits `EnvironmentFile=<path>` when `Some`.

- [ ] **Step 1: Write the failing builder test**

In `supervisor/src/systemd_user/builder/tests.rs`, add:

```rust
#[test]
fn environment_file_rendered_when_set() {
    let mut spec = minimal_spec("svc");
    spec.environment_file = Some(std::path::PathBuf::from("/home/u/.config/kastellan/kastellan.env"));
    let unit = build_unit_file(&spec);
    assert!(
        unit.contains("EnvironmentFile=/home/u/.config/kastellan/kastellan.env"),
        "unit should carry EnvironmentFile=; got:\n{unit}"
    );
}

#[test]
fn environment_file_absent_when_none() {
    let unit = build_unit_file(&minimal_spec("svc"));
    assert!(!unit.contains("EnvironmentFile="), "no EnvironmentFile= when None");
}
```

- [ ] **Step 2: Run — verify it fails to compile (field missing)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-supervisor environment_file 2>&1 | tail -15`
Expected: FAIL — `no field environment_file on type ServiceSpec`.

- [ ] **Step 3: Add the field to `ServiceSpec`**

In `supervisor/src/lib.rs`, inside `pub struct ServiceSpec`, after the `restart_backoff` field, add:

```rust
    /// Optional path to a systemd `EnvironmentFile=` (KEY=value lines) the
    /// service reads on start. `None` (default) renders no directive —
    /// byte-identical to today for every current caller. Used by the
    /// installer to point the core daemon at `~/.config/kastellan/kastellan.env`
    /// so operators can tune LLM/prompt/data settings without reinstalling.
    /// **Ignored on launchd** (no equivalent; the installer bakes the same
    /// values into the plist `EnvironmentVariables` there if needed later).
    #[serde(default)]
    pub environment_file: Option<PathBuf>,
```

(`PathBuf` is already imported in lib.rs.)

- [ ] **Step 4: Render it in `build_unit_file`**

In `supervisor/src/systemd_user/builder.rs`, immediately AFTER the `for (k, v) in &spec.env { … }` loop that emits `Environment=` lines, add:

```rust
    if let Some(env_file) = &spec.environment_file {
        out.push_str(&format!("EnvironmentFile={}\n", env_file.display()));
    }
```

- [ ] **Step 5: Add `environment_file: None` to every `ServiceSpec` literal**

Add `environment_file: None,` to the struct literals in: `supervisor/src/specs.rs` (the `core_service_spec` and `postgres_service_spec` bodies), `supervisor/src/lib.rs` (test helpers `spec(...)` and the literal ~line 452), `supervisor/src/systemd_user/tests.rs` (`minimal_spec`), `supervisor/src/systemd_user/builder/tests.rs` (`minimal_spec`), `supervisor/src/launchd_agents/tests.rs` (`minimal_spec`), `supervisor/src/launchd_agents/builders/tests.rs` (`minimal_spec` + the literal ~line 170), `supervisor/tests/systemd_user_smoke.rs` (~line 112), `supervisor/tests/target_smoke.rs` (`dummy_spec`), `supervisor/tests/launchd_agents_smoke.rs` (~lines 131/190/233). Use `cargo build -p kastellan-supervisor --all-targets` to surface any literal still missing the field (the compiler lists each).

- [ ] **Step 6: Run the suite + clippy**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-supervisor && cargo clippy -p kastellan-supervisor --all-targets -- -D warnings`
Expected: all pass (incl. the 2 new tests); clippy clean.

- [ ] **Step 7: Commit**

```bash
git add supervisor/src/lib.rs supervisor/src/specs.rs supervisor/src/systemd_user/builder.rs supervisor/src/systemd_user/builder/tests.rs supervisor/src/systemd_user/tests.rs supervisor/src/launchd_agents/tests.rs supervisor/src/launchd_agents/builders/tests.rs supervisor/tests/systemd_user_smoke.rs supervisor/tests/target_smoke.rs supervisor/tests/launchd_agents_smoke.rs
git commit -m "feat(supervisor): ServiceSpec.environment_file -> EnvironmentFile= unit directive"
```

---

### Task 2: Pure install plan — `kastellan_core::install::plan`

**Files:**
- Create: `core/src/install/mod.rs` (declares `pub mod plan;` + re-exports — IO added in Task 3)
- Create: `core/src/install/plan.rs`
- Modify: `core/src/lib.rs` (add `pub mod install;`)

**Interfaces:**
- Consumes: `kastellan_supervisor::specs::{core_service_spec, postgres_service_spec, kastellan_target_spec}`, `kastellan_supervisor::{ServiceSpec, TargetSpec}`.
- Produces:
  - `pub struct Layout { home, user, bin_dir, assets_dir, prompts_dir, l0_rules_file, data_dir, config_dir, env_file, log_dir }` (all `PathBuf` except `user: String`)
  - `pub fn resolve_layout(home: &Path, user: &str) -> Layout`
  - `pub fn render_env_file(model: &str, url: &str, embedding_model: Option<&str>, layout: &Layout) -> String`
  - `pub fn required_binaries() -> &'static [&'static str]` / `pub fn optional_binaries() -> &'static [&'static str]`
  - `pub fn default_llm_url() -> &'static str`
  - `pub struct InstallSpecs { pub members: Vec<ServiceSpec>, pub target: TargetSpec }` + `pub fn build_specs(layout: &Layout, postgres_binary: &Path) -> InstallSpecs`
  - `pub struct InstallArgs { pub llm_model: String, pub llm_url: String, pub embedding_model: Option<String>, pub pg_bin_dir: Option<PathBuf>, pub from: Option<PathBuf>, pub no_start: bool }` + `pub fn parse_install_args(args: &[String]) -> Result<InstallArgs, String>`

- [ ] **Step 1: Write the failing tests**

Create `core/src/install/plan.rs` with ONLY the tests first (and `use` lines), to drive the API:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn layout() -> Layout {
        resolve_layout(Path::new("/home/u"), "u")
    }

    #[test]
    fn layout_uses_xdg_per_user_paths() {
        let l = layout();
        assert_eq!(l.bin_dir, PathBuf::from("/home/u/.local/lib/kastellan"));
        assert_eq!(l.assets_dir, PathBuf::from("/home/u/.local/share/kastellan"));
        assert_eq!(l.prompts_dir, PathBuf::from("/home/u/.local/share/kastellan/prompts"));
        assert_eq!(l.l0_rules_file, PathBuf::from("/home/u/.local/share/kastellan/seeds/memory/l0_meta_rules.toml"));
        assert_eq!(l.data_dir, PathBuf::from("/home/u/.local/share/kastellan/pg/data"));
        assert_eq!(l.config_dir, PathBuf::from("/home/u/.config/kastellan"));
        assert_eq!(l.env_file, PathBuf::from("/home/u/.config/kastellan/kastellan.env"));
        assert_eq!(l.log_dir, PathBuf::from("/home/u/.local/state/kastellan"));
    }

    #[test]
    fn env_file_has_all_required_keys_and_prefix_paths() {
        let l = layout();
        let s = render_env_file("my-model", "http://127.0.0.1:8000", Some("emb-model"), &l);
        assert!(s.contains("KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:8000\n"));
        assert!(s.contains("KASTELLAN_LLM_LOCAL_MODEL=my-model\n"));
        assert!(s.contains("KASTELLAN_LLM_EMBEDDING_URL=http://127.0.0.1:8000\n"));
        assert!(s.contains("KASTELLAN_LLM_EMBEDDING_MODEL=emb-model\n"));
        assert!(s.contains("KASTELLAN_PROMPTS_DIR=/home/u/.local/share/kastellan/prompts\n"));
        assert!(s.contains("KASTELLAN_L0_RULES_FILE=/home/u/.local/share/kastellan/seeds/memory/l0_meta_rules.toml\n"));
        assert!(s.contains("KASTELLAN_DATA_DIR=/home/u/.local/share/kastellan/pg/data\n"));
    }

    #[test]
    fn env_file_omits_embedding_model_when_absent() {
        let s = render_env_file("m", "u", None, &layout());
        assert!(!s.contains("KASTELLAN_LLM_EMBEDDING_MODEL="));
    }

    #[test]
    fn required_binaries_include_daemon_and_egress_proxy() {
        let r = required_binaries();
        assert!(r.contains(&"kastellan"));
        assert!(r.contains(&"kastellan-db-init"));
        assert!(r.contains(&"kastellan-worker-egress-proxy"));
        // optional set holds the on-demand workers
        assert!(optional_binaries().contains(&"kastellan-worker-matrix"));
        assert!(optional_binaries().contains(&"kastellan-worker-lockdown-exec"));
    }

    #[test]
    fn specs_point_core_at_installed_binary_and_env_file() {
        let l = layout();
        let specs = build_specs(&l, Path::new("/usr/lib/postgresql/18/bin/postgres"));
        assert_eq!(specs.members.len(), 2);
        let core = specs.members.iter().find(|s| s.name == "kastellan-core").unwrap();
        assert_eq!(core.program, PathBuf::from("/home/u/.local/lib/kastellan/kastellan"));
        assert_eq!(core.environment_file.as_deref(), Some(l.env_file.as_path()));
        let pg = specs.members.iter().find(|s| s.name == "kastellan-postgres").unwrap();
        assert_eq!(pg.program, PathBuf::from("/usr/lib/postgresql/18/bin/postgres"));
        assert!(specs.target.members.contains(&"kastellan-postgres".to_string()));
        assert!(specs.target.members.contains(&"kastellan-core".to_string()));
    }

    #[test]
    fn parse_requires_llm_model_and_defaults_url() {
        let a = parse_install_args(&["--llm-model".into(), "m".into()]).unwrap();
        assert_eq!(a.llm_model, "m");
        assert_eq!(a.llm_url, default_llm_url());
        assert!(!a.no_start);
        assert!(parse_install_args(&[]).is_err()); // missing --llm-model
        assert!(parse_install_args(&["--bogus".into()]).is_err());
        let a2 = parse_install_args(&["--llm-model".into(), "m".into(), "--llm-url".into(), "http://x:1".into(), "--no-start".into()]).unwrap();
        assert_eq!(a2.llm_url, "http://x:1");
        assert!(a2.no_start);
    }
}
```

- [ ] **Step 2: Run — verify it fails (module/symbols missing)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::plan 2>&1 | tail -15`
Expected: FAIL — unresolved `install` module / `resolve_layout` etc. not found.

- [ ] **Step 3: Implement the module + wire it in**

In `core/src/lib.rs` add (alphabetically among the `pub mod` lines): `pub mod install;`

Create `core/src/install/mod.rs`:

```rust
//! Operator installer for a per-user supervised Kastellan
//! (`kastellan-cli install`). Pure layout/spec planning in [`plan`];
//! the IO orchestration (copy, db-init, supervisor install, verify)
//! is added alongside.

pub mod plan;
```

Create `core/src/install/plan.rs` (above the `#[cfg(test)]` block):

```rust
//! Pure planning for `kastellan-cli install`: per-user layout, the
//! EnvironmentFile contents, the binary sets to copy, and the
//! `ServiceSpec`s. No I/O — every function is deterministic.

use std::path::{Path, PathBuf};

use kastellan_supervisor::specs::{core_service_spec, kastellan_target_spec, postgres_service_spec};
use kastellan_supervisor::{ServiceSpec, TargetSpec};

/// Resolved per-user install paths.
pub struct Layout {
    pub home: PathBuf,
    pub user: String,
    pub bin_dir: PathBuf,
    pub assets_dir: PathBuf,
    pub prompts_dir: PathBuf,
    pub l0_rules_file: PathBuf,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub env_file: PathBuf,
    pub log_dir: PathBuf,
}

/// Compute the per-user layout from `$HOME` + `$USER`. Pure.
pub fn resolve_layout(home: &Path, user: &str) -> Layout {
    let assets_dir = home.join(".local/share/kastellan");
    let config_dir = home.join(".config/kastellan");
    Layout {
        home: home.to_path_buf(),
        user: user.to_string(),
        bin_dir: home.join(".local/lib/kastellan"),
        prompts_dir: assets_dir.join("prompts"),
        l0_rules_file: assets_dir.join("seeds/memory/l0_meta_rules.toml"),
        data_dir: assets_dir.join("pg/data"),
        env_file: config_dir.join("kastellan.env"),
        log_dir: home.join(".local/state/kastellan"),
        assets_dir,
        config_dir,
    }
}

/// The default local LLM URL per OS (Linux: vLLM/SGLang :8000; macOS: Ollama :11434).
pub fn default_llm_url() -> &'static str {
    if cfg!(target_os = "macos") {
        "http://127.0.0.1:11434"
    } else {
        "http://127.0.0.1:8000"
    }
}

/// Render the `kastellan.env` EnvironmentFile contents.
pub fn render_env_file(model: &str, url: &str, embedding_model: Option<&str>, layout: &Layout) -> String {
    let mut s = String::new();
    s.push_str(&format!("KASTELLAN_LLM_LOCAL_URL={url}\n"));
    s.push_str(&format!("KASTELLAN_LLM_LOCAL_MODEL={model}\n"));
    s.push_str(&format!("KASTELLAN_LLM_EMBEDDING_URL={url}\n"));
    if let Some(em) = embedding_model {
        s.push_str(&format!("KASTELLAN_LLM_EMBEDDING_MODEL={em}\n"));
    }
    s.push_str(&format!("KASTELLAN_PROMPTS_DIR={}\n", layout.prompts_dir.display()));
    s.push_str(&format!("KASTELLAN_L0_RULES_FILE={}\n", layout.l0_rules_file.display()));
    s.push_str(&format!("KASTELLAN_DATA_DIR={}\n", layout.data_dir.display()));
    s
}

/// Binaries whose absence aborts the install (daemon + db-init + the
/// fail-closed egress proxy + the operator CLI).
pub fn required_binaries() -> &'static [&'static str] {
    &[
        "kastellan",
        "kastellan-cli",
        "kastellan-db-init",
        "kastellan-worker-egress-proxy",
    ]
}

/// On-demand workers: copied when present in the build dir, skipped (with
/// a log line) when not — their absence only disables that one tool.
pub fn optional_binaries() -> &'static [&'static str] {
    &[
        "kastellan-worker-shell-exec",
        "kastellan-worker-web-fetch",
        "kastellan-worker-web-search",
        "kastellan-worker-python-exec",
        "kastellan-worker-matrix",
        "kastellan-worker-lockdown-exec",
    ]
}

/// The supervisor specs + target for the install, with absolute prefix paths.
pub struct InstallSpecs {
    pub members: Vec<ServiceSpec>,
    pub target: TargetSpec,
}

/// Build the postgres + core service specs (start order) + the target.
/// `postgres_binary` is the resolved absolute path to the `postgres` exe.
pub fn build_specs(layout: &Layout, postgres_binary: &Path) -> InstallSpecs {
    let postgres = postgres_service_spec(postgres_binary, &layout.data_dir, &layout.log_dir);
    let mut core = core_service_spec(&layout.bin_dir.join("kastellan"), &layout.log_dir);
    core.environment_file = Some(layout.env_file.clone());
    InstallSpecs { members: vec![postgres, core], target: kastellan_target_spec() }
}

/// Parsed `install` arguments.
pub struct InstallArgs {
    pub llm_model: String,
    pub llm_url: String,
    pub embedding_model: Option<String>,
    pub pg_bin_dir: Option<PathBuf>,
    pub from: Option<PathBuf>,
    pub no_start: bool,
}

/// Parse `install [--llm-model <m>] [--llm-url <u>] [--embedding-model <m>] [--pg-bin-dir <d>] [--from <d>] [--no-start]`.
pub fn parse_install_args(args: &[String]) -> Result<InstallArgs, String> {
    let (mut model, mut url, mut emb, mut pg, mut from, mut no_start) =
        (None::<String>, None::<String>, None::<String>, None::<PathBuf>, None::<PathBuf>, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--llm-model" => { model = Some(take(args, &mut i, "--llm-model")?); }
            "--llm-url" => { url = Some(take(args, &mut i, "--llm-url")?); }
            "--embedding-model" => { emb = Some(take(args, &mut i, "--embedding-model")?); }
            "--pg-bin-dir" => { pg = Some(PathBuf::from(take(args, &mut i, "--pg-bin-dir")?)); }
            "--from" => { from = Some(PathBuf::from(take(args, &mut i, "--from")?)); }
            "--no-start" => { no_start = true; i += 1; }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let llm_model = model.ok_or("install requires --llm-model <name>")?;
    Ok(InstallArgs {
        llm_model,
        llm_url: url.unwrap_or_else(|| default_llm_url().to_string()),
        embedding_model: emb,
        pg_bin_dir: pg,
        from,
        no_start,
    })
}

fn take(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    let v = args.get(*i + 1).ok_or_else(|| format!("{flag} requires a value"))?.clone();
    *i += 2;
    Ok(v)
}
```

- [ ] **Step 4: Run — verify the tests pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::plan -- --nocapture`
Expected: PASS (6 tests).

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings`

```bash
git add core/src/lib.rs core/src/install/mod.rs core/src/install/plan.rs
git commit -m "feat(install): pure layout/env/spec planning module"
```

---

### Task 3: Install IO orchestration (lib) + hermetic file-producing test

**Files:**
- Create: `core/src/install/run.rs` (IO: copy, db-init, install target, linger, start, verify, uninstall)
- Modify: `core/src/install/mod.rs` (add `pub mod run;` + re-exports)
- Test: `core/tests/install_e2e.rs`

**Interfaces:**
- Consumes: `plan::{Layout, InstallArgs, InstallSpecs, resolve_layout, render_env_file, build_specs, required_binaries, optional_binaries}`, `kastellan_supervisor::default_supervisor`, `kastellan_db::{find_pg_bin_dir, default_pg_bin_dir_candidates}`.
- Produces:
  - `pub fn prepare_filesystem(layout: &Layout, from_dir: &Path, assets_src: &Path, args: &InstallArgs) -> Result<Vec<String>, String>` — creates dirs, copies required+optional binaries (atomic temp+rename), copies `prompts/` + `seeds/` from `assets_src`, writes `kastellan.env` (0600). Returns the list of copied binary names. Fails closed if a required binary is missing in `from_dir`.
  - `pub fn run_install(args: InstallArgs) -> Result<(), String>` — full orchestration.
  - `pub fn run_uninstall(purge: bool) -> Result<(), String>`.

- [ ] **Step 1: Write the failing hermetic test**

Create `core/tests/install_e2e.rs`:

```rust
//! Hermetic test of the file-producing half of `kastellan-cli install`
//! (`kastellan_core::install::prepare_filesystem`). No systemd, no PG —
//! drives the copy + env-file generation against a temp HOME and a fake
//! build dir. The live install (db-init + systemd start) is verified by
//! running `kastellan-cli install` on a real host (the DGX).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs;
use std::path::Path;

use kastellan_core::install::plan::{resolve_layout, required_binaries, InstallArgs};
use kastellan_core::install::run::prepare_filesystem;

fn touch_exec(dir: &Path, name: &str) {
    let p = dir.join(name);
    fs::write(&p, b"#!/bin/sh\ntrue\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn prepare_filesystem_populates_prefix_and_env_file() {
    let tmp = std::env::temp_dir().join(format!("kastellan-install-test-{}", std::process::id()));
    let home = tmp.join("home");
    let from = tmp.join("from");
    let assets_src = tmp.join("src");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(assets_src.join("prompts")).unwrap();
    fs::create_dir_all(assets_src.join("seeds/memory")).unwrap();
    fs::write(assets_src.join("prompts/system.txt"), b"hi").unwrap();
    fs::write(assets_src.join("seeds/memory/l0_meta_rules.toml"), b"[x]\n").unwrap();
    for b in required_binaries() {
        touch_exec(&from, b);
    }

    let layout = resolve_layout(&home, "tester");
    let args = InstallArgs {
        llm_model: "test-model".into(),
        llm_url: "http://127.0.0.1:8000".into(),
        embedding_model: None,
        pg_bin_dir: None,
        from: Some(from.clone()),
        no_start: true,
    };

    let copied = prepare_filesystem(&layout, &from, &assets_src, &args).expect("prepare_filesystem");

    // Required binaries landed in the flat prefix, executable.
    for b in required_binaries() {
        let dest = layout.bin_dir.join(b);
        assert!(dest.is_file(), "missing installed binary {b}");
        assert!(copied.contains(&b.to_string()));
    }
    // Assets copied.
    assert!(layout.prompts_dir.join("system.txt").is_file());
    assert!(layout.l0_rules_file.is_file());
    // Env file rendered with the model + prefix data dir.
    let env = fs::read_to_string(&layout.env_file).unwrap();
    assert!(env.contains("KASTELLAN_LLM_LOCAL_MODEL=test-model\n"));
    assert!(env.contains(&format!("KASTELLAN_DATA_DIR={}\n", layout.data_dir.display())));

    fs::remove_dir_all(&tmp).ok();
}

#[test]
fn prepare_filesystem_fails_closed_on_missing_required_binary() {
    let tmp = std::env::temp_dir().join(format!("kastellan-install-miss-{}", std::process::id()));
    let home = tmp.join("home");
    let from = tmp.join("from");
    let assets_src = tmp.join("src");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(assets_src.join("prompts")).unwrap();
    fs::create_dir_all(assets_src.join("seeds/memory")).unwrap();
    // Deliberately copy only ONE required binary → must fail.
    touch_exec(&from, "kastellan");

    let layout = resolve_layout(&home, "tester");
    let args = InstallArgs {
        llm_model: "m".into(), llm_url: "u".into(), embedding_model: None,
        pg_bin_dir: None, from: Some(from.clone()), no_start: true,
    };
    let err = prepare_filesystem(&layout, &from, &assets_src, &args).unwrap_err();
    assert!(err.contains("kastellan-db-init") || err.contains("kastellan-worker-egress-proxy"),
            "error should name a missing required binary; got: {err}");

    fs::remove_dir_all(&tmp).ok();
}
```

- [ ] **Step 2: Run — verify it fails (run module missing)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test install_e2e 2>&1 | tail -15`
Expected: FAIL — unresolved `kastellan_core::install::run`.

- [ ] **Step 3: Implement `run.rs` + wire the module**

In `core/src/install/mod.rs` add: `pub mod run;`

Create `core/src/install/run.rs`:

```rust
//! IO orchestration for `kastellan-cli install`/`uninstall`. Thin over the
//! pure `plan` module: copy binaries + assets, init the cluster (shelling
//! out to the idempotent `kastellan-db-init`), install the supervisor
//! target, enable linger, start, and verify. Every external failure maps
//! to an actionable message.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kastellan_supervisor::default_supervisor;

use super::plan::{
    build_specs, optional_binaries, render_env_file, required_binaries, InstallArgs, Layout,
};

/// Create dirs, copy binaries + assets, write the EnvironmentFile.
/// Returns the names of binaries actually copied. Fails closed if a
/// required binary is absent in `from_dir`.
pub fn prepare_filesystem(
    layout: &Layout,
    from_dir: &Path,
    assets_src: &Path,
    args: &InstallArgs,
) -> Result<Vec<String>, String> {
    for d in [&layout.bin_dir, &layout.assets_dir, &layout.config_dir, &layout.log_dir, &layout.data_dir] {
        fs::create_dir_all(d).map_err(|e| format!("create {}: {e}", d.display()))?;
    }

    // Required binaries: all must be present.
    for name in required_binaries() {
        let src = from_dir.join(name);
        if !src.is_file() {
            return Err(format!(
                "required binary {name:?} not found in {} — run `cargo build --release` first",
                from_dir.display()
            ));
        }
        copy_exec(&src, &layout.bin_dir.join(name))?;
    }
    let mut copied: Vec<String> = required_binaries().iter().map(|s| s.to_string()).collect();
    // Optional binaries: copy when present.
    for name in optional_binaries() {
        let src = from_dir.join(name);
        if src.is_file() {
            copy_exec(&src, &layout.bin_dir.join(name))?;
            copied.push(name.to_string());
        } else {
            eprintln!("note: optional worker {name} not found in build dir — skipping (its tool will be disabled)");
        }
    }

    copy_tree(&assets_src.join("prompts"), &layout.prompts_dir)?;
    copy_tree(&assets_src.join("seeds"), &layout.assets_dir.join("seeds"))?;

    let env = render_env_file(&args.llm_model, &args.llm_url, args.embedding_model.as_deref(), layout);
    write_private(&layout.env_file, env.as_bytes())?;

    Ok(copied)
}

/// Full install: prepare filesystem → db-init → install target → linger → start → verify.
pub fn run_install(args: InstallArgs) -> Result<(), String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("$HOME unset")?);
    let user = std::env::var("USER").map_err(|_| "$USER unset".to_string())?;
    let layout = super::plan::resolve_layout(&home, &user);

    // `--from` defaults to the directory of the running kastellan-cli (target/release in a build tree).
    let from = match &args.from {
        Some(p) => p.clone(),
        None => std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .ok_or("cannot resolve current_exe dir; pass --from <built-bin-dir>")?,
    };
    // Assets source = the repo (cwd) prompts/ + seeds/.
    let assets_src = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;

    let copied = prepare_filesystem(&layout, &from, &assets_src, &args)?;
    eprintln!("installed {} binaries into {}", copied.len(), layout.bin_dir.display());

    // db-init (idempotent) via the just-copied binary.
    let mut dbinit = Command::new(layout.bin_dir.join("kastellan-db-init"));
    dbinit.arg("--data-dir").arg(&layout.data_dir);
    if let Some(bd) = &args.pg_bin_dir {
        dbinit.arg("--bin-dir").arg(bd);
    }
    run_checked(&mut dbinit, "kastellan-db-init")?;

    // Resolve the postgres binary path for the unit.
    let pg_bin_dir = match &args.pg_bin_dir {
        Some(d) => d.clone(),
        None => kastellan_db::find_pg_bin_dir(&kastellan_db::default_pg_bin_dir_candidates())
            .ok_or("could not find Postgres bin dir; install PostgreSQL 18 or pass --pg-bin-dir <dir>")?,
    };
    let specs = build_specs(&layout, &pg_bin_dir.join("postgres"));

    let sup = default_supervisor();
    sup.install_target(&specs.target, &specs.members).map_err(|e| format!("install units: {e}"))?;
    eprintln!("installed systemd units for kastellan.target");

    if args.no_start {
        eprintln!("--no-start: units installed but not started. Start with: systemctl --user start kastellan.target");
        return Ok(());
    }

    // Linger so --user services persist on a headless box (Linux only).
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("loginctl").arg("enable-linger").arg(&layout.user).status();
    }

    sup.start_target(&specs.target).map_err(|e| format!("start kastellan.target: {e}"))?;
    verify_running(&layout)?;
    eprintln!("kastellan.target is up. Check: systemctl --user status kastellan.target");
    Ok(())
}

/// Stop + remove the units. `--purge` also deletes the prefix + data dir.
pub fn run_uninstall(purge: bool) -> Result<(), String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("$HOME unset")?);
    let user = std::env::var("USER").map_err(|_| "$USER unset".to_string())?;
    let layout = super::plan::resolve_layout(&home, &user);
    let specs = build_specs(&layout, Path::new("/usr/bin/postgres")); // path irrelevant for stop/uninstall

    let sup = default_supervisor();
    let _ = sup.stop_target(&specs.target);
    sup.uninstall_target(&specs.target).map_err(|e| format!("uninstall units: {e}"))?;
    eprintln!("removed kastellan.target units");

    if purge {
        for d in [&layout.bin_dir, &layout.assets_dir, &layout.config_dir] {
            fs::remove_dir_all(d).map_err(|e| format!("purge {}: {e}", d.display()))?;
        }
        eprintln!("purged prefix + data dir (cluster + secrets deleted)");
    } else {
        eprintln!("kept data dir + secrets at {} (use --purge to delete)", layout.assets_dir.display());
    }
    Ok(())
}

/// Poll for the PG socket, then confirm both services are `active`.
fn verify_running(layout: &Layout) -> Result<(), String> {
    let socket = layout.data_dir.join("sockets/.s.PGSQL.5432");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if !socket.exists() {
        return Err(format!(
            "Postgres socket never appeared at {}. Inspect: journalctl --user -u kastellan-postgres -n 50",
            socket.display()
        ));
    }
    for svc in ["kastellan-postgres", "kastellan-core"] {
        let out = Command::new("systemctl").args(["--user", "is-active", svc]).output()
            .map_err(|e| format!("systemctl is-active {svc}: {e}"))?;
        let state = String::from_utf8_lossy(&out.stdout);
        if state.trim() != "active" {
            return Err(format!(
                "{svc} is not active (state: {}). Inspect: journalctl --user -u {svc} -n 50",
                state.trim()
            ));
        }
    }
    Ok(())
}

fn copy_exec(src: &Path, dest: &Path) -> Result<(), String> {
    let tmp = dest.with_extension("tmp-install");
    fs::copy(src, &tmp).map_err(|e| format!("copy {} -> {}: {e}", src.display(), tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, dest).map_err(|e| format!("rename into {}: {e}", dest.display()))?;
    Ok(())
}

fn copy_tree(src: &Path, dest: &Path) -> Result<(), String> {
    if !src.exists() {
        return Err(format!("asset source missing: {} (run install from the repo root)", src.display()));
    }
    fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    for entry in fs::read_dir(src).map_err(|e| format!("read_dir {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

fn run_checked(cmd: &mut Command, label: &str) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("spawn {label}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{label} failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}
```

Re-export in `core/src/install/mod.rs` for convenience: add `pub use run::{prepare_filesystem, run_install, run_uninstall};`.

- [ ] **Step 4: Run — verify the hermetic tests pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test install_e2e -- --nocapture`
Expected: PASS (2 tests).

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings`

```bash
git add core/src/install/mod.rs core/src/install/run.rs core/tests/install_e2e.rs
git commit -m "feat(install): IO orchestration (copy/db-init/install/start/verify) + hermetic test"
```

---

### Task 4: CLI `install`/`uninstall` subcommands

**Files:**
- Create: `core/src/bin/kastellan-cli/install.rs`
- Modify: `core/src/bin/kastellan-cli/main.rs` (`mod install;` + dispatch + usage)

**Interfaces:**
- Consumes: `kastellan_core::install::{plan::parse_install_args, run::{run_install, run_uninstall}}`, `crate::common` (none needed beyond ExitCode).

- [ ] **Step 1: Implement the thin CLI module**

Create `core/src/bin/kastellan-cli/install.rs`:

```rust
//! `install` / `uninstall` — operator bring-up of a per-user supervised
//! Kastellan. Thin wrapper over `kastellan_core::install`.

use std::io::{self, Write};
use std::process::ExitCode;

use kastellan_core::install::plan::parse_install_args;
use kastellan_core::install::run::{run_install, run_uninstall};

pub(crate) fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("uninstall") => uninstall(&args[1..]),
        // `install` is the implicit verb: `kastellan-cli install [flags]`
        _ if !args.is_empty() && args[0].starts_with("--") => install(args),
        Some("install") => install(&args[1..]),
        _ => {
            eprintln!("usage: kastellan-cli install [--llm-model <m>] [--llm-url <u>] [--embedding-model <m>] [--pg-bin-dir <d>] [--from <d>] [--no-start]");
            eprintln!("       kastellan-cli uninstall [--purge]");
            ExitCode::from(2)
        }
    }
}

fn install(args: &[String]) -> ExitCode {
    let parsed = match parse_install_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\nusage: kastellan-cli install --llm-model <name> [--llm-url <url>] [--embedding-model <name>] [--pg-bin-dir <dir>] [--from <dir>] [--no-start]");
            return ExitCode::from(2);
        }
    };
    match run_install(parsed) {
        Ok(()) => ExitCode::from(0),
        Err(e) => { eprintln!("install failed: {e}"); ExitCode::from(1) }
    }
}

fn uninstall(args: &[String]) -> ExitCode {
    let purge = match args {
        [] => false,
        [flag] if flag == "--purge" => true,
        _ => { eprintln!("usage: kastellan-cli uninstall [--purge]"); return ExitCode::from(2); }
    };
    if purge {
        eprint!("--purge DELETES the Postgres cluster + stored secrets. Type 'purge' to confirm: ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() || line.trim() != "purge" {
            eprintln!("aborted.");
            return ExitCode::from(1);
        }
    }
    match run_uninstall(purge) {
        Ok(()) => ExitCode::from(0),
        Err(e) => { eprintln!("uninstall failed: {e}"); ExitCode::from(1) }
    }
}
```

- [ ] **Step 2: Wire into `main.rs`**

In `core/src/bin/kastellan-cli/main.rs`: add `mod install;` beside the other `mod` lines, and dispatch arms beside `"pair"`:

```rust
        "install"   => install::run(&args[1..]),
        "uninstall" => install::run(&args[1..]),
```

(Note: passing `&args[1..]` keeps the verb in `args[0]` so `install::run` can route both `install` and `uninstall`.) Add usage lines to the top-of-file `//!` doc block and the printed help:

```
    kastellan-cli install [--llm-model <m>] [--llm-url <u>] [--no-start]
    kastellan-cli uninstall [--purge]
```

- [ ] **Step 3: Build + clippy**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --bin kastellan-cli && cargo clippy --workspace --all-targets -- -D warnings`
Expected: builds; clippy clean.

- [ ] **Step 4: Commit**

```bash
git add core/src/bin/kastellan-cli/install.rs core/src/bin/kastellan-cli/main.rs
git commit -m "feat(cli): kastellan-cli install|uninstall"
```

---

### Task 5: DGX acceptance + record (live gate)

**Files:** none (verification + handover note deferred to post-merge consolidation).

- [ ] **Step 1: Build on the DGX**

`ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && git fetch origin && git checkout feat/kastellan-installer && cargo build --release'`
Expected: workspace release build succeeds.

- [ ] **Step 2: Run the installer**

`ssh dgx 'cd ~/src/kastellan && ./target/release/kastellan-cli install --llm-model <model-served-on-:8000>'`
Expected: copies binaries, db-init runs, units installed, linger enabled, target started, verify prints "kastellan.target is up". (Use the model name actually served on the DGX's vLLM/SGLang at :8000.)

- [ ] **Step 3: Confirm it's up + the secret CLI works**

`ssh dgx 'systemctl --user status kastellan.target --no-pager | head; ~/.local/lib/kastellan/kastellan-cli secret list'`
Expected: target active; `secret list` connects (prints `(no secrets)` or rows — no connection error).

- [ ] **Step 4: Store the Matrix secret** (operator does this — needs the password)

`printf %s '<@kastellan password>' | ~/.local/lib/kastellan/kastellan-cli secret put matrix_kastellan_password`

---

## Self-Review

- **Spec coverage:** layout (Task 2 `resolve_layout`) ✓; command surface install/uninstall + flags (Task 4 + `parse_install_args`) ✓; full bring-up sequence — copy/db-init/env-file/install_target/linger/start/verify (Task 3 `run_install`) ✓; per-user flat prefix (Task 2 layout, Task 3 copy) ✓; EnvironmentFile mechanism (Task 1 supervisor + Task 2 `build_specs` + Task 3 render) ✓; idempotent (db-init idempotent; create_dir_all; temp+rename copy) ✓; fail-closed actionable errors (required-binary check, db-init/PG-not-found/verify messages) ✓; `--pg-bin-dir`/`--from`/`--no-start`/`--embedding-model` ✓; uninstall + `--purge` typed confirm (Task 4) ✓; cross-platform via `default_supervisor` + cfg-gated linger ✓; pure/IO seam + hermetic test + DGX acceptance (Tasks 2/3/5) ✓.
- **Placeholder scan:** none — every step has concrete code/commands.
- **Type consistency:** `Layout`, `InstallArgs`, `InstallSpecs`, `resolve_layout`, `render_env_file`, `required_binaries`/`optional_binaries`, `build_specs`, `parse_install_args` defined in Task 2 and used with identical signatures in Tasks 3/4; `prepare_filesystem`/`run_install`/`run_uninstall` defined in Task 3 and consumed in Task 4; `ServiceSpec.environment_file: Option<PathBuf>` defined in Task 1 and set in Task 2's `build_specs`.
- **Note for implementer:** Task 1 changes a shared struct — confirm `cargo build -p kastellan-supervisor --all-targets` is clean (every `ServiceSpec` literal updated) before Step 6. Task 3's `find_pg_bin_dir`/`default_pg_bin_dir_candidates` are `kastellan_db` public fns (per `db/src/lib.rs`); if the exact names differ, grep `db/src/lib.rs` for `pub fn .*pg_bin_dir` and adjust.
