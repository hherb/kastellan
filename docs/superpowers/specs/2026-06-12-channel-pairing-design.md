# Comms slice #3 — DM pairing (in-channel single-use code) + DB-backed authorizer

**Date:** 2026-06-12
**Status:** Design approved (operator: in-channel code handshake; defer WebAuthn). Plan → build.
**ROADMAP:** Phase 2 — "DM pairing flow". **Depends on:** slice #1 (the channel bus + the
`PeerAuthorizer`/`StaticPairings` seam), slice #2 (the `MatrixChannel` consumer).
**Replaces:** `StaticPairings` (operator-config peer list) with a DB-backed authorizer + a real
pairing handshake.

---

## Problem

Slice #1 made the bus **fail-closed**: an unrecognised peer's message is dropped before any
processing. That leaves a chicken-and-egg: how does a *new* peer ever become recognised without
the operator hand-editing a config and restarting? The ROADMAP's answer: a **short-lived pairing
code issued via a separate trusted channel** (the operator's terminal), which the new peer presents
to the bot. "Static contact allowlists rejected (forgeable)" — a Matrix `@user:server` in a config
file is trivially spoofable as an identity claim *in the channel*; the pairing must prove the peer
controls an account the operator deliberately authorized, via a secret only the operator handed them.

## Decision

- **In-channel, single-use, short-lived code handshake.** Operator runs `kastellan-cli pair issue`
  → a random high-entropy code (printed once, stored only as a SHA-256 hash + TTL) → operator hands
  it to the new user out-of-band → the user sends it to the bot as a message → the bot binds that
  `(channel, peer)` in a `pairings` table and consumes the code.
- **A tightly-bounded carve-out** is the only place unpaired input is touched (security analysis
  below). Default (no active code) ⇒ **all unpaired messages dropped**, exactly as slice #1.
- **DB-backed `PeerAuthorizer`** (replaces `StaticPairings` in production): authorization is an
  async DB lookup keyed on `(channel, peer)` against active (non-revoked) pairings. No in-memory
  cache/NOTIFY — at single-user volume a query per inbound message is trivial, and it makes CLI
  revoke take effect immediately with zero cache-coherence code.
- **WebAuthn deferred** — no browser/CLI *client* channel exists to consume it yet (YAGNI). Added
  when such a surface appears.
- **"TOTP/HOTP" reading:** for a one-shot pairing a random single-use nonce + TTL is the correct,
  simpler primitive (TOTP/HOTP are for *repeated* auth). The code is a 160-bit random token,
  base32-ish, single-use, default 10-minute TTL.

## Security analysis of the carve-out (the load-bearing decision)

The carve-out lets an *unpaired* peer's message reach **one** narrow code path. It is safe because
that path:

1. **Only runs when the operator has an active, unconsumed, unexpired code.** No pending code ⇒ the
   carve-out is inert and the message is dropped + `channel.rejected_unpaired`, identical to slice #1.
2. **Compares, never interprets.** The body is SHA-256'd and compared (constant-time) against stored
   code hashes. It is **never** screened-into a task, never reaches the LLM/scheduler, never echoed.
   A non-matching body is dropped + audited; it cannot influence the agent.
3. **Single-use + TTL + rate-limited.** A code is consumed atomically on first match (a DB
   conditional UPDATE, so two racing claims can't both win); expired/consumed codes never match.
   Failed attempts are audited (`channel.pairing_failed`) and rate-limited per peer to blunt
   guessing (160-bit space makes guessing infeasible regardless; the limit caps audit-log spam).
4. **Binds the channel-native identity, not a claim.** The binding records the *actual*
   `(channel, peer)` the matched message came from — the peer proved control of that account by
   delivering a secret only the operator held. This is strictly stronger than a config allowlist.

The carve-out therefore does **not** weaken the "no unpaired input reaches the agent" guarantee; it
adds a separate, compare-only authentication path gated on explicit operator action.

## Schema (migration 0018)

```sql
-- pairings: the authorizer's source of truth.
CREATE TABLE pairings (
    id          BIGSERIAL PRIMARY KEY,
    channel     TEXT NOT NULL,
    peer        TEXT NOT NULL,
    method      TEXT NOT NULL DEFAULT 'code',
    paired_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ                 -- NULL = active
);
CREATE UNIQUE INDEX pairings_active_uniq ON pairings (channel, peer) WHERE revoked_at IS NULL;

-- pairing_codes: pending operator-issued codes (hash only; plaintext never stored).
CREATE TABLE pairing_codes (
    id          BIGSERIAL PRIMARY KEY,
    code_sha256 TEXT NOT NULL,
    label       TEXT,                       -- operator note (who it's for)
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,                -- NULL = still claimable
    consumed_by TEXT                        -- "<channel>/<peer>" that claimed it
);
CREATE INDEX pairing_codes_claimable ON pairing_codes (expires_at) WHERE consumed_at IS NULL;
```

**Grants** (least-privilege; the daemon's `kastellan_runtime` role vs the operator's admin
connection — mirrors the 0016/0017 REVOKE pattern, since 0002's `ALTER DEFAULT PRIVILEGES`
auto-grants full CRUD):
- `pairings`: runtime gets `SELECT, INSERT` (read for authz; insert on a successful code);
  **REVOKE UPDATE, DELETE, TRUNCATE** — revocation is operator-only (admin UPDATE).
- `pairing_codes`: runtime gets `SELECT, UPDATE` (find + atomically consume);
  **REVOKE INSERT, DELETE, TRUNCATE** — minting codes is operator-only (admin INSERT).

## Architecture

### `db::pairings` (typed helpers, the only SQL)

- `is_paired(executor, channel, peer) -> bool` (active, revoked_at IS NULL).
- `insert_pairing(executor, channel, peer, method) -> id` (ON CONFLICT on the active-unique index
  → no-op/idempotent).
- `revoke_pairing(admin_executor, channel, peer) -> bool` (set revoked_at; operator path).
- `list_pairings(executor, include_revoked) -> Vec<Pairing>`.
- `insert_code(admin_executor, code_sha256, label, ttl) -> id` (operator path).
- `claim_code(executor, code_sha256, by) -> bool` — the atomic single-use consume:
  `UPDATE pairing_codes SET consumed_at=now(), consumed_by=$by WHERE code_sha256=$h AND consumed_at IS NULL AND expires_at > now()` → `rows_affected == 1`. Constant-time compare is moot once we match
  on the *hash* (the hash lookup is exact); the plaintext→hash is done in the service.
- `any_active_code(executor) -> bool` — cheap gate so the carve-out is skipped when no code is pending.

### `PeerAuthorizer` becomes async + channel-scoped

Slice #1's `fn authorize(&self, peer) -> AuthDecision` becomes
`async fn authorize(&self, channel: &ChannelId, peer: &PeerId) -> AuthDecision`. Rationale:
authorization is genuinely a DB fact, and pairings are keyed `(channel, peer)`. `StaticPairings`
keeps working (async impl, peer-only match) for tests/legacy. New `DbPeerAuthorizer { pool }` runs
`db::pairings::is_paired`.

### `PairingService` seam + the bus carve-out

A new seam the bus consults **only** for authorizer-rejected peers:

```rust
#[async_trait] pub trait PairingService: Send + Sync {
    async fn try_pair(&self, channel: &ChannelId, peer: &PeerId, body: &str) -> PairingOutcome;
}
pub enum PairingOutcome { Paired, NotAPairingAttempt }
```

`DbPairingService { pool }::try_pair`: if `!any_active_code` → `NotAPairingAttempt` (fast inert
path); else SHA-256 the body, `claim_code(hash, "<channel>/<peer>")`; on success
`insert_pairing` + return `Paired`; else `NotAPairingAttempt`.

`bus::handle_inbound` is restructured (and now returns `Option<OutgoingMessage>` — an immediate
pairing-ack the per-channel task sends back via `ch.send`):

```
let auth = authorizer.authorize(&msg.channel, &msg.peer).await;
if auth == Rejected {
    match pairing { Some(p) => match p.try_pair(channel, peer, body).await {
        Paired => { audit channel.paired; return Some(reply "✓ paired — you can now message me"); }
        NotAPairingAttempt => {}
    }, None => {} }
    audit channel.rejected_unpaired (or channel.pairing_failed if a code was active but no match);
    return None;
}
match screen_and_classify(msg) {           // the pure slice-#1 screen→payload, authz removed
    Enqueue { payload } => { enqueue + audit channel.received }
    InjectionBlocked { .. } => { audit channel.injection_blocked }
}
None
```

`ingest.rs` is refactored: the pure `InboundDecision` drops `RejectUnpaired` (rejection is now an
authorizer concern handled in the bus); the pure part is `screen_and_classify(msg, screen) ->
{Enqueue | InjectionBlocked}`. `ChannelBus::spawn` gains an `Option<Arc<dyn PairingService>>` param;
the per-channel task sends any returned pairing-ack.

### CLI: `kastellan-cli pair {issue,list,revoke}`

Over `connect_admin_pool` (operator privilege), mirroring the `entities kinds`/`relations kinds`
CLIs:
- `pair issue [--label TEXT] [--ttl-mins N]` → generates a random code, stores its hash, **prints
  the plaintext once** with instructions; writes a `pairing.code_issued` audit row (hash + label +
  expiry, never the code).
- `pair list [--all]` → active (or all) pairings.
- `pair revoke <channel> <peer>` → `revoke_pairing`; `pairing.revoked` audit row. Takes effect on
  the next message (no cache).

## Verifiable here vs PG-gated

- **Verifiable anywhere:** the pure `screen_and_classify` refactor + its tests; `StaticPairings`
  async; the `PairingService`/authorizer **seam** behaviour via fakes in the bus tests (paired /
  unpaired-no-code / pairing-success-acks / wrong-code-dropped); the CLI arg parsing + code
  generation/hashing (pure); the carve-out logic in `handle_inbound` with a `FakePairingService`.
- **PG-gated (skip-as-pass as root here; live on DGX/Mac):** `db::pairings` round-trips
  (`is_paired`, atomic single-use `claim_code` under concurrency, revoke, grants enforced), and an
  e2e wiring `DbPeerAuthorizer` + `DbPairingService` through the bus against a live cluster.

## Decomposition (→ plan)

| # | Task | Verifiable here? |
|---|------|------------------|
| 1 | Migration 0018 (`pairings` + `pairing_codes` + grants) | builds; live = PG |
| 2 | `db::pairings` typed helpers + PG e2e (`claim_code` atomicity) | PG-gated |
| 3 | `PeerAuthorizer` → async + channel; `StaticPairings` async; `DbPeerAuthorizer` | yes (Static + seam); Db = PG |
| 4 | `ingest.rs` refactor (drop `RejectUnpaired`; pure `screen_and_classify`) | yes |
| 5 | `bus.rs`: `PairingService` seam + carve-out + `handle_inbound -> Option<reply>` + `spawn(+pairing)` | yes (fakes) |
| 6 | `DbPairingService` (code hash + atomic claim + bind) | logic yes; live = PG |
| 7 | CLI `pair {issue,list,revoke}` + code generation/hashing | yes (parse+codegen); live = PG |
| 8 | Docs (threat-model carve-out note + negative tests; ROADMAP; HANDOVER) | yes |

## Open points

- **Reply transport for the pairing-ack.** Returned from `handle_inbound` and sent via the same
  channel's `ch.send` (the peer is now paired, so a direct ack is fine). Confirm the bus per-channel
  task wiring sends it (it owns `ch`).
- **Rate-limiting store.** A per-peer failed-attempt counter — for v1 a simple in-memory
  `HashMap<PeerId, (count, window)>` in `DbPairingService` (single process); the 160-bit code makes
  this defense-in-depth, not load-bearing.
- **main.rs wiring** rides with slice #2 Phase D (the live `MatrixChannel`): swap `StaticPairings`
  → `DbPeerAuthorizer` + pass `DbPairingService` to `ChannelBus::spawn`. Until then the bus is not
  daemon-wired, so this slice is exercised by tests, not the running daemon.

## What this slice leaves true

Real, revocable, self-service pairing: a new peer authenticates by presenting an operator-issued
single-use secret; the binding is DB-backed, audited, and the agent never sees unpaired input. The
authorizer is now dynamic (no restart to add/remove a peer). Deferred: WebAuthn (no consumer
surface), per-peer pairing policy / classification floor (slice tie-in), and the daemon wiring
(rides slice #2 Phase D).
