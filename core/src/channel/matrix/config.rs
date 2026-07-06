//! Operator/daemon configuration parsing for the Matrix channel: the env-gated
//! [`MatrixConfig`] / [`MatrixSpawnConfig`] builders plus the pure homeserver-URL
//! parsers that scope the `Net::Allowlist` to the actual endpoint.
//!
//! All pure over injectable getters (`parse_daemon_spawn_config` takes a closure)
//! so the required/optional/`enforce_sandbox` contract is unit-tested without
//! mutating the process environment.
//!
//! Split out of the parent `matrix.rs` (2026-07-07 prod-split, Item 9b); every
//! public `matrix::…` path is byte-identical via the parent's `pub use`
//! re-exports. `parse_daemon_spawn_config` is `pub(crate)` for the tests
//! (`super::`), not part of the public surface.

use std::path::PathBuf;

use crate::channel::PeerId;

/// Operator configuration for the Matrix channel, read from the daemon env.
/// `from_env` returns `None` when `KASTELLAN_MATRIX_HOMESERVER` is unset — the
/// daemon then starts no channel bus and is byte-identical to a Matrix-less
/// build. The actual spawn (sandbox + egress + persistent store + the live
/// matrix-rust-sdk worker) + `ChannelBus` wiring is comms-slice-#2 Phase D.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixConfig {
    /// Homeserver host (e.g. `matrix.example.org`) — used for the `Net::Allowlist`.
    pub homeserver: String,
    /// Recognised peers (the fail-closed `StaticPairings` set until slice #3's
    /// pairing flow). Empty ⇒ deny all (logged).
    pub peers: Vec<PeerId>,
}

impl MatrixConfig {
    /// Read config from the env. `None` when the homeserver is unset.
    pub fn from_env() -> Option<Self> {
        let homeserver = std::env::var("KASTELLAN_MATRIX_HOMESERVER").ok()?;
        let peers = parse_peers_csv(&std::env::var("KASTELLAN_MATRIX_PEERS").unwrap_or_default());
        Some(Self { homeserver, peers })
    }
}

/// Build the daemon's [`MatrixSpawnConfig`] from the environment, gated on
/// `KASTELLAN_MATRIX_HOMESERVER_URL` (returns `None` when unset, so the
/// Matrix-less daemon is byte-identical). `exe_dir` is the directory holding the
/// daemon binary; the worker is its sibling unless `KASTELLAN_MATRIX_WORKER_BIN`
/// overrides.
///
/// Env contract:
/// - `KASTELLAN_MATRIX_HOMESERVER_URL` (required) — e.g. `https://matrix.kastellan.dev`.
/// - `KASTELLAN_MATRIX_USER` (required) — e.g. `@kastellan:matrix.kastellan.dev`.
/// - `KASTELLAN_MATRIX_STORE` (optional) — default `<state>/matrix/store`.
/// - `KASTELLAN_MATRIX_WORKER_BIN` (optional) — default `exe_dir/kastellan-worker-matrix`.
/// - `KASTELLAN_MATRIX_ENFORCE_SANDBOX` (optional, default on — `matrix_client`
///   seccomp [TSYNC'd] + Landlock) — `0`/`false` is the operator debug opt-out.
///
/// `password` is `None`: the daemon relies on the worker's persisted
/// `session.json` (do the one-time initial login with `kastellan-cli matrix
/// probe`). Materializing the password in-daemon needs the keyring initialized
/// outside the tokio runtime — a follow-up.
pub fn daemon_spawn_config_from_env(exe_dir: Option<&std::path::Path>) -> Option<MatrixSpawnConfig> {
    let default_store = crate::audit_mirror::default_state_dir().map(|d| d.join("matrix").join("store"));
    parse_daemon_spawn_config(|k| std::env::var(k).ok(), exe_dir, default_store.as_deref())
}

/// Pure builder behind [`daemon_spawn_config_from_env`] over an injectable getter
/// plus resolved defaults, so the required/optional/`enforce_sandbox` contract is
/// unit-tested without mutating the process environment. `default_store` is the
/// `<state>/matrix/store` fallback; `exe_dir` sources the worker-binary fallback.
pub(crate) fn parse_daemon_spawn_config(
    get: impl Fn(&str) -> Option<String>,
    exe_dir: Option<&std::path::Path>,
    default_store: Option<&std::path::Path>,
) -> Option<MatrixSpawnConfig> {
    let homeserver_url = get("KASTELLAN_MATRIX_HOMESERVER_URL")?;
    let user = get("KASTELLAN_MATRIX_USER")?;
    let store_dir = get("KASTELLAN_MATRIX_STORE")
        .map(PathBuf::from)
        .or_else(|| default_store.map(|p| p.to_path_buf()))?;
    let worker_bin = get("KASTELLAN_MATRIX_WORKER_BIN")
        .map(PathBuf::from)
        .or_else(|| exe_dir.map(|d| d.join("kastellan-worker-matrix")))?;
    // Default ON (fail-safe): only an explicit `0`/`false` disables the worker's
    // seccomp + Landlock.
    let enforce_sandbox = get("KASTELLAN_MATRIX_ENFORCE_SANDBOX")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let use_microvm = get("KASTELLAN_MATRIX_USE_MICROVM")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let password = get("KASTELLAN_MATRIX_PASSWORD").filter(|v| !v.is_empty());
    Some(MatrixSpawnConfig {
        worker_bin,
        homeserver_url,
        user,
        store_dir,
        password,
        device_name: Some("kastellan-daemon".to_string()),
        enforce_sandbox,
        use_microvm,
    })
}

