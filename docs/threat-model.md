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
| Sandbox            | OS-enforced FS / net / syscall limits per worker |
| Egress proxy       | Per-worker host allowlist, TLS pinning, audit-log every request |
| Postgres role isolation | Workers cannot reach Postgres at all; only the core has the DB connection |
| Append-only audit log   | Every tool call, LLM call, channel message, memory write |

## Negative tests (CI-enforced as backends land)

- `python-exec` attempts `socket.connect` → blocked.
- `web-fetch` attempts a non-allowlisted host → blocked at egress proxy.
- `shell-exec` attempts a non-allowlisted argv → rejected before spawn.
- `browser-driver` attempts to read `~/.ssh/` → blocked by sandbox.
- Adversarial web page in agent context tries to exfiltrate via `web-fetch` → request blocked, audit log shows attempt.

## Open items

- Choice of egress-proxy TLS-pinning approach (cert pinning vs CA pinning vs SPKI pinning).
- Whether `python-exec` should default to micro-VM rather than seccomp/Seatbelt-only.
- Concrete `setrlimit` budgets per worker class.
