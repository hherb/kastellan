//! Typed CRUD against the `tasks` table.
//!
//! All writes go through this module; the scheduler never builds raw
//! SQL. Reads are typed too (no `serde_json::Value` leaking out where
//! a `Task` would do).

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use time::Duration;

use crate::DbError;

pub mod turns;

/// The two concurrency lanes. `fast` is the default; `long` is opt-in
/// via the producer (CLI flag, channel adapter default, etc.).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Fast,
    Long,
}

impl Lane {
    pub fn as_sql(self) -> &'static str {
        match self {
            Lane::Fast => "fast",
            Lane::Long => "long",
        }
    }

    pub fn from_sql(s: &str) -> Result<Self, DbError> {
        match s {
            "fast" => Ok(Lane::Fast),
            "long" => Ok(Lane::Long),
            other => Err(DbError::Other(format!("unknown lane: {other}"))),
        }
    }
}

/// Default deadlines per lane. Used at claim time when the producer
/// does not pin `payload.deadline_seconds` itself.
pub const DEFAULT_DEADLINE_FAST_S: i64 = 60;
pub const DEFAULT_DEADLINE_LONG_S: i64 = 30 * 60;

/// Default plan-iteration caps per lane. Mirror values in
/// `core::scheduler` so a producer omitting the cap gets the same
/// behaviour as the runner enforces. Fast is 5 (not 3): with step
/// error `code`/`detail` now fed back into the planner prompt the
/// agent can actually recover across replans, so a couple of extra
/// attempts buy real convergence rather than blind flailing.
pub const DEFAULT_MAX_PLANS_FAST: u32 = 5;
pub const DEFAULT_MAX_PLANS_LONG: u32 = 12;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_round_trips_through_sql_string() {
        assert_eq!(Lane::Fast.as_sql(), "fast");
        assert_eq!(Lane::Long.as_sql(), "long");
        assert_eq!(Lane::from_sql("fast").unwrap(), Lane::Fast);
        assert_eq!(Lane::from_sql("long").unwrap(), Lane::Long);
        assert!(Lane::from_sql("medium").is_err());
    }
}

/// One decoded `tasks` row.
#[derive(Clone, Debug)]
pub struct Task {
    pub id: i64,
    pub state: String,
    pub lane: Lane,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub plan_count: i32,
    pub payload: serde_json::Value,
    pub result: Option<serde_json::Value>,
}

