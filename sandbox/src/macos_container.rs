//! macOS micro-VM backend for [`SandboxBackend`]: shells out to the Apple
//! `container` CLI (`/opt/homebrew/bin/container`, distributed via
//! `brew install container`). Sibling to [`crate::macos_seatbelt`] — not a
//! replacement.
//!
//! What this backend gives you (Slice 1, 2026-05-21):
//!   - Linux-namespace + capability isolation inside an Apple
//!     `Virtualization.framework`-backed micro-VM (one VM per container).
//!     Closes the **memory-cap gap** that macOS Seatbelt has today: `mem_mb`
//!     is enforced via `-m <N>M` with SIGKILL on overrun (200 MiB floor — see
//!     [`clamp_memory_to_minimum`]).
//!   - Same [`SandboxPolicy`] surface as the other backends: `fs_read`,
//!     `fs_write`, `net`, `env`, `cpu_quota_pct`, `tasks_max`, `mem_mb`,
//!     `cpu_ms` all flow into `container run` flags.
//!   - Profile presets: [`crate::Profile::WorkerStrict`] adds
//!     `--read-only --cap-drop ALL --user nobody`; [`crate::Net::Deny`] adds
//!     `--network none`.
//!   - Auto-removal via `--rm`, suppressed progress output via
//!     `--progress none`, JSON-RPC-friendly stdio via `-i`.
//!
//! Sibling, not default: [`crate::default_backend`] on darwin still returns
//! [`crate::macos_seatbelt::MacosSeatbelt`] in Slice 1; the lightweight
//! Seatbelt path (<50 ms spawn) stays correct for workers that don't need a
//! memory cap. Slice 2 introduces per-worker backend selection
//! (`WorkerSpec.sandbox_backend`) so workers that need memory enforcement
//! (`gliner-relex`, future `python-exec`) opt in to this backend explicitly.
//!
//! Latency: warm spawn 0.76–0.81 s (vs Seatbelt's ~50 ms). The cost
//! amortises to ~0 ms per call inside a long-lived stdio worker
//! ([`crate::SandboxBackend`] consumers like
//! `core::worker_lifecycle::IdleTimeoutLifecycle`). For
//! `SingleUseLifecycle` workers the full 0.8 s is per-call latency — flag in
//! that worker's spec.
//!
//! Cross-platform parity context: this backend closes today's documented
//! macOS gap on [`crate::SandboxPolicy::mem_mb`]
//! (and the analogous `cpu_quota_pct` / `tasks_max` gaps). Linux already
//! enforces all three via `systemd-run --user --scope` + cgroup v2 — see
//! [`crate::linux_cgroup`].
//!
//! See [`docs/superpowers/specs/2026-05-21-macos-container-spike-notes.md`]
//! for the discovery-spike write-up that locked this design.

use std::process::{Child, Command, Stdio};

use crate::{Net, Profile, SandboxBackend, SandboxError, SandboxPolicy};

/// Apple `container` rejects `-m` values below 200 MiB with
/// `invalidArgument: minimum memory amount allowed is 200 MiB`. Anything
/// smaller in `SandboxPolicy::mem_mb` is clamped up to this floor; the
/// callsite logs a `tracing::warn!` so operators see when their policy is
/// being silently widened.
pub const CONTAINER_MEM_MIN_MIB: u64 = 200;

/// Container image used by Slice 1's smoke tests. Plain `alpine:3.20`
/// (Apache-2.0 base layers). Real workers ship their own image (see Slice
/// 2.5's `gliner-relex` Containerfile follow-up); this default exists so
/// [`MacosContainer::new`] can construct a working instance for ad-hoc
/// invocation and tests without forcing every caller through
/// [`MacosContainer::with_image`].
pub const DEFAULT_IMAGE: &str = "alpine:3.20";

/// Outcome of clamping a requested `mem_mb` to the
/// [`CONTAINER_MEM_MIN_MIB`] floor. The boolean is the "clamping fired"
/// flag the callsite uses to decide whether to emit a `tracing::warn!`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClampedMemory {
    /// The effective `mem_mb` value (always `>= CONTAINER_MEM_MIN_MIB`).
    pub mib: u64,
    /// True iff the requested value was below the floor and got raised.
    pub clamped: bool,
}

