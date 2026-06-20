//! `matrix probe` — bring up the live Matrix worker against the configured
//! homeserver and prove the round-trip without the full daemon/`ChannelBus`.
//!
//! It materializes the bot password from the Vault, spawns the sandboxed
//! `kastellan-worker-matrix` (live-matrix build), blocks on `matrix.init`
//! (login + first sync), and prints the bot identity. With `--send` it pushes a
//! message to a room; with `--listen <secs>` it prints inbound events. This is
//! the comms-slice-#2 **login/round-trip smoke** — the daemon wiring proper is
//! `core::channel::matrix::from_env` + `ChannelBus`.
//!
//! Defaults target `matrix.kastellan.dev` / `@kastellan`. `--enforce-sandbox`
//! turns on the worker's seccomp + Landlock (off by default for the first
//! bring-up, so a login failure isn't masked by a sandbox-policy failure).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use kastellan_core::channel::matrix::{spawn_matrix_worker, MatrixSpawnConfig, SpawnedMatrixWorker};
use kastellan_core::channel::{Channel, ChannelId, ConversationId, OutgoingMessage, PeerId};

use crate::common::{resolve_connect_spec, with_runtime};

const DEFAULT_HOMESERVER: &str = "https://matrix.kastellan.dev";
const DEFAULT_USER: &str = "@kastellan:matrix.kastellan.dev";
const DEFAULT_SECRET: &str = "matrix_kastellan_password";

struct Args {
    homeserver: String,
    user: String,
    secret: String,
    store: Option<PathBuf>,
    worker_bin: Option<PathBuf>,
    send_room: Option<String>,
    body: String,
    listen_secs: Option<u64>,
    enforce_sandbox: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        homeserver: DEFAULT_HOMESERVER.to_string(),
        user: DEFAULT_USER.to_string(),
        secret: DEFAULT_SECRET.to_string(),
        store: None,
        worker_bin: None,
        send_room: None,
        body: "kastellan online.".to_string(),
        listen_secs: None,
        enforce_sandbox: false,
    };
    let mut i = 0;
    while i < args.len() {
        let need = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i).cloned().ok_or_else(|| format!("{} requires a value", args[*i - 1]))
        };
        match args[i].as_str() {
            "--homeserver" => a.homeserver = need(&mut i)?,
            "--user" => a.user = need(&mut i)?,
            "--secret" => a.secret = need(&mut i)?,
            "--store" => a.store = Some(PathBuf::from(need(&mut i)?)),
            "--worker-bin" => a.worker_bin = Some(PathBuf::from(need(&mut i)?)),
            "--send" => a.send_room = Some(need(&mut i)?),
            "--body" => a.body = need(&mut i)?,
            "--listen" => {
                let v = need(&mut i)?;
                a.listen_secs = Some(v.parse().map_err(|_| format!("--listen: not a number: {v}"))?);
            }
            "--enforce-sandbox" => a.enforce_sandbox = true,
            other => return Err(format!("unknown flag: {other}")),
        }
        i += 1;
    }
    Ok(a)
}

/// Locate the matrix worker binary as a sibling of the running CLI executable
/// (the install layout flattens every binary into one prefix).
fn default_worker_bin() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe.parent().ok_or("current_exe has no parent dir")?;
    Ok(dir.join("kastellan-worker-matrix"))
}

fn default_store() -> Result<PathBuf, String> {
    let base = kastellan_core::audit_mirror::default_state_dir()
        .ok_or("$HOME unset; cannot resolve state dir")?;
    Ok(base.join("matrix").join("store"))
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match args.first().map(|s| s.as_str()) {
        Some("probe") => with_runtime("matrix probe", probe(&args[1..])),
        _ => {
            eprintln!("usage: kastellan-cli matrix probe [--homeserver URL] [--user USER]\n  \
                       [--secret NAME] [--store DIR] [--worker-bin PATH]\n  \
                       [--send ROOM] [--body TEXT] [--listen SECS] [--enforce-sandbox]");
            ExitCode::from(2)
        }
    }
}

async fn probe(args: &[String]) -> ExitCode {
    let a = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("matrix probe: {e}");
            return ExitCode::from(2);
        }
    };

    let worker_bin = match a.worker_bin.clone().map(Ok).unwrap_or_else(default_worker_bin) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("matrix probe: {e}");
            return ExitCode::from(1);
        }
    };
    if !worker_bin.exists() {
        eprintln!("matrix probe: worker binary not found: {}\n  \
                   build it with `cargo build --release -p kastellan-worker-matrix --features live-matrix`",
            worker_bin.display());
        return ExitCode::from(1);
    }
    let store = match a.store.clone().map(Ok).unwrap_or_else(default_store) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("matrix probe: {e}");
            return ExitCode::from(1);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match kastellan_db::pool::connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("matrix probe: connect db: {e}");
            return ExitCode::from(1);
        }
    };
    let kp = match kastellan_db::secrets::OsKeyringProvider::ensure_initialized() {
        Ok(kp) => kp,
        Err(e) => {
            eprintln!("matrix probe: keyring: {e}");
            return ExitCode::from(1);
        }
    };
    let vault = kastellan_core::secrets::Vault::new();
    let sandboxes = kastellan_sandbox::SandboxBackends::default_for_current_os();
    #[cfg(target_os = "linux")]
    let backend = &*sandboxes.bwrap;
    #[cfg(target_os = "macos")]
    let backend = &*sandboxes.seatbelt;

    let cfg = MatrixSpawnConfig {
        worker_bin: worker_bin.clone(),
        homeserver_url: a.homeserver.clone(),
        user: a.user.clone(),
        store_dir: store.clone(),
        password_secret_name: a.secret.clone(),
        device_name: Some("kastellan-probe".to_string()),
        enforce_sandbox: a.enforce_sandbox,
    };

    eprintln!(
        "matrix probe: spawning worker {} → {} as {} (store {}, sandbox {})",
        worker_bin.display(),
        a.homeserver,
        a.user,
        store.display(),
        if a.enforce_sandbox { "enforced" } else { "disabled" },
    );

    let SpawnedMatrixWorker { mut channel, identity } = match spawn_matrix_worker(
        backend,
        &pool,
        &vault,
        &kp,
        ChannelId("matrix".to_string()),
        &cfg,
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("matrix probe: spawn/login failed: {e:#}");
            return ExitCode::from(1);
        }
    };

    println!("LOGIN OK — identity: {identity}");

    if let Some(room) = &a.send_room {
        let msg = OutgoingMessage {
            channel: ChannelId("matrix".to_string()),
            peer: PeerId(String::new()),
            conversation: ConversationId(room.clone()),
            body: a.body.clone(),
        };
        if let Err(e) = channel.send(msg).await {
            eprintln!("matrix probe: send failed: {e:#}");
            return ExitCode::from(1);
        }
        println!("SENT to {room}: {:?}", a.body);
    }

    if let Some(secs) = a.listen_secs {
        println!("listening {secs}s for inbound events (Ctrl-C to stop early)…");
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, channel.recv()).await {
                Ok(Some(msg)) => println!(
                    "RECV from {} in {}: {:?}",
                    msg.peer.0, msg.conversation.0, msg.body
                ),
                Ok(None) => {
                    eprintln!("matrix probe: inbound channel closed (worker died?)");
                    return ExitCode::from(1);
                }
                Err(_) => break, // deadline
            }
        }
        println!("listen window done.");
    }

    // Dropping `channel` closes the worker's stdin → EOF → clean worker exit.
    ExitCode::from(0)
}
