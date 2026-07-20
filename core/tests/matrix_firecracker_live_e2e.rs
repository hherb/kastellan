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
use kastellan_sandbox::{SandboxBackend, SandboxBackends};
use kastellan_tests_common::microvm::{firecracker_backend, image_dir, skip_if_no_microvm};
use serde_json::{json, Value};

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "matrix.ext4";

/// Opt-in gate: the operator sets this when the homeserver + accounts are staged.
const GATE: &str = "KASTELLAN_MATRIX_FC_LIVE_E2E";
/// The bot worker binary baked into the rootfs (its in-guest path).
const GUEST_MATRIX_BIN: &str = "/usr/local/bin/kastellan-worker-matrix";

/// Serializes the two VM tests. `cargo test` runs tests in parallel by default, but
/// these two CANNOT run concurrently: [`kill_vm`]'s `pkill -f kastellan-microvm-run`
/// is GLOBAL (it would kill the other test's VM), and both share the one
/// `matrix-state.ext4`. Each test holds this guard for its whole duration so a plain
/// `cargo test … -- --ignored` still runs them one at a time. Poison-tolerant: a
/// panicking test must not wedge the other.
static VM_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn vm_test_guard() -> std::sync::MutexGuard<'static, ()> {
    VM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ── Firecracker harness helpers (mirrored from net_demo_firecracker_egress_e2e.rs) ──

/// The persistent E2E store ext4 image — the same stable path
/// `build_matrix_vm_policy` derives (`<image_dir>/matrix-state.ext4`). It survives a
/// VM respawn (mkfs-once), which is what carries the #321 downtime recovery.
fn matrix_state_image() -> PathBuf {
    PathBuf::from(image_dir()).join("matrix-state.ext4")
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

/// Mint a unique shallow scratch subdir per spawn (fresh sidecar UDS each respawn;
/// the `<scratch>/egress.sock` path must fit `sun_path`). `root` is the test's
/// auto-cleaned `TempDir` (a short `/tmp/.tmpXXXXXX`, itself a slice-3 SHARE_ANCHOR
/// descendant); every subdir — and its UDS — is removed when the root drops, so
/// repeated DGX runs don't accumulate `/tmp` cruft.
fn scratch_subdir(root: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = root.join(format!("vm-{seq}"));
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
    scratch_root: PathBuf,
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

        let scratch = scratch_subdir(&scratch_root);
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
    if skip_if_no_microvm(VM_ROOTFS) {
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
/// is a silent no-op). Returns `true` iff `pkill` actually matched (exit 0) — i.e.
/// a running launcher was found and killed. A `false` return means the kill was
/// VACUOUS (no VM to kill), which the restart test asserts against: without a real
/// kill the "recovery" would be a live delivery to a still-alive original VM, not
/// the fresh-VM `/data`-store respawn under test (the false-green the sibling
/// `net_demo_firecracker_egress_e2e` guards via its `calls_served` counter).
fn kill_vm() -> bool {
    eprintln!("[INFO] sending SIGKILL (-f) to kastellan-microvm-run to force VM death");
    std::process::Command::new("pkill")
        .args(["-9", "-f", "kastellan-microvm-run"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── tests ───────────────────────────────────────────────────────────────────────

#[test]
#[ignore = "live: DGX KVM + HTTPS homeserver + two accounts in a shared encrypted room"]
fn matrix_vm_send_recv_round_trip() {
    let Some((homeserver, bot, peer, room, proxy, peer_bin)) = gate() else {
        return;
    };
    // Serialize against the sibling VM test (global pkill + shared persistent store).
    let _vm_lock = vm_test_guard();
    // Fresh persistent store each run (the FC backend mkfs's it on first spawn).
    let _ = std::fs::remove_file(matrix_state_image());

    let peer_store = tempfile::tempdir().expect("peer store dir");
    let mut peer_client = spawn_peer(&peer_bin, &homeserver, &peer, peer_store.path());
    let _peer_id: Value = peer_client.call("matrix.init", json!({})).expect("peer init");

    // Auto-cleaned scratch root (holds every spawn's sidecar UDS); declared before
    // `bot` so it drops AFTER the worker teardown that uses those sockets.
    let scratch_root = tempfile::tempdir().expect("scratch root dir");
    let bot = PersistentWorker::spawn(
        "matrix-vm",
        vm_bot_factory(
            homeserver.clone(),
            bot.user.clone(),
            bot.password.clone(),
            proxy,
            scratch_root.path().to_path_buf(),
        ),
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
            Err(e) => {
                // Back off rather than busy-spin the deadline away on a persistent err.
                eprintln!("[INFO] poll err (transient?): {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
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
    // Serialize against the sibling VM test (global pkill + shared persistent store).
    let _vm_lock = vm_test_guard();
    // Fresh persistent store: the FIRST spawn mkfs's + populates it; the respawn
    // reuses the same image (mkfs-once), which is the mechanism under test.
    let _ = std::fs::remove_file(matrix_state_image());

    let peer_store = tempfile::tempdir().expect("peer store dir");
    let mut peer_client = spawn_peer(&peer_bin, &homeserver, &peer, peer_store.path());
    let _peer_id: Value = peer_client.call("matrix.init", json!({})).expect("peer init");

    // Auto-cleaned scratch root (holds every (re)spawn's sidecar UDS); declared
    // before `bot` so it drops AFTER the worker teardown that uses those sockets.
    let scratch_root = tempfile::tempdir().expect("scratch root dir");
    let bot = PersistentWorker::spawn(
        "matrix-vm",
        vm_bot_factory(
            homeserver.clone(),
            bot.user.clone(),
            bot.password.clone(),
            proxy,
            scratch_root.path().to_path_buf(),
        ),
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
    // matrix-state.ext4 on the next call. Assert the kill was NON-VACUOUS: if pkill
    // matched nothing the original VM is still alive and any "recovered" message
    // would be a live delivery, not the fresh-VM /data-store recovery under test.
    assert!(
        kill_vm(),
        "kill_vm matched no kastellan-microvm-run process — the VM was never killed, \
         so this test would false-green on a live delivery instead of exercising the \
         fresh-VM #321 recovery"
    );

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
    // Fresh-VM proof: the driver replies Err to the in-flight caller when the
    // transport is dead (persistent.rs) and does NOT transparently retry, so at
    // least one poll MUST Err while the killed VM + sidecar respawn. A still-alive
    // original (vacuous kill) would keep answering Ok and never surface an Err — so
    // requiring `saw_respawn_err` rules out the "live delivery" false-green.
    let mut saw_respawn_err = false;
    while Instant::now() < deadline {
        match bot.call("matrix.poll", json!({ "timeout_ms": 2000 })) {
            Ok(res) => {
                let events = res["events"].as_array().cloned().unwrap_or_default();
                if events.iter().any(|e| e["body"] == json!(body)) {
                    received = true;
                    break;
                }
            }
            Err(e) => {
                saw_respawn_err = true;
                // Back off rather than busy-spin through the multi-second respawn.
                eprintln!("[INFO] poll err during respawn (expected): {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    bot.shutdown();
    assert!(
        saw_respawn_err,
        "no poll error was observed after the kill — the transport never broke, so a \
         fresh-VM respawn did not actually happen and {body:?} (if seen) was a live \
         delivery to a still-running original VM, not #321 /data-store recovery"
    );
    assert!(
        received,
        "#321 regression (VM): bot did not surface {body:?} sent during downtime — \
         fresh VM may not have restored the persisted /data store"
    );
}
