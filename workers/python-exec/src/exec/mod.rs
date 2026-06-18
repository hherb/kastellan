//! Drive one CPython child: build the argv, pipe the code over stdin,
//! capture + cap the output. The pure pieces ([`python_args`],
//! [`truncate_lossy`], [`serialize_params`]) are unit-testable without an
//! interpreter.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use serde_json::Value;

/// Byte cap on the submitted source. Far beyond any sane agent-authored
/// snippet; exists so a runaway planner can't feed the pipe megabytes.
pub const MAX_CODE_BYTES: usize = 256 * 1024;

/// Byte cap on each captured stream (stdout, stderr independently).
/// Oversized tool results are the handoff cache's job, not this pipe's.
pub const MAX_CAPTURE_BYTES: usize = 256 * 1024;

/// Scratch root inside the jail. On Linux this is bwrap's per-spawn
/// ephemeral tmpfs (#89), granted through Landlock by the explicit
/// `KASTELLAN_LANDLOCK_RW=["/tmp"]` the host policy carries; on macOS
/// the host sets [`WORKER_SCRATCH_ENV`] to a per-spawn writable dir and
/// this constant serves only as the fallback when that var is unset.
pub const SCRATCH_DIR: &str = "/tmp";

/// Env var by which the host hands this worker its per-spawn scratch dir
/// (macOS). Unset on Linux (the bwrap `/tmp` tmpfs is the scratch). **Keep in
/// sync** with core's `kastellan_core::tool_host::ENV_WORKER_SCRATCH`.
pub const WORKER_SCRATCH_ENV: &str = "KASTELLAN_WORKER_SCRATCH";

/// Resolve the scratch dir: the host-provided [`WORKER_SCRATCH_ENV`] value, or
/// the default [`SCRATCH_DIR`] (`/tmp`) when unset. Pure (no I/O) so the
/// fallback is unit-testable; the worker reads the real env at the call site.
pub fn scratch_dir_from_env(lookup: impl Fn(&str) -> Option<String>) -> String {
    lookup(WORKER_SCRATCH_ENV)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| SCRATCH_DIR.to_string())
}

/// Env var carrying the runtime params JSON object to the skill. The worker
/// ALWAYS sets it (default `{}`) so the author's
/// `json.loads(os.environ["KASTELLAN_PYTHON_PARAMS"])` never KeyErrors on the
/// lookup. Survives `-I` (which drops only `PYTHON*` names).
pub const PARAMS_ENV: &str = "KASTELLAN_PYTHON_PARAMS";

/// The execve-safe **inline** threshold. Params serializing to ≤ this many
/// bytes ride the `KASTELLAN_PYTHON_PARAMS` env var; larger payloads are
/// written to `<scratch>/params.json` (the file channel — see
/// [`decide_param_channel`]). Sits under the Linux `MAX_ARG_STRLEN` (128 KiB)
/// per-env-string `execve` wall with headroom. The host gate mirrors the file
/// ceiling, not this threshold (the worker owns the inline-vs-file split).
pub const INLINE_PARAMS_MAX: usize = 64 * 1024;

/// Default file-channel ceiling when `KASTELLAN_PYTHON_PARAMS_FILE_MAX` is
/// unset, and the absolute clamp ceiling regardless of operator config.
pub const PARAMS_FILE_MAX_DEFAULT: usize = 1024 * 1024;
/// Absolute ceiling — operator config can never exceed this (the clamp in [`params_file_max`]).
pub const PARAMS_FILE_MAX_ABS: usize = 16 * 1024 * 1024;

/// Operator-config env naming the file-channel ceiling. The host manifest
/// injects this into the jail (when the operator sets it); **keep in sync**
/// with `core/src/workers/python_exec.rs`.
pub const PARAMS_FILE_MAX_ENV: &str = "KASTELLAN_PYTHON_PARAMS_FILE_MAX";

/// Env var by which the worker hands the child the PATH to the params file —
/// set ONLY when params exceed [`INLINE_PARAMS_MAX`] (the file channel). When
/// present the author reads the file; otherwise the inline [`PARAMS_ENV`].
/// Canonical idiom (covers both sizes):
///
/// ```python
/// import json, os
/// if p := os.environ.get("KASTELLAN_PYTHON_PARAMS_FILE"):
///     with open(p) as f:
///         params = json.load(f)
/// else:
///     params = json.loads(os.environ.get("KASTELLAN_PYTHON_PARAMS", "{}"))
/// ```
pub const PARAMS_FILE_ENV: &str = "KASTELLAN_PYTHON_PARAMS_FILE";

