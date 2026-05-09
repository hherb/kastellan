//! On-disk JSONL mirror of the `audit_log` table.
//!
//! ## Why on-disk
//!
//! Operators want to `tail -f` the audit log without bringing up a
//! Postgres client every time. The DB is the source of truth (and the
//! only place new rows can land — see [`hhagent_db::audit`]); this
//! module is a downstream replicator that turns committed rows into
//! daily-rotated JSONL files under `~/.local/state/hhagent/`. The
//! reader [`hhagent-cli audit tail`] then needs no DB connection at
//! all, so it works even when the cluster is down.
//!
//! ## Wake-up via LISTEN/NOTIFY
//!
//! Migration `0003_audit_log_notify.sql` installs an AFTER INSERT
//! trigger on `audit_log` that emits `pg_notify('audit_log_inserted',
//! NEW.id::text)`. The mirror task holds a dedicated [`sqlx::postgres::PgListener`]
//! on that channel; each NOTIFY wakes it up to fetch the new row by
//! id and append it to today's JSONL file.
//!
//! Wake-up via NOTIFY is best-effort (notifications can be lost on
//! listener reconnect, transaction rollback, etc.). A periodic
//! catch-up SELECT (`audit::fetch_since(last_seen)`) closes the loop:
//! every [`CATCHUP_INTERVAL`] seconds, the task pulls anything with
//! `id > last_seen_id`. The DB is therefore the eventual source of
//! truth — the JSONL stream lags but never leads.
//!
//! ## fsync per write
//!
//! Operator visibility beats throughput at Phase 0 scale (tens to low
//! hundreds of audit rows per minute, not thousands per second). Every
//! line that hits the JSONL file is fsynced before we acknowledge it
//! durable; a crash that loses the page cache will lose at most rows
//! that were *also* lost from the DB's WAL — and those would be
//! recovered from `audit_log` on next startup via [`fetch_since`].
//!
//! ## Daily rotation
//!
//! File names are `audit-YYYY-MM-DD.jsonl` (UTC date). When the date
//! rolls over, the open file is fsynced + closed and the new one is
//! opened lazily on the next write. Rotation is keyed on UTC, not
//! local time, so log files stay contiguous across DST transitions
//! and timezone changes.

use std::path::{Path, PathBuf};

use hhagent_db::audit::{self, AuditRow};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

/// Environment variable used by tests (and operators who want a
/// non-XDG path) to override the default state directory.
///
/// Production deployments leave this unset and rely on
/// [`default_state_dir`].
pub const ENV_STATE_DIR: &str = "HHAGENT_STATE_DIR";

/// How often the mirror task does a fallback catch-up SELECT
/// (in addition to NOTIFY-driven wake-ups). 5 s is the cadence
/// HANDOVER's Option I sketch called out — fast enough that a
/// missed NOTIFY surfaces within seconds, slow enough that idle
/// daemons don't spin needlessly.
pub const CATCHUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Catch-up batch size. A multi-day outage could in principle leave
/// thousands of unmirrored rows; the catch-up loop calls
/// [`fetch_since`] repeatedly with this LIMIT until it returns fewer
/// rows than asked, so memory stays bounded regardless of backlog.
pub const CATCHUP_BATCH: i64 = 256;

/// XDG-style default for the JSONL state directory:
/// `$HOME/.local/state/hhagent`.
///
/// On Linux this is the canonical XDG_STATE_HOME location. macOS
/// doesn't follow XDG by default but supports the path; we use the
/// same one on both OSes so operator docs stay simple (mirrors the
/// `default_data_dir` choice in [`hhagent_db`]).
///
/// Returns `None` when `$HOME` is unset.
pub fn default_state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        let mut p = PathBuf::from(h);
        p.push(".local/state/hhagent");
        p
    })
}

/// JSONL file path for `date` under `state_dir`:
/// `<state_dir>/audit-YYYY-MM-DD.jsonl`.
///
/// Pure: deterministic, no I/O. Tested with literal date pins so a
/// date-formatting refactor (e.g. switching to `Display`) cannot
/// silently change the layout.
pub fn audit_log_path_for(state_dir: &Path, date: time::Date) -> PathBuf {
    state_dir.join(format!(
        "audit-{:04}-{:02}-{:02}.jsonl",
        date.year(),
        u8::from(date.month()),
        date.day(),
    ))
}

