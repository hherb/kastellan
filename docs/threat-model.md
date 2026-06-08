# hhagent — Threat Model

> **Status: skeleton.** Updated as backends and workers come online.

## Invariant

A worst-case compromise reaches at most:

1. The agent's own OS user account.
2. Its own Postgres role (one DB on a localhost UDS, peer auth).
3. Its own scratch FS (per-worker scratch dir).
4. The explicitly allowlisted network endpoints for the *single* tool that was compromised.

Nothing else.

## Adversaries / scenarios in scope

1. **Prompt injection** drives malicious tool calls (the LLM is *not* trusted).
2. **A tool worker is fully compromised** — RCE inside the sandbox.
3. **A Python dependency contains a supply-chain backdoor**.
4. **The agent autonomously authors malicious Python** and runs it (Phase 4 capability).
5. **A messaging-channel peer impersonates the user**.
6. **Memory-write injection** — a process (or compromised worker) with `INSERT`
   on `memories` plants attacker-controlled text. The recall lane
   (`core::recall_assembly`, wired into `RouterAgent::formulate_plan` from
   2026-05-17) surfaces matching rows verbatim inside the assembled system
   prompt's `<recalled>` block. Phase 1 trusts the model's tokeniser on the
   same basis as L0/L1; if `memories` writes ever become reachable from a
   less-trusted code path (e.g. a tool worker), the recall lane must
   sanitise (or partition by trust label) before rendering.

## Out of scope

- Hardware attacks, GPU side-channels, kernel 0-days.
- The user's own account being malicious.
- Model weight extraction.
- Defending the user's wider machine from the user themselves.

## Worker-binary discovery trust assumption