/// Basename of the params file written into the scratch dir.
pub const PARAMS_FILE_NAME: &str = "params.json";

/// Which transport carries the serialized params to the child interpreter.
#[derive(Debug, PartialEq, Eq)]
pub enum ParamChannel {
    /// Fits the execve-safe env var — set [`PARAMS_ENV`] directly.
    Inline,
    /// Too big for the env var — write `<scratch>/params.json` and point the
    /// child at it via [`PARAMS_FILE_ENV`].
    File,
}

/// Why a params payload was rejected. The handler maps both arms to
/// JSON-RPC `INVALID_PARAMS`.
#[derive(Debug)]
pub enum ParamsError {
    /// Present but not a JSON object (array / scalar / null).
    NotObject,
    /// Serialized object exceeds the applicable ceiling.
    TooLarge { got: usize, max: usize },
}

impl std::fmt::Display for ParamsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamsError::NotObject => write!(f, "params must be a JSON object"),
            ParamsError::TooLarge { got, max } => {
                write!(f, "params is {got} bytes; cap is {max}")
            }
        }
    }
}

/// Resolve the file-channel ceiling from [`PARAMS_FILE_MAX_ENV`]: parse the
/// value, fall back to [`PARAMS_FILE_MAX_DEFAULT`] when unset/empty/unparseable,
/// then clamp to `[INLINE_PARAMS_MAX, PARAMS_FILE_MAX_ABS]` (a ceiling below the
/// inline threshold is nonsensical — the file channel only fires above it).
/// Pure (lookup injected) so the parse/clamp truth-table is unit-testable.
pub fn params_file_max(lookup: impl Fn(&str) -> Option<String>) -> usize {
    lookup(PARAMS_FILE_MAX_ENV)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(PARAMS_FILE_MAX_DEFAULT)
        .clamp(INLINE_PARAMS_MAX, PARAMS_FILE_MAX_ABS)
}

/// Decide the channel for a params payload of `serialized_len` bytes:
/// `≤ inline_max` → [`ParamChannel::Inline`]; `≤ file_max` → [`ParamChannel::File`];
/// otherwise [`ParamsError::TooLarge`]. Pure. `file_max ≥ inline_max` is
/// guaranteed by [`params_file_max`]'s clamp.
pub fn decide_param_channel(
    serialized_len: usize,
    inline_max: usize,
    file_max: usize,
) -> Result<ParamChannel, ParamsError> {
    if serialized_len <= inline_max {
        Ok(ParamChannel::Inline)
    } else if serialized_len <= file_max {
        Ok(ParamChannel::File)
    } else {
        Err(ParamsError::TooLarge { got: serialized_len, max: file_max })
    }
}

/// The child env pairs for the chosen params channel. `Inline` → just
/// [`PARAMS_ENV`]; `File` → [`PARAMS_ENV`]`="{}"` (the stable empty-default so a
/// legacy unconditional `json.loads(os.environ["KASTELLAN_PYTHON_PARAMS"])`
/// never `KeyError`s) **plus** [`PARAMS_FILE_ENV`]`=file_path`. Pure (no I/O) so
/// the env contract is unit-testable; [`run_code`] writes the file then applies
/// these.
pub fn params_env_pairs(
    channel: &ParamChannel,
    params_json: &str,
    file_path: &str,
) -> Vec<(&'static str, String)> {
    match channel {
        ParamChannel::Inline => vec![(PARAMS_ENV, params_json.to_string())],
        ParamChannel::File => vec![
            (PARAMS_ENV, "{}".to_string()),
            (PARAMS_FILE_ENV, file_path.to_string()),
        ],
    }
}

/// Serialize the optional params object to its compact JSON string.
///
/// * `None` ⇒ `"{}"` (the stable empty-default contract).
/// * `Some(obj)` where `obj` is a JSON object ⇒ its compact serialization.
/// * `Some(non-object)` ⇒ [`ParamsError::NotObject`].
///
/// Size enforcement is **not** here — [`decide_param_channel`] decides inline
/// vs file vs reject by the serialized length. Pure (no I/O); the worker is the
/// AUTHORITATIVE enforcer (a direct/malformed call must never reach `execve`
/// with garbage). serde escapes every control char as `\uXXXX`, so the result
/// is safe both as a single C-string env value and as file bytes.
pub fn serialize_params(params: &Option<Value>) -> Result<String, ParamsError> {
    match params {
        None => Ok("{}".to_string()),
        Some(v @ Value::Object(_)) => Ok(serde_json::to_string(v).unwrap_or_default()),
        Some(_) => Err(ParamsError::NotObject),
    }
}

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

