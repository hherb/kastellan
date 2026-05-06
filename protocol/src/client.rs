//! Client-side helper: talk to a child worker over its stdio pipes.

use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout};

use crate::{codes, Request, Response, RpcError};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("worker exited before responding")]
    EarlyExit,
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
            io::Error::new(io::ErrorKind::Other, "child stdin not piped")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "child stdout not piped")
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

        let mut line = String::new();
        let n = self.stdout.read_line(&mut line)?;
        if n == 0 {
            return Err(ClientError::EarlyExit);
        }
        let resp: Response = serde_json::from_str(line.trim())?;
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
}
