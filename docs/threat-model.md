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

## Out of scope

- Hardware attacks, GPU side-channels, kernel 0-days.
- The user's own account being malicious.
- Model weight extraction.
- Defending the user's wider machine from the user themselves.

## Asymmetric platform note

The macOS sandbox (`sandbox-exec` / Seatbelt) is partially private API and less audited than the Linux stack (bubblewrap + Landlock + seccomp-bpf, battle-tested via Flatpak). The *weaker* of the two platform backends sets the real bar. We accept this asymmetry openly here rather than implying the two are identical. Where higher assurance is required on macOS, opt the relevant worker into the micro-VM backend (Apple `container` CLI on Tahoe+).

## Defence-in-depth layers

| Layer | Purpose |
| ----- | ------- |
| Policy gate (core) | Static allow/deny per `(tool, args, data class)` before any tool spawn |
| Parent-side sandbox (bwrap / Seatbelt) | Namespace isolation, FS bind-mount, network unshare. Applied by `core::tool_host`. |
| Worker-side sandbox (Landlock + seccomp-bpf) | Second, finer kernel filter installed by the worker on itself via [`hhagent-worker-prelude`](../workers/prelude/). One-way: cannot be relaxed once `restrict_self`/`apply_filter` returns. |
| Egress proxy       | Per-worker host allowlist, TLS pinning, audit-log every request |
| Postgres role isolation | Workers cannot reach Postgres at all; only the core has the DB connection |
| Append-only audit log   | Every tool call, LLM call, channel message, memory write |

The two sandbox rows together implement the "parent denies + child denies again" double containment: a kernel bug in either layer alone does not breach the worker's threat boundary. The worker-side layer is enforced from inside the worker process *after* dynamic-linker resolution but *before* serving any JSON-RPC request, via `hhagent_worker_prelude::serve_stdio`.

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

## Open items

- Choice of egress-proxy TLS-pinning approach (cert pinning vs CA pinning vs SPKI pinning).
- Whether `python-exec` should default to micro-VM rather than seccomp/Seatbelt-only.
- Concrete `setrlimit` budgets per worker class.