/// Insert a fresh `pending` task row. The `tasks_inserted` trigger
/// will fire `pg_notify('tasks_inserted', NEW.id::text)` for any
/// listeners (the lane runner of the matching lane).
pub async fn insert_pending(
    pool: &PgPool,
    lane: Lane,
    payload: serde_json::Value,
) -> Result<i64, DbError> {
    let row = sqlx::query(
        "INSERT INTO tasks (state, lane, payload) \
         VALUES ('pending', $1, $2) \
         RETURNING id",
    )
    .bind(lane.as_sql())
    .bind(&payload)
    .fetch_one(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks insert: {e}")))?;

    row.try_get::<i64, _>("id")
        .map_err(|e| DbError::Query(format!("decode tasks.id: {e}")))
}

/// Atomically claim the oldest `pending` task on the given lane,
/// transitioning state to `running` and setting `started_at` +
/// `lease_expires_at`. Returns `None` if no pending row exists on
/// that lane.
///
/// Uses `FOR UPDATE SKIP LOCKED` — the standard PG queue idiom — so
/// concurrent callers (different lane runners, or two daemons during
/// a transient overlap) never race over the same row. The per-lane
/// filter is what keeps the two lane runners from ever racing each
/// other.
pub async fn claim_one(
    pool: &PgPool,
    lane: Lane,
    deadline_seconds: i64,
) -> Result<Option<Task>, DbError> {
    let now = OffsetDateTime::now_utc();
    let lease_expires_at = now + Duration::seconds(deadline_seconds);

    let row = sqlx::query(
        "UPDATE tasks \
         SET state = 'running', \
             started_at = now(), \
             updated_at = now(), \
             lease_expires_at = $2 \
         WHERE id = ( \
             SELECT id FROM tasks \
             WHERE lane = $1 AND state = 'pending' \
             ORDER BY created_at ASC \
             LIMIT 1 \
             FOR UPDATE SKIP LOCKED \
         ) \
         RETURNING id, state, lane, created_at, updated_at, started_at, \
                   finished_at, lease_expires_at, plan_count, payload, result",
    )
    .bind(lane.as_sql())
    .bind(lease_expires_at)
    .fetch_optional(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks claim_one: {e}")))?;

    let Some(row) = row else { return Ok(None) };

    Ok(Some(decode_task_row(&row)?))
}

/// Decode a `tasks` row into a typed `Task`. Used by `claim_one`,
/// `get`, `list`, and any future read function that returns `Task`s.
/// Centralised so a column-rename mistake fails in one place, not
/// many.
fn decode_task_row(row: &PgRow) -> Result<Task, DbError> {
    Ok(Task {
        id: row.try_get("id")
            .map_err(|e| DbError::Query(format!("decode tasks.id: {e}")))?,
        state: row.try_get("state")
            .map_err(|e| DbError::Query(format!("decode tasks.state: {e}")))?,
        lane: Lane::from_sql(
            row.try_get::<&str, _>("lane")
                .map_err(|e| DbError::Query(format!("decode tasks.lane: {e}")))?,
        )?,
        created_at: row.try_get("created_at")
            .map_err(|e| DbError::Query(format!("decode tasks.created_at: {e}")))?,
        updated_at: row.try_get("updated_at")
            .map_err(|e| DbError::Query(format!("decode tasks.updated_at: {e}")))?,
        started_at: row.try_get("started_at")
            .map_err(|e| DbError::Query(format!("decode tasks.started_at: {e}")))?,
        finished_at: row.try_get("finished_at")
            .map_err(|e| DbError::Query(format!("decode tasks.finished_at: {e}")))?,
        lease_expires_at: row.try_get("lease_expires_at")
            .map_err(|e| DbError::Query(format!("decode tasks.lease_expires_at: {e}")))?,
        plan_count: row.try_get("plan_count")
            .map_err(|e| DbError::Query(format!("decode tasks.plan_count: {e}")))?,
        payload: row.try_get("payload")
            .map_err(|e| DbError::Query(format!("decode tasks.payload: {e}")))?,
        result: row.try_get("result")
            .map_err(|e| DbError::Query(format!("decode tasks.result: {e}")))?,
    })
}

/// Terminal state writer. Sets `state = $term`, `result = $result`,
/// `finished_at = now()`, then the `notify_task_completed` trigger
/// fires the NOTIFY for any CLI subscribers.
///
/// Caller is the lane runner's `finalize` step. The `state` argument
/// must be one of the terminal states (everything except 'pending'
/// and 'running'); the CHECK constraint will reject other values.
///
/// Silent no-op if the task has already transitioned out of `running`
/// (e.g. cancelled out from under the lane runner, or finalised twice).
/// Returns `Ok(())` either way; the caller does not need to distinguish
/// "I won the race" from "someone else terminalised this row first."
///
/// `turn_record` is the conversational-continuity record for a channel task
/// (#701): what this task did, for the next turn in the same conversation to
/// read — see [`turns`]. `None` for every other task kind, which leaves the
/// column NULL. Written here, in the same UPDATE that makes the task terminal,
/// so a record can never describe a task that did not finish.
pub async fn finalize(
    pool: &PgPool,
    task_id: i64,
    state: &str,
    result: Option<serde_json::Value>,
    turn_record: Option<serde_json::Value>,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE tasks \
         SET state = $2, \
             result = $3, \
             turn_record = $4, \
             finished_at = now(), \
             updated_at = now() \
         WHERE id = $1 AND state = 'running'",
    )
    .bind(task_id)
    .bind(state)
    .bind(result)
    .bind(turn_record)
    .execute(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks finalize: {e}")))?;
    Ok(())
}

/// Read just the state column. Cheap; called from the inner loop's
/// per-iteration cancellation poll.
pub async fn observe_state(pool: &PgPool, task_id: i64) -> Result<String, DbError> {
    let row = sqlx::query("SELECT state FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(pool)
        .await
        .map_err(|e| DbError::Query(format!("tasks observe_state: {e}")))?;
    row.try_get::<String, _>("state")
        .map_err(|e| DbError::Query(format!("decode tasks.state: {e}")))
}

/// Producer-side cancellation. Sets `state = 'cancelled'` only if the
/// task is still in `pending`, `running` or `awaiting_operator`; the
/// trigger fires the `tasks_cancelled` NOTIFY.
///
/// Returns a [`Cancellation`] — the post-update row via `RETURNING` so the
/// caller can emit one producer-side audit row (e.g. `actor='cli'
/// action='task.cancelled'`) without a follow-up SELECT, plus the state the
/// task was in **before** the cancel and how many asks went with it.
/// `None` means the row was not in a cancellable state (already terminal,
/// or does not exist) — idempotent.
///
/// `previous_state` exists because the audit emitter downstream has to
/// decide whether the scheduler will *also* write a `task.finalize` row for
/// this task, and after the UPDATE every cancelled row looks alike. See
/// [`Cancellation::previous_state`].
///
/// Mirrors the shape [`sweep_crashed`] took on 2026-05-12 for the same
/// reason: an audit emitter downstream needs the row's `lane` and
/// `plan_count` to build the canonical lifecycle payload.
///
/// # Why this also cancels the task's asks (#564)
///
/// `awaiting_operator` was added so a task can suspend on a human
/// decision. Cancelling such a task while leaving its ask `pending` would
/// leave a live question attached to a dead task: still resolvable, and
/// resolving it would try to re-enqueue something already cancelled.
///
/// The ask write lives **inside** this function, in the same transaction,
/// rather than in a separate cancel-both helper — which is why `db::tasks`
/// depends on `db::asks`. With a separate helper, any caller reaching for
/// plain `mark_cancelled` (and the CLI cancel path is one) would silently
/// strand the ask. One cancel path that cannot be bypassed is worth the
/// coupling; same argument `AllowlistDecl` made in #545 for making the
/// half-declared state unrepresentable.
///
/// # Lock order: asks → tasks, and it is NOT arbitrary
///
/// This function cancels the task's asks **before** running the guarded
/// `tasks` UPDATE — asks locked first, tasks second. That matches
/// `asks::resolve`, `asks::resolve_with_nonce`, and `asks::expire_due`,
/// which all write `asks` then `tasks` inside one transaction. Locking
/// tasks first inverts that order relative to the other three, and PG
/// detects the resulting lock cycle between a concurrent
/// `mark_cancelled(T)` and `resolve(A)` on the same (task, ask) pair as a
/// deadlock (SQLSTATE 40P01) and aborts one side — surfacing as a database
/// error on either the operator's cancel or their approval, for no reason
/// but acquisition order. Reproduced against a live PG 18 before this was
/// written. **If you are tempted to swap this back so the "primary" tasks
/// UPDATE runs first: don't — it silently reintroduces that deadlock.**
///
/// `asks::raise` is the one writer of both tables that takes them the
/// other way round (`suspend_for_ask`, then INSERT). It cannot participate
/// in that cycle: it requires `state = 'running'`, and a task with a
/// pending ask is `awaiting_operator`, so the only transaction it can
/// contend with on the ask side is another `raise` for the same task —
/// which serializes on the `tasks` row first. What it *can* do is race
/// this function, which is what the re-sweep below is for.
///
/// # Why the ask cancel runs TWICE
///
/// The asks-first order opens a window the tasks-first order did not have,
/// and it is not theoretical — it was reproduced on a live PG 18:
///
/// 1. `raise(T)` locks task `T` (`running` → `awaiting_operator`).
/// 2. `mark_cancelled(T)` sweeps `asks` — **0 rows**; under READ COMMITTED
///    the ask `raise` is about to insert is invisible, and there is no row
///    version to block on.
/// 3. `raise(T)` inserts ask `A` and commits.
/// 4. `mark_cancelled(T)`'s tasks UPDATE unblocks, re-checks against the
///    committed row, finds `awaiting_operator` in the widened `IN` list,
///    and cancels.
///
/// Result: task `cancelled`, ask `A` still `pending` — precisely the
/// stranded live-question-on-a-dead-task this function's coupling exists
/// to prevent. The second sweep runs *after* the tasks row lock is held,
/// so it sees anything committed in that window. It cannot deadlock: by
/// the time we hold the tasks lock, no `raise` for this task can be
/// mid-flight holding an ask row (it takes the tasks lock first), and any
/// ask that existed at step 2 is already locked by us.
///
/// Cancelling the asks before the tasks UPDATE succeeds is still correct
/// when the task turns out NOT to be cancellable: the function returns
/// `Ok(None)` without committing, and dropping `tx` rolls both sweeps back
/// along with it. The ask cancel is provisional on the task actually being
/// cancelled.
pub async fn mark_cancelled(
    pool: &PgPool,
    task_id: i64,
) -> Result<Option<Cancellation>, DbError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::Query(format!("tasks mark_cancelled begin: {e}")))?;

    // First sweep: establishes the asks → tasks acquisition order.
    let mut asks_cancelled = crate::asks::cancel_for_task(&mut tx, task_id).await?;

    // Read the pre-cancel state. `RETURNING` yields the NEW row, so this is
    // the only place the old state is still observable.
    //
    // `FOR UPDATE`, and that is load-bearing rather than incidental. A plain
    // SELECT is a snapshot read: in the raise-racing-cancel interleaving
    // described above it returns the *stale* `running`, because the raiser's
    // `awaiting_operator` is not committed yet. `previous_state` would then
    // say `running`, the audit emitter would conclude a scheduler-side
    // inner loop is going to write the `task.finalize` row, and none would
    // ever be written — the exact undercount this field exists to prevent,
    // reintroduced under the exact race the second sweep exists to handle.
    // Locking the row makes the read wait for that writer and report what
    // the UPDATE below will actually see. It also takes the `tasks` lock
    // here rather than at the UPDATE, which is still *after* the asks
    // sweep, so the acquisition order is unchanged.
    let previous_state = state_locked_in_tx(&mut tx, task_id).await?;

    let row = sqlx::query(
        "UPDATE tasks \
         SET state = 'cancelled', \
             finished_at = now(), \
             updated_at = now() \
         WHERE id = $1 AND state IN ('pending', 'running', 'awaiting_operator') \
         RETURNING id, state, lane, created_at, updated_at, started_at, \
                   finished_at, lease_expires_at, plan_count, payload, result",
    )
    .bind(task_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("tasks mark_cancelled: {e}")))?;

    let Some(row) = row else {
        // Not cancellable. Dropping `tx` rolls back — including the ask
        // cancel above, which must not survive a task that stayed
        // uncancelled.
        return Ok(None);
    };
    let task = decode_task_row(&row)?;

    // Second sweep — see "Why the ask cancel runs TWICE" above. Now that
    // the tasks row lock is held, an ask committed by a `raise` racing
    // step 1 is visible and gets cancelled with its task.
    asks_cancelled += crate::asks::cancel_for_task(&mut tx, task_id).await?;

    tx.commit()
        .await
        .map_err(|e| DbError::Query(format!("tasks mark_cancelled commit: {e}")))?;
    Ok(Some(Cancellation {
        task,
        previous_state: previous_state.unwrap_or_default(),
        asks_cancelled,
    }))
}

