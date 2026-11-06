//! The harness `cli_session_e2e` and `cli_ask_follow_e2e` share: a per-test
//! Postgres and the real `kastellan-cli session` driven over its stdin/stdout.
//! Moved out of `cli_session_e2e.rs` unchanged but for `pub`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use kastellan_db::pool::connect_runtime_pool;
use kastellan_db::probe::run as probe_run;
use kastellan_db::tasks::{self, insert_pending, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, cli_binary, current_username, pg_bin_dir_or_skip, skip_if_no_supervisor,
    unique_suffix, PgCluster,
};
use serde_json::{json, Value};

/// Long enough for a follow's 3 s poll tick plus a slow CI host.
pub const WAIT: Duration = Duration::from_secs(30);

pub async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ss-d",
        "ss-l",
        &format!("kastellan-postgres-cli-session-e2e-{label}-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup", json!({"test": format!("cli_session_e2e-{label}")}))
        .await
        .expect("probe run");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("runtime pool");
    Some((pool, cluster))
}

/// A task in `running`, as a claimed one is.
pub async fn seed_running_task(pool: &sqlx::PgPool) -> i64 {
    let id = insert_pending(pool, Lane::Fast, json!({"instruction": "x", "max_plans": 3})).await.expect("insert");
    tasks::claim_one(pool, Lane::Fast, 60).await.expect("claim").expect("a task");
    id
}

/// The real `kastellan-cli session`, with a line reader on its stdout.
pub struct Session {
    child: Child,
    pub stdin: ChildStdin,
    lines: Receiver<Value>,
    next_id: i64,
}

impl Session {
    pub fn start(cluster: &PgCluster) -> Session {
        let user = std::env::var("USER").unwrap_or_else(|_| current_username());
        let mut cmd = Command::new(cli_binary());
        cmd.arg("session")
            .env_clear()
            .env("KASTELLAN_DATA_DIR", &cluster.data_dir)
            .env("USER", user)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(home) = std::env::var_os("HOME") {
            cmd.env("HOME", home);
        }
        let mut child = cmd.spawn().expect("spawn kastellan-cli session");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line:?}: {e}"));
                if tx.send(v).is_err() {
                    return;
                }
            }
        });
        Session { child, stdin, lines, next_id: 0 }
    }

    pub fn send_line(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to session");
        self.stdin.flush().unwrap();
    }

    /// Send a request; return its id.
    pub fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
        self.send_line(&req.to_string());
        self.next_id
    }

    /// The next message, whatever it is.
    pub fn next(&self) -> Value {
        match self.lines.recv_timeout(WAIT) {
            Ok(v) => v,
            Err(RecvTimeoutError::Timeout) => panic!("no message from the session within {WAIT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the session closed its stdout"),
        }
    }

    /// Assert nothing arrives for `d`.
    pub fn quiet_for(&self, d: Duration) {
        match self.lines.recv_timeout(d) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(v) => panic!("expected nothing for {d:?}, got {v}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the session closed its stdout"),
        }
    }

    /// Send a request and return its response, which must come next.
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params);
        let resp = self.next();
        assert_eq!(resp["id"], id, "expected the response to {method} next, got {resp}");
        resp
    }

    pub fn ok(&mut self, method: &str, params: Value) -> Value {
        let resp = self.call(method, params);
        assert!(resp.get("error").is_none(), "{method} failed: {resp}");
        resp["result"].clone()
    }

    pub fn error_code(&mut self, method: &str, params: Value) -> i64 {
        let resp = self.call(method, params);
        resp["error"]["code"].as_i64().unwrap_or_else(|| panic!("{method} should have failed: {resp}"))
    }

    /// Close stdin and expect a clean exit.
    pub fn finish(mut self) {
        drop(self.stdin);
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "session must exit 0 at EOF: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "session did not exit at EOF");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

pub fn steps_payload() -> Value {
    json!({"steps": [{"id": 1, "purpose": "find it", "tool": "mail", "done_when": "found"}]})
}

pub fn tick_payload() -> Value {
    json!({"step": 1, "note": "found", "findings": [{"text": "invoice 42", "source": "msg 7"}],
           "summary": "one invoice", "classification": "Public"})
}

/// Assert `msg` is a `notebook.entry` for `seq` of `kind`.
pub fn assert_entry(msg: &Value, task_id: i64, seq: i64, kind: &str) {
    assert_eq!(msg["method"], "notebook.entry", "expected an entry, got {msg}");
    assert_eq!(msg["params"]["task_id"], task_id, "{msg}");
    assert_eq!(msg["params"]["seq"], seq, "{msg}");
    assert_eq!(msg["params"]["kind"], kind, "{msg}");
}
