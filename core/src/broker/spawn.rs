//! Spawn a per-worker broker sidecar and wait for it to bind its UDS.
//!
//! Mirrors [`crate::egress::spawn::spawn_sidecar`] but is simpler: the broker is
//! a plain sandboxed `Child` that serves JSON-RPC over its UDS (not over stdio,
//! so there is no `Client` handshake), forwarding to the operator's backend.
//! There is no MITM CA, no decision stream, and no cert-pin config — just:
//!   1. mint a short scratch dir (`<prefix><pid>-<seq>`),
//!   2. spawn the broker under `Net::Allowlist([backend host:port])` with the
//!      broker's UDS + endpoint env, deriving the worker-prelude lockdown env
//!      exactly like every other spawn (the `e70174b` lesson — without it the
//!      broker's own Landlock would block its DNS),
//!   3. wait (bounded) for the socket, and
//!   4. hand back a [`BrokerSidecar`] whose `Drop` kills the broker and removes
//!      the scratch dir 1:1 with the consuming worker.
//!
//! Every per-kind string (env keys, socket basename, scratch prefix) comes from
//! [`BrokerKind`] via `spec.kind`, so a second broker kind reuses this path.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};
use url::Url;

use super::config::BrokerConfig;
use super::kind::BrokerKind;
use super::BrokerSpec;
use crate::sidecar_ready::{may_kill_after_failed_poll, wait_for_sidecar_ready, SidecarReady};
use crate::tool_host::{derive_lockdown_env, ToolHostError};

/// How long [`spawn_broker`] waits for the broker to `bind()` its UDS.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Cumulative CPU budget (ms → RLIMIT_CPU seconds) for the broker. It lives 1:1
/// with a single web-research dispatch (SingleUse, 60s wall-clock), so it is
/// short-lived — a bounded `RLIMIT_CPU` is defense-in-depth (the only per-process
/// CPU primitive on macOS). Backend forwarding is I/O-bound, so 10s of CPU is
/// generous. Matches the egress short-lived sidecar cap (issue #395).
///
/// **Revisit before the first broker-backed `IdleTimeout` worker.** This cap is
/// sized for one SingleUse dispatch. A warm `IdleTimeout` worker keeps the broker
/// it was cold-spawned with across many dispatches, so its cumulative CPU could
/// eventually hit this cap and RLIMIT_CPU would SIGKILL the broker — silently
/// breaking that worker's backend route while it stays warm. Every broker-backed
/// worker today is SingleUse, so this is latent; the fix (mirroring the egress
/// sidecar's `long_lived` split — `cpu_ms: 0` for a long-lived broker, bounded
/// otherwise) lands with the first such worker.
const BROKER_CPU_MS: u64 = 10_000;

/// Max byte length of a `sockaddr_un.sun_path` (104 macOS / 108 Linux, incl. the
/// NUL). The broker binds `<scratch>/<uds_file>`, so the scratch dir must be
/// short enough that the projected socket path still fits — see
/// [`make_broker_scratch_dir`]. Mirrors the egress `SUN_PATH_MAX`.
#[cfg(target_os = "macos")]
const SUN_PATH_MAX: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_MAX: usize = 108;

/// A running broker sidecar, held on [`crate::tool_host::SupervisedWorker`]'s
/// additive `broker` field. Its [`Drop`] kills + reaps the broker (removing the
/// UDS) and removes the owned scratch dir, so teardown is 1:1 with the worker.
pub struct BrokerSidecar {
    child: Child,
    uds_path: PathBuf,
    /// Per-worker scratch dir holding the broker's UDS, owned for RAII cleanup.
    scratch: PathBuf,
}

