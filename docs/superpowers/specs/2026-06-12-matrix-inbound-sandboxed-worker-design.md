# Comms slice #2 — Matrix inbound via a sandboxed worker

**Date:** 2026-06-12
**Status:** Design approved (operator: sandboxed-worker architecture, spec+plan first); implementation plan pending → built.
**ROADMAP:** Phase 2 — Channels (read-only): "Matrix inbound" + "Homeserver supervisor unit" (overlaps slice #6).
**Depends on:** comms slice #1 (the channel-bus abstraction — `core/src/channel`, the four seams) — MERGED on `claude/zen-bell-6bn2ze`.
**Builds on:** the egress proxy (slices #1–#3, force-routing ON by default), the worker prelude (Landlock+seccomp), `kastellan-protocol` (stdio JSON-RPC), `tool_host::spawn_worker`.

---

## Problem

Slice #1 shipped the transport-agnostic channel bus (the `Channel` trait + fail-closed
authorize→screen→enqueue inbound path + finalized-task→reply outbound path + the `ChannelBus`
runtime), proven with a `FakeChannel`. It deliberately shipped **no real transport**. Slice #2
delivers the first real one: **Matrix**, the decided primary channel (E2E, self-hosted,
vendor-neutral — `docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`).

A Matrix client parses untrusted bytes from **two** untrusted sources — the homeserver and message
peers — and performs E2E cryptography. Per the project's hard constraints ("every worker is
sandboxed before it runs"; "the core never executes untrusted code in-process") and the
threat-model invariant (a worst-case compromise reaches *at most* the compromised tool's own user +
scratch + allowlisted endpoints), the Matrix client **must run in a sandboxed worker**, not in the
core. A client-side RCE is then contained to the worker's own OS user, its persistent E2E store, and
its one allowlisted endpoint (the homeserver) — never the core's DB, secrets vault, or memory.

## Goals (slice #2)

1. A new sandboxed worker crate **`workers/matrix`** (`kastellan-worker-matrix`) wrapping
   `matrix-rust-sdk`: logs in as the bot account, runs the E2E sync loop, **buffers decrypted
   inbound messages**, and serves three synchronous JSON-RPC methods over stdio
   (`matrix.init` / `matrix.poll` / `matrix.send`) via the prelude's `serve_stdio` + `lock_down`.
2. A core-side **`MatrixChannel`** implementing the slice-#1 `Channel` trait, driving the worker
   over the synchronous `kastellan-protocol` `Client` from a dedicated blocking driver thread,
   bridged to the async `recv()`/`send()` surface via tokio mpsc.
3. The worker is **force-routed through the egress proxy** (`Net::Allowlist` = homeserver host:port
   only + `proxy_uds`), with a **persistent** (not per-task-ephemeral) E2E store dir bind-mounted
   writable into the jail. Login credentials + the E2E recovery key come from `db::secrets` via the
   `Vault`, injected into the jail env at spawn (never logged).
4. **`main.rs` wiring**: when Matrix is configured, the daemon spawns the `MatrixChannel` + the
   `ChannelBus` (with a `StaticPairings` authorizer from operator config) alongside the scheduler;
   absent config ⇒ the daemon is byte-identical to today (no bus spawned).
5. A **dev homeserver setup script** (local conduwuit, federation-off) so the live round-trip is
   exercisable; the hermetic core path is proven without any homeserver.

## Non-goals (this slice)

- **Outbound polish / agent "final message" convention.** Slice #2 routes the slice-#1 `reply_body`
  (compact-JSON fallback) back out; the richer agent-side final-message field is **slice #4**.
- **Pairing handshake.** Recognised peers come from `StaticPairings` (operator config) here; the
  TOTP/HOTP/WebAuthn flow that *adds* peers + the DB-backed `PeerAuthorizer` is **slice #3**.
- **Email fallback.** **Slice #5.**
- **Production homeserver supervisor unit + hardening.** The conduwuit *systemd/launchd* unit +
  Tier A/B/C install path is **slice #6**; slice #2 ships only a **dev** setup script for the
  `#[ignore]` live test.
- **Rich media / attachments.** Text messages only; attachments are an injection surface deferred.

## Architecture

### Why a long-lived worker breaks the request/response mould (and how we keep the protocol pure)

`kastellan-protocol::Client` is **synchronous, blocking, one-request-at-a-time, strict
request→response** (no server-initiated notifications — `client.rs`). A Matrix client, by contrast,
must *push* inbound events to the core unprompted. We reconcile this **without changing the protocol
or the trust boundary** by keeping the worker a pure JSON-RPC *server* and moving the streaming
concern into the worker's internals + the core-side driver:

- **Worker internals (async, private):** the worker holds a tokio runtime; `matrix-rust-sdk`'s sync
  loop runs as a spawned task that **decrypts and buffers** inbound text messages into an in-process
  bounded `VecDeque` (drop-oldest past a cap, with a counter). The JSON-RPC surface stays a *sync*
  `serve_stdio` handler that `runtime.block_on`s the small SDK calls it needs.
- **Three methods (all request→response):**
  - `matrix.init {}` → `{ user_id, device_id }` — confirms login + E2E ready (idempotent; the worker
    actually logs in at startup before `lock_down`, see "Startup ordering").
  - `matrix.poll { timeout_ms }` → `{ events: [{conversation, peer, body}] }` — returns currently
    buffered events, **long-polling up to `timeout_ms`** if the buffer is empty. The worker removes
    returned events from its buffer only *after* writing the response line (best-effort at-most-once;
    a single-user low-volume channel makes the cancellation-loss window negligible — see "Open
    points" for the optional `ack` upgrade).
  - `matrix.send { conversation, body }` → `{ ok: true }` — sends an E2E message to a room.
- **Core-side driver (`MatrixChannel`):** a dedicated **blocking thread** (`spawn_blocking` /
  `std::thread`) owns the blocking `Client`. Its loop, each iteration:
  1. drain a `std::sync::mpsc` of pending outbound messages → `client.call("matrix.send", …)`;
  2. `client.call("matrix.poll", { timeout_ms: POLL_MS })` → push each event into an async
     `tokio::mpsc::Sender<IncomingMessage>`.
  The trait surface is then trivial + cancellation-safe: `recv()` = `inbound_rx.recv().await`
  (buffered in the channel, so a cancelled `recv` future loses nothing — the bus's `select!` can
  drop it freely); `send()` = `outbound_tx.send(msg)` (just queues; the driver delivers it within
  one poll cycle, ≤ `POLL_MS`). Outbound latency is bounded by `POLL_MS` (a few seconds — fine for a
  single-user assistant; tunable).

This is the crux decision: **the synchronous request/response protocol is preserved; all
concurrency lives in the core-side driver thread + the worker's internal queue.** No
server-initiated notifications, no protocol change, no second pipe.

### Sandbox + egress (containment)

The worker is spawned **sandboxed + force-routed**, reusing the egress coupling that already exists
for net workers — but with two differences from the per-step net workers:

- **Persistent state dir, not ephemeral scratch.** `matrix-rust-sdk` persists the E2E store
  (device keys, cross-signing, sync token) to disk; losing it every restart would re-bootstrap E2E
  and break device trust. So the worker gets a **persistent** `fs_write` dir
  (`~/.local/state/kastellan/matrix/store`, bind-mounted writable into the jail) instead of the
  per-task RAII scratch. The egress sidecar's UDS + the worker's runtime scratch may still be
  ephemeral; only the SDK store persists.
- **Long-lived, restart-on-crash.** Unlike `SingleUse` step workers, the channel worker runs for the
  daemon's lifetime. It is spawned once at bring-up and supervised with restart-backoff (reuse the
  `RestartBackoff` shape); it is **not** in the `ToolRegistry` and never goes through
  `tool_host::dispatch` (it's a channel, not a per-step tool).

Policy: `Net::Allowlist([<homeserver_host>:443])` + `proxy_uds` (force-routed; the worker reaches
the homeserver only via the egress proxy, which enforces host:port + SSRF + TLS-intercept) +
`Profile::WorkerNetClient` + `fs_read` = [binary, `/etc/{resolv.conf,hosts,nsswitch.conf}`, the
egress CA] + `fs_write` = [the persistent store dir]. Cross-platform via the existing
`SandboxBackend` seam (bwrap / Seatbelt / container).

> **Egress interaction note.** The egress proxy MITM-terminates the worker's TLS (slice #3a) and the
> worker trusts only the per-instance CA. matrix-rust-sdk must therefore be pointed at that CA
> (rustls `RootCertStore` from `KASTELLAN_EGRESS_PROXY_CA`, mirroring `web-common::ProxyConnectGet`).
> Confirm at plan time that the SDK's reqwest/rustls client accepts a custom root + the CONNECT-over-
> UDS proxy, OR (fallback) scope the homeserver into the slice-#3 pin so MITM is bypassed for it.
> This is the highest-risk integration unknown — flagged in "Open points".

### Secrets (login + E2E recovery)

The bot's homeserver credentials and the E2E recovery key are kastellan secrets (`db::secrets`,
AES-256-GCM at rest). The **core** materializes them from the `Vault` (core-only-DB) and injects them
into the worker's jail env at spawn (`--setenv`, the same path `KASTELLAN_WEB_SEARCH_ENDPOINT` uses):
`KASTELLAN_MATRIX_HOMESERVER`, `KASTELLAN_MATRIX_USER`, and a `secret://`-redeemed access token +
recovery key. Plaintext-in-jail-env is acceptable (the worker is the authorized consumer; the env is
set only into that one jail). The audit redaction invariant (#147) already keeps the ref string out
of audit rows.

### Startup ordering (E2E before lockdown)

matrix-rust-sdk needs network (login + initial sync) and writes its store. The worker therefore:
1. reads env, opens/initializes the SDK store in the persistent dir, **logs in + does the first
   sync** (network needed — this happens *through the egress proxy UDS*, so the proxy must be up
   first: the core spawns the sidecar, waits for `ca.pem`, then the worker);
2. **then** calls the prelude `lock_down` (Landlock RW on the store dir + RO on the CA/resolver +
   the `WorkerNetClient` seccomp profile) and enters `serve_stdio`.
This mirrors the egress proxy's "generate CA before lock_down" ordering. Confirm the SDK does no
fresh `socket()`/`openat()` outside the locked-down set *after* `serve_stdio` begins (the sync task
keeps running post-lockdown — its sockets are already open; new connections go via the proxy UDS,
which `WorkerNetClient` permits as it does for `web-fetch`).

### main.rs wiring

A new `core::channel::matrix::from_env(...)` builds the `MatrixChannel` (spawns sidecar + worker,
returns the `Channel`) when `KASTELLAN_MATRIX_HOMESERVER` is set; `None` otherwise. When present,
`main.rs` builds `StaticPairings` from `KASTELLAN_MATRIX_PEERS` (comma-separated recognised peer ids
— fail-closed: empty ⇒ deny all, logged), constructs `PgChannelEvents` + `PgCompletedTasks` over the
runtime pool, and `ChannelBus::spawn(...)`. The bus handle joins the graceful-shutdown sequence
(stop bus → scheduler → mirror → pool). Absent `KASTELLAN_MATRIX_HOMESERVER` ⇒ no bus, byte-identical
daemon.

## Data flow (one inbound message → reply)

```
peer (Element)  ──E2E──▶  homeserver (conduwuit, federation-off)
                              │  (sync)
                  egress proxy UDS (allowlist+SSRF+MITM)
                              │
                    kastellan-worker-matrix (sandboxed)
                      decrypt → buffer in VecDeque
                              ▲ matrix.poll          │ matrix.send ▼
            MatrixChannel driver thread (blocking Client)
                      inbound_tx │                   ▲ outbound_rx
                    ChannelBus per-channel task (slice #1)
        recv() → handle_inbound: authorize(StaticPairings) → injection_guard
                  → InboundDecision::Enqueue → insert_pending(tasks, kind:"channel")
                              │ (scheduler runs the task — UNCHANGED)
                  tasks_completed NOTIFY → PgCompletedTasks
                  → handle_completed → reply_for_completed_task → send()
                              ▼
            outbound_rx → driver → matrix.send → homeserver → peer
```

The slice-#1 security guarantees apply unchanged: unpaired peer → dropped + `channel.rejected_unpaired`;
injection → dropped + `channel.injection_blocked` (hash only); the scheduler/runner is untouched.

## Cross-platform

Worker is pure-Rust matrix-rust-sdk + tokio; sandbox via the existing per-OS `SandboxBackend`. The
persistent store dir + the egress coupling work identically on Linux (bwrap) and macOS
(Seatbelt/container). No OS-specific channel code. The dev homeserver script targets Linux (conduwuit
binary) with a macOS note (Docker/`container`).

## Dependencies (AGPL-compat — verify at plan time)

- **`matrix-rust-sdk`** (Apache-2.0) + its E2E stack `vodozemac` (Apache-2.0). Apache-2.0 is
  AGPL-compatible — OK. **Heavy dependency**: pulls a large async tree (tokio, reqwest, a store
  backend — prefer the SQLite store to avoid a second embedded-KV dep; confirm its license). Run the
  full `cargo deny`/license pass on the new subtree before merge (the hard constraint: block any
  CDDL/BUSL/SSPL/Elastic/"source-available").
- **conduwuit** (dev only, not a cargo dep) — operator infra; license confirmed in slice #6.

## Testing

- **Worker unit (`workers/matrix`)** — pure parts only: env parsing / config (`from_env` fail-closed
  on missing homeserver/user/token), the inbound-buffer drop-oldest cap, the JSON-RPC method
  dispatch + param validation + `METHOD_NOT_FOUND` (handler tested with a **fake SDK seam** — the
  matrix calls behind a trait so dispatch is unit-tested without a homeserver).
- **`MatrixChannel` driver unit (core)** — the driver loop over a **fake `Client` seam** (not the
  real worker): poll returns events → land on `recv()`; `send()` enqueues → fake records the
  `matrix.send` call; cancellation-safety (drop a `recv()` future mid-poll → next `recv()` still gets
  the buffered event). No worker process, no network.
- **`core` integration (`matrix_channel_e2e`)** — hermetic: spawn a **fake worker binary** (a tiny
  test stub speaking the three JSON-RPC methods, echoing a canned inbound + recording sends) under
  the real sandbox, drive it through `MatrixChannel` + the real `ChannelBus` with fake DB seams →
  assert the canned message round-trips to a recorded send. Proves the spawn + protocol + driver +
  bus integration **without matrix-rust-sdk or a homeserver** (mirrors egress slice #1's "prove with
  a test client" precedent).
- **`#[ignore]` live test** — against a local conduwuit (dev script): real login + E2E + send/recv a
  message end to end. Run on a box with the homeserver (DGX/dev), not CI.
- **Negative test (threat-model)** — an inbound message from a peer **not** in `KASTELLAN_MATRIX_PEERS`
  is dropped (no task), `channel.rejected_unpaired` row — exercised via the fake-worker e2e.

## Decomposition (tasks → the plan)

| # | Task | Verifiable here? |
|---|------|------------------|
| 1 | `workers/matrix` crate skeleton + JSON-RPC wire types (`PollResult`, `SendParams`, `Event`) + the SDK seam trait | yes (types + dispatch unit) |
| 2 | Worker handler (`matrix.init/poll/send` dispatch + buffer cap + fake-SDK unit tests) | yes |
| 3 | Worker `main.rs`: real matrix-rust-sdk impl of the SDK seam (login, sync loop, send) + startup-before-lockdown ordering | **no — DGX/homeserver** |
| 4 | Core `MatrixChannel` + blocking driver thread + the `Client` seam + driver unit tests | yes (fake Client) |
| 5 | Sandbox+egress spawn path for a long-lived channel worker (persistent store dir, sidecar coupling, restart-backoff) | partial (spawn shape unit; live = DGX) |
| 6 | `core::channel::matrix::from_env` + `main.rs` wiring (config-gated, byte-identical when absent) | yes (compile + absent-path) |
| 7 | Hermetic `matrix_channel_e2e` with a fake-worker stub binary | yes |
| 8 | Dev homeserver script (`scripts/matrix/setup-conduwuit.sh`) + `#[ignore]` live test | script yes; live = DGX |
| 9 | License pass on the matrix-rust-sdk subtree; docs (threat-model, ROADMAP, HANDOVER) | yes |

Per the operator decision, tasks 1–2, 4, 6–7, 9 (the hermetic + wiring parts) are implementable +
verifiable in any environment; tasks 3, 5(live), 8(live) need a homeserver and are built then
verified on the DGX/dev box (their live tests `#[ignore]`/skip-as-pass elsewhere).

## Open points (resolve in the plan / during impl)

- **matrix-rust-sdk through the MITM egress proxy.** Does the SDK's rustls client accept a custom
  root CA (the per-instance egress CA) + a CONNECT-over-UDS proxy? If not cleanly, fallback options:
  (a) add the homeserver to a slice-#3 MITM-bypass pin (proxy still does allowlist+SSRF, just no TLS
  interception for the homeserver — acceptable, the homeserver is already trusted infra); (b) give
  the SDK a direct route in a private netns with only the proxy... no — (a) is the clean fallback.
  **This is the biggest integration risk; spike it first in task 3.**
- **at-most-once vs exactly-once `poll`.** The drop-after-response window can lose an event if the
  poll response is lost on driver-thread death. Optional `matrix.ack {cursor}` upgrade (worker keeps
  events until acked) if loss is ever observed; not needed for v1 single-user.
- **Store encryption.** matrix-rust-sdk can encrypt its SQLite store with a passphrase — source it
  from the vault too, so the persistent store is encrypted at rest like everything else.
- **Restart + re-login.** On worker crash + restart, the persistent store should let it resume
  without re-bootstrapping E2E; confirm the SDK restores cross-signing from the store + recovery key.
- **conduwuit license + exact homeserver pin** — confirmed in slice #6; referenced here.

## What this slice leaves true

Slice #2 makes Matrix a **live, E2E, sandboxed** inbound channel wired into the running daemon, with
the client fully contained (a Matrix-client RCE reaches only the worker's user + its persistent store
+ the homeserver endpoint). It leaves for later: outbound richness (slice #4), the pairing handshake
that replaces `StaticPairings` (slice #3), the email fallback (slice #5), and the production
homeserver supervisor unit + Tier A/B/C hardening (slice #6). The slice-#1 seams are unchanged; this
slice is purely a new `Channel` implementation + its sandboxed worker + the daemon wiring.
