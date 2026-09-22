//! Spawn the sandboxed egress-proxy sidecar on a per-worker UDS and wait for it
//! to be ready. Reusable host-side API; slice #2 calls this from the net-worker
//! bring-up path and ties `SidecarHandle::shutdown` to worker-terminal teardown.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

use crate::worker_stderr::{TailState, TAIL_DRAIN_WAIT};

/// Env keys the sidecar binary reads (must match `egress-proxy::main`).
const ENV_UDS: &str = "KASTELLAN_EGRESS_PROXY_UDS";
const ENV_ALLOWLIST: &str = "KASTELLAN_EGRESS_PROXY_ALLOWLIST";
const ENV_WORKER: &str = "KASTELLAN_EGRESS_PROXY_WORKER";
const ENV_PINS: &str = "KASTELLAN_EGRESS_PROXY_PINS";
/// Env key that puts the sidecar into no-MITM (transparent-tunnel) mode for
/// workers that do their own end-to-end TLS (the browser). Must match the read
/// in `egress-proxy::main`.
const ENV_DISABLE_MITM: &str = "KASTELLAN_EGRESS_PROXY_DISABLE_MITM";
/// Env key pointing the sidecar at an operator-provided extra CA to trust on the
/// re-origination (upstream) leg — for a self-signed private origin (localmail,
/// #491). Must match the read in `egress-proxy::main`.
const ENV_UPSTREAM_EXTRA_CA: &str = "KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA";

/// Basename of the per-worker sidecar UDS under the scratch dir. Shared so the
/// force-routing scratch-dir guard (`net_worker::make_worker_scratch_dir`) can
/// project the exact socket path the sidecar will `bind()`.
pub(crate) const UDS_FILE_NAME: &str = "egress.sock";

/// Basename of the per-worker CA cert the sidecar exports for the host to inject
/// into the worker's trust store (slice #3a). Lives beside the UDS in scratch.
pub(crate) const CA_FILE_NAME: &str = "ca.pem";

/// How long `spawn_sidecar` waits for the proxy to `bind()` its UDS.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const READY_POLL: Duration = Duration::from_millis(25);

/// Cumulative CPU budget (ms → ceil-div RLIMIT_CPU seconds) for a **short-lived**
/// per-tool-call sidecar. Matches the web-fetch worker's own `cpu_ms` (the
/// sidecar lives 1:1 with that single dispatch), and restores the CPU
/// defense-in-depth that `e70174b` had to drop blanket-wide (issue #395). A
/// long-lived channel sidecar (matrix) gets `0` instead — see [`proxy_policy`].
const SHORT_LIVED_SIDECAR_CPU_MS: u64 = 10_000;

/// A running sidecar. Dropping it kills and reaps the child and removes the
/// UDS; [`shutdown`](SidecarHandle::shutdown) and
/// [`terminate`](SidecarHandle::terminate) do the same thing explicitly.
///
/// The `Drop` is load-bearing, not a convenience (issue #502). Every spawn
/// path builds a bare handle first and only later wraps it in the
/// `Drop`-bearing `egress::net_worker::EgressSidecar` — after the *worker*
/// has also spawned. If that second spawn fails, the bare handle is dropped
/// by `?` on the error path, and without this impl that drop was a no-op
/// (`std::process::Child`'s own drop does not kill the child), so the proxy
/// survived for as long as the spawning thread lived — which for a
/// long-lived channel's persistent supervisor thread is effectively forever.
/// Both channel families retry that path in a loop, so the leak accumulated
/// one `kastellan-worker-egress-proxy` per failed attempt, per channel.
#[derive(Debug)]
pub struct SidecarHandle {
    child: Child,
    pub uds_path: PathBuf,
}

impl Drop for SidecarHandle {
    fn drop(&mut self) {
        // Idempotent with an explicit `terminate()`/`shutdown()` — a second
        // `kill`/`wait` on a reaped child and a `remove_file` on a gone path
        // both error harmlessly, and every error here is ignored by design.
        // `EgressSidecar::drop` still calls `terminate()` explicitly because
        // it needs the proxy dead *before* it removes the scratch dir, and a
        // field's drop glue runs only after that body returns.
        self.terminate();
    }
}

impl SidecarHandle {
    /// Kill the sidecar and reap it. Idempotent-ish (errors ignored).
    pub fn shutdown(mut self) {
        self.terminate();
    }

