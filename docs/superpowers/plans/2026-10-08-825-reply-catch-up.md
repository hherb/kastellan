# #825 Reply Catch-Up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every finished channel task is routed, recorded as undelivered, or recorded as unroutable, exactly once, whether or not a bus was listening when it finished.

**Architecture:** A claim column on `tasks` (`reply_settled_at` + `reply_disposition`, migration 0027) makes every route an atomic `UPDATE … WHERE reply_settled_at IS NULL`. The live NOTIFY path and a new backlog sweep both go through one function, `handle_completed`, which claims before it queues. The sweep runs at outbound-pump start and every 5 min. A reply more than 5 min late is prefixed with a relative-time note.

**Tech Stack:** Rust (tokio, sqlx 0.9, `time`), Postgres migrations embedded via `sqlx::migrate!`.

**Spec:** `docs/superpowers/specs/2026-10-08-825-reply-catch-up-design.md`, which is approved. Read it first: this plan argues from it.

## Global Constraints

- `cargo` is not on the non-interactive PATH: begin every shell with `source "$HOME/.cargo/env"`.
- Clippy is enforced: `cargo clippy --workspace --all-targets -- -D warnings` must stay clean.
- Never `touch` sources to force a rebuild (it falsifies the #687 image gate). The new migration is picked up because `db/migrations/` is read by `sqlx::migrate!` at compile time. If a test does not see 0027, run `cargo clean -p kastellan-db`.
- Files: keep new files under 500 lines. `core/src/channel/bus/tests.rs` (1269) and `core/tests/channel_bus_pg_e2e.rs` (819) are already over the cap: add **no** new tests to either; put them in new files.
- Never stage with `git add -A`; stage the files you touched by name.
- Commit messages go through a heredoc (`git commit -F - <<'EOF'`): backticks inside `-m "…"` are command-substituted by the shell.
- Every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- No closing keyword (`close[sd]?|fix(e[sd])?|resolve[sd]?` followed by `#N`) in any commit message. The PR body alone carries `Closes #825`.
- Terminal states, verbatim and in this order: `completed, failed, cancelled, blocked, timed_out, crashed, refused`.
- Constants: `LATE_AFTER` = 5 min, `SWEEP_EVERY` = 5 min, `SWEEP_PAGE` = 100.
- Note text, verbatim: `(Delayed reply — you sent this {age} ago.)`, followed by a blank line, then the body.
- Row field values, verbatim: `via` is `"notify"` / `"catch_up"`; the new action is `"channel.reply_unroutable"`; the dispositions are `'routed' | 'unroutable' | 'backfilled'`.
- Tests run from the repo's own `target/` (a private `CARGO_TARGET_DIR` breaks daemon e2es). PG tests need `KASTELLAN_PG_BIN_DIR` set; on this Mac that is `/Applications/Postgres 2.app/Contents/Versions/18/bin`.

## Review Focus

Five failure modes the spec implies but its listed tests don't exercise, most likely first. Each one has its pinning test in the owning task, marked **(RF n)**.

1. **Clock skew.** `finished_at` (Postgres's clock) is *after* the daemon's `now`: no note, no panic, no negative `delayed_secs`. Task 3.
2. **A transport that refuses every send (email today).** A caught-up reply is claimed once and refused once. One `reply_undelivered` row, and **no** re-send on later sweeps. Task 5.
3. **A backlog bigger than the per-channel queue (32) after a long outage.** All of it is delivered, with no deadlock between the sweep and the per-channel pump. Task 5.
4. **Delivery order.** Within a backlog, a peer receives replies in task-id order, oldest first. Task 5.
5. **A task for a channel no running bus serves.** It is never claimed by the sweep, and it does not starve the tasks after it. Task 5.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `db/migrations/0027_tasks_reply_settled.sql` (new) | columns, the pairing CHECK, the backfill, the partial index |
| `db/src/tasks/reply_claim.rs` (new) | `REPLIED_STATES`, `ReplyDisposition`, `ClaimedReply`, `claim_reply`, `unsettled_channel_replies` |
| `db/src/tasks.rs` | add `pub mod reply_claim;` (one line) |
| `db/src/tasks/turns.rs` | use the moved const; update its doc |
| `db/tests/reply_claim_e2e.rs` (new) | backfill, the claim's single winner, terminal-only, the state list, paging |
| `core/src/channel/catch_up.rs` (new) | pure: `Via`, `LATE_AFTER`, `SWEEP_EVERY`, `SWEEP_PAGE`, `lateness`, `delay_note`, `humanize` |
| `core/src/channel/bus_outbound.rs` (new, via a movement-only split) | `handle_completed`, `send_or_record`, `PgCompletedTasks`, and later `sweep` |
| `core/src/channel/bus.rs` | the `CompletedTasks` seam gains `claim`/`unsettled`; re-exports; the pump loop |
| `core/src/channel/mod.rs` | `pub mod catch_up; mod bus_outbound;`, `actions::REPLY_UNROUTABLE`, doc updates |
| `core/src/channel/undelivered.rs` | the `QueueClosed` doc |
| `core/src/channel/bus/tests/catch_up.rs` (new) | the PG-free claim/sweep tests |
| `core/src/channel/bus/tests.rs`, `bus/tests/dropped.rs` | fakes gain the two methods; the #815/#813 tests retargeted |
| `core/tests/{channel_bus_e2e,matrix_channel_e2e,email_channel_e2e}.rs` | fakes gain the two methods |
| `core/tests/channel_bus_pg_e2e.rs` | the `handle_completed` call gains `Via::Notify` (no new tests) |
| `core/tests/reply_catch_up_pg_e2e.rs` (new) | two real buses, a real backlog, the note, the live NOTIFY after |
| `docs/threat-model.md`, `docs/devel/runbooks/2026-06-12-matrix-live-and-email-dgx.md` | operator-facing row docs |

---

### Task 1: Split the outbound half of `bus.rs` (movement only)

`bus.rs` is 481 lines and is about to grow. Move three items verbatim into `bus_outbound.rs` and *prove* the move: byte-identical regions plus a matching `fn`-name set, plus a negative control showing the checker can fail.

**Files:**
- Create: `core/src/channel/bus_outbound.rs`
- Modify: `core/src/channel/bus.rs` (remove lines 161–291: `PgCompletedTasks` + its impl, `send_or_record`, `handle_completed`)
- Modify: `core/src/channel/mod.rs` (declare the module)

**Interfaces:**
- Produces: `crate::channel::bus_outbound::{handle_completed, PgCompletedTasks}` (pub, re-exported from `bus`); `send_or_record` becomes `pub(super)`. That is the **only** non-movement token change: the per-channel pump in `bus.rs` still calls it.

- [ ] **Step 1: Record the pre-move regions from `main`**

```bash
cd /Users/hherb/src/kastellan
mkdir -p "$HOME/.local/state/kastellan/move-825"
M="$HOME/.local/state/kastellan/move-825"
git show HEAD:core/src/channel/bus.rs > "$M/bus_before.rs"
# The three regions (doc comments included), verified by eye against the file:
sed -n '161,194p' "$M/bus_before.rs" > "$M/r1_pgcompleted.rs"   # /// Real `CompletedTasks` … end of impl
sed -n '196,225p' "$M/bus_before.rs" > "$M/r2_send_or_record.rs" # /// `ch.send(out)` … end of fn
sed -n '227,291p' "$M/bus_before.rs" > "$M/r3_handle_completed.rs" # /// Handle one completed-task id … end of fn
head -1 "$M"/r*.rs; tail -1 "$M"/r*.rs
```

Expected: each region starts with its `///` doc line and ends with a lone `}`. If a boundary is off, adjust the ranges before continuing.

- [ ] **Step 2: Create `bus_outbound.rs`** with this header, followed by the three regions **pasted verbatim** in the order r1, r2, r3, separated by one blank line:

```rust
//! The outbound half of the channel bus: completed task → route → the
//! owning channel's queue, and the transport attempt's failure row. Its own
//! module since #825 (`bus.rs` was at 481 lines, and the reply catch-up was
//! about to grow this half).

use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use kastellan_db::tasks;

use super::bus::{ChannelEvents, CompletedTasks};
use super::route::reply_for_completed_task;
use super::{actions, Channel, ChannelId, OutgoingMessage};
```

Then change exactly one token in the pasted r2: `async fn send_or_record(` → `pub(super) async fn send_or_record(`.

- [ ] **Step 3: Remove the regions from `bus.rs` and re-export**

Delete lines 161–291 of `bus.rs` (r1 through r3, including the blank lines between them). In their place:

```rust
/// The outbound half — `handle_completed`, `PgCompletedTasks` and the
/// transport-failure row — in its own module since #825.
pub use super::bus_outbound::{handle_completed, PgCompletedTasks};
use super::bus_outbound::send_or_record;
```

Fix the now-unused imports at the top of `bus.rs`. Expected removals: `debug` and `warn` from `tracing`, `self` from `kastellan_db::tasks::{self, Lane}` (→ `kastellan_db::tasks::Lane`), and `reply_for_completed_task`. Let the compiler say which; do not guess.

In `core/src/channel/mod.rs`, next to the existing `mod bus_inbound;` (or `pub mod bus_inbound;`, matching its visibility), add `mod bus_outbound;`.

- [ ] **Step 4: Prove the move**

```bash
M="$HOME/.local/state/kastellan/move-825"
F=core/src/channel/bus_outbound.rs
# Each region must occur verbatim in the new file (r2 after undoing its one sanctioned token).
python3 - "$M" "$F" <<'PY'
import sys, pathlib
m, f = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]).read_text()
f_norm = f.replace("pub(super) async fn send_or_record(", "async fn send_or_record(", 1)
bad = [r.name for r in sorted(m.glob("r*.rs")) if r.read_text() not in f_norm]
print("MISSING:", bad) if bad else print("all 3 regions byte-identical")
sys.exit(1 if bad else 0)
PY
# fn-name set: before == after across both files
fns() { grep -hoE '\bfn [a-z_0-9]+' "$@" | sort; }
diff <(fns "$M/bus_before.rs") <(fns core/src/channel/bus.rs core/src/channel/bus_outbound.rs) && echo "fn set identical"
# Negative control: one changed byte must be caught.
cp "$F" "$M/neg.rs"; sed -i '' 's/outbound load failed/outbound load faile/' "$M/neg.rs"
python3 - "$M" "$M/neg.rs" <<'PY' && echo "NEGATIVE CONTROL FAILED TO FAIL" || echo "negative control caught the change"
import sys, pathlib
m, f = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]).read_text()
f = f.replace("pub(super) async fn send_or_record(", "async fn send_or_record(", 1)
sys.exit(0 if all(r.read_text() in f for r in m.glob("r*.rs")) else 1)
PY
```

Expected: `all 3 regions byte-identical`, `fn set identical`, `negative control caught the change`.

- [ ] **Step 5: Build and run the bus tests**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib channel::bus 2>&1 | tail -5
```

Expected: `test result: ok.`, with the same count as before the move (run it once on the pre-move tree, or read the count from the previous sweep log).

- [ ] **Step 6: Commit**

```bash
git add core/src/channel/bus.rs core/src/channel/bus_outbound.rs core/src/channel/mod.rs
git commit -F - <<'EOF'
Split the bus's outbound half into bus_outbound.rs (movement only)

handle_completed, send_or_record and PgCompletedTasks move verbatim
(three regions checked byte-identical, fn-name set unchanged, negative
control caught a one-byte edit). The one token change: send_or_record
is pub(super), since the per-channel pump in bus.rs still calls it.
Ahead of #825, which grows this half.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

---

### Task 2: Migration 0027 and the DB claim layer

**Files:**
- Create: `db/migrations/0027_tasks_reply_settled.sql`
- Create: `db/src/tasks/reply_claim.rs`
- Modify: `db/src/tasks.rs` (one line: `pub mod reply_claim;` after `pub mod turns;`)
- Modify: `db/src/tasks/turns.rs` (lines 33–44 doc, 76–80 const, the `REPLIED_STATES` use at line 127)
- Test: `db/tests/reply_claim_e2e.rs` (new)

**Interfaces:**
- Produces (in `kastellan_db::tasks::reply_claim`):
  - `pub const REPLIED_STATES: [&str; 7]`
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum ReplyDisposition { Routed, Unroutable }` with `pub fn as_sql(self) -> &'static str`
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct ClaimedReply { pub created_at: OffsetDateTime, pub finished_at: Option<OffsetDateTime> }`
  - `pub async fn claim_reply(pool: &PgPool, task_id: i64, d: ReplyDisposition) -> Result<Option<ClaimedReply>, DbError>`
  - `pub async fn unsettled_channel_replies(pool: &PgPool, after_id: i64, limit: i64) -> Result<Vec<i64>, DbError>`

- [ ] **Step 1: Write the failing e2e** in `db/tests/reply_claim_e2e.rs`:

```rust
//! PG-gated e2e for the reply claim (#825, migration 0027). Skip-as-pass
//! without a supervisor/PG; live on the DGX, and on a Mac that exports
//! `KASTELLAN_PG_BIN_DIR`.

use kastellan_db::tasks::reply_claim::{
    claim_reply, unsettled_channel_replies, ReplyDisposition, REPLIED_STATES,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// The last migration before 0027. The backfill can only be observed on rows
/// that existed when 0027 ran, so that test stops here, seeds, then finishes.
const BEFORE_0027: i64 = 26;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn channel_payload() -> serde_json::Value {
    serde_json::json!({"kind": "channel", "instruction": "hi", "channel": "matrix",
                       "peer": "@me:srv", "conversation": "!room:srv"})
}

/// Insert a channel task and force it into `state` with raw SQL (the
/// production writers stamp `now()` and only walk the legal transitions).
async fn seed(pool: &sqlx::PgPool, payload: serde_json::Value, state: &str) -> i64 {
    let id = kastellan_db::tasks::insert_pending(pool, kastellan_db::tasks::Lane::Fast, payload)
        .await
        .expect("insert pending");
    sqlx::query("UPDATE tasks SET state = $2, finished_at = now() WHERE id = $1")
        .bind(id)
        .bind(state)
        .execute(pool)
        .await
        .expect("force state");
    id
}

async fn disposition(pool: &sqlx::PgPool, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT reply_disposition FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read disposition")
}

/// D4: history is settled at deploy, honestly labelled, and nothing else is.
#[test]
fn the_backfill_settles_only_finished_channel_tasks() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(&bin_dir, "rc-bd", "rc-bl",
        &format!("kastellan-supervisor-test-pg-rcb-{suffix}"));
    runtime().block_on(async {
        kastellan_db::probe::ensure_database_exists(&cluster.conn_spec).await.expect("db");
        let admin = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec).await.expect("admin");
        kastellan_db::MIGRATOR.run_to(BEFORE_0027, &admin).await.expect("migrate to 0026");

        // Raw INSERTs: `insert_pending` is fine at 0026, but the states must be forced.
        let ins = |state: &'static str, payload: serde_json::Value| {
            let admin = admin.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "INSERT INTO tasks (state, payload, finished_at) VALUES ($1, $2, \
                     CASE WHEN $1 = 'running' THEN NULL ELSE now() END) RETURNING id")
                    .bind(state).bind(payload).fetch_one(&admin).await.expect("seed")
            }
        };
        let done = ins("completed", channel_payload()).await;
        let crashed = ins("crashed", channel_payload()).await;
        let running = ins("running", channel_payload()).await;
        let not_channel = ins("completed", serde_json::json!({"kind": "ask"})).await;

        kastellan_db::MIGRATOR.run(&admin).await.expect("migrate 0027+");

        assert_eq!(disposition(&admin, done).await.as_deref(), Some("backfilled"));
        assert_eq!(disposition(&admin, crashed).await.as_deref(), Some("backfilled"));
        assert_eq!(disposition(&admin, running).await, None, "an unfinished task is not history");
        assert_eq!(disposition(&admin, not_channel).await, None, "not a channel task");
        let backlog = unsettled_channel_replies(&admin, 0, 100).await.expect("backlog");
        assert!(backlog.is_empty(), "nothing replays at deploy: {backlog:?}");
        admin.close().await;
    });
}

