//! `LiveSdk`: the real `matrix-rust-sdk`-backed implementation of the
//! [`MatrixSdk`](crate::sdk::MatrixSdk) seam, compiled only under the
//! `live-matrix` feature. The hermetic handler tests use a fake; this is the
//! code that talks to an actual homeserver.
//!
//! ## Shape
//!
//! `matrix-sdk` is async; the [`MatrixSdk`] seam is synchronous (the core-side
//! driver thread issues one blocking `matrix.poll` / `matrix.send` at a time).
//! `LiveSdk` therefore owns a multi-thread tokio [`Runtime`] and `block_on`s the
//! SDK calls behind the sync methods. A background **sync task** runs on that
//! same runtime, decrypts inbound room-text events in an event handler, and
//! pushes them onto a bounded [`VecDeque`] that [`poll`](LiveSdk::poll) drains.
//!
//! ## Egress transport
//!
//! When `KASTELLAN_EGRESS_PROXY_UDS` is set (the force-routed deployment), a
//! [`ProxyBridge`](crate::bridge::ProxyBridge) binds a loopback TCP port and
//! relays to the sidecar UDS; the SDK is pointed at it via the builder's
//! `.proxy()`. This is the transport the Phase D spike confirmed
//! (`egress_spike.rs`). The egress sidecar runs in `disable_mitm` mode for the
//! matrix worker (transparent tunnel), so the SDK keeps native end-to-end TLS
//! validation against the self-hosted homeserver — no custom CA is injected.
//!
//! ## Persistent encrypted state
//!
//! The SQLite store (device keys, sync token, room state) lives in the worker's
//! persistent store dir. The login session itself is written to
//! `<store>/session.json` after the first password login and restored on
//! restart, so the device id — and therefore the E2E identity — is stable across
//! restarts (a fresh login each start would rotate device keys and break E2E).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tokio::runtime::Runtime;

use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::RoomId;
use matrix_sdk::store::StateStoreDataKey;
use matrix_sdk::Client;

use kastellan_matrix_wire::{Event, InitResult};

use crate::bridge::ProxyBridge;
use crate::sdk::{MatrixSdk, SendError, SendEvidence};
use crate::sync_retry;

mod login;
use login::{ensure_cross_signing, restore_or_login};
mod room_gate;
use room_gate::{register_autojoin_handler, register_message_handler};

/// Bounded depth of the inbound buffer the sync task fills and `poll` drains. A
/// single-user channel never reaches this; it is a backstop against a flooding
/// peer (mirrors the core-side `INBOUND_BUFFER`).
const INBOUND_CAP: usize = 256;

/// Default device display name when the operator doesn't set one.
const DEVICE_NAME_DEFAULT: &str = "kastellan";

/// Where the login session is persisted inside the store dir.
const SESSION_FILE: &str = "session.json";

/// How long `poll` sleeps between buffer checks while long-polling.
const POLL_TICK: Duration = Duration::from_millis(50);

/// A `sync()` that ran at least this long before returning was healthy and
/// serving; its return is a transient blip, so the consecutive-failure counter
/// resets (see [`crate::sync_retry`]).
const SYNC_HEALTHY_RUN: Duration = Duration::from_secs(60);
/// Base + cap for the continuous-sync retry backoff (mirrors the core-side
/// `MatrixChannel` respawn backoff).
const SYNC_BACKOFF_BASE: Duration = Duration::from_secs(1);
const SYNC_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Consecutive *fast* `sync()` failures the loop tolerates before giving up and
/// exiting for a fresh supervised respawn — a persistently-wedged client (bad
/// token, corrupt store) only a fresh `connect` recovers, so retrying in place
/// forever would never heal it.
const SYNC_MAX_CONSECUTIVE: u32 = 10;