/// What [`mark_cancelled`] returns when it actually cancelled something.
#[derive(Debug, Clone)]
pub struct Cancellation {
    /// The post-update row, `state = 'cancelled'`.
    pub task: Task,
    /// The state the task was in **immediately before** the cancel, read
    /// inside the same transaction.
    ///
    /// Load-bearing, not diagnostic. The producer-side audit emitter has to
    /// decide whether the scheduler's inner loop will *also* write a
    /// `task.finalize` row for this task, and after the UPDATE a task
    /// cancelled out of `running` and one cancelled out of
    /// `awaiting_operator` are indistinguishable — both have `started_at`
    /// set. Only the first has a live inner loop to emit that row. See
    /// `core::cli_audit::task::scheduler_will_emit_finalize`.
    pub previous_state: String,
    /// How many of the task's `pending` asks were cancelled with it.
    ///
    /// Surfaced rather than discarded so the audit trail can record that a
    /// human's outstanding question was destroyed. Without it, a pending
    /// ask vanishes from `asks::list_pending` with nothing in `audit_log`
    /// saying it ever existed.
    pub asks_cancelled: u64,
}

/// Read a task's current state inside a caller's transaction, **without**
/// locking the row.
///
/// For diagnostics only — `asks::raise` uses it to name the state it found
/// when the task was not suspendable. A snapshot read is right there: the
/// caller is already on an error path, is about to roll back, and must not
/// block behind whichever writer moved the task out from under it.
/// Somewhere a concurrent commit lands between the guard and this read, the
/// message names the slightly-stale state, which is a strictly better
/// message than the fixed three-way list it replaced.
///
/// Distinct from [`observe_state`], which takes a pool and errors on a
/// missing row: this one returns `None` for "no such task" because its
/// caller is already handling a failure and must not have it masked by a
/// second one. Takes `&mut PgConnection` for the same reason the write
/// helpers do — see [`suspend_for_ask`].
pub(crate) async fn state_in_tx(
    conn: &mut sqlx::PgConnection,
    task_id: i64,
) -> Result<Option<String>, DbError> {
    decode_state_row(
        sqlx::query("SELECT state FROM tasks WHERE id = $1")
            .bind(task_id)
            .fetch_optional(conn)
            .await
            .map_err(|e| DbError::Query(format!("tasks state_in_tx: {e}")))?,
    )
}

