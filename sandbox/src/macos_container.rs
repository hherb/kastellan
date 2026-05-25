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
/// `HHAGENT_CPU_MS` — the same code path the existing Linux + macOS
/// workers already use. The `core::tool_host::derive_lockdown_env` helper
/// sets `HHAGENT_CPU_MS` on the worker's env before it's passed here.
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
        Net::Allowlist(_) => {
            // The allowlist itself is enforced by the future egress proxy
            // worker (see docs/architecture.md invariant 5), not by
            // `container` — same split as bwrap's `--share-net`.
            argv.push("--network".into());
            argv.push("default".into());
        }
    }

    // Profile-driven hardening flags. Both presets drop all capabilities
    // and run as a low-priv user; only `WorkerStrict` makes the root FS
    // read-only.
    match policy.profile {
        Profile::WorkerStrict => {
            argv.push("--read-only".into());
            argv.push("--cap-drop".into());
            argv.push("ALL".into());
            argv.push("--user".into());
            argv.push("nobody".into());
            argv.push("--tmpfs".into());
            argv.push("/tmp".into());
        }
        Profile::WorkerNetClient => {
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
mod tests {
    use super::*;
    use crate::{Net, Profile};
    use std::path::PathBuf;

    // ---------- clamp_memory_to_minimum ----------

    #[test]
    fn clamp_raises_one_mib_to_floor_and_flags_clamping() {
        let out = clamp_memory_to_minimum(1);
        assert_eq!(
            out,
            ClampedMemory {
                mib: CONTAINER_MEM_MIN_MIB,
                clamped: true,
            }
        );
    }

    #[test]
    fn clamp_raises_one_hundred_mib_to_floor_and_flags_clamping() {
        let out = clamp_memory_to_minimum(100);
        assert_eq!(
            out,
            ClampedMemory {
                mib: 200,
                clamped: true,
            }
        );
    }

    #[test]
    fn clamp_passes_through_two_fifty_six_mib_without_clamping() {
        let out = clamp_memory_to_minimum(256);
        assert_eq!(
            out,
            ClampedMemory {
                mib: 256,
                clamped: false,
            }
        );
    }

    #[test]
    fn clamp_passes_through_one_gib_without_clamping() {
        let out = clamp_memory_to_minimum(1024);
        assert_eq!(
            out,
            ClampedMemory {
                mib: 1024,
                clamped: false,
            }
        );
    }

    /// Direct call with `0` clamps to the floor and flags clamping. The
    /// only in-tree callsite guards `mem_mb > 0` before calling, so this
    /// path is unreachable from `build_container_argv` today — pinned to
    /// match the docstring's documented behaviour for any future direct
    /// caller (e.g. a different backend reusing the helper).
    #[test]
    fn clamp_zero_raises_to_floor_and_flags_clamping() {
        let out = clamp_memory_to_minimum(0);
        assert_eq!(
            out,
            ClampedMemory {
                mib: CONTAINER_MEM_MIN_MIB,
                clamped: true,
            }
        );
    }

    /// Exact-floor input is NOT clamped (the boundary is inclusive on the
    /// "above" side). Pinned so a future "fix" to `<=` doesn't silently
    /// log every container spawn at the floor.
    #[test]
    fn clamp_at_exact_floor_does_not_flag_clamping() {
        let out = clamp_memory_to_minimum(CONTAINER_MEM_MIN_MIB);
        assert_eq!(
            out,
            ClampedMemory {
                mib: CONTAINER_MEM_MIN_MIB,
                clamped: false,
            }
        );
    }

    // ---------- build_container_argv ----------

    fn strict_policy() -> SandboxPolicy {
        SandboxPolicy::default()
    }

    fn netclient_policy() -> SandboxPolicy {
        SandboxPolicy {
            profile: Profile::WorkerNetClient,
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            ..SandboxPolicy::default()
        }
    }

    #[test]
    fn argv_starts_with_container_run() {
        let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/echo", &["hi"]);
        assert_eq!(argv[0], "container");
        assert_eq!(argv[1], "run");
    }

    /// Always-on flags must appear regardless of policy: `--rm` (auto-remove),
    /// `-i` (stdin open for JSON-RPC), `--init` (signal-forwarding + zombie-reap),
    /// `--progress none` (suppress noisy stderr progress lines).
    #[test]
    fn argv_always_carries_rm_and_interactive_and_init_and_progress_none() {
        let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
        assert!(argv.contains(&"--rm".to_string()), "missing --rm; got: {argv:?}");
        assert!(argv.contains(&"-i".to_string()), "missing -i; got: {argv:?}");
        assert!(argv.contains(&"--init".to_string()), "missing --init; got: {argv:?}");
        // --progress none must appear as adjacent argv elements (not just both present somewhere).
        let progress_idx = argv
            .iter()
            .position(|s| s == "--progress")
            .expect("missing --progress");
        assert_eq!(
            argv[progress_idx + 1],
            "none",
            "--progress not followed by `none`; got: {argv:?}"
        );
    }

    /// `--init` must appear in every container run argv: it forwards
    /// signals (so the lifecycle manager's outer-process kill reaches the
    /// in-VM worker) and reaps zombies (Python's multiprocessing fork). The
    /// flag is parallel to LinuxBwrap's unconditional `--as-pid-1` posture.
    /// Pinned by issue #107.
    #[test]
    fn argv_carries_init_for_signal_forwarding_and_zombie_reaping() {
        let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
        assert!(
            argv.contains(&"--init".to_string()),
            "missing --init; got: {argv:?}"
        );
    }

    /// `Net::Deny` must emit `--network none`. Explicit on both arms so a
    /// future change to container's default doesn't silently re-open the
    /// network on Deny policies.
    #[test]
    fn net_deny_emits_network_none() {
        let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
        let net_idx = argv
            .iter()
            .position(|s| s == "--network")
            .expect("missing --network");
        assert_eq!(argv[net_idx + 1], "none");
    }

    #[test]
    fn net_allowlist_emits_network_default() {
        let argv = build_container_argv(&netclient_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
        let net_idx = argv
            .iter()
            .position(|s| s == "--network")
            .expect("missing --network");
        assert_eq!(argv[net_idx + 1], "default");
    }

    /// WorkerStrict adds `--read-only` (root FS RO), `--cap-drop ALL`,
    /// `--user nobody`, and `--tmpfs /tmp` (so processes have a writable
    /// scratch despite --read-only).
    #[test]
    fn strict_profile_adds_readonly_capdrop_user_nobody_and_tmpfs() {
        let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
        assert!(argv.contains(&"--read-only".to_string()), "got: {argv:?}");
        let cap_idx = argv
            .iter()
            .position(|s| s == "--cap-drop")
            .expect("missing --cap-drop");
        assert_eq!(argv[cap_idx + 1], "ALL");
        let user_idx = argv
            .iter()
            .position(|s| s == "--user")
            .expect("missing --user");
        assert_eq!(argv[user_idx + 1], "nobody");
        let tmpfs_idx = argv
            .iter()
            .position(|s| s == "--tmpfs")
            .expect("missing --tmpfs");
        assert_eq!(argv[tmpfs_idx + 1], "/tmp");
    }

    /// WorkerNetClient is like Strict but WITHOUT `--read-only` (workers in
    /// this profile may need to write outside /tmp). cap-drop, user nobody,
    /// and /tmp tmpfs still apply.
    #[test]
    fn netclient_profile_drops_readonly_but_keeps_capdrop_user_and_tmpfs() {
        let argv = build_container_argv(&netclient_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
        assert!(
            !argv.contains(&"--read-only".to_string()),
            "NetClient must not be --read-only; got: {argv:?}"
        );
        assert!(argv.contains(&"--cap-drop".to_string()));
        let user_idx = argv
            .iter()
            .position(|s| s == "--user")
            .expect("missing --user");
        assert_eq!(argv[user_idx + 1], "nobody");
        assert!(argv.contains(&"--tmpfs".to_string()));
    }

    /// `mem_mb == 0` means "unset"; the `-m` flag is dropped entirely (let
    /// container's host default win). Pinned so a future regression doesn't
    /// silently emit `-m 0M` (which container rejects).
    #[test]
    fn mem_mb_zero_drops_m_flag_entirely() {
        let mut p = strict_policy();
        p.mem_mb = 0;
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        assert!(
            !argv.iter().any(|s| s == "-m"),
            "mem_mb=0 must drop -m; got: {argv:?}"
        );
    }

    /// Non-zero `mem_mb` below the floor emits `-m 200M` (clamped). The
    /// `tracing::warn!` is emitted by the build function; we can't observe
    /// it from the test (no `tracing-test` dep) but the argv is the
    /// load-bearing assertion.
    #[test]
    fn mem_mb_below_floor_emits_clamped_two_hundred_megabyte() {
        let mut p = strict_policy();
        p.mem_mb = 64;
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        let m_idx = argv.iter().position(|s| s == "-m").expect("missing -m");
        assert_eq!(argv[m_idx + 1], "200M");
    }

    #[test]
    fn mem_mb_above_floor_passes_through() {
        let mut p = strict_policy();
        p.mem_mb = 1024;
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        let m_idx = argv.iter().position(|s| s == "-m").expect("missing -m");
        assert_eq!(argv[m_idx + 1], "1024M");
    }

    /// `cpu_quota_pct: None` does not emit `-c` (let container's
    /// `--default-cpus` win). Pinned to prevent a future default-200% drift
    /// that would diverge from `linux_cgroup`'s posture without an
    /// explicit decision.
    #[test]
    fn cpu_quota_pct_none_drops_c_flag_entirely() {
        let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
        assert!(
            !argv.iter().any(|s| s == "-c"),
            "cpu_quota_pct=None must drop -c; got: {argv:?}"
        );
    }

    #[test]
    fn cpu_quota_pct_two_hundred_emits_two_fractional_vcpus() {
        let mut p = strict_policy();
        p.cpu_quota_pct = Some(200);
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        let c_idx = argv.iter().position(|s| s == "-c").expect("missing -c");
        assert_eq!(argv[c_idx + 1], "2");
    }

    #[test]
    fn cpu_quota_pct_fractional_emits_decimal_vcpus() {
        let mut p = strict_policy();
        p.cpu_quota_pct = Some(150);
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        let c_idx = argv.iter().position(|s| s == "-c").expect("missing -c");
        assert_eq!(argv[c_idx + 1], "1.5");
    }

    /// `Some(0)` is treated as "unset" — drops `-c` entirely rather than
    /// emitting `-c 0` (which `container` rejects with an opaque error).
    /// Mirrors the `mem_mb == 0` drop-the-flag posture.
    #[test]
    fn cpu_quota_pct_zero_drops_c_flag_entirely() {
        let mut p = strict_policy();
        p.cpu_quota_pct = Some(0);
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        assert!(
            !argv.iter().any(|s| s == "-c"),
            "cpu_quota_pct=Some(0) must drop -c; got: {argv:?}"
        );
    }

    #[test]
    fn tasks_max_none_drops_ulimit_flag_entirely() {
        let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
        assert!(
            !argv.iter().any(|s| s == "--ulimit"),
            "tasks_max=None must drop --ulimit; got: {argv:?}"
        );
    }

    #[test]
    fn tasks_max_emits_ulimit_nproc_with_soft_eq_hard() {
        let mut p = strict_policy();
        p.tasks_max = Some(64);
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        let u_idx = argv
            .iter()
            .position(|s| s == "--ulimit")
            .expect("missing --ulimit");
        assert_eq!(argv[u_idx + 1], "nproc=64:64");
    }

    #[test]
    fn fs_read_emits_readonly_bind_mount_per_path() {
        let mut p = strict_policy();
        p.fs_read = vec![PathBuf::from("/etc/ssl"), PathBuf::from("/opt/data")];
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        let joined = argv.join(" ");
        assert!(
            joined.contains("--mount type=bind,source=/etc/ssl,target=/etc/ssl,readonly"),
            "got: {argv:?}"
        );
        assert!(
            joined.contains("--mount type=bind,source=/opt/data,target=/opt/data,readonly"),
            "got: {argv:?}"
        );
    }

    #[test]
    fn fs_write_emits_writable_bind_mount_per_path() {
        let mut p = strict_policy();
        p.fs_write = vec![PathBuf::from("/var/lib/hhagent/scratch")];
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        let joined = argv.join(" ");
        assert!(
            joined.contains("--mount type=bind,source=/var/lib/hhagent/scratch,target=/var/lib/hhagent/scratch"),
            "got: {argv:?}"
        );
        // The fs_write path must NOT emit a separate `,readonly` mount.
        assert!(
            !joined.contains("type=bind,source=/var/lib/hhagent/scratch,target=/var/lib/hhagent/scratch,readonly"),
            "fs_write path was emitted as readonly; got: {argv:?}"
        );
    }

    #[test]
    fn env_entries_emit_dash_e_kv() {
        let mut p = strict_policy();
        p.env = vec![
            ("FOO".into(), "bar".into()),
            ("HHAGENT_CPU_MS".into(), "5000".into()),
        ];
        let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
        // -e flags appear as adjacent pairs; locate by value.
        for needle in &["FOO=bar", "HHAGENT_CPU_MS=5000"] {
            let i = argv
                .iter()
                .position(|s| s == needle)
                .unwrap_or_else(|| panic!("missing {needle:?} in {argv:?}"));
            assert_eq!(argv[i - 1], "-e");
        }
    }

    /// Image must appear in the argv exactly once, before `program` and any
    /// `args`. Pinned to prevent a refactor from forgetting the image
    /// (silently making `container run` use its default).
    #[test]
    fn image_appears_before_program_and_args() {
        let argv = build_container_argv(
            &strict_policy(),
            "alpine:3.20",
            "/bin/echo",
            &["hello", "world"],
        );
        let img_idx = argv
            .iter()
            .position(|s| s == "alpine:3.20")
            .expect("missing image");
        let prog_idx = argv
            .iter()
            .position(|s| s == "/bin/echo")
            .expect("missing program");
        let arg_idx = argv
            .iter()
            .position(|s| s == "hello")
            .expect("missing first arg");
        assert!(
            img_idx < prog_idx && prog_idx < arg_idx,
            "expected image < program < args order; got: {argv:?}"
        );
        assert_eq!(argv[prog_idx + 1], "hello");
        assert_eq!(argv[prog_idx + 2], "world");
        // Image must appear exactly once.
        let img_count = argv.iter().filter(|s| s.as_str() == "alpine:3.20").count();
        assert_eq!(img_count, 1, "image emitted more than once; got: {argv:?}");
    }

    #[test]
    fn relative_policy_paths_are_rejected_by_spawn() {
        let backend = MacosContainer::new();
        let mut p = strict_policy();
        p.fs_read.push(PathBuf::from("relative/path"));
        let err = backend
            .spawn_under_policy(&p, "/bin/true", &[])
            .expect_err("must reject relative paths");
        let msg = format!("{err}");
        assert!(
            msg.contains("must be absolute"),
            "expected 'must be absolute' error, got: {msg}"
        );
    }

    /// `MacosContainer::with_image` overrides the default. Pinned so a
    /// refactor that drops the constructor would trip immediately rather
    /// than silently using the hard-coded `alpine:3.20`.
    #[test]
    fn with_image_overrides_default() {
        let backend = MacosContainer::with_image("ghcr.io/example/worker:dev");
        assert_eq!(backend.image(), "ghcr.io/example/worker:dev");
    }

    #[test]
    fn default_constructor_uses_default_image() {
        let backend = MacosContainer::new();
        assert_eq!(backend.image(), DEFAULT_IMAGE);
    }

    // ---------- build_image_inspect_argv (issue #120) ----------

    /// Pins the exact argv shape: `["container", "image", "inspect", <tag>]`.
    /// Any change here is operator-visible (it changes the subprocess we
    /// spawn), so a deliberate test update is the right friction.
    #[test]
    fn build_image_inspect_argv_shape() {
        let argv = build_image_inspect_argv("hhagent/gliner-relex:dev");
        assert_eq!(
            argv,
            vec!["container", "image", "inspect", "hhagent/gliner-relex:dev"]
        );
    }

    /// Tag is passed verbatim — no quoting, no escaping, no munging. The
    /// shell-injection footgun is structurally impossible because we use
    /// `Command::args(...)` (not a shell), but pinning the verbatim pass
    /// keeps the contract obvious.
    #[test]
    fn build_image_inspect_argv_passes_tag_verbatim() {
        for tag in [
            "alpine:3.20",
            "ghcr.io/foo/bar:v1.2.3",
            "tag-with-dashes",
            "tag_with_underscores",
            "registry.example.com:5000/myimg:dev",
        ] {
            let argv = build_image_inspect_argv(tag);
            assert_eq!(argv[3], tag, "tag mangled for input {tag:?}");
            assert_eq!(argv.len(), 4, "argv length drifted for input {tag:?}");
        }
    }

    /// Empty tag still produces a 4-element argv (the subprocess will fail
    /// loudly at the binary level, which is the right place for that
    /// diagnostic — not here).
    #[test]
    fn build_image_inspect_argv_empty_tag_is_passthrough() {
        let argv = build_image_inspect_argv("");
        assert_eq!(argv, vec!["container", "image", "inspect", ""]);
    }

    // ---------- probe_image guards (post-review fixup) ----------

    /// `probe_image("")` short-circuits before spawning with an operator-
    /// actionable diagnostic. The pure argv builder still passes empty
    /// strings through (`build_image_inspect_argv_empty_tag_is_passthrough`);
    /// the spawn-path is where the guard belongs because that's where
    /// the failure mode is operator-visible.
    #[test]
    fn probe_image_rejects_empty_tag_upfront() {
        let err =
            MacosContainer::probe_image("").expect_err("empty tag must error before spawn");
        let msg = format!("{err}");
        assert!(
            msg.contains("empty image_tag"),
            "expected empty-tag diagnostic, got: {msg}"
        );
        // Hint at the correct fix — caller should use None rather than
        // Some(""). The exact wording is allowed to drift; we just
        // pin the operator-actionable cue.
        assert!(
            msg.contains("None"),
            "expected None-fallback hint in diagnostic, got: {msg}"
        );
    }
}
