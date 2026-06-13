//! Binary entry point: fail-closed env resolution, then the prelude's
//! lockdown + serve loop (Landlock + seccomp + rlimit before any I/O).

use kastellan_worker_prelude::serve_stdio;
use kastellan_worker_python_exec::handler::PythonExecHandler;

fn main() -> anyhow::Result<()> {
    let mut handler = PythonExecHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