/// Serialise one [`AuditRow`] to a single-line JSONL string, with the
/// trailing newline that JSONL readers expect.
///
/// `ts` is emitted as RFC 3339 (`2026-05-10T12:34:56.789012345Z`) so
/// downstream consumers parse it with any compliant parser (jq,
/// Python's `datetime.fromisoformat` in 3.11+, etc.).
///
/// Pure: returns a new `String`, no I/O. Caller is responsible for
/// writing it.
pub fn format_jsonl_line(row: &AuditRow) -> String {
    let ts = row
        .ts
        .format(&Rfc3339)
        .unwrap_or_else(|_| row.ts.unix_timestamp().to_string());
    let v = serde_json::json!({
        "id":      row.id,
        "ts":      ts,
        "actor":   row.actor,
        "action":  row.action,
        "payload": row.payload,
    });
    let mut s = serde_json::to_string(&v)
        .expect("AuditRow serialisation cannot fail");
    s.push('\n');
    s
}

/// Owning handle to a running mirror task.
///
/// Drop it (or call [`MirrorHandle::shutdown`]) to stop the task. The
/// handle holds the cancellation watch sender; the task observes the
/// flip and exits cleanly within one tick of [`CATCHUP_INTERVAL`].
pub struct MirrorHandle {
    cancel: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl MirrorHandle {
    /// Signal the mirror to stop, then await its task. Idempotent —
    /// calling twice is a no-op.
    pub async fn shutdown(self) {
        // `send` returns Err only if every receiver has been dropped,
        // which we don't care about — the task is finishing anyway.
        let _ = self.cancel.send(true);
        let _ = self.join.await;
    }
}

/// Errors surfaced by [`spawn_mirror`].
#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
    #[error("mkdir state dir {0}: {1}")]
    Mkdir(PathBuf, std::io::Error),
    #[error("PgListener connect: {0}")]
    Listener(String),
    #[error("LISTEN audit_log_inserted: {0}")]
    Listen(String),
}

/// Start the mirror task.
///
/// Steps:
///   1. `mkdir -p state_dir` (idempotent).
///   2. Open a `PgListener` against the cluster described by `spec`
///      and `LISTEN audit_log_inserted`.
///   3. Drain anything already in `audit_log` since `id > 0` so the
///      mirror file is consistent on cold starts (the daemon's
///      bring-up audit row is typically the first thing it picks up).
///   4. Spawn a background task that races `listener.recv()` against
///      the catch-up timer and the cancellation watch; each path
///      pulls fresh rows and appends them to the JSONL file with
///      fsync.
///
/// Returns the [`MirrorHandle`] once the task has spawned. The
/// initial drain in step 3 happens **inside the spawned task**, not
/// synchronously — so a slow Postgres at startup does not delay
/// daemon readiness. This is intentional: the daemon's
/// fail-closed contract is for the *probe*, not for the mirror.
///
/// `pool` is moved into the task (sqlx pools are Arc-internal — the
/// move is cheap and shares the same underlying connection set with
/// any clones the caller still holds). The listener gets its own
/// dedicated connection from the pool's options via
/// [`PgListener::connect_with`], so it doesn't compete with
/// pool-acquired connections for slots.
pub async fn spawn_mirror(
    pool: PgPool,
    state_dir: PathBuf,
) -> Result<MirrorHandle, MirrorError> {
    tokio::fs::create_dir_all(&state_dir)
        .await
        .map_err(|e| MirrorError::Mkdir(state_dir.clone(), e))?;

    // Open the listener BEFORE returning so a misconfigured cluster
    // surfaces synchronously (bad role, no NOTIFY trigger). Pool
    // cloning is cheap.
    let mut listener = PgListener::connect_with(&pool)
        .await
        .map_err(|e| MirrorError::Listener(e.to_string()))?;
    listener
        .listen("audit_log_inserted")
        .await
        .map_err(|e| MirrorError::Listen(e.to_string()))?;

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let join = tokio::spawn(run_mirror_loop(
        listener,
        pool,
        state_dir,
        cancel_rx,
    ));

    info!("audit_mirror task started");
    Ok(MirrorHandle {
        cancel: cancel_tx,
        join,
    })
}

