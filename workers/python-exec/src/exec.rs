//! Drive one CPython child: build the argv, pipe the code over stdin,
//! capture + cap the output. The pure pieces ([`python_args`],
//! [`truncate_lossy`]) are unit-testable without an interpreter.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Byte cap on the submitted source. Far beyond any sane agent-authored
/// snippet; exists so a runaway planner can't feed the pipe megabytes.
pub const MAX_CODE_BYTES: usize = 256 * 1024;

/// Byte cap on each captured stream (stdout, stderr independently).
/// Oversized tool results are the handoff cache's job, not this pipe's.
pub const MAX_CAPTURE_BYTES: usize = 256 * 1024;

/// Scratch root inside the jail. On Linux this is bwrap's per-spawn
/// ephemeral tmpfs (#89), granted through Landlock by the explicit
/// `KASTELLAN_LANDLOCK_RW=["/tmp"]` the host policy carries; on macOS
/// slice #1 it exists but is not writable (Seatbelt `(deny default)`).
pub const SCRATCH_DIR: &str = "/tmp";

/// Interpreter flags, pinned by a unit test:
///
/// * `-I` isolated — implies `-E` (ignore `PYTHON*` env) + `-s` (no user
///   site dir), and drops the script dir/cwd from `sys.path`.
/// * `-S` — skip the `site` module: system site-/dist-packages never
///   join `sys.path`. This is the roadmap's "curated stdlib bind" — a
///   determinism measure; the *security* boundary is the jail.
/// * `-B` — never write `.pyc`.
/// * `-` — read the program from stdin until EOF (no scratch write, no
///   argv-size limit, nothing in `/proc/*/cmdline`).
pub fn python_args() -> [&'static str; 4] {
    ["-I", "-S", "-B", "-"]
}

/// Outcome of one interpreter run, pre-truncation already applied.
#[derive(Debug)]
pub struct ExecOutcome {
    /// `None` when the child was killed by a signal (SIGKILL from the
    /// cgroup OOM-killer, SIGXCPU past the rlimit, SIGSYS from seccomp).
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Lossy-decode `bytes` and cap the result at `cap` bytes without
/// splitting a UTF-8 sequence. Returns the (possibly shortened) string
/// and whether truncation happened.
pub fn truncate_lossy(bytes: &[u8], cap: usize) -> (String, bool) {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= cap {
        return (s.into_owned(), false);
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// Run `code` under `python`. The child's environment is cleared — the
/// jail's lockdown vars are not its business — then given exactly
/// `TMPDIR`/`HOME` pointing at the scratch dir, with cwd there too (when
/// it exists, which it always does inside the jail).
pub fn run_code(python: &Path, code: &str) -> std::io::Result<ExecOutcome> {
    let mut cmd = Command::new(python);
    cmd.args(python_args())
        .env_clear()
        .env("TMPDIR", SCRATCH_DIR)
        .env("HOME", SCRATCH_DIR)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if Path::new(SCRATCH_DIR).is_dir() {
        cmd.current_dir(SCRATCH_DIR);
    }
    let mut child = cmd.spawn()?;

    // CPython with `-` reads the whole program to EOF before executing,
    // so a single-threaded write-then-wait cannot deadlock — but feed
    // stdin from a helper thread anyway so a pathological interpreter
    // that interleaves reads with output still drains cleanly.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let code_owned = code.as_bytes().to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&code_owned);
        // stdin drops here → EOF.
    });
    let output = child.wait_with_output()?;
    let _ = feeder.join();

    let (stdout, stdout_truncated) = truncate_lossy(&output.stdout, MAX_CAPTURE_BYTES);
    let (stderr, stderr_truncated) = truncate_lossy(&output.stderr, MAX_CAPTURE_BYTES);
    Ok(ExecOutcome {
        exit_code: output.status.code(),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag set is a containment decision (curated stdlib, env
    /// isolation, stdin delivery) — pin it so a change is deliberate.
    #[test]
    fn python_args_pin_isolated_no_site_no_pyc_stdin() {
        assert_eq!(python_args(), ["-I", "-S", "-B", "-"]);
    }

    #[test]
    fn truncate_lossy_passes_small_input_through() {
        let (s, t) = truncate_lossy(b"hello", 16);
        assert_eq!(s, "hello");
        assert!(!t);
    }

    #[test]
    fn truncate_lossy_caps_at_exact_boundary_without_flag() {
        let (s, t) = truncate_lossy(b"abcd", 4);
        assert_eq!(s, "abcd");
        assert!(!t);
    }

    #[test]
    fn truncate_lossy_never_splits_a_multibyte_char() {
        // "é" is 2 bytes in UTF-8; a 3-byte cap on "aéb" must cut before
        // the 'b' lands but also must not split 'é'.
        let bytes = "aéb".as_bytes(); // [0x61, 0xC3, 0xA9, 0x62]
        let (s, t) = truncate_lossy(bytes, 2);
        assert_eq!(s, "a");
        assert!(t);
        let (s3, t3) = truncate_lossy(bytes, 3);
        assert_eq!(s3, "aé");
        assert!(t3);
    }

    #[test]
    fn truncate_lossy_handles_invalid_utf8_lossily() {
        let (s, t) = truncate_lossy(&[0x61, 0xFF, 0x62], 64);
        assert_eq!(s, "a\u{FFFD}b");
        assert!(!t);
    }
}