#[test]
fn the_claim_has_one_winner_and_only_for_finished_tasks() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(&bin_dir, "rc-cd", "rc-cl",
        &format!("kastellan-supervisor-test-pg-rcc-{suffix}"));
    runtime().block_on(async {
        kastellan_db::probe::run(&cluster.conn_spec, "core", "startup",
            serde_json::json!({"version": "test", "purpose": "reply-claim-e2e"}))
            .await.expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool_with_max(&cluster.conn_spec, 8)
            .await.expect("runtime pool");

        // Every terminal state is in the backlog, in id order (pins the
        // partial index's predicate against the const).
        let mut by_state = Vec::new();
        for state in REPLIED_STATES {
            by_state.push(seed(&pool, channel_payload(), state).await);
        }
        let pending = kastellan_db::tasks::insert_pending(
            &pool, kastellan_db::tasks::Lane::Fast, channel_payload()).await.expect("pending");
        assert_eq!(unsettled_channel_replies(&pool, 0, 100).await.unwrap(), by_state);

        // Paging: `after_id` is exclusive, `limit` is honoured.
        let page = unsettled_channel_replies(&pool, by_state[1], 2).await.unwrap();
        assert_eq!(page, by_state[2..4].to_vec());

        // A task that has not finished can never be settled.
        assert_eq!(claim_reply(&pool, pending, ReplyDisposition::Routed).await.unwrap(), None);
        assert_eq!(disposition(&pool, pending).await, None);

        // Eight concurrent claimers, one winner.
        let target = by_state[0];
        let claims = futures::future::join_all(
            (0..8).map(|_| claim_reply(&pool, target, ReplyDisposition::Routed)),
        ).await;
        let winners = claims.iter().filter(|c| matches!(c, Ok(Some(_)))).count();
        assert_eq!(winners, 1, "{claims:?}");
        let won = claims.into_iter().find_map(|c| c.ok().flatten()).unwrap();
        assert!(won.finished_at.is_some());
        assert_eq!(disposition(&pool, target).await.as_deref(), Some("routed"));

        // The other disposition is stored as itself.
        claim_reply(&pool, by_state[1], ReplyDisposition::Unroutable).await.unwrap().unwrap();
        assert_eq!(disposition(&pool, by_state[1]).await.as_deref(), Some("unroutable"));

        // Settled tasks leave the backlog.
        assert_eq!(unsettled_channel_replies(&pool, 0, 100).await.unwrap(), by_state[2..].to_vec());
        pool.close().await;
    });
}
```

Check `db/Cargo.toml` `[dev-dependencies]` for `futures`. If it is absent, add `futures = { workspace = true }`, or a `futures = "0.3"` line if the workspace has no entry (it is MIT/Apache; check how `core/Cargo.toml` lists it and copy that form).

- [ ] **Step 2: Run it to verify it fails**

```bash
source "$HOME/.cargo/env"
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test reply_claim_e2e 2>&1 | tail -5
```

Expected: a compile error, `unresolved import kastellan_db::tasks::reply_claim`.

- [ ] **Step 3: Write the migration** `db/migrations/0027_tasks_reply_settled.sql`:

```sql
-- 0027_tasks_reply_settled.sql
--
-- The reply claim (#825). Before this, the only trigger for a channel reply
-- was the `tasks_completed` NOTIFY, which Postgres never replays: a task that
-- finished while no bus was listening (a restart's backoff, a graceful
-- shutdown's scheduler drain, the boot crash sweep) or whose load failed
-- after the NOTIFY was consumed lost its reply with at most a WARN.
--
-- Now every route claims the task first:
--   UPDATE tasks SET reply_settled_at = now(), reply_disposition = …
--    WHERE id = $1 AND reply_settled_at IS NULL AND state IN (terminal)
-- so exactly one router wins — across the two buses (#497), and between the
-- live NOTIFY and the catch-up sweep, which walks the partial index below.
--
-- Design: docs/superpowers/specs/2026-10-08-825-reply-catch-up-design.md
--
-- ⚠️ The terminal-state list below is a reviewed copy of
-- `notify_task_completed`'s (0012) and of `REPLIED_STATES`
-- (db/src/tasks/reply_claim.rs). `db/tests/reply_claim_e2e.rs` pins this
-- index against the const; pinning the trigger is #712.

BEGIN;

ALTER TABLE tasks
    ADD COLUMN reply_settled_at  TIMESTAMPTZ,
    ADD COLUMN reply_disposition TEXT
        CHECK (reply_disposition IN ('routed', 'unroutable', 'backfilled')),
    ADD CONSTRAINT tasks_reply_settled_together
        CHECK ((reply_settled_at IS NULL) = (reply_disposition IS NULL));

-- History is settled, honestly labelled: whether those replies were delivered
-- is unknowable now, and without this the first sweep after deploy would
-- re-send every reply ever made.
UPDATE tasks
   SET reply_settled_at  = COALESCE(finished_at, updated_at),
       reply_disposition = 'backfilled'
 WHERE payload->>'kind' = 'channel'
   AND state IN ('completed','failed','cancelled','blocked','timed_out','crashed','refused');

-- Only the backlog is indexed, so a sweep over an empty backlog reads nothing.
CREATE INDEX tasks_unsettled_channel_replies ON tasks (id)
 WHERE reply_settled_at IS NULL
   AND payload->>'kind' = 'channel'
   AND state IN ('completed','failed','cancelled','blocked','timed_out','crashed','refused');

COMMIT;
```

Check whether 0026 (`db/migrations/0026_tasks_turn_record.sql`) wraps itself in `BEGIN;`/`COMMIT;`. sqlx already runs each migration in a transaction. If 0026 has no explicit `BEGIN`, drop these two lines to match it.

- [ ] **Step 4: Write `db/src/tasks/reply_claim.rs`**

```rust
//! The reply claim (#825): which finished channel tasks still owe their peer
//! a reply, and the atomic step that settles one.
//!
//! Every route of a channel reply — from the live `tasks_completed` NOTIFY or
//! from the catch-up sweep, on either bus (#497) — claims the task first. The
//! claim is an `UPDATE … WHERE reply_settled_at IS NULL`, so it has exactly one
//! winner and every other router backs off silently. It is **at-most-once**: a
//! task is settled when its reply is *routed*; a transport failure after that
//! is recorded as `channel.reply_undelivered`, never retried (see the spec's
//! D2 for why not at-least-once).

use sqlx::PgPool;
use sqlx::Row;
use time::OffsetDateTime;

use crate::DbError;

/// The terminal states: exactly the set `notify_task_completed` fires on
/// (migration 0005, widened with `refused` by 0012), which is the set a
/// channel peer is replied to for.
///
/// One Rust copy, shared by the claim, the backlog read and
/// `turns::conversation_turns`. Migration 0027's backfill and partial index
/// hold reviewed SQL copies; `db/tests/reply_claim_e2e.rs` pins the index
/// against this const. **If the trigger's list is ever widened, this list and
/// 0027's move with it** — machine-checking the trigger is #712.
pub const REPLIED_STATES: [&str; 7] = [
    "completed", "failed", "cancelled", "blocked", "timed_out", "crashed", "refused",
];

/// How a reply was settled by the claim. (`backfilled` exists only in
/// migration 0027: no code path settles a task that way.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyDisposition {
    /// Handed to the owning channel's outbound queue.
    Routed,
    /// A channel task with no routing metadata: there is no one to reply to.
    Unroutable,
}

impl ReplyDisposition {
    /// The stored spelling, matching 0027's CHECK.
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Routed => "routed",
            Self::Unroutable => "unroutable",
        }
    }
}

