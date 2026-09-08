//! IO orchestration for `kastellan-cli install`/`uninstall`. Thin over the
//! pure `plan` module: copy binaries + assets, init the cluster (shelling
//! out to the idempotent `kastellan-db-init`), install the supervisor
//! target, enable linger, start, and verify. Every external failure maps
//! to an actionable message.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kastellan_supervisor::{default_supervisor, ServiceStatus, Supervisor};

use super::plan::{
    build_specs, cli_path_precedence_note, estimate_model_bytes, is_local_ollama, memory_suffices,
    optional_binaries, render_env_file, required_binaries, required_memory_bytes, InstallArgs,
    Layout,
};
use crate::install::env_diff::EnvDiff;

/// How an operator makes an overlay edit take effect, per platform.
///
/// systemd re-reads `EnvironmentFile=` at every service start, so a restart is
/// enough. launchd has no such directive — the backend folds the pairs into the
/// plist at *install* time — so an edit needs another install, and it must carry
/// the same flags as the original, because a bare re-run regenerates
/// `kastellan.env` from flag defaults and reverts them.
///
/// A `const` per platform rather than a `cfg!()` inside the message: the test
/// then asserts through the same const it renders, so the two cannot drift, and
/// it passes on whichever host runs it.
#[cfg(target_os = "macos")]
pub(crate) const OVERLAY_APPLY_HINT: &str =
    "then re-run `kastellan-cli install` WITH THE SAME FLAGS you used originally — launchd has no \
     EnvironmentFile=, so the values are baked into the plist at install time and a restart alone \
     will not pick them up";
#[cfg(not(target_os = "macos"))]
pub(crate) const OVERLAY_APPLY_HINT: &str =
    "then `systemctl --user restart kastellan-core` to pick them up";

/// What the supervisor backend calls the things it just installed.
#[cfg(target_os = "macos")]
pub(crate) const UNIT_NOUN: &str = "launchd agents";
#[cfg(not(target_os = "macos"))]
pub(crate) const UNIT_NOUN: &str = "systemd units";

/// How the operator starts the target by hand after `--no-start`.
#[cfg(target_os = "macos")]
pub(crate) const START_TARGET_HINT: &str = "launchctl kickstart gui/$UID/kastellan-core";
#[cfg(not(target_os = "macos"))]
pub(crate) const START_TARGET_HINT: &str = "systemctl --user start kastellan.target";

/// How the operator inspects a running (or failed) install.
///
/// Both backends redirect the daemon's stdout/stderr into `log_dir`, so the
/// files are the portable half and come first; the service-manager command
/// differs and must not name a binary the host does not have — advising
/// `journalctl` on macOS is exactly the kind of dead-end this const exists to
/// prevent.
#[cfg(target_os = "macos")]
pub(crate) const INSPECT_LOGS_HINT: &str =
    "~/.local/state/kastellan/kastellan-core.err (and -postgres.err), \
     or `launchctl print gui/$UID/kastellan-core`";
#[cfg(not(target_os = "macos"))]
pub(crate) const INSPECT_LOGS_HINT: &str =
    "~/.local/state/kastellan/kastellan-core.err (and -postgres.err), \
     or `journalctl --user -u kastellan-core -n 50`";

/// Build the operator-facing warning for a destructive env-file rewrite.
///
/// Pure, so the text an operator actually reads is unit-testable — the point
/// of this warning is that it is SEEN, and a message with no test is one
/// refactor away from silently losing a line.
///
/// Key names only: values are never interpolated here. The operator reads
/// values from the backup, which keeps anything secret-shaped out of the
/// install transcript.
fn render_drop_warning(env_file: &Path, backup: &Path, local: &Path, diff: &EnvDiff) -> String {
    let mut msg = format!("warning: install is regenerating {}\n", env_file.display());
    for k in &diff.lost {
        msg.push_str(&format!("  dropped: {k}\n"));
    }
    for k in &diff.changed {
        msg.push_str(&format!("  changed: {k}\n"));
    }
    msg.push_str(&format!(
        "  previous file saved to {}\n  \
         to keep these across future installs, move them into {} —\n  \
         the installer never writes that file, and its values override this one;\n  \
         {OVERLAY_APPLY_HINT}.",
        backup.display(),
        local.display()
    ));
    msg
}