/// Per-task state: which date the currently-open file is for, and
/// the highest `audit_log.id` we have already mirrored. Both are kept
/// in sync with the file on disk.
struct MirrorState {
    last_seen_id: i64,
    open_file: Option<OpenFile>,
}

struct OpenFile {
    date: time::Date,
    file: tokio::fs::File,
    path: PathBuf,
}

async fn run_mirror_loop(
    mut listener: PgListener,
    pool: PgPool,
    state_dir: PathBuf,
    mut cancel: watch::Receiver<bool>,
) {
    let mut state = MirrorState {
        last_seen_id: 0,
        open_file: None,
    };

    // Initial drain: catch up on anything already in the table. The
    // bring-up audit row from `probe::run` is typically the first
    // thing this picks up.
    if let Err(e) = catch_up(&pool, &state_dir, &mut state).await {
        warn!(error = %e, "initial audit_log catch-up failed; will retry on next tick");
    }

    let mut catchup_timer = tokio::time::interval(CATCHUP_INTERVAL);
    catchup_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately with `interval`; we just did the
    // initial drain, so eat that tick.
    catchup_timer.tick().await;

    loop {
        tokio::select! {
            biased;

            // Cancellation has highest priority so a shutdown signal
            // is honoured even under heavy NOTIFY traffic.
            res = cancel.changed() => {
                if res.is_err() || *cancel.borrow() {
                    debug!("audit_mirror cancellation observed; exiting");
                    break;
                }
            }

            // NOTIFY arrived. We don't trust the payload directly —
            // the catch-up SELECT (which reads everything > last_seen)
            // is the canonical fetch path. This keeps NOTIFY a pure
            // wake-up signal, not a load-bearing data path.
            recv = listener.recv() => {
                match recv {
                    Ok(_notif) => {
                        if let Err(e) = catch_up(&pool, &state_dir, &mut state).await {
                            warn!(error = %e, "audit_log fetch after NOTIFY failed");
                        }
                    }
                    Err(e) => {
                        // sqlx auto-reconnects PgListener on drop, but
                        // we still log so a flapping connection
                        // surfaces in the daemon log.
                        warn!(error = %e, "PgListener recv error; sqlx will reconnect");
                    }
                }
            }

            _ = catchup_timer.tick() => {
                if let Err(e) = catch_up(&pool, &state_dir, &mut state).await {
                    warn!(error = %e, "periodic audit_log catch-up failed");
                }
            }
        }
    }

    // Final fsync on the way out so the JSONL file's on-disk state
    // matches what the operator saw before shutdown.
    if let Some(ref mut of) = state.open_file {
        if let Err(e) = of.file.sync_all().await {
            warn!(error = %e, path = %of.path.display(), "final fsync failed");
        }
    }
}

/// Pull every row with `id > state.last_seen_id` and append each one
/// to the JSONL file. Updates `state` in place. Loops in batches of
/// [`CATCHUP_BATCH`] until the result is shorter than the batch
/// (canonical "drain a paginated stream" idiom).
async fn catch_up(
    pool: &PgPool,
    state_dir: &Path,
    state: &mut MirrorState,
) -> Result<(), CatchUpError> {
    loop {
        let rows = audit::fetch_since(pool, state.last_seen_id, CATCHUP_BATCH)
            .await
            .map_err(|e| CatchUpError::Fetch(e.to_string()))?;
        let n = rows.len();
        for row in rows {
            write_row(state_dir, state, &row).await?;
        }
        if (n as i64) < CATCHUP_BATCH {
            return Ok(());
        }
    }
}