/// What the claim's winner learns about the task: enough to tell how late
/// its reply is (`core::channel::catch_up::lateness`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimedReply {
    pub created_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
}

fn states() -> Vec<String> {
    REPLIED_STATES.iter().map(|s| (*s).to_string()).collect()
}

/// Settle `task_id`'s reply as `d`. `Ok(None)` means someone else already
/// settled it (the other bus, the other path, an earlier sweep) **or** the
/// task has not finished — and a task that has not finished must never be
/// settled, or its real completion would find it already claimed.
pub async fn claim_reply(
    pool: &PgPool,
    task_id: i64,
    d: ReplyDisposition,
) -> Result<Option<ClaimedReply>, DbError> {
    let row = sqlx::query(
        "UPDATE tasks SET reply_settled_at = now(), reply_disposition = $2 \
          WHERE id = $1 AND reply_settled_at IS NULL AND state = ANY($3) \
          RETURNING created_at, finished_at",
    )
    .bind(task_id)
    .bind(d.as_sql())
    .bind(states())
    .fetch_optional(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks claim_reply: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(ClaimedReply {
        created_at: row
            .try_get("created_at")
            .map_err(|e| DbError::Query(format!("decode tasks.created_at: {e}")))?,
        finished_at: row
            .try_get("finished_at")
            .map_err(|e| DbError::Query(format!("decode tasks.finished_at: {e}")))?,
    }))
}

/// Up to `limit` ids of finished channel tasks whose reply is unsettled, with
/// `id > after_id`, ascending — so a sweep pages through the backlog oldest
/// first, and a caller that skips an id still moves past it. The predicate is
/// 0027's partial index's, so this reads only the backlog.
pub async fn unsettled_channel_replies(
    pool: &PgPool,
    after_id: i64,
    limit: i64,
) -> Result<Vec<i64>, DbError> {
    sqlx::query_scalar(
        "SELECT id FROM tasks \
          WHERE reply_settled_at IS NULL \
            AND payload->>'kind' = 'channel' \
            AND state = ANY($1) \
            AND id > $2 \
          ORDER BY id \
          LIMIT $3",
    )
    .bind(states())
    .bind(after_id)
    .bind(limit.max(0))
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks unsettled_channel_replies: {e}")))
}
```

⚠️ `state = ANY($1)` with a bound array may not match a partial index whose predicate spells `state IN (…)` literally, and the planner might then scan. That is a performance matter, not a correctness one. If `EXPLAIN` shows a seq scan on a large table, inline the literal list in this one query. Don't do it speculatively.

In `db/src/tasks.rs`, after `pub mod turns;`, add `pub mod reply_claim;`.

In `db/src/tasks/turns.rs`:
- Delete the `const REPLIED_STATES …` item (lines 76–80) together with its doc comment.
- Add `use super::reply_claim::REPLIED_STATES;` after `use crate::DbError;`.
- In the module doc's "Why this state list" section, replace the sentence beginning `[`REPLIED_STATES`] is exactly the set` with: `` [`REPLIED_STATES`](super::reply_claim::REPLIED_STATES) — shared with the reply claim since #825 — is exactly the set ``. Keep the rest of that paragraph.

- [ ] **Step 5: Run the e2e to verify it passes**

```bash
cargo test -p kastellan-db --test reply_claim_e2e -- --nocapture 2>&1 | grep -E "^test |test result|\[SKIP\]|\[E2E\]"
```

Expected: 2 passed, `[E2E]` lines, no `[SKIP]`. If the migration is not seen, run `cargo clean -p kastellan-db` and retry.

- [ ] **Step 6: Run the db crate's existing conversation tests** (they use the moved const)

```bash
cargo test -p kastellan-db --test conversation_turns_e2e 2>&1 | tail -3
cargo test -p kastellan-db --lib 2>&1 | tail -3
```

Expected: both `ok`.

- [ ] **Step 7: Mutation check** (copy the file, never `git checkout`):

```bash
cp db/src/tasks/reply_claim.rs "$HOME/.local/state/kastellan/move-825/rc.bak"
sed -i '' 's/AND reply_settled_at IS NULL AND state = ANY(\$3)/AND state = ANY($3)/' db/src/tasks/reply_claim.rs
cargo test -p kastellan-db --test reply_claim_e2e 2>&1 | grep -E "test result|panicked" | head -3   # expect FAILED (8 winners)
cp "$HOME/.local/state/kastellan/move-825/rc.bak" db/src/tasks/reply_claim.rs
sed -i '' 's/AND reply_settled_at IS NULL AND state = ANY(\$3)/AND reply_settled_at IS NULL/' db/src/tasks/reply_claim.rs
cargo test -p kastellan-db --test reply_claim_e2e 2>&1 | grep -E "test result|panicked" | head -3   # expect FAILED (pending claimed)
cp "$HOME/.local/state/kastellan/move-825/rc.bak" db/src/tasks/reply_claim.rs
git diff --stat db/src/tasks/reply_claim.rs   # expect: no output beyond the new-file state
```

Expected: both mutants fail (`compiled and failed`, not `did not compile`: look for `panicked`).

- [ ] **Step 8: Commit**

```bash
git add db/migrations/0027_tasks_reply_settled.sql db/src/tasks/reply_claim.rs db/src/tasks.rs \
        db/src/tasks/turns.rs db/tests/reply_claim_e2e.rs db/Cargo.toml
git commit -F - <<'EOF'
Reply claim: migration 0027 and db::tasks::reply_claim (#825)

reply_settled_at + reply_disposition on tasks; history backfilled as
'backfilled'; a partial index over the backlog only. claim_reply is an
UPDATE ... WHERE reply_settled_at IS NULL on a terminal task, so it has
one winner; unsettled_channel_replies pages the backlog by id.
REPLIED_STATES moves here from turns.rs, one Rust copy for both.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

(Drop `db/Cargo.toml` from the `git add` if it did not change.)

---

### Task 3: The pure catch-up module

**Files:**
- Create: `core/src/channel/catch_up.rs` (tests inline under `#[cfg(test)] mod tests`, since the module is small)
- Modify: `core/src/channel/mod.rs` (`pub mod catch_up;`)

**Interfaces:**
- Produces (`crate::channel::catch_up`):
  - `pub enum Via { Notify, CatchUp }` with `pub fn as_str(self) -> &'static str`
  - `pub const LATE_AFTER: time::Duration`, `pub const SWEEP_EVERY: std::time::Duration`, `pub const SWEEP_PAGE: i64`
  - `pub fn lateness(created_at: OffsetDateTime, finished_at: Option<OffsetDateTime>, now: OffsetDateTime) -> Option<time::Duration>`
  - `pub fn delay_note(created_at: OffsetDateTime, finished_at: Option<OffsetDateTime>, now: OffsetDateTime) -> Option<String>`
  - `pub fn humanize(d: time::Duration) -> String`

- [ ] **Step 1: Write the failing tests.** Create `catch_up.rs` containing only the test module below. Add `pub mod catch_up;` in `mod.rs` so it compiles to a failure.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;
    use time::Duration;

    const NOW: OffsetDateTime = datetime!(2026-10-08 12:00:00 UTC);

    #[test]
    fn a_reply_exactly_late_after_is_not_late() {
        let f = NOW - LATE_AFTER;
        assert_eq!(lateness(f - Duration::minutes(1), Some(f), NOW), None);
        assert_eq!(delay_note(f - Duration::minutes(1), Some(f), NOW), None);
    }

    #[test]
    fn a_reply_one_second_past_late_after_is_late() {
        let f = NOW - LATE_AFTER - Duration::seconds(1);
        assert_eq!(lateness(f, Some(f), NOW), Some(LATE_AFTER + Duration::seconds(1)));
        assert!(delay_note(f, Some(f), NOW).is_some());
    }

    #[test]
    fn the_age_is_counted_from_when_the_peer_asked() {
        // Asked 3 h 12 min ago, finished 3 h ago: the peer waited 3 h 12 min.
        let created = NOW - Duration::minutes(192);
        let finished = NOW - Duration::hours(3);
        assert_eq!(
            delay_note(created, Some(finished), NOW).as_deref(),
            Some("(Delayed reply — you sent this 3 h 12 min ago.)")
        );
    }

    #[test]
    fn a_missing_finished_at_falls_back_to_created_at() {
        let created = NOW - Duration::hours(1);
        assert_eq!(lateness(created, None, NOW), Some(Duration::hours(1)));
    }

    /// (RF 1) Postgres's clock ahead of the daemon's: never a note, never a
    /// negative lateness.
    #[test]
    fn a_finish_in_the_future_is_not_late() {
        let f = NOW + Duration::minutes(10);
        assert_eq!(lateness(NOW - Duration::hours(2), Some(f), NOW), None);
        assert_eq!(delay_note(NOW - Duration::hours(2), Some(f), NOW), None);
        assert_eq!(humanize(Duration::minutes(-5)), "1 min", "a negative age never renders");
    }

    #[test]
    fn humanize_has_two_units_at_most_and_never_zero_minutes() {
        assert_eq!(humanize(Duration::seconds(59)), "1 min");
        assert_eq!(humanize(Duration::minutes(47)), "47 min");
        assert_eq!(humanize(Duration::minutes(60)), "1 h");
        assert_eq!(humanize(Duration::minutes(192)), "3 h 12 min");
        assert_eq!(humanize(Duration::hours(24)), "1 day");
        assert_eq!(humanize(Duration::hours(25) + Duration::minutes(30)), "1 day 1 h");
        assert_eq!(humanize(Duration::days(2) + Duration::hours(4) + Duration::minutes(30)), "2 days 4 h");
        assert_eq!(humanize(Duration::days(3)), "3 days");
    }

    #[test]
    fn via_spells_the_row_values() {
        assert_eq!(Via::Notify.as_str(), "notify");
        assert_eq!(Via::CatchUp.as_str(), "catch_up");
    }
}
```

Check `core/Cargo.toml` for the `time` crate's `macros` feature (`datetime!`). If it is missing, build `NOW` with `OffsetDateTime::from_unix_timestamp(1_791_460_800).unwrap()` instead. Don't add a feature for a test.

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test -p kastellan-core --lib channel::catch_up 2>&1 | grep -E "error\[|cannot find" | head -5
```

Expected: `cannot find function lateness` (and its siblings).

- [ ] **Step 3: Implement** above the test module:

