# #832 — a bus stop loses nothing silently

**Issue:** [#832](https://github.com/hherb/kastellan/issues/832) ·
**Date:** 2026-10-11 · **Status:** approved design, pre-plan ·
**Amends:** #825's D1 ("every route claims the task before queueing") — the claim moves to the
send side.

## Goal

When a channel bus stops — `ChannelBus::shutdown` aborting its pumps, or a pump ending by itself —
every piece of in-flight work is either **recoverable** (a later bus delivers it) or **said**: a
line on the `[audit-lost]` marker naming channel, peer and task id, never a body. A pump panic is
no longer discarded.

Out of scope, decided: **no grace period** at shutdown (abort-then-join stays); a bounded drain can
be its own issue. A row lost to a crash stays unreported (no `Drop` runs).

## The windows (today)

| # | Window | Today's outcome |
| --- | --- | --- |
| W1 | Inbound message taken from the transport, abort on a non-audit await (`authorize`, `try_pair`, `resolve`, `any_live_nonce`, `enqueue`) or on its ack's `send` | gone: no task, no row, no line (Matrix's cursor already advanced) |
| W2 | Reply in the 32-slot per-channel queue when that pump ends | claimed `routed`, `channel.replied` row, never sent; the #825 start sweep fills this queue on purpose, so a flapping pump loses up to ~32 per flap |
| W3 | Abort on the claim's own round-trip | the `UPDATE` can still commit: `routed`, nothing sent, no row |
| W4 | Receiver dropped between `is_closed()` and `permit.send` | as W2, in a two-line window |
| W5 | Raised ask in the per-channel queue when the pump ends | `ask.delivered` row, never sent |
| W6 | `shutdown` does `let _ = h.await` | a pump's `JoinError::Panic` discarded |

## Decisions

- **D1 — the claim moves to the per-channel pump, immediately before the transport attempt.**
  The outbound pump (NOTIFY and sweep) loads, routes and queues a reply **unclaimed**. Anything
  that drops it before the claim — a stop, a flap, a closed queue, W4's race — leaves it in the
  backlog, and the next sweep delivers it. W2 and W4 are closed by construction, not reported.
  #825's reserve-before-claim ordering becomes moot and goes.
- **D2 — duplicates are resolved by the claim, not prevented.** NOTIFY and a sweep tick can queue
  the same task twice while a queue is backed up; the second claim returns `Ok(None)` and is
  skipped. No in-memory dedup set (YAGNI: a backed-up queue means a wedged transport or Postgres,
  where the sweep cannot read the backlog either).
- **D3 — one shared `CompletedTasks`.** `next_completed` becomes `&self`; `PgCompletedTasks` keeps
  its `PgListener` in a `tokio::sync::Mutex` locked only by the outbound pump. `ChannelBus::spawn`
  takes `Arc<dyn CompletedTasks>`. Chosen over splitting the trait in two: no new concept, no
  extra `spawn` parameter, same fake churn.
- **D4 — the queue item says what it is.** `enum Queued { Reply { task_id, via, out }, Ask(out) }`.
  The outbox registers `mpsc::Sender<Queued>` and wraps a raised ask as `Queued::Ask`.
- **D5 — the per-channel pump delivers a reply:** claim → late note → `channel.replied`
  (`task_id`, `channel`, `peer`, `via`, `delayed_secs`; still meaning *routed*) → `send_or_record`.
  `Ok(None)` skips. `Err` keeps #825's re-read: settled → `reply_undelivered` `claim_unknown`, not
  sent; otherwise WARN, left for catch-up.