    /// Kill + reap the sidecar and remove its UDS, in place. Idempotent-ish
    /// (errors ignored). Shared by [`shutdown`](Self::shutdown) and by the
    /// coupled-teardown `Drop` of `egress::net_worker::EgressSidecar`, which
    /// holds the handle by value and cannot consume `self`.
    pub fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.uds_path);
    }

    /// Borrow the child's stdout for the caller's decision-ingest loop.
    pub fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }
}

/// The TLS posture of a worker's egress sidecar.
///
/// One value rather than two fields, because the two are not independent: an
/// upstream trust anchor is meaningful ONLY on the re-origination leg, and that
/// leg exists only when the proxy terminates the worker's TLS. Before #494 this
/// was a `disable_mitm: bool` beside an `upstream_extra_ca: Option<&Path>`, and
/// the nonsensical pair (a tunnel handed an anchor it can never consult) had to
/// be rejected at runtime by [`check_upstream_extra_ca`]. That rule is not gone
/// — it moved into this type, where the pair cannot be written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mitm<'a> {
    /// The proxy terminates the worker's TLS and re-originates upstream. The
    /// worker trusts the sidecar's per-instance CA (exported beside the UDS);
    /// the sidecar validates the real origin with webpki plus
    /// `upstream_extra_ca` when the operator configured one (#491/#492).
    Intercept { upstream_extra_ca: Option<&'a Path> },
    /// The proxy relays ciphertext untouched; the worker validates the origin
    /// itself and never receives our CA. For workers that cannot be made to
    /// trust a per-instance CA — the browser (Chromium's NSS store) and
    /// matrix-sdk.
    Transparent,
}

impl<'a> Mitm<'a> {
    /// Whether the sidecar must be told to skip interception.
    pub(crate) fn is_transparent(&self) -> bool {
        matches!(self, Mitm::Transparent)
    }

    /// The upstream trust anchor, if this posture can use one. `Transparent`
    /// always yields `None` — structurally, not by convention.
    pub(crate) fn upstream_extra_ca(&self) -> Option<&'a Path> {
        match self {
            Mitm::Intercept { upstream_extra_ca } => *upstream_extra_ca,
            Mitm::Transparent => None,
        }
    }
}

/// Everything [`proxy_policy`] and [`spawn_sidecar`] need to describe one
/// sidecar. A struct rather than 8-9 positional arguments (#494): the old
/// signature had `disable_mitm` and `long_lived` adjacent as bare bools, and
/// transposing them compiled silently into both the wrong TLS posture and the
/// wrong CPU governance (the #395 SIGKILL shape).
pub struct SidecarSpawn<'a> {
    /// The egress-proxy binary.
    pub binary: &'a Path,
    /// `host:port` entries this sidecar may dial.
    pub allowlist: &'a [String],
    /// Per-worker scratch dir; the UDS and the exported CA live here.
    pub scratch: &'a Path,
    /// Worker name, for the proxy's decision rows.
    pub worker: &'a str,
    /// SPKI pin JSON (slice #4). Passed opaque; the proxy parses + enforces.
    pub cert_pins_json: Option<&'a str>,
    /// TLS posture — see [`Mitm`].
    pub mitm: Mitm<'a>,
    /// Lifetime-scoped CPU governance (issue #395). `true` for a channel
    /// sidecar that outlives many dispatches (no cumulative `RLIMIT_CPU`, which
    /// would eventually SIGKILL it mid-flight); `false` for a per-tool-call
    /// sidecar, which keeps the bounded cap as defense-in-depth.
    pub long_lived: bool,
}

