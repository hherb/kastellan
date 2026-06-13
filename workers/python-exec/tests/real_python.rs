//! Integration tests against a real system CPython, when one is present
//! (`[SKIP]`-as-pass otherwise — same posture as the sandbox suites).
//! These run *unjailed* (the jail is the host's job and is exercised by
//! `core/tests/python_exec_e2e.rs`); what they pin is the worker's own
//! drive: flags, stdin delivery, env isolation, capture caps, and the
//! exception-is-not-an-RPC-error contract.

use std::path::PathBuf;

use kastellan_protocol::server::Handler;
use kastellan_worker_python_exec::exec::MAX_CAPTURE_BYTES;
use kastellan_worker_python_exec::handler::PythonExecHandler;

/// First existing interpreter from the manifest's per-OS candidate
/// cascade, or `None` → `[SKIP]`. Mirrors
/// `core/src/workers/python_exec.rs::PYTHON_CANDIDATES` (this crate can't
/// depend on `core`, so the list is duplicated — keep them in sync). On
/// macOS `/usr/bin/python3` is deliberately absent: it is Apple's xcrun
/// shim, which re-injects `SDKROOT`/`CPATH`/etc. into the real python
/// child and so breaks the env-isolation contract these tests pin (and
/// cannot run inside the jail at all).
fn find_python() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let candidates = [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/Library/Developer/CommandLineTools/usr/bin/python3",
    ];
    #[cfg(not(target_os = "macos"))]
    let candidates = ["/usr/bin/python3", "/usr/local/bin/python3"];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    eprintln!("\n[SKIP] no python3 found on this host; skipping real-interpreter tests\n");
    None
}

fn call(python: PathBuf, code: &str) -> serde_json::Value {
    let mut h = PythonExecHandler::with_python(python);
    h.call("python.exec", serde_json::json!({ "code": code }))
        .expect("python.exec should return a result, not an RPC error")
}

#[test]
fn happy_path_prints_and_exits_zero() {
    let Some(py) = find_python() else { return };
    let r = call(py, "print(6 * 7)");
    assert_eq!(r["exit_code"], 0);
    assert_eq!(r["stdout"].as_str().unwrap().trim_end(), "42");
    assert_eq!(r["stderr"], "");
    assert_eq!(r["stdout_truncated"], false);
}

#[test]
fn exception_comes_back_as_exit_code_not_rpc_error() {
    let Some(py) = find_python() else { return };
    let r = call(py, "raise ValueError('boom')");
    assert_eq!(r["exit_code"], 1);
    let stderr = r["stderr"].as_str().unwrap();
    assert!(stderr.contains("ValueError"), "stderr: {stderr}");
    assert!(stderr.contains("boom"), "stderr: {stderr}");
}

#[test]
fn child_env_is_cleared_except_tmpdir_and_home() {
    let Some(py) = find_python() else { return };
    let r = call(
        py,
        "import os; ks = sorted(k for k in os.environ); print(','.join(ks))",
    );
    assert_eq!(r["exit_code"], 0);
    let keys = r["stdout"].as_str().unwrap().trim_end();
    // Python itself may add LC_CTYPE on some platforms; KASTELLAN_PYTHON_PARAMS
    // is intentionally injected by run_code. What must NOT leak is anything
    // else kastellan- or host-shaped.
    for k in keys.split(',').filter(|k| !k.is_empty()) {
        assert!(
            matches!(
                k,
                "TMPDIR"
                    | "HOME"
                    | "LC_CTYPE"
                    | "__CF_USER_TEXT_ENCODING"
                    | "KASTELLAN_PYTHON_PARAMS"
            ),
            "unexpected env var leaked into the python child: {k} (full: {keys})"
        );
    }
}

#[test]
fn site_packages_are_not_on_sys_path() {
    let Some(py) = find_python() else { return };
    let r = call(
        py,
        "import sys; print(any('site-packages' in p or 'dist-packages' in p for p in sys.path))",
    );
    assert_eq!(r["exit_code"], 0);
    assert_eq!(r["stdout"].as_str().unwrap().trim_end(), "False");
}

#[test]
fn oversized_stdout_is_capped_and_flagged() {
    let Some(py) = find_python() else { return };
    let r = call(py, "import sys; sys.stdout.write('x' * 600_000)");
    assert_eq!(r["exit_code"], 0);
    assert_eq!(r["stdout"].as_str().unwrap().len(), MAX_CAPTURE_BYTES);
    assert_eq!(r["stdout_truncated"], true);
    assert_eq!(r["stderr_truncated"], false);
}

#[test]
fn flooding_both_streams_is_capped_without_deadlock() {
    let Some(py) = find_python() else { return };
    // Both pipes far past the kernel pipe buffer simultaneously: pins
    // that the two reader threads drain concurrently (a sequential read
    // would deadlock once the unread pipe fills and stalls the child)
    // and that worker memory stays O(cap) rather than O(output).
    // concat! (not a `\`-continued literal, which strips the block
    // indentation Python needs).
    let code = concat!(
        "import sys\n",
        "for _ in range(200):\n",
        "    sys.stdout.write('o' * 4096)\n",
        "    sys.stderr.write('e' * 4096)\n",
    );
    let r = call(py, code);
    assert_eq!(r["exit_code"], 0);
    assert_eq!(r["stdout"].as_str().unwrap().len(), MAX_CAPTURE_BYTES);
    assert_eq!(r["stderr"].as_str().unwrap().len(), MAX_CAPTURE_BYTES);
    assert_eq!(r["stdout_truncated"], true);
    assert_eq!(r["stderr_truncated"], true);
}

#[test]
fn large_code_over_the_pipe_buffer_still_runs() {
    let Some(py) = find_python() else { return };
    // > 64 KiB of source exercises the stdin feeder thread past the
    // kernel pipe buffer.
    let mut code = String::new();
    for i in 0..8000 {
        code.push_str(&format!("v{i} = {i}\n"));
    }
    code.push_str("print(v7999)\n");
    assert!(code.len() > 64 * 1024);
    let r = call(py, &code);
    assert_eq!(r["exit_code"], 0);
    assert_eq!(r["stdout"].as_str().unwrap().trim_end(), "7999");
}

#[test]
fn scratch_write_round_trip_under_tmpdir() {
    let Some(py) = find_python() else { return };
    // Unjailed here, so this writes the host /tmp via tempfile — the
    // point is the worker wiring (TMPDIR honoured, cwd usable), not the
    // jail; the jailed equivalent lives in python_exec_e2e.
    // NB: built with explicit \n + indentation — a Rust `\`-continued
    // string literal strips leading whitespace, which would destroy
    // Python's block indentation.
    let code = concat!(
        "import tempfile\n",
        "with tempfile.NamedTemporaryFile('w+', delete=True) as f:\n",
        "    f.write('scratch-ok')\n",
        "    f.flush()\n",
        "    f.seek(0)\n",
        "    print(f.read())\n",
    );
    let r = call(py, code);
    assert_eq!(r["exit_code"], 0, "stderr: {}", r["stderr"]);
    assert_eq!(r["stdout"].as_str().unwrap().trim_end(), "scratch-ok");
}
