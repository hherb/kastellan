# #832 — a bus stop loses nothing silently: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every piece of work in flight when a channel bus stops is recoverable (a later bus
delivers it) or said on `[audit-lost]`; a pump panic is no longer discarded.

**Architecture:** The reply claim moves from the outbound pump (`route_one`) to the per-channel
pump, immediately before the transport attempt, so a reply dropped with its queue is still
unclaimed and the catch-up sweep re-delivers it. What cannot be recovered — an inbound message
mid-handling, a claim/send in flight, a queued raised ask — is reported by `Drop` guards through a
new sync `ChannelEvents::report_lost`. `CompletedTasks` becomes shareable (`&self`
`next_completed`, `Arc<dyn …>`), so per-channel pumps can claim.

**Tech Stack:** Rust, tokio (mpsc, Mutex, JoinHandle), async-trait, sqlx (PgListener), tracing.

**Spec:** `docs/superpowers/specs/2026-10-11-832-bus-stop-windows-design.md`

## Global Constraints

- No grace period: `ChannelBus::shutdown` stays abort-then-join.
- A lost-work line names only channel, peer and task id (`audit_text::quoted_id` for strings) —
  **never a body, never DB/transport error text**.
- Guards tell a stop from a panic with `std::thread::panicking()`, as `pg_events::PendingInsert`.
- `channel.replied` keeps its payload keys (`task_id`, `channel`, `peer`, `via`, optional
  `delayed_secs`) and its meaning (*routed*, written just before the transport attempt).
- `UndeliveredReason::QueueClosed` / `"queue_closed"` stays defined and pinned; the bus stops
  writing it.
- Files ≤ 500 lines where feasible; `core/src/channel/bus/tests.rs` (1306) must not grow — new
  tests go in new files under `core/src/channel/bus/tests/`.
- Clippy `-D warnings` clean; count **27** `Checking kastellan` lines on a cold run in
  `CARGO_TARGET_DIR=$HOME/.cargo-clippy-832`.
- Commits: `git add` specific files only; no backticks inside `git commit -m "..."`.

## Review Focus

1. **A duplicate queued reply** (NOTIFY + sweep tick both queue task 7) must be sent once and
   leave exactly one `channel.replied` — Task 4 pins it.
2. **A claim that fails at the pump repeatedly** (DB refusing `UPDATE`) must reach ERROR, not WARN
   forever — Task 4 pins the pump streak via `Delivery::ClaimFailed` counting.
3. **A stop landing on the final `channel.replied` insert** yields two true lines
   (`PendingInsert` + `ReplyInFlight`), not one and not three — Task 4 updates
   `dropped.rs::a_replied_row_abandoned_by_the_stop_is_reported`.
4. **Queued replies at stop must stay unclaimed** (no `[audit-lost]` line, disposition `None`) and a
   fresh bus delivers them — Task 5 (unit) and Task 7 (PG).
5. **A raised ask queued at stop** gets a line naming channel and peer, never its body (which
   contains the approval token) — Task 5.

---

### Task 0: Docs reflect the current state (pre-work)

**Files:** Modify `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`; create
`docs/devel/handovers/archive/handover_20261011_832_pre-prune.md`.

- [ ] Snapshot HANDOVER.md to the archive file (verbatim copy).
- [ ] HANDOVER: header → last updated 2026-10-11, PR #842 merged; repoint the archive link; move
  #842 to *Recently merged*; *This session* → #832 in progress; prune to < 500 lines (compress
  the #827/#826, #813 and audit-sink "Previous" blocks into one-liners pointing at the archive).
- [ ] ROADMAP: mark #825/PR #842 merged; add #832 as current.
- [ ] Commit: `git add` the three files; message "Handover + roadmap: #842 merged; start #832".

### Task 1: `Streak` replaces `FailedSweeps`

**Files:** Modify `core/src/channel/catch_up.rs`, `core/src/channel/bus.rs` (the `note` closure).
Tests in `catch_up.rs`'s own test module.