/// Pick a backup path that does not destroy an earlier one.
///
/// The first destructive install writes `kastellan.env.bak`. A *later*
/// destructive install must not overwrite it: by then the live file is the
/// already-stripped one, so overwriting would replace the only surviving copy
/// of the keys the first install dropped with a copy that no longer contains
/// them — and the transcript only ever printed key names, by design. Later
/// backups therefore get `.bak.1`, `.bak.2`, … and the warning names whichever
/// path was actually used.
///
/// Bounded rather than unbounded: a host with 100 unpruned backups is not a
/// state to paper over, and failing here is safe because the caller has not yet
/// overwritten anything.
fn next_backup_path(env_file: &Path) -> Result<PathBuf, String> {
    let first = env_file.with_extension("env.bak");
    if !first.exists() {
        return Ok(first);
    }
    for n in 1..100 {
        let candidate = env_file.with_extension(format!("env.bak.{n}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "refusing to overwrite an existing backup: {} and .bak.1 … .bak.99 all exist. \
         Prune the ones you no longer need and re-run install.",
        first.display()
    ))
}

/// Back up and report on the `kastellan.env` an install is about to overwrite.
///
/// `install` regenerates the env file from CLI flags, so every hand-added key is
/// dropped and every hand-tuned value reverts (#458). Silence there cost the
/// deployed agent its mail capability for two days. This makes the loss loud and
/// recoverable:
///
/// * nothing to lose (fresh install, or a purely additive rewrite) ⇒ no backup,
///   no output — the common case stays quiet;
/// * otherwise ⇒ copy the current file to [`next_backup_path`] and name every key
///   being dropped or changed, pointing at `kastellan.env.local` as the fix.
///
/// **Key names only, never values.** The operator reads values from the backup;
/// keeping them out of the install transcript means an env file that one day
/// holds a secret does not echo it to a terminal.
///
/// **Fails closed on an env file it cannot read.** Only `NotFound` means "first
/// install"; `InvalidData` (one non-UTF-8 byte) and `PermissionDenied` used to
/// take the same early return, and the caller's very next act is a truncating
/// `write_private` — so an unreadable file was destroyed with no backup, no
/// diff and no warning. That is #458 reproduced by its own fix, in the one case
/// where the operator has no other copy.
pub(crate) fn preserve_and_report_env(
    env_file: &Path,
    env_local_file: &Path,
    new_contents: &str,
) -> Result<(), String> {
    let old = match fs::read_to_string(env_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(format!(
                "read {}: {e}\n  \
                 refusing to continue — install is about to overwrite this file, and a file it \
                 cannot read is one it can neither diff nor back up. Move it aside (or repair its \
                 permissions/encoding) and re-run install.",
                env_file.display()
            ))
        }
    };
    let diff = crate::install::env_diff::diff_env_files(&old, new_contents);
    if diff.is_empty() {
        return Ok(());
    }

    let bak = next_backup_path(env_file)?;
    write_private(&bak, old.as_bytes())?;

    eprintln!("{}", render_drop_warning(env_file, &bak, env_local_file, &diff));
    Ok(())
}

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
    //
    // The hint names `scripts/build-release.sh`, not a bare
    // `cargo build --release`. Following the bare form and then installing
    // ships a `kastellan-worker-matrix` built WITHOUT `live-matrix`, which
    // refuses to run at spawn — the exact failure that script exists to
    // prevent (issue #682).
    for name in required_binaries() {
        let src = from_dir.join(name);
        if !src.is_file() {
            return Err(format!(
                "required binary {name:?} not found in {} — run \
                 `bash scripts/build-release.sh` first",
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

    let env = render_env_file(args, layout);
    // `layout` owns both paths: re-deriving the overlay here would let a rename
    // in `resolve_layout` point the warning at a file the service spec does not
    // read, with no test failing.
    preserve_and_report_env(&layout.env_file, &layout.env_local_file, &env)?;
    write_private(&layout.env_file, env.as_bytes())?;

    // Say what was found at the overlay path (#531). `preserve_and_report_env`
    // above reports what this install DESTROYED; this reports what it will
    // read, which is the other half and the one with no signal before now:
    // "absent by choice" and "absent by typo" used to be the same silence.
    // Naming the path is the diagnostic — an operator whose heredoc landed in
    // `~/.config/kastellan.env.local` cannot see the missing directory
    // component any other way.
    eprintln!(
        "{}",
        kastellan_supervisor::env_file::render_overlay_found(
            &layout.env_local_file,
            &kastellan_supervisor::env_file::inspect_overlay(&layout.env_local_file),
        )
    );

    // Put the operator CLI on PATH. The flat prefix (`bin_dir`) lives under
    // `~/.local/lib/` which is not on PATH, so without this symlink operators
    // can't reach `kastellan-cli` and tend to hand-copy a binary into
    // /usr/local/bin — which then goes stale and shadows the real one.
    // `current_exe()` resolves through the symlink, so sibling worker discovery
    // is unaffected. Symlink is Unix-only (the production targets).
    #[cfg(unix)]
    symlink_replace(&layout.bin_dir.join("kastellan-cli"), &layout.cli_link)?;

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

    // Ensure the chat + embedding models will fit and are available. Runs
    // before any filesystem change so a too-big model aborts cleanly. The
    // memory-fit half runs unconditionally — including under `--no-start`,
    // which still bakes the model name into `kastellan.env` — while only the
    // (potentially multi-GB) `ollama pull` is gated on actually starting.
    ensure_model_available(&args.llm_url, &args.llm_model, !args.no_start)?;
    if let Some(em) = &args.embedding_model {
        ensure_model_available(&args.llm_url, em, !args.no_start)?;
    }

    let copied = prepare_filesystem(&layout, &from, &assets_src, &args)?;
    eprintln!("installed {} binaries into {}", copied.len(), layout.bin_dir.display());
    eprintln!("linked operator CLI: {}", layout.cli_link.display());

    // Warn if the per-user CLI dir won't take precedence over system-wide bins
    // (a host may run one Kastellan per user; each user's ~/.local/bin must win).
    if let Some(path_var) = std::env::var_os("PATH") {
        if let Some(note) = cli_path_precedence_note(&path_var.to_string_lossy(), &home) {
            eprintln!("{note}");
        }
    }

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
    eprintln!("installed {UNIT_NOUN} for kastellan.target");

    if args.no_start {
        eprintln!("--no-start: units installed but not started. Start with: {START_TARGET_HINT}");
        // The units ARE enabled (`install_target` did that, #508), but this
        // early return skips the linger call below — so on a headless host
        // the per-user manager still won't be running at boot to act on it.
        // Say so here: the operator sees this message, not the comment.
        #[cfg(target_os = "linux")]
        eprintln!(
            "--no-start also skips `loginctl enable-linger`: on a headless host \
             (one nobody logs into) the units are armed but the per-user systemd \
             manager won't be up at boot to start them. For reboot persistence \
             run: loginctl enable-linger {}",
            layout.user
        );
        return Ok(());
    }

    // Linger so --user services persist on a headless box (Linux only).
    //
    // Surviving a reboot needs BOTH halves, and neither works alone:
    //   1. linger, here — starts the per-user systemd manager at boot
    //      instead of at first login (a headless box has no login);
    //   2. `systemctl --user enable`, run by `install_target` above —
    //      links `kastellan.target` under `default.target`, giving that
    //      manager something to start (#508).
    // Until (2) existed, this call looked like it delivered reboot
    // persistence and did not: the manager came up with nothing wanted.
    //
    // Note `--no-start` returns before this, so it arms the units for the
    // next boot (2) without setting linger (1). That is deliberate —
    // lingering is a host-level change an operator asking for "install but
    // don't start" has not asked for; on a desktop session it is not needed
    // at all, since login starts the manager.
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("loginctl").arg("enable-linger").arg(&layout.user).status();
    }

    // Restart (stop→start), not just start: a plain `start` is a no-op when a
    // member is already active, so a re-install/upgrade would keep running the
    // OLD binaries/env. Stopping first guarantees the new artifacts take effect.
    // On a fresh install nothing is running, so the stop is a harmless no-op.
    let _ = sup.stop_target(&specs.target);
    sup.start_target(&specs.target).map_err(|e| format!("start kastellan.target: {e}"))?;
    verify_running(sup.as_ref(), &layout)?;
    eprintln!("kastellan.target is up. Inspect: {INSPECT_LOGS_HINT}");
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

    // Remove the operator-CLI symlink (a pointer, not data — gone on any
    // uninstall, not just --purge). Only if it actually points into our prefix,
    // so we never clobber an unrelated file the operator put at that path.
    #[cfg(unix)]
    if fs::read_link(&layout.cli_link).map(|t| t.starts_with(&layout.bin_dir)).unwrap_or(false) {
        let _ = fs::remove_file(&layout.cli_link);
        eprintln!("removed operator CLI symlink: {}", layout.cli_link.display());
    }

    if purge {
        // Best-effort per dir: a partial/aborted install may be missing some of
        // these, and `remove_dir_all` errors on a missing path. Treat NotFound
        // as already-purged so cleanup is idempotent like the rest of the flow.
        for d in [&layout.bin_dir, &layout.assets_dir, &layout.config_dir, &layout.log_dir] {
            match fs::remove_dir_all(d) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("purge {}: {e}", d.display())),
            }
        }
        eprintln!(
            "purged prefix + data + logs (cluster + secrets deleted), including \
             kastellan.env.local and any kastellan.env.bak* if present — copy anything \
             you need out of them first next time"
        );
    } else {
        eprintln!("kept data dir + secrets at {} (use --purge to delete)", layout.assets_dir.display());
    }
    Ok(())
}