/// Operator configuration for the live worker, read from its environment. The
/// core-side spawn fills these in; the live e2e sets them directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSdkConfig {
    /// Full homeserver URL, e.g. `https://matrix.example.org`.
    pub homeserver_url: String,
    /// Login user (localpart or full `@user:server`).
    pub user: String,
    /// Login password. Optional: only needed for the *initial* password login
    /// (no persisted session yet). Once `<store>/session.json` exists it is
    /// restored instead, so the spawn need not materialize the secret on every
    /// restart.
    pub password: Option<String>,
    /// Persistent store dir (SQLite state/crypto store + `session.json`).
    pub store_dir: PathBuf,
    /// Initial device display name (cosmetic).
    pub device_name: String,
    /// Egress sidecar UDS; when set, traffic is routed through a `ProxyBridge`.
    pub proxy_uds: Option<PathBuf>,
    /// `KASTELLAN_MATRIX_PEERS` (comma-separated user ids), the same set the
    /// core authorises against. Enforced here at the room edge as well
    /// (security audit 2026-09-02, channel F1): invites from anyone else are
    /// declined and their events are never buffered. Empty ⇒ no per-sender
    /// filter (the core remains the gate), but the two-party room rule below
    /// still applies.
    pub allowed_peers: Vec<String>,
}

impl LiveSdkConfig {
    /// Read config from the process environment, failing closed if a required
    /// variable is unset.
    ///
    /// The password is resolved from `KASTELLAN_MATRIX_PASSWORD_FILE` first (its
    /// contents, trailing newline trimmed) and only then from
    /// `KASTELLAN_MATRIX_PASSWORD`. The host spawns the worker with the *file*
    /// path — never the secret value in the environment — so the plaintext never
    /// lands in the bwrap argv (`/proc/<pid>/cmdline`, `ps`). The file is consumed
    /// (deleted) once read, so it doesn't linger at rest beyond the initial login.
    pub fn from_env() -> anyhow::Result<Self> {
        let file_password = std::env::var("KASTELLAN_MATRIX_PASSWORD_FILE").ok().and_then(|path| {
            let value = std::fs::read_to_string(&path).ok();
            // Best-effort consume: minimize the at-rest window for the secret.
            let _ = std::fs::remove_file(&path);
            value.map(|v| v.trim_end_matches(['\n', '\r']).to_string())
        });
        parse_config(|k| {
            if k == "KASTELLAN_MATRIX_PASSWORD" {
                if let Some(p) = &file_password {
                    return Some(p.clone());
                }
            }
            std::env::var(k).ok()
        })
    }
}