```rust
//! Pure pieces of the reply catch-up (#825): how late a reply is, the note
//! that says so, and the sweep's cadence. No I/O — the sweep itself lives
//! beside `handle_completed` in `bus_outbound`.
//!
//! A reply is *late* when it goes out more than [`LATE_AFTER`] after its task
//! finished — which only happens when the live NOTIFY was missed and the
//! sweep found it. Late replies are delivered anyway (the operator chose no
//! age cap) but say so, measured from when the peer *asked*, in relative
//! terms so the peer's time zone never matters.

use time::{Duration, OffsetDateTime};

/// Which path routed a reply — recorded as `via` on `channel.replied`, so an
/// operator can count how often catch-up actually saved one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// The live `tasks_completed` NOTIFY.
    Notify,
    /// The backlog sweep.
    CatchUp,
}

impl Via {
    /// The row's spelling — a committed operator-facing value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::CatchUp => "catch_up",
        }
    }
}

/// A reply this long after its task finished carries a delay note.
pub const LATE_AFTER: Duration = Duration::minutes(5);

/// How often the outbound pump re-sweeps the backlog while it runs. The
/// start-of-pump sweep covers everything missed while the bus was down; this
/// one covers a load or claim that failed while it was up.
pub const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Backlog ids read per page.
pub const SWEEP_PAGE: i64 = 100;

/// How late a reply sent `now` is, if it is late enough to say so. Measured
/// from `finished_at` (when the reply was due), falling back to `created_at`.
/// `None` for an on-time reply — and for a `finished_at` in the future, which
/// is clock skew between Postgres and the daemon, not a late reply.
pub fn lateness(
    created_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<Duration> {
    let late_by = now - finished_at.unwrap_or(created_at);
    (late_by > LATE_AFTER).then_some(late_by)
}

/// The line prepended to a late reply, or `None` when it is on time. Built
/// from timestamps only — never from task content.
pub fn delay_note(
    created_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<String> {
    lateness(created_at, finished_at, now)
        .map(|_| format!("(Delayed reply — you sent this {} ago.)", humanize(now - created_at)))
}

/// A duration in at most two units: `47 min`, `3 h 12 min`, `2 days 4 h`.
/// Never `0 min` (the floor is one minute), never negative.
pub fn humanize(d: Duration) -> String {
    let mins = (d.whole_minutes()).max(1);
    let (days, hours, m) = (mins / (24 * 60), (mins % (24 * 60)) / 60, mins % 60);
    let day_word = |n: i64| if n == 1 { "day" } else { "days" };
    match (days, hours, m) {
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} {}", day_word(d)),
        (d, h, _) => format!("{d} {} {h} h", day_word(d)),
    }
}
```

- [ ] **Step 4: Run to verify they pass**

```bash
cargo test -p kastellan-core --lib channel::catch_up 2>&1 | tail -3
```

Expected: 7 passed.

- [ ] **Step 5: Mutation check.** Change `late_by > LATE_AFTER` to `late_by >= LATE_AFTER`, and expect `a_reply_exactly_late_after_is_not_late` to FAIL. Then drop the `.max(1)`, and expect the humanize test to FAIL. Restore from a copy each time.

- [ ] **Step 6: Commit**

```bash
git add core/src/channel/catch_up.rs core/src/channel/mod.rs
git commit -F - <<'EOF'
catch_up: lateness, the delay note and the sweep cadence, pure (#825)

A reply more than 5 min past its task's finish is late; the note counts
from when the peer asked, in relative terms; a finish in the future
(clock skew) is never late.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

---

### Task 4: The claim in `handle_completed`

The seam gains two methods, and `handle_completed` gains the claim, the closed-queue hand-off, the unroutable row, `via` and `delayed_secs`. The sweep and the pump loop are Task 5.

**Files:**
- Modify: `core/src/channel/bus.rs` (the `CompletedTasks` trait; re-export the db types)
- Modify: `core/src/channel/bus_outbound.rs` (`handle_completed`, a new `settle_unroutable`, and `PgCompletedTasks`'s two new methods)
- Modify: `core/src/channel/mod.rs` (`actions::REPLY_UNROUTABLE`; update the `REPLIED` and `REPLY_UNDELIVERED` docs)
- Modify: `core/src/channel/undelivered.rs` (the `QueueClosed` doc)
- Create: `core/src/channel/bus/tests/catch_up.rs`
- Modify: `core/src/channel/bus/tests.rs` (`mod catch_up;` plus the `claim`/`unsettled` methods on `FakeCompleted`, `EndedCompleted` and `ParkingCompleted`; existing `handle_completed` calls gain `Via::Notify`)
- Modify: `core/src/channel/bus/tests/dropped.rs` (`GatedCompleted` gets the methods; two tests retargeted, see Step 6)
- Modify: `core/tests/channel_bus_e2e.rs`, `core/tests/matrix_channel_e2e.rs`, `core/tests/email_channel_e2e.rs` (fake methods), `core/tests/channel_bus_pg_e2e.rs` (`Via::Notify` at its one call)

**Interfaces:**
- Consumes: `kastellan_db::tasks::reply_claim::{claim_reply, unsettled_channel_replies, ClaimedReply, ReplyDisposition}` (Task 2); `crate::channel::catch_up::{Via, lateness, delay_note}` (Task 3).
- Produces:
  - `CompletedTasks::claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>>`
  - `CompletedTasks::unsettled(&self, after_id: i64, limit: i64) -> anyhow::Result<Vec<i64>>`
  - `pub use kastellan_db::tasks::reply_claim::{ClaimedReply, ReplyDisposition};` from `channel::bus`
  - `pub async fn handle_completed(completed: &dyn CompletedTasks, events: &dyn ChannelEvents, senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>, id: i64, via: Via) -> Option<OutgoingMessage>`
  - `actions::REPLY_UNROUTABLE: &str = "channel.reply_unroutable"`

- [ ] **Step 1: Write the failing tests** in `core/src/channel/bus/tests/catch_up.rs`. The fake here has real claim semantics. Task 5 adds more tests to this file, reusing the same fake.

```rust
//! #825: the reply claim, and the catch-up sweep that relies on it.
//!
//! `Backlog` is a `CompletedTasks` with real claim semantics in memory: a
//! task is claimable once, and only when it has a row. NOTIFY ids come in on
//! an mpsc the test holds, so a test decides when "Postgres" announces one.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::channel::catch_up::Via;

/// A channel task routed to `@me:srv` on `matrix`, answering `body`.
fn channel_row(body: &str) -> (Value, Option<Value>) {
    (
        serde_json::json!({"kind":"channel","channel":"matrix","peer":"@me:srv","conversation":"!room:srv"}),
        Some(serde_json::json!({"kind":"completed","message": body})),
    )
}

pub(super) struct Backlog {
    pub(super) rows: Mutex<HashMap<i64, (Value, Option<Value>)>>,
    /// When each task finished, relative to the claim's `now`.
    pub(super) finished_ago: time::Duration,
    pub(super) settled: Mutex<HashMap<i64, ReplyDisposition>>,
    /// Ids whose next `load` fails (each once).
    pub(super) load_fails_once: Mutex<HashSet<i64>>,
    /// When set, every `unsettled` call fails.
    pub(super) unsettled_fails: std::sync::atomic::AtomicBool,
    pub(super) unsettled_calls: AtomicUsize,
    pub(super) claim_calls: AtomicUsize,
    pub(super) notify_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<i64>>,
}

impl Backlog {
    pub(super) fn new(rows: Vec<(i64, (Value, Option<Value>))>) -> (Arc<Self>, mpsc::UnboundedSender<i64>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let backlog = Arc::new(Self {
            rows: Mutex::new(rows.into_iter().collect()),
            finished_ago: time::Duration::seconds(1),
            settled: Mutex::new(HashMap::new()),
            load_fails_once: Mutex::new(HashSet::new()),
            unsettled_fails: std::sync::atomic::AtomicBool::new(false),
            unsettled_calls: AtomicUsize::new(0),
            claim_calls: AtomicUsize::new(0),
            notify_rx: tokio::sync::Mutex::new(rx),
        });
        (backlog, tx)
    }

    pub(super) fn with_finished_ago(rows: Vec<(i64, (Value, Option<Value>))>, ago: time::Duration)
        -> (Arc<Self>, mpsc::UnboundedSender<i64>) {
        let (b, tx) = Self::new(rows);
        let mut b = Arc::try_unwrap(b).ok().expect("fresh");
        b.finished_ago = ago;
        (Arc::new(b), tx)
    }

    pub(super) fn disposition(&self, id: i64) -> Option<ReplyDisposition> {
        self.settled.lock().unwrap().get(&id).copied()
    }
}

#[async_trait::async_trait]
impl CompletedTasks for Arc<Backlog> {
    async fn next_completed(&mut self) -> Option<i64> {
        match self.notify_rx.lock().await.recv().await {
            Some(id) => Some(id),
            None => std::future::pending().await, // the test dropped its sender: park
        }
    }
    async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        if self.load_fails_once.lock().unwrap().remove(&id) {
            anyhow::bail!("SECRET-DB-TEXT: load refused");
        }
        Ok(self.rows.lock().unwrap().get(&id).cloned())
    }
    async fn claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
        self.claim_calls.fetch_add(1, Ordering::SeqCst);
        let mut settled = self.settled.lock().unwrap();
        if settled.contains_key(&id) || !self.rows.lock().unwrap().contains_key(&id) {
            return Ok(None);
        }
        settled.insert(id, d);
        let finished = time::OffsetDateTime::now_utc() - self.finished_ago;
        Ok(Some(ClaimedReply { created_at: finished - time::Duration::minutes(1), finished_at: Some(finished) }))
    }
    async fn unsettled(&self, after_id: i64, limit: i64) -> anyhow::Result<Vec<i64>> {
        self.unsettled_calls.fetch_add(1, Ordering::SeqCst);
        if self.unsettled_fails.load(Ordering::SeqCst) {
            anyhow::bail!("SECRET-DB-TEXT: backlog read refused");
        }
        let settled = self.settled.lock().unwrap();
        let mut ids: Vec<i64> = self.rows.lock().unwrap().keys().copied()
            .filter(|id| *id > after_id && !settled.contains_key(id)).collect();
        ids.sort_unstable();
        ids.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(ids)
    }
}

/// One open queue for `matrix`, plus its receiver.
fn matrix_sender() -> (HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>, mpsc::Receiver<OutgoingMessage>) {
    let (tx, rx) = mpsc::channel::<OutgoingMessage>(64);
    (HashMap::from([(ChannelId("matrix".into()), tx)]), rx)
}

fn actions_of(ev: &FakeEvents) -> Vec<String> {
    ev.audited.lock().unwrap().iter().map(|(a, _)| a.clone()).collect()
}

#[tokio::test]
async fn a_routed_reply_is_claimed_and_its_row_says_via() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let (senders, mut rx) = matrix_sender();

    let out = handle_completed(&backlog, &ev, &senders, 7, Via::Notify).await.expect("routed");

    assert_eq!(out.body, "done", "on time: no note");
    assert_eq!(rx.recv().await.unwrap().body, "done");
    assert_eq!(backlog.disposition(7), Some(ReplyDisposition::Routed));
    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].0, actions::REPLIED);
    assert_eq!(audited[0].1["via"], "notify");
    assert!(audited[0].1.get("delayed_secs").is_none(), "on time: {}", audited[0].1);
}

#[tokio::test]
async fn a_second_route_of_a_settled_task_sends_nothing_and_writes_nothing() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let (senders, mut rx) = matrix_sender();

    handle_completed(&backlog, &ev, &senders, 7, Via::Notify).await.expect("first wins");
    assert!(handle_completed(&backlog, &ev, &senders, 7, Via::CatchUp).await.is_none());

    rx.recv().await.unwrap();
    assert!(rx.try_recv().is_err(), "exactly one send");
    assert_eq!(actions_of(&ev), vec![actions::REPLIED.to_string()]);
}

#[tokio::test]
async fn a_late_reply_carries_the_note_and_delayed_secs() {
    let (backlog, _n) = Backlog::with_finished_ago(vec![(7, channel_row("done"))], time::Duration::hours(3));
    let ev = FakeEvents::default();
    let (senders, _rx) = matrix_sender();

    let out = handle_completed(&backlog, &ev, &senders, 7, Via::CatchUp).await.expect("routed");

    assert_eq!(out.body, "(Delayed reply — you sent this 3 h 1 min ago.)\n\ndone");
    let row = ev.audited.lock().unwrap()[0].1.clone();
    assert_eq!(row["via"], "catch_up");
    let secs = row["delayed_secs"].as_i64().expect("an integer");
    assert!((3 * 3600..3 * 3600 + 60).contains(&secs), "{row}");
}

