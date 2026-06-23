//! Live Matrix round-trip e2e (`#[ignore]`) — Phase D acceptance.
//!
//! Proves the real `matrix-rust-sdk`-backed worker ([`sdk_live::LiveSdk`]) does a
//! genuine login + E2E send + E2E receive against a running homeserver. It is
//! `#[ignore]` (real network + manual homeserver/account setup) so CI stays
//! green, and **skip-as-passes** unless the operator explicitly opts in.
//!
//! ## What it exercises
//!
//! Two worker processes are spawned, one per account (`bot` and `peer`), each
//! reading its own config from its own environment. The `peer` sends a uniquely
//! tagged message to a shared encrypted room; the `bot` must surface that message
//! via `matrix.poll`. That single round-trip proves, end to end: login (both),
//! the background sync loop, outbound E2E send, and inbound E2E decrypt — all
//! through the worker's real SDK path. Reusing the worker binary as the test's
//! second Matrix client keeps the core test crate free of any `matrix-sdk`
//! dependency.
//!
//! ## How to run (DGX / dev box)
//!
//! Build the live worker (`cargo build -p kastellan-worker-matrix --features
//! live-matrix`), stand up conduwuit via `scripts/matrix/setup-conduwuit.sh`
//! (federation-off, loopback), create two accounts with the printed registration
//! token, create one **encrypted** room, and join both accounts to it. Then:
//!
//! ```sh
//! KASTELLAN_MATRIX_LIVE_E2E=1 \
//! KASTELLAN_MATRIX_HOMESERVER_URL=http://127.0.0.1:6167 \
//! KASTELLAN_MATRIX_USER=@bot:localhost       KASTELLAN_MATRIX_PASSWORD=… \
//! KASTELLAN_MATRIX_PEER_USER=@peer:localhost KASTELLAN_MATRIX_PEER_PASSWORD=… \
//! KASTELLAN_MATRIX_ROOM='!roomid:localhost' \
//! cargo test -p kastellan-core --test matrix_live_e2e -- --ignored --nocapture
//! ```
//!
//! The worker's `lock_down` is set to `none`/`none` here (via
//! `KASTELLAN_SECCOMP_PROFILE` and `KASTELLAN_LANDLOCK_PROFILE`): this test
//! targets *SDK* correctness; the real sandbox/seccomp/Landlock and the
//! egress-sidecar coupling are exercised by the channel-worker production spawn
//! (plan Task 5) and the sandbox suites.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use kastellan_protocol::client::Client;
use serde_json::{json, Value};

/// Opt-in gate: the operator sets this when conduwuit + accounts are staged.
const GATE: &str = "KASTELLAN_MATRIX_LIVE_E2E";

/// Locate the live worker binary (`<target>/debug/kastellan-worker-matrix`).
fn worker_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // core/
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("kastellan-worker-matrix")
}

/// One bot account's config for a spawned worker.
struct Account {
    user: String,
    password: String,
}

