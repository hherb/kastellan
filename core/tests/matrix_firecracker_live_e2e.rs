//! DGX real-KVM live e2e for slice 5b-4b: the Matrix worker runs in a Firecracker
//! VM, force-routed through a host egress sidecar, with its E2E crypto/session store
//! on a persistent ext4 image at `/data`. Proves (1) VM-mode login + a real
//! send/recv round-trip against the live homeserver, and (2) the #321 downtime
//! recovery composed with a genuine fresh-VM respawn: kill the VMM
//! (`pkill -f kastellan-microvm-run`), `PersistentWorker` respawns a fresh VM +
//! sidecar, and the message sent while the bot was down is recovered from the sync
//! token persisted on the `/data` ext4 image (which survives the respawn).
//!
//! ## Design
//!
//! The bot runs the real `kastellan-worker-matrix --features live-matrix` INSIDE a
//! Firecracker VM: its `SandboxPolicy` comes from the production
//! [`build_matrix_vm_policy`] (empty fs_read/fs_write, `persistent_store` at
//! `/data`, `KASTELLAN_MICROVM_ROOTFS=matrix.ext4`), and it reaches the homeserver
//! ONLY through a per-worker transparent-tunnel egress sidecar spawned by
//! [`spawn_net_transport`] (the same call the daemon's `spawn_matrix_worker` factory
//! makes) — the VM has no NIC. The bot is driven over the raw `matrix.init/poll/send`
//! JSON-RPC methods (exactly like `matrix_live_e2e.rs`), which keeps this test focused
//! on the NEW 5b-4b surface (VM boot + persistent store + sidecar egress); the
//! cross-platform `MatrixChannel`/`PolledWorkerDriver` wrapper is separately covered
//! by the hermetic `matrix_channel_e2e.rs`. The peer is a plain direct worker.
//!
//! ## How to run (DGX only)
//!
//! ```sh
//!   export KASTELLAN_MATRIX_FC_LIVE_E2E=1
//!   export PATH=$HOME/.local/bin:$PATH        # firecracker on PATH
//!   # build the release launcher + matrix rootfs (stale-launcher gotcha):
//!   cargo build --release -p kastellan-microvm-run -p kastellan-microvm-init \
//!               -p kastellan-worker-matrix --features live-matrix
//!   ./scripts/workers/microvm/build-matrix-rootfs.sh
//!   # the peer (direct) worker + the host egress-proxy sidecar (debug ok):
//!   cargo build -p kastellan-worker-matrix --features live-matrix
//!   cargo build -p kastellan-worker-egress-proxy
//!   # live accounts on an HTTPS homeserver (bot + peer in a shared ENCRYPTED room):
//!   #   KASTELLAN_MATRIX_HOMESERVER_URL=https://matrix.kastellan.dev
//!   #   KASTELLAN_MATRIX_USER=@kastellan:kastellan.dev  KASTELLAN_MATRIX_PASSWORD=…
//!   #   KASTELLAN_MATRIX_PEER_USER=@…:kastellan.dev     KASTELLAN_MATRIX_PEER_PASSWORD=…
//!   #   KASTELLAN_MATRIX_ROOM='!…:kastellan.dev'
//!   cargo test -p kastellan-core --test matrix_firecracker_live_e2e -- --ignored --nocapture
//! ```
//!
//! On macOS the whole file is excluded (`#![cfg(target_os = "linux")]`) — it compiles
//! to an empty test binary, matching `python_exec_firecracker_e2e.rs`. It also
//! skip-as-passes on any Linux host lacking the gate env / KVM / rootfs / binaries.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kastellan_core::channel::matrix::build_matrix_vm_policy;
use kastellan_core::egress::persistent_net::{spawn_net_transport, NetTransportSpawn};
use kastellan_core::worker_lifecycle::{PersistentFactory, PersistentTransport, PersistentWorker};
use kastellan_protocol::client::Client;
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};
use serde_json::{json, Value};

/// Opt-in gate: the operator sets this when the homeserver + accounts are staged.
const GATE: &str = "KASTELLAN_MATRIX_FC_LIVE_E2E";
/// The bot worker binary baked into the rootfs (its in-guest path).
const GUEST_MATRIX_BIN: &str = "/usr/local/bin/kastellan-worker-matrix";

// ── Firecracker harness helpers (mirrored from net_demo_firecracker_egress_e2e.rs) ──

/// The micro-VM image dir (kernel + rootfs). Matches the backend default and is
/// overridable for a user-local build, exactly like the runtime resolver.
fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage {
        kernel_path: dir.join("vmlinux"),
        rootfs_path: dir.join("matrix.ext4"),
    }
}

