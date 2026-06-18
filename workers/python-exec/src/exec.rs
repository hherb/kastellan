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
pub const PARAMS_FILE_MAX_ABS: usize = 16 * 1024 * 1024;

/// Operator-config env naming the file-channel ceiling. The host manifest
/// injects this into the jail (when the operator sets it); **keep in sync**
/// with `core/src/workers/python_exec.rs`.
pub const PARAMS_FILE_MAX_ENV: &str = "KASTELLAN_PYTHON_PARAMS_FILE_MAX";

/// Env var by which the worker hands the child the PATH to the params file —
/// set ONLY when the file channel is used. The author reads the file when this
/// is present, else the inline [`PARAMS_ENV`]. (See the [`PARAMS_FILE_ENV`]
/// doc-comment in Task 2 for the author idiom.)
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

/// Serialize the optional params object to the env-var string.
///
/// * `None` ⇒ `"{}"` (the stable empty-default contract).
/// * `Some(obj)` where `obj` is a JSON object ⇒ its compact serialization,
///   rejected if it exceeds [`INLINE_PARAMS_MAX`].
/// * `Some(non-object)` ⇒ [`ParamsError::NotObject`].
///
/// Pure (no I/O) so it is unit-testable without an interpreter. The worker is
/// the AUTHORITATIVE enforcer of these checks — a direct or malformed call must
/// never reach `execve` with an oversize/garbage env var.
pub fn serialize_params(params: &Option<Value>) -> Result<String, ParamsError> {
    match params {
        None => Ok("{}".to_string()),
        Some(v @ Value::Object(_)) => {
            // Safe: a `Value` always serializes. serde escapes every control
            // char as a JSON \uXXXX sequence (so NUL never appears raw),
            // making the result safe to hand to execve as one C-string env
            // value.
            let s = serde_json::to_string(v).unwrap_or_default();
            if s.len() > INLINE_PARAMS_MAX {
                return Err(ParamsError::TooLarge { got: s.len(), max: INLINE_PARAMS_MAX });
            }
            Ok(s)
        }
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

/// Run `code` under `python`. The child's environment is cleared — the
/// jail's lockdown vars are not its business — then given exactly
/// `TMPDIR`/`HOME` pointing at the scratch dir, with cwd there too (when
/// it exists, which it always does inside the jail). Runtime params arrive
/// as the JSON string `params_json` in the [`PARAMS_ENV`] env var; the
/// value has already been validated and serialized by the caller.
pub fn run_code(python: &Path, code: &str, params_json: &str) -> std::io::Result<ExecOutcome> {
    let scratch = scratch_dir_from_env(|k| std::env::var(k).ok());
    let mut cmd = Command::new(python);
    cmd.args(python_args())
        .env_clear()
        .env("TMPDIR", &scratch)
        .env("HOME", &scratch)
        .env(PARAMS_ENV, params_json)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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

    #[test]
    fn read_capped_under_cap_returns_everything_unflagged() {
        let (bytes, truncated) = read_capped(std::io::Cursor::new(b"hello".to_vec()), 16).unwrap();
        assert_eq!(bytes, b"hello");
        assert!(!truncated);
    }

    #[test]
    fn read_capped_over_cap_keeps_prefix_drains_rest_and_flags() {
        // 100 KiB source, 4 KiB cap: the buffer must hold exactly the
        // first 4 KiB (multiple-chunk path), the flag must be set, and
        // the read must run to EOF (drain) rather than stopping at cap.
        let data: Vec<u8> = (0..100 * 1024).map(|i| (i % 251) as u8).collect();
        let cap = 4 * 1024;
        let (bytes, truncated) = read_capped(std::io::Cursor::new(data.clone()), cap).unwrap();
        assert_eq!(bytes, &data[..cap]);
        assert!(truncated);
    }

    #[test]
    fn read_capped_at_exact_cap_is_unflagged() {
        let data = vec![7u8; 64];
        let (bytes, truncated) = read_capped(std::io::Cursor::new(data.clone()), 64).unwrap();
        assert_eq!(bytes, data);
        assert!(!truncated);
    }

    #[test]
    fn serialize_params_none_is_empty_object() {
        assert_eq!(serialize_params(&None).unwrap(), "{}");
    }

    #[test]
    fn serialize_params_object_round_trips() {
        let v = serde_json::json!({"a": 1, "b": "x"});
        let s = serialize_params(&Some(v)).unwrap();
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back, serde_json::json!({"a": 1, "b": "x"}));
    }

    #[test]
    fn serialize_params_rejects_non_object() {
        assert!(matches!(
            serialize_params(&Some(serde_json::json!([1, 2]))),
            Err(ParamsError::NotObject)
        ));
        assert!(matches!(
            serialize_params(&Some(serde_json::json!("flat"))),
            Err(ParamsError::NotObject)
        ));
        assert!(matches!(
            serialize_params(&Some(serde_json::Value::Null)),
            Err(ParamsError::NotObject)
        ));
    }

    #[test]
    fn serialize_params_rejects_over_cap() {
        let big = "x".repeat(INLINE_PARAMS_MAX);
        let v = serde_json::json!({ "k": big });
        assert!(matches!(
            serialize_params(&Some(v)),
            Err(ParamsError::TooLarge { .. })
        ));
    }

    #[test]
    fn serialize_params_allows_newlines_in_values() {
        let v = serde_json::json!({ "text": "line1\nline2" });
        let s = serialize_params(&Some(v)).unwrap();
        assert!(!s.contains('\n'), "raw newline must be escaped inside JSON");
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["text"], "line1\nline2");
    }

    #[test]
    fn serialize_params_escapes_nul_no_raw_nul_for_execve() {
        // The serialized string becomes a single C-string env value handed to
        // `execve`; a raw NUL would silently truncate it. serde escapes NUL
        // as the 6-char sequence \u0000, so the output must contain no raw
        // NUL byte and must still round-trip to the original value.
        let v = serde_json::json!({ "text": "a\u{0000}b" });
        let s = serialize_params(&Some(v)).unwrap();
        assert!(!s.as_bytes().contains(&0), "serialized params must be NUL-free");
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["text"], "a\u{0000}b");
    }

    #[test]
    fn scratch_dir_defaults_to_tmp_when_unset() {
        let s = scratch_dir_from_env(|_| None);
        assert_eq!(s, "/tmp");
    }

    #[test]
    fn scratch_dir_uses_env_when_set() {
        let s = scratch_dir_from_env(|k| {
            (k == WORKER_SCRATCH_ENV).then(|| "/var/folders/xx/pyexec-1-1".to_string())
        });
        assert_eq!(s, "/var/folders/xx/pyexec-1-1");
    }

    #[test]
    fn scratch_dir_falls_back_when_env_is_empty() {
        let s = scratch_dir_from_env(|_| Some(String::new()));
        assert_eq!(s, "/tmp");
    }

    #[test]
    fn params_file_max_defaults_when_unset() {
        let m = params_file_max(|_| None);
        assert_eq!(m, PARAMS_FILE_MAX_DEFAULT);
    }

    #[test]
    fn params_file_max_parses_a_valid_value() {
        let m = params_file_max(|k| (k == PARAMS_FILE_MAX_ENV).then(|| "200000".to_string()));
        assert_eq!(m, 200_000);
    }

    #[test]
    fn params_file_max_garbage_falls_back_to_default() {
        let m = params_file_max(|k| (k == PARAMS_FILE_MAX_ENV).then(|| "not-a-number".to_string()));
        assert_eq!(m, PARAMS_FILE_MAX_DEFAULT);
    }

    #[test]
    fn params_file_max_clamps_below_inline_and_above_abs() {
        // Below the inline threshold is nonsensical (file channel only fires
        // above inline) → clamp up.
        let low = params_file_max(|k| (k == PARAMS_FILE_MAX_ENV).then(|| "1".to_string()));
        assert_eq!(low, INLINE_PARAMS_MAX);
        // Above the absolute ceiling → clamp down.
        let high = params_file_max(|k| (k == PARAMS_FILE_MAX_ENV).then(|| "999999999".to_string()));
        assert_eq!(high, PARAMS_FILE_MAX_ABS);
    }

    #[test]
    fn decide_inline_at_and_below_threshold() {
        assert_eq!(
            decide_param_channel(0, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT).unwrap(),
            ParamChannel::Inline
        );
        assert_eq!(
            decide_param_channel(INLINE_PARAMS_MAX, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT).unwrap(),
            ParamChannel::Inline
        );
    }

    #[test]
    fn decide_file_just_over_inline_and_at_ceiling() {
        assert_eq!(
            decide_param_channel(INLINE_PARAMS_MAX + 1, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT).unwrap(),
            ParamChannel::File
        );
        assert_eq!(
            decide_param_channel(PARAMS_FILE_MAX_DEFAULT, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT).unwrap(),
            ParamChannel::File
        );
    }

    #[test]
    fn decide_too_large_over_ceiling() {
        let err = decide_param_channel(
            PARAMS_FILE_MAX_DEFAULT + 1, INLINE_PARAMS_MAX, PARAMS_FILE_MAX_DEFAULT,
        )
        .unwrap_err();
        assert!(matches!(err, ParamsError::TooLarge { .. }));
    }

    #[test]
    fn params_env_pairs_inline_sets_only_params_env() {
        let pairs = params_env_pairs(&ParamChannel::Inline, r#"{"a":1}"#, "/unused");
        assert_eq!(pairs, vec![(PARAMS_ENV, r#"{"a":1}"#.to_string())]);
    }

    #[test]
    fn params_env_pairs_file_sets_empty_default_plus_path() {
        let pairs = params_env_pairs(&ParamChannel::File, r#"{"a":1}"#, "/tmp/params.json");
        assert_eq!(
            pairs,
            vec![
                (PARAMS_ENV, "{}".to_string()),
                (PARAMS_FILE_ENV, "/tmp/params.json".to_string()),
            ]
        );
    }
}
