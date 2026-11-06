//! `kastellan-cli session` — a long-lived JSON-RPC 2.0 session over
//! stdin/stdout, for a front end (bottie) or a terminal to drive tasks
//! without one process per message (#922 slice 2; design §6.5, D8).
//!
//! One JSON object per line, the same framing the workers use
//! (`kastellan-protocol`, one record capped at `MAX_RECORD_BYTES`). Run it
//! locally, or as `ssh <host> kastellan-cli session`: there is no listener,
//! and the trust boundary is ssh plus the operator's OS user, exactly as for
//! `ask`. Message text only ever travels on stdin, never on a command line.
//!
//! ## Methods (request → result)
//!
//! | Method | Params | Result |
//! | --- | --- | --- |
//! | `task.submit` | `{instruction, lane?: "fast"\|"long", classification_floor?}` | `{task_id, lane}` |
//! | `task.follow` | `{task_id, after_seq?}` | `{task_id, after_seq}`, then notifications |
//! | `task.correct` | `{task_id, text}` | `{seq, audited}` |
//! | `task.cancel` | `{task_id}` | `{state}` |
//! | `task.status` | `{task_id}` | `{task_id, state, lane, plan_count, result, notebook, notebook_error}` |
//! | `inbox.list` | `{limit?}` | `{asks: [{ask_id, task_id, kind, question, options, deadline_at}]}` |
//! | `inbox.answer` | `{ask_id, choice: "approve"\|"deny", note?}` | `{task_id, audited}` |
//!
//! ## Notifications (no `id`), sent by `task.follow`
//!
//! * `notebook.entry {task_id, seq, kind, payload, created_at}` — each entry
//!   with `seq > after_seq`, in order, then each new one as it lands.
//! * `task.awaiting_operator {task_id}` — the task parked on an operator ask;
//!   `inbox.list` shows the question, `inbox.answer` answers it.
//! * `task.finished {task_id, state, answer, detail, result}` — the last
//!   message of a follow.
//! * `task.follow_failed {task_id, last_seq, error}` — the follow stopped
//!   early; follow again with `after_seq: last_seq`.
//!
//! A follow answers at once and streams in the background, so `task.cancel`
//! and `task.correct` work while it runs. Its response is always written
//! before its first notification. A dropped session does **not** cancel its
//! tasks: they keep running, and a new session follows them from `after_seq`.
//!
//! The session ends at EOF on stdin, and exits non-zero only when it cannot
//! start (no database).

mod follow;
mod methods;
mod params;

use std::process::ExitCode;

use kastellan_protocol::{codes, err_response, ok_response, read_capped_record, Record, Request, RpcError, MAX_RECORD_BYTES};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::common::{resolve_connect_spec, with_runtime};

/// How many outgoing messages may queue for stdout before senders wait. A
/// slow client then slows the follows down instead of growing memory.
const OUT_QUEUE: usize = 256;

pub(crate) fn run_session(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("usage: kastellan-cli session   (JSON-RPC 2.0 on stdin/stdout; takes no arguments)");
        return ExitCode::from(2);
    }
    with_runtime("session", session_async())
}

async fn session_async() -> ExitCode {
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("session: {e}"); return ExitCode::from(1); }
    };
    let pool = match kastellan_db::pool::connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("session: db connect failed: {e}"); return ExitCode::from(1); }
    };

    // One writer owns stdout, so a response and a notification can never
    // interleave inside one line.
    let (out, out_rx) = mpsc::channel::<Value>(OUT_QUEUE);
    let writer = tokio::spawn(write_lines(tokio::io::stdout(), out_rx));
    let mut records = read_records(std::io::stdin());
    let mut follows = tokio::task::JoinSet::new();

    while let Some(record) = records.recv().await {
        let line = match record {
            Ok(Record::Line(line)) => line,
            Ok(Record::Eof) => break,
            Ok(Record::TooLarge) => {
                // The stream is out of step: the rest of that line would be
                // read as garbage. Say so and stop, as the workers' server does.
                let err = RpcError::new(codes::INVALID_REQUEST, "request exceeded record cap");
                let _ = out.send(to_value(err_response(Value::Null, err))).await;
                break;
            }
            Err(e) => { eprintln!("session: reading stdin: {e}"); break; }
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice::<Request>(&line) {
            Err(e) => err_response(Value::Null, RpcError::new(codes::PARSE_ERROR, format!("parse error: {e}"))),
            Ok(req) if req.method == "task.follow" => match start_follow(&pool, req.params).await {
                Ok(f) => {
                    // The answer goes out first, then the stream starts.
                    let reply = ok_response(req.id, json!({"task_id": f.task_id, "after_seq": f.after_seq}));
                    if out.send(to_value(reply)).await.is_err() {
                        break;
                    }
                    follows.spawn(follow::follow(pool.clone(), f.task_id, f.after_seq, out.clone()));
                    continue;
                }
                Err(e) => err_response(req.id, e),
            },
            Ok(req) => match methods::call(&pool, &req.method, req.params).await {
                Ok(result) => ok_response(req.id, result),
                Err(e) => err_response(req.id, e),
            },
        };
        if out.send(to_value(response)).await.is_err() {
            break; // stdout is gone; no one is listening
        }
    }

    // The client hung up: its follows have no one to send to. Their tasks run on.
    follows.abort_all();
    while follows.join_next().await.is_some() {}
    drop(out);
    let _ = writer.await;
    ExitCode::SUCCESS
}

/// Check a `task.follow` request before answering it: its params, and that
/// the task exists (a typo'd id is an error, not a follow that never ends).
async fn start_follow(pool: &sqlx::PgPool, p: Value) -> Result<params::Follow, RpcError> {
    let f = params::follow(p)?;
    match kastellan_db::tasks::get(pool, f.task_id).await {
        Ok(Some(_)) => Ok(f),
        Ok(None) => Err(RpcError::new(codes::OPERATION_FAILED, format!("no task {}", f.task_id))),
        Err(e) => Err(RpcError::new(codes::INTERNAL_ERROR, e.to_string())),
    }
}

fn to_value(r: kastellan_protocol::Response) -> Value {
    serde_json::to_value(r).expect("a JSON-RPC response is plain JSON")
}

/// Read stdin as capped records on a thread of its own (stdin is a blocking
/// read), handing each to the async loop. Uses the protocol's own
/// `read_capped_record`, so the session frames exactly as the workers do.
fn read_records<R: std::io::Read + Send + 'static>(input: R) -> mpsc::Receiver<std::io::Result<Record>> {
    let (tx, rx) = mpsc::channel(1);
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(input);
        loop {
            let record = read_capped_record(&mut reader, MAX_RECORD_BYTES);
            let last = !matches!(record, Ok(Record::Line(_)));
            if tx.blocking_send(record).is_err() || last {
                return;
            }
        }
    });
    rx
}

/// Write each message as one line, flushed, until every sender is gone or
/// the write fails (the client closed its end).
async fn write_lines<W: tokio::io::AsyncWrite + Unpin>(mut w: W, mut rx: mpsc::Receiver<Value>) {
    while let Some(msg) = rx.recv().await {
        let mut line = msg.to_string();
        line.push('\n');
        if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
            return;
        }
    }
}
