//! JSON-RPC dispatch for the one method this worker serves: `python.exec`.

use std::path::PathBuf;

use kastellan_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;

use crate::exec::{run_code, MAX_CODE_BYTES};

/// Env var carrying the absolute interpreter path. Set by the host
/// manifest (`core/src/workers/python_exec.rs`) via `policy.env`; the
/// same name doubles as the operator's daemon-side discovery override.
pub const PYTHON_BIN_ENV: &str = "KASTELLAN_PYTHON_EXEC_PYTHON";

#[derive(Deserialize)]
struct ExecParams {
    code: String,
}

pub struct PythonExecHandler {
    python: PathBuf,
}

impl PythonExecHandler {
    /// Fail-closed startup: no interpreter path, no worker. (The host
    /// manifest always injects it; a bare manual spawn must not guess.)
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var(PYTHON_BIN_ENV)
            .map_err(|_| anyhow::anyhow!("{PYTHON_BIN_ENV} must be set (absolute interpreter path)"))?;
        if raw.trim().is_empty() {
            anyhow::bail!("{PYTHON_BIN_ENV} is set but empty");
        }
        let python = PathBuf::from(raw);
        if !python.is_absolute() {
            anyhow::bail!("{PYTHON_BIN_ENV} must be an absolute path, got {python:?}");
        }
        Ok(Self { python })
    }

    /// Test constructor: bypass the env (unit/integration tests inject
    /// the interpreter directly).
    pub fn with_python(python: PathBuf) -> Self {
        Self { python }
    }
}

impl Handler for PythonExecHandler {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        if method != "python.exec" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
        let p: ExecParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
        if p.code.len() > MAX_CODE_BYTES {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                format!("code is {} bytes; cap is {MAX_CODE_BYTES}", p.code.len()),
            ));
        }

        let outcome = run_code(&self.python, &p.code)
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("spawn failed: {e}")))?;

        Ok(serde_json::json!({
            "exit_code": outcome.exit_code,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
            "stdout_truncated": outcome.stdout_truncated,
            "stderr_truncated": outcome.stderr_truncated,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler() -> PythonExecHandler {
        // The interpreter is never reached by these tests (they fail
        // validation first), so a dummy path is fine.
        PythonExecHandler::with_python(PathBuf::from("/nonexistent/python3"))
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let err = handler()
            .call("python.evaluate", serde_json::json!({"code": "1"}))
            .unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn missing_code_is_invalid_params() {
        let err = handler().call("python.exec", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn non_string_code_is_invalid_params() {
        let err = handler()
            .call("python.exec", serde_json::json!({"code": 42}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn over_cap_code_is_invalid_params() {
        let big = "#".repeat(MAX_CODE_BYTES + 1);
        let err = handler()
            .call("python.exec", serde_json::json!({"code": big}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("cap"));
    }

    #[test]
    fn unspawnable_interpreter_is_operation_failed() {
        let err = handler()
            .call("python.exec", serde_json::json!({"code": "print(1)"}))
            .unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
    }
}