/// The common closed-queue case is no longer a drop (#815's row): the next
/// bus's start sweep delivers it, so nothing is claimed and nothing written.
#[tokio::test]
async fn a_closed_queue_leaves_the_reply_for_catch_up() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let (senders, rx) = matrix_sender();
    drop(rx);

    assert!(handle_completed(&backlog, &ev, &senders, 7, Via::Notify).await.is_none());

    assert_eq!(backlog.disposition(7), None, "unclaimed: still in the backlog");
    assert_eq!(backlog.claim_calls.load(Ordering::SeqCst), 0);
    assert!(actions_of(&ev).is_empty(), "{:?}", actions_of(&ev));
}

#[tokio::test]
async fn a_failed_load_leaves_the_reply_for_catch_up() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    backlog.load_fails_once.lock().unwrap().insert(7);
    let ev = FakeEvents::default();
    let (senders, mut rx) = matrix_sender();

    assert!(handle_completed(&backlog, &ev, &senders, 7, Via::Notify).await.is_none());
    assert_eq!(backlog.disposition(7), None);

    // The next route (the sweep's) delivers it.
    handle_completed(&backlog, &ev, &senders, 7, Via::CatchUp).await.expect("delivered on retry");
    assert_eq!(rx.recv().await.unwrap().body, "done");
}

/// A channel task with no routing metadata has no one to reply to. The claim
/// makes its row exactly-once even with two buses seeing every NOTIFY.
#[tokio::test]
async fn an_unroutable_task_is_settled_with_one_row() {
    let row = (serde_json::json!({"kind":"channel","peer":"@me:srv"}), None); // no channel
    let (backlog, _n) = Backlog::new(vec![(9, row)]);
    let ev = FakeEvents::default();
    let (senders, _rx) = matrix_sender();

    for via in [Via::Notify, Via::CatchUp, Via::CatchUp] {
        assert!(handle_completed(&backlog, &ev, &senders, 9, via).await.is_none());
    }

    assert_eq!(backlog.disposition(9), Some(ReplyDisposition::Unroutable));
    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].0, "channel.reply_unroutable");
    assert_eq!(audited[0].1["task_id"], 9);
    assert!(audited[0].1["observed_at"].is_string(), "{}", audited[0].1);
}

#[tokio::test]
async fn a_reply_for_a_channel_this_bus_does_not_serve_is_not_claimed() {
    let mut row = channel_row("done");
    row.0["channel"] = "email".into();
    let (backlog, _n) = Backlog::new(vec![(7, row)]);
    let ev = FakeEvents::default();
    let (senders, _rx) = matrix_sender();

    assert!(handle_completed(&backlog, &ev, &senders, 7, Via::CatchUp).await.is_none());
    assert_eq!(backlog.disposition(7), None, "the email bus will route it");
    assert!(actions_of(&ev).is_empty());
}

/// The narrow race #815's `queue_closed` row still covers: the queue closes
/// between the closed-queue check and the send. Staged by a claim that drops
/// the queue's receiver before it returns.
#[tokio::test]
async fn a_queue_closing_after_the_claim_still_writes_queue_closed() {
    struct ClosingClaim {
        inner: Arc<Backlog>,
        rx: Mutex<Option<mpsc::Receiver<OutgoingMessage>>>,
    }
    #[async_trait::async_trait]
    impl CompletedTasks for ClosingClaim {
        async fn next_completed(&mut self) -> Option<i64> { std::future::pending().await }
        async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> { self.inner.load(id).await }
        async fn claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
            drop(self.rx.lock().unwrap().take()); // the pump ends right now
            self.inner.claim(id, d).await
        }
        async fn unsettled(&self, a: i64, l: i64) -> anyhow::Result<Vec<i64>> { self.inner.unsettled(a, l).await }
    }
    let (inner, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let (senders, rx) = matrix_sender();
    let completed = ClosingClaim { inner, rx: Mutex::new(Some(rx)) };
    let ev = FakeEvents::default();

    assert!(handle_completed(&completed, &ev, &senders, 7, Via::Notify).await.is_none());

    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].0, actions::REPLY_UNDELIVERED);
    assert_eq!(audited[0].1["reason"], "queue_closed");
}
```

Note: `handle_completed(&backlog, …)` passes `&Arc<Backlog>` as `&dyn CompletedTasks`, which works because the impl is on `Arc<Backlog>`.

Add to `core/src/channel/bus/tests.rs`, next to the other `mod` lines:

```rust
/// #825: the reply claim and the catch-up sweep.
mod catch_up;
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test -p kastellan-core --lib channel::bus::tests::catch_up 2>&1 | grep -E "^error" | head -5
```

Expected: errors on the missing `claim`/`unsettled` methods, `ClaimedReply`/`ReplyDisposition` and the fifth `handle_completed` argument.

- [ ] **Step 3: Extend the seam** in `bus.rs`. Replace the `CompletedTasks` trait with:

```rust
/// The reply claim's types, re-exported so `CompletedTasks` implementors in
/// other crates' tests need no direct `kastellan-db` path.
pub use kastellan_db::tasks::reply_claim::{ClaimedReply, ReplyDisposition};

/// Outbound source seam: a stream of completed task ids, a reader for the
/// row, and — since #825 — the reply claim and the backlog it settles.
#[async_trait::async_trait]
pub trait CompletedTasks: Send + Sync {
    /// Next completed task id, or `None` when the stream ends.
    async fn next_completed(&mut self) -> Option<i64>;
    /// Fetch `(payload, result)` for a task id, or `None` if absent.
    async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>>;
    /// Settle `id`'s reply as `d`. `Ok(None)`: already settled, or not
    /// finished — either way, not this caller's to route. See
    /// `kastellan_db::tasks::reply_claim::claim_reply`.
    async fn claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>>;
    /// Up to `limit` unsettled finished channel task ids above `after_id`,
    /// ascending.
    async fn unsettled(&self, after_id: i64, limit: i64) -> anyhow::Result<Vec<i64>>;
}
```

- [ ] **Step 4: Implement it in `bus_outbound.rs`**

Add to `impl CompletedTasks for PgCompletedTasks`:

```rust
    async fn claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
        Ok(kastellan_db::tasks::reply_claim::claim_reply(&self.pool, id, d).await?)
    }
    async fn unsettled(&self, after_id: i64, limit: i64) -> anyhow::Result<Vec<i64>> {
        Ok(kastellan_db::tasks::reply_claim::unsettled_channel_replies(&self.pool, after_id, limit).await?)
    }
```

Update the imports: `use super::bus::{ChannelEvents, ClaimedReply, CompletedTasks, ReplyDisposition};`, `use super::catch_up::{self, Via};`, and `use tracing::{debug, info, warn};`.

Replace `handle_completed` (keep its doc, extended as shown) with:

```rust
/// Handle one completed-task id on the outbound side: load it, route it
/// (pure), claim it, and queue it for the matching channel. The single path
/// for both the live NOTIFY and the catch-up sweep (`via` says which), so the
/// claim (#825) is what makes a reply go out once whichever path — and
/// whichever of the two buses (#497) — gets there first. `senders` maps
/// `ChannelId` → an outbound `send` handle. Returns the `OutgoingMessage`
/// queued (for tests).
///
/// Anything that stops a reply *before* the claim leaves it in the backlog
/// for the next sweep: a failed load, a failed claim, a closed queue. Only a
/// claimed reply can be lost, and that loss always has a row.
pub async fn handle_completed(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>,
    id: i64,
    via: Via,
) -> Option<OutgoingMessage> {
    let (payload, result) = match completed.load(id).await {
        Ok(Some(pr)) => pr,
        Ok(None) => return None, // rolled back between NOTIFY and SELECT — benign
        Err(e) => {
            // Not lost since #825: unclaimed, so the next sweep retries it.
            warn!(task_id = id, error = %e, "outbound load failed; reply left for catch-up");
            return None;
        }
    };
    let Some(mut out) = reply_for_completed_task(&payload, result.as_ref()) else {
        // `None` is the normal answer for a completion that is not a channel
        // task (an `ask`/`l3_run`). A channel task with no routing metadata
        // is a reply nobody can deliver: settle it, so the sweep stops
        // finding it, and let the claim's one winner say so.
        if payload.get("kind").and_then(Value::as_str) == Some("channel") {
            settle_unroutable(completed, events, id).await;
        }
        return None;
    };
    let Some(tx) = senders.get(&out.channel) else {
        // NOT a warning: the daemon runs one `ChannelBus` per channel family
        // and every bus sees every NOTIFY (#497), so "not a channel I serve"
        // is the normal case — the other bus routes it. Unclaimed on purpose.
        debug!(channel = %out.channel.0, "reply is for a channel this bus does not serve; ignoring");
        return None;
    };
    if tx.is_closed() {
        // The queue's only receiver is this channel's pump, so it has ended
        // and the bus is about to restart. Until #825 this was a drop with a
        // `queue_closed` row; now the reply stays unclaimed and the next
        // bus's start sweep delivers it.
        info!(channel = %out.channel.0, task_id = id, "send queue closed; reply left for catch-up");
        return None;
    }
    let claimed = match completed.claim(id, ReplyDisposition::Routed).await {
        Ok(Some(c)) => c,
        Ok(None) => return None, // already routed: the other path or the other bus
        Err(e) => {
            warn!(task_id = id, error = %e, "reply claim failed; reply left for catch-up");
            return None;
        }
    };
    let now = time::OffsetDateTime::now_utc();
    let late_by = catch_up::lateness(claimed.created_at, claimed.finished_at, now);
    if let Some(note) = catch_up::delay_note(claimed.created_at, claimed.finished_at, now) {
        out.body = format!("{note}\n\n{}", out.body);
    }
    if let Err(mpsc::error::SendError(dropped)) = tx.send(out.clone()).await {
        // The queue closed between the check above and this send. The reply
        // is claimed, so no sweep will find it again: this one IS lost, and
        // says so (#815).
        warn!(channel = %dropped.channel.0, "outbound send queue closed; reply dropped");
        let reply = super::UndeliveredReply::of(
            &dropped,
            super::UndeliveredReason::QueueClosed,
            now,
        );
        events.audit(actions::REPLY_UNDELIVERED, reply.payload()).await;
        return None;
    }
    let mut row = serde_json::json!({
        "task_id": id, "channel": out.channel.0, "peer": out.peer.0, "via": via.as_str(),
    });
    if let Some(late_by) = late_by {
        row["delayed_secs"] = late_by.whole_seconds().into();
    }
    events.audit(actions::REPLIED, row).await;
    Some(out)
}

/// Settle a channel task that has no routing metadata, and — for the claim's
/// one winner only — say so: a WARN and a `channel.reply_unroutable` row.
/// The row names the task, since there is no channel or peer to name.
async fn settle_unroutable(completed: &dyn CompletedTasks, events: &dyn ChannelEvents, id: i64) {
    match completed.claim(id, ReplyDisposition::Unroutable).await {
        Ok(Some(_)) => {
            warn!(task_id = id, "channel task has no routing metadata; reply dropped");
            let observed_at = super::undelivered::observed_at_json(time::OffsetDateTime::now_utc());
            events
                .audit(actions::REPLY_UNROUTABLE, serde_json::json!({"task_id": id, "observed_at": observed_at}))
                .await;
        }
        Ok(None) => {}
        Err(e) => warn!(task_id = id, error = %e, "unroutable reply claim failed; left for catch-up"),
    }
}
```

`observed_at_json` is `pub(crate)` in `undelivered.rs`; check the module path (`super::undelivered` vs a re-export in `mod.rs`) and use whichever compiles.

- [ ] **Step 5: Add the action** in `core/src/channel/mod.rs`'s `actions`, after `REPLY_UNDELIVERED`:

```rust
    /// A finished channel task's reply could not be routed at all: the task
    /// is `kind: "channel"` but carries no channel, peer or conversation, so
    /// there is no one to send it to (#825). Carries the `task_id` and the
    /// event's time (`observed_at`) only — there is no channel or peer to
    /// name. Written once, by whichever bus wins the task's reply claim
    /// (`kastellan_db::tasks::reply_claim`); before #825 this was a WARN on
    /// every bus, with no row.
    pub const REPLY_UNROUTABLE: &str = "channel.reply_unroutable";