/// Read every required env var; return `None` (caller skip-as-passes) if any is
/// missing so a broad `--ignored` run doesn't fail on an unconfigured box.
fn required_env() -> Option<(String, Account, Account, String)> {
    let get = |k: &str| std::env::var(k).ok();
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

/// Spawn one worker process for `acct` against `homeserver`, with a private
/// persistent store under `store_dir`, and connect a JSON-RPC client. The worker
/// does login + first sync before it answers, so the first `call` blocks until
/// the SDK is ready.
fn spawn_worker(homeserver: &str, acct: &Account, store_dir: &std::path::Path) -> Client {
    let child = std::process::Command::new(worker_bin())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .env("KASTELLAN_MATRIX_HOMESERVER_URL", homeserver)
        .env("KASTELLAN_MATRIX_USER", &acct.user)
        .env("KASTELLAN_MATRIX_PASSWORD", &acct.password)
        .env("KASTELLAN_MATRIX_STORE", store_dir)
        // SDK-correctness focus: keep lockdown a no-op (the sandbox is covered
        // by the sandbox suites + the channel-worker production spawn).
        .env("KASTELLAN_SECCOMP_PROFILE", "none")
        .env("KASTELLAN_LANDLOCK_PROFILE", "none")
        .spawn()
        .expect("spawn matrix worker");
    Client::from_child(child).expect("connect to matrix worker")
}

#[test]
#[ignore = "live: needs a running conduwuit + two bot accounts in a shared encrypted room"]
fn matrix_send_recv_round_trip() {
    if std::env::var(GATE).is_err() {
        eprintln!("\n[SKIP] {GATE} unset — live Matrix e2e needs a homeserver; see module docs\n");
        return;
    }
    let bin = worker_bin();
    if !bin.exists() {
        eprintln!(
            "\n[SKIP] live worker not built: {} — run `cargo build -p kastellan-worker-matrix --features live-matrix`\n",
            bin.display()
        );
        return;
    }
    let Some((homeserver, bot, peer, room)) = required_env() else {
        eprintln!(
            "\n[SKIP] live Matrix e2e env incomplete — need KASTELLAN_MATRIX_HOMESERVER_URL, \
             _USER/_PASSWORD, _PEER_USER/_PEER_PASSWORD, _ROOM\n"
        );
        return;
    };

    let bot_store = tempfile::tempdir().expect("bot store dir");
    let peer_store = tempfile::tempdir().expect("peer store dir");
    let mut bot_client = spawn_worker(&homeserver, &bot, bot_store.path());
    let mut peer_client = spawn_worker(&homeserver, &peer, peer_store.path());

    // Both log in + first-sync (blocks until ready) and report a sane identity.
    let bot_id: Value = bot_client.call("matrix.init", json!({})).expect("bot init");
    assert!(
        bot_id["user_id"].as_str().is_some_and(|u| u.starts_with('@')),
        "bot identity should be a user id, got {bot_id:?}"
    );
    let _peer_id: Value = peer_client.call("matrix.init", json!({})).expect("peer init");

    // Peer sends a uniquely-tagged message; the bot must receive + decrypt it.
    let body = format!("kastellan-live-e2e-{}", std::process::id());
    peer_client
        .call("matrix.send", json!({ "conversation": room, "body": body }))
        .expect("peer send");

    // Poll the bot until the tagged message surfaces (bounded; E2E + sync latency).
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut received = false;
    while Instant::now() < deadline {
        let res: Value = bot_client
            .call("matrix.poll", json!({ "timeout_ms": 2000 }))
            .expect("bot poll");
        let events = res["events"].as_array().cloned().unwrap_or_default();
        if events.iter().any(|e| e["body"] == json!(body)) {
            received = true;
            break;
        }
    }
    assert!(received, "bot never received the peer's message {body:?} within the deadline");
}

/// Regression proof for **issue #321**: a message sent to a room while the bot
/// worker was down must be surfaced after the bot restarts.
///
/// The fix in `workers/matrix/src/sdk_live.rs` reads the SDK's persisted sync
/// token before the initial sync (`read_prior_sync_token`) and seeds a `live`
/// flag via `initial_live_state(Option<&str>) -> bool`. When a token is present
/// (restart path), `live` starts `true` so the incremental catch-up sync —
/// which replays only events since the last run — is NOT suppressed. Without
/// the fix the bot would ignore the downtime backlog (it would start `live =
/// false` and treat the catch-up as a history replay to silence).
#[test]
#[ignore = "live: needs a running conduwuit + two bot accounts in a shared encrypted room"]
fn matrix_restart_recovers_downtime_message() {
    // ── Gate 1: operator opt-in ─────────────────────────────────────────────
    if std::env::var(GATE).is_err() {
        eprintln!(
            "\n[SKIP] {GATE} unset — live Matrix restart e2e needs a homeserver; see module docs\n"
        );
        return;
    }

    // ── Gate 2: worker binary present ──────────────────────────────────────
    let bin = worker_bin();
    if !bin.exists() {
        eprintln!(
            "\n[SKIP] live worker not built: {} — run `cargo build -p kastellan-worker-matrix --features live-matrix`\n",
            bin.display()
        );
        return;
    }

    // ── Gate 3: all required env vars present ──────────────────────────────
    let Some((homeserver, bot, peer, room)) = required_env() else {
        eprintln!(
            "\n[SKIP] live Matrix restart e2e env incomplete — need KASTELLAN_MATRIX_HOMESERVER_URL, \
             _USER/_PASSWORD, _PEER_USER/_PEER_PASSWORD, _ROOM\n"
        );
        return;
    };

    // ── Create persistent store dirs ────────────────────────────────────────
    //
    // bot_store MUST outlive BOTH spawns of the bot worker; binding it here
    // (before either spawn) ensures the tempdir is not dropped between them.
    // The peer gets its own independent store.
    let bot_store = tempfile::tempdir().expect("bot store dir");
    let peer_store = tempfile::tempdir().expect("peer store dir");

    // ── First spawn: bot does initial login + first sync ────────────────────
    //
    // `matrix.init` blocks until the SDK is ready and has persisted a sync
    // token + session.json into bot_store. After this call the token exists on
    // disk, which is the precondition for the #321 fix to take effect on restart.
    let mut bot_client = spawn_worker(&homeserver, &bot, bot_store.path());
    let mut peer_client = spawn_worker(&homeserver, &peer, peer_store.path());

    let bot_id: Value = bot_client.call("matrix.init", json!({})).expect("bot first init");
    assert!(
        bot_id["user_id"].as_str().is_some_and(|u| u.starts_with('@')),
        "bot identity should be a user id, got {bot_id:?}"
    );
    let _peer_id: Value = peer_client.call("matrix.init", json!({})).expect("peer init");

    // ── Gracefully stop the bot so its token is persisted ───────────────────
    //
    // `close(self)` drops stdin → worker sees EOF → exits cleanly → LiveSdk
    // Drop persists final sync state. After this the bot is DOWN.
    let bot_exit = bot_client.close().expect("bot worker close");
    assert!(
        bot_exit.success(),
        "bot worker exited uncleanly on first shutdown (status: {bot_exit})"
    );

    // ── Peer sends a message WHILE the bot is down ──────────────────────────
    //
    // Use a distinct tag from the round-trip test so a shared room cannot
    // accidentally cross-match messages between the two tests.
    let body = format!("kastellan-live-e2e-restart-{}", std::process::id());
    peer_client
        .call("matrix.send", json!({ "conversation": room, "body": body }))
        .expect("peer send during bot downtime");

    // ── Respawn the bot against the SAME store dir ──────────────────────────
    //
    // The worker finds session.json + the persisted sync token in bot_store;
    // `initial_live_state(Some(&token))` returns `true`, so the incremental
    // catch-up sync (events since last run = the downtime window) is surfaced
    // rather than silenced. This is the exact behaviour #321 fixes.
    let mut bot_client2 = spawn_worker(&homeserver, &bot, bot_store.path());
    let _bot_id2: Value = bot_client2.call("matrix.init", json!({})).expect("bot second init");

    // ── Poll until the downtime message surfaces (or deadline) ──────────────
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut received = false;
    while Instant::now() < deadline {
        let res: Value = bot_client2
            .call("matrix.poll", json!({ "timeout_ms": 2000 }))
            .expect("bot poll after restart");
        let events = res["events"].as_array().cloned().unwrap_or_default();
        if events.iter().any(|e| e["body"] == json!(body)) {
            received = true;
            break;
        }
    }

    // If this assertion fires the #321 regression is back: the bot discarded
    // the downtime backlog instead of resuming from the persisted token.
    assert!(
        received,
        "#321 regression: bot did not surface {body:?} sent during downtime — \
         catch-up sync may be starting without the persisted token"
    );
}
