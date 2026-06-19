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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tokio::runtime::Runtime;

use matrix_sdk::config::SyncSettings;
use matrix_sdk::matrix_auth::MatrixSession;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
};
use matrix_sdk::ruma::RoomId;
use matrix_sdk::{Client, Room};

use kastellan_matrix_wire::{push_bounded, Event, InitResult};

use crate::bridge::ProxyBridge;
use crate::sdk::MatrixSdk;

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

/// Operator configuration for the live worker, read from its environment. The
/// core-side spawn fills these in; the live e2e sets them directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSdkConfig {
    /// Full homeserver URL, e.g. `https://matrix.example.org`.
    pub homeserver_url: String,
    /// Login user (localpart or full `@user:server`).
    pub user: String,
    /// Login password.
    pub password: String,
    /// Persistent store dir (SQLite state/crypto store + `session.json`).
    pub store_dir: PathBuf,
    /// Initial device display name (cosmetic).
    pub device_name: String,
    /// Egress sidecar UDS; when set, traffic is routed through a `ProxyBridge`.
    pub proxy_uds: Option<PathBuf>,
}

impl LiveSdkConfig {
    /// Read config from the process environment, failing closed if a required
    /// variable is unset.
    pub fn from_env() -> anyhow::Result<Self> {
        parse_config(|k| std::env::var(k).ok())
    }
}

