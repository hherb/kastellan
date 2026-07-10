//! Spawn a per-worker embed-broker sidecar and wait for it to bind its UDS.
//!
//! Mirrors [`crate::egress::spawn::spawn_sidecar`] but is simpler: the broker is
//! a plain sandboxed `Child` that serves JSON-RPC `embed` over its UDS (not over
//! stdio, so there is no `Client` handshake), forwarding to the operator's
//! embedding backend. There is no MITM CA, no decision stream, and no cert-pin
//! config — just:
//!   1. mint a short scratch dir (`embed-<pid>-<seq>`),
//!   2. spawn the broker under `Net::Allowlist([backend host:port])` with the
//!      broker's UDS + endpoint env, deriving the worker-prelude lockdown env
//!      exactly like every other spawn (the `e70174b` lesson — without it the
//!      broker's own Landlock would block its DNS),
//!   3. wait (bounded) for `embed.sock`, and
//!   4. hand back an [`EmbedBrokerSidecar`] whose `Drop` kills the broker and
//!      removes the scratch dir 1:1 with the consuming worker.

use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};
use url::Url;

use super::config::EmbedBrokerConfig;
use super::{EmbedBrokerSpec, EMBED_BROKER_UDS_ENV};
use crate::egress::scratch_sweep::EMBED_SCRATCH_DIR_PREFIX;
use crate::tool_host::{derive_lockdown_env, ToolHostError};

/// Env key the broker binary reads for the socket path it `bind()`s — the shared
/// [`EMBED_BROKER_UDS_ENV`] contract (same value core injects into the worker).
const ENV_BROKER_UDS: &str = EMBED_BROKER_UDS_ENV;
/// Env key the broker binary reads for the backend embeddings URL to forward to.
const ENV_BROKER_ENDPOINT: &str = "KASTELLAN_EMBED_BROKER_ENDPOINT";

/// Basename of the broker's UDS under its scratch dir. The broker `bind()`s
/// `<scratch>/embed.sock`; core binds the same path into the worker's jail.
const UDS_FILE_NAME: &str = "embed.sock";

/// How long [`spawn_embed_broker`] waits for the broker to `bind()` its UDS.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const READY_POLL: Duration = Duration::from_millis(25);

/// Cumulative CPU budget (ms → RLIMIT_CPU seconds) for the broker. It lives 1:1
/// with a single web-research dispatch (SingleUse, 60s wall-clock), so it is
/// short-lived — a bounded `RLIMIT_CPU` is defense-in-depth (the only per-process
/// CPU primitive on macOS). Embedding forwarding is I/O-bound, so 10s of CPU is
/// generous. Matches the egress short-lived sidecar cap (issue #395).
///
/// **Revisit before the first broker-backed `IdleTimeout` worker.** This cap is
/// sized for one SingleUse dispatch. A warm `IdleTimeout` worker keeps the broker
/// it was cold-spawned with across many dispatches, so its cumulative CPU could
/// eventually hit this cap and RLIMIT_CPU would SIGKILL the broker — silently
/// breaking that worker's embed route while it stays warm. Every broker-backed
/// worker today is SingleUse, so this is latent; the fix (mirroring the egress
/// sidecar's `long_lived` split — `cpu_ms: 0` for a long-lived broker, bounded
/// otherwise) lands with the first such worker.
const BROKER_CPU_MS: u64 = 10_000;

/// Max byte length of a `sockaddr_un.sun_path` (104 macOS / 108 Linux, incl. the
/// NUL). The broker binds `<scratch>/embed.sock`, so the scratch dir must be
/// short enough that the projected socket path still fits — see
/// [`make_broker_scratch_dir`]. Mirrors the egress `SUN_PATH_MAX`.
#[cfg(target_os = "macos")]
const SUN_PATH_MAX: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_MAX: usize = 108;

/// A running broker sidecar, held on [`crate::tool_host::SupervisedWorker`]'s
/// additive `embed_broker` field. Its [`Drop`] kills + reaps the broker (removing
/// the UDS) and removes the owned scratch dir, so teardown is 1:1 with the worker.
pub struct EmbedBrokerSidecar {
    child: Child,
    uds_path: PathBuf,
    /// Per-worker scratch dir holding `embed.sock`, owned for RAII cleanup.
    scratch: PathBuf,
}