/// Build the sandbox policy for the proxy: `Net::ProxyEgress` (real outbound +
/// DNS, self-enforcing), `WorkerNetClient` (permits `socket(2)`), fs_read for
/// the DNS resolver files + the binary, fs_write for the scratch dir (to create
/// the UDS), and the env contract.
///
/// `spec.long_lived` selects the CPU governance (issue #395). A channel sidecar
/// (matrix) lives 1:1 with a worker that runs for weeks, so a cumulative
/// `RLIMIT_CPU` would eventually SIGKILL it mid-flight → `cpu_ms: 0` (no cap;
/// bounded instead by the cgroup `CPUQuota` on Linux / the mem cap). A
/// short-lived per-tool-call sidecar (web-fetch) lives only for the one
/// dispatch, so it gets a bounded [`SHORT_LIVED_SIDECAR_CPU_MS`] cap back —
/// restoring the defense-in-depth that only mattered on macOS, where
/// `RLIMIT_CPU` is the sole per-process CPU-governance primitive.
///
/// This function is a pure descriptor builder and validates nothing: the
/// precondition on `upstream_extra_ca` (must be absolute) is enforced by
/// [`check_upstream_extra_ca`], which [`spawn_sidecar`] calls before it builds
/// the policy. Callers reaching for `proxy_policy` directly must uphold it
/// themselves.
pub fn proxy_policy(spec: &SidecarSpawn<'_>) -> SandboxPolicy {
    let uds = spec.scratch.join(UDS_FILE_NAME);
    let allow_json = serde_json::to_string(spec.allowlist).expect("Vec<String> serializes");
    let mut env = vec![
        (ENV_UDS.to_string(), uds.to_string_lossy().into_owned()),
        (ENV_ALLOWLIST.to_string(), allow_json),
        (ENV_WORKER.to_string(), spec.worker.to_string()),
    ];
    // Pins are static operator config (slice #4). Omit the key entirely when
    // absent so the no-pin path is byte-identical to slice #3b.
    if let Some(pins) = spec.cert_pins_json.filter(|s| !s.trim().is_empty()) {
        env.push((ENV_PINS.to_string(), pins.to_string()));
    }
    // Omit the disable-MITM key entirely when intercepting so the MITM path is
    // byte-identical to the pre-#494 default (mirrors the pins pattern).
    if spec.mitm.is_transparent() {
        env.push((ENV_DISABLE_MITM.to_string(), "1".to_string()));
    }
    // Operator-provided extra CA for the re-origination leg (#491). Omit the key
    // entirely when absent so the no-extra-CA path is byte-identical. The env
    // value is `to_string_lossy` (as ENV_UDS above is) while the fs_read bind
    // below keeps the exact bytes, so a non-UTF-8 path would disagree — the
    // proxy then can't open the mangled path and startup fails closed.
    if let Some(ca) = spec.mitm.upstream_extra_ca() {
        env.push((ENV_UPSTREAM_EXTRA_CA.to_string(), ca.to_string_lossy().into_owned()));
    }
    let mut fs_read = vec![
        spec.binary.to_path_buf(),
        PathBuf::from("/etc/resolv.conf"),
        PathBuf::from("/etc/hosts"),
        PathBuf::from("/etc/nsswitch.conf"),
    ];
    // The proxy reads the extra CA at startup (before lock_down); it must be
    // bound into the jail's fs_read to be openable.
    if let Some(ca) = spec.mitm.upstream_extra_ca() {
        fs_read.push(ca.to_path_buf());
    }
    SandboxPolicy {
        fs_read,
        fs_write: vec![spec.scratch.to_path_buf()],
        net: Net::ProxyEgress,
        // CPU governance is lifetime-scoped (issue #395). A long-lived channel
        // sidecar (matrix, weeks) gets no cumulative RLIMIT_CPU — same
        // convention as `build_matrix_policy` — because the historical `10_000`
        // WOULD have SIGKILLed it mid-flight once the spawn fix below made the
        // lockdown env actually reach the proxy (it never did before `e70174b`).
        // A short-lived per-tool-call sidecar lives only for its one dispatch,
        // so it keeps the bounded cap as defense-in-depth (the only CPU primitive
        // on macOS, where there is no cgroup quota).
        cpu_ms: if spec.long_lived { 0 } else { SHORT_LIVED_SIDECAR_CPU_MS },
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env,
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    }
}

/// Reject an anchor that cannot do what the caller intends, before anything is
/// spawned: a **relative** path. The CA is bound into the proxy jail via
/// `SandboxPolicy.fs_read`, and both backends reject relative `fs_read` entries
/// — so the failure would name the sandbox rather than the misconfigured field.
/// (A *nonexistent* absolute path is deliberately NOT rejected: `canonicalize_one`
/// tolerates `NotFound` and the Linux bind is `--ro-bind-try`, leaving the proxy
/// — the authority on the PEM's content — to fail closed on it at startup.)
///
/// The old second rule ("never paired with a transparent tunnel") is gone
/// because [`Mitm`] makes that pair unrepresentable.
fn check_upstream_extra_ca(mitm: Mitm<'_>) -> anyhow::Result<()> {
    let Some(ca) = mitm.upstream_extra_ca() else {
        return Ok(());
    };
    if !ca.is_absolute() {
        anyhow::bail!(
            "upstream extra CA path must be absolute (it is bound into the proxy jail via \
             fs_read, which rejects relative paths): {ca:?}"
        );
    }
    Ok(())
}