```

Update the `REPLIED` doc. After its first paragraph, add:

```rust
    ///
    /// Since #825 the payload also carries `via` — `"notify"` (the live
    /// `tasks_completed` NOTIFY) or `"catch_up"` (the backlog sweep, for a
    /// reply the NOTIFY missed) — and, when the reply went out more than
    /// `catch_up::LATE_AFTER` after its task finished, `delayed_secs`. Every
    /// row is backed by the task's reply claim, so there is one per task.
```

In the `REPLY_UNDELIVERED` doc, replace the clause `and, since #815, `handle_completed`, when the channel's queue is closed because its pump has ended (`queue_closed`; that reply has **no** [`REPLIED`] row, since it was never routed)` with `and, since #815, `handle_completed`, when the channel's queue closes between the reply's claim and its queueing (`queue_closed`; that reply has **no** [`REPLIED`] row) — since #825 a queue found *already* closed leaves the reply unclaimed for the catch-up sweep instead, and writes nothing`.

Update `UndeliveredReason::QueueClosed`'s doc in `undelivered.rs` the same way: it now means "closed after the claim", and a queue found closed *before* the claim is handed to catch-up (#825).

- [ ] **Step 6: Bring every other `CompletedTasks` implementor and caller up to date**

To each of `FakeCompleted`, `EndedCompleted`, `ParkingCompleted` (`bus/tests.rs`), `GatedCompleted` (`bus/tests/dropped.rs`), and the fakes in `core/tests/channel_bus_e2e.rs`, `matrix_channel_e2e.rs` and `email_channel_e2e.rs`, add:

```rust
    /// Claims always succeed: this fake predates the reply claim (#825),
    /// whose own semantics are tested against `bus/tests/catch_up.rs`'s
    /// `Backlog` and real Postgres.
    async fn claim(&self, _id: i64, _d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
        let now = time::OffsetDateTime::now_utc();
        Ok(Some(ClaimedReply { created_at: now, finished_at: Some(now) }))
    }
    async fn unsettled(&self, _after_id: i64, _limit: i64) -> anyhow::Result<Vec<i64>> {
        Ok(Vec::new())
    }
```

In the integration tests, import the types with `use kastellan_core::channel::bus::{ClaimedReply, ReplyDisposition};`. Every existing `handle_completed(…, id)` / `handle_completed(…, 7)` call (in `bus/tests.rs`, `dropped.rs` and `core/tests/channel_bus_pg_e2e.rs`) gains `, Via::Notify` (import `crate::channel::catch_up::Via` or `kastellan_core::channel::catch_up::Via`).

Then retarget the two tests whose premise #825 changes, in `dropped.rs`:

1. Rename `a_reply_to_a_closed_send_queue_audits_queue_closed` to `a_reply_to_a_closed_send_queue_writes_no_row` and change its body's assertions to: `handle_completed` returns `None`, `ev.audited` is **empty**. Reword its doc: since #825 a closed queue leaves the reply unclaimed for the catch-up sweep; the claim-then-close race that still writes `queue_closed` is `catch_up::a_queue_closing_after_the_claim_still_writes_queue_closed`.
2. Retarget `a_queue_closed_row_abandoned_by_the_stop_is_reported` (#813's coverage through the outbound pump) to the `channel.replied` row. **Do not** drop `inbound_tx` and **do not** wait for the death signal: the per-channel pump stays alive, the reply is claimed (the fake's claim always succeeds) and queued, and the outbound pump then awaits the `channel.replied` insert on the stalled pool. Rename the test to `a_replied_row_abandoned_by_the_stop_is_reported`, and assert:

```rust
    assert!(
        lost[0].1.starts_with(
            r#"channel.replied row for channel "matrix", peer "@me:srv", task_id 7 may not have been written"#
        ),
        "{lost:?}"
    );
```

Rewrite its doc around the new sequence: a completion is routed and its `channel.replied` insert hangs on a wedged Postgres; the supervisor stops the bus; #813's guard reports the abandoned row. Keep the `SECRET_BODY` assertion. If `describe_row`'s field order differs from the string above, copy the order `pg_events/tests.rs` pins and don't change `describe_row`. Keep `drop(inbound_tx)` *after* `bus.shutdown().await`, so the channel is not ended early.

⚠️ **Swap `RefusingChannel` for a channel that accepts sends** in this test. With a refusing transport the per-channel pump's `send_or_record` awaits a *second* insert (`reply_undelivered`, `send_failed`) on the same stalled pool, and the test would see two lost rows instead of one. Define it in `dropped.rs`:

```rust
/// Accepts every send and parks on `recv`, so the only row the bus awaits is
/// the outbound pump's own `channel.replied`.
struct AcceptingChannel {
    inbound_rx: mpsc::Receiver<IncomingMessage>,
}
#[async_trait::async_trait]
impl Channel for AcceptingChannel {
    fn id(&self) -> ChannelId {
        ChannelId("matrix".into())
    }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        self.inbound_rx.recv().await
    }
    async fn send(&self, _msg: OutgoingMessage) -> anyhow::Result<()> {
        Ok(())
    }
}
```

- [ ] **Step 7: Run the tests**

```bash
cargo test -p kastellan-core --lib channel::bus 2>&1 | tail -4
cargo test -p kastellan-core --test channel_bus_e2e --test matrix_channel_e2e --test email_channel_e2e 2>&1 | grep "test result"
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-core --test channel_bus_pg_e2e 2>&1 | grep "test result"
```

Expected: all `ok`. The 8 new `catch_up` tests pass; the 2 retargeted `dropped` tests pass under their new names.

- [ ] **Step 8: Mutation check** (copy, never `git checkout`). Each mutant must FAIL a test:
  - Delete the `if tx.is_closed() { … }` block → `a_closed_queue_leaves_the_reply_for_catch_up` fails.
  - Change `Ok(None) => return None, // already routed` to send anyway (replace it with `Ok(None) => ClaimedReply { created_at: time::OffsetDateTime::now_utc(), finished_at: None },`) → `a_second_route_of_a_settled_task…` fails.
  - In `settle_unroutable`, write the row on `Ok(None)` too → `an_unroutable_task_is_settled_with_one_row` fails.

- [ ] **Step 9: Commit**

```bash
git add core/src/channel/bus.rs core/src/channel/bus_outbound.rs core/src/channel/mod.rs \
        core/src/channel/undelivered.rs core/src/channel/bus/tests.rs \
        core/src/channel/bus/tests/catch_up.rs core/src/channel/bus/tests/dropped.rs \
        core/tests/channel_bus_e2e.rs core/tests/matrix_channel_e2e.rs \
        core/tests/email_channel_e2e.rs core/tests/channel_bus_pg_e2e.rs
git commit -F - <<'EOF'
Claim every reply before routing it; leave the unclaimable for catch-up (#825)

handle_completed claims the task (reply_claim) before queueing, so a
reply goes out once whichever path and bus gets there first. A failed
load or claim, or a queue already closed, leaves it unclaimed for the
sweep instead of dropping it. A channel task with no routing metadata
is settled as unroutable with one channel.reply_unroutable row.
channel.replied gains via, and delayed_secs plus a note on a late reply.
#813's outbound coverage moves to the channel.replied row.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

---

### Task 5: The sweep and the pump loop

**Files:**
- Modify: `core/src/channel/bus_outbound.rs` (add `sweep`)
- Modify: `core/src/channel/bus.rs` (the outbound pump in `ChannelBus::spawn`; the `death_signal` doc only if it lists pump exits that change)
- Test: `core/src/channel/bus/tests/catch_up.rs` (append)

**Interfaces:**
- Consumes: `handle_completed(…, via)` and `Backlog` (Task 4); `catch_up::{SWEEP_EVERY, SWEEP_PAGE, Via}` (Task 3).
- Produces: `pub async fn sweep(completed: &dyn CompletedTasks, events: &dyn ChannelEvents, senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>)` (re-exported from `bus`).

- [ ] **Step 1: Write the failing tests** (append to `bus/tests/catch_up.rs`):

```rust
/// A channel whose `send` records, and whose `recv` parks on a sender the
/// test holds (dropping it is how a test would end the inbound pump).
struct RecordingChannel {
    inbound_rx: mpsc::Receiver<IncomingMessage>,
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
    refuse: bool,
}
#[async_trait::async_trait]
impl Channel for RecordingChannel {
    fn id(&self) -> ChannelId {
        ChannelId("matrix".into())
    }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        self.inbound_rx.recv().await
    }
    async fn send(&self, msg: OutgoingMessage) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(msg);
        if self.refuse {
            anyhow::bail!("test transport refuses (EmailChannel::send's shape)");
        }
        Ok(())
    }
}

struct Rig {
    bus: ChannelBus,
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
    ev: Arc<FakeEvents>,
    _inbound: mpsc::Sender<IncomingMessage>,
}

fn rig(backlog: &Arc<Backlog>, refuse: bool) -> Rig {
    let (inbound_tx, inbound_rx) = mpsc::channel(1);
    let sent = Arc::new(Mutex::new(Vec::new()));
    let ev = Arc::new(FakeEvents::default());
    let channel = RecordingChannel { inbound_rx, sent: sent.clone(), refuse };
    let bus = ChannelBus::spawn(
        vec![Box::new(channel)],
        Arc::new(StaticPairings::new()),
        None,
        ev.clone(),
        Box::new(backlog.clone()),
        None,
    );
    Rig { bus, sent, ev, _inbound: inbound_tx }
}

/// Poll `cond` until true, or fail after 5 s (real time; multi-thread tests).
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn bodies(sent: &Mutex<Vec<OutgoingMessage>>) -> Vec<String> {
    sent.lock().unwrap().iter().map(|m| m.body.clone()).collect()
}

/// The backlog left while no bus listened goes out when one starts.
/// (RF 4) In task-id order.
#[tokio::test(flavor = "multi_thread")]
async fn a_starting_bus_delivers_the_backlog_in_task_order() {
    let (backlog, _n) = Backlog::new(vec![(3, channel_row("c")), (1, channel_row("a")), (2, channel_row("b"))]);
    let r = rig(&backlog, false);
    eventually("three sends", || r.sent.lock().unwrap().len() == 3).await;
    assert_eq!(bodies(&r.sent), ["a", "b", "c"]);
    let rows = r.ev.audited.lock().unwrap().clone();
    assert!(rows.iter().all(|(a, p)| a == actions::REPLIED && p["via"] == "catch_up"), "{rows:?}");
    r.bus.shutdown().await;
}