impl Drop for BrokerSidecar {
    fn drop(&mut self) {
        // Kill + reap the broker, then remove its socket + scratch dir.
        // Best-effort — a left-behind scratch dir is a leak, never a safety
        // issue, and must not wedge worker teardown.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.uds_path);
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

impl BrokerSidecar {
    /// The bound UDS path. Core binds this into the worker's jail via
    /// [`kastellan_sandbox::SandboxPolicy::broker_uds`] and injects it as the
    /// kind's `uds_env()` (the spawn chokepoint).
    pub fn uds_path(&self) -> &Path {
        &self.uds_path
    }
}

/// Pure: the broker's own `Net::Allowlist` entry (`host:port`) for the backend it
/// forwards to. Port defaults: the URL's explicit port, else the scheme default
/// (443 https / 80 http, via `port_or_known_default`, falling back to 443).
/// Returns an empty vec for an unparseable/hostless endpoint — the broker will
/// then fail closed at its own `validate_endpoint`/URL parse.
fn broker_allowlist_from_endpoint(endpoint: &str) -> Vec<String> {
    match Url::parse(endpoint) {
        Ok(u) => match u.host_str() {
            Some(host) => vec![format!("{host}:{}", u.port_or_known_default().unwrap_or(443))],
            None => vec![],
        },
        Err(_) => vec![],
    }
}

/// Pure: the sandbox policy for the broker. `Net::Allowlist([backend host:port])`
/// (its only egress is the operator's backend), `WorkerNetClient` (must permit
/// AF_UNIX accept + AF_INET connect — DGX-verify), fs_read for the DNS resolver
/// files + the binary, fs_write for the scratch dir (to `bind()` the UDS), and the
/// broker's UDS + endpoint env (keyed off `kind`).
fn broker_policy(binary: &Path, endpoint: &str, scratch: &Path, kind: BrokerKind) -> SandboxPolicy {
    let uds = scratch.join(kind.uds_file());
    SandboxPolicy {
        fs_read: vec![
            binary.to_path_buf(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![scratch.to_path_buf()],
        net: Net::Allowlist(broker_allowlist_from_endpoint(endpoint)),
        cpu_ms: BROKER_CPU_MS,
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env: vec![
            (kind.uds_env().to_string(), uds.to_string_lossy().into_owned()),
            (kind.endpoint_env().to_string(), endpoint.to_string()),
        ],
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    }
}

/// Mint a unique scratch subdir under `scratch_root` for one broker's UDS. Name
/// is `<prefix><pid>-<seq>` — `pid` scopes it to this daemon, `seq` (a
/// process-lifetime atomic) guarantees uniqueness across concurrent spawns. The
/// prefix (`kind.scratch_prefix()`) is kept in sync with the #251 startup sweep
/// (`crate::egress::scratch_sweep`) so it reclaims husks. Rejects up front if
/// `<dir>/<uds_file>` would overflow `sun_path`.
fn make_broker_scratch_dir(scratch_root: &Path, kind: BrokerKind) -> Result<PathBuf, ToolHostError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = scratch_root.join(format!(
        "{}{}-{}",
        kind.scratch_prefix(),
        std::process::id(),
        seq
    ));
    let projected_uds = dir.join(kind.uds_file());
    let uds_len = projected_uds.as_os_str().len();
    if uds_len + 1 > SUN_PATH_MAX {
        return Err(ToolHostError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "broker socket path is {uds_len} bytes (+NUL), over the \
                 {SUN_PATH_MAX}-byte sockaddr_un.sun_path limit — shorten \
                 {} (projected: {})",
                kind.scratch_dir_env(),
                projected_uds.display()
            ),
        )));
    }
    // Exclusive + 0700 — same reasoning as `egress::net_worker` (audit
    // 2026-09-02, S1): never adopt a pre-existing per-spawn directory.
    kastellan_sandbox::private_dir::create_private_dir(&dir).map_err(ToolHostError::Io)?;
    Ok(dir)
}