/// Wait for the socket to appear AND both services to reach `active`, polling
/// over a window. `kastellan-core` is `After=` Postgres but systemd does not
/// wait for PG *readiness*, so core typically crash-restarts a few times (with
/// `Restart=on-failure` backoff) before the cluster accepts connections — this
/// poll gives the target time to converge rather than failing on the first
/// not-yet-active read.
///
/// Queries through the [`Supervisor`] trait rather than shelling to
/// `systemctl` directly. The old direct call was Linux-only and mapped *"I
/// could not run the query"* onto the state `"unknown"` — two different facts
/// the caller could not tell apart. On macOS, where the backend is launchd and
/// `systemctl` does not exist, that made every install spin the full 90 s and
/// then fail with `postgres=unknown, core=unknown` plus a `journalctl` hint,
/// about services that may well have been running fine.
fn verify_running(sup: &dyn Supervisor, layout: &Layout) -> Result<(), String> {
    let socket = layout.data_dir.join("sockets/.s.PGSQL.5432");
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last = String::new();
    while Instant::now() < deadline {
        // A supervisor ERROR is not a service state — it means the query
        // itself cannot succeed (no launchd user domain, unspawnable
        // systemctl). Polling that for 90 s is not patience, it is a hidden
        // hard failure, so fail immediately and say which query broke.
        let pg = sup
            .status("kastellan-postgres")
            .map_err(|e| format!("cannot query kastellan-postgres via the service manager: {e}"))?;
        let core = sup
            .status("kastellan-core")
            .map_err(|e| format!("cannot query kastellan-core via the service manager: {e}"))?;
        if socket.exists() && pg == ServiceStatus::Active && core == ServiceStatus::Active {
            return Ok(());
        }
        last = format!("postgres={pg:?}, core={core:?}, socket={}", socket.exists());
        std::thread::sleep(Duration::from_secs(2));
    }
    // A cluster created by an older install (superuser != $USER) yields a role
    // error the daemon can't recover from; db-init won't re-init an existing
    // data dir. Surface that specifically so the operator knows to purge.
    let core_err = std::fs::read_to_string(layout.log_dir.join("kastellan-core.err")).unwrap_or_default();
    if core_err.contains("does not exist") {
        return Err(format!(
            "kastellan-core cannot authenticate to Postgres ({last}). The daemon log shows a missing \
             database role — the cluster was likely created by an older install with a different \
             superuser. Fix: `kastellan-cli uninstall --purge` then reinstall. \
             (Log: {})",
            layout.log_dir.join("kastellan-core.err").display()
        ));
    }
    Err(format!(
        "kastellan.target did not become healthy within 90s ({last}). Inspect: {}",
        INSPECT_LOGS_HINT,
    ))
}

