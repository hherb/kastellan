# Close the Matrix inbound-loss window on worker respawn — design (#321)

**Date:** 2026-06-23
**Issue:** [#321](https://github.com/hherb/kastellan/issues/321)
**Scope:** the *downtime window* only (messages a user DMs the bot while the worker
process is down). The harder worker→host delivery gap (an event the worker
synced + buffered but died before the host drained it) is explicitly out of
scope — it needs at-least-once delivery across the `matrix.poll` protocol and is
a separate issue/session.

## Problem

`workers/matrix/src/sdk_live.rs` gates inbound delivery on a `live: AtomicBool`
that is `false` during the initial catch-up sync (`sync_once`) and flips to
`true` afterwards. The gate exists to stop the worker replaying the whole room
history as fresh inbound on every (re)start.

PR #320 added a self-healing supervised driver (`MatrixChannel::supervised`)
that respawns the worker after a death. The two interact badly: any message a
user sends **while the worker is down** arrives in the respawned worker's
catch-up sync and is silently dropped by the `live` gate. The user gets no reply
and no error.

## Core insight

The "sync-token watermark" the issue proposes **already exists**. matrix-sdk
persists the sync token in its SQLite state store across restarts, and
`sync_once` with `SyncSettings::default()` automatically resumes from it (the
`since` parameter) at the network level. Therefore, on a restart, the catch-up
sync returns **only events received since the previous run** — i.e. exactly the
downtime backlog — not the full history.

The bug is purely that our `live` gate suppresses those incremental events along
with the genuine full-history replay. We do not need to invent or persist a new
watermark — only to *read* the SDK's existing token and stop suppressing when it
is present.

## Fix

Read the persisted sync token **before** the initial sync, to distinguish the
two cases:

- **Prior token present (restart / session restore that has synced before):**
  the catch-up sync is incremental → its events are genuinely-unprocessed
  downtime messages → seed `live = true` from the start so they reach the
  inbound buffer.
- **No prior token (fresh login, or a restore that never completed a sync):**
  the catch-up sync replays recent room history → keep `live = false` during
  `sync_once`, then flip to `true` afterwards (today's behavior, unchanged).

## Components

1. **Pure decision** — `initial_live_state(prior_sync_token: Option<&str>) -> bool`
   returning `prior_sync_token.is_some()`. This is the unit-tested core (rule #1,
   TDD). It carries the whole semantic decision; the wiring around it is thin.

2. **Thin async reader** — `read_prior_sync_token(client: &Client) -> Option<String>`
   wrapping
   `client.state_store().get_kv_data(StateStoreDataKey::SyncToken).await.ok().flatten().and_then(|v| v.into_sync_token())`.
   **Fail-soft:** a store-read `Err` (or absent value) maps to `None` →
   "fresh / suppress". A read failure can therefore never cause history replay;
   at worst it re-drops a downtime window, which is exactly the pre-fix
   behavior. No new failure mode is introduced.

3. **Wiring** in `connect_client` — read the token after `restore_or_login`
   (the store is open by then) and before `register_message_handler`; seed the
   `live` `AtomicBool` from `initial_live_state(token.as_deref())` instead of
   the hardcoded `false`. The existing post-`sync_once`
   `live.store(true, SeqCst)` stays — it is a no-op when already `true`, and
   still flips the fresh-login case live after its backlog drains.

## API path (verified, matrix-sdk 0.18, all public)

- `Client::state_store() -> &DynStateStore` — public.
- `StateStore::get_kv_data(StateStoreDataKey::SyncToken) -> Result<Option<StateStoreDataValue>, _>`.
- `StateStoreDataValue::into_sync_token() -> Option<String>` — public.
- `StateStoreDataKey` / `StateStoreDataValue` re-exported via `matrix_sdk::store::*`.

(`Client::sync_token()` itself is `pub(crate)` in 0.18, hence the store route.)

## Rejected alternatives

- **Custom watermark file** (e.g. last-processed event id / `origin_server_ts`
  under `<store>/`): reinvents persistence the SDK already does, adds on-disk
  state to keep consistent with the SDK's own token, and increases gap /
  double-delivery risk. Rejected.
- **Gate on "did we restore a session" (a bool from `restore_or_login`)**
  instead of on the token: imprecise. A restored session that never completed a
  sync has no token; treating it as live would replay its first full sync as
  fresh inbound — the exact bug we are fixing. The token is the precise signal.

## Error handling / safety

Fail-soft to "fresh / suppress" everywhere the token can't be read. The change
only ever *widens* delivery when a prior position is known to exist; it cannot
introduce a replay of stale history, because the no-token path is byte-identical
to today.

## Testing

- **Unit (TDD, hermetic):** `initial_live_state` — `Some(_) -> true`,
  `None -> false`. Lives beside the existing `parse_config` / `drain` unit tests
  in `sdk_live.rs` (default build, no `live-matrix` needed for the pure fn — but
  the fn sits behind the `live-matrix` cfg with the rest of the module, so the
  tests run under `--features live-matrix`).
- **Doc update:** `MatrixChannel::supervised` doc comment
  (`core/src/channel/matrix.rs` ~lines 176–180) currently states the inbound
  window "is NOT recovered … tracked as issue #321"; update it to describe the
  sync-token-gated recovery.
- **Live proof (DGX):** extend the `#[ignore]` `core/tests/matrix_live_e2e.rs`
  with a restart scenario if the homeserver harness allows it cleanly (bot up →
  stop bot → peer sends → restart bot → bot must surface the message via
  `matrix.poll`); otherwise document a manual DGX verification of the same
  sequence. This matches how the live worker is verified today (the round-trip
  e2e is already `#[ignore]` + DGX-gated).

## Footprint

- Files: `workers/matrix/src/sdk_live.rs` (logic + unit tests),
  `core/src/channel/matrix.rs` (doc comment), optionally
  `core/tests/matrix_live_e2e.rs` (restart e2e).
- Pure-Rust, `live-matrix`-gated, no protocol / schema / migration change.
- Unit gate runs on the Mac; live proof on the DGX.
