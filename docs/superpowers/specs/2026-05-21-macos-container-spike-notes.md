# Apple `container` micro-VM backend — discovery spike notes

**Date:** 2026-05-21
**Issue:** [#55](https://github.com/hherb/kastellan/issues/55)
**Host:** Apple M3 Max, macOS 26.5 (Build 25F71)
**`container` version:** 0.12.3 (installed via `brew install container`)
**Scope:** one-session feasibility spike. Throwaway POC + this write-up.
**Verdict:** **COMMIT** — viable. Recommended slice shape at bottom.

## What's Apple `container`?

A macOS-native CLI for running Linux containers in lightweight VMs backed by `Virtualization.framework` (Apple's equivalent of Firecracker). Apache-2.0 ([apple/container](https://github.com/apple/container)), distributed via Homebrew (`brew install container`). Requires ARM64 + macOS ≥ 15. The CLI surface is Docker-like; semantics are container-per-VM with a single shared kernel image.

## Answers to the four discovery questions

### Q1 — Is the CLI stable enough?

**Yes, for our purposes.** 0.12.3 is the stable bottle on `homebrew-core`. The CLI surface is clear: `container run`, `container exec`, `container network create`, `container image pull`, etc. — Docker-flavoured but Apple-implemented. Every invocation in the spike (~25 runs across the matrix) behaved as documented. The first start-up needs a one-time kernel install (kata-containers static kernel, downloaded automatically by `container system start --enable-kernel-install`) — that step is interactive by default but scriptable with the flag.

One CLI ergonomic quirk: every `container run` emits `[6/6] Starting container [0s]`-style progress bars on **stderr**. They don't corrupt stdout (verified: JSON-RPC lines pass through cleanly), but they will interleave with worker `tracing` output and need to be either suppressed (via `--progress none`) or stripped in `tool_host`.

### Q2 — Can JSON-RPC stdio work over the container boundary?

**Yes, cleanly.** `container run -i <image> <cmd>` keeps stdin open and pipes through line-buffered. Verified two-way:

- **Single-shot:** `echo '{"jsonrpc":"2.0",...}' | container run --rm -i alpine cat` → exact byte-for-byte round-trip on stdout.
- **Long-lived multi-call:** five JSON-RPC requests fed sequentially into one `container run --rm -i alpine sh -c 'while read line; do ... done'` returned five responses through the same stdio pair in **0.76s total** — the per-request cost *inside* an already-running container is effectively zero.

This is exactly the shape `core::worker_lifecycle::IdleTimeoutLifecycle` is designed for. The spawn cost amortises across all calls within the idle window.

### Q3 — What does the `SandboxPolicy` mapping look like?

Clean 1-to-1 mapping. Apple `container` exposes every primitive our policy needs:

| `SandboxPolicy` field | `container run` flag | Notes |
|---|---|---|
| `fs_read: Vec<PathBuf>` | `--mount type=bind,source=<P>,target=<P>,readonly` per path | Same shape as bwrap `--ro-bind`. |
| `fs_write: Vec<PathBuf>` | `--mount type=bind,source=<P>,target=<P>` per path | Same shape as bwrap `--bind`. |
| `mem_mb: u64` | `-m <N>M` (suffix-aware; value is MiB per the policy field's unit) | **Floor: 200 MiB.** `container` rejects anything smaller with `invalidArgument: minimum memory amount allowed is 200 MiB`. Slice 1 emits this via `clamp_memory_to_minimum`; see Slice 1 note about emitting a `tracing::warn!` when clamping actually fires (so operators see when their policy is being widened). |
| `cpu_ms: u64` | reuse `workers/prelude::rlimit::apply_from_env` via `KASTELLAN_CPU_MS` env | POSIX `RLIMIT_CPU` works inside the Linux VM unchanged — same code path as the existing Linux/macOS workers. No `container`-side flag needed. |
| `cpu_quota_pct: Option<u32>` | `-c <fractional vCPUs>` (e.g. `200% → -c 2.0`) | The field is documented as "percent of one CPU" (per `sandbox/src/lib.rs::SandboxPolicy::cpu_quota_pct`), so `200%` ↔ `2.0` vCPUs is the natural conversion. **The field is Linux-cgroup-only today** (the docstring says so explicitly); macOS support is new in this slice. |
| `tasks_max: Option<u64>` | `--ulimit nproc=<N>:<N>` | **Semantic gap worth flagging.** On Linux, `tasks_max` maps to cgroup `pids.max` (per-cgroup process count, enforced by the kernel pids controller). On macOS via `--ulimit nproc`, it becomes per-real-UID `RLIMIT_NPROC` inside the Linux VM — i.e. per-user across the VM rather than per-cgroup. Inside a one-worker container running as a single UID the practical effect is similar, but the guarantees are not identical and Slice 1 should call this out in the field's doc-comment. |
| `env: Vec<(String, String)>` | `-e <key>=<value>` per entry | Direct match. |
| `net: Net::Deny` | `--network none` | Verified: loopback only, no `eth0`. Fail-closed. |
| `net: Net::Allowlist(_)` | `--network default` + egress proxy worker | Same architectural posture as Linux — the bridge is open NAT, the allowlist is enforced one process out by the future egress proxy. |
| `profile: WorkerStrict` | `--read-only --cap-drop ALL --network none --user nobody --tmpfs <scratch>` | Verified: `--read-only` rejects `touch /usr/bin/x`; `--cap-drop ALL` makes `ping` fail with `permission denied`; `--tmpfs /scratch` provides writable scratch atop read-only root. |
| `profile: NetClient` | `--read-only --cap-drop ALL --network default --user nobody --tmpfs <scratch>` | Same as Strict plus NAT egress. |
| Workspace lifecycle | `--rm --name <task_id>` | Same posture as bwrap: container auto-removed on exit. `--cidfile` available if we want to track CIDs out-of-band. |

**Cross-platform parity context (load-bearing — Linux side cited):** the `SandboxPolicy::mem_mb` field is documented *today* as **"Linux only — enforced via cgroup `MemoryMax`. macOS memory enforcement is deferred to the future micro-VM backend (RLIMIT_AS has high false-positive risk)"** ([`sandbox/src/lib.rs`](../../../sandbox/src/lib.rs)). The Linux enforcement path is `systemd-run --user --scope -p MemoryMax=<N>M -p MemorySwapMax=0` wrapping `bwrap` ([`sandbox/src/linux_cgroup.rs`](../../../sandbox/src/linux_cgroup.rs)), verified by the `worker_with_low_mem_max_is_oom_killed` integration test in [`sandbox/tests/linux_smoke.rs`](../../../sandbox/tests/linux_smoke.rs). This spike's recommended `MacosContainerBackend` is **the planned macOS counterpart already named in that docstring** — closing today's open `mem_mb` gap on macOS rather than introducing a one-platform-stronger asymmetry. Same observation applies to `cpu_quota_pct` and `tasks_max` (both Linux-cgroup-only today per their docstrings); the container backend adds the macOS-side enforcement these fields already documented as Linux-only.

**Filesystem isolation verified:** by default `/Users`, `/home`, the host `/etc/passwd` are all invisible. `/etc/passwd` inside is the container's own (`root:x:0:0:root:/root:/bin/sh`).

**Memory cap verified:** Python bytearray allocator at `-m 256M` died with exit 137 (SIGKILL) at ~192 MiB allocated — overhead + buffer accounts for the rest.

**Read-only root verified:** writes to `/usr/bin/x`, `/tmp/x` all rejected with `Read-only file system`; combining `--read-only` with `--tmpfs /tmp` gives writable scratch without losing the read-only invariant on the rest.

### Q4 — Cold-start latency?

| Path | Observed | vs reference |
|---|---|---|
| First-ever pull of `alpine:3.20` (multi-arch fan-out, ~6 MB image + multi-arch unpack) | **42 s** | one-time, image-fetch dominated |
| Cold container run after image pull (first run with cleared cache) | **14.5 s** | one-time, kernel boot + VM init |
| **Warm spawn** (image cached, fresh container per call) | **0.76–0.81 s** | 80× slower than bwrap (~10 ms), 15× slower than Seatbelt (~50 ms) |
| Per-request inside already-running container (long-lived stdio worker) | **≈ 0 ms** | dominated by stdio + sh `read` loop |

The warm 0.8 s per-spawn is the load-bearing number. For `SingleUseLifecycle` workers (today's `shell-exec` posture) this is real per-call latency tax. For `IdleTimeoutLifecycle` workers (the GLiNER-Relex shape, already in tree) it amortises to zero across the idle window. **Design intent:** no current worker is proposed to use `SingleUseLifecycle` + container backend in Slices 1–3 below — `shell-exec` stays on Seatbelt (Slice 2's default), and the workers that move to container (`gliner-relex`, future `python-exec`) all use `IdleTimeoutLifecycle`. If a future slice does pair `SingleUseLifecycle` with container, the 0.8 s amortises across exactly one call and the latency tax is real — flag it in that slice's spec.

## Recommended slice shape

**Goal:** add `MacosContainerBackend` as a **sibling** to `MacosSeatbelt`, selected per-worker, not as a replacement.

Rationale: replacing Seatbelt globally costs us today's <50 ms macOS bwrap-equivalent latency for every worker that doesn't need a memory cap. Most current workers (`shell-exec`, future tiny stdio shims) don't need memory enforcement; the lightweight Seatbelt path stays correct. Workers that *do* need memory caps (`gliner-relex`, future `python-exec` for agent-authored Python) opt in to the container backend via a new manifest field.

### Slice 1 (1 session) — `MacosContainerBackend` skeleton

- New `sandbox/src/macos_container.rs` implementing `SandboxBackend`.
  - `probe()` checks `container --version` exit 0 + `container system status` returns `running`. Fails closed if either fails.
  - `spawn_under_policy(&SandboxPolicy, &[OsString]) -> io::Result<Child>` builds the `container run` argv from the mapping table above and spawns. Sandbox argv builder is a pure function (per project convention from `linux_bwrap.rs::build_argv`).
  - Wire the `--progress none` flag globally so `[6/6]` lines don't pollute stderr. **Defense-in-depth:** the `tool_host` stderr-mux path should also be resilient to unexpected stderr lines (i.e. if `--progress none` is removed or renamed in a future `container` release, the worker still functions and we get a tracing-level log rather than a hard tool-dispatch failure). Pin this with a unit test that feeds a noisy stderr fixture through the mux and asserts the JSON-RPC stdout response still parses.
- New `clamp_memory_to_minimum(mem_mib: u64) -> ClampedMemory` pure helper (or a `(u64, bool)` tuple) returning **both the clamped value and a flag indicating whether clamping fired**. Callsite in `build_container_argv` emits `tracing::warn!(requested = mem_mb, clamped_to = 200, "container backend raised mem_mb below 200 MiB floor")` exactly when the flag is true, so operators see when their policy has been silently widened. Pinned with unit tests: 1 MiB → (200, true); 100 MiB → (200, true); 256 MiB → (256, false); 1 GiB → (1024, false).
- Unit-test `build_container_argv` exhaustively, mirroring the `linux_bwrap` pattern (~10–15 cases covering each policy field's flag emission).
- Cross-platform smoke test under `macos_container_smoke.rs` running `--cap-drop ALL` + bind-mount + memory-cap + stdio-round-trip, parallel to `macos_smoke.rs` for Seatbelt.
- **Field doc-comment updates** (small but load-bearing for cross-platform correctness): widen the `Linux only` notes on `SandboxPolicy::{mem_mb, cpu_quota_pct, tasks_max}` to reflect that macOS now enforces these via the container backend when selected; call out the `tasks_max` semantic difference (cgroup `pids.max` per-cgroup on Linux vs `RLIMIT_NPROC` per-UID on macOS) in that field's doc-comment.

### Slice 2 (0.5–1 session) — per-worker backend selection

- Add `WorkerSpec.sandbox_backend: Option<SandboxBackendKind>` (`Seatbelt` / `Container` / `Bwrap`).
- `default_sandbox()` on darwin returns Seatbelt unless `spec.sandbox_backend == Some(Container)`.
- **Validation target for Slice 2 is a plain `alpine` smoke worker, not `gliner-relex`.** Real-model validation against `gliner-relex` requires the image-build path (cross-compile `aarch64-unknown-linux-musl` or build a Python+CUDA-less Alpine image with the model weights mounted in) — that's Slice 2.5 below, not Slice 2. Keeping the scope honest: Slice 2 proves the backend-selection plumbing works; Slice 2.5 proves the worker-image plumbing works; only then can a production worker meaningfully run under container.

### Slice 2.5 (1 session, depends on Slice 2) — `gliner-relex` Containerfile + image-build smoke

- Write a `workers/gliner-relex/Containerfile` (Python 3.12 + `uv sync` + weights mounted at `/weights` via `--mount`).
- Operator-runnable `container build -t kastellan/gliner-relex:dev workers/gliner-relex/` step (no `cargo build` automation yet — that's a future slice).
- Update the `gliner-relex` `WorkerSpec` to set `sandbox_backend: Some(Container)` + the image tag, then re-run the e2e on macOS and confirm canonical `Dr Smith --[treats]--> asthma (0.994)` output through the container.
- This is the slice that actually validates the end-to-end story on a real workload that needs memory enforcement. Slice 2 is just the plumbing.

### Slice 3 (deferred until Phase 4) — `python-exec` defaults to container on macOS

- New `python-exec` worker (agent-authored Python; higher trust risk) defaults to `Container` on darwin via its manifest.
- Validates the architectural choice with a worker that genuinely needs memory enforcement.

## What this spike deliberately does NOT do

- **No worker-image build pipeline.** Slices above assume the operator runs `container build` against a per-worker `Containerfile` and tags locally. A future slice could automate this via `cargo build` integration, but that's a meaningful slice on its own.
- **No Containerfile for any existing worker.** Today's workers are macOS-native binaries (`kastellan-worker-shell-exec` builds for darwin). Adopting container backend per-worker requires either (a) cross-compiling the Rust worker to `aarch64-unknown-linux-musl` and packaging into an image, or (b) writing the worker in a language with a Linux runtime that already has an image (Python is the obvious candidate for `gliner-relex` — Slice 2.5 above is exactly this). Slice 1's smoke test will use a plain `alpine` shim until the image-build path exists; Slice 2's plumbing-validation also uses `alpine`. Slice 2.5 ships the first real `Containerfile` (for `gliner-relex`).
- **No latency comparison to a real GLiNER-Relex inference call.** The 0.76 s warm-spawn cost is the floor; the actual end-to-end cost depends on model load time, which we know from the macOS MPS spike is ~3.7 s cold + 82 ms per-call CPU on Apple Silicon.
- **No Linux Firecracker counterpart.** The cross-platform parity story is the opposite direction from the usual: today Linux *already* enforces `mem_mb` / `cpu_quota_pct` / `tasks_max` via `systemd-run --user --scope` + cgroup v2 ([`sandbox/src/linux_cgroup.rs`](../../../sandbox/src/linux_cgroup.rs)), and macOS is the platform with the open gap (Seatbelt has no memory primitive — see the `mem_mb` docstring in [`sandbox/src/lib.rs`](../../../sandbox/src/lib.rs)). Adding `MacosContainerBackend` closes the macOS gap and brings the two platforms to parity; it is **not** a Linux-side strengthening. A Linux-side Firecracker layer would be defense-in-depth atop the existing cgroup enforcement, not a parity fix, and is out of scope for this spike.

## Cost trade-offs (the operator-readable summary)

| Property | Seatbelt (current macOS) | Apple `container` (proposed) |
|---|---|---|
| Memory cap | **none** (open gap) | yes, with 200 MiB floor + SIGKILL on overrun |
| Spawn latency | ~50 ms | ~800 ms warm / 14.5 s cold |
| Per-call cost (long-lived worker) | ~0 ms | ~0 ms |
| Worker binary | macOS-native (no extra build step) | Linux binary, packaged as container image |
| FS allowlist | yes (TinyScheme rules) | yes (`--mount` flags) |
| Net deny | yes (Seatbelt `deny network*`) | yes (`--network none`) |
| Capability drop | implicit (Seatbelt allow-list) | explicit (`--cap-drop ALL`) |
| Cross-platform mapping to `SandboxPolicy` | yes | yes |

## Closing note + back-out

The spike installed `container` 0.12.3 + the kata-containers static kernel. To back out: `brew services stop container` (if registered; the spike used `container system start` only), `container system stop`, `brew uninstall container`, `rm -rf "~/Library/Application Support/com.apple.container/"`. Total disk footprint ≈ 263 MB binaries + the kernel install.

This spike leaves the install in place for the recommended Slice 1 follow-up. If the slice doesn't pick up within ~2 sessions, the operator should back out per the steps above to reclaim disk.