/// Pure config parse over an injectable getter so the required/optional contract
/// is unit-tested without mutating the process environment.
fn parse_config(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<LiveSdkConfig> {
    let req = |key: &str| get(key).with_context(|| format!("{key} must be set"));
    Ok(LiveSdkConfig {
        homeserver_url: req("KASTELLAN_MATRIX_HOMESERVER_URL")?,
        user: req("KASTELLAN_MATRIX_USER")?,
        password: req("KASTELLAN_MATRIX_PASSWORD")?,
        store_dir: PathBuf::from(req("KASTELLAN_MATRIX_STORE")?),
        device_name: get("KASTELLAN_MATRIX_DEVICE_NAME")
            .unwrap_or_else(|| DEVICE_NAME_DEFAULT.to_string()),
        proxy_uds: get("KASTELLAN_EGRESS_PROXY_UDS").map(PathBuf::from),
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
            .build()
            .context("build tokio runtime")?;
        let buffer: Arc<Mutex<VecDeque<Event>>> = Arc::new(Mutex::new(VecDeque::new()));

        let (client, identity, bridge) =
            runtime.block_on(connect_client(&config, buffer.clone()))?;

        // Continuous background sync: keeps the buffer filled for `poll` and the
        // crypto state fresh for `send`. Errors end the task; the driver thread
        // then sees `poll` fail and the worker is restarted by the supervisor.
        let sync_client = client.clone();
        let sync_task = runtime.spawn(async move {
            if let Err(e) = sync_client.sync(SyncSettings::default()).await {
                eprintln!("kastellan-worker-matrix: sync loop ended: {e}");
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

    fn send(&mut self, conversation: &str, body: &str) -> anyhow::Result<()> {
        let body = body.to_string();
        let conversation = conversation.to_string();
        // Clone the client (cheap — it's `Arc`-backed) so the future doesn't
        // borrow `self` across the runtime's `block_on`.
        let client = self.client.as_ref().expect("live client present").clone();
        self.runtime.block_on(async move {
            let room_id = RoomId::parse(&conversation)
                .with_context(|| format!("invalid room id {conversation:?}"))?;
            let room = client
                .get_room(&room_id)
                .with_context(|| format!("unknown room {conversation}"))?;
            room.send(RoomMessageEventContent::text_plain(body))
                .await
                .context("send room message")?;
            Ok(())
        })
    }
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

    // Identity is known locally post-login/restore — no extra network round-trip.
    let user_id = client.user_id().context("no user id after login")?.to_owned();
    let identity = InitResult {
        user_id: user_id.to_string(),
        device_id: client
            .device_id()
            .context("no device id after login")?
            .to_string(),
    };

    register_message_handler(&client, buffer, user_id);

    // One initial sync so room state + member device keys are present before we
    // start serving `send`; the continuous sync task takes over afterwards.
    client
        .sync_once(SyncSettings::default())
        .await
        .context("initial sync")?;

    Ok((client, identity, bridge))
}

/// Restore a persisted session if one exists, else password-login and persist
/// the resulting session so the device identity is stable across restarts.
async fn restore_or_login(client: &Client, config: &LiveSdkConfig) -> anyhow::Result<()> {
    let session_path = config.store_dir.join(SESSION_FILE);
    if let Ok(bytes) = std::fs::read(&session_path) {
        let session: MatrixSession =
            serde_json::from_slice(&bytes).context("decode persisted session")?;
        client
            .restore_session(session)
            .await
            .context("restore session")?;
        return Ok(());
    }

    client
        .matrix_auth()
        .login_username(&config.user, &config.password)
        .initial_device_display_name(&config.device_name)
        .send()
        .await
        .context("password login")?;

    if let Some(session) = client.matrix_auth().session() {
        let bytes = serde_json::to_vec(&session).context("encode session")?;
        std::fs::write(&session_path, bytes)
            .with_context(|| format!("persist session to {session_path:?}"))?;
    }
    Ok(())
}

/// Register the room-message event handler: decode text bodies, skip our own
/// echoes, and push onto the bounded inbound buffer.
fn register_message_handler(
    client: &Client,
    buffer: Arc<Mutex<VecDeque<Event>>>,
    own_user_id: matrix_sdk::ruma::OwnedUserId,
) {
    client.add_event_handler(move |ev: OriginalSyncRoomMessageEvent, room: Room| {
        let buffer = buffer.clone();
        let own = own_user_id.clone();
        async move {
            // Never re-ingest our own outbound messages (echo-loop guard).
            if ev.sender == own {
                return;
            }
            if let MessageType::Text(text) = ev.content.msgtype {
                let event = Event {
                    conversation: room.room_id().to_string(),
                    peer: ev.sender.to_string(),
                    body: text.body,
                };
                let dropped = push_bounded(
                    &mut buffer.lock().expect("buffer not poisoned"),
                    event,
                    INBOUND_CAP,
                );
                if dropped {
                    eprintln!("kastellan-worker-matrix: inbound buffer full; dropped oldest event");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn parse_config_reads_required_and_defaults() {
        let cfg = parse_config(getter(&[
            ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m.example.org"),
            ("KASTELLAN_MATRIX_USER", "@bot:m.example.org"),
            ("KASTELLAN_MATRIX_PASSWORD", "hunter2"),
            ("KASTELLAN_MATRIX_STORE", "/var/lib/k/matrix"),
        ]))
        .expect("required vars present");
        assert_eq!(cfg.homeserver_url, "https://m.example.org");
        assert_eq!(cfg.user, "@bot:m.example.org");
        assert_eq!(cfg.store_dir, PathBuf::from("/var/lib/k/matrix"));
        assert_eq!(cfg.device_name, DEVICE_NAME_DEFAULT);
        assert_eq!(cfg.proxy_uds, None);
    }

    #[test]
    fn parse_config_threads_optional_proxy_and_device_name() {
        let cfg = parse_config(getter(&[
            ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m.example.org"),
            ("KASTELLAN_MATRIX_USER", "bot"),
            ("KASTELLAN_MATRIX_PASSWORD", "pw"),
            ("KASTELLAN_MATRIX_STORE", "/store"),
            ("KASTELLAN_MATRIX_DEVICE_NAME", "kastellan-dgx"),
            ("KASTELLAN_EGRESS_PROXY_UDS", "/run/egress.sock"),
        ]))
        .expect("all vars present");
        assert_eq!(cfg.device_name, "kastellan-dgx");
        assert_eq!(cfg.proxy_uds, Some(PathBuf::from("/run/egress.sock")));
    }

    #[test]
    fn parse_config_fails_closed_on_missing_required() {
        let err = parse_config(getter(&[("KASTELLAN_MATRIX_USER", "bot")])).unwrap_err();
        assert!(err.to_string().contains("KASTELLAN_MATRIX_HOMESERVER_URL"));
    }

    #[test]
    fn drain_returns_all_and_empties() {
        let buf = Mutex::new(VecDeque::new());
        buf.lock().unwrap().push_back(Event {
            conversation: "!r:s".into(),
            peer: "@p:s".into(),
            body: "one".into(),
        });
        buf.lock().unwrap().push_back(Event {
            conversation: "!r:s".into(),
            peer: "@p:s".into(),
            body: "two".into(),
        });
        let drained = drain(&buf);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].body, "one");
        assert!(drain(&buf).is_empty(), "second drain sees an empty buffer");
    }
}
