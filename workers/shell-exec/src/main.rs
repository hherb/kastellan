//! shell-exec: a tool worker that runs an argv from a strict allowlist and
//! returns stdout/stderr/exit code over JSON-RPC stdio. **No shell interpretation.**
//!
//! The allowlist is read once at startup from environment variable
//! `HHAGENT_SHELL_ALLOWLIST` as a JSON array of `[argv0, argv1, ...]` patterns.
//! Each pattern is exact-match on `argv[0]` and is the *only* allowed entry
//! point. The agent core is responsible for keeping that env var deny-by-default.
//!
//! Method exposed:
//!   - `shell.exec` — params: `{ "argv": ["program", "arg1", ...] }`
//!     result: `{ "exit_code": int, "stdout": str, "stderr": str }`
//!     err code [`POLICY_DENIED`] if argv[0] is not on the allowlist.

use std::collections::HashSet;
use std::process::Command;

use hhagent_protocol::{codes, server::Handler, server::serve_stdio, RpcError};
use serde::Deserialize;

#[derive(Deserialize)]
struct ExecParams {
    argv: Vec<String>,
}

struct ShellExecHandler {
    allowed_argv0: HashSet<String>,
}

impl ShellExecHandler {
    fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var("HHAGENT_SHELL_ALLOWLIST").unwrap_or_else(|_| "[]".to_string());
        let allowed: Vec<String> = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!("HHAGENT_SHELL_ALLOWLIST is not a valid JSON array of strings: {e}")
        })?;
        Ok(Self {
            allowed_argv0: allowed.into_iter().collect(),
        })
    }
}

impl Handler for ShellExecHandler {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        if method != "shell.exec" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
        let p: ExecParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
        let program = p.argv.first().ok_or_else(|| {
            RpcError::new(codes::INVALID_PARAMS, "argv must be non-empty")
        })?;
        if !self.allowed_argv0.contains(program) {
            return Err(RpcError::new(
                codes::POLICY_DENIED,
                format!("argv[0] {program:?} not in allowlist"),
            ));
        }

        let output = Command::new(program)
            .args(&p.argv[1..])
            .output()
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("exec failed: {e}")))?;

        Ok(serde_json::json!({
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }))
    }
}

fn main() -> anyhow::Result<()> {
    let mut handler = ShellExecHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