/// Spawn the broker sidecar for one broker-backed worker and wait (bounded) for
/// it to bind its UDS. Fail-closed: on spawn failure or bind timeout the scratch
/// dir is removed and the broker killed, and an `Err` is returned (no
/// half-spawned broker, no orphan worker).
///
/// Returns the [`BrokerSidecar`] (RAII bundle for the worker to own) and the
/// bound UDS path (which core binds into the worker's jail + injects as the
/// kind's `uds_env()`).
///
/// `backend` must be a **host** sandbox backend (Seatbelt/Bwrap) — v1 broker mode
/// is host-only (VM × broker is deferred; the manifest ignores the broker gate
/// under `USE_MICROVM`), so the worker's backend is the host default and is passed
/// through here.
pub fn spawn_broker(
    cfg: &BrokerConfig,
    spec: &BrokerSpec,
    backend: &dyn SandboxBackend,
) -> Result<(BrokerSidecar, PathBuf), ToolHostError> {
    // The chokepoint resolves `cfg` via `BrokerConfigs::for_kind(spec.kind)`, so
    // the config and spec kinds are the same by construction. Pin that invariant.
    debug_assert_eq!(cfg.kind, spec.kind, "broker config/spec kind mismatch at spawn");
    // Fail fast on a malformed endpoint: an unparseable/hostless URL yields an
    // empty broker allowlist, so the broker would spawn with no reachable backend
    // and only surface as a 5s bind-timeout. Reject it up front, before minting
    // scratch or spawning, with an error that names the bad endpoint.
    if broker_allowlist_from_endpoint(&spec.endpoint).is_empty() {
        return Err(ToolHostError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "broker endpoint {:?} has no parseable host:port — refusing to spawn",
                spec.endpoint
            ),
        )));
    }
    let scratch = make_broker_scratch_dir(&cfg.scratch_root, spec.kind)?;
    match spawn_broker_in(cfg, spec, backend, &scratch) {
        Ok(sidecar) => {
            let uds = sidecar.uds_path().to_path_buf();
            Ok((sidecar, uds))
        }
        Err(e) => {
            // No sidecar to own the scratch dir — remove it now (fail-closed).
            let _ = std::fs::remove_dir_all(&scratch);
            Err(e)
        }
    }
}

/// Inner spawn against an already-minted `scratch` dir. Split out so the scratch
/// dir has a single fail-closed cleanup owner in [`spawn_broker`].
fn spawn_broker_in(
    cfg: &BrokerConfig,
    spec: &BrokerSpec,
    backend: &dyn SandboxBackend,
    scratch: &Path,
) -> Result<BrokerSidecar, ToolHostError> {
    let uds_path = scratch.join(spec.kind.uds_file());
    let _ = std::fs::remove_file(&uds_path);

    let policy = broker_policy(&cfg.broker_bin, &spec.endpoint, scratch, spec.kind);
    // Derive the worker-prelude lockdown env (seccomp + Landlock RO/RW) exactly
    // like every other spawn. Without it the broker's in-process lock_down would
    // run without its fs_read grants and DNS would fail post-lockdown — the
    // `e70174b` egress-proxy lesson (see egress::spawn).
    let derived = derive_lockdown_env(&policy);
    let program = cfg.broker_bin.to_string_lossy();
    let mut child = backend.spawn_under_policy(&derived, &program, &[])?;

    // Drain BOTH of the broker's stdio pipes so neither can fill (~64 KiB) and
    // deadlock it. The broker serves over the UDS, not stdio, so core never reads
    // either pipe: today it writes only to stderr, but draining stdout too makes
    // the no-deadlock guarantee independent of that (a future stdout write, or a
    // prelude diagnostic, can't stall the broker).
    let pid = child.id();
    if let Some(stderr) = child.stderr.take() {
        crate::worker_stderr::spawn_drain(pid, stderr);
    }
    if let Some(stdout) = child.stdout.take() {
        crate::worker_stderr::spawn_unread_drain(pid, stdout, "stdout");
    }

    match wait_for_sidecar_ready(READY_TIMEOUT, || uds_path.exists(), || child.try_wait()) {
        SidecarReady::Ready => Ok(BrokerSidecar {
            child,
            uds_path,
            scratch: scratch.to_path_buf(),
        }),
        // Broker died before binding — reap it and surface its real exit status
        // (not a misleading bind-timeout).
        SidecarReady::Exited(status) => {
            let _ = child.wait();
            Err(ToolHostError::Io(std::io::Error::other(format!(
                "broker exited before binding {uds_path:?} (status: {status})"
            ))))
        }
        SidecarReady::TimedOut => {
            let _ = child.kill();
            let _ = child.wait();
            Err(ToolHostError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("broker did not bind {uds_path:?} within {READY_TIMEOUT:?}"),
            )))
        }
        // Fail closed like the timeout: we cannot see the broker, so do not keep
        // it — unless `ECHILD` says it is no longer our child, when its pid may
        // already belong to another process and must not be signalled.
        SidecarReady::Unpollable(e) => {
            if may_kill_after_failed_poll(&e) {
                let _ = child.kill();
                let _ = child.wait();
            }
            Err(ToolHostError::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "could not poll the broker's exit status while waiting for {uds_path:?} \
                     ({e}) — our waitpid failed, not the broker's bind"
                ),
            )))
        }
    }
}

#[cfg(test)]
mod tests;
