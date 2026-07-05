//! Client-side helper: talk to a child worker over its stdio pipes.

use std::io::{self, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout};

use crate::{codes, read_capped_record, Record, Request, Response, RpcError, MAX_RECORD_BYTES};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("worker exited before responding")]
    EarlyExit,
    #[error("worker response exceeded the {cap}-byte record cap")]
    ResponseTooLarge { cap: usize },
    #[error("response id {got:?} does not match request id {expected:?}")]
    IdMismatch {
        expected: serde_json::Value,
        got: serde_json::Value,
    },
    #[error(transparent)]
    Rpc(#[from] RpcError),
}

/// Wrap a [`Child`] whose stdin and stdout are piped, and call methods on it.
pub struct Client {
    child: Child,
    /// `Option` so [`close`](Self::close) and the [`Drop`] reaper can take it
    /// (closing the pipe signals EOF to the worker) without moving a field out
    /// of a `Drop`-implementing type, which the compiler forbids.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    /// Take ownership of `child`; both `stdin` and `stdout` must already be
    /// configured as `Stdio::piped()` by the spawner.
    pub fn from_child(mut child: Child) -> io::Result<Self> {
        let stdin = child.stdin.take().ok_or_else(|| {
            io::Error::other("child stdin not piped")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::other("child stdout not piped")
        })?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    /// Make one request and wait for its response.
    pub fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        let id = serde_json::Value::from(self.next_id);
        self.next_id += 1;

        let req = Request {
            jsonrpc: "2.0".into(),
            id: id.clone(),
            method: method.into(),
            params,
        };
        let stdin = self
            .stdin
            .as_mut()
            .ok_or(ClientError::EarlyExit)?;
        serde_json::to_writer(&mut *stdin, &req)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;

        // Bounded read (shared with the server): a worker that never emits `\n`
        // must not be able to drive the core to OOM (audit finding #2).
        let buf = match read_capped_record(&mut self.stdout, MAX_RECORD_BYTES)? {
            Record::Line(buf) => buf,
            Record::Eof => return Err(ClientError::EarlyExit),
            Record::TooLarge => {
                return Err(ClientError::ResponseTooLarge {
                    cap: MAX_RECORD_BYTES,
                })
            }
        };
        // serde_json tolerates the trailing `\n` (surrounding whitespace is skipped).
        let resp: Response = serde_json::from_slice(&buf)?;
        if resp.id != id {
            return Err(ClientError::IdMismatch {
                expected: id,
                got: resp.id,
            });
        }
        if let Some(err) = resp.error {
            return Err(ClientError::Rpc(err));
        }
        resp.result
            .ok_or_else(|| {
                ClientError::Rpc(RpcError::new(
                    codes::INTERNAL_ERROR,
                    "response had neither result nor error",
                ))
            })
    }

    /// Close stdin (signals EOF to the worker) and wait for it to exit.
    /// Returns the exit status.
    pub fn close(mut self) -> io::Result<std::process::ExitStatus> {
        // Take + drop stdin to send EOF. `self.child.wait()` reaps the worker;
        // the [`Drop`] impl then runs but finds the child already collected (its
        // status is cached), so it is a no-op — no double `waitpid`.
        self.stdin = None;
        self.child.wait()
    }

    /// Kill the worker without waiting for graceful shutdown.
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    /// Blocking reap: wait for the worker to exit and return its status. Unlike
    /// [`close`](Self::close) it borrows rather than consumes, so a `Drop` impl
    /// can guarantee the child is collected (no lingering zombie) on teardown.
    pub fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    /// Non-blocking reap: `Ok(Some(status))` once the worker has exited, `Ok(None)`
    /// while it is still running. Used on the death path to record *why* a worker
    /// exited (e.g. a clean `exit status: 1` vs a `signal: 6 (SIGABRT)` crash)
    /// without risking a hang if the process is unexpectedly still alive.
    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for Client {
    /// Reap the worker so a `Client` dropped without [`close`](Self::close) /
    /// [`wait`](Self::wait) — an error path, or a single-use worker torn down via
    /// `SupervisedWorker`'s (Drop-less) field drop — never leaves a zombie child
    /// that survives until the daemon restarts (#342).
    ///
    /// Best-effort throughout: a teardown `Drop` must neither panic nor hang.
    fn drop(&mut self) {
        // Close stdin first so a still-running worker sees EOF and exits on its
        // own; a `None` here (already closed by `close`) is a no-op.
        self.stdin = None;
        // Fast path: already exited. `try_wait` collects a since-exited child and
        // returns `Ok(Some(_))`; after `close`/`wait` the cached status is
        // returned without a second `waitpid`. `Err` means it is already reaped
        // or otherwise gone — nothing to do either way.
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        // Still running after EOF (unexpected on the teardown path): force it down
        // so the reaping `wait` cannot block, then collect it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod drop_tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// `/proc/<pid>/stat` state field is `Z` for a zombie. Absent `/proc` entry
    /// means the pid was reaped (or never existed) — not a zombie.
    fn is_zombie(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // "pid (comm) STATE ...": the state char follows the last ')'.
            Ok(s) => {
                s.rsplit_once(')')
                    .and_then(|(_, rest)| rest.trim_start().chars().next())
                    == Some('Z')
            }
            Err(_) => false,
        }
    }

    #[test]
    fn dropping_client_reaps_child_no_zombie() {
        // `cat` blocks reading stdin and exits on EOF; piped stdio makes it a
        // valid `Client` child. Dropping the `Client` without `close()`/`wait()`
        // must collect it — the #342 regression left it as a zombie.
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn cat");
        let pid = child.id();
        let client = Client::from_child(child).expect("from_child");
        drop(client); // no close()/wait(): the Drop reaper must run.
        assert!(!is_zombie(pid), "child {pid} left as a zombie after Client drop");
    }

    #[test]
    fn close_still_reaps_and_drop_is_a_noop() {
        // The graceful path stays correct after the Drop addition: close() reaps,
        // and the subsequent implicit Drop finds a cached status (no double wait).
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn cat");
        let pid = child.id();
        let client = Client::from_child(child).expect("from_child");
        let status = client.close().expect("close waits for cat to exit on EOF");
        assert!(status.success(), "cat should exit 0 on EOF");
        assert!(!is_zombie(pid), "child {pid} zombied after close()");
    }
}