/// Read a task's current state inside a caller's transaction and **hold the
/// row lock**, so the value is what a subsequent guarded UPDATE in the same
/// transaction will see.
///
/// Used by [`mark_cancelled`], where a stale answer is not cosmetic: see the
/// `FOR UPDATE` note at its call site.
async fn state_locked_in_tx(
    conn: &mut sqlx::PgConnection,
    task_id: i64,
) -> Result<Option<String>, DbError> {
    decode_state_row(
        sqlx::query("SELECT state FROM tasks WHERE id = $1 FOR UPDATE")
            .bind(task_id)
            .fetch_optional(conn)
            .await
            .map_err(|e| DbError::Query(format!("tasks state_locked_in_tx: {e}")))?,
    )
}

fn decode_state_row(row: Option<PgRow>) -> Result<Option<String>, DbError> {
    row.as_ref()
        .map(|r| {
            r.try_get::<String, _>("state")
                .map_err(|e| DbError::Query(format!("decode tasks.state: {e}")))
        })
        .transpose()
}

/// Cancel a task **only if it is still `pending`** (never claimed).
///
/// Unlike [`mark_cancelled`] (which also cancels a `running` task), this
/// no-ops on a task the daemon has already claimed. That distinction is
/// the safety property the `memory l3 run` no-daemon path relies on: if
/// the daemon claims the task in the race window between the liveness
/// check and the cancel, this returns `None` and the CLI waits for the
/// real result instead of orphaning a `--execute` it believed it had
/// stopped (issue #179 follow-up). Returns the cancelled row (for the
/// downstream audit emitter) or `None` if the task was not `pending`.
pub async fn mark_cancelled_if_pending(
    pool: &PgPool,
    task_id: i64,
) -> Result<Option<Task>, DbError> {
    let row = sqlx::query(
        "UPDATE tasks \
         SET state = 'cancelled', \
             finished_at = now(), \
             updated_at = now() \
         WHERE id = $1 AND state = 'pending' \
         RETURNING id, state, lane, created_at, updated_at, started_at, \
                   finished_at, lease_expires_at, plan_count, payload, result",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks mark_cancelled_if_pending: {e}")))?;
    row.as_ref().map(decode_task_row).transpose()
}

/// True iff at least one task is currently `running` with an **unexpired**
/// lease — a proxy for "a daemon is alive and consuming a lane".
///
/// Used by `memory l3 run` to tell a *busy* daemon (something else is
/// running; keep waiting) apart from an *absent* one (nothing running;
/// cancel + error) when its submitted task lingers `pending` past the
/// grace window. The lease bound excludes a crashed daemon's stale
/// `running` rows (their `lease_expires_at` is in the past until the next
/// startup sweep reclaims them).
pub async fn any_live_worker(pool: &PgPool) -> Result<bool, DbError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tasks \
         WHERE state = 'running' AND lease_expires_at > now())",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks any_live_worker: {e}")))?;
    Ok(exists)
}