/// Pure config parse over an injectable getter so the required/optional contract
/// is unit-tested without mutating the process environment.
fn parse_config(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<LiveSdkConfig> {
    let req = |key: &str| get(key).with_context(|| format!("{key} must be set"));
    Ok(LiveSdkConfig {
        homeserver_url: req("KASTELLAN_MATRIX_HOMESERVER_URL")?,
        user: req("KASTELLAN_MATRIX_USER")?,
        password: get("KASTELLAN_MATRIX_PASSWORD"),
        store_dir: PathBuf::from(req("KASTELLAN_MATRIX_STORE")?),
        device_name: get("KASTELLAN_MATRIX_DEVICE_NAME")
            .unwrap_or_else(|| DEVICE_NAME_DEFAULT.to_string()),
        proxy_uds: get("KASTELLAN_EGRESS_PROXY_UDS").map(PathBuf::from),
        allowed_peers: get("KASTELLAN_MATRIX_PEERS")
            .unwrap_or_default()
            .split(',')
            .map(|p| p.trim().to_ascii_lowercase())
            .filter(|p| !p.is_empty())
            .collect(),
    })
}

/// The live SDK handle. Owns the tokio runtime, the connected client, the
/// background sync task, and (when force-routed) the egress proxy bridge.
pub struct LiveSdk {
    runtime: Runtime,
    // `Option` so [`Drop`] can `take()` it and drop it *inside* the runtime —
    // see the `Drop` impl for why that ordering matters.
    client: Option<Client>,
    identity: InitResult,
    buffer: Arc<Mutex<VecDeque<Event>>>,
    // Kept alive for the worker's lifetime; both abort/close on drop.
    _bridge: Option<ProxyBridge>,
    _sync_task: tokio::task::JoinHandle<()>,
}

impl Drop for LiveSdk {
    fn drop(&mut self) {
        // matrix-sdk's SQLite state/crypto/event-cache stores use `deadpool`,
        // whose pooled-connection `Drop` calls tokio `spawn_blocking` to close
        // the connection — which panics ("aborting") unless a tokio runtime
        // context is active on the dropping thread. The worker drops `LiveSdk`
        // on its (non-runtime) main thread after `serve_stdio` returns, so we
        // must drop the client *inside* `block_on` to give that teardown a
        // runtime context; otherwise the worker SIGABRTs on every shutdown.
        if let Some(client) = self.client.take() {
            self.runtime.block_on(async move { drop(client) });
        }
    }
}

impl LiveSdk {
    /// Connect: build the client (optionally through the egress bridge), restore
    /// or establish a login session, register the inbound event handler, do one
    /// initial sync, and spawn the continuous sync task. Blocks until the client
    /// is logged in and first-synced — this is the network-needing init that the
    /// worker `main` runs **before** `lock_down`.
    pub fn connect(config: LiveSdkConfig) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            // Landlock is per-thread and this runtime is built BEFORE the
            // worker's `lock_down` (login + first sync need the network
            // first), so without this hook every runtime thread — the ones
            // that parse homeserver data — would carry no Landlock at all
            // (security audit 2026-09-02, F4/F5). Restrict each worker thread
            // as it starts, from the same env the main thread will use; a
            // failure aborts, since serving one layer short is the thing this
            // exists to prevent.
            .on_thread_start(|| {
                #[cfg(target_os = "linux")]
                if let Err(e) = kastellan_worker_prelude::landlock_lock::apply_from_env() {
                    eprintln!("kastellan-worker-matrix: per-thread landlock failed: {e}");
                    std::process::abort();
                }
            })
            .build()
            .context("build tokio runtime")?;
        let buffer: Arc<Mutex<VecDeque<Event>>> = Arc::new(Mutex::new(VecDeque::new()));

        let (client, identity, bridge) =
            runtime.block_on(connect_client(&config, buffer.clone()))?;

        // Continuous background sync: keeps the buffer filled for `poll` and the
        // crypto state fresh for `send`. `client.sync` returns whenever the SDK
        // hits an interruption — a transient server 5xx, a network blip through the
        // egress tunnel, a long-poll hiccup — none of which is necessarily fatal.
        //
        // Treating *every* return as fatal (the original `process::exit(1)`) meant a
        // single transient blip killed the whole worker and forced a supervised
        // respawn — the ~20–90s churn of #348. Instead we **retry in place** with
        // capped exponential backoff and only fail loud (exit → fresh respawn) after
        // sustained consecutive fast failures, since a persistently-wedged client
        // (bad token, corrupt store) only a fresh `connect` recovers (see
        // [`crate::sync_retry`]). The task is aborted before its body re-runs when
        // the client is dropped on shutdown, so the `process::exit` give-up path is
        // reached only on a real failure run; it skips destructors deliberately —
        // the deadpool SQLite teardown that needs a runtime context (see the `Drop`
        // impl) never runs off-runtime; the OS reclaims everything.
        let sync_client = client.clone();
        let sync_task = runtime.spawn(async move {
            let mut consecutive = 0u32;
            loop {
                let started = Instant::now();
                let result = sync_client.sync(SyncSettings::default()).await;
                let ran_for = started.elapsed();
                match &result {
                    Ok(()) => eprintln!(
                        "kastellan-worker-matrix: sync loop returned (no error) after {ran_for:?}; retrying"
                    ),
                    Err(e) => eprintln!(
                        "kastellan-worker-matrix: sync loop failed after {ran_for:?}: {e}; retrying"
                    ),
                }
                consecutive =
                    sync_retry::update_consecutive(consecutive, ran_for, SYNC_HEALTHY_RUN);
                match sync_retry::next_action(
                    consecutive,
                    SYNC_MAX_CONSECUTIVE,
                    SYNC_BACKOFF_BASE,
                    SYNC_BACKOFF_MAX,
                ) {
                    sync_retry::SyncOutcome::Backoff(delay) => {
                        tokio::time::sleep(delay).await;
                    }
                    sync_retry::SyncOutcome::GiveUp => {
                        eprintln!(
                            "kastellan-worker-matrix: sync loop failed {consecutive} times \
                             consecutively; exiting for a fresh supervised respawn"
                        );
                        std::process::exit(1);
                    }
                }
            }
        });

        Ok(Self {
            runtime,
            client: Some(client),
            identity,
            buffer,
            _bridge: bridge,
            _sync_task: sync_task,
        })
    }
}