**Interfaces — Produces:**
```rust
/// Consecutive failures of one kind. Pure.
#[derive(Debug, Default)]
pub struct Streak(u32);
impl Streak {
    /// Count one outcome. `Some(n)` once `n` consecutive failures reach
    /// `SWEEP_FAILURES_LOUD`; any success resets.
    pub fn record(&mut self, failed: bool) -> Option<u32>;
}
```

- [ ] Write failing tests: `Streak::default().record(true)` ×2 → `None`, 3rd → `Some(3)`, 4th →
  `Some(4)`; a `record(false)` resets so the next 2 failures are `None`.
- [ ] Run `cargo test -p kastellan-core --lib channel::catch_up` → fails (no `Streak`).
- [ ] Implement `Streak`; delete `FailedSweeps`; migrate its existing tests to
  `streak.record(report.is_failure())`; in `bus.rs` the outbound pump's closure calls
  `streak.record(report.is_failure())`.
- [ ] Tests pass; commit ("Streak: one pure consecutive-failure counter (#832)").

### Task 2: A shareable `CompletedTasks`

**Files:** Modify `core/src/channel/bus.rs` (trait + `spawn`), `core/src/channel/bus_outbound.rs`
(`PgCompletedTasks`), every fake: `bus/tests.rs` (`FakeCompleted`, `EndedCompleted`,
`ParkingCompleted`, `bus_over`, the spawn calls), `bus/tests/catch_up.rs` (`Arc<Backlog>`,
`SamePage`, `ClosingClaim`), `bus/tests/catch_up/pump.rs` (`rig`), `bus/tests/dropped.rs`
(`GatedCompleted` — `release: Mutex<Option<oneshot::Receiver<()>>>`),
`core/tests/{channel_bus_e2e,matrix_channel_e2e}.rs` (`yielded: AtomicBool`),
`core/tests/email_channel_e2e.rs`, `core/tests/reply_catch_up_pg_e2e.rs`,
`core/tests/channel_bus_pg_e2e.rs` (`let completed` no longer `mut`), `core/src/main/{matrix,email}_boot.rs`.

**Interfaces — Produces:**
```rust
async fn next_completed(&self) -> Option<i64>;   // was &mut self; called by one pump only
pub fn spawn(..., completed: Arc<dyn CompletedTasks>, asks: Option<Arc<AskWiring>>) -> Self;
pub struct PgCompletedTasks { listener: tokio::sync::Mutex<sqlx::postgres::PgListener>, pool: sqlx::PgPool }
```

- [ ] Change the trait and `PgCompletedTasks::next_completed` (`let mut l = self.listener.lock().await;`
  then the same loop on `l.recv()`); doc: "the lock is only ever taken by the outbound pump; a
  `select!` that drops this future drops the guard with it".
- [ ] `spawn` takes `Arc<dyn CompletedTasks>`; the outbound pump moves a clone.
- [ ] Migrate every fake/call site listed above (`Box::new(x)` → `Arc::new(x)`).
- [ ] `cargo build --workspace --tests` clean; `cargo test -p kastellan-core --lib channel::` and
  the four core integration suites green (pure refactor: identical counts).
- [ ] Commit ("CompletedTasks is shared: next_completed takes &self (#832)").

### Task 3: In-flight guards and `report_lost`

**Files:** Create `core/src/channel/bus_guards.rs` (+ `mod bus_guards;` in `channel/mod.rs`);
modify `core/src/channel/bus.rs` (trait method), `core/src/channel/pg_events.rs` (override),
`core/src/channel/bus/tests.rs` (`FakeEvents` gains `lost` — a 6-line change, allowed).
Tests: `core/src/channel/bus_guards.rs` `#[cfg(test)] mod tests`.