- **D6 — guards for what cannot be recovered**, each a `Drop` type reporting through a new sync
  `ChannelEvents::report_lost(&self, line: &str)` (default: `emit_audit_lost_report(Bus, line)`;
  `PgChannelEvents` uses its `Reporter`; fakes record). Each tells a stop from a panic as
  `PendingInsert` does (`std::thread::panicking()`), and names only channel, peer and task id,
  each `quoted_id`'d:
  - **`InboundInFlight`** (W1): armed when `recv()` yields a message, disarmed when
    `handle_inbound` *and* its ack's `send_or_record` return. Line: *a message from channel "x",
    peer "y" was abandoned mid-handling: the bus stopped; it may not have been enqueued or
    recorded*. A stop landing on the final audit insert also gets `PendingInsert`'s line — two
    true lines, deliberately not deduplicated.
  - **`ReplyInFlight`** (W3): armed before the claim, disarmed when the reply's delivery returns
    (whichever arm: sent, skipped, `claim_unknown`, left for catch-up). Line: *task N's reply for
    channel "x", peer "y" may not have been sent: the bus stopped while it was in flight*. The
    outbound pump's unroutable claim gets the same guard, naming the task only (there is no
    channel or peer): *task N's unroutable reply may have been settled without its
    `channel.reply_unroutable` row*.
  - **`PumpQueue`** (W5): owns the per-channel `rx`; its `Drop` calls `close()` and drains with
    `try_recv()`. One line per `Ask` (*an ask message for channel "x", peer "y" was not sent: the
    bus stopped with it queued*); one INFO count for `Reply` items (*N replies left for catch-up*),
    since those are recoverable.
- **D7 — `shutdown` logs a panicked pump** at ERROR. Under release's `panic = "abort"` it never
  fires; debug and test builds see it.
- **D8 — the claim-failure streak moves with the claim.** `FailedSweeps` becomes a pure
  `Streak` counter (`record(failed: bool) -> Option<u32>`) used by the sweep (read failure) and by
  each per-channel pump (consecutive failed claims → ERROR at `SWEEP_FAILURES_LOUD`).
  `SweepReport::claim_failed` keeps counting the unroutable claims the sweep still makes.
- **D9 — `queue_closed` is no longer written by the bus.** The label stays pinned: old rows exist
  and it is an operator interface.

## Data flow (after)

```
NOTIFY / sweep ─► route_one: load → route → (unroutable? claim+row) → tx.send(Queued::Reply).await
                                                                        │ closed / aborted: unclaimed → backlog
per-channel pump ◄──────────────────────────────────────────────────────┘
  Queued::Reply → [ReplyInFlight] claim → note → channel.replied → send_or_record
  Queued::Ask   → send_or_record
  recv()        → [InboundInFlight] handle_inbound → ack send_or_record
  drop          → [PumpQueue] asks: a line each; replies: INFO count
```

## Modules

- `channel/bus_deliver.rs` (new): `Queued`, the per-channel pump's `deliver` (D5), and the guards
  (D6) — or the guards in `channel/bus_guards.rs` if `bus_deliver.rs` would pass ~400 lines.
- `bus_outbound.rs`: `route_one` loses the claim and the post-claim tail; keeps the unroutable
  settle and the sweep.
- `bus.rs`: the per-channel pump calls `deliver` and holds `PumpQueue`; `shutdown` logs panics.
- `catch_up.rs`: `Streak` replaces `FailedSweeps`.
- `outbox.rs`: `Sender<Queued>`.
- Tests: new files under `bus/tests/` (`bus/tests.rs` is 1306 lines); no growth of that file.

## Testing

PG-free unless noted; each new arm mutation-checked.

- Guards: settled → silent; dropped → exactly one line with channel/peer/task id and never the
  body; a panic is told from a stop.
- `PumpQueue`: queued asks → one line each; queued replies → no `[audit-lost]` line.
- A reply queued to a bus that is then stopped is still unclaimed; a fresh bus delivers it (W2).
- A stop while a claim is parked → exactly one `ReplyInFlight` line (W3).
- A stop while `handle_inbound` is parked on `enqueue` → one `InboundInFlight` line (W1).
- The same task queued twice is sent once (D2).
- The late-note, `claim_unknown` and claim-failure tests move from `handle_completed` to the pump.
- A panicking pump is logged by `shutdown` (D7).
- PG: `reply_catch_up_pg_e2e` gains a stop-with-queued-replies → restart → delivered case.

## Residual (documented, not closed)

- A transport `send` in flight at abort may or may not have delivered — `ReplyInFlight` /
  `InboundInFlight` say so as "may not have been sent".
- A crash (no `Drop`) still loses everything above silently; the catch-up sweep still recovers
  any reply not yet claimed.
