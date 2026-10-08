# #825 — reply catch-up: no completed channel task leaves without a trace

**Issue:** [#825](https://github.com/hherb/kastellan/issues/825) ·
**Date:** 2026-10-08 · **Status:** approved design, pre-plan

## Goal

Every finished channel task (`payload.kind = 'channel'`, in a terminal state) ends in exactly
one of: **routed** to its peer, **recorded as undelivered**, or **recorded as unroutable**,
whether or not a bus was listening when it finished. No duplicate replies, and no replay of
history at deploy.

## Why today's outbound path loses replies

The only trigger for a reply is the `tasks_completed` NOTIFY, and NOTIFY is never replayed. A
reply is lost, with at most a WARN (invisible under `RUST_LOG=error`), whenever no bus consumes
it:

| Window | Where |
| --- | --- |
| Bus down after a pump death (restart backoff 1–60 s, plus any outage) | `boot_supervisor` stops the bus right after `wait_for_death()` |
| `completed.load(id)` returns `Err`, after the NOTIFY was consumed | `bus::handle_completed` |
| Graceful shutdown: tasks the scheduler finalizes while draining | `main.rs` stops the channels **before** `scheduler.shutdown()` |
| Boot: the crash sweep's `crashed` rows, and anything that finishes before a bus is up | the scheduler is spawned before the supervisors finish bring-up (Matrix login can take up to 60 s) |
| NOTIFYs during a listener reconnect | `PgCompletedTasks::next_completed` returns `None` on the first error |
| A channel task with no routing metadata | WARN only (no channel or peer to name in a row) |

Two things rule out deriving "already handled" from `audit_log`. First, audit inserts are
best-effort by design, so a lost `channel.replied` row would turn into a duplicate reply.
Second, `channel.reply_undelivered` rows carry no `task_id`. And while there are two buses
(#497), both see every NOTIFY, so any row keyed by `task_id` needs a single winner.

## Decisions

- **D1: the durable marker is a claim column on `tasks`.** Every route claims the task
  atomically before queueing. Rejected alternatives: deriving the marker from `audit_log`
  (lossy, racy, unindexed), and a separate `channel_replies` table (the same guarantees, but an
  unbounded anti-join for the sweep).
- **D2: the claim is at-most-once.** A task is claimed when its reply is *routed*, the same
  moment `channel.replied` is written today. A send that fails after that is recorded as
  `channel.reply_undelivered` (`send_failed`), exactly as now. Replies still buffered in a
  per-channel queue when a bus is aborted remain **#832's** problem. Claiming at *delivery*
  instead would need a lease and an attempt cap: `EmailChannel::send` refuses every message, so
  without a cap email replies would be re-sent on every sweep.
- **D3: no age cap; late replies carry a note.** Every missed reply is delivered whenever a bus
  next comes up. One that is more than `LATE_AFTER` = 5 min past `finished_at` is prefixed with
  a relative note, so the peer's time zone doesn't matter.
- **D4: history is backfilled as settled,** with the honest disposition `backfilled`.
  Otherwise the first sweep after deploy would re-send every historical reply.
- **D5: the sweep runs at outbound-pump start and every `SWEEP_EVERY` = 5 min.** A periodic run
  is the only thing that catches a load `Err` while the bus stays up.

## Data model: migration `0027_tasks_reply_settled.sql`

```sql
ALTER TABLE tasks
    ADD COLUMN reply_settled_at  TIMESTAMPTZ,
    ADD COLUMN reply_disposition TEXT
        CHECK (reply_disposition IN ('routed', 'unroutable', 'backfilled')),
    ADD CONSTRAINT tasks_reply_settled_together
        CHECK ((reply_settled_at IS NULL) = (reply_disposition IS NULL));

-- D4: history is settled, honestly labelled.
UPDATE tasks
   SET reply_settled_at  = COALESCE(finished_at, updated_at),
       reply_disposition = 'backfilled'
 WHERE payload->>'kind' = 'channel'
   AND state IN ('completed','failed','cancelled','blocked','timed_out','crashed','refused');

-- The backlog only: an empty backlog makes a sweep effectively free.
CREATE INDEX tasks_unsettled_channel_replies ON tasks (id)
 WHERE reply_settled_at IS NULL
   AND payload->>'kind' = 'channel'
   AND state IN ('completed','failed','cancelled','blocked','timed_out','crashed','refused');
```

The grants need no change: `kastellan_runtime` already has `UPDATE` on `tasks`.

⚠️ **The terminal-state list now exists in four places:** the `notify_task_completed` trigger
(0012), this backfill, this index predicate, and Rust. The Rust side gets **one** const,
`REPLIED_STATES`, which moves from `db/src/tasks/turns.rs` to a `pub` const in `reply_claim.rs` (`pub` because the db e2e iterates it) so the sweep shares
the copy that `conversation_turns` reads. A PG test pins the index predicate against that const
(see Testing). Pinning the trigger itself remains #712.

⚠️ `sqlx::migrate!` embeds migrations at compile time, so `kastellan-db` must be rebuilt.

## DB layer: `db/src/tasks/reply_claim.rs` (new; `tasks.rs` is not grown)

```rust
pub enum ReplyDisposition { Routed, Unroutable }   // 'backfilled' is migration-only

pub struct ClaimedReply { pub created_at: OffsetDateTime, pub finished_at: Option<OffsetDateTime> }

/// `None`: already settled (another bus, an earlier sweep), or not terminal.
pub async fn claim_reply(pool, task_id: i64, d: ReplyDisposition)
    -> Result<Option<ClaimedReply>, DbError>;
// UPDATE tasks SET reply_settled_at = now(), reply_disposition = $2
//  WHERE id = $1 AND reply_settled_at IS NULL AND state = ANY($3 /* REPLIED_STATES */)
//  RETURNING created_at, finished_at

/// Ids in the backlog with `id > after_id`, ascending, at most `limit`.
pub async fn unsettled_channel_replies(pool, after_id: i64, limit: i64)
    -> Result<Vec<i64>, DbError>;
```

`claim_reply` also requires a terminal state. A task that is not yet finished must never be
settled, or its real completion would find it already claimed.

## Bus: the outbound flow

The `CompletedTasks` seam (`bus.rs`) gains:

```rust
async fn claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>>;
async fn unsettled(&self, after_id: i64, limit: i64) -> anyhow::Result<Vec<i64>>;
```

`handle_completed(…, id, via: Via)` (with `Via::{Notify, CatchUp}`) is the **single** path for
both the NOTIFY loop and the sweep. Its steps, in order:

1. **Load.** On `Err`: do not claim. WARN `outbound load failed; left for catch-up`. On
   `Ok(None)` (rolled back), return as today.
2. **Route** (pure, unchanged). If the task is not a channel task, return. If it is a channel
   task with no routing metadata, `claim(id, Unroutable)`. The winner alone WARNs and writes
   `channel.reply_unroutable`; a loser returns silently. A failed claim is WARNed, and the task
   stays in the backlog.
3. **Senders lookup.** If this bus does not serve the channel, return without claiming (debug,
   as today; the other bus will route it).
4. **Queue closed** (`tx.is_closed()`): do not claim. INFO `send queue closed; reply left for
   catch-up`. The next bus's start sweep delivers it, so no row is written.
5. **`claim(id, Routed)`.** On `Ok(None)` (already handled), return silently. On `Err`, WARN and
   leave the task in the backlog. On `Ok(Some(c))`, build the reply, prepend
   `delay_note(c.created_at, c.finished_at, now)` when it is `Some`, and `tx.send`. If the send
   fails (the narrow race where the queue closes between step 4 and here), write the existing
   `channel.reply_undelivered` row with reason `queue_closed`. Otherwise write `channel.replied`.

**The sweep** (`bus_outbound::sweep`, beside `handle_completed`, so `catch_up.rs` stays pure) walks the backlog in pages of `SWEEP_PAGE` = 100 by
ascending id, calling `handle_completed(…, Via::CatchUp)` for each id. The cursor advances
past every id, including skipped ones (tasks this bus doesn't serve, load failures), so a stuck
set can't starve the rest. A DB error WARNs and ends **this** sweep only. The pump keeps
running and the death bell is **not** rung.

**The outbound pump:**

```text
sweep()                                   // LISTEN is already up: connect() ran before spawn
loop select! {
    id = completed.next_completed() => match id { Some(id) => handle_completed(.., Notify),
                                                  None => break /* bell, as today */ },
    _ = interval(SWEEP_EVERY).tick()  => sweep(),   // MissedTickBehavior::Delay
}
```

A NOTIFY that arrives during a sweep is queued by the listener and processed afterwards; the
claim drops the duplicate.

## What the peer sees

New pure module `core/src/channel/catch_up.rs`:

- `pub const LATE_AFTER: Duration = 5 min`
- `pub fn delay_note(created_at, finished_at: Option<_>, now) -> Option<String>`. Returns `None`
  unless `now - finished_at > LATE_AFTER` (with `finished_at` falling back to `created_at` when
  it is NULL). Otherwise it returns `"(Delayed reply — you sent this {humanize(now - created_at)}
  ago.)"`. The age runs from `created_at`, because that is when the peer asked.
- `pub fn humanize(Duration) -> String` gives two units at most: `47 min`, `3 h 12 min`,
  `2 days 4 h`. It never renders `0 min` (the floor is `1 min`).
- The note and the body are joined by a blank line. The note is built from timestamps only,
  never from task content.

## Audit rows

| Action | Change |
| --- | --- |
| `channel.replied` | adds `via` (`"notify"` / `"catch_up"`), and `delayed_secs` when a note was added |
| `channel.reply_unroutable` | **new**, `actions::REPLY_UNROUTABLE`, payload `{task_id, observed_at}`: no channel or peer exists to name; written once, by the claim winner |
| `channel.reply_undelivered` `queue_closed` | now written only for the claim-then-close race; the common closed-queue case is left for catch-up and writes no row |

Operator docs to update: `docs/threat-model.md` (the audit-row list) and the Matrix runbook.

## Files

- **Movement-only commit first:** `handle_completed`, `send_or_record` and `PgCompletedTasks`
  move from `core/src/channel/bus.rs` (481 lines) to a new `core/src/channel/bus_outbound.rs`,
  re-exported from `bus` so no caller changes. Prove the move as #750 and #826 did:
  byte-identical moved regions, a matching `fn`-name set, and a negative control.
- New: `db/migrations/0027_tasks_reply_settled.sql`, `db/src/tasks/reply_claim.rs`,
  `core/src/channel/catch_up.rs` (+ tests).
- Touched: `bus.rs` (the seam and the pump loop), `bus_outbound.rs`, `channel/mod.rs`
  (`actions`), `db/src/tasks/turns.rs` (the const moves out), the bus test fakes.

## Testing (TDD, red first)

**Pure** (`catch_up` tests):
- the `LATE_AFTER` edges: exactly 5 min gives `None`; 5 min + 1 s gives `Some`;
- a NULL `finished_at` falls back to `created_at`;
- the age is measured from `created_at`, not `finished_at`;
- `humanize` at its boundaries: 59 s gives `1 min`; 60 min gives `1 h`; 1 h 0 min gives no
  `0 min`; 24 h gives `1 day`; 2 days 4 h 30 min gives `2 days 4 h`.

**Bus, PG-free**, with the fake `CompletedTasks` given in-memory claim semantics:
- a NOTIFY plus a sweep of the same id sends **once**;
- a lost claim gives no send and no row;
- a closed queue gives no claim and no row, and a later sweep with an open queue delivers;
- a load `Err` gives no claim, and the next sweep delivers;
- an unroutable task gives one row and one WARN across two sweeps and two "buses";
- the sweep pages past more than 100 unserved ids and still reaches a served one;
- a sweep `Err` does not end the pump (the bell does not ring);
- the periodic tick runs a sweep (paused tokio clock);
- `via` and `delayed_secs` are present on `channel.replied`, and `delayed_secs` is absent when
  the reply is on time.

**PG e2e** (`core/tests/channel_bus_pg_e2e.rs`):
- the backfill settles a pre-existing terminal channel task as `backfilled`, and leaves a
  non-channel task and a `running` channel task NULL;
- `claim_reply` has exactly one winner across concurrent claimers;
- a non-terminal task cannot be claimed;
- **every state in `REPLIED_STATES`** is returned by `unsettled_channel_replies` (pins the index
  predicate and the const together);
- end to end: a task finalized with no bus running is delivered, with the note when it is old,
  once the bus starts.

**Mutants to kill:** the claim's `IS NULL` guard, the terminal-state guard, the `is_closed`
pre-check, the `LATE_AFTER` comparison (`>` vs `>=`), and the sweep cursor advancing past
skipped ids.

**Gate:** a full Mac sweep (`KASTELLAN_PG_BIN_DIR` set, `--no-fail-fast -- --test-threads=4
--nocapture`, the whole log under `~/.local/state/kastellan/gate-logs/`), the delta predicted
and reconciled per suite, and a cold clippy in a dedicated `CARGO_TARGET_DIR` (27 `Checking
kastellan` lines).

## Out of scope

- #832: replies buffered in a per-channel queue at an abort, and inbound messages abandoned at
  stop.
- #497: unifying the buses. The claim makes two buses safe, not redundant.
- At-least-once delivery (D2).
- #712: machine-checking the trigger's state list.