**Interfaces — Produces:**
```rust
// bus.rs, on ChannelEvents:
/// Say that work was lost without a row (a stop mid-flight). Sync: called
/// from `Drop`. Default: the bus's `[audit-lost]` writer.
fn report_lost(&self, line: &str) {
    crate::worker_stderr::emit_audit_lost_report(crate::worker_stderr::AuditLostWriter::Bus, line);
}

// bus_guards.rs:
pub(crate) enum InFlight { Inbound, Reply { task_id: i64 }, Unroutable { task_id: i64 } }
pub(crate) struct InFlightGuard<'a> { events: &'a dyn ChannelEvents, what: InFlight, about: String, settled: bool }
impl<'a> InFlightGuard<'a> {
    /// `about` = `audit_text`-quoted channel/peer, e.g. ` for channel "m", peer "p"` (empty for Unroutable).
    pub(crate) fn arm(events: &'a dyn ChannelEvents, what: InFlight, channel: Option<&ChannelId>, peer: Option<&PeerId>) -> Self;
    pub(crate) fn settle(self);
}
impl Drop for InFlightGuard<'_> { /* !settled → events.report_lost(&format_abandoned(..)) */ }
/// Pure.
pub(crate) fn format_abandoned(what: &InFlight, about: &str, panicked: bool) -> String;
pub(crate) fn format_ask_unsent(channel: &ChannelId, peer: &PeerId) -> String;
```
Line texts (pinned literally by tests):
- Inbound: `a message{about} was abandoned mid-handling: {cause}; it may not have been enqueued or recorded`
- Reply: `task {id}'s reply{about} may not have been sent: {cause}`
- Unroutable: `task {id}'s unroutable reply may have been settled without its channel.reply_unroutable row: {cause}`
- `{cause}` = `the bus stopped while it was in flight` | `its handling panicked`
- Ask: `an ask message for channel "x", peer "y" was not sent: the bus stopped with it queued`

- [ ] Failing tests: (a) armed then `settle()` → `report_lost` never called; (b) armed then dropped
  → exactly one line equal to the pinned Inbound text for channel `"matrix"`, peer `"@me:srv"`;
  (c) Reply/Unroutable texts pinned; (d) a peer containing `"` and a newline is quoted
  (`quoted_id`), never raw; (e) a guard dropped while a thread panics (`std::panic::catch_unwind`
  around a closure that arms then panics) reports `its handling panicked`. Use a local
  `RecordingEvents { lost: Mutex<Vec<String>> }` implementing `ChannelEvents`.
- [ ] Run → fails to compile; implement; run → pass.
- [ ] `PgChannelEvents::report_lost` → `(self.report)(AuditLostWriter::Bus, line)`; a test in
  `pg_events/tests.rs` that `with_reporter(refused_pool(), record)` routes `report_lost` to the
  recorder.
- [ ] `FakeEvents` in `bus/tests.rs` gains `lost: Mutex<Vec<String>>` + override.
- [ ] Commit ("In-flight guards report work a bus stop abandons (#832)").

### Task 4: Claim at send — `Queued`, `deliver`, unclaimed queueing

