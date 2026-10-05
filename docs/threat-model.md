# kastellan — Threat Model

> **Status: maintained.** Current as of 2026-07 (the v0.2.0 era); kept in step
> with the shipped backends and workers.

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
4. **The agent autonomously authors malicious Python** and runs it — live today
   via `python-exec` and the L3 python-skill lifecycle, so in scope now.
5. **A messaging-channel peer impersonates the user**.
6. **Memory-write injection** — a process (or compromised worker) with `INSERT`
   on `memories` plants attacker-controlled text. The recall lane
   (`core::recall_assembly`, wired into `RouterAgent::formulate_plan` from
   2026-05-17) surfaces matching rows inside the assembled system prompt's
   `<recalled>` block. Recalled bodies are escaped and injection-screened
   before rendering (2026-07-02 audit, #392), and recall is partitioned by
   layer: it returns only L1/L2/L4 rows, never L0 meta-rules or L3 skills,
   which reach the prompt only through their own gated blocks (#785). If `memories`
   writes ever become reachable from a less-trusted code path (e.g. a tool
   worker), recall must also partition by a per-row trust label.

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
worker binaries must not be writable by any principal other than root or the
daemon's own euid.** The per-user install that `kastellan-cli install` produces
(`~/.local/lib/kastellan`, owned by the daemon user, no root required) *is* the
trusted shape — writability by the daemon's own user is already inside the
threat-model boundary, because a compromise that can write there already runs
as the daemon. What the invariant excludes is any *lesser* principal: a
world-writable or group-writable dir, or one owned by a foreign uid, would let
that principal drop a malicious `kastellan-worker-<name>` next to the daemon
and have it registered as a tool on the next start. The daemon probes this at
startup ([`assess_install_dir` /
`InstallDirTrust`](../core/src/worker_manifest.rs): Untrusted iff
world-writable, group-writable, or owned by a uid that is neither 0 nor the
daemon's euid) — a warn-only advisory unless
`KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR=1` makes it enforcing. **Known
residual:** the probe inspects the leaf dir only, so a safe leaf under a
group/world-writable *parent* is still substitutable wholesale via `rename(2)`.
The `KASTELLAN_*_BIN` override is authoritative and **fails closed** (a
set-but-invalid override is rejected, never silently substituted by the
sibling), so it cannot be used to widen discovery beyond the operator's
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
| Parent-side sandbox (bwrap / Seatbelt) | Namespace isolation, FS bind-mount, network unshare; `--disable-userns` refuses nested user namespaces (2026-09-02). Applied by `core::tool_host`. |
| Worker-side sandbox (Landlock + seccomp-bpf) | Second, finer kernel filter installed by the worker on itself via [`kastellan-worker-prelude`](../workers/prelude/). One-way: cannot be relaxed once `restrict_self`/`apply_filter` returns. `clone` is admitted only without namespace flags and `clone3` answers `ENOSYS`, so every spelling of "new namespace" is refused, not just `unshare`; Landlock is per-thread, so networked workers construct their transports *after* lockdown (`serve_stdio_with`) and a missing profile or an unenforceable ruleset is an error, never a silent skip (all 2026-09-02). |
| **Optional separate-kernel micro-VM** (opt-in, Linux: Firecracker `FirecrackerVm`; macOS: Apple `container`) | A throwaway **guest kernel** — under KVM on Linux, under Virtualization.framework on macOS. **What it replaces vs. what it keeps (slice 1):** the micro-VM is a worker's *parent-side* sandbox backend — it is selected *instead of* the bwrap/Seatbelt row for that one worker (a worker carries exactly one backend), so for a VM-mode worker bwrap is **not** also applied. It does **not** replace the *worker-side* row **on the Linux Firecracker path**: there the unchanged worker still installs its own seccomp-bpf on itself inside the guest. Net effect is stronger than bwrap: VM-grade isolation **+** the worker self-filter, with `mem_mb` enforced by the hypervisor (closing the macOS-Seatbelt memory gap and adding a blast wall on Linux). A kernel-level escape in the worker-side seccomp layer still has to cross the guest-kernel/VM boundary. ⚠️ **On the Linux Firecracker path the worker-side LANDLOCK half is absent, and that is a deliberate, stated exception rather than an oversight:** the pinned guest kernel is built without `CONFIG_SECURITY_LANDLOCK`, so no ruleset can be created there at all, and [`linux_firecracker::plan`](../sandbox/src/linux_firecracker/plan.rs) therefore sets `KASTELLAN_LANDLOCK_PROFILE=none` for the guest — the opt-out the worker-side row's own fail-closed rule demands of any host that cannot enforce it. This is the **only** production setter of that variable, and because it is injected inside the backend — after `tool_host::warn_lockdown_overrides` has inspected the derived policy — the backend emits its own WARN per spawn so the downgrade is not silent. Seccomp is unaffected (`CONFIG_SECCOMP_FILTER=y`). What is lost is the FS rules **and** Landlock's ABI-v6 scoping of abstract UNIX sockets and signals. What stands in for it is the guest's own: a purpose-built rootfs holding only the worker and its interpreter, read-only drives for everything it must not write, and the VM boundary under all of it. Restoring the layer means moving the guest-kernel pin to one built with Landlock; until then, a **VM-mode worker on Linux has one fewer worker-side layer than the same worker under bwrap**, which is the honest trade and is why the two rows are not simply ordered. (Stacking the VM *on top of* host bwrap — VM-in-bwrap — is a later slice; today it is VM-instead-of-bwrap.) **Opt-in per worker, not the default** — `python-exec`, `web-fetch`, `web-search`, `web-research` and `browser-driver` each opt in via `KASTELLAN_{PYTHON_EXEC,WEB_FETCH,WEB_SEARCH,WEB_RESEARCH,BROWSER_DRIVER}_USE_MICROVM=1` (Linux). Net egress works inside the VM: a networked VM worker reaches its force-routed egress sidecar over a vsock transport, so VM mode is no longer `Net::Deny`-only. **The macOS `container` arm is weaker still, and differently:** `verify_pinned_kernel` has exactly one caller (`sandbox::linux_firecracker`), so nothing pins Apple's guest kernel, and the worker-side row does not survive there at all for gliner-relex — its `container_mode_entry` carries `lockdown_shim: None` and runs a Python console script, so no Rust prelude runs and **neither** seccomp nor Landlock is installed inside that guest. python-exec container mode does bake the Rust worker, but nothing in this tree establishes whether Apple's guest kernel offers either LSM; if it lacks Landlock it would hit the same fail-closed refusal Firecracker hit, with no equivalent opt-out in `sandbox::macos_container`. Treat the macOS arm as VM isolation plus an unverified worker-side layer. **On Linux** the guest kernel is sha256-pinned and re-verified at **every VM boot** ([`sandbox::guest_kernel_pin`](../sandbox/src/guest_kernel_pin.rs), fail-closed). Applied by [`sandbox::linux_firecracker`](../sandbox/src/linux_firecracker.rs) / `sandbox::macos_container`. Scope is unchanged for non-opted workers (still bwrap/Seatbelt). |
| Resource caps (Linux: cgroup v2 via `systemd-run --user --scope`) | Hard `MemoryMax` + `MemorySwapMax=0` from `policy.mem_mb`; defense-in-depth `CPUQuota=200%` and `TasksMax=64` defaults. Wraps `bwrap` so the cgroup is in place before the worker namespace is created. Applied by [`sandbox::linux_cgroup`](../sandbox/src/linux_cgroup.rs). |
| Egress proxy       | Per-worker host:port allowlist, SSRF/IP-pinning, TLS-intercept leak scan, SPKI pinning, audit-log every request. **All four slices built** (`workers/egress-proxy`) and **force-routed by default** in the supervised deployment (`KASTELLAN_EGRESS_FORCE_ROUTING=1`) — see "Network egress" below. |
| Postgres role isolation | Workers cannot reach Postgres at all; only the core has the DB connection |
| Append-only audit log   | Every tool call, LLM call, channel message, memory write |

The two sandbox rows together implement the "parent denies + child denies again" double containment: a kernel bug in either layer alone does not breach the worker's threat boundary. The worker-side layer is enforced from inside the worker process *after* dynamic-linker resolution but *before* serving any JSON-RPC request, via `kastellan_worker_prelude::serve_stdio`.

### Secrets in the audit log

Redeemed secret plaintext never appears in the request snapshot (`payload.req` of any `tool:<name>` row, snapshotted *before* `secret://<8-hex>` substitution — issue #147) nor in any `actor='policy'` row (issue #146 / Item 31). Since the [2026-09-02 audit](security-audit-2026-09-02.md) (finding H1) the dispatch chokepoint also scrubs every secret redeemed for a call out of the worker's **response** — the `Ok` value *and* an `RpcError`'s message/data — before either reaches `payload.result` / `payload.err`, the JSONL mirror, or the planner's step detail. The scrub matches verbatim, contiguous bytes only (`leak-scan`), so an encoded or split secret still evades it; it is the floor under a per-tool secret policy, not a substitute for one, and worker-emitted plaintext remains the egress proxy's and the injection guard's problem on the network side.

### User data in the daemon log

The `audit_log` invariants above cover the database, not the daemon's own
tracing output (`~/.local/state/kastellan/*.out` under the supervised
deployment) — a plaintext file readable by anything running as the agent's OS
user, with none of the `audit_log` table's role gating. Anything logged there
is *more* exposed than the same bytes in Postgres, not less, so log statements
carrying model or worker payloads are held at `debug!` rather than `warn!`.

The live case is `scheduler::agent`'s plan-decode failure path: a planner
completion restates recalled memories and prior step output verbatim, so on a
mail task the failed plan contains the user's correspondence. The `warn!` there
carries only structural facts (`detail`, `raw_len`, `has_brace`,
`finish_reason`, token counts) — enough to tell an empty completion from a
non-JSON one, which is the discrimination that matters — while the raw head
sits at `debug!`. Raising the log level to triage a live planner fault is
therefore a deliberate act that widens exposure for the duration.

### Injection-screening of planner-bound tool output (and its split-slice limit)

Successful tool output is fed back to the planner (#338), so every worker
result is screened by `cassandra::injection_guard` before it can reach the
planner prompt. Two chokepoints enforce this: `tool_host::dispatch` screens the
first `SCAN_BYTE_CAP` (64 KiB) of each result inline, and
`scheduler::tool_dispatch::fetch_screen` re-screens every `fetch_handoff` slice
served from the handoff cache (the cache holds the full body, but `tool_host`
only saw its first 64 KiB). On a `Block` verdict each substitutes a placeholder
carrying a human-readable `note` string — the only field the planner-summary
render surfaces — so the planner gets an intelligible *"content withheld"*
signal rather than a silent gap that would tempt it to re-run the step (#340).

**Known limitation (not a regression):** screening operates on one slice at a
time. An injection payload deliberately split across a 64 KiB boundary — or
across two `fetch_handoff` slices — can have each fragment fall below the
catalogue's per-slice threshold and evade the screen, the same way it could
already evade `tool_host`'s `SCAN_BYTE_CAP` window. This is inherent to
streaming, bounded-memory screening; the OS sandbox and the egress proxy remain
the actual containment boundary, with injection screening as defense-in-depth
that lowers attempt volume rather than a guarantee. Cross-slice stateful
screening is a possible future hardening.

### Network egress: the force-routed proxy boundary (and its residual risks)

Every networked worker egresses through a sandboxed per-worker CONNECT proxy
([`workers/egress-proxy`](../workers/egress-proxy/)). All four slices are
built, and force-routing is **on by default** in the supervised deployment:

- **Slice #1 — boundary allowlist + SSRF/DNS defence.** The proxy matches every
  request against the admin-controlled allowlist at the `host:port` *endpoint*
  level (#241; a bare-host, port-unconstrained grant is flagged distinctly in
  `audit_log`), resolves DNS *itself*, rejects
  private/loopback/link-local/ULA/CGNAT/multicast resolved IPs (with a
  literal-IP carve-out for an operator-allowlisted address such as a local
  SearxNG `127.0.0.1`), **pins** the surviving IP, dials it, and audits every
  decision. The SSRF predicate is `kastellan-net-classify::is_denied_range`,
  shared by every consumer so the definitions cannot drift.
- **Slice #2 — unbypassable force-routing, the default live path.** Under
  `KASTELLAN_EGRESS_FORCE_ROUTING=1` (the supervised deployment's default) the
  kernel does the enforcing, not the worker: a `Net::Allowlist` worker is
  placed in a **private network namespace** on Linux (`bwrap`: `--unshare-all`
  minus `--share-net`, the proxy UDS bind-mounted in — AF_UNIX is
  mount-ns-scoped, not net-ns) or behind a **deny-all-outbound-except-the-UDS**
  Seatbelt filter on macOS (gated by the on-host `seatbelt_uds_probe.rs`; a
  host that can't prove AF_INET is denied falls back to the `MacosContainer`
  VM-netns backend). The worker has *no direct route*; its only egress is
  `CONNECT host:port` to the proxy over the UDS (`web-common::ProxyConnectGet`).
  Host-side wiring is
  [`core/src/worker_lifecycle/force_route.rs`](../core/src/worker_lifecycle/force_route.rs)
  + [`core/src/egress/spawn.rs`](../core/src/egress/spawn.rs) — sidecar-first,
  **fail-closed** (no proxy ⇒ no worker), 1:1 teardown, decision-ingest →
  `audit_log`.
- **Slice #3 — TLS intercept + credential-leak scan.** The proxy MITMs the
  worker's TLS with a per-spawn CA the worker trusts, scans the cleartext for
  credential leaks, and re-originates TLS upstream itself.
- **Slice #4 — SPKI cert pinning.** `KASTELLAN_EGRESS_PROXY_PINS` pins upstream
  origins by SPKI hash (`PinningVerifier`, on the re-origination leg). No pins
  are provisioned by default; unset means standard webpki validation.

The `web-fetch` worker's *self-enforced* host allowlist (require `https`, match
the request host and every redirect hop against the admin-controlled list from
`tool_allowlists`, refuse off-list with `POLICY_DENIED`) is retained as
**defence-in-depth layer 2** behind the proxy. It matches host *names*, not
resolved IPs — IP-level containment (SSRF, DNS rebinding) is the proxy's job:
self-resolved DNS + `is_denied_range` + IP pinning at the boundary.

**Upstream extra-CA (operator config, single private origin).** A private
origin serving a self-signed / private-CA cert is unreachable through the
proxy's webpki-only upstream leg until the operator sets
`KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA={"<private-ip-literal>":"/abs/ca.pem"}`
([`core/src/egress/upstream_ca.rs`](../core/src/egress/upstream_ca.rs)). The
trust-scope rule is **enforced, not documented**: an anchor is handed to a
sidecar only when that worker's allowlist resolves to a **single private
origin written as an IP literal** (privateness decided by the same
`is_denied_range`), and a refusal **fails the spawn**; PEMs are read at daemon
startup, fail-closed. Known limitation: keying is per-**host**, not
per-`host:port`, so two private services sharing one address share an anchor —
give co-located private services distinct addresses.

**`browser-driver` exception — loopback shim, and a macOS-only caveat (#286).**
A headless Chromium cannot speak `CONNECT`-over-UDS, so the browser reaches its
sidecar via an in-jail loopback-TCP↔UDS shim (`shim.py`) and is pointed at it
with `--proxy-server=127.0.0.1:<port>` + `--proxy-bypass-list=<-loopback>` (the
bypass removal forces *even loopback* destinations through the sidecar). On
**Linux** this is fully contained: the private netns isolates loopback to the
worker, so the shim is its only loopback peer. On **macOS** there is no netns —
Seatbelt's `localhost:*` widening (`Profile::WorkerBrowserClient` + `proxy_uds`)
is the *host's* loopback, so a *compromised* browser worker could open a raw
socket to a host-local service (e.g. Postgres) directly, bypassing the sidecar.
This is latent today (Chromium is proxy-routed; macOS render is anyway blocked
by the pre-existing #284) but is a real Linux/macOS guarantee divergence —
tracked in **#286** (scope the rule to the shim's bound port, use a UDS-only
transport, or route the browser through the `MacosContainer` VM-netns backend).

**Residual risks, stated honestly.** The credential-leak scan sees only
cleartext crossing the proxy and matches **verbatim contiguous bytes** — an
encoded, split, or worker-side-encrypted secret evades it. TLS intercept
applies only where the worker trusts the proxy CA: the browser-driver's
Chromium and the `email-in` channel worker ride their sidecars as
**transparent tunnels** (no MITM) — still force-routed and
allowlist/SSRF-enforced, but their TLS payload is not inspected, and
`KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` has no effect on the email channel's
tunnel (a known slice-3 gap; the `mail` *tool* worker's MITM'd sidecar is
covered). `Net::ProxyEgress` is the policy variant the proxy itself runs
under.

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
2. **Peer authentication (pairing).** Built (slice #3): a `DbPeerAuthorizer` gates the bus on an
   active `(channel, peer)` row in the `pairings` table (fail-closed on any DB error). A new peer
   pairs by presenting a **single-use, short-lived, operator-issued code** (`kastellan-cli pair
   issue`, hash-only storage); the bus's pairing carve-out is the **only** path that touches
   unpaired input, and it is **compare-only** — it matches the body's SHA-256 against an active code,
   never enqueues/echoes it — gated on the operator having minted a code (`any_active_code`), atomic
   single-use (`claim_code`), and audited (`channel.paired` / `channel.rejected_unpaired`). Revoke is
   operator-only (admin UPDATE; runtime is REVOKEd). WebAuthn is deferred (no browser/CLI client
   surface yet). Matrix device cross-signing reinforces it channel-natively (slice #2 Phase D).
3. **Untrusted-input screening + audit.** Every inbound channel message is screened by
   `cassandra::injection_guard` exactly like worker output — a channel peer is no more trusted
   than a fetched web page — and every inbound/outbound message lands in `audit_log`.

**Channel-worker network containment:** each channel client — the sandboxed Matrix worker and
the `email-in` poller (which speaks to a localmail `/v1` endpoint; no IMAP client and no mail
credentials inside a kastellan jail) — runs under `Net::Allowlist` scoped to only its configured
server endpoint(s), force-routed through the per-worker egress proxy, so a compromised channel
worker reaches its one server and nothing else.

**Homeserver hosting blast radius (Tiers B/C).** Co-hosting conduwuit on the WireGuard/ingress
VPS (Tier B) or on the kastellan host (Tier C, "poor man's") places the larger public-facing
surface adjacent to, respectively, the network tunnel into the home/DGX network or the agent's
own user/Postgres/scratch/vault. A homeserver RCE then has shared-host adjacency to those
assets. Tier A (a dedicated VPS) is preferred for this reason; Tiers B/C require systemd
hardening (dedicated unprivileged user, `NoNewPrivileges`/`ProtectSystem=strict`/tight
`SystemCallFilter`, loopback-bound behind a TLS reverse proxy, no federation port) as the
minimum bar — defense-in-depth that reduces but does not eliminate shared-host blast radius.
**Email is the fallback because Matrix has no single-user homeserver failover** — redundancy is
cross-transport, not a second homeserver. Email is treated as **low-trust** (spoofable), so its
inbound path (slice 1, shipped) is gated twice before a message can become a task: the DMARC gate
(an `Authentication-Results` header written by the operator's own MX, authserv-id exact-match)
plus the per-pairing in-body token, with `DbPeerAuthorizer` enforcing that evidence at the
ChannelBus authorization chokepoint — the same path Matrix tasks take. The channel is config-gated
off by default (`KASTELLAN_EMAIL_*` unset ⇒ channel absent; a misconfiguration disables it loudly
while the daemon keeps running). **Outbound email does not exist yet** (slice 2, SMTP):
`EmailChannel::send` refuses unconditionally and the refusal is audited as
`channel.reply_undelivered` — today email can carry (heavily gated) commands but cannot yet
deliver notifications or replies.

## Negative tests (CI-enforced)

- `python-exec` attempts `socket.connect` → blocked.
- `shell-exec` attempts a non-allowlisted argv → rejected before spawn.
- `browser-driver` attempts to read `~/.ssh/` → blocked by sandbox.
- Adversarial web page in agent context tries to exfiltrate via `web-fetch` → request blocked, audit log shows attempt.
- `channel`: a message from an **unpaired** peer → dropped (never enqueued as a task), audit row `channel.rejected_unpaired`. (Shipped: `core/src/channel` `handle_inbound` + the hermetic/PG e2e; the unpaired peer's body is never even screened/echoed — authorize-before-screen.)
- `channel`: an inbound message carrying a catalogued prompt-injection → blocked (never enqueued), audit row `channel.injection_blocked` carrying only the SHA-256 + reason codes (never the body). (Shipped: `classify_inbound` under `GuardProfile::Strict`.)
- `channel` (Matrix): an inbound message from a peer **not** in `KASTELLAN_MATRIX_PEERS` → dropped, no task enqueued, no reply sent. (Shipped: `core/tests/matrix_channel_e2e.rs::unpaired_inbound_is_dropped_no_reply` — a real worker process driven through `MatrixChannel` + the bus, hermetic; the live sandboxed matrix-rust-sdk client + egress routing is slice #2 Phase D.)
- `channel`: a message whose **channel, peer or conversation id holds a NUL** (U+0000, which Postgres cannot store) → refused **before authorization** with one `channel.rejected_malformed` row (channel, peer, a fixed `field` label, `reason: "nul"` — never the body); a NUL in the **body** is escaped to `␀` and the message carries on, the count recorded as `nul_escaped_body` on its `channel.received` / `channel.injection_blocked` row. Escaping never applies to an id: a reply is routed and a pairing matched by them (#818). (Shipped: `core/src/channel/inbound_nul.rs` + its `handle_inbound` tests; the db-side rule — escape records, refuse identities — is `kastellan_db::nul`, pinned live by `db/tests/nul_non_audit_e2e.rs`.)
- `channel` pairing (slice #3): with **no active code**, an unpaired peer's message is dropped (`channel.rejected_unpaired`), the carve-out inert. With an active code, a **wrong** body is dropped (`channel.rejected_unpaired`) and **never enqueued/echoed** (compare-only). A **correct** code binds the peer (`pairings` row + `channel.paired`), consumes the code single-use (`claim_code` atomic UPDATE), and returns only a fixed ack — the code body itself never reaches the agent. (Shipped: `bus::handle_inbound` carve-out tests + `db::pairings` PG e2e single-use claim.)

Already shipped:

- `sandbox/tests/linux_smoke.rs` — bwrap denies `/etc/passwd`, `/home`, network under `Net::Deny`.
- `core/tests/shell_exec_e2e.rs` — non-allowlisted argv rejected by worker policy with `POLICY_DENIED`; full round-trip through bwrap + Landlock + seccomp.
- `workers/prelude/tests/landlock_smoke.rs` — write to non-allowlisted path is denied with EACCES; allowlisted scratch writes succeed; reads under `/usr` continue to work.
- `workers/prelude/tests/seccomp_smoke.rs` — `unshare(CLONE_NEWUSER)` and `mount(...)` are killed with `SIGSYS`; `getpid()` survives.
- `sandbox/tests/macos_smoke.rs` — Seatbelt denies `/etc/master.passwd`, `/Users/...`, raw `/dev/disk0`, and network under `Net::Deny`. Also: a worker calling `bootstrap_look_up("com.apple.coreservices.appleevents")` is denied (`worker_cannot_look_up_arbitrary_mach_services`, issue #1) — closes the largest pre-existing asymmetry vs the threat-model invariant; and the worker process is the leader of a fresh session, so any future attempt to open `/dev/tty` fails with ENXIO regardless of profile broadening (`worker_runs_in_its_own_session`, issue #2).
- `sandbox/tests/linux_smoke.rs::worker_with_low_mem_max_is_oom_killed` — a worker that allocates 256 MiB under `MemoryMax=32M` is OOM-killed by the kernel. Closes the cgroup-resource layer.
- `web-fetch` attempts a non-allowlisted host → blocked at the egress proxy boundary (`workers/egress-proxy` `decide_blocks_off_allowlist` / `handle_conn_reports_block_for_off_allowlist`), with the worker's own layer-2 refusal pinned by `core/tests/web_fetch_e2e.rs::host_outside_allowlist_is_denied`.

### Memory-write injection: the agent-raised L1 path

Adversary #6's "less-trusted code path" exists today: the planner's own
`l1_insight` is persisted to `memories` at layer 1 and rendered into every later
task's `<l1_insights>` block. Since the 2026-09-02 audit (H2) an agent-raised
body is screened by the strict injection catalogue at promotion (refused and
audited as `l1.injection_blocked`) and every L1 row is screened again at
prompt-assembly time, alongside the framing escape the July audit added; the
planner prompt states that these blocks are data, not instructions. Operator
rows (`memory l1 add`) are trusted at write time but still pass the load-time
screen.

### Shared-host adjacency: per-spawn directories

Every per-spawn directory the daemon mints under the temp root (egress
sidecar scratch, broker scratch, the Matrix channel's, a Firecracker run dir)
is created **exclusively** with mode `0700` and verified owner-private
(`kastellan_sandbox::private_dir`), and the secret-bearing files inside them
(`secret_hashes.json`, `fc.json`, VM images) are created `O_EXCL` `0600`. A
pre-planted name from another local uid fails the spawn closed instead of
being adopted — the name predictability itself is kept for the orphan sweeps
and costs an attacker at most one failed spawn of ours.

### Trusted sidecars (brokers) — an open trust assumption

`embed-broker` and `search-broker` run under `Net::Allowlist` with no OS-level
enforcement (bwrap `--share-net`) and are not force-routed through an egress
sidecar. A broker compromised through a malicious embedding or search response
reaches the whole host network. This is a documented gap pending its own
egress-sidecar slice; until then the brokers' backends are trusted inputs.

## Open items

- Whether `python-exec` should default to micro-VM rather than seccomp/Seatbelt-only.
- Concrete `setrlimit` budgets per worker class (macOS has only `RLIMIT_CPU`
  worker-side; Seatbelt enforces no memory/pids cap).
- Force-routing (`KASTELLAN_EGRESS_FORCE_ROUTING`) is opt-in; making it the
  default with an explicit opt-out is recommended before release.
- Route the brokers through egress sidecars (above).
- Bind `secret://` refs to an allowed-tool set at `materialize`.