/// Unique staging path beside `dest`, for a replace-by-rename.
///
/// Appends `.tmp-install.<pid>.<n>` to the **whole** file name. The former
/// `dest.with_extension("tmp-install")` was a pure function of the
/// destination, so two concurrent `kastellan-cli install` runs picked the
/// same staging path and the loser's `rename` failed `ENOENT` — the same
/// hazard `kastellan_supervisor::atomic_write` fixes for unit files, in
/// the same install flow. (`with_extension` also *replaces* the final
/// `.`-component, which is harmless for today's extensionless binaries
/// and wrong in general.)
///
/// Not shared with the supervisor's copy: that one publishes bytes, these
/// callers publish a `fs::copy` and a `symlink`. [#104] tracks
/// de-duplicating the `pid`-suffix pattern across the workspace.
///
/// [#104]: https://github.com/hherb/kastellan/issues/104
fn staging_path(dest: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // Both callers pass `<dir>/<name>`, so `file_name()` is always Some;
    // the fallback keeps this total rather than adding a `Result` for a
    // case neither caller can produce.
    let mut name = dest.file_name().unwrap_or(dest.as_os_str()).to_os_string();
    name.push(format!(
        ".tmp-install.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    dest.with_file_name(name)
}

/// Copy `src` over `dest` atomically, executable-bit set.
///
/// Every failure removes the staging file. That is mandatory now that the
/// staging name is unique: a deterministic name meant the next attempt
/// overwrote the previous one's leftover, whereas a unique name would
/// leave one more file per failed install — and these land in the
/// operator's `~/.local/bin`, where the litter is executable.
fn copy_exec(src: &Path, dest: &Path) -> Result<(), String> {
    let tmp = staging_path(dest);
    let published = stage_exec(src, &tmp).and_then(|()| {
        fs::rename(&tmp, dest).map_err(|e| format!("rename into {}: {e}", dest.display()))
    });
    if published.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    published
}

/// Copy `src` to the staging path and make it executable. Split out of
/// [`copy_exec`] so the caller has a single error seam to clean up behind.
fn stage_exec(src: &Path, tmp: &Path) -> Result<(), String> {
    fs::copy(src, tmp).map_err(|e| format!("copy {} -> {}: {e}", src.display(), tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tmp, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
    }
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

/// Create/refresh the `link` symlink → `target` (mkdir parent; replace an
/// existing link/file atomically so reinstalls always point at the current
/// binary). Unix-only.
#[cfg(unix)]
fn symlink_replace(target: &Path, link: &Path) -> Result<(), String> {
    use std::os::unix::fs::symlink;
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    // symlink() fails if the path exists, so stage a temp link and rename
    // over. The staging path is unique per writer, so — unlike the former
    // destination-derived name — there is nothing to unlink first: doing so
    // would delete a *concurrent* writer's staging link. Failures clean up
    // after themselves for the same reason `copy_exec` does.
    let tmp = staging_path(link);
    let published = symlink(target, &tmp)
        .map_err(|e| format!("symlink {} -> {}: {e}", tmp.display(), target.display()))
        .and_then(|()| {
            fs::rename(&tmp, link)
                .map_err(|e| format!("rename symlink into {}: {e}", link.display()))
        });
    if published.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    published
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

/// Fail-closed memory-fit guard for `model` on *this* host.
///
/// Deliberately independent of the backend. A model's footprint is a property
/// of `(model, host)`, not of the mechanism that fetches it — this check used
/// to sit *below* the `is_local_ollama` branch in [`ensure_model_available`],
/// so when macOS moved to oMLX (never a local-Ollama endpoint) the default
/// macOS install silently stopped being checked at all. The macOS default is a
/// 27B 8-bit model needing ~34 GiB by this rule; on a 16/24/32 GB Mac the
/// install used to complete cleanly and hand the operator a daemon that dies
/// at its first plan.
///
/// Approximate by design — embedding models carry no `<n>b` token and are
/// skipped rather than guessed at.
fn check_model_fits(model: &str) -> Result<(), String> {
    let (Some(est), Some(total)) = (estimate_model_bytes(model), total_system_memory_bytes())
    else {
        return Ok(());
    };
    if memory_suffices(est, total) {
        return Ok(());
    }
    // Quote the number actually compared against, not the bare weight
    // estimate: "needs ~25 GiB but the host has ~32 GiB" reads as a
    // contradiction, because the 20% headroom + 2 GiB reserve is invisible.
    Err(format!(
        "model {model} needs ~{} GiB of RAM (weights ~{} GiB + 20% headroom + 2 GiB OS reserve) \
         but this host has ~{} GiB total. Choose a smaller model with --llm-model{}, or serve it \
         from another host with --llm-url + --llm-model.",
        required_memory_bytes(est) >> 30,
        est >> 30,
        total >> 30,
        if cfg!(target_os = "macos") { " (an MLX repo id — e.g. a -4bit or smaller-B variant)" } else { "" },
    ))
}

/// Ensure `model` is available to serve at `url`.
///
/// Order matters and is the point of this function: the memory-fit guard runs
/// **first, for every backend**, and only the *fetch* is Ollama-specific. For
/// a local Ollama endpoint the installer pulls a missing model; for anything
/// else — oMLX, vLLM, llama.cpp — it cannot, and says so with the remedy
/// rather than merely reporting that nothing happened.
///
/// `may_fetch` is false under `--no-start`: that mode only lays down
/// artifacts, so it should not trigger a multi-GB pull for a target the
/// operator is not bringing up. The fit check still runs — `--no-start` writes
/// the model name into `kastellan.env` all the same, so a model that cannot
/// fit is just as wrong there, and checking costs nothing.
fn ensure_model_available(url: &str, model: &str, may_fetch: bool) -> Result<(), String> {
    check_model_fits(model)?;
    if !is_local_ollama(url) {
        // Worded for the operator's mental model, not the code's: on macOS
        // the default backend is oMLX, and a note about *Ollama* reads as an
        // aside about someone else's setup. This line is the only notice a
        // macOS operator gets that the installer cannot fetch for them.
        eprintln!(
            "note: the installer cannot fetch models for {url} (not a local Ollama endpoint). \
             Load {model:?} into that server yourself before starting — on macOS that is oMLX's \
             own admin UI."
        );
        return Ok(());
    }
    if Command::new("ollama").arg("--version").output().is_err() {
        eprintln!("note: `ollama` CLI not found — ensure model {model:?} is pulled on the Ollama host at {url}");
        return Ok(());
    }
    match ollama_has_model(model) {
        Ok(true) => {
            eprintln!("model {model} already present");
            return Ok(());
        }
        Ok(false) => {} // fall through to pull
        Err(e) => {
            eprintln!("note: could not query Ollama ({e}) — ensure model {model:?} is pulled at {url}");
            return Ok(());
        }
    }
    if !may_fetch {
        eprintln!("--no-start: skipping `ollama pull {model}` — pull it before starting the target");
        return Ok(());
    }
    eprintln!("pulling {model} via ollama (this can take a while)...");
    let status = Command::new("ollama")
        .arg("pull")
        .arg(model)
        .status()
        .map_err(|e| format!("ollama pull {model}: {e}"))?;
    if !status.success() {
        return Err(format!("ollama pull {model} failed ({status})"));
    }
    Ok(())
}

/// True when `ollama list` reports `model` (matching the bare tag or `:latest`).
fn ollama_has_model(model: &str) -> Result<bool, String> {
    let out = Command::new("ollama")
        .arg("list")
        .output()
        .map_err(|e| format!("ollama list: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ollama list failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    let want = model.strip_suffix(":latest").unwrap_or(model);
    Ok(listing.lines().skip(1).any(|line| {
        let name = line.split_whitespace().next().unwrap_or("");
        name == model || name == want || name == format!("{want}:latest")
    }))
}

/// Total physical RAM in bytes, or `None` if it can't be determined.
fn total_system_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                // "MemTotal:  263456789 kB"
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("sysctl").arg("-n").arg("hw.memsize").output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests;