impl Drop for EmbedBrokerSidecar {
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

impl EmbedBrokerSidecar {
    /// The bound UDS path. Core binds this into the worker's jail via
    /// [`kastellan_sandbox::SandboxPolicy::embed_broker_uds`] and injects it as
    /// `KASTELLAN_EMBED_BROKER_UDS` (Task 4).
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
/// (its only egress is the operator's embedding backend), `WorkerNetClient` (must
/// permit AF_UNIX accept + AF_INET connect — DGX-verify), fs_read for the DNS
/// resolver files + the binary, fs_write for the scratch dir (to `bind()` the
/// UDS), and the broker's UDS + endpoint env.
fn broker_policy(binary: &Path, endpoint: &str, scratch: &Path) -> SandboxPolicy {
    let uds = scratch.join(UDS_FILE_NAME);
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
            (ENV_BROKER_UDS.to_string(), uds.to_string_lossy().into_owned()),
            (ENV_BROKER_ENDPOINT.to_string(), endpoint.to_string()),
        ],
        proxy_uds: None,
        embed_broker_uds: None,
        persistent_store: None,
    }
}

/// Mint a unique scratch subdir under `scratch_root` for one broker's UDS. Name
/// is `embed-<pid>-<seq>` — `pid` scopes it to this daemon, `seq` (a
/// process-lifetime atomic) guarantees uniqueness across concurrent spawns. Kept
/// in sync with [`EMBED_SCRATCH_DIR_PREFIX`] so the #251 startup sweep reclaims
/// husks. Rejects up front if `<dir>/embed.sock` would overflow `sun_path`.
fn make_broker_scratch_dir(scratch_root: &Path) -> Result<PathBuf, ToolHostError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = scratch_root.join(format!(
        "{}{}-{}",
        EMBED_SCRATCH_DIR_PREFIX,
        std::process::id(),
        seq
    ));
    let projected_uds = dir.join(UDS_FILE_NAME);
    let uds_len = projected_uds.as_os_str().len();
    if uds_len + 1 > SUN_PATH_MAX {
        return Err(ToolHostError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "embed-broker socket path is {uds_len} bytes (+NUL), over the \
                 {SUN_PATH_MAX}-byte sockaddr_un.sun_path limit — shorten \
                 KASTELLAN_EMBED_BROKER_SCRATCH_DIR (projected: {})",
                projected_uds.display()
            ),
        )));
    }
    std::fs::create_dir_all(&dir).map_err(ToolHostError::Io)?;
    Ok(dir)
}

/// Outcome of waiting for the broker to bind its UDS. Distinguishes a clean bind
/// from an early process exit (so the caller can surface the real exit status
/// instead of a misleading bind-timeout) and from a genuine timeout.
enum BrokerReady {
    /// The UDS appeared — the broker is (about to be) serving.
    Bound,
    /// The broker process exited before binding — fail fast with its status.
    Exited(ExitStatus),
    /// Neither happened within the deadline.
    TimedOut,
}

/// Poll for `uds` to exist, up to `timeout`, while also watching for the broker
/// to exit early (via the injected `exited` probe). Returns as soon as either the
/// socket appears or the broker dies, so a broker that fails at startup (bad
/// endpoint, panic during bind) surfaces its exit status immediately rather than
/// blocking the full `timeout`. `exited` is a closure (not a `&mut Child`
/// directly) so the readiness contract stays hermetically unit-testable without a
/// real process (the live bind is DGX-gated).
fn wait_for_broker_ready(
    uds: &Path,
    timeout: Duration,
    mut exited: impl FnMut() -> Option<ExitStatus>,
) -> BrokerReady {
    let deadline = Instant::now() + timeout;
    loop {
        if uds.exists() {
            return BrokerReady::Bound;
        }
        if let Some(status) = exited() {
            return BrokerReady::Exited(status);
        }
        if Instant::now() >= deadline {
            return BrokerReady::TimedOut;
        }
        std::thread::sleep(READY_POLL);
    }
}