impl MatrixSdk for LiveSdk {
    fn identity(&self) -> InitResult {
        self.identity.clone()
    }

    fn poll(&mut self, timeout_ms: u64) -> Vec<Event> {
        let first = drain(&self.buffer);
        if !first.is_empty() || timeout_ms == 0 {
            return first;
        }
        // Long-poll: wait up to `timeout_ms` for the first event, then return
        // whatever arrived (possibly empty). Runs on the owned runtime.
        let buffer = self.buffer.clone();
        self.runtime.block_on(async move {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                let events = drain(&buffer);
                if !events.is_empty() {
                    return events;
                }
                if Instant::now() >= deadline {
                    return Vec::new();
                }
                tokio::time::sleep(POLL_TICK).await;
            }
        })
    }

    fn send(&mut self, conversation: &str, body: &str) -> Result<(), SendError> {
        let body = body.to_string();
        let conversation = conversation.to_string();
        // Clone the client (cheap — it's `Arc`-backed) so the future doesn't
        // borrow `self` across the runtime's `block_on`.
        let client = self.client.as_ref().expect("live client present").clone();
        self.runtime.block_on(async move {
            let room_id = RoomId::parse(&conversation).map_err(|e| {
                SendError::new(SendEvidence::Local, format!("invalid room id {conversation:?}: {e}"))
            })?;
            let room = client.get_room(&room_id).ok_or_else(|| {
                SendError::new(SendEvidence::Local, format!("unknown room {conversation}"))
            })?;
            room.send(RoomMessageEventContent::text_plain(body)).await.map_err(|e| {
                SendError::new(send_evidence(&e), format!("send room message: {e}"))
            })?;
            Ok(())
        })
    }
}

/// What a failed `room.send` got back (#782): no HTTP response at all, or the
/// homeserver's status. Everything else — a crypto or local-store failure, a
/// response that was not a Matrix error — is [`SendEvidence::Local`], which
/// charges the room; those are rare, and a room is still given up only after
/// an hour of them with the channel otherwise answering.
fn send_evidence(e: &matrix_sdk::Error) -> SendEvidence {
    match e {
        matrix_sdk::Error::Timeout => SendEvidence::NoResponse,
        matrix_sdk::Error::Http(h) if matches!(**h, matrix_sdk::HttpError::Reqwest(_)) => {
            SendEvidence::NoResponse
        }
        _ => e
            .as_client_api_error()
            .map_or(SendEvidence::Local, |api| SendEvidence::Status(api.status_code.as_u16())),
    }
}

/// Decide whether inbound delivery should be live from the very start of the
/// initial sync, given the sync token (if any) the SDK persisted on a previous
/// run.
///
/// matrix-sdk stores its sync token in the SQLite state store and resumes from
/// it on restart, so when a prior token exists the catch-up sync returns only
/// events received *since* that token — genuinely-unprocessed messages,
/// including any a user sent while the worker was down. Those must be surfaced,
/// so we start live (`true`).
///
/// With no prior token this is a fresh login, whose catch-up sync replays recent
/// room history; that must stay suppressed, so we start not-live (`false`) and
/// flip live only after the initial sync drains (see `connect_client`).
///
/// An empty string is not a real sync position and is treated as no token,
/// ensuring a fresh login can never replay full room history (defense-in-depth).
fn initial_live_state(prior_sync_token: Option<&str>) -> bool {
    prior_sync_token.is_some_and(|token| !token.is_empty())
}