/// Append one row to today's file (rotating if the date has changed
/// since the file was opened) and fsync.
async fn write_row(
    state_dir: &Path,
    state: &mut MirrorState,
    row: &AuditRow,
) -> Result<(), CatchUpError> {
    let date = row.ts.date();
    // Rotate if today's file isn't the right one.
    let need_open = match &state.open_file {
        None => true,
        Some(of) => of.date != date,
    };
    if need_open {
        if let Some(of) = state.open_file.take() {
            // Close the old file with a final fsync so a tail -f
            // reader sees everything before the rotation. tokio's
            // `File::sync_all` takes `&self`; we rely on Drop to
            // close the underlying fd after the future resolves.
            let _ = of.file.sync_all().await;
        }
        let path = audit_log_path_for(state_dir, date);
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| CatchUpError::OpenFile(path.clone(), e))?;
        state.open_file = Some(OpenFile { date, file, path });
    }

    let line = format_jsonl_line(row);
    let of = state.open_file.as_mut().expect("open_file just set");
    of.file
        .write_all(line.as_bytes())
        .await
        .map_err(|e| CatchUpError::Write(of.path.clone(), e))?;
    of.file
        .sync_all()
        .await
        .map_err(|e| CatchUpError::Fsync(of.path.clone(), e))?;

    state.last_seen_id = row.id;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum CatchUpError {
    #[error("audit_log fetch: {0}")]
    Fetch(String),
    #[error("open {0}: {1}")]
    OpenFile(PathBuf, std::io::Error),
    #[error("write {0}: {1}")]
    Write(PathBuf, std::io::Error),
    #[error("fsync {0}: {1}")]
    Fsync(PathBuf, std::io::Error),
}

impl From<CatchUpError> for std::io::Error {
    fn from(e: CatchUpError) -> Self {
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
    }
}

// `error!` is used inside the loop for unrecoverable failures; expose
// it for IDE-jump-to-symbol sanity even though it's only conditionally
// hit. Stops "unused import" lint complaints.
#[allow(dead_code)]
fn _ensure_error_macro_resolves() {
    error!("noop");
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn audit_log_path_pads_zeros_in_month_and_day() {
        let p = audit_log_path_for(Path::new("/srv/state"), date!(2026 - 03 - 05));
        assert_eq!(p, PathBuf::from("/srv/state/audit-2026-03-05.jsonl"));
    }

    #[test]
    fn audit_log_path_handles_year_4_digits() {
        let p = audit_log_path_for(Path::new("/x"), date!(2026 - 12 - 31));
        assert_eq!(p, PathBuf::from("/x/audit-2026-12-31.jsonl"));
    }

    #[test]
    fn jsonl_line_ends_with_newline() {
        let row = AuditRow {
            id: 7,
            ts: time::OffsetDateTime::UNIX_EPOCH,
            actor: "core".into(),
            action: "startup".into(),
            payload: serde_json::json!({"version": "0.0.0"}),
        };
        let line = format_jsonl_line(&row);
        assert!(line.ends_with('\n'), "JSONL must end with \\n: {line:?}");
        // Exactly one newline (no double-line emission).
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn jsonl_line_serialises_all_audit_row_fields() {
        let row = AuditRow {
            id: 42,
            ts: time::OffsetDateTime::UNIX_EPOCH,
            actor: "tool:shell-exec".into(),
            action: "call".into(),
            payload: serde_json::json!({"req": {"argv": ["echo", "hi"]}, "ms": 7}),
        };
        let line = format_jsonl_line(&row);
        let v: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("valid JSON line");
        assert_eq!(v.get("id").and_then(|v| v.as_i64()), Some(42));
        assert_eq!(v.get("actor").and_then(|v| v.as_str()), Some("tool:shell-exec"));
        assert_eq!(v.get("action").and_then(|v| v.as_str()), Some("call"));
        assert!(v.get("ts").and_then(|v| v.as_str()).is_some());
        assert!(v.get("payload").and_then(|v| v.as_object()).is_some());
    }

    #[test]
    fn default_state_dir_lives_under_xdg_state_home() {
        // Hermetic: don't depend on the test runner's $HOME.
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", "/tmp/fakehome-hhagent-mirror");
        let p = default_state_dir().unwrap();
        assert_eq!(p, PathBuf::from("/tmp/fakehome-hhagent-mirror/.local/state/hhagent"));
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}