/// Spawn the embed-broker sidecar for one broker-backed worker and wait (bounded)
/// for it to bind its UDS. Fail-closed: on spawn failure or bind timeout the
/// scratch dir is removed and the broker killed, and an `Err` is returned (no
/// half-spawned broker, no orphan worker).
///
/// Returns the [`EmbedBrokerSidecar`] (RAII bundle for the worker to own) and the
/// bound UDS path (which core binds into the worker's jail + injects as
/// `KASTELLAN_EMBED_BROKER_UDS`).
///
/// `backend` must be a **host** sandbox backend (Seatbelt/Bwrap) — v1 broker mode
/// is host-only (VM × broker is deferred; the manifest ignores the broker gate
/// under `USE_MICROVM`), so the worker's backend is the host default and is passed
/// through here.
pub fn spawn_embed_broker(
    cfg: &EmbedBrokerConfig,
    spec: &EmbedBrokerSpec,
    backend: &dyn SandboxBackend,
) -> Result<(EmbedBrokerSidecar, PathBuf), ToolHostError> {
    // Fail fast on a malformed endpoint: an unparseable/hostless URL yields an
    // empty broker allowlist, so the broker would spawn with no reachable backend
    // and only surface as a 5s bind-timeout. Reject it up front, before minting
    // scratch or spawning, with an error that names the bad endpoint.
    if broker_allowlist_from_endpoint(&spec.endpoint).is_empty() {
        return Err(ToolHostError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "embed-broker endpoint {:?} has no parseable host:port — refusing to spawn",
                spec.endpoint
            ),
        )));
    }
    let scratch = make_broker_scratch_dir(&cfg.scratch_root)?;
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
/// dir has a single fail-closed cleanup owner in [`spawn_embed_broker`].
fn spawn_broker_in(
    cfg: &EmbedBrokerConfig,
    spec: &EmbedBrokerSpec,
    backend: &dyn SandboxBackend,
    scratch: &Path,
) -> Result<EmbedBrokerSidecar, ToolHostError> {
    let uds_path = scratch.join(UDS_FILE_NAME);
    let _ = std::fs::remove_file(&uds_path);

    let policy = broker_policy(&cfg.broker_bin, &spec.endpoint, scratch);
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
        std::thread::spawn(move || crate::worker_stderr::drain_reader(pid, stdout, None));
    }

    match wait_for_broker_ready(&uds_path, READY_TIMEOUT, || child.try_wait().ok().flatten()) {
        BrokerReady::Bound => Ok(EmbedBrokerSidecar {
            child,
            uds_path,
            scratch: scratch.to_path_buf(),
        }),
        // Broker died before binding — reap it and surface its real exit status
        // (not a misleading bind-timeout).
        BrokerReady::Exited(status) => {
            let _ = child.wait();
            Err(ToolHostError::Io(std::io::Error::other(format!(
                "embed-broker exited before binding {uds_path:?} (status: {status})"
            ))))
        }
        BrokerReady::TimedOut => {
            let _ = child.kill();
            let _ = child.wait();
            Err(ToolHostError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("embed-broker did not bind {uds_path:?} within {READY_TIMEOUT:?}"),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_uses_explicit_port() {
        assert_eq!(
            broker_allowlist_from_endpoint("http://127.0.0.1:11434/v1/embeddings"),
            vec!["127.0.0.1:11434".to_string()]
        );
    }

    #[test]
    fn allowlist_defaults_https_port() {
        assert_eq!(
            broker_allowlist_from_endpoint("https://embed.example.org/embed"),
            vec!["embed.example.org:443".to_string()]
        );
    }

    #[test]
    fn allowlist_empty_for_unparseable_endpoint() {
        assert!(broker_allowlist_from_endpoint("not a url").is_empty());
    }

    #[test]
    fn policy_shape_is_net_client_allowlist_with_env() {
        let p = broker_policy(
            Path::new("/opt/embed-broker"),
            "http://127.0.0.1:11434/v1/embeddings",
            Path::new("/scratch"),
        );
        assert!(matches!(p.profile, Profile::WorkerNetClient));
        match &p.net {
            Net::Allowlist(hosts) => assert_eq!(hosts, &vec!["127.0.0.1:11434".to_string()]),
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
        assert!(p.fs_write.contains(&PathBuf::from("/scratch")));
        assert!(p.embed_broker_uds.is_none(), "the broker itself has no upstream broker");
        assert!(p.proxy_uds.is_none());
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_BROKER_UDS], "/scratch/embed.sock");
        assert_eq!(env[ENV_BROKER_ENDPOINT], "http://127.0.0.1:11434/v1/embeddings");
    }

    #[test]
    fn derived_broker_policy_carries_lockdown_env_for_dns() {
        // Regression pin (e70174b lesson): the broker spawn derives the
        // worker-prelude lockdown env from its policy, so seccomp + Landlock RO
        // grants for the DNS resolver files are present. Without them the broker's
        // own lock_down would block its backend DNS post-lockdown.
        let p = broker_policy(
            Path::new("/opt/embed-broker"),
            "http://127.0.0.1:11434/v1/embeddings",
            Path::new("/scratch"),
        );
        let d = derive_lockdown_env(&p);
        let env: std::collections::HashMap<_, _> = d.env.into_iter().collect();
        assert_eq!(env["KASTELLAN_SECCOMP_PROFILE"], "net_client");
        let ro: Vec<String> = serde_json::from_str(&env["KASTELLAN_LANDLOCK_RO"]).unwrap();
        for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"] {
            assert!(ro.iter().any(|r| r == path), "Landlock RO must grant {path}");
        }
    }

    #[test]
    fn scratch_dir_name_uses_embed_prefix() {
        let dir = make_broker_scratch_dir(Path::new("/tmp")).expect("mint under /tmp");
        let name = dir.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(EMBED_SCRATCH_DIR_PREFIX), "unexpected name: {name}");
        // Clean up the real dir this created.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scratch_dir_rejects_overlong_sun_path() {
        // A pathological scratch root whose projected embed.sock overflows sun_path
        // must fail-closed BEFORE creating the dir.
        let long_root = PathBuf::from(format!("/tmp/{}", "x".repeat(SUN_PATH_MAX)));
        let err = make_broker_scratch_dir(&long_root).expect_err("must reject overlong path");
        match err {
            ToolHostError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
            other => panic!("expected Io(InvalidInput), got {other:?}"),
        }
    }

    #[test]
    fn wait_for_broker_ready_times_out_when_absent_and_alive() {
        // Hermetic readiness pin: a socket that never appears AND a broker that
        // never exits → TimedOut quickly (the live bind path is DGX-gated).
        let missing = PathBuf::from("/tmp/kastellan-embed-broker-nonexistent-xyz.sock");
        let _ = std::fs::remove_file(&missing);
        let ready = wait_for_broker_ready(&missing, Duration::from_millis(60), || None);
        assert!(matches!(ready, BrokerReady::TimedOut));
    }

    #[test]
    fn wait_for_broker_ready_reports_early_exit_without_waiting_full_timeout() {
        // A broker that exits before binding is reported as `Exited` immediately,
        // NOT after the (here 10s) timeout — the whole point of watching liveness.
        let missing = PathBuf::from("/tmp/kastellan-embed-broker-early-exit-xyz.sock");
        let _ = std::fs::remove_file(&missing);
        // Spawn a real short-lived process just to obtain a genuine ExitStatus.
        let status = std::process::Command::new("true")
            .status()
            .expect("run `true`");
        let start = Instant::now();
        let ready = wait_for_broker_ready(&missing, Duration::from_secs(10), || Some(status));
        assert!(matches!(ready, BrokerReady::Exited(_)));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "must return on early exit, not block the full timeout"
        );
    }

    /// Backend whose spawn always fails — proves the endpoint check runs *before*
    /// any spawn (a passing test means the backend was never reached).
    struct FailBackend;
    impl SandboxBackend for FailBackend {
        fn spawn_under_policy(
            &self,
            _policy: &SandboxPolicy,
            _program: &str,
            _args: &[&str],
        ) -> Result<Child, kastellan_sandbox::SandboxError> {
            Err(kastellan_sandbox::SandboxError::Backend("test: spawn refused".into()))
        }
    }

    #[test]
    fn spawn_rejects_malformed_endpoint_before_touching_the_backend() {
        // A hostless/unparseable endpoint is refused up front (fail-fast, clear
        // error) rather than surfacing as a 5s bind-timeout. The check runs before
        // scratch is minted or the backend is spawned — `FailBackend` (which would
        // error if reached) is never invoked, so the `InvalidInput` error proves
        // the early rejection.
        let cfg = EmbedBrokerConfig::new(
            PathBuf::from("/nonexistent/kastellan-worker-embed-broker"),
            PathBuf::from("/tmp"),
        );
        let spec = EmbedBrokerSpec::new("not a url", "test-model");
        // `EmbedBrokerSidecar` isn't `Debug`, so match rather than `expect_err`.
        match spawn_embed_broker(&cfg, &spec, &FailBackend) {
            Err(ToolHostError::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
                assert!(
                    e.to_string().contains("no parseable host:port"),
                    "error should name the malformed endpoint, got: {e}"
                );
            }
            Err(other) => panic!("expected Io(InvalidInput), got {other:?}"),
            Ok(_) => panic!("expected malformed endpoint to be rejected, but a broker spawned"),
        }
    }
}