/// Read the sync token matrix-sdk persisted on a previous run, if any.
///
/// `Client::sync_token()` is `pub(crate)` in matrix-sdk 0.18, so we read the
/// same value the SDK stores via the public state-store key. **Fail-soft:** a
/// store-read error (or an absent value) yields `None`, which routes
/// [`initial_live_state`] to "fresh / suppress". A read failure can therefore
/// never cause a stale-history replay — at worst it re-drops a downtime window,
/// which is exactly the pre-#321 behavior.
async fn read_prior_sync_token(client: &Client) -> Option<String> {
    client
        .state_store()
        .get_kv_data(StateStoreDataKey::SyncToken)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.into_sync_token())
}

/// Drain all currently-buffered inbound events, leaving the buffer empty. Pure
/// helper so the drain contract is testable without the SDK.
fn drain(buffer: &Mutex<VecDeque<Event>>) -> Vec<Event> {
    buffer.lock().expect("inbound buffer not poisoned").drain(..).collect()
}

/// Build + log in the client and register the inbound handler. Returns the
/// client, the resolved identity, and the bridge (kept alive by the caller).
async fn connect_client(
    config: &LiveSdkConfig,
    buffer: Arc<Mutex<VecDeque<Event>>>,
) -> anyhow::Result<(Client, InitResult, Option<ProxyBridge>)> {
    std::fs::create_dir_all(&config.store_dir)
        .with_context(|| format!("create store dir {:?}", config.store_dir))?;

    // Bind the egress bridge first (if force-routed) so the builder can point
    // `.proxy()` at it. The SDK then issues `CONNECT homeserver:443` through the
    // bridge → sidecar UDS (the transport the spike confirmed).
    let mut builder = Client::builder()
        .homeserver_url(&config.homeserver_url)
        .sqlite_store(&config.store_dir, None);
    let bridge = match &config.proxy_uds {
        Some(uds) => {
            let b = ProxyBridge::bind(uds.clone())
                .await
                .with_context(|| format!("bind egress bridge to {uds:?}"))?;
            builder = builder.proxy(format!("http://{}", b.proxy_addr()));
            Some(b)
        }
        None => None,
    };
    let client = builder.build().await.context("build matrix client")?;

    restore_or_login(&client, config).await?;

    // Bootstrap the account's cross-signing identity so the bot self-signs its
    // own device — clears Element's "device not verified by its owner" shield.
    // Idempotent + best-effort: a failure must not stop the worker serving.
    if let Err(e) = ensure_cross_signing(&client, config).await {
        eprintln!("kastellan-worker-matrix: cross-signing bootstrap failed (non-fatal): {e:#}");
    }

    // Identity is known locally post-login/restore — no extra network round-trip.
    let user_id = client.user_id().context("no user id after login")?.to_owned();
    let identity = InitResult {
        user_id: user_id.to_string(),
        device_id: client
            .device_id()
            .context("no device id after login")?
            .to_string(),
    };

    // Gate inbound delivery on `live`. On a *fresh login* the catch-up sync
    // replays recent room history, which must be suppressed (false until the
    // initial sync drains). On a *restart*, the SDK resumes from its persisted
    // sync token, so the catch-up sync returns only events received since the
    // last run — including any a user sent while the worker was down. We seed
    // `live` from whether that prior token exists, so the restart backlog is
    // surfaced instead of dropped (#321). The token is read before the handler is
    // registered so the decision covers the entire initial sync.
    let prior_sync_token = read_prior_sync_token(&client).await;
    let live = Arc::new(AtomicBool::new(initial_live_state(prior_sync_token.as_deref())));
    let allowed_peers: Arc<Vec<String>> = Arc::new(config.allowed_peers.clone());
    register_message_handler(&client, buffer, user_id, live.clone(), allowed_peers.clone());
    register_autojoin_handler(&client, allowed_peers);

    // One initial sync so room state + member device keys are present before we
    // start serving `send`; the continuous sync task takes over afterwards.
    client
        .sync_once(SyncSettings::default())
        .await
        .context("initial sync")?;

    // Backlog drained; surface inbound events from here on.
    live.store(true, Ordering::SeqCst);

    Ok((client, identity, bridge))
}

#[cfg(test)]
mod tests;
