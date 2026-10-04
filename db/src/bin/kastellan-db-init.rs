//! `kastellan-db-init` — idempotent setup for the per-user Postgres cluster.
//!
//! What this binary does, in order:
//!   1. Resolve the cluster data dir (CLI flag or default
//!      `$HOME/.local/share/kastellan/pg/data`).
//!   2. Resolve the Postgres bin dir (CLI flag or auto-detect from the
//!      `default_pg_bin_dir_candidates()` list — PG 18 first).
//!   3. If the data dir already contains a `PG_VERSION` file, skip
//!      `initdb` and proceed to step 5 (idempotency).
//!   4. Make the parent dir of the data dir, then run `initdb` with the
//!      argv built by `db::build_initdb_argv`. `initdb` itself creates
//!      the data dir.
//!   5. Make the socket dir (`<data_dir>/sockets`, mode 0700) if absent.
//!   6. Atomically write `<data_dir>/postgresql.auto.conf` (write to
//!      `.tmp`, fsync, rename) with the contents from
//!      `db::build_postgresql_auto_conf` — `listen_addresses=''`,
//!      `unix_socket_directories=<sockets>`, etc.
//!   7. Print a one-line success log so the operator (or supervisor)
//!      can confirm.
//!
//! The CLI is deliberately small. Anything more elaborate (extension
//! enablement, role provisioning, migrations) belongs in a separate
//! tool and a later session — this one just brings the cluster into
//! the "initialized + locked-down config" state.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use kastellan_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_data_dir,
    default_pg_bin_dir_candidates, default_socket_dir, find_pg_bin_dir,
    is_data_dir_initialized, require_absolute, DbError, InitDbOptions,
    PgConfigOptions,
};

fn main() -> ExitCode {
    match run(env::args_os().collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kastellan-db-init: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Default)]
struct Args {
    data_dir: Option<PathBuf>,
    bin_dir: Option<PathBuf>,
    username: Option<String>,
    print_help: bool,
}

const HELP: &str = "\
Usage: kastellan-db-init [OPTIONS]

Idempotent setup for the per-user Postgres cluster used by kastellan.

OPTIONS:
  --data-dir <PATH>   Cluster data dir. Default: $HOME/.local/share/kastellan/pg/data
  --bin-dir  <PATH>   Postgres bin dir (containing postgres + initdb).
                      Default: auto-detect (PG 18 first, then 17 .. 14).
  --username <NAME>   Postgres role to create as superuser.  Default: kastellan
  -h, --help          Print this help and exit.

Re-running on an already-initialized data dir is a no-op (skips initdb)
but still re-writes postgresql.auto.conf so config drift is corrected.
";

fn parse_args(argv: Vec<OsString>) -> Result<Args, String> {
    let mut iter = argv.into_iter().skip(1);
    let mut a = Args::default();
    while let Some(raw) = iter.next() {
        let s = raw.to_string_lossy().into_owned();
        match s.as_str() {
            "-h" | "--help" => a.print_help = true,
            "--data-dir" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--data-dir requires a path argument".to_string())?;
                a.data_dir = Some(PathBuf::from(v));
            }
            "--bin-dir" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--bin-dir requires a path argument".to_string())?;
                a.bin_dir = Some(PathBuf::from(v));
            }
            "--username" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--username requires a name argument".to_string())?;
                a.username = Some(v.to_string_lossy().into_owned());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(a)
}

fn run(argv: Vec<OsString>) -> Result<(), DbError> {
    let args = parse_args(argv).map_err(DbError::Io)?;
    if args.print_help {
        print!("{HELP}");
        return Ok(());
    }

    let data_dir = args
        .data_dir
        .or_else(default_data_dir)
        .ok_or_else(|| DbError::Io("$HOME unset and no --data-dir given".into()))?;
    require_absolute(&data_dir)?;

    let bin_dir = match args.bin_dir {
        Some(b) => {
            require_absolute(&b)?;
            b
        }
        None => find_pg_bin_dir(&default_pg_bin_dir_candidates())?,
    };
    require_absolute(&bin_dir)?;

    let initdb_bin = bin_dir.join("initdb");
    let socket_dir = default_socket_dir(&data_dir);

    // Step 1: parent dir of the data dir must exist before initdb runs.
    if let Some(parent) = data_dir.parent() {
        fs::create_dir_all(parent)?;
    }

    // Step 2: skip initdb if the cluster is already populated. We still
    // proceed to re-write postgresql.auto.conf so a hand-edited cluster
    // is corrected back to our policy on every invocation.
    if !is_data_dir_initialized(&data_dir) {
        let opts = InitDbOptions {
            data_dir: data_dir.clone(),
            username: args.username.unwrap_or_else(|| "kastellan".into()),
            ..InitDbOptions::default()
        };
        let argv = build_initdb_argv(&initdb_bin, &opts);
        eprintln!("kastellan-db-init: running {}", argv.join(" "));
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            // initdb needs PATH (calls bootstrap binaries) and LANG/LC_ALL
            // for encoding-aware bootstrap. PATH is the smallest sensible
            // floor; locale is set explicitly via --encoding above so we
            // don't propagate the host's LANG.
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .status()
            .map_err(|e| DbError::Io(format!("spawn initdb: {e}")))?;
        if !status.success() {
            return Err(DbError::InitDbFailed(format!(
                "exit status {status}; argv: {argv:?}"
            )));
        }
    } else {
        eprintln!(
            "kastellan-db-init: data dir already initialized, skipping initdb (PG_VERSION present at {})",
            data_dir.display()
        );
    }

    // Step 3: socket dir lives inside the data dir at mode 0700.
    if !socket_dir.exists() {
        fs::create_dir(&socket_dir)?;
    }
    {
        let mut perms = fs::metadata(&socket_dir)?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&socket_dir, perms)?;
    }

    // Step 4: write postgresql.auto.conf atomically (write-to-tmp +
    // fsync + rename — same pattern supervisor::systemd_user uses for
    // unit files).
    let cfg = PgConfigOptions {
        socket_dir: socket_dir.clone(),
        ..PgConfigOptions::default()
    };
    let conf_body = build_postgresql_auto_conf(&cfg);
    write_atomic(&data_dir.join("postgresql.auto.conf"), conf_body.as_bytes())?;

    eprintln!(
        "kastellan-db-init: cluster ready at {} (socket {}); start via supervisor",
        data_dir.display(),
        socket_dir.display()
    );
    Ok(())
}

fn write_atomic(target: &Path, body: &[u8]) -> Result<(), DbError> {
    let tmp = target.with_extension("auto.conf.tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, target)?;
    Ok(())
}