/// A NOTIFY for a task the start sweep already routed sends nothing more.
#[tokio::test(flavor = "multi_thread")]
async fn a_notify_after_the_sweep_does_not_resend() {
    let (backlog, notify) = Backlog::new(vec![(1, channel_row("a"))]);
    let r = rig(&backlog, false);
    eventually("the sweep's send", || r.sent.lock().unwrap().len() == 1).await;
    notify.send(1).unwrap();
    // Let the pump take the NOTIFY: its claim call is the proof it ran.
    eventually("the NOTIFY's claim", || backlog.claim_calls.load(Ordering::SeqCst) == 2).await;
    assert_eq!(r.sent.lock().unwrap().len(), 1, "exactly one send");
    r.bus.shutdown().await;
}

/// (RF 5) A task for a channel no bus here serves is never claimed, and the
/// cursor moves past it: more than a page of them does not starve the one
/// behind.
#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_pages_past_tasks_it_does_not_serve() {
    let mut rows: Vec<(i64, (Value, Option<Value>))> = (1..=150)
        .map(|id| {
            let mut row = channel_row("not mine");
            row.0["channel"] = "email".into();
            (id, row)
        })
        .collect();
    rows.push((151, channel_row("mine")));
    let (backlog, _n) = Backlog::new(rows);
    let r = rig(&backlog, false);
    eventually("the served task", || r.sent.lock().unwrap().len() == 1).await;
    assert_eq!(bodies(&r.sent), ["mine"]);
    assert!((1..=150).all(|id| backlog.disposition(id).is_none()), "unserved stays unclaimed");
    r.bus.shutdown().await;
}

/// (RF 3) A backlog far bigger than the per-channel queue (32) all goes
/// out: the sweep waits on the queue while the per-channel pump drains it.
#[tokio::test(flavor = "multi_thread")]
async fn a_backlog_larger_than_the_queue_is_all_delivered() {
    let rows = (1..=80).map(|id| (id, channel_row(&format!("r{id}")))).collect();
    let (backlog, _n) = Backlog::new(rows);
    let r = rig(&backlog, false);
    eventually("eighty sends", || r.sent.lock().unwrap().len() == 80).await;
    r.bus.shutdown().await;
}

/// (RF 2) A transport that refuses every send — email's today — is tried
/// once per reply: claimed, refused, recorded, never swept again.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_catch_up_reply_is_not_resent_by_later_sweeps() {
    let (backlog, _n) = Backlog::new(vec![(1, channel_row("a"))]);
    let r = rig(&backlog, true);
    eventually("the refused send", || {
        r.ev.audited.lock().unwrap().iter().any(|(a, _)| a == actions::REPLY_UNDELIVERED)
    })
    .await;
    // A second sweep over the same backlog (as the periodic tick would run).
    let (senders, _rx) = matrix_sender();
    sweep(&backlog, &*r.ev, &senders).await;
    assert_eq!(r.sent.lock().unwrap().len(), 1, "one attempt");
    let undelivered = r.ev.audited.lock().unwrap().iter()
        .filter(|(a, _)| a == actions::REPLY_UNDELIVERED).count();
    assert_eq!(undelivered, 1);
    r.bus.shutdown().await;
}

/// A sweep that cannot read the backlog says so and carries on: it is not a
/// pump death, so the bell does not ring.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sweep_does_not_end_the_pump() {
    let (backlog, notify) = Backlog::new(vec![(1, channel_row("a"))]);
    backlog.unsettled_fails.store(true, Ordering::SeqCst);
    let r = rig(&backlog, false);
    eventually("the start sweep ran", || backlog.unsettled_calls.load(Ordering::SeqCst) >= 1).await;
    let died = tokio::time::timeout(std::time::Duration::from_millis(200), r.bus.death_signal()).await;
    assert!(died.is_err(), "a failed sweep must not ring the death bell");
    // The live path still works.
    notify.send(1).unwrap();
    eventually("the NOTIFY's send", || r.sent.lock().unwrap().len() == 1).await;
    r.bus.shutdown().await;
}