/// Read at most `cap` bytes from `r` into a buffer, then keep draining
/// (discarding) to EOF so the child never blocks on a full pipe. Worker
/// memory stays O(cap) no matter how much the child prints — the
/// runaway-print payload is bounded by the policy's CPU/wall caps, not
/// by this process's heap. Returns the captured bytes and whether
/// anything beyond `cap` was discarded.
pub fn read_capped<R: Read>(mut r: R, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            return Ok((buf, truncated));
        }
        if buf.len() < cap {
            let take = (cap - buf.len()).min(n);
            buf.extend_from_slice(&chunk[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
}

/// Lossy-decode `bytes` and cap the result at `cap` bytes without
/// splitting a UTF-8 sequence. Returns the (possibly shortened) string
/// and whether truncation happened.
///
/// Capping happens at *both* stages: [`read_capped`] bounds the raw
/// bytes buffered from the child, and this bounds the decoded string —
/// necessary because lossy decoding can inflate (each invalid byte
/// becomes the 3-byte U+FFFD), so `cap` raw bytes may decode to up to
/// `3 × cap` string bytes. The result is therefore always ≤ `cap`
/// bytes, and the reported flag is the OR of the two stages.
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

/// Write `json` to `path` as a private (0600) file, truncating any prior
/// content. Both worker targets (Linux, macOS) are unix, so the mode is set
/// atomically at open. Fail-closed: an error propagates so the worker never
/// silently falls back to the oversize env channel.
fn write_params_file(path: &Path, json: &str) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(json.as_bytes())
}

/// Run `code` under `python`. The child's environment is cleared — the
/// jail's lockdown vars are not its business — then given exactly
/// `TMPDIR`/`HOME` pointing at the scratch dir, with cwd there too (when
/// it exists, which it always does inside the jail). Runtime params arrive
/// via `channel`: inline (≤ 64 KiB) in [`PARAMS_ENV`], or file (> 64 KiB)
/// written to `<scratch>/params.json` and pointed at by [`PARAMS_FILE_ENV`].
pub fn run_code(
    python: &Path,
    code: &str,
    params_json: &str,
    channel: ParamChannel,
) -> std::io::Result<ExecOutcome> {
    let scratch = scratch_dir_from_env(|k| std::env::var(k).ok());
    let file_path = Path::new(&scratch).join(PARAMS_FILE_NAME);
    if matches!(channel, ParamChannel::File) {
        // Fail-closed: a scratch-write error aborts the run rather than
        // falling back to the oversize env channel (which would exceed the
        // execve wall).
        write_params_file(&file_path, params_json)?;
    }
    let mut cmd = Command::new(python);
    cmd.args(python_args())
        .env_clear()
        .env("TMPDIR", &scratch)
        .env("HOME", &scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in params_env_pairs(&channel, params_json, &file_path.to_string_lossy()) {
        cmd.env(k, v);
    }
    if Path::new(&scratch).is_dir() {
        cmd.current_dir(&scratch);
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

    // Capped concurrent capture (NOT wait_with_output, which buffers the
    // child's entire output unbounded — on macOS, where Seatbelt has no
    // memory cap, a `print('x' * 10**9)` payload would balloon the
    // worker's own RSS). Both pipes are drained in parallel so neither
    // can fill and stall the child.
    let out_pipe = child.stdout.take().expect("stdout was piped");
    let err_pipe = child.stderr.take().expect("stderr was piped");
    let out_reader = std::thread::spawn(move || read_capped(out_pipe, MAX_CAPTURE_BYTES));
    let err_reader = std::thread::spawn(move || read_capped(err_pipe, MAX_CAPTURE_BYTES));

    let status = child.wait()?;
    let _ = feeder.join();
    let (out_bytes, out_raw_truncated) =
        out_reader.join().expect("stdout reader thread panicked")?;
    let (err_bytes, err_raw_truncated) =
        err_reader.join().expect("stderr reader thread panicked")?;

    let (stdout, out_decode_truncated) = truncate_lossy(&out_bytes, MAX_CAPTURE_BYTES);
    let (stderr, err_decode_truncated) = truncate_lossy(&err_bytes, MAX_CAPTURE_BYTES);
    Ok(ExecOutcome {
        exit_code: status.code(),
        stdout,
        stderr,
        stdout_truncated: out_raw_truncated || out_decode_truncated,
        stderr_truncated: err_raw_truncated || err_decode_truncated,
    })
}

#[cfg(test)]
mod tests;