**Files:** Create `core/src/channel/bus_deliver.rs`; modify `bus_outbound.rs` (`route_one`
loses reserve/claim/tail; `handle_completed` returns `Option<Queued>`), `bus.rs` (pump outbound
arm, sender maps typed `Sender<Queued>`, re-exports), `outbox.rs` (`Sender<Queued>`,
`try_deliver` wraps `Queued::Ask`), `undelivered.rs` (`QueueClosed` doc: not written since #832);
tests: create `core/src/channel/bus/tests/deliver.rs` (+ `mod deliver;`), migrate
`bus/tests/catch_up.rs`, `bus/tests/dropped.rs`, `bus/tests.rs` (`outbound_routes…`,
`outbound_ignores…`, `the_bus_registers…`), `outbox.rs` tests,
`core/tests/channel_bus_pg_e2e.rs` (route then `deliver`).

**Interfaces — Produces:**
```rust
// bus_deliver.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Queued {
    /// A finished task's reply, NOT yet claimed: the per-channel pump claims it.
    Reply { task_id: i64, via: Via, out: OutgoingMessage },
    /// A core-initiated message (a raised ask): `ask.delivered` is already written.
    Ask(OutgoingMessage),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery { Sent, NotClaimed, ClaimFailed }
/// Claim → late note → channel.replied → send_or_record, under a Reply guard; an Ask is sent as is.
pub async fn deliver(completed: &dyn CompletedTasks, events: &dyn ChannelEvents, ch: &dyn Channel, id: &ChannelId, item: Queued) -> Delivery;
// bus_outbound.rs
pub async fn handle_completed(completed, events, senders: &HashMap<ChannelId, mpsc::Sender<Queued>>, id, via) -> Option<Queued>;
pub async fn sweep(completed, events, senders: &HashMap<ChannelId, mpsc::Sender<Queued>>) -> SweepReport;
```
`deliver` body for `Reply` (moved from `route_one`, unchanged in substance):
```rust
let guard = InFlightGuard::arm(events, InFlight::Reply { task_id }, Some(&out.channel), Some(&out.peer));
let outcome = match completed.claim(task_id, ReplyDisposition::Routed).await {
    Ok(Some(claimed)) => {
        let late = catch_up::late_reply(&claimed, time::OffsetDateTime::now_utc());
        if let Some((_, note)) = &late { out.body = format!("{note}\n\n{}", out.body); }
        let mut row = serde_json::json!({"task_id": task_id, "channel": out.channel.0, "peer": out.peer.0, "via": via.as_str()});
        if let Some((late_by, _)) = late { row["delayed_secs"] = late_by.whole_seconds().into(); }
        events.audit(actions::REPLIED, row).await;
        send_or_record(ch, events, id, out, "channel send failed").await;
        Delivery::Sent
    }
    Ok(None) => Delivery::NotClaimed, // NOTIFY + sweep both queued it, or the other bus won
    Err(e) => { /* claim_outcome re-read; Some(true) → claim_unknown row */ Delivery::ClaimFailed }
};
guard.settle();
outcome
```
`route_one` after the sender lookup: `match tx.send(Queued::Reply { task_id: id, via, out }).await { Ok(()) => Routed::Queued(item_clone), Err(_) => { info!(…"send queue closed; reply left for catch-up"); Routed::Skipped } }`.
`claim_outcome` moves to `bus_deliver.rs` as `pub(super)` (still used by `settle_unroutable`).
Per-channel pump (bus.rs): holds `let mut claims = catch_up::Streak::default();` and on
`Some(item) = rx.recv()` calls `deliver(&*completed, &*events, &*ch, &id, item)`; if
`claims.record(d == Delivery::ClaimFailed)` is `Some(n)` → `error!(channel, consecutive = n, "reply claims keep failing; finished tasks' replies are not going out")`
(`Delivery::NotClaimed`/`Sent` count as success).

Test migration rule: add to `bus/tests/catch_up.rs` a helper
```rust
/// Route `id` (the outbound pump's half), then deliver whatever it queued
/// (the per-channel pump's half) through a recording channel: the composed
/// path every pre-#832 `handle_completed` test asserted on.
async fn route_and_deliver(c: &dyn CompletedTasks, ev: &FakeEvents, id: i64, via: Via) -> Option<OutgoingMessage>
```
(it builds a 64-slot matrix queue, calls `handle_completed`, `try_recv`s, `deliver`s into a
`SinkChannel { sent: Mutex<Vec<OutgoingMessage>> }`, returns the sent message). Each
`handle_completed(&backlog, &ev, &senders, ID, VIA)` in the claim tests becomes
`route_and_deliver(&backlog, &ev, ID, VIA)`; assertions stay. Replace:
- `a_queue_closing_after_the_claim_still_writes_queue_closed` → delete; add
  `a_reply_queued_to_a_queue_that_then_closes_stays_unclaimed` (queue, drop rx, assert
  `disposition(7) == None`, no rows, next sweep with an open queue delivers "done").
- `an_abort_while_waiting_for_queue_space_leaves_the_reply_unclaimed` → keep; filler becomes
  `Queued::Ask(filler)`.
- `a_closed_queue_leaves_the_reply_for_catch_up` → claim_calls stays 0.
- `a_sweep_reports_what_it_left_behind` → claim failures no longer happen in the sweep: expected
  becomes `queued: 2, unserved: 1, load_failed: 1, claim_failed: 0` on the first sweep; the
  repeated-claim-failure half moves to `deliver.rs` (below).
New tests in `bus/tests/deliver.rs`:
- `the_same_task_queued_twice_is_sent_once` (two `Queued::Reply` for 7 → `Sent`, `NotClaimed`;
  one send, one `channel.replied`).
- `a_queued_reply_is_claimed_only_when_delivered` (after `handle_completed`, `disposition == None`;
  after `deliver`, `Routed`).
- `an_ask_is_sent_without_a_claim` (`claim_calls == 0`, no `channel.replied`).
- `repeated_claim_failures_are_counted_by_the_pump` — through a running bus (`Backlog` with
  `claim_fails_once` re-armed): `Delivery::ClaimFailed` returned 3× in a row (assert on `deliver`
  directly + a `Streak` fed from it reaching `Some(3)`).
- `dropped.rs::a_replied_row_abandoned_by_the_stop_is_reported` → now expects **2** lines: the
  `channel.replied … may not have been written` one, then `task 7's reply for channel "matrix",
  peer "@me:srv" may not have been sent: the bus stopped while it was in flight`; neither carries
  `SECRET_BODY`.

- [ ] Write `bus/tests/deliver.rs` tests + the `route_and_deliver` helper; run → compile failure.
- [ ] Implement `bus_deliver.rs`, reshape `route_one`, retype senders/outbox, wire the pump.
- [ ] Migrate the listed tests; update docs on `handle_completed`, `route_one`, `UndeliveredReason::QueueClosed`, `bus.rs` module doc ("claim → push" becomes "route → push; the per-channel pump claims").
- [ ] `cargo test -p kastellan-core --lib channel::` + `channel_bus_pg_e2e` (PG knob) green.
- [ ] Mutation checks (copy file, mutate, run, restore by copy): (1) claim in `route_one` again
  instead of `deliver` → `a_queued_reply_is_claimed_only_when_delivered` fails; (2) `Ok(None)` →
  send anyway → `the_same_task_queued_twice_is_sent_once` fails; (3) drop `guard.settle()` → a
  `deliver` test asserting `ev.lost` empty fails.
- [ ] Commit ("Claim a reply where it is sent, so a dropped queue loses nothing (#832)").

### Task 5: Inbound guard, queue drain, unroutable guard

**Files:** Modify `core/src/channel/bus.rs` (per-channel pump), `bus_guards.rs` (`PumpQueue`),
`bus_outbound.rs` (`settle_unroutable` guard). Tests: create `core/src/channel/bus/tests/stop.rs`.

**Interfaces — Produces:**
```rust
/// Owns a per-channel queue's receiver. On drop: close, drain with
/// `try_recv`, report each `Queued::Ask` (format_ask_unsent), and INFO-count
/// the replies (unclaimed, so the sweep delivers them).
pub(crate) struct PumpQueue { rx: mpsc::Receiver<Queued>, events: Arc<dyn ChannelEvents>, channel: ChannelId }
impl PumpQueue { pub(crate) fn new(rx, events, channel) -> Self; pub(crate) async fn recv(&mut self) -> Option<Queued>; }
```
Per-channel pump inbound arm:
```rust
Some(msg) => {
    let guard = InFlightGuard::arm(&*events, InFlight::Inbound, Some(&msg.channel), Some(&msg.peer));
    if let Some(ack) = handle_inbound(...).await { send_or_record(...).await; }
    guard.settle();
}
```
Tests in `bus/tests/stop.rs` (multi_thread, `FakeEvents` with `lost`):
- `a_stop_mid_enqueue_reports_the_inbound_message` — `FakeEvents` variant whose `enqueue` parks
  on `pending()` after flagging `entered`; send a paired message; wait for `entered`;
  `bus.shutdown()`; `lost` has exactly the Inbound line for `"matrix"`/`"@me:srv"`; no body text.
- `a_stop_with_asks_queued_reports_each_ask` — a channel whose `send` parks forever (so the pump
  is stuck on the first item); `outbox.try_deliver` 3 asks; shutdown → `lost` = 2 Ask lines (the
  first is in `send`, inside no guard: it is an `Ask` → the in-flight send of an ask is the
  residual; assert exactly 2) and none contains the ask body.
- `a_stop_with_replies_queued_leaves_them_unclaimed` — `Backlog` with 3 rows, a channel whose
  `send` parks after recording; start the bus; wait for 1 recorded send; shutdown; assert
  `disposition` of the two queued is `None`, `lost` has exactly 1 Reply line (the one in flight),
  and a new bus over the same `Backlog` with a recording channel sends the remaining two.
- `a_stop_mid_unroutable_claim_reports_it` — `Backlog` claim that parks for the unroutable task;
  shutdown → one Unroutable line.

- [ ] Write tests; run → fail. Implement `PumpQueue`, the inbound guard, the unroutable guard.
- [ ] Mutation: remove `PumpQueue`'s drain → `a_stop_with_asks_queued…` fails; settle the inbound
  guard before `handle_inbound` → `a_stop_mid_enqueue…` fails.
- [ ] Commit ("Say what a bus stop abandons: inbound, queued asks, unroutable claims (#832)").

### Task 6: `shutdown` reports a panicked pump

**Files:** Modify `core/src/channel/bus.rs`; test in `bus/tests/stop.rs`.

**Interfaces — Produces:** `pub async fn shutdown(self) -> usize` — the number of pumps that had
panicked; each logged `error!(error = %e, "channel bus pump panicked")`. Callers that ignore it
need no change.

- [ ] Failing test `a_panicked_pump_is_counted_at_shutdown`: a channel whose `recv` panics; wait
  for `death_signal`; `assert_eq!(bus.shutdown().await, 1)`.
- [ ] Implement (`if let Err(e) = h.await { if e.is_panic() { error!(…); n += 1 } }`); pass.
- [ ] Commit ("Bus shutdown no longer discards a pump panic (#832)").

### Task 7: PG e2e — stop with replies queued, restart, delivered

**Files:** Modify `core/tests/reply_catch_up_pg_e2e.rs` (new test; split the file first if it
would pass 500 lines).

- [ ] Test `replies_queued_at_a_stop_are_delivered_by_the_next_bus`: 3 finished tasks; bus A over a
  channel whose `send` records then parks forever; wait for 1 send; `bus_a.shutdown()`; assert
  task dispositions: exactly one `routed`, two `NULL`; bus B over a recording channel; eventually 2
  more sends with the two remaining bodies; all three `routed`.
- [ ] Run under `KASTELLAN_PG_REQUIRE_E2E=1 cargo test -p kastellan-core --test reply_catch_up_pg_e2e -- --nocapture`; confirm `[E2E]` and no `[SKIP]`.
- [ ] Commit ("PG e2e: replies queued at a bus stop reach the next bus (#832)").

### Task 8: Docs, sweep, handover, PR

**Files:** `core/src/channel/pg_events.rs` module doc (remove "still unsaid … (#832)" items now
said), `bus_outbound.rs::handle_completed` doc (the three exceptions), `docs/threat-model.md`
(the #825 residual paragraph), the operator runbook entry for the reply backlog, HANDOVER.md,
ROADMAP.md.

- [ ] Update docs; grep for "queue_closed" and "reserve" mentions that now lie.
- [ ] Full sweep: `cargo build --workspace` then `cargo test --workspace --no-fail-fast 2>&1 | tee ~/.local/state/kastellan/gate-logs/sweep-832.log` (whole log); reconcile per-suite counts vs 4934.
- [ ] Cold clippy in `CARGO_TARGET_DIR=$HOME/.cargo-clippy-832`; 27 `Checking kastellan`.
- [ ] `bash scripts/run-e2e-gate.sh pg` as evidence.
- [ ] Handover + roadmap; closing-keyword grep over commits and PR body; push via gh HTTPS; PR
  "Closes #832".