/// The periodic tick re-sweeps: a reply whose load failed while the bus
/// stayed up goes out on the next tick, without a restart.
#[tokio::test(start_paused = true)]
async fn the_periodic_tick_resweeps_the_backlog() {
    let (backlog, _n) = Backlog::new(vec![(1, channel_row("a"))]);
    backlog.load_fails_once.lock().unwrap().insert(1); // the start sweep's load fails
    let r = rig(&backlog, false);
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(backlog.unsettled_calls.load(Ordering::SeqCst), 1, "the start sweep only");
    assert!(r.sent.lock().unwrap().is_empty(), "its load failed");

    tokio::time::sleep(crate::channel::catch_up::SWEEP_EVERY + std::time::Duration::from_secs(1)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(backlog.unsettled_calls.load(Ordering::SeqCst) >= 2, "the tick swept again");
    assert_eq!(bodies(&r.sent), ["a"]);
    r.bus.shutdown().await;
}
```

Notes for the implementer:
- `IncomingMessage` and `StaticPairings` are already imported by the parent test module through `use super::*`.
- In the `start_paused` test, `tokio::time::sleep` auto-advances the paused clock. Five minutes elapse instantly.

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test -p kastellan-core --lib channel::bus::tests::catch_up 2>&1 | grep -E "^error|FAILED|panicked" | head -8
```

Expected: `cannot find function sweep`. Once that compiles (after Step 3 alone), the start-sweep tests fail, because no sweep runs yet.

- [ ] **Step 3: Implement `sweep`** in `bus_outbound.rs`:

```rust
/// Walk the reply backlog — finished channel tasks whose reply was never
/// settled — and route each through [`handle_completed`] (#825). What it
/// finds is what the live NOTIFY missed: a reply that finished while no bus
/// listened, whose load or claim failed, or whose queue was closed.
///
/// Pages by ascending id and moves the cursor past every id, including the
/// ones it skips (another bus's channel, a failed load), so a stuck set
/// cannot starve the rest. A backlog read that fails ends **this** sweep
/// with a WARN; the pump carries on and the next sweep retries.
pub async fn sweep(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>,
) {
    let mut after = 0;
    loop {
        let page = match completed.unsettled(after, catch_up::SWEEP_PAGE).await {
            Ok(page) => page,
            Err(e) => {
                warn!(error = %e, "reply catch-up sweep could not read the backlog; retrying next sweep");
                return;
            }
        };
        let Some(&last) = page.last() else { return };
        for &id in &page {
            handle_completed(completed, events, senders, id, Via::CatchUp).await;
        }
        if i64::try_from(page.len()).unwrap_or(i64::MAX) < catch_up::SWEEP_PAGE {
            return;
        }
        after = last;
    }
}
```

Re-export it from `bus.rs`: `pub use super::bus_outbound::{handle_completed, sweep, PgCompletedTasks};`.

- [ ] **Step 4: Rewrite the outbound pump** in `ChannelBus::spawn`. Replace the block from `// Outbound pump: NOTIFY → load → route → push into the per-channel sender.` through its `handles.push(…);` with:

```rust
        // Outbound pump: sweep the backlog, then NOTIFY → load → route →
        // claim → push into the per-channel sender, re-sweeping every
        // `SWEEP_EVERY` (#825). The start sweep needs no ordering care:
        // `completed`'s LISTEN was established before `spawn` was called, so
        // a task finishing during the sweep is announced afterwards and its
        // claim drops the duplicate.
        let events_out = events.clone();
        let life = bell.guard();
        handles.push(tokio::spawn(async move {
            let _life = life;
            sweep(&*completed, &*events_out, &senders).await;
            let every = super::catch_up::SWEEP_EVERY;
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // A tick that wins this race drops an in-flight
                // `next_completed`, and sqlx does not document
                // `PgListener::recv` as cancel-safe. A NOTIFY lost that way
                // is still not a lost reply: its task stays unsettled, and
                // the very sweep that won the race finds it.
                tokio::select! {
                    next = completed.next_completed() => match next {
                        Some(id) => {
                            handle_completed(&*completed, &*events_out, &senders, id, Via::Notify).await;
                        }
                        None => break,
                    },
                    _ = tick.tick() => sweep(&*completed, &*events_out, &senders).await,
                }
            }
            info!("outbound pump stopped");
        }));
```

Add `use super::catch_up::Via;` to `bus.rs`. If the borrow checker rejects `&*completed` in the tick handler while `completed.next_completed()` borrows mutably in the other branch, bind the branch result first: `let next = tokio::select! { n = completed.next_completed() => Some(n), _ = tick.tick() => None };` and then match on it outside the `select!`. tokio drops the branch futures before running handlers, and the per-channel pump above already uses `ch` in both arms, so the inline form is expected to compile.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p kastellan-core --lib channel::bus 2>&1 | tail -4
```

Expected: all pass, including the 7 new sweep tests and the existing `a_dead_outbound_pump_fires_the_death_signal`. (`EndedCompleted` still ends the stream; its sweep reads an empty backlog first.)

- [ ] **Step 6: Mutation check** (copy, never `git checkout`). Each mutant must FAIL a test:
  - Delete the start-of-pump `sweep(…)` call → `a_starting_bus_delivers_the_backlog_in_task_order` fails.
  - In `sweep`, delete `after = last;` → `the_sweep_pages_past_tasks_it_does_not_serve` times out ("timed out waiting"), and does not hang forever, because `eventually` bounds it.
  - Change the tick arm's body to `{}` → `the_periodic_tick_resweeps_the_backlog` fails.

- [ ] **Step 7: Commit**

```bash
git add core/src/channel/bus.rs core/src/channel/bus_outbound.rs core/src/channel/bus/tests/catch_up.rs
git commit -F - <<'EOF'
Sweep the reply backlog at pump start and every 5 min (#825)

The outbound pump routes every unsettled finished channel task through
handle_completed before taking NOTIFYs, and again on a 5-minute tick:
a reply that finished while no bus listened, or whose load failed while
one did, now goes out. The sweep pages by id past what it skips, and a
failed backlog read ends that sweep only, never the pump.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

---

### Task 6: End to end against real Postgres

**Files:**
- Create: `core/tests/reply_catch_up_pg_e2e.rs`

**Interfaces:**
- Consumes: `ChannelBus::spawn`, `PgCompletedTasks::connect`, `PgChannelEvents::new` (`kastellan_core::channel::bus`), `kastellan_db::tasks::{insert_pending, claim_one, finalize, Lane}`, `kastellan_db::audit::fetch_since`.

- [ ] **Step 1: Write the test**

```rust
//! PG-gated e2e for the reply catch-up (#825): replies that finished while no
//! bus listened are delivered when the buses start — once, across two buses
//! (#497) — the late one with its note, and the live NOTIFY path still works
//! after. Skip-as-pass without a supervisor/PG.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use kastellan_core::channel::auth::StaticPairings;
use kastellan_core::channel::bus::{ChannelBus, PgChannelEvents, PgCompletedTasks};
use kastellan_core::channel::{actions, Channel, ChannelId, IncomingMessage, OutgoingMessage};
use kastellan_db::tasks::{self, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

struct RecordingChannel {
    inbound_rx: mpsc::Receiver<IncomingMessage>,
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
}
#[async_trait::async_trait]
impl Channel for RecordingChannel {
    fn id(&self) -> ChannelId {
        ChannelId("matrix".into())
    }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        self.inbound_rx.recv().await
    }
    async fn send(&self, msg: OutgoingMessage) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(msg);
        Ok(())
    }
}

fn payload() -> serde_json::Value {
    serde_json::json!({"kind": "channel", "instruction": "q", "channel": "matrix",
                       "peer": "@me:srv", "conversation": "!room:srv"})
}

/// A channel task that finished `finished_ago` ago, asked one minute before
/// that, answering `body` — written with raw SQL because the production
/// writers stamp `now()`.
async fn finished_task(pool: &sqlx::PgPool, body: &str, finished_ago_mins: i32) -> i64 {
    let id = tasks::insert_pending(pool, Lane::Fast, payload()).await.expect("insert");
    sqlx::query(
        "UPDATE tasks SET state = 'completed', \
                finished_at = now() - make_interval(mins => $2), \
                created_at  = now() - make_interval(mins => $2 + 1), \
                result = $3 WHERE id = $1",
    )
    .bind(id)
    .bind(finished_ago_mins)
    .bind(serde_json::json!({"kind": "text", "body": body}))
    .execute(pool)
    .await
    .expect("finish");
    id
}

async fn spawn_bus(pool: &sqlx::PgPool, sent: &Arc<Mutex<Vec<OutgoingMessage>>>)
    -> (ChannelBus, mpsc::Sender<IncomingMessage>) {
    let (tx, inbound_rx) = mpsc::channel(1);
    let completed = PgCompletedTasks::connect(pool.clone()).await.expect("LISTEN");
    let bus = ChannelBus::spawn(
        vec![Box::new(RecordingChannel { inbound_rx, sent: sent.clone() })],
        Arc::new(StaticPairings::new()),
        None,
        Arc::new(PgChannelEvents::new(pool.clone())),
        Box::new(completed),
        None,
    );
    (bus, tx)
}

async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missed_replies_are_delivered_once_when_the_buses_start() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(&bin_dir, "rcu-d", "rcu-l",
        &format!("kastellan-supervisor-test-pg-rcu-{suffix}"));
    kastellan_db::probe::run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"version": "test", "purpose": "reply-catch-up-e2e"}))
        .await.expect("probe");
    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    // Finished while no bus listened: one three hours ago, one just now.
    let late = finished_task(&pool, "late answer", 180).await;
    let fresh = finished_task(&pool, "fresh answer", 0).await;

    // Two buses serving the same channel: the claim must pick one router.
    let sent = Arc::new(Mutex::new(Vec::new()));
    let (bus_a, _ia) = spawn_bus(&pool, &sent).await;
    let (bus_b, _ib) = spawn_bus(&pool, &sent).await;

    eventually("two catch-up sends", || sent.lock().unwrap().len() >= 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // room for a wrong third
    let bodies: Vec<String> = sent.lock().unwrap().iter().map(|m| m.body.clone()).collect();
    assert_eq!(bodies.len(), 2, "once each, across two buses: {bodies:?}");
    assert!(bodies.contains(&"(Delayed reply — you sent this 3 h 1 min ago.)\n\nlate answer".to_string()),
        "{bodies:?}");
    assert!(bodies.contains(&"fresh answer".to_string()), "{bodies:?}");

    for id in [late, fresh] {
        let d: Option<String> = sqlx::query_scalar("SELECT reply_disposition FROM tasks WHERE id = $1")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(d.as_deref(), Some("routed"));
    }

    // The live path still works after the sweep, and still once.
    let live = tasks::insert_pending(&pool, Lane::Fast, payload()).await.expect("insert live");
    let claimed = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().expect("claim");
    assert_eq!(claimed.id, live);
    tasks::finalize(&pool, live, "completed",
        Some(serde_json::json!({"kind": "text", "body": "live answer"})), None)
        .await.expect("finalize");
    eventually("the live send", || sent.lock().unwrap().len() >= 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(sent.lock().unwrap().len(), 3, "the NOTIFY reached both buses; one sent");

    bus_a.shutdown().await;
    bus_b.shutdown().await;

    let rows = kastellan_db::audit::fetch_since(&pool, 0, 500).await.expect("audit");
    let replied: Vec<_> = rows.iter().filter(|r| r.action == actions::REPLIED).collect();
    assert_eq!(replied.len(), 3, "one channel.replied per task");
    let row_for = |id: i64| replied.iter().find(|r| r.payload["task_id"] == id).expect("row").payload.clone();
    assert_eq!(row_for(late)["via"], "catch_up");
    assert!(row_for(late)["delayed_secs"].as_i64().unwrap() >= 3 * 3600);
    assert_eq!(row_for(fresh)["via"], "catch_up");
    assert!(row_for(fresh).get("delayed_secs").is_none());
    assert_eq!(row_for(live)["via"], "notify");
    pool.close().await;
}
```

Check the audit row type returned by `kastellan_db::audit::fetch_since` before relying on `r.payload`: the field may be named differently (read `db/src/audit.rs`). If `async_trait` is not a `core` dev-dependency usable from integration tests, copy the import form `core/tests/channel_bus_e2e.rs` uses.

- [ ] **Step 2: Run it**

```bash
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
KASTELLAN_PG_REQUIRE_E2E=1 cargo test -p kastellan-core --test reply_catch_up_pg_e2e -- --nocapture 2>&1 \
  | grep -aE "^test |test result|\[SKIP\]|\[E2E\]"
```

Expected: 1 passed, `[E2E]` lines, 0 `[SKIP]`. If it fails, the earlier tasks have a defect. Use superpowers:systematic-debugging; do not weaken the test.

- [ ] **Step 3: Negative control.** Temporarily make `PgCompletedTasks::claim` ignore the db claim and always return `Some(ClaimedReply{ now, Some(now) })` (copy-backup first). Expected: the test FAILS with `once each, across two buses` (4 sends). Restore.

- [ ] **Step 4: Commit**

```bash
git add core/tests/reply_catch_up_pg_e2e.rs
git commit -F - <<'EOF'
e2e: missed replies are delivered once when two buses start (#825)

Against real Postgres: two tasks finished with no bus listening reach
the peer once each across two buses, the 3-hour-old one with its note;
a live NOTIFY afterwards still routes once; one channel.replied per task
with via and delayed_secs.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

---

### Task 7: Operator docs, the gate, the handover, the PR

**Files:**
- Modify: `docs/threat-model.md` (the negative-tests list, after the `#826` `channel.inbound_dropped` bullet at line ~289)
- Modify: `docs/devel/runbooks/2026-06-12-matrix-live-and-email-dgx.md` (where it names `channel.inbound_dropped` / `rejected_unverifiable`)
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Threat model.** Add one bullet after the `channel.inbound_dropped` bullet:

```markdown
- `channel`: a channel task that **finishes while no bus is listening** — a restart's backoff, a graceful shutdown's scheduler drain, the boot crash sweep — or whose load fails after its `tasks_completed` NOTIFY was consumed → not lost: every route **claims** the task first (`tasks.reply_settled_at`, migration 0027, an `UPDATE … WHERE reply_settled_at IS NULL`, so exactly one bus routes it), and an outbound **catch-up sweep** at bus start and every 5 min routes whatever is unsettled. A reply more than 5 min late is prefixed with a relative-time note; `channel.replied` records `via` (`notify`/`catch_up`) and `delayed_secs`. A channel task with no routing metadata is settled once as `channel.reply_unroutable` (task id only). At-most-once: a claimed reply whose send fails is `channel.reply_undelivered`, never retried (#825). (Shipped: `db/src/tasks/reply_claim.rs`, `channel/bus_outbound.rs`, `channel/catch_up.rs`; `core/tests/reply_catch_up_pg_e2e.rs`.)
```

- [ ] **Step 2: Runbook.** Find the paragraph that lists the channel rows (`grep -n "inbound_dropped" docs/devel/runbooks/2026-06-12-matrix-live-and-email-dgx.md`) and add, in the same style:
  - `channel.replied`'s `via`/`delayed_secs`;
  - `channel.reply_unroutable`;
  - a monitoring query: `select payload->>'via', count(*) from audit_log where action = 'channel.replied' group by 1;`. A non-zero `catch_up` count is replies the NOTIFY missed.
  - A backlog check: `select count(*) from tasks where reply_settled_at is null and payload->>'kind' = 'channel' and state in ('completed','failed','cancelled','blocked','timed_out','crashed','refused');`. It should be 0, or briefly small; a growing number means no bus serves that channel.

- [ ] **Step 3: Full gate sweep.** Predict the count first, from the previous baseline 4906/0/52 over 193 suites:
  - db `reply_claim_e2e`: +2, as a new suite (194);
  - core lib: +7 `catch_up` pure, +8 claim, +7 sweep (the 2 retargeted `dropped` tests are renames, net 0);
  - core `reply_catch_up_pg_e2e`: +1, as a new suite (195).

Predicted: **4931 / 0 / 52 over 195 suites**. Reconcile any delta per suite, exactly.

```bash
source "$HOME/.cargo/env"
pkill -f "cargo check --workspace --message-format" || true
cargo build --workspace 2>&1 | tail -2
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
LOG="$HOME/.local/state/kastellan/gate-logs/sweep-825.log"
cargo test --workspace --no-fail-fast -- --test-threads=4 --nocapture > "$LOG" 2>&1; echo "TEST_EXIT=$?" >> "$LOG"
grep -a "TEST_EXIT" "$LOG"
grep -a "^test result:" "$LOG" | awk '{p+=$4; f+=$6; i+=$8; n++} END {print p" passed / "f" failed / "i" ignored over "n" suites"}'
grep -ac "\[WARN\]" "$LOG"; grep -ac "\[SKIP\]" "$LOG"
```

Expected: `TEST_EXIT=0`, the predicted totals, `[WARN]` 0, `[SKIP]` 23 (the same as the baseline). Known flakes (`scheduler_ask_expiry_e2e`, `asks_e2e`, `conversation_turns_e2e`, showing `the database system is starting up`) are re-run in isolation and noted. They are never ignored silently.

- [ ] **Step 4: Cold clippy**

```bash
CARGO_TARGET_DIR=$HOME/.cargo-clippy-825 cargo clippy --workspace --all-targets -- -D warnings 2>&1 \
  | tee "$HOME/.local/state/kastellan/gate-logs/clippy-825.log" | tail -3
grep -c "Checking kastellan" "$HOME/.local/state/kastellan/gate-logs/clippy-825.log"
```

Expected: exit 0, and **27** `Checking kastellan` lines. Then `rm -rf $HOME/.cargo-clippy-825`.

- [ ] **Step 5: Handover + ROADMAP.** Follow the checklist at the bottom of HANDOVER.md:
  - a "This session" block for #825: what binds, which is the claim, at-most-once, the closed-queue hand-off, the `select!` cancel note, and the four copies of the state list;
  - a test-baseline row reconciled against 4906;
  - move #825 out of Next TODO;
  - TODO 5's deploy note: **migration 0027 runs at deploy** and backfills history;
  - the header PR list.

HANDOVER.md is at 514 lines. Snapshot it to `archive/handover_20261008_825_pre-prune.md`, prune the file to under 500 lines, and **repoint the header's archive link in the same commit**. Add a ROADMAP line.

- [ ] **Step 6: Commit, push, PR**

```bash
git add docs/threat-model.md docs/devel/runbooks/2026-06-12-matrix-live-and-email-dgx.md \
        docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md docs/devel/handovers/archive/handover_20261008_825_pre-prune.md
git commit -F - <<'EOF'
Handover + roadmap + operator docs: the reply catch-up (#825)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
# Closing-keyword check over every commit on the branch:
git log --format=%B origin/main..HEAD | grep -niE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+' && echo "REMOVE THESE" || echo "clean"
git -c credential.helper='!gh auth git-credential' push -u https://github.com/hherb/kastellan.git fix/825-reply-catch-up
gh pr create --base main --head fix/825-reply-catch-up --title "Reply catch-up: no finished channel task leaves without a trace (#825)" --body-file <(printf '%s\n' "Closes #825" "" "…summary, test evidence, the behaviour change (closed queue → catch-up, no queue_closed row), the deploy note (migration 0027 backfills)…" "" "🤖 Generated with [Claude Code](https://claude.com/claude-code)")
```

Write the real body; the `…` above stands for prose to be written from the session, not to be pasted. Then grep the body for closing keywords: only `Closes #825` may match.