/// Operator-side escape hatch: forcibly mark a `running` task as
/// crashed before its lease elapses. Mirrors the startup sweep but
/// scoped to one row, used by `kastellan-cli tasks fail <id>`. Returns
/// true iff a row was updated.
pub async fn mark_failed_running(pool: &PgPool, task_id: i64) -> Result<bool, DbError> {
    let r = sqlx::query(
        "UPDATE tasks \
         SET state = 'crashed', \
             finished_at = now(), \
             updated_at = now() \
         WHERE id = $1 AND state = 'running' \
           AND lease_expires_at > now()",
    )
    .bind(task_id)
    .execute(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks mark_failed_running: {e}")))?;
    Ok(r.rows_affected() == 1)
}

/// Startup sweep. Marks every task whose lease has elapsed but is
/// still `running` as `crashed`. Idempotent; safe to re-run.
///
/// Returns the recovered rows (`RETURNING *`) so the caller can emit
/// one `scheduler/task.crashed` audit row per task. The post-UPDATE
/// state ('crashed') and post-UPDATE `finished_at` (now()) are included
/// — that's the value RETURNING expressly returns, distinct from the
/// pre-UPDATE row.
///
/// An empty vec means there was nothing to sweep (the idempotent case).
pub async fn sweep_crashed(pool: &PgPool) -> Result<Vec<Task>, DbError> {
    let rows = sqlx::query(
        "UPDATE tasks \
         SET state = 'crashed', \
             finished_at = now(), \
             updated_at = now() \
         WHERE state = 'running' AND lease_expires_at < now() \
         RETURNING id, state, lane, created_at, updated_at, started_at, \
                   finished_at, lease_expires_at, plan_count, payload, result",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks sweep_crashed: {e}")))?;
    rows.iter().map(decode_task_row).collect()
}

/// Mirror `tasks.plan_count` from the inner loop after each
/// `formulate_plan` succeeds. Best-effort: if the task is no longer
/// in `running` (cancelled out from under us), the UPDATE is a no-op
/// and the next iteration's cancellation poll will catch it.
pub async fn increment_plan_count(
    pool: &PgPool,
    task_id: i64,
    new_plan_count: i32,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE tasks SET plan_count = $2, updated_at = now() \
         WHERE id = $1 AND state = 'running'",
    )
    .bind(task_id)
    .bind(new_plan_count)
    .execute(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks increment_plan_count: {e}")))?;
    Ok(())
}

/// Suspend a `running` task while an operator ask is outstanding
/// (#564). Sets `state = 'awaiting_operator'` and **releases the lease**.
///
/// Returns `true` iff a row moved. `false` means the task was not
/// `running` — already terminal, cancelled out from under the caller, or
/// never claimed — and the caller must treat that as a refusal to
/// suspend, not as success.
///
/// Releasing the lease is **hygiene, not a load-bearing invariant**, and
/// the distinction matters because slice 1b must not cite it as one. Every
/// consumer of `lease_expires_at` also filters on `state = 'running'` —
/// `any_live_worker` (`state = 'running' AND lease_expires_at > now()`)
/// and `sweep_crashed` (`state = 'running' AND lease_expires_at < now()`)
/// both — and `claim_one` overwrites the column unconditionally on the way
/// back to `running`. So a suspended task that kept its lease would not in
/// fact look busy to anything; it would just be a confusing artifact for
/// an operator reading the table. (An earlier version of this comment
/// claimed the `any_live_worker` consequence. It was wrong in the same way
/// the e2e assertion beside it was — that helper's `state` predicate
/// excludes a suspended task before the lease is ever consulted.)
///
/// Takes `&mut PgConnection` so `asks::raise` can call it inside its
/// transaction — the ask INSERT and this UPDATE must commit together — and
/// so that a `&PgPool` (which `E: Executor` would have accepted) cannot
/// run it standalone and suspend a task whose ask never gets written.
/// **Sole intended caller:** `asks::raise`.
///
/// `pub(crate)`, not `pub`: called from anywhere else, this parks a task
/// in `awaiting_operator` with no ask backing it. `claim_one`
/// (`state = 'pending'`) and `sweep_crashed` (`state = 'running'`) both
/// skip that state, and `asks::expire_due` can only reach a task *through*
/// an ask row — so a task parked with no ask is invisible to all three and
/// wedges permanently until a manual cancel.
pub(crate) async fn suspend_for_ask(
    conn: &mut sqlx::PgConnection,
    task_id: i64,
) -> Result<bool, DbError> {
    let r = sqlx::query(
        "UPDATE tasks \
         SET state = 'awaiting_operator', \
             lease_expires_at = NULL, \
             updated_at = now() \
         WHERE id = $1 AND state = 'running'",
    )
    .bind(task_id)
    .execute(conn)
    .await
    .map_err(|e| DbError::Query(format!("tasks suspend_for_ask: {e}")))?;
    Ok(r.rows_affected() == 1)
}

/// Return a suspended task to the queue after its ask resolved (#564).
///
/// Guarded on `awaiting_operator` so it cannot resurrect a task that was
/// cancelled or expired while the ask was outstanding. Returns `true` iff
/// a row moved.
///
/// The `tasks_notify_resumed` trigger fires `pg_notify('tasks_resumed', id)`
/// on this transition, which is what wakes the lane runner immediately
/// rather than at its next 30 s heartbeat.
///
/// `started_at` and `plan_count` are deliberately left alone **by this
/// UPDATE** — it does not reset either to a fresh-task value.
///
/// That is now true of the observed outcome too: nothing downstream
/// resets `plan_count`, so it genuinely carries forward across a resume.
///
/// **Slice 1b made that call** (`runner::task_exec::resume_budget`): the
/// context is seeded from this column, so it is monotonic and no longer
/// rewinds, and `max_plans` is *extended* by the carried count rather than
/// the budget being either reset or spent-from. Spending from the original
/// budget would leave a task that escalated on its last allowed plan
/// resuming with none, making the operator's approval buy nothing.
///
/// Takes `&mut PgConnection` so `asks::resolve` and
/// `asks::resolve_with_nonce` can call it inside their transactions, and
/// so a `&PgPool` cannot re-enqueue a task standalone while the ask's
/// resolution rolls back. **Sole intended caller:** `asks::resolve` /
/// `asks::resolve_with_nonce`.
///
/// `pub(crate)`, not `pub`: called from anywhere else, this resurrects a
/// task from `awaiting_operator` with no resolved ask behind it, silently
/// bypassing the human decision the state exists to wait on.
pub(crate) async fn resume_from_ask(
    conn: &mut sqlx::PgConnection,
    task_id: i64,
) -> Result<bool, DbError> {
    let r = sqlx::query(
        "UPDATE tasks \
         SET state = 'pending', \
             updated_at = now() \
         WHERE id = $1 AND state = 'awaiting_operator'",
    )
    .bind(task_id)
    .execute(conn)
    .await
    .map_err(|e| DbError::Query(format!("tasks resume_from_ask: {e}")))?;
    Ok(r.rows_affected() == 1)
}

/// Terminal write for a task whose ask expired (#564).
///
/// Separate from [`finalize`] rather than widening its guard. `finalize`
/// means "the lane runner finished a task it was running" and matches
/// `state = 'running'`; keeping that true is worth one small function,
/// because a widened guard would also let a stray `finalize` terminalise a
/// task that is merely suspended.
///
/// The result payload matches `Outcome::Failed`'s shape
/// (`{"kind":"error","detail":…}`) so a reader does not have to know which
/// path produced it. State is `failed` rather than `timed_out`: `timed_out`
/// means the task's own wall-clock deadline elapsed while it was working,
/// and conflating the two would make lane-latency queries count tasks that
/// spent their time waiting on a human.
///
/// **Sole intended caller:** `asks::expire_due`. Takes
/// `&mut PgConnection` so it runs inside that sweep's transaction and
/// cannot be handed a `&PgPool`. `pub(crate)`, not `pub`: called from
/// anywhere else, this terminalises a task that may still have a
/// `pending` ask attached, leaving a resolvable ask pointing at a dead
/// task — the same wedge `mark_cancelled`'s coupled ask-cancel exists to
/// prevent, reintroduced from a different call site.
pub(crate) async fn fail_awaiting_operator(
    conn: &mut sqlx::PgConnection,
    task_id: i64,
    detail: &str,
) -> Result<bool, DbError> {
    let r = sqlx::query(
        "UPDATE tasks \
         SET state = 'failed', \
             result = $2, \
             finished_at = now(), \
             updated_at = now() \
         WHERE id = $1 AND state = 'awaiting_operator'",
    )
    .bind(task_id)
    .bind(serde_json::json!({"kind": "error", "detail": detail}))
    .execute(conn)
    .await
    .map_err(|e| DbError::Query(format!("tasks fail_awaiting_operator: {e}")))?;
    Ok(r.rows_affected() == 1)
}

/// Fetch one task by id (any state). Used by CLI status subcommand
/// and by the synthetic-load harness.
pub async fn get(pool: &PgPool, task_id: i64) -> Result<Option<Task>, DbError> {
    let row = sqlx::query(
        "SELECT id, state, lane, created_at, updated_at, started_at, \
                finished_at, lease_expires_at, plan_count, payload, result \
         FROM tasks WHERE id = $1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks get: {e}")))?;

    let Some(row) = row else { return Ok(None) };
    Ok(Some(decode_task_row(&row)?))
}

/// Recent tasks, optionally filtered by lane and/or state. FIFO
/// (created_at DESC), capped at `limit`.
pub async fn list(
    pool: &PgPool,
    lane: Option<Lane>,
    state: Option<&str>,
    limit: i64,
) -> Result<Vec<Task>, DbError> {
    let limit = limit.max(0);  // clamp; LIMIT -1 would be a PG error
    let rows = sqlx::query(
        "SELECT id, state, lane, created_at, updated_at, started_at, \
                finished_at, lease_expires_at, plan_count, payload, result \
         FROM tasks \
         WHERE ($1::text IS NULL OR lane = $1) \
           AND ($2::text IS NULL OR state = $2) \
         ORDER BY created_at DESC \
         LIMIT $3",
    )
    .bind(lane.map(|l| l.as_sql()))
    .bind(state)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks list: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(decode_task_row(row)?);
    }
    Ok(out)
}