/// One-line stderr summary for a sidecar bring-up failure, waiting up to
/// [`TAIL_DRAIN_WAIT`] for the drain thread to reach EOF.
///
/// The proxy's fail-closed startup aborts (a malformed pin set, an unreadable
/// upstream extra CA) are reported only as `Err` out of `main`, i.e. on its
/// stderr — a pipe the host drains to `debug` and nothing else reads. Without
/// folding it into the error, a mistyped operator CA path surfaces as a bare
/// readiness timeout that blames the UDS bind.
///
/// # This was the third renderer of the same tail ([#746])
///
/// It had both of the defects #732 removed from the other two:
///
/// 1. **It waited for non-empty, not for EOF.** It never consulted
///    `wait_for_drain`, so an empty result after the settle period was
///    ambiguous in exactly the #732 way — and it rendered as `no stderr
///    captured`, a claim about the *proxy*, when the only thing established
///    was a fact about *us*. A proxy that fails closed on a malformed pin set
///    a few milliseconds late got the operator `no stderr captured` plus a
///    bare readiness timeout blaming the UDS bind — precisely what this
///    function exists to prevent.
/// 2. **It collapsed the `None` arm into the same string.** "our spawn path
///    piped no stderr" and "the proxy was silent" point a reader at different
///    places; `tool_worker.rs` has an explicit test forbidding that collapse.
///
/// Both call sites reach this after the child is **dead** — one observed its
/// exit via `try_wait`, the other killed and reaped it — so the pipe really
/// does close and the wait is a cap rather than a cost.
///
/// [#746]: https://github.com/hherb/kastellan/issues/746
fn stderr_note(tail: Option<&crate::worker_stderr::StderrTail>) -> String {
    let Some(tail) = tail else {
        // NOT "no stderr captured": that is a claim about the proxy, and this
        // is a fact about our own spawn path.
        return "its stderr was not piped, so there is nothing to report".to_string();
    };
    render_stderr_note(&crate::worker_stderr::collect_tail_after_drain(tail))
}

/// Pure: the sidecar bring-up note for one captured tail.
///
/// Separated from the wait so every state can be unit-tested without a child
/// process — the same reason `format_worker_failure_report` is pure. Matches
/// [`TailState`] exhaustively, so a state added to the system cannot silently
/// inherit whichever arm happens to be last.
fn render_stderr_note(tail: &crate::worker_stderr::CapturedTail) -> String {
    let ms = TAIL_DRAIN_WAIT.as_millis();
    match tail.state() {
        // The one state that licenses a claim about the PROXY.
        TailState::KnownSilent => "no stderr captured".to_string(),
        TailState::LastWords(lines) => format!("recent stderr: {}", lines.join(" | ")),
        TailState::NothingCapturedYet => format!(
            "its stderr drain did not reach EOF within {ms} ms, so nothing was captured YET \
             — a PARTIAL capture, not evidence that the proxy was silent"
        ),
        TailState::FirstWords(lines) => format!(
            "stderr so far (drain incomplete after {ms} ms, so these may be its FIRST lines \
             rather than its most recent): {}",
            lines.join(" | ")
        ),
        // #747: our read of the pipe failed, so the emptiness is ours.
        TailState::DrainFailedSilent => {
            "our read of its stderr FAILED before anything arrived, so nothing was captured \
             — our pipe, not evidence that the proxy was silent"
                .to_string()
        }
        TailState::DrainFailedWords(lines) => format!(
            "stderr up to the point our read of the pipe FAILED (so these may be its FIRST \
             lines rather than its most recent, and there may have been more we could never \
             read): {}",
            lines.join(" | ")
        ),
    }
}

