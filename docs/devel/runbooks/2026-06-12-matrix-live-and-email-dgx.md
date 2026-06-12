# Runbook — Matrix slice #2 Phase D (live) + email fallback (slice #5)

**Audience:** a session on the DGX (or any box with a Matrix homeserver + outbound
network), finishing the parts of comms slices #2 and #5 that **cannot be verified
in a hermetic CI container** (real `matrix-rust-sdk` networking + E2E; real
IMAP/SMTP). The hermetic substrate (#1 bus, #2 A–C+E, #3 pairing, #4 outbound
mapping, #6 homeserver infra) is already merged.

**Read first:**
[`docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md`](../../superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md),
[`docs/superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md`](../../superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md)
(Phase D tasks), [`docs/deploy/matrix-homeserver.md`](../../deploy/matrix-homeserver.md).

---

## Part A — Matrix slice #2 Phase D (the live worker + daemon wiring)

### A0. Prerequisites
- A conduwuit homeserver up (per `docs/deploy/matrix-homeserver.md`), federation
  OFF, with two accounts: your operator account and `@kastellan:<server>`.
- The bot's **access token** stored as a kastellan secret (`db::secrets`) so the
  daemon can materialize it via the `Vault` (never an env plaintext in prod).
- Build with the worker's `live-matrix` feature for the steps below.

### A1. SPIKE FIRST — matrix-rust-sdk through the MITM egress proxy (top risk)
The egress proxy MITM-terminates worker TLS (slice #3a) and the worker must trust
**only** the per-instance CA. Determine whether `matrix-rust-sdk`'s HTTP client
accepts **both**: a custom root CA **and** a CONNECT-over-UDS proxy.
- Try: point the SDK's reqwest/rustls client at a `RootCertStore` built from
  `KASTELLAN_EGRESS_PROXY_CA` + the UDS proxy (mirror `web-common::ProxyConnectGet`).
- **If it works:** proceed — the worker is force-routed like web-fetch.
- **If it doesn't cleanly:** take the documented fallback — add the homeserver to
  a slice-#3 **MITM-bypass pin** so the proxy still does allowlist + SSRF + IP-pin
  but does **not** TLS-intercept the homeserver (which is already trusted infra).
  This is acceptable and keeps containment (the worker still reaches only the
  homeserver, via the proxy). Decide before writing the sync loop.

### A2. Dependency + license
- Add `matrix-sdk` (SQLite store, rustls) under `workers/matrix` `[features]
  live-matrix`. Run the license pass on the new subtree (`cargo deny` / manual) —
  block any non-AGPL-compatible license (CDDL/BUSL/SSPL/Elastic/source-available).

### A3. `workers/matrix/src/sdk_live.rs` — `LiveSdk: MatrixSdk`
- Hold a tokio `Runtime`; at construction `block_on`: open the **persistent
  encrypted** SQLite store in the bind-mounted state dir (passphrase from the
  Vault), **log in** (token from env, injected by the spawn), and do the first
  sync. Network here flows through the proxy UDS (so the sidecar must be up first).
- Spawn the sync task: decrypt room **text** events → `push_bounded` into the
  bounded `Mutex<VecDeque<Event>>` shared with the handler.
- Implement `identity()` / `poll(timeout_ms)` (drain + bounded wait) / `send()`
  (E2E room send) — the seam the already-merged `MatrixHandler` dispatches to.

### A4. `workers/matrix/src/main.rs` (live-matrix arm — already stubbed)
- Order: build `LiveSdk::from_env()` (network — needs the proxy UDS up) → THEN
  `kastellan_worker_prelude::lock_down()` → THEN `serve_stdio(MatrixHandler)`.
  Mirrors the egress proxy's "do the network-needing init, then lock down".
- Confirm the post-lockdown sync task does no NEW disallowed syscalls under the
  `WorkerNetClient` seccomp profile (its sockets are already open; new connections
  go via the proxy UDS, which the profile permits — like web-fetch).

### A5. The long-lived sandboxed spawn (finish `core::channel::matrix`)
- Implement `spawn_matrix_worker(pool, vault, exe_dir, force_routing)`:
  - resolve the worker binary (sibling of `kastellan`);
  - materialize secrets (homeserver URL, user, access token, store passphrase,
    E2E recovery key) from the `Vault` → jail env;
  - `build_matrix_policy(bin, homeserver_host, persistent_store_dir, proxy_uds,
    egress_ca)` (already implemented, pure + tested);
  - **spawn the egress sidecar first** (`egress::spawn_sidecar`, wait for
    `ca.pem`), `rewrite_worker_policy` to bind the CA + set the env, then
    `spawn_worker_client(backend, &policy, program, args)` (already implemented)
    → `MatrixChannel::new`;
  - own a guard that tears down sidecar + worker; add restart-backoff supervision
    (reuse `RestartBackoff` shape) — a crashed channel worker is respawned.
  - **Persistent** store dir (`~/.local/state/kastellan/matrix/store`), NOT the
    per-task ephemeral scratch (E2E keys must survive restarts).

### A6. `main.rs` daemon wiring (swap the seams in)
- `core::channel::matrix::from_env` → when `KASTELLAN_MATRIX_HOMESERVER` is set,
  spawn the channel (A5) and return it + the pairing pieces.
- In `main.rs`, replace the slice-#4 placeholder log block with:
  - `let authorizer = Arc::new(DbPeerAuthorizer::new(pool.clone()));`
  - `let pairing = Arc::new(DbPairingService::new(pool.clone()));`
  - `let events = Arc::new(PgChannelEvents::new(pool.clone()));`
  - `let completed = Box::new(PgCompletedTasks::connect(pool.clone()).await?);`
  - `let bus = ChannelBus::spawn(vec![Box::new(channel)], authorizer, Some(pairing), events, completed);`
  - add `bus.shutdown().await` + the spawn guard teardown to the graceful-shutdown
    sequence (before the scheduler), and gate the whole block so the absent-config
    path stays byte-identical.

### A7. Tests + acceptance (on the DGX)
- `core/tests/matrix_live_e2e.rs` `#[ignore]`: real login + E2E + send/recv round
  trip against the local conduwuit.
- Manual acceptance: from Element as your **paired operator** account, message the
  bot → the agent replies in the same room (verify the reply is the agent's answer
  text, not JSON — slice #4). Then: an **unpaired** account's message is dropped
  (`channel.rejected_unpaired`, no reply); `kastellan-cli pair issue` → send the
  code from a new account → `channel.paired` + ack; a catalogued injection →
  `channel.injection_blocked`.
- Update HANDOVER + ROADMAP ("Matrix inbound"/"Matrix outbound" → `[x]`), flip the
  `live-matrix` build on in the deployment, and record the A1 spike outcome.

---

## Part B — Email fallback (slice #5)

**Design note (needs a short spec before coding):** email is the **low-trust,
cross-transport fallback** — "notifications, never commands"
([primary-communication-channel-design](../../superpowers/specs/2026-06-12-primary-communication-channel-design.md)).
That makes it structurally different from Matrix: email **inbound must never
enqueue an agent task**. So slice #5 is primarily **outbound**, with inbound (if
any) tightly gated and surfaced to the human only.

### B1. SMTP outbound first (the useful core)
- New worker `workers/email` (or extend a mail worker), sandboxed `Net::Allowlist`
  = the configured SMTP server only, egress-routed. `lettre` (MIT/Apache) for SMTP.
- Wire it as a second `Channel` whose `send()` emails the operator; its `recv()`
  initially returns `None`/parks (outbound-only). Use it as the fallback delivery
  for `reply_for_completed_task` output when Matrix is unavailable (the bus can
  fan a reply to a secondary channel on primary failure — a small policy addition).
- This gives "kastellan can still reach me when the Matrix box is down" — the
  redundancy the whole design hinges on.

### B2. IMAP inbound (optional, later — its own spec)
- If inbound is wanted: IMAP worker, `Net::Allowlist` = the IMAP server only.
- **Never enqueue.** Require SPF/DKIM/DMARC pass **and** a per-pairing in-body
  token; on pass, surface the message to the **operator** (e.g. forward into the
  Matrix room) as a notification — the agent never acts on it. Spoofability is why
  email is never a command path. Write a `2026-..-email-fallback-design.md` spec
  resolving the inbound-gating + "surface, don't action" semantics before coding
  this half.

### B3. Tests
- Hermetic: SMTP send via a fake transport (mirror web-common's `HttpGet`/`FakeGet`
  seam); the bus fan-to-fallback policy unit-tested with fakes.
- `#[ignore]` live: real SMTP send to a test mailbox; (if B2) real IMAP fetch +
  the SPF/DKIM/DMARC + token gate.

---

## Quick reference — what's already done (don't redo)
- Bus + four seams (`Channel`/`PeerAuthorizer`/`ChannelEvents`/`CompletedTasks`),
  pairing carve-out (`PairingService`), `DbPeerAuthorizer`, `DbPairingService`,
  `db::pairings`, `pair` CLI, `MatrixChannel` + driver + `ProtocolWorkerClient` +
  `spawn_worker_client` + `build_matrix_policy`, the matrix worker handler + SDK
  seam + wire crate, `reply_body` (real completion shape), the conduwuit infra.
- Only the **live SDK networking**, the **egress-coupled persistent spawn +
  supervision**, the **daemon wiring**, and **email** remain.
