# Channel pairing (comms slice #3) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or
> superpowers:executing-plans. Checkbox (`- [ ]`) steps.

**Goal:** Replace `StaticPairings` with a real, revocable, self-service pairing handshake: an
operator issues a single-use short-lived code (CLI), a new peer presents it in-channel, the bot
binds `(channel, peer)` in a DB-backed `pairings` table, and the async `DbPeerAuthorizer` gates the
bus on it. The agent never sees unpaired input (the carve-out is compare-only).

**Architecture:** `docs/superpowers/specs/2026-06-12-channel-pairing-design.md`.

**Verification:** Tasks 4, 5, 7 (the pure refactor, the bus carve-out via fakes, CLI codegen/parse)
are verifiable anywhere. Tasks 1, 2, 6 and the e2e are PG-gated (skip-as-pass as root; live on
DGX/Mac). Task 3 splits: `StaticPairings`/seam here, `DbPeerAuthorizer` PG.

**Reference:** `db/migrations/0017_relation_kinds.sql` (CREATE+GRANT+REVOKE shape),
`db/src/{relation_kinds,tasks}.rs` (typed helpers), `core/src/bin/kastellan-cli/relations_kinds.rs`
(admin-pool CLI), `core/src/channel/{auth,ingest,bus}.rs` (slice #1), `core/tests/injection_guard_e2e.rs`
(PG-gated bootstrap).

**Build prelude:** `source "$HOME/.cargo/env"`.

---

## Task 1: Migration 0018 — `pairings` + `pairing_codes` + grants

**Files:** create `db/migrations/0018_pairings.sql`.

- [ ] Write the migration per the spec's schema block: both tables, the two indexes (active-pairing
  unique partial index; claimable-codes partial index), and the grant/revoke:
  `GRANT SELECT, INSERT ON pairings TO kastellan_runtime; REVOKE UPDATE, DELETE, TRUNCATE ON pairings FROM kastellan_runtime;`
  `GRANT SELECT, UPDATE ON pairing_codes TO kastellan_runtime; REVOKE INSERT, DELETE, TRUNCATE ON pairing_codes FROM kastellan_runtime;`
  Header comment in the 0016/0017 style explaining the REVOKE (0002 `ALTER DEFAULT PRIVILEGES`
  auto-grants full CRUD) + why runtime can pair-but-not-revoke and consume-but-not-mint.
- [ ] `cargo build -p kastellan-db` (the embedded `MIGRATOR` picks it up). Commit.

## Task 2: `db::pairings` typed helpers + PG e2e

**Files:** create `db/src/pairings.rs`; modify `db/src/lib.rs` (`pub mod pairings;`);
`db/tests/postgres_e2e.rs` (or a new `pairings_e2e.rs`).

- [ ] `Pairing` struct + helpers: `is_paired`, `insert_pairing` (ON CONFLICT DO NOTHING on the
  active-unique index), `revoke_pairing`, `list_pairings`, `insert_code`, `any_active_code`, and the
  atomic `claim_code` (conditional UPDATE → `rows_affected()==1`). All over generic
  `sqlx::Executor` (like `db::audit`).
- [ ] PG e2e (skip-as-pass without supervisor/PG): insert→is_paired true; revoke→is_paired false;
  duplicate insert idempotent; `claim_code` single-use (**second claim of the same code returns
  false**, and an expired code never claims); `any_active_code` reflects state.
- [ ] Run (PG box) + commit `feat(db): pairings + pairing_codes typed helpers (single-use claim)`.

## Task 3: `PeerAuthorizer` → async + channel; `DbPeerAuthorizer`

**Files:** `core/src/channel/auth.rs`; `core/Cargo.toml` (db already a dep).

- [ ] Make the trait `#[async_trait] async fn authorize(&self, channel: &ChannelId, peer: &PeerId) -> AuthDecision`.
  Update `StaticPairings` (async; match peer-only, ignore channel) + its tests (`.await`, pass a
  `ChannelId`).
