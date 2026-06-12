# kastellan — Threat Model

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
worth stating explicitly: **the install directory containing `kastellan` and its
worker binaries must not be writable by the agent's own OS user.** The invariant
above grants a worst-case compromise the agent's own user account — so if
`<exe_dir>` were user-writable, a compromised process could drop a malicious
`kastellan-worker-<name>` next to the daemon and have it registered as a tool on
the next start. Production deployment therefore installs the daemon + workers
into a root-owned bindir (the systemd/launchd unit's install path); the
user-writable cargo `target/debug` tree is a dev convenience, not a production
trust boundary. The `KASTELLAN_*_BIN` override is authoritative and **fails
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
| Worker-side sandbox (Landlock + seccomp-bpf) | Second, finer kernel filter installed by the worker on itself via [`kastellan-worker-prelude`](../workers/prelude/). One-way: cannot be relaxed once `restrict_self`/`apply_filter` returns. |
| Resource caps (Linux: cgroup v2 via `systemd-run --user --scope`) | Hard `MemoryMax` + `MemorySwapMax=0` from `policy.mem_mb`; defense-in-depth `CPUQuota=200%` and `TasksMax=64` defaults. Wraps `bwrap` so the cgroup is in place before the worker namespace is created. Applied by [`sandbox::linux_cgroup`](../sandbox/src/linux_cgroup.rs). |
| Egress proxy       | Per-worker host:port allowlist, SSRF/IP-pinning, TLS pinning, audit-log every request. **Slices #1+#2 built** (boundary allowlist + SSRF/IP defense + unbypassable OS force-routing + CONNECT-over-UDS transport + port-scoping #241, `workers/egress-proxy`); the scheduler auto-flip that makes force-routing the default live path is the remaining wire-up — see "Network egress" below. TLS-intercept leak-scanner + TLS-pinning are slices #3–4. |
| Postgres role isolation | Workers cannot reach Postgres at all; only the core has the DB connection |
| Append-only audit log   | Every tool call, LLM call, channel message, memory write |

The two sandbox rows together implement the "parent denies + child denies again" double containment: a kernel bug in either layer alone does not breach the worker's threat boundary. The worker-side layer is enforced from inside the worker process *after* dynamic-linker resolution but *before* serving any JSON-RPC request, via `kastellan_worker_prelude::serve_stdio`.

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

Crucially, the worker's self-enforced allowlist matches **host *names*, not
resolved IPs.** DNS resolution happens inside the jail, so an allowlisted name
that resolves to a private/internal address — or an attacker performing DNS
rebinding on a record they control — would still be connected to.
**Host-allowlist ≠ IP-level containment.**

**Egress-proxy slice #1 (2026-06-10) builds the mechanism that closes this gap**
([`workers/egress-proxy`](../workers/egress-proxy/)): a sandboxed per-worker
CONNECT proxy that resolves DNS *itself*, rejects
private/loopback/link-local/ULA/CGNAT/multicast resolved IPs (with a literal-IP
carve-out for an operator-allowlisted address such as a local SearxNG
`127.0.0.1`), **pins** the surviving IP, dials it, and audits every decision.

**Egress-proxy slice #2 (2026-06-11) builds the unbypassable force-routing**
([`feat/egress-proxy-slice2-impl`](../workers/egress-proxy/)). Three layers now
make a *compromised* worker unable to bypass the proxy:
- **OS-level barrier (the kernel does the enforcing, not the worker).** A
  `Net::Allowlist` worker with `proxy_uds` set is placed in a **private network
  namespace** on Linux (`bwrap`: `--unshare-all` minus `--share-net`, the proxy
  UDS bind-mounted in — AF_UNIX is mount-ns-scoped, not net-ns) and a
  **deny-all-outbound-except-the-UDS** Seatbelt filter on macOS. The worker has
  *no route off the allowlist*; its only egress is the proxy UDS. The macOS
  filter is gated by a real on-host probe (`seatbelt_uds_probe.rs`, AF_INET
  denied / UDS allowed — **confirmed** on the dev Mac); if a host can't prove
  AF_INET is denied, net workers fall back to the `MacosContainer` (real VM
  netns) backend. The Linux kernel barrier is proven by `linux_force_routing.rs`
  (run on the DGX).
- **Worker transport.** The worker reaches origins only by speaking
  `CONNECT host:port` to the proxy over the UDS (`web-common::ProxyConnectGet`,
  selected by `make_get` when `KASTELLAN_EGRESS_PROXY_UDS` is set). TLS stays
  end-to-end worker↔origin.
- **Port-scoped boundary (#241).** The proxy's allowlist now matches the
  `host:port` *endpoint*, not just the host; a bare-host (port-unconstrained)
  grant is flagged distinctly in `audit_log`.

The coupled host-side spawn (`core::egress::spawn_net_worker`: sidecar-first +
**fail-closed** — no proxy ⇒ no worker — policy rewrite, 1:1 teardown,
decision-ingest → `audit_log`) is **built and unit-tested**. The remaining step
to make this the *default live path* is wiring `spawn_net_worker` into the
scheduler's worker-lifecycle spawn site (a shared-trait change, landing with the
DGX force-routing acceptance run). Until that flip ships and the operator enables
it, the live containment for `web-fetch`/`web-search` remains the worker's
self-enforced allowlist — so do not yet treat the proxy as protection against
egress to internal IP ranges from a compromised worker in the running daemon,
even though the mechanism that provides it is now complete and tested.
`Net::ProxyEgress` is the policy variant the proxy itself runs under.

## Communication channel (adversary #5)

The primary user↔kastellan channel is **Matrix, self-hosted, single-user, federation OFF**
(E2E via `matrix-rust-sdk`), with **email as a cross-transport, low-trust fallback**
(decision 2026-06-12 —
[`docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`](superpowers/specs/2026-06-12-primary-communication-channel-design.md)).
The channel defends adversary #5 ("a messaging-channel peer impersonates the user") in three
separable layers, because transport security and peer identity are distinct problems:

1. **Transport confidentiality + integrity (E2E).** Matrix E2E stops the homeserver/provider or
   any MITM from *reading or injecting* message content. The pairing layer below does **not**
   cover this — only E2E does. Federation-off shrinks the homeserver attack surface to a
   near-private two-party appliance.
2. **Peer authentication (pairing).** The Phase-2 DM pairing flow (TOTP/HOTP + WebAuthn, revocable,
   audited) authenticates the *peer principal* above the channel-bus, transport-agnostic;
   Matrix device cross-signing reinforces it channel-natively.
3. **Untrusted-input screening + audit.** Every inbound channel message is screened by
   `cassandra::injection_guard` exactly like worker output — a channel peer is no more trusted
   than a fetched web page — and every inbound/outbound message lands in `audit_log`.

**Channel-worker network containment:** the Matrix/IMAP/SMTP client runs under `Net::Allowlist`
scoped to only its configured server endpoint(s), force-routed through the per-worker egress
proxy, so a compromised channel worker reaches its one server and nothing else.

**Homeserver hosting blast radius (Tiers B/C).** Co-hosting conduwuit on the WireGuard/ingress
VPS (Tier B) or on the kastellan host (Tier C, "poor man's") places the larger public-facing
surface adjacent to, respectively, the network tunnel into the home/DGX network or the agent's
own user/Postgres/scratch/vault. A homeserver RCE then has shared-host adjacency to those
assets. Tier A (a dedicated VPS) is preferred for this reason; Tiers B/C require systemd
hardening (dedicated unprivileged user, `NoNewPrivileges`/`ProtectSystem=strict`/tight
`SystemCallFilter`, loopback-bound behind a TLS reverse proxy, no federation port) as the
minimum bar — defense-in-depth that reduces but does not eliminate shared-host blast radius.
**Email is the fallback because Matrix has no single-user homeserver failover** — redundancy is
cross-transport, not a second homeserver. Email is treated as **low-trust** (spoofable):
notifications only, never commands, surfaced only after SPF/DKIM/DMARC pass + a per-pairing
in-body token.

## Negative tests (CI-enforced as backends land)

- `python-exec` attempts `socket.connect` → blocked.
- `web-fetch` attempts a non-allowlisted host → blocked at egress proxy.
- `shell-exec` attempts a non-allowlisted argv → rejected before spawn.
- `browser-driver` attempts to read `~/.ssh/` → blocked by sandbox.
- Adversarial web page in agent context tries to exfiltrate via `web-fetch` → request blocked, audit log shows attempt.
- `channel`: a message from an **unpaired** peer → dropped (never enqueued as a task), audit row `channel.rejected_unpaired`. (Shipped: `core/src/channel` `handle_inbound` + the hermetic/PG e2e; the unpaired peer's body is never even screened/echoed — authorize-before-screen.)
- `channel`: an inbound message carrying a catalogued prompt-injection → blocked (never enqueued), audit row `channel.injection_blocked` carrying only the SHA-256 + reason codes (never the body). (Shipped: `classify_inbound` under `GuardProfile::Strict`.)

Already shipped (Phase 0 + Phase 0 hardening stage 1):

- `sandbox/tests/linux_smoke.rs` — bwrap denies `/etc/passwd`, `/home`, network under `Net::Deny`.
- `core/tests/shell_exec_e2e.rs` — non-allowlisted argv rejected by worker policy with `POLICY_DENIED`; full round-trip through bwrap + Landlock + seccomp.
- `workers/prelude/tests/landlock_smoke.rs` — write to non-allowlisted path is denied with EACCES; allowlisted scratch writes succeed; reads under `/usr` continue to work.
- `workers/prelude/tests/seccomp_smoke.rs` — `unshare(CLONE_NEWUSER)` and `mount(...)` are killed with `SIGSYS`; `getpid()` survives.
- `sandbox/tests/macos_smoke.rs` — Seatbelt denies `/etc/master.passwd`, `/Users/...`, raw `/dev/disk0`, and network under `Net::Deny`. Also: a worker calling `bootstrap_look_up("com.apple.coreservices.appleevents")` is denied (`worker_cannot_look_up_arbitrary_mach_services`, issue #1) — closes the largest pre-existing asymmetry vs the threat-model invariant; and the worker process is the leader of a fresh session, so any future attempt to open `/dev/tty` fails with ENXIO regardless of profile broadening (`worker_runs_in_its_own_session`, issue #2).
- `sandbox/tests/linux_smoke.rs::worker_with_low_mem_max_is_oom_killed` — a worker that allocates 256 MiB under `MemoryMax=32M` is OOM-killed by the kernel. Closes the cgroup-resource layer.

## Open items

- Choice of egress-proxy TLS-pinning approach (cert pinning vs CA pinning vs SPKI pinning).
- Egress proxy must close the `web-fetch` SSRF/DNS-rebinding gap: reject private/link-local/loopback resolved IPs and pin the resolved address across the connection (see "Network egress" above). The host-name allowlist alone does not contain egress to internal IP ranges. **Slice #1 implements this logic** (`workers/egress-proxy`), but it is not yet enforcing on live workers — slice #2's force-routing wires it in.
- Whether `python-exec` should default to micro-VM rather than seccomp/Seatbelt-only.
- Concrete `setrlimit` budgets per worker class.
