//! Client-side helper: talk to a child worker over its stdio pipes.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout};

use crate::{codes, Request, Response, RpcError, MAX_RECORD_BYTES};

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
    stdin: ChildStdin,
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
            stdin,
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
        serde_json::to_writer(&mut self.stdin, &req)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        // Bounded read: a worker that never emits `\n` must not be able to
        // drive the core to OOM (audit finding #2).
        let buf = match read_capped_record(&mut self.stdout, MAX_RECORD_BYTES)? {
            Some(buf) => buf,
            None => return Err(ClientError::EarlyExit),
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
        // Drop stdin to send EOF.
        drop(self.stdin);
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

/// Read one `\n`-terminated record from `r`, buffering at most `cap` bytes.
///
/// `Ok(None)` = EOF before any byte; `Ok(Some(bytes))` = the record (including
/// a trailing `\n` if present); `Err(ResponseTooLarge)` if the record would
/// exceed `cap` without a newline. `take(cap + 1)` bounds the allocation so an
/// unbounded line cannot OOM the reader (audit finding #2).
fn read_capped_record<R: BufRead>(
    r: &mut R,
    cap: usize,
) -> Result<Option<Vec<u8>>, ClientError> {
    let mut buf: Vec<u8> = Vec::new();
    let n = (&mut *r)
        .take(cap as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if n > cap {
        return Err(ClientError::ResponseTooLarge { cap });
    }
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_capped_record_returns_a_normal_line() {
        let mut r: &[u8] = b"{\"ok\":true}\nnext";
        let got = read_capped_record(&mut r, 1024).unwrap();
        assert_eq!(got.as_deref(), Some(&b"{\"ok\":true}\n"[..]));
    }

    #[test]
    fn read_capped_record_eof_is_none() {
        let mut r: &[u8] = b"";
        assert!(read_capped_record(&mut r, 1024).unwrap().is_none());
    }

    #[test]
    fn read_capped_record_rejects_over_cap_line_without_ooming() {
        // A 10 KiB line with NO newline against a 16-byte cap: the reader must
        // buffer at most cap+1 bytes and error, not grow unbounded. This is the
        // OOM guard (finding #2) in miniature.
        let flood = vec![b'a'; 10 * 1024];
        let mut r: &[u8] = &flood;
        match read_capped_record(&mut r, 16) {
            Err(ClientError::ResponseTooLarge { cap }) => assert_eq!(cap, 16),
            other => panic!("expected ResponseTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn read_capped_record_accepts_record_at_cap() {
        // 15 content bytes + newline = 16 total; the cap counts the whole
        // record (newline included), so this is exactly at budget and accepted.
        let mut r: &[u8] = b"aaaaaaaaaaaaaaa\n";
        let got = read_capped_record(&mut r, 16).unwrap();
        assert_eq!(got.unwrap().len(), 16);
    }
}