/// Parse a comma-separated recognised-peer list into [`PeerId`]s, trimming
/// whitespace and dropping empty entries.
pub fn parse_peers_csv(csv: &str) -> Vec<PeerId> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| PeerId(s.to_string()))
        .collect()
}

/// Extract `(host, port)` from a homeserver URL for the `Net::Allowlist` entry.
/// The port is the explicit `:port` if present, else the scheme default
/// (`https` → 443, `http` → 80, no scheme → 443). Strips the scheme + any path
/// and handles bracketed IPv6 literals (`https://[::1]:8448` → `("::1", 8448)`).
/// This is what scopes egress to the *actual* homeserver endpoint, so a
/// self-hosted server on a non-443 port (e.g. `:8448`) is reachable.
pub fn host_port_from_url(url: &str) -> anyhow::Result<(String, u16)> {
    let (scheme, after_scheme) = match url.split_once("://") {
        Some((s, rest)) => (Some(s), rest),
        None => (None, url),
    };
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let (host, port_str) = if let Some(rest) = authority.strip_prefix('[') {
        // [ipv6]:port → host up to the closing bracket, optional `:port` after.
        let mut parts = rest.splitn(2, ']');
        let host = parts.next().unwrap_or(rest);
        let port = parts.next().unwrap_or("").strip_prefix(':');
        (host, port)
    } else {
        // host[:port] → split on the final colon.
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        anyhow::bail!("could not parse host from homeserver url {url:?}");
    }
    let port = match port_str {
        Some(p) => p
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("invalid port in homeserver url {url:?}"))?,
        None if scheme.is_some_and(|s| s.eq_ignore_ascii_case("http")) => 80,
        None => 443,
    };
    Ok((host.to_string(), port))
}

/// Extract the bare host from a homeserver URL (e.g. `https://matrix.example.org`
/// → `matrix.example.org`), dropping the port. Thin wrapper over
/// [`host_port_from_url`].
pub fn host_from_url(url: &str) -> anyhow::Result<String> {
    Ok(host_port_from_url(url)?.0)
}

/// Everything `spawn_matrix_worker` needs to bring up the live worker. The
/// homeserver URL + user are operator config (env). The `password` is only used
/// for the *initial* login; once the worker has persisted `session.json` in the
/// store it restores from that, so `None` is correct on every restart. Callers
/// that materialize the password from the Vault must do so themselves (the
/// keyring's secret-service backend must be initialized *outside* a tokio
/// runtime — see `kastellan-cli`'s `matrix probe`).
pub struct MatrixSpawnConfig {
    /// Path to the (live-matrix) worker binary.
    pub worker_bin: PathBuf,
    /// Full homeserver URL, e.g. `https://matrix.kastellan.dev`.
    pub homeserver_url: String,
    /// Login user (localpart or full `@user:server`).
    pub user: String,
    /// Persistent encrypted E2E store dir (created if absent).
    pub store_dir: PathBuf,
    /// Bot password — `Some` only for the initial login (no persisted session
    /// yet); `None` relies on the restored session.
    pub password: Option<String>,
    /// Optional device display name.
    pub device_name: Option<String>,
    /// When `false`, the worker runs with seccomp + Landlock disabled — an
    /// operator debug escape hatch (or SDK-correctness smoke runs). Production
    /// passes `true` (the install default): the worker then runs under the
    /// `matrix_client` seccomp profile (TSYNC'd across all threads) + Landlock.
    pub enforce_sandbox: bool,
    /// When `true` (Linux only, `KASTELLAN_MATRIX_USE_MICROVM=1`), the worker runs
    /// in a Firecracker VM: the caller resolves the `FirecrackerVm` backend and
    /// `spawn_matrix_worker` builds the VM policy (persistent_store at /data + baked
    /// rootfs). Ignored on macOS. Default `false` ⇒ the 5b-4a bwrap/Seatbelt path.
    pub use_microvm: bool,
}