/// Clamp `requested_mib` up to [`CONTAINER_MEM_MIN_MIB`] if it is smaller,
/// returning the effective value plus a "clamping fired" flag.
///
/// Pure function: no I/O, no logging. The callsite in
/// [`build_container_argv`] is responsible for emitting `tracing::warn!`
/// when `clamped == true`, so the warning carries the request context
/// (the operator's original value) rather than appearing as a free-floating
/// log line.
///
/// `0` is treated as "unset" (the [`SandboxPolicy`] convention for
/// time-budget fields) — it's still clamped up to the floor, because
/// `container run` requires a concrete `-m` flag once we emit one.
/// Callers that want to skip the `-m` flag entirely should pass `mem_mb =
/// 0` to [`build_container_argv`], which drops the `-m` flag (see the
/// build function's docs); this helper is only invoked when an `-m` flag
/// is actually being emitted.
pub fn clamp_memory_to_minimum(requested_mib: u64) -> ClampedMemory {
    if requested_mib < CONTAINER_MEM_MIN_MIB {
        ClampedMemory {
            mib: CONTAINER_MEM_MIN_MIB,
            clamped: true,
        }
    } else {
        ClampedMemory {
            mib: requested_mib,
            clamped: false,
        }
    }
}

/// Build the `container run` argv (including the leading `container`) for
/// `program` `args` under `policy`, running inside `image`.
///
/// Pure function — no I/O, no syscalls — exposed so unit tests can assert
/// on the argv shape without spawning a process (mirrors
/// [`crate::linux_bwrap::build_argv`]).
///
/// The argv shape is:
/// ```text
/// container run --rm -i --init --progress none [<policy flags...>] <image> <program> <args...>
/// ```
///
/// Always-on flags:
/// * `--rm` — container auto-removed on exit (mirrors bwrap's stateless
///   per-spawn posture).
/// * `-i` — keep stdin open for JSON-RPC stdio (otherwise `container run`
///   closes stdin and any worker speaking JSON-RPC over stdio hangs).
/// * `--init` — Apple `container`'s init-shim; forwards signals to the
///   worker process and reaps zombies. Parallel to LinuxBwrap's
///   unconditional `--as-pid-1`. Closes issue #107.
/// * `--progress none` — suppress the `[6/6] Starting container [0s]`
///   progress lines that `container run` emits on stderr by default.
///   They don't corrupt stdout (the JSON-RPC parser only reads stdout) but
///   they interleave noisily with worker `tracing` output in test
///   captures.
///
/// Policy-driven flags (in the same order as [`crate::SandboxPolicy`]):
/// * `fs_read` paths → `--mount type=bind,source=<P>,target=<P>,readonly`
/// * `fs_write` paths → `--mount type=bind,source=<P>,target=<P>`
/// * `env` entries → `-e <key>=<value>`
/// * `net::Deny` → `--network none`
/// * `net::Allowlist` → `--network default` (the host allowlist itself is
///   enforced by the future egress proxy worker, not by `container`)
/// * `mem_mb` (non-zero) → `-m <clamped>M`
/// * `cpu_quota_pct` (`Some`) → `-c <fractional vCPUs>` (e.g. `200% →
///   -c 2.0`). Defense-in-depth: defaults are not emitted by this
///   backend (unlike `linux_cgroup` which always emits a 200% default);
///   the absence of a `-c` flag means the container picks up the host's
///   `--default-cpus` configuration.
/// * `tasks_max` (`Some`) → `--ulimit nproc=<N>:<N>`
/// * `profile::WorkerStrict` → `--read-only --cap-drop ALL --user nobody`
///   plus `--tmpfs /tmp` so processes that need a writable scratch (almost
///   all of them) can still write there
/// * `profile::WorkerNetClient` → `--cap-drop ALL --user nobody --tmpfs
///   /tmp` (no `--read-only`; the worker may need to write outside `/tmp`)
///
/// `cpu_ms` is **not** mapped to a container-side flag: POSIX
/// `RLIMIT_CPU` works inside the Linux VM unchanged via
/// `workers/prelude::rlimit::apply_from_env` reading
/// `KASTELLAN_CPU_MS` — the same code path the existing Linux + macOS
/// workers already use. The `core::tool_host::derive_lockdown_env` helper
/// sets `KASTELLAN_CPU_MS` on the worker's env before it's passed here.
pub fn build_container_argv(
    policy: &SandboxPolicy,
    image: &str,
    program: &str,
    args: &[&str],
) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(64);
    argv.push("container".into());
    argv.push("run".into());

    argv.push("--rm".into());
    argv.push("-i".into());
    // Always-on signal-forwarding + zombie-reaping init shim.
    // Parallel to LinuxBwrap's unconditional `--as-pid-1` posture. For
    // short-lived smoke containers the overhead is one extra small init
    // process (negligible); for long-lived `IdleTimeoutLifecycle`
    // workers (gliner-relex, future python-exec) this is load-bearing:
    // without it, the in-VM worker inherits PID 1 and ignores SIGTERM
    // by default. Closes issue #107.
    argv.push("--init".into());
    argv.push("--progress".into());
    argv.push("none".into());

    // --network: explicit on both arms so a future change to container's
    // default (today: default = NAT egress) doesn't silently re-open the
    // network on Net::Deny policies.
    match &policy.net {
        Net::Deny => {
            argv.push("--network".into());
            argv.push("none".into());
        }
        Net::Allowlist(_) | Net::ProxyEgress => {
            // The allowlist itself is enforced by the egress proxy worker, not
            // by `container` — same split as bwrap's `--share-net`. ProxyEgress
            // is the proxy's own policy (real netns); Allowlist is a worker's.
            argv.push("--network".into());
            argv.push("default".into());
        }
    }

    // Profile-driven hardening flags. Both presets drop all capabilities
    // and run as a low-priv user; only `WorkerStrict` makes the root FS
    // read-only.
    match policy.profile {
        // gliner-relex (WorkerMlClient) is Net::Deny like WorkerStrict, so it
        // gets the same read-only-root container hardening; the ml_client
        // seccomp widening is a Linux-only host-backend concern.
        Profile::WorkerStrict | Profile::WorkerMlClient => {
            argv.push("--read-only".into());
            argv.push("--cap-drop".into());
            argv.push("ALL".into());
            argv.push("--user".into());
            argv.push("nobody".into());
            argv.push("--tmpfs".into());
            argv.push("/tmp".into());
        }
        // Both net-capable profiles get the same container hardening (writable
        // root + tmpfs /tmp + dropped caps + low-priv user). The browser-
        // specific Seatbelt/seccomp widening is applied by the host backends,
        // not the container backend.
        Profile::WorkerNetClient | Profile::WorkerBrowserClient => {
            argv.push("--cap-drop".into());
            argv.push("ALL".into());
            argv.push("--user".into());
            argv.push("nobody".into());
            argv.push("--tmpfs".into());
            argv.push("/tmp".into());
        }
    }

    // Memory cap. `mem_mb == 0` means "unset" — drop the flag entirely
    // and let `container` fall back to its host default. Any non-zero
    // value is clamped up to the 200 MiB floor (see
    // [`clamp_memory_to_minimum`]); the callsite emits the
    // `tracing::warn!` so the operator sees the silent widening.
    if policy.mem_mb > 0 {
        let clamped = clamp_memory_to_minimum(policy.mem_mb);
        if clamped.clamped {
            tracing::warn!(
                requested = policy.mem_mb,
                clamped_to = clamped.mib,
                "container backend raised mem_mb below {CONTAINER_MEM_MIN_MIB} MiB floor",
            );
        }
        argv.push("-m".into());
        argv.push(format!("{}M", clamped.mib));
    }

    // CPU quota: percent-of-one-CPU → fractional vCPUs. 200% → -c 2.0.
    // No default emitted when None (`container` uses its host default).
    // Consistent with `mem_mb > 0` posture: `Some(0)` is treated as
    // "unset" and drops the `-c` flag. Apple `container` rejects `-c 0`
    // with an opaque error; better to fall through to the host
    // `--default-cpus` than to surface a confusing failure.
    if let Some(pct) = policy.cpu_quota_pct.filter(|&p| p > 0) {
        let vcpus = f64::from(pct) / 100.0;
        argv.push("-c".into());
        argv.push(format!("{vcpus}"));
    }

    // pids cap: --ulimit nproc=N:N. Same semantic note as the docstring
    // on `SandboxPolicy::tasks_max`: on Linux this maps to cgroup
    // `pids.max` (per-cgroup process count), but inside the Linux VM
    // `--ulimit nproc` becomes per-real-UID `RLIMIT_NPROC`. Inside a
    // one-worker container running as a single UID the practical effect
    // is similar, but the guarantees are not identical.
    if let Some(n) = policy.tasks_max {
        argv.push("--ulimit".into());
        argv.push(format!("nproc={n}:{n}"));
    }

    // Bind-mounts. fs_read is readonly; fs_write is read+write. Order is
    // fs_read first then fs_write so the argv stays stable across
    // policy-field reorderings.
    for path in &policy.fs_read {
        let s = path.display().to_string();
        argv.push("--mount".into());
        argv.push(format!("type=bind,source={s},target={s},readonly"));
    }
    for path in &policy.fs_write {
        let s = path.display().to_string();
        argv.push("--mount".into());
        argv.push(format!("type=bind,source={s},target={s}"));
    }

    // Per-policy env. `container run -e KEY=VALUE` injects each pair into
    // the container's environment. The host env is NOT inherited by
    // default (container's behaviour, not ours) — `core::tool_host`
    // pre-clears anyway via `derive_lockdown_env`.
    for (k, v) in &policy.env {
        argv.push("-e".into());
        argv.push(format!("{k}={v}"));
    }

    argv.push(image.into());
    argv.push(program.into());
    for a in args {
        argv.push((*a).into());
    }
    argv
}