- [ ] Add `DbPeerAuthorizer { pool: PgPool }` → `db::pairings::is_paired(&pool, &channel.0, &peer.0)`
  (errors fail-closed → `Rejected`, logged). (Its own behaviour is covered by the Task-2 db e2e +
  the slice's PG e2e; the trait contract is exercised by `StaticPairings` here.)
- [ ] `cargo test -p kastellan-core --lib channel::auth`. Commit.

## Task 4: `ingest.rs` refactor — drop `RejectUnpaired`, pure `screen_and_classify`

**Files:** `core/src/channel/ingest.rs`.

- [ ] `InboundDecision` becomes `{ Enqueue { payload }, InjectionBlocked { .. } }` (remove
  `RejectUnpaired` — rejection is now the authorizer's job, handled in the bus). Replace
  `classify_inbound*` with pure `screen_and_classify(msg, screen) -> InboundDecision` (the screen →
  payload/blocked half, authz removed) + a `screen_and_classify_real(msg)` using the real guard.
  Keep `build_channel_task_payload` + `SCAN_BYTE_CAP` + `sha256_hex`.
- [ ] Update the ingest tests: drop the unpaired test (moves to the bus carve-out tests); keep the
  clean-enqueue + injection-blocked + real-guard tests.
- [ ] `cargo test -p kastellan-core --lib channel::ingest`. Commit.

## Task 5: `bus.rs` — `PairingService` seam + carve-out + `handle_inbound -> Option<reply>`

**Files:** `core/src/channel/bus.rs`.

- [ ] Add the seam: `#[async_trait] PairingService { async fn try_pair(channel, peer, body) -> PairingOutcome }`
  + `enum PairingOutcome { Paired, NotAPairingAttempt }`.
- [ ] Rewrite `handle_inbound(authorizer, pairing: Option<&dyn PairingService>, events, msg) -> Option<OutgoingMessage>`
  per the spec: async authorize → on `Rejected` run the carve-out (try_pair → `Paired`: audit
  `channel.paired`, return the ack `OutgoingMessage`; else audit `channel.rejected_unpaired`) → on
  `Recognised` run `screen_and_classify` (enqueue+`channel.received` / `channel.injection_blocked`).
  Add `PAIRED` audit action const + the ack body constant.
- [ ] `ChannelBus::spawn(channels, authorizer, pairing: Option<Arc<dyn PairingService>>, events, completed)`:
  the per-channel task does `if let Some(reply) = handle_inbound(&*authorizer, pairing.as_deref(), &*events, &msg).await { let _ = ch.send(reply).await; }`.
- [ ] Update the bus unit tests (async authorizer fakes; `+None`/`+Some(fake)` pairing): paired-clean
  enqueues; unpaired+no-pairing → rejected; **unpaired + FakePairing returns Paired → no enqueue,
  ack returned, `channel.paired` audited**; unpaired + FakePairing `NotAPairingAttempt` → rejected.
- [ ] Update both e2e callers (`channel_bus_e2e`, `matrix_channel_e2e`) to pass `None` for pairing.
- [ ] `cargo test -p kastellan-core channel` + the two e2e. Commit.

## Task 6: `DbPairingService`

**Files:** `core/src/channel/matrix.rs` or a new `core/src/channel/pairing.rs`.

- [ ] `DbPairingService { pool }::try_pair`: `if !any_active_code → NotAPairingAttempt`; else
  `sha256_hex(body)` → `claim_code(hash, "<channel>/<peer>")`; on success `insert_pairing(channel,
  peer, "code")` → `Paired`; else `NotAPairingAttempt`. Optional in-memory per-peer failed-attempt
  rate limiter (defense-in-depth). Pure helpers (hashing) unit-tested here; the DB path is PG e2e.
- [ ] Commit `feat(channel): DbPairingService — single-use code → bind pairing`.

## Task 7: CLI `kastellan-cli pair {issue,list,revoke}`

**Files:** create `core/src/bin/kastellan-cli/pair.rs`; modify `main.rs` (mod + dispatch).

- [ ] `pair issue [--label T] [--ttl-mins N]`: generate a 160-bit random code (use the workspace
  `rand`), render base32/hex, store `sha256` via `insert_code` (admin pool), **print the plaintext
  once** + instructions, write `pairing.code_issued` audit (hash+label+expiry, never plaintext).
- [ ] `pair list [--all]` → `list_pairings`; `pair revoke <channel> <peer>` → `revoke_pairing` +
  `pairing.revoked` audit.
- [ ] Pure unit tests: arg parsing (`--ttl-mins`, `--label`, `--all`); code generation length/charset;
  `sha256_hex` stability. (DB paths exercised by the slice PG e2e.)
- [ ] `cargo test` + `cargo build`. Commit.

## Task 8: Docs

- [ ] **threat-model.md**: under "Communication channel", add the carve-out analysis (compare-only,
  operator-gated, single-use, binds channel-native identity); add negative tests — "unpaired peer
  with no active code → dropped"; "wrong code while a code is active → dropped + `channel.pairing_failed`/
  `rejected_unpaired`, never reaches the agent"; "correct code → bound + `channel.paired`".
- [ ] **ROADMAP**: tick the "DM pairing flow" item (note WebAuthn deferred; daemon wiring rides
  slice #2 Phase D). **HANDOVER** entry.
- [ ] Final gate: `cargo test -p kastellan-core channel`, `cargo test -p kastellan-db`,
  `cargo clippy --workspace --all-targets -- -D warnings`. Commit.

## What this leaves true

Real revocable self-service pairing, DB-backed + audited, agent never sees unpaired input, no
restart to add/remove peers. Deferred: WebAuthn (no consumer surface), per-peer policy, daemon
wiring (swap `StaticPairings`→`DbPeerAuthorizer` + pass `DbPairingService` — rides slice #2 Phase D).