/// The persistent E2E store ext4 image — the same stable path
/// `build_matrix_vm_policy` derives (`<image_dir>/matrix-state.ext4`). It survives a
/// VM respawn (mkfs-once), which is what carries the #321 downtime recovery.
fn matrix_state_image() -> PathBuf {
    PathBuf::from(image_dir()).join("matrix-state.ext4")
}

/// Locate the `kastellan-microvm-run` launcher (release preferred, then debug) and
/// prepend its dir to `$PATH` so the backend's `Command::new("kastellan-microvm-run")`
/// resolves it. Returns the path if found.
fn locate_microvm_run() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core has a workspace parent")
        .join("target");
    for profile in ["release", "debug"] {
        let p = target.join(profile).join("kastellan-microvm-run");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Skip (early-return `true`) when this host can't run the micro-VM. On success,
/// prepend the launcher's dir to `$PATH` exactly once.
fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&firecracker_image()) {
        eprintln!("\n[SKIP] firecracker probe failed: {e}\n");
        return true;
    }
    match locate_microvm_run() {
        Some(bin) => {
            use std::sync::Once;
            static PATH_ONCE: Once = Once::new();
            PATH_ONCE.call_once(|| {
                let dir = bin.parent().unwrap().to_path_buf();
                let cur = std::env::var_os("PATH").unwrap_or_default();
                let mut paths = vec![dir];
                paths.extend(std::env::split_paths(&cur));
                let joined = std::env::join_paths(paths).expect("join PATH");
                std::env::set_var("PATH", joined);
            });
            false
        }
        None => {
            eprintln!(
                "\n[SKIP] kastellan-microvm-run not built; run \
                 `cargo build --release -p kastellan-microvm-run`\n"
            );
            true
        }
    }
}

fn firecracker_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(Some(SandboxBackendKind::FirecrackerVm), None)
}

/// The HOST backend (bwrap on Linux) for the egress-proxy sidecar — the sidecar is
/// the real-network boundary and must run on the host, never in the VM.
fn host_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(None, None)
}

