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

use crate::bounded_command;
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
        Profile::WorkerNetClient | Profile::WorkerBrowserClient | Profile::WorkerMatrixClient => {
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

    // The guest kernel has no Landlock, so state the exception — as a DEFAULT
    // THAT NEVER OVERRIDES A CALLER. Exactly #669's remedy for the Firecracker
    // guest kernel, and found the same way: by a freshness gate (#687) that
    // made a stale fixture stop certifying a run.
    //
    // ⚠️ MEASURED on Apple `container` 1.1.0 / guest kernel 6.18.15, not
    // assumed: `/sys/kernel/security/lsm` does not exist in the guest, and a
    // worker reaching its own lockdown dies with "landlock: Landlock ruleset
    // is not enforced by this kernel". The whole macOS container tier had been
    // dead since the 2026-09-02 audit made that fail-closed; the only reason
    // nobody saw it is that the image on the dev Mac predated the audit.
    //
    // ⚠️ Seccomp is UNAFFECTED, and that is what the justification rests on —
    // also measured, not argued: the same hand-run worker reports
    // `lockdown Linux { landlock: Disabled, seccomp: Installed, .. }`.
    //
    // Delete this once an Apple `container` guest kernel ships Landlock; a
    // caller can already opt back in today, since this only fills the key in
    // when the policy has not chosen.
    //
    // ⚠️ Something DOES now say when that day arrives (#689):
    // `macos_container_smoke::the_guest_kernel_still_has_no_landlock_so_the_opt_out_still_applies`
    // boots a real container and reads `crate::GUEST_LSM_LIST_PATH`. It matters
    // that the reminder is automatic here specifically: the Firecracker twin's
    // guest kernel is a sha256 pin this repo controls, and only moves on a
    // deliberate edit — Apple's moves on a routine `brew upgrade container`,
    // outside anybody's decision. The tier with no detector was the one whose
    // assumption could expire on its own.
    if !policy
        .env
        .iter()
        .any(|(k, _)| k == crate::LANDLOCK_PROFILE_ENV)
    {
        // Say it out loud, once per spawn. `tool_host::warn_lockdown_overrides`
        // exists so a sandbox-disabling env is never silent, but it inspects
        // the DERIVED POLICY before a backend runs — and this injection happens
        // inside the backend, so that detector is structurally blind to it.
        // The Firecracker plan carries the identical warning for the identical
        // reason.
        tracing::warn!(
            worker_env = crate::LANDLOCK_PROFILE_ENV,
            "Apple `container` guest kernel does not enforce Landlock, so this worker runs \
             with the worker-side FS layer DISABLED inside the guest; seccomp is unaffected"
        );
        argv.push("-e".into());
        argv.push(format!(
            "{}={}",
            crate::LANDLOCK_PROFILE_ENV,
            crate::LANDLOCK_PROFILE_NONE
        ));
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
/// images, which is the load-bearing signal for
/// [`MacosContainer::probe_image`], which ignores stdout.
///
/// ⚠️ **A second caller now reads that stdout.** The macOS container
/// freshness gate (`kastellan_tests_common::microvm::container`, #687)
/// parses the image-manifest JSON for `configuration.creationDate` and
/// compares it against source mtimes. So the JSON is no longer
/// "irrelevant": changing this argv to suppress or reformat stdout
/// would silently disable that gate.
pub fn build_image_inspect_argv(image_tag: &str) -> Vec<String> {
    vec![
        "container".into(),
        "image".into(),
        "inspect".into(),
        image_tag.into(),
    ]
}

/// The remedy for an Apple `container` call that never answered.
///
/// Apple `container` is **daemon-backed**: the CLI talks XPC to
/// `container-apiserver`. A wedged or half-started apiserver makes these calls
/// *block* rather than return an error, which is a different fault from "the
/// service is stopped" and needs a different sentence — folding the two would
/// re-create the #684 defect that sent an operator to a build for a problem no
/// build can fix. Shared with the test-side preflight so both spell one remedy
/// (#690).
pub const WEDGED_APISERVER_HINT: &str = "the `container-apiserver` may be wedged — try \
     `container system stop && container system start`";

/// Why an Apple `container` probe could not answer (#690).
///
/// Two causes with two different remedies, kept apart by the **type** rather
/// than by prose a caller would have to match on. This module already refuses
/// to decide anything by matching on another program's wording, and a caller
/// that has to grep a sentence to learn what to suggest is that rule broken
/// one layer up.
///
/// The distinction is not cosmetic: `container system start` is the remedy for
/// a *stopped* service and the wrong advice for a *wedged* one, which needs a
/// `stop` first. Handing an operator the weaker remedy alongside the right one
/// is the #684 folding defect in its politest form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFault {
    /// The CLI is absent, or it answered and said the host is not usable.
    /// Remedy: install it, or start the service.
    Unavailable(String),
    /// The CLI was spawned and never answered inside its budget, and was
    /// killed. Remedy: unwedge the apiserver.
    Wedged(String),
}

impl ProbeFault {
    /// The operator-facing sentence, for a caller that wants prose and has no
    /// use for the classification.
    pub fn reason(&self) -> &str {
        match self {
            Self::Unavailable(reason) | Self::Wedged(reason) => reason,
        }
    }
}

/// Run one `container` probe under [`bounded_command::PROBE_BUDGET`] (#690).
///
/// The budget is **not** a latency limit and must never fire on a slow but
/// working host — measured on the dev Mac at `container` 1.1.0, these calls
/// answer in 10–240 ms against a ten-second budget. It exists only so a wedged
/// apiserver turns into a sentence instead of a `cargo test --workspace` that
/// stalls with no `[SKIP]`, no `[WARN]`, no panic and no message at all.
///
/// The three outcomes stay apart, which is the whole point: `Ok` for a CLI
/// that answered (**including a non-zero exit** — that is an answer), a
/// wedge-worded `Err` for one that did not, and the caller's own spawn wording
/// for a `container` that is not installed. `spawn_suffix` is appended to the
/// spawn message so each call site keeps the remedy it already had.
fn container_probe_output(
    cmd: &mut Command,
    command: &str,
    spawn_suffix: &str,
) -> Result<std::process::Output, ProbeFault> {
    bounded_command::probe_output(cmd, bounded_command::PROBE_BUDGET).map_err(|failure| match failure
    {
        bounded_command::ProbeFailure::Wedged(t) => ProbeFault::Wedged(
            bounded_command::timed_out_reason(command, &t, WEDGED_APISERVER_HINT),
        ),
        bounded_command::ProbeFailure::Spawn(e) => {
            ProbeFault::Unavailable(format!("could not spawn `{command}`: {e}{spawn_suffix}"))
        }
    })
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
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let output =
            container_probe_output(&mut cmd, &format!("container image inspect {image_tag}"), "")
                .map_err(|fault| SandboxError::Backend(fault.reason().to_string()))?;
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
        Self::probe_fault().map_err(|fault| SandboxError::Backend(fault.reason().to_string()))
    }

    /// [`Self::probe`] without flattening the verdict — the `*_or_reason`
    /// sibling pattern (#653), one step further: return the fault *classified*
    /// so one caller can name a remedy that another cannot.
    ///
    /// [`Self::probe`] must keep returning [`SandboxError`] because that is
    /// what the [`SandboxBackend`] contract is written in, and a fault that
    /// has been turned into a string can only be classified again by matching
    /// on prose — which this module forbids everywhere else.
    pub fn probe_fault() -> Result<(), ProbeFault> {
        let mut version_cmd = Command::new("container");
        version_cmd.arg("--version");
        let version = container_probe_output(
            &mut version_cmd,
            "container --version",
            " (install with `brew install container`)",
        )?;
        if !version.status.success() {
            return Err(ProbeFault::Unavailable(format!(
                "`container --version` failed: {}",
                String::from_utf8_lossy(&version.stderr).trim()
            )));
        }

        let mut status_cmd = Command::new("container");
        status_cmd.args(["system", "status"]);
        let status = container_probe_output(&mut status_cmd, "container system status", "")?;
        if !status.status.success() {
            return Err(ProbeFault::Unavailable(format!(
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

        // Fail closed on force-routing (audit finding #5): a `proxy_uds`-set
        // policy means "this worker's ONLY egress is the proxy UDS". This
        // backend maps Net::Allowlist to `--network default` (NAT egress) and
        // does not bind the UDS, so it cannot honour that contract — running
        // anyway would silently give a force-routed worker full internet
        // egress, bypassing the egress proxy's SSRF/allowlist enforcement.
        // The Seatbelt backend implements the deny-all-except-UDS filter; a
        // net worker that cannot prove that on this host must route through a
        // backend that can, never through here.
        if policy.proxy_uds.is_some() {
            return Err(SandboxError::Backend(
                "MacosContainer cannot enforce force-routed egress (proxy_uds set); \
                 refuse rather than grant unrestricted NAT egress"
                    .into(),
            ));
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