/// Spawn the proxy under `backend` and wait (bounded) for its UDS to appear.
/// Fail-closed: returns `Err` on a failed precondition
/// ([`check_upstream_extra_ca`]), on spawn failure, on the proxy exiting before
/// it is ready, or on the readiness timeout.
///
/// `spec.long_lived` scopes the sidecar's CPU cap — see [`proxy_policy`]. Pass
/// `true` for a channel sidecar that outlives many dispatches (matrix), `false`
/// for a per-tool-call sidecar (web-fetch) so it gets a bounded `RLIMIT_CPU`
/// back.
pub fn spawn_sidecar(
    backend: &dyn SandboxBackend,
    spec: &SidecarSpawn<'_>,
) -> anyhow::Result<SidecarHandle> {
    check_upstream_extra_ca(spec.mitm)?;
    if let Some(ca) = spec.mitm.upstream_extra_ca() {
        // Operator-visible record that this sidecar's upstream trust is wider
        // than webpki. The proxy logs its own WARN, but only to its stderr —
        // drained to `debug` below — so without this the daemon log would never
        // carry the fact at a level an operator reads.
        tracing::warn!(
            worker = spec.worker,
            extra_ca = %ca.display(),
            "egress sidecar trusts an operator-provided upstream extra CA on its re-origination \
             leg (widens trust beyond webpki for EVERY host this sidecar may reach)"
        );
    }
    let policy = proxy_policy(spec);
    let uds_path = spec.scratch.join(UDS_FILE_NAME);
    // The scratch dir is minted exclusively per spawn (audit 2026-09-02, S1),
    // so nothing may already be at the socket or CA path. The readiness loop
    // below waits for both files to EXIST — a planted pair would have made it
    // return `Ok` while the real proxy died on `bind()`, coupling the worker
    // to an impostor. Refuse instead of removing.
    let ca_path_pre = spec.scratch.join(CA_FILE_NAME);
    if uds_path.symlink_metadata().is_ok() || ca_path_pre.symlink_metadata().is_ok() {
        anyhow::bail!(
            "egress sidecar scratch {:?} already holds {UDS_FILE_NAME} or {CA_FILE_NAME} before \
             the proxy was spawned — refusing to trust a pre-existing socket/CA",
            spec.scratch
        );
    }

    // Derive the worker-side lockdown env (KASTELLAN_SECCOMP_PROFILE +
    // KASTELLAN_LANDLOCK_RW/RO) exactly like every other spawn path. Without
    // it the proxy's in-process lock_down ran with NO seccomp and — worse — a
    // Landlock ruleset missing the fs_read grants, so post-lockdown glibc
    // could not open /etc/resolv.conf|hosts|nsswitch.conf and EVERY
    // DNS-needing CONNECT failed EAI_AGAIN ("Temporary failure in name
    // resolution") on Linux. Literal-IP tunnels never resolve, which is why
    // the hermetic suites stayed green while real-hostname egress was broken.
    let derived = crate::tool_host::derive_lockdown_env(&policy);
    let program = spec.binary.to_string_lossy();
    let mut child = backend
        .spawn_under_policy(&derived, &program, &[])
        .map_err(|e| anyhow::anyhow!("spawn egress-proxy sidecar: {e}"))?;

    // Drain the sidecar's piped stderr on a detached thread — same reason
    // `tool_host::spawn_worker` drains a worker's: the backends pipe stderr but
    // nothing here reads it, so a proxy writing past the ~64 KiB pipe buffer
    // would block on write. The tail-retaining variant additionally lets a
    // bring-up failure below report the proxy's OWN account of what went wrong.
    let pid = child.id();
    let stderr_tail = child
        .stderr
        .take()
        .map(|stderr| crate::worker_stderr::spawn_drain_with_tail(pid, stderr));

    // Slice #3a: the sidecar also exports its per-instance MITM CA next to the
    // UDS. Wait for BOTH so the host never binds a worker before the CA it must
    // trust exists on disk.
    let ca_path = spec.scratch.join(CA_FILE_NAME);
    let deadline = Instant::now() + READY_TIMEOUT;
    while !(uds_path.exists() && ca_path.exists()) {
        // The proxy fails CLOSED on bad operator config (a malformed pin set, an
        // unreadable/certless upstream extra CA) by aborting at startup. Catch
        // that exit as soon as it happens: otherwise it only ever surfaces as the
        // READY_TIMEOUT below — a five-second wait whose message blames the UDS
        // bind rather than the config that actually failed.
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "egress-proxy sidecar exited before becoming ready ({status}); {}",
                stderr_note(stderr_tail.as_ref())
            );
        }
        if Instant::now() >= deadline {
            let mut handle = SidecarHandle { child, uds_path: uds_path.clone() };
            handle.child.kill().ok();
            handle.child.wait().ok();
            anyhow::bail!(
                "egress-proxy sidecar did not bind {uds_path:?} + write {ca_path:?} within \
                 {READY_TIMEOUT:?}; {}",
                stderr_note(stderr_tail.as_ref())
            );
        }
        std::thread::sleep(READY_POLL);
    }
    Ok(SidecarHandle { child, uds_path })
}

#[cfg(test)]
mod tests;