/// Locate the host egress-proxy sidecar binary (debug or release). `None` → skip.
fn proxy_bin() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
    [
        target.join("debug").join("kastellan-worker-egress-proxy"),
        target.join("release").join("kastellan-worker-egress-proxy"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Locate the peer's `kastellan-worker-matrix` (debug preferred — the peer runs
/// directly on the host, not in a VM).
fn peer_worker_bin() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
    [
        target.join("debug").join("kastellan-worker-matrix"),
        target.join("release").join("kastellan-worker-matrix"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// One account's login config.
struct Account {
    user: String,
    password: String,
}

/// Read every required env var; `None` ⇒ caller skip-as-passes.
fn required_env() -> Option<(String, Account, Account, String)> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let homeserver = get("KASTELLAN_MATRIX_HOMESERVER_URL")?;
    let bot = Account {
        user: get("KASTELLAN_MATRIX_USER")?,
        password: get("KASTELLAN_MATRIX_PASSWORD")?,
    };
    let peer = Account {
        user: get("KASTELLAN_MATRIX_PEER_USER")?,
        password: get("KASTELLAN_MATRIX_PEER_PASSWORD")?,
    };
    let room = get("KASTELLAN_MATRIX_ROOM")?;
    Some((homeserver, bot, peer, room))
}

/// Parse `host:port` from a homeserver URL (default 443 for https, 80 for http).
fn parse_host_port(url: &str) -> (String, u16) {
    let no_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let hostport = no_scheme.split('/').next().unwrap_or(no_scheme);
    if let Some((h, p)) = hostport.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return (h.to_string(), port);
        }
    }
    let default = if url.starts_with("http://") { 80 } else { 443 };
    (hostport.to_string(), default)
}

/// Spawn the PEER as a plain direct worker process (no VM, no sidecar) and connect
/// a JSON-RPC client — identical to `matrix_live_e2e.rs::spawn_worker`. The peer is
/// just the test's second Matrix client; login + first-sync block until ready.
fn spawn_peer(bin: &Path, homeserver: &str, acct: &Account, store_dir: &Path) -> Client {
    let child = std::process::Command::new(bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .env("KASTELLAN_MATRIX_HOMESERVER_URL", homeserver)
        .env("KASTELLAN_MATRIX_USER", &acct.user)
        .env("KASTELLAN_MATRIX_PASSWORD", &acct.password)
        .env("KASTELLAN_MATRIX_STORE", store_dir)
        .env("KASTELLAN_SECCOMP_PROFILE", "none")
        .env("KASTELLAN_LANDLOCK_PROFILE", "none")
        .spawn()
        .expect("spawn peer matrix worker");
    Client::from_child(child).expect("connect to peer matrix worker")
}

/// Mint a unique shallow scratch dir under `/tmp` per spawn (fresh sidecar UDS each
/// respawn; the `<scratch>/egress.sock` path must fit `sun_path`). `/tmp` is a
/// slice-3 SHARE_ANCHOR.
fn scratch_root_subdir() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from("/tmp").join(format!("matrix-vm-{}-{}", std::process::id(), seq));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Build the VM-bot `PersistentFactory`: `build_matrix_vm_policy` + the login env +
/// `spawn_net_transport` (VM worker backend + host-bwrap sidecar). Re-run verbatim on
/// every (re)spawn, so a fresh VM boots against the SAME `matrix-state.ext4`.
fn vm_bot_factory(
    homeserver: String,
    user: String,
    password: String,
    proxy_bin: PathBuf,
) -> PersistentFactory {
    let backend = firecracker_backend();
    let host_backend = host_backend();
    Box::new(move || {
        let (host, port) = parse_host_port(&homeserver);
        let mut policy =
            build_matrix_vm_policy(&host, port, image_dir(), matrix_state_image());
        // Login env (the daemon's spawn_matrix_worker pushes these after the policy
        // builder). Password via env is fine for a test; the production RO-share
        // path is exercised by the unit tests. STORE=/data is the persistent mount.
        policy
            .env
            .push(("KASTELLAN_MATRIX_HOMESERVER_URL".into(), homeserver.clone()));
        policy.env.push(("KASTELLAN_MATRIX_USER".into(), user.clone()));
        policy.env.push(("KASTELLAN_MATRIX_PASSWORD".into(), password.clone()));
        policy.env.push(("KASTELLAN_MATRIX_STORE".into(), "/data".into()));
        policy
            .env
            .push(("KASTELLAN_MATRIX_DEVICE_NAME".into(), "kastellan-fc-e2e".into()));
        // SDK-correctness focus: the VM already isolates; skip the in-worker
        // seccomp/Landlock here (matches matrix_live_e2e's stance).
        policy.env.push(("KASTELLAN_SECCOMP_PROFILE".into(), "none".into()));
        policy.env.push(("KASTELLAN_LANDLOCK_PROFILE".into(), "none".into()));

        let scratch = scratch_root_subdir();
        let allow = vec![format!("{host}:{port}")];
        let params = NetTransportSpawn {
            backend: &*backend,
            sidecar_backend: &*host_backend,
            proxy_bin: &proxy_bin,
            program: GUEST_MATRIX_BIN,
            args: &[],
            base_policy: policy,
            allowlist: &allow,
            worker_name: "matrix",
            extra_ca: None,
        };
        let t = spawn_net_transport(&params, &scratch, |_row| {})?;
        Ok(Box::new(t) as Box<dyn PersistentTransport>)
    })
}

/// Shared gate: returns the live config or `None` (skip-as-pass), after checking the
/// opt-in env, the micro-VM readiness, and the peer + proxy binaries.
fn gate() -> Option<(String, Account, Account, String, PathBuf, PathBuf)> {
    if std::env::var(GATE).is_err() {
        eprintln!("\n[SKIP] {GATE} unset — VM-mode live Matrix e2e needs a homeserver; see module docs\n");
        return None;
    }
    if skip_if_no_microvm() {
        return None;
    }
    let Some(proxy) = proxy_bin() else {
        eprintln!("[SKIP] egress-proxy not built; run `cargo build -p kastellan-worker-egress-proxy`");
        return None;
    };
    let Some(peer_bin) = peer_worker_bin() else {
        eprintln!(
            "[SKIP] peer worker not built; run `cargo build -p kastellan-worker-matrix --features live-matrix`"
        );
        return None;
    };
    let Some((homeserver, bot, peer, room)) = required_env() else {
        eprintln!(
            "\n[SKIP] VM live Matrix e2e env incomplete — need KASTELLAN_MATRIX_HOMESERVER_URL, \
             _USER/_PASSWORD, _PEER_USER/_PEER_PASSWORD, _ROOM\n"
        );
        return None;
    };
    Some((homeserver, bot, peer, room, proxy, peer_bin))
}

/// SIGKILL the launcher → force VM death. `-f` matches the full command line (the
/// 21-char "kastellan-microvm-run" overflows the 15-char `comm`, so a bare `pkill`
/// is a silent no-op).
fn kill_vm() {
    eprintln!("[INFO] sending SIGKILL (-f) to kastellan-microvm-run to force VM death");
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-f", "kastellan-microvm-run"])
        .status();
}

// ── tests ───────────────────────────────────────────────────────────────────────

#[test]
#[ignore = "live: DGX KVM + HTTPS homeserver + two accounts in a shared encrypted room"]
fn matrix_vm_send_recv_round_trip() {
    let Some((homeserver, bot, peer, room, proxy, peer_bin)) = gate() else {
        return;
    };
    // Fresh persistent store each run (the FC backend mkfs's it on first spawn).
    let _ = std::fs::remove_file(matrix_state_image());

    let peer_store = tempfile::tempdir().expect("peer store dir");
    let mut peer_client = spawn_peer(&peer_bin, &homeserver, &peer, peer_store.path());
    let _peer_id: Value = peer_client.call("matrix.init", json!({})).expect("peer init");

    let bot = PersistentWorker::spawn(
        "matrix-vm",
        vm_bot_factory(homeserver.clone(), bot.user.clone(), bot.password.clone(), proxy),
    )
    .expect("boot matrix VM");

    // The VM boots + logs in + first-syncs before matrix.init returns; give it room.
    let bot_id: Value = bot.call("matrix.init", json!({})).expect("bot init (VM)");
    assert!(
        bot_id["user_id"].as_str().is_some_and(|u| u.starts_with('@')),
        "bot identity should be a user id, got {bot_id:?}"
    );
    eprintln!("[INFO] VM bot logged in as {}", bot_id["user_id"]);

    let body = format!("kastellan-fc-live-{}", std::process::id());
    peer_client
        .call("matrix.send", json!({ "conversation": room, "body": body }))
        .expect("peer send");

    // Poll until the tagged message surfaces (VM + E2E + sync latency ⇒ generous).
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut received = false;
    while Instant::now() < deadline {
        match bot.call("matrix.poll", json!({ "timeout_ms": 2000 })) {
            Ok(res) => {
                let events = res["events"].as_array().cloned().unwrap_or_default();
                if events.iter().any(|e| e["body"] == json!(body)) {
                    received = true;
                    break;
                }
            }
            Err(e) => eprintln!("[INFO] poll err (transient?): {e}"),
        }
    }
    bot.shutdown();
    assert!(received, "VM bot never received the peer's message {body:?} within the deadline");
}

/// #321 + `persistent_store` composed: a message sent while the bot VM was down is
/// recovered after a genuine fresh-VM respawn (the sync token persisted on the
/// `/data` ext4 image survives the SIGKILL).
#[test]
#[ignore = "live: DGX KVM + HTTPS homeserver + two accounts in a shared encrypted room"]
fn matrix_vm_restart_recovers_downtime_message() {
    let Some((homeserver, bot, peer, room, proxy, peer_bin)) = gate() else {
        return;
    };
    // Fresh persistent store: the FIRST spawn mkfs's + populates it; the respawn
    // reuses the same image (mkfs-once), which is the mechanism under test.
    let _ = std::fs::remove_file(matrix_state_image());

    let peer_store = tempfile::tempdir().expect("peer store dir");
    let mut peer_client = spawn_peer(&peer_bin, &homeserver, &peer, peer_store.path());
    let _peer_id: Value = peer_client.call("matrix.init", json!({})).expect("peer init");

    let bot = PersistentWorker::spawn(
        "matrix-vm",
        vm_bot_factory(homeserver.clone(), bot.user.clone(), bot.password.clone(), proxy),
    )
    .expect("boot matrix VM");

    // First boot: login + first sync persist session.json + the sync token onto the
    // /data ext4 image — the precondition for the #321 fix on restart.
    let bot_id: Value = bot.call("matrix.init", json!({})).expect("bot first init (VM)");
    assert!(
        bot_id["user_id"].as_str().is_some_and(|u| u.starts_with('@')),
        "bot identity should be a user id, got {bot_id:?}"
    );

    // Kill the VM. PersistentWorker will respawn a FRESH VM against the same
    // matrix-state.ext4 on the next call.
    kill_vm();

    // Peer sends WHILE the bot is down (distinct tag from the round-trip test).
    let body = format!("kastellan-fc-live-restart-{}", std::process::id());
    peer_client
        .call("matrix.send", json!({ "conversation": room, "body": body }))
        .expect("peer send during bot downtime");

    // Poll through the respawn window: the first calls Err while the VM + sidecar
    // respawn; then the fresh VM re-inits from the persisted /data store (the #321
    // fix surfaces the downtime backlog) and the tagged message appears.
    let deadline = Instant::now() + Duration::from_secs(150);
    let mut received = false;
    while Instant::now() < deadline {
        match bot.call("matrix.poll", json!({ "timeout_ms": 2000 })) {
            Ok(res) => {
                let events = res["events"].as_array().cloned().unwrap_or_default();
                if events.iter().any(|e| e["body"] == json!(body)) {
                    received = true;
                    break;
                }
            }
            Err(e) => eprintln!("[INFO] poll err during respawn (expected): {e}"),
        }
    }
    bot.shutdown();
    assert!(
        received,
        "#321 regression (VM): bot did not surface {body:?} sent during downtime — \
         fresh VM may not have restored the persisted /data store"
    );
}