/// Build the argv for `container image inspect <tag>` (issue #120).
///
/// Used by [`MacosContainer::probe_image`] to check whether a tag is
/// present in the local image store. Pure function so the argv shape
/// can be pinned by unit tests separately from the spawn.
///
/// The shape is always exactly `["container", "image", "inspect", <tag>]`
/// — no flags. `container image inspect` exits non-zero on absent
/// images, which is the load-bearing signal here; we don't read its
/// stdout (the verbose image-manifest JSON is irrelevant for a
/// presence check).
pub fn build_image_inspect_argv(image_tag: &str) -> Vec<String> {
    vec![
        "container".into(),
        "image".into(),
        "inspect".into(),
        image_tag.into(),
    ]
}

/// Shell out to Apple `container` for sandboxing. Holds the image tag the
/// container is run inside; default is [`DEFAULT_IMAGE`] (`alpine:3.20`)
/// for ad-hoc usage and Slice 1's smoke test, but per-worker callers (Slice
/// 2 onward) should use [`Self::with_image`] to pin the worker's own image.
pub struct MacosContainer {
    image: String,
}

impl Default for MacosContainer {
    fn default() -> Self {
        Self::new()
    }
}

impl MacosContainer {
    /// Construct a backend that uses [`DEFAULT_IMAGE`] (`alpine:3.20`).
    pub fn new() -> Self {
        Self {
            image: DEFAULT_IMAGE.into(),
        }
    }