The daemon locates plain compiled workers as **siblings of its own binary**
(`current_exe()`-relative `<exe_dir>/<worker-name>`; see
[`core::worker_manifest::discover_binary`](../core/src/worker_manifest.rs)), so a
flat install resolves with no env vars. This introduces one trust assumption
worth stating explicitly: **the install directory containing `hhagent` and its
worker binaries must not be writable by the agent's own OS user.** The invariant
above grants a worst-case compromise the agent's own user account — so if
`<exe_dir>` were user-writable, a compromised process could drop a malicious
`hhagent-worker-<name>` next to the daemon and have it registered as a tool on
the next start. Production deployment therefore installs the daemon + workers
into a root-owned bindir (the systemd/launchd unit's install path); the
user-writable cargo `target/debug` tree is a dev convenience, not a production
trust boundary. The `HHAGENT_*_BIN` override is authoritative and **fails
closed** (a set-but-invalid override is rejected, never silently substituted by
the sibling), so it cannot be used to widen discovery beyond the operator's
explicit intent.

## Asymmetric platform note

The macOS sandbox (`sandbox-exec` / Seatbelt) is partially private API and less audited than the Linux stack (bubblewrap + Landlock + seccomp-bpf, battle-tested via Flatpak). The *weaker* of the two platform backends sets the real bar. We accept this asymmetry openly here rather than implying the two are identical. Where higher assurance is required on macOS, opt the relevant worker into the micro-VM backend (Apple `container` CLI on Tahoe+).

The macOS implementation shells out to `/usr/bin/sandbox-exec`, which Apple
has marked as private API and emits a deprecation warning for, while
continuing to ship and maintain it (it remains the foundation of the
system's own sandboxing of daemons under `/usr/share/sandbox/`). We accept
this risk explicitly: should Apple ever remove `sandbox-exec`, the
migration path is the entitlement-based App Sandbox combined with Endpoint
Security framework filters, both of which require code-signing and
entitlements that we do not have today. Until that day, `sandbox-exec` is
the best containment available without entitlements.

## Defence-in-depth layers

| Layer | Purpose |
| ----- | ------- |
| Policy gate (core) | Static allow/deny per `(tool, args, data class)` before any tool spawn |
| Parent-side sandbox (bwrap / Seatbelt) | Namespace isolation, FS bind-mount, network unshare. Applied by `core::tool_host`. |
| Worker-side sandbox (Landlock + seccomp-bpf) | Second, finer kernel filter installed by the worker on itself via [`hhagent-worker-prelude`](../workers/prelude/). One-way: cannot be relaxed once `restrict_self`/`apply_filter` returns. |
| Resource caps (Linux: cgroup v2 via `systemd-run --user --scope`) | Hard `MemoryMax` + `MemorySwapMax=0` from `policy.mem_mb`; defense-in-depth `CPUQuota=200%` and `TasksMax=64` defaults. Wraps `bwrap` so the cgroup is in place before the worker namespace is created. Applied by [`sandbox::linux_cgroup`](../sandbox/src/linux_cgroup.rs). |
| Egress proxy       | Per-worker host allowlist, TLS pinning, audit-log every request |
| Postgres role isolation | Workers cannot reach Postgres at all; only the core has the DB connection |
| Append-only audit log   | Every tool call, LLM call, channel message, memory write |

The two sandbox rows together implement the "parent denies + child denies again" double containment: a kernel bug in either layer alone does not breach the worker's threat boundary. The worker-side layer is enforced from inside the worker process *after* dynamic-linker resolution but *before* serving any JSON-RPC request, via `hhagent_worker_prelude::serve_stdio`.

### Secrets in the audit log

Redeemed secret plaintext never appears in the request snapshot (`payload.req` of any `tool:<name>` row, snapshotted *before* `secret://<8-hex>` substitution — issue #147) nor in any `actor='policy'` row (issue #146 / Item 31). It does **not** follow that the audit log is free of secrets: a worker that is legitimately handed a secret may echo it into its own output, which lands in `payload.result`. That field is the worker's response, not the request, and is out of scope of the redaction invariant — the worker is the authorized consumer, so an operator with `audit_log` read access can recover any secret a worker chose to emit. Containing worker-emitted plaintext is the egress proxy's and the injection guard's job, not the audit redactor's.

### Network egress: interim containment and the SSRF/DNS caveat

The `web-fetch` worker is the first network-egress tool, but the **egress proxy
(the row above) is not yet built**. Until it lands, containment for `web-fetch`
is the worker's *self-enforced* host allowlist: it requires `https`, matches the
request host (and every redirect hop) against the admin-controlled allowlist
sourced from `tool_allowlists`, and refuses anything off-list with
`POLICY_DENIED`. This is real (a compromised LLM cannot widen the list — it is
injected by the host-side manifest, not from `step.parameters`) but
**worker-trust-dependent**: it holds only as long as the worker binary itself is
not compromised. It becomes defense-in-depth layer 2 once the egress proxy
enforces the same allowlist at the boundary.

Crucially, the allowlist matches **host *names*, not resolved IPs.** DNS
resolution happens inside the jail, so an allowlisted name that resolves to a
private/internal address — or an attacker performing DNS rebinding on a record
they control — would still be connected to. **Host-allowlist ≠ IP-level
containment.** Closing the SSRF gap (rejecting private/link-local/loopback
targets, and pinning the resolved IP across the TLS connection) is the egress
proxy's job; do not treat the current self-enforced allowlist as protection
against egress to internal network ranges. `Net::Allowlist` policy data is
populated now precisely so the proxy slice can enforce it later.

## Negative tests (CI-enforced as backends land)

- `python-exec` attempts `socket.connect` → blocked.
- `web-fetch` attempts a non-allowlisted host → blocked at egress proxy.
- `shell-exec` attempts a non-allowlisted argv → rejected before spawn.
- `browser-driver` attempts to read `~/.ssh/` → blocked by sandbox.
- Adversarial web page in agent context tries to exfiltrate via `web-fetch` → request blocked, audit log shows attempt.

Already shipped (Phase 0 + Phase 0 hardening stage 1):

- `sandbox/tests/linux_smoke.rs` — bwrap denies `/etc/passwd`, `/home`, network under `Net::Deny`.
- `core/tests/shell_exec_e2e.rs` — non-allowlisted argv rejected by worker policy with `POLICY_DENIED`; full round-trip through bwrap + Landlock + seccomp.
- `workers/prelude/tests/landlock_smoke.rs` — write to non-allowlisted path is denied with EACCES; allowlisted scratch writes succeed; reads under `/usr` continue to work.
- `workers/prelude/tests/seccomp_smoke.rs` — `unshare(CLONE_NEWUSER)` and `mount(...)` are killed with `SIGSYS`; `getpid()` survives.
- `sandbox/tests/macos_smoke.rs` — Seatbelt denies `/etc/master.passwd`, `/Users/...`, raw `/dev/disk0`, and network under `Net::Deny`. Also: a worker calling `bootstrap_look_up("com.apple.coreservices.appleevents")` is denied (`worker_cannot_look_up_arbitrary_mach_services`, issue #1) — closes the largest pre-existing asymmetry vs the threat-model invariant; and the worker process is the leader of a fresh session, so any future attempt to open `/dev/tty` fails with ENXIO regardless of profile broadening (`worker_runs_in_its_own_session`, issue #2).
- `sandbox/tests/linux_smoke.rs::worker_with_low_mem_max_is_oom_killed` — a worker that allocates 256 MiB under `MemoryMax=32M` is OOM-killed by the kernel. Closes the cgroup-resource layer.

## Open items

- Choice of egress-proxy TLS-pinning approach (cert pinning vs CA pinning vs SPKI pinning).
- Egress proxy must close the `web-fetch` SSRF/DNS-rebinding gap: reject private/link-local/loopback resolved IPs and pin the resolved address across the connection (see "Network egress" above). The host-name allowlist alone does not contain egress to internal IP ranges.
- Whether `python-exec` should default to micro-VM rather than seccomp/Seatbelt-only.
- Concrete `setrlimit` budgets per worker class.
