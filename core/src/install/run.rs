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
    // The cluster's initdb superuser MUST be this OS user: the daemon connects
    // via peer auth as `ConnectSpec::default_for` (= current_os_user), so the
    // superuser role has to match `$USER` — not kastellan-db-init's default
    // `kastellan` role (which would yield `role "<user>" does not exist`).
    dbinit.arg("--username").arg(&layout.user);
    if let Some(bd) = &args.pg_bin_dir {
        dbinit.arg("--bin-dir").arg(bd);
    }
    run_checked(&mut dbinit, "kastellan-db-init")?;

    // Resolve the postgres binary path for the unit.
    let pg_bin_dir = match &args.pg_bin_dir {
        Some(d) => d.clone(),
        None => kastellan_db::find_pg_bin_dir(&kastellan_db::default_pg_bin_dir_candidates())
            .map_err(|e| format!("could not find Postgres bin dir ({e}); install PostgreSQL 18 or pass --pg-bin-dir <dir>"))?,
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
        for d in [&layout.bin_dir, &layout.assets_dir, &layout.config_dir, &layout.log_dir] {
            fs::remove_dir_all(d).map_err(|e| format!("purge {}: {e}", d.display()))?;
        }
        eprintln!("purged prefix + data + logs (cluster + secrets deleted)");
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
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        f.write_all(bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
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