    /// Construct a backend that runs containers from `image`. Slice 2 wires
    /// this into per-worker manifests (`WorkerSpec.sandbox_backend`).
    pub fn with_image(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
        }
    }

    /// Tag this backend currently uses for `container run` invocations.
    /// Exposed for test assertions and operator-facing diagnostics.
    pub fn image(&self) -> &str {
        &self.image
    }

    /// Check that a specific image tag is present in the local image store
    /// (issue #120). Returns `Ok(())` if `container image inspect <tag>`
    /// exits zero; `Err(SandboxError::Backend)` otherwise — with an
    /// operator-facing diagnostic suggesting the worker's build-image
    /// helper.
    ///
    /// Mechanism: `container image inspect <tag>` exits non-zero when the
    /// image is absent. Single targeted call — no `container image list`
    /// parsing, so the per-line substring-matching footgun (`devbox`
    /// matching `dev`) is structurally impossible here.
    ///
    /// Spawn cost: one short-lived process per call. Intended for one-shot
    /// callers (e.g. daemon-startup health checks that walk every
    /// registered `ToolEntry.container_image` once); NOT for hot paths.
    /// Per-call cost on Apple `container` 0.12.3 measures at ~30 ms when
    /// the image IS present (the absent-image error path is slightly
    /// slower).
    pub fn probe_image(image_tag: &str) -> Result<(), SandboxError> {
        // Reject empty tag up-front rather than spawning `container image
        // inspect ""` and relying on the CLI to error out with an
        // unspecified diagnostic. A `ToolEntry.container_image =
        // Some("")` is a caller bug (the resolver substitutes
        // `DEFAULT_IMAGE` only for `None`, not for `Some("")`); fail loud
        // with an operator-actionable diagnostic.
        if image_tag.is_empty() {
            return Err(SandboxError::Backend(
                "probe_image: empty image_tag (likely a misconfigured \
                 ToolEntry.container_image — use None to fall back to DEFAULT_IMAGE \
                 rather than Some(\"\"))"
                    .into(),
            ));
        }
        let argv = build_image_inspect_argv(image_tag);
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                SandboxError::Backend(format!(
                    "could not spawn `container image inspect {image_tag}`: {e}"
                ))
            })?;
        if !output.status.success() {
            return Err(SandboxError::Backend(format!(
                "image `{image_tag}` not present in local store \
                 (build it with the worker's `scripts/workers/<worker>/build-image.sh` \
                 or pull manually with `container image pull {image_tag}`): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Check that Apple `container` is installed and the system service is
    /// running. Mirrors [`crate::linux_bwrap::LinuxBwrap::probe`] and
    /// [`crate::macos_seatbelt::MacosSeatbelt::probe`] so integration tests
    /// can `[SKIP]` cleanly rather than false-fail when the platform is
    /// unavailable.
    ///
    /// Two-step check:
    /// 1. `container --version` exit 0 — proves the binary is on `$PATH`
    ///    and runs.
    /// 2. `container system status` exit 0 — proves the system service is
    ///    running (otherwise `container run` fails with `apiServerNotRunning`
    ///    on the first invocation).
    ///
    /// Fail-closed: either failure returns `Err`. The operator-facing
    /// fix is `brew install container && container system start
    /// --enable-kernel-install` (one-time).
    pub fn probe() -> Result<(), SandboxError> {
        let version = Command::new("container")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                SandboxError::Backend(format!(
                    "could not spawn `container --version`: {e} \
                     (install with `brew install container`)"
                ))
            })?;
        if !version.status.success() {
            return Err(SandboxError::Backend(format!(
                "`container --version` failed: {}",
                String::from_utf8_lossy(&version.stderr).trim()
            )));
        }

        let status = Command::new("container")
            .args(["system", "status"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                SandboxError::Backend(format!("could not spawn `container system status`: {e}"))
            })?;
        if !status.status.success() {
            return Err(SandboxError::Backend(format!(
                "`container system status` failed: {} \
                 (start the service with `container system start --enable-kernel-install`)",
                String::from_utf8_lossy(&status.stderr).trim()
            )));
        }

        Ok(())
    }
}

impl SandboxBackend for MacosContainer {
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> Result<Child, SandboxError> {
        // Same upfront-rejection posture as the other backends: relative
        // paths in fs_read/fs_write would silently produce a misconfigured
        // bind-mount (container's `--mount source=` requires absolute) so
        // surface the error in user-friendly form before spawning.
        for p in policy.fs_read.iter().chain(policy.fs_write.iter()) {
            if !p.is_absolute() {
                return Err(SandboxError::Backend(format!(
                    "policy paths must be absolute, got {p:?}"
                )));
            }
        }

        let argv = build_container_argv(policy, &self.image, program, args);

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.spawn()
            .map_err(|e| SandboxError::Backend(format!("container spawn failed: {e}")))
    }
}

#[cfg(test)]
mod tests;
