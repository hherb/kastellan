# python-exec >64 KiB scratch-file param channel — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let agent-authored Python skills receive runtime params larger than the 64 KiB env-var limit, by having the python-exec worker write large params to a file in its own per-spawn writable scratch and pointing the child interpreter at the path.

**Architecture:** Params already reach the worker over (unbounded) JSON-RPC stdio; the only real limit is the worker→child `execve` env-var size. So the worker decides by serialized size: ≤64 KiB → inline env `KASTELLAN_PYTHON_PARAMS` (unchanged, byte-identical); >64 KiB and ≤ ceiling → write `<scratch>/params.json` (mode 0600) and set `KASTELLAN_PYTHON_PARAMS_FILE` to the in-jail path while leaving `KASTELLAN_PYTHON_PARAMS="{}"`; over the ceiling → fail-closed. The ceiling is operator-configurable via `KASTELLAN_PYTHON_PARAMS_FILE_MAX` (default 1 MiB, clamped to 16 MiB), enforced authoritatively worker-side; the host gate keeps a fixed 16 MiB structural backstop so its functions stay pure.

**Tech Stack:** Rust (workspace crates `kastellan-core`, `kastellan-worker-python-exec`), `serde_json`, std `Command`/`OpenOptions` (unix), `tokio` for the e2e. No new dependencies.

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible dependencies only.** This plan adds **no** dependencies.
- **Cross-platform: Linux + macOS first-class.** The worker is built/run only on Linux + macOS (both unix); the scratch dir abstraction (`scratch_dir_from_env`) already hides the per-OS difference (Linux in-jail `/tmp` tmpfs, macOS host-created dir). No OS-specific branch is added in this feature.
- **Rust core, Python only inside sandboxed workers.** No PyO3; the only "Python" is the agent-authored snippet the worker already runs.
- **Every worker stays sandboxed; no unsandboxed escape hatch.** Unchanged — the file is written *inside* the existing jail's writable scratch.
- **Keep code files under 500 LOC.** `workers/python-exec/src/exec.rs` is ~360 LOC incl. tests today; additions are small and the decision logic is pure. Re-check with `wc -l` after Task 2; if it crosses, lift the `#[cfg(test)] mod tests` to a sibling `exec/tests.rs` (out of scope unless it actually crosses).
- **TDD, frequent commits, all tests green before commit.** Source cargo env first in every shell: `source "$HOME/.cargo/env"`.
- **Build/test commands** (run from repo root `/Users/hherb/src/kastellan`):
  - `cargo test -p kastellan-worker-python-exec` — worker unit tests (Tasks 1–2).
  - `cargo test -p kastellan-core --lib` — core unit tests incl. l3py + manifest (Tasks 3–4).
  - `cargo build --workspace` — needed before the e2e so the worker binary + shim are fresh (Task 5).
  - `cargo clippy --workspace --all-targets -D warnings` — must stay clean (final task).

---

### Task 1: Worker pure channel-decision building blocks (additive)

Add the pure logic for choosing inline-vs-file and resolving the configurable ceiling, **without yet rewiring** `serialize_params`/`run_code`/the handler. This task is purely additive plus a mechanical rename, so every existing test stays green.

**Files:**
- Modify: `workers/python-exec/src/exec.rs` (constants ~13–50, tests ~237+)
- Modify: `workers/python-exec/src/handler.rs:143` (rename reference only)

**Interfaces:**
- Produces (consumed by Task 2):
  - `pub const INLINE_PARAMS_MAX: usize` (= `64 * 1024`) — the execve-safe inline threshold (renamed from `MAX_PARAMS_BYTES`).
  - `pub const PARAMS_FILE_MAX_DEFAULT: usize` (= `1024 * 1024`), `pub const PARAMS_FILE_MAX_ABS: usize` (= `16 * 1024 * 1024`).
  - `pub const PARAMS_FILE_MAX_ENV: &str` (= `"KASTELLAN_PYTHON_PARAMS_FILE_MAX"`), `pub const PARAMS_FILE_ENV: &str` (= `"KASTELLAN_PYTHON_PARAMS_FILE"`), `pub const PARAMS_FILE_NAME: &str` (= `"params.json"`).
  - `pub enum ParamChannel { Inline, File }` (derives `Debug, PartialEq, Eq`).
  - `pub fn params_file_max(lookup: impl Fn(&str) -> Option<String>) -> usize`.
  - `pub fn decide_param_channel(serialized_len: usize, inline_max: usize, file_max: usize) -> Result<ParamChannel, ParamsError>`.
  - `pub fn params_env_pairs(channel: &ParamChannel, params_json: &str, file_path: &str) -> Vec<(&'static str, String)>`.

- [ ] **Step 1: Rename the inline-threshold constant**

In `workers/python-exec/src/exec.rs`, rename the existing constant (currently lines 46–50) and update its doc-comment:

```rust
/// The execve-safe **inline** threshold. Params serializing to ≤ this many
/// bytes ride the `KASTELLAN_PYTHON_PARAMS` env var; larger payloads are
/// written to `<scratch>/params.json` (the file channel — see
/// [`decide_param_channel`]). Sits under the Linux `MAX_ARG_STRLEN` (128 KiB)
/// per-env-string `execve` wall with headroom. The host gate mirrors the file
/// ceiling, not this threshold (the worker owns the inline-vs-file split).
pub const INLINE_PARAMS_MAX: usize = 64 * 1024;
```

Update the two existing references to the old name so the crate still compiles:
- `workers/python-exec/src/exec.rs` test `serialize_params_rejects_over_cap` (the `"x".repeat(MAX_PARAMS_BYTES)` line): change `MAX_PARAMS_BYTES` → `INLINE_PARAMS_MAX`. Also update the cap check inside `serialize_params` (the `if s.len() > MAX_PARAMS_BYTES` and the `ParamsError::TooLarge { ... max: MAX_PARAMS_BYTES }` lines) → `INLINE_PARAMS_MAX`, and the `ParamsError` doc-comment reference. (Behavior unchanged this task — still caps at 64 KiB; Task 2 removes the cap.)
- `workers/python-exec/src/handler.rs:143` (`"x".repeat(crate::exec::MAX_PARAMS_BYTES)`): change `MAX_PARAMS_BYTES` → `INLINE_PARAMS_MAX`.

- [ ] **Step 2: Run the crate tests to confirm the rename is green**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec`
Expected: PASS (mechanical rename, no behavior change).

- [ ] **Step 3: Write the failing tests for the new pure functions**

Append to the `#[cfg(test)] mod tests` block in `workers/python-exec/src/exec.rs`:

```rust
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
```

- [ ] **Step 4: Run the new tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec params_file_max params_env_pairs decide_`
Expected: FAIL to compile ("cannot find function `params_file_max`" etc.).

- [ ] **Step 5: Add the new constants, enum, and pure functions**

In `workers/python-exec/src/exec.rs`, add near the other params constants (after `INLINE_PARAMS_MAX`):

```rust
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
```

- [ ] **Step 6: Run the worker tests to verify all pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec`
Expected: PASS (all existing + the 9 new pure tests).

- [ ] **Step 7: Commit**

```bash
git add workers/python-exec/src/exec.rs workers/python-exec/src/handler.rs
git commit -m "feat(python-exec): pure channel-decision building blocks for file params

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Worker file-channel delivery + handler wiring

Switch the worker to actually use the file channel: drop the size cap from `serialize_params` (the cap now lives in `decide_param_channel`), write the file in `run_code`, and rewire the handler to compute the channel and pass it through. Document the author idiom.

**Files:**
- Modify: `workers/python-exec/src/exec.rs` (`serialize_params` ~83–99, `run_code` ~182–235, `PARAMS_FILE_ENV` doc, tests)
- Modify: `workers/python-exec/src/handler.rs` (`call` ~66–70, tests ~141–152)

**Interfaces:**
- Consumes (from Task 1): `INLINE_PARAMS_MAX`, `params_file_max`, `decide_param_channel`, `ParamChannel`, `params_env_pairs`, `PARAMS_FILE_NAME`.
- Produces:
  - `pub fn run_code(python: &Path, code: &str, params_json: &str, channel: ParamChannel) -> std::io::Result<ExecOutcome>` (new 4th arg).
  - `fn write_params_file(path: &Path, json: &str) -> std::io::Result<()>` (module-private I/O helper, 0600).
  - `serialize_params` now returns the serialized JSON with **only** the `NotObject` check (no size cap).

- [ ] **Step 1: Write the failing test for `write_params_file`**

Append to the `#[cfg(test)] mod tests` block in `workers/python-exec/src/exec.rs`:

```rust
    #[test]
    fn write_params_file_writes_exact_content_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "pyexec-params-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PARAMS_FILE_NAME);
        write_params_file(&path, r#"{"blob":"xyz"}"#).unwrap();
        let back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(back, r#"{"blob":"xyz"}"#);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "params file must be private (0600)");
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec write_params_file`
Expected: FAIL to compile ("cannot find function `write_params_file`").

- [ ] **Step 3: Implement `write_params_file` and slim `serialize_params`**

In `workers/python-exec/src/exec.rs`, replace the body of `serialize_params` (keep the signature) so it no longer caps — only the `NotObject` check remains:

```rust
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
```

Add the private writer near `run_code`:

```rust
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
```

- [ ] **Step 4: Run the writer test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec write_params_file`
Expected: PASS.

- [ ] **Step 5: Remove the now-stale `serialize_params_rejects_over_cap` test**

In `workers/python-exec/src/exec.rs`, delete the `serialize_params_rejects_over_cap` test (size rejection now lives in `decide_too_large_over_ceiling` from Task 1). Keep `serialize_params_object_round_trips`, `..._none_is_empty_object`, `..._rejects_non_object`, `..._allows_newlines_in_values`, `..._escapes_nul_no_raw_nul_for_execve`.

- [ ] **Step 6: Rewire `run_code` to take a channel and write the file**

In `workers/python-exec/src/exec.rs`, change `run_code`'s signature and the env-setting block. Replace the signature line and the command-build prefix (currently lines ~182–195):

```rust
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
```

(The rest of `run_code` — the stdin feeder + capped capture + outcome build — is unchanged.)

- [ ] **Step 7: Document the author idiom on `PARAMS_FILE_ENV`**

In `workers/python-exec/src/exec.rs`, expand the `PARAMS_FILE_ENV` doc-comment added in Task 1 to carry the canonical author idiom:

```rust
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
```

- [ ] **Step 8: Rewire the handler and update its over-cap test**

In `workers/python-exec/src/handler.rs`, replace the `serialize_params` + `run_code` block in `call` (currently lines ~66–70) with:

```rust
        let params_json = serialize_params(&p.params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, e.to_string()))?;
        let file_max = exec::params_file_max(|k| std::env::var(k).ok());
        let channel = exec::decide_param_channel(params_json.len(), exec::INLINE_PARAMS_MAX, file_max)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, e.to_string()))?;

        let outcome = run_code(&self.python, &p.code, &params_json, channel)
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("spawn failed: {e}")))?;
```

Update the `use` at the top of `handler.rs` (currently `use crate::exec::{run_code, serialize_params, MAX_CODE_BYTES};`) to:

```rust
use crate::exec::{self, run_code, serialize_params, MAX_CODE_BYTES};
```

Update the `over_cap_params_is_invalid_params` test so it exceeds the **file** ceiling (default 1 MiB), since a 64 KiB param is now Inline, not a reject:

```rust
    #[test]
    fn over_file_cap_params_is_invalid_params() {
        // A param larger than the default 1 MiB file ceiling is rejected
        // fail-closed (INVALID_PARAMS) — proves the file channel still has a
        // hard ceiling. (Env unset → default ceiling.)
        let big = "x".repeat(crate::exec::PARAMS_FILE_MAX_DEFAULT + 1024);
        let err = handler()
            .call(
                "python.exec",
                serde_json::json!({"code": "print(1)", "params": {"k": big}}),
            )
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("cap"), "got: {}", err.message);
    }
```

- [ ] **Step 9: Run the full worker test suite**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec`
Expected: PASS (all unit tests, including the rewired handler + new write/file tests).

- [ ] **Step 10: Confirm the file stays under the LOC cap**

Run: `wc -l workers/python-exec/src/exec.rs`
Expected: under 500 (≈ 410). If it crossed 500, lift `#[cfg(test)] mod tests` into a sibling `workers/python-exec/src/exec/tests.rs` (`mod exec { ... }` → `#[path]` not needed; use `#[cfg(test)] mod tests;` + move the block) and re-run Step 9. Otherwise skip.

- [ ] **Step 11: Commit**

```bash
git add workers/python-exec/src/exec.rs workers/python-exec/src/handler.rs
git commit -m "feat(python-exec): worker writes >64 KiB params to scratch file channel

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Host gate — parameterize the cap as a structural backstop

Make the host-side validator take an explicit cap and call it with a fixed 16 MiB hard-max from the two pure call sites, keeping them env-free. The operator-configurable ceiling is enforced worker-side (Task 4); the host only rejects the structurally-impossible.

**Files:**
- Modify: `core/src/memory/l3py_invoke/pure.rs` (`MAX_PARAMS_BYTES` ~29–33, `validate_python_params` ~62–83, tests ~272)
- Modify: `core/src/memory/l3py_invoke/agent.rs:58`
- Modify: `core/src/memory/l3py_invoke/operator.rs:43`

**Interfaces:**
- Produces:
  - `pub const HOST_PARAMS_HARD_MAX: usize` (= `16 * 1024 * 1024`) in `pure.rs`.
  - `pub fn validate_python_params(params: &Value, max_bytes: usize) -> Result<Value, PyParamError>` (new 2nd arg).
- Consumers: `expand_python_for_agent` (agent.rs), `prepare_python_steps` (operator.rs) — both pass `HOST_PARAMS_HARD_MAX`.

- [ ] **Step 1: Write the failing test for the parameterized cap**

In `core/src/memory/l3py_invoke/pure.rs`, replace the existing over-cap test (the one using `MAX_PARAMS_BYTES` ~line 272) with two explicit-cap tests:

```rust
    #[test]
    fn validate_python_params_rejects_over_the_passed_cap() {
        let big = "x".repeat(2048);
        let v = serde_json::json!({ "k": big });
        // A tiny explicit cap rejects; deterministic, no env.
        assert!(validate_python_params(&v, 1024).is_err());
    }

    #[test]
    fn validate_python_params_accepts_up_to_the_passed_cap() {
        let v = serde_json::json!({ "k": "x".repeat(100) });
        assert!(validate_python_params(&v, HOST_PARAMS_HARD_MAX).is_ok());
    }
```

Also update every other `validate_python_params(&v)` call in this test module (the valid/invalid/snake_case/null cases around lines 246–303) to pass a cap, e.g. `validate_python_params(&v, HOST_PARAMS_HARD_MAX)`.

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3py_invoke::pure`
Expected: FAIL to compile (arity mismatch — `validate_python_params` takes 1 arg, `HOST_PARAMS_HARD_MAX` undefined).

- [ ] **Step 3: Add the constant and parameterize the validator**

In `core/src/memory/l3py_invoke/pure.rs`, replace the `MAX_PARAMS_BYTES` constant (lines ~29–33) with:

```rust
/// Structural backstop the **host** gate enforces: a payload above this is
/// rejected early for a clean refusal. This is NOT the operator knob — the
/// configurable ceiling (`KASTELLAN_PYTHON_PARAMS_FILE_MAX`, default 1 MiB) is
/// enforced WORKER-side (the real boundary; see
/// `workers/python-exec/src/exec.rs::params_file_max`). Equal to the worker's
/// absolute clamp ceiling so the host never refuses something the worker would
/// have accepted.
pub const HOST_PARAMS_HARD_MAX: usize = 16 * 1024 * 1024;
```

Change `validate_python_params` (lines ~62–83) to take the cap and use it (snake_case + null passthrough unchanged):

```rust
pub fn validate_python_params(params: &Value, max_bytes: usize) -> Result<Value, PyParamError> {
    if params.is_null() {
        return Ok(params.clone());
    }
    let obj = params.as_object().ok_or(PyParamError::NotObject)?;
    for key in obj.keys() {
        if !is_snake_ident(key) {
            return Err(PyParamError::BadKey(key.clone()));
        }
    }
    let serialized = serde_json::to_string(params).unwrap_or_default();
    if serialized.len() > max_bytes {
        return Err(PyParamError::TooLarge { got: serialized.len(), max: max_bytes });
    }
    Ok(params.clone())
}
```

Update the doc-comment above it: change "serialized ≤ [`MAX_PARAMS_BYTES`]" → "serialized ≤ `max_bytes`" and the line-30 cross-reference comment that mentions the worker's `MAX_PARAMS_BYTES` → point at the worker's `params_file_max` / `INLINE_PARAMS_MAX` instead.

- [ ] **Step 4: Update the two callers**

In `core/src/memory/l3py_invoke/agent.rs:58`, change:

```rust
    let params = validate_python_params(params, super::pure::HOST_PARAMS_HARD_MAX)
        .map_err(|e| InvokeRefusal { reasons: vec![e.to_string()] })?;
```

In `core/src/memory/l3py_invoke/operator.rs:43`, change:

```rust
    let params = validate_python_params(params, super::pure::HOST_PARAMS_HARD_MAX)
        .map_err(|e| InvokeRefusal { reasons: vec![e.to_string()] })?;
```

(Both modules already `use super::pure::{... validate_python_params ...}`; reference `HOST_PARAMS_HARD_MAX` via the `super::pure::` path shown, or add it to the existing `use` — either is fine as long as it compiles.)

- [ ] **Step 5: Run the l3py unit tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3py_invoke`
Expected: PASS (pure gate + agent + operator suites; existing small-param tests still green, new cap tests pass).

- [ ] **Step 6: Commit**

```bash
git add core/src/memory/l3py_invoke/pure.rs core/src/memory/l3py_invoke/agent.rs core/src/memory/l3py_invoke/operator.rs
git commit -m "feat(python-exec): host param gate takes an explicit cap (16 MiB backstop)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Manifest — inject the operator's file-max into the jail

Forward the operator's `KASTELLAN_PYTHON_PARAMS_FILE_MAX` (when set) into the worker's `policy.env`, so the worker's `params_file_max` reads the configured ceiling. Unset → omitted → worker default (byte-identical env).

**Files:**
- Modify: `core/src/workers/python_exec.rs` (`python_exec_entry` ~126–167, `resolve` ~294, env-name consts ~34–44)
- Modify: `core/src/workers/python_exec.rs` tests module (`core/src/workers/python_exec/tests.rs`)
- Modify: `core/tests/python_exec_e2e.rs:139` (call-site arity)

**Interfaces:**
- Produces: `python_exec_entry(binary, python, interpreter_lib_dirs, params_file_max: Option<String>) -> ToolEntry` (new 4th arg). When `Some(non-empty)`, pushes `(PARAMS_FILE_MAX_ENV, value)` into `policy.env`; when `None`/empty, no change.
- Consumers: `PythonExecManifest::resolve` (passes `ctx.get_env(PARAMS_FILE_MAX_ENV)`), `python_exec_e2e::dispatch_in_jail` (passes `None`).

- [ ] **Step 1: Write the failing manifest test**

In `core/src/workers/python_exec/tests.rs`, add (adjust the `use`/helper to match the file's existing test style for building an entry — the entry builder is `python_exec_entry`):

```rust
    #[test]
    fn entry_injects_params_file_max_when_set() {
        let entry = super::python_exec_entry(
            std::path::PathBuf::from("/bin/worker"),
            std::path::PathBuf::from("/usr/bin/python3"),
            vec![],
            Some("250000".to_string()),
        );
        let got = entry
            .policy
            .env
            .iter()
            .find(|(k, _)| k == "KASTELLAN_PYTHON_PARAMS_FILE_MAX")
            .map(|(_, v)| v.as_str());
        assert_eq!(got, Some("250000"));
    }

    #[test]
    fn entry_omits_params_file_max_when_unset() {
        let entry = super::python_exec_entry(
            std::path::PathBuf::from("/bin/worker"),
            std::path::PathBuf::from("/usr/bin/python3"),
            vec![],
            None,
        );
        assert!(
            !entry
                .policy
                .env
                .iter()
                .any(|(k, _)| k == "KASTELLAN_PYTHON_PARAMS_FILE_MAX"),
            "unset → env must stay byte-identical (no file-max key)"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::python_exec`
Expected: FAIL to compile (`python_exec_entry` takes 3 args).

- [ ] **Step 3: Add the env-name constant and the 4th parameter**

In `core/src/workers/python_exec.rs`, add a constant near the other env-name consts (~line 44):

```rust
/// Operator-config ceiling for the >64 KiB params file channel, forwarded into
/// the jail when set. Worker-side default + clamp live in
/// `workers/python-exec/src/exec.rs::params_file_max`; keep the name in sync.
const PARAMS_FILE_MAX_ENV: &str = "KASTELLAN_PYTHON_PARAMS_FILE_MAX";
```

Change `python_exec_entry` to take and apply the value:

```rust
pub fn python_exec_entry(
    binary: PathBuf,
    python: PathBuf,
    interpreter_lib_dirs: Vec<PathBuf>,
    params_file_max: Option<String>,
) -> ToolEntry {
    let mut fs_read = vec![binary.clone(), python.clone()];
    if let Some(extra) = interpreter_extra_fs_read(&python) {
        fs_read.push(extra);
    }
    fs_read.extend(interpreter_lib_dirs);
    let mut env = vec![
        (PYTHON_ENV.to_string(), python.to_string_lossy().into_owned()),
        (ENV_LANDLOCK_RW.to_string(), r#"["/tmp"]"#.to_string()),
    ];
    // Forward the operator's file-channel ceiling into the jail ONLY when set,
    // so an unset config leaves the worker env byte-identical (worker default
    // 1 MiB). Blank values are treated as unset.
    if let Some(v) = params_file_max.filter(|v| !v.trim().is_empty()) {
        env.push((PARAMS_FILE_MAX_ENV.to_string(), v));
    }
    let policy = SandboxPolicy {
        fs_read,
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerStrict,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: true,
    }
}
```

- [ ] **Step 4: Pass the operator env through in `resolve`**

In `core/src/workers/python_exec.rs`, update the `Resolution::Register(python_exec_entry(...))` call (~line 294) to read and forward the env:

```rust
        let params_file_max = (ctx.get_env)(PARAMS_FILE_MAX_ENV);
        Resolution::Register(python_exec_entry(
            binary,
            python,
            interpreter_lib_dirs,
            params_file_max,
        ))
```

- [ ] **Step 5: Fix the e2e call site**

In `core/tests/python_exec_e2e.rs` (~line 139), add the `None` 4th argument:

```rust
    let entry = python_exec_entry(
        env.worker_path.clone(),
        env.python.clone(),
        interpreter_lib_dirs,
        None,
    );
```

- [ ] **Step 6: Run the manifest unit tests + confirm the workspace compiles**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::python_exec`
Expected: PASS (new injection tests + existing manifest tests).

Run: `source "$HOME/.cargo/env" && cargo build --workspace --tests`
Expected: compiles (proves the e2e call site is fixed).

- [ ] **Step 7: Commit**

```bash
git add core/src/workers/python_exec.rs core/src/workers/python_exec/tests.rs core/tests/python_exec_e2e.rs
git commit -m "feat(python-exec): manifest forwards KASTELLAN_PYTHON_PARAMS_FILE_MAX into the jail

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: End-to-end — >64 KiB param round-trips through the real worker + jail

Prove the whole chain: a param larger than the inline threshold is delivered via the file channel and the agent reads the full payload, under the production policy in the real Seatbelt (macOS) / bwrap (DGX) jail.

**Files:**
- Modify: `core/tests/python_exec_e2e.rs` (new test next to `scratch_tmp_write_round_trip_inside_jail` ~line 231)

**Interfaces:**
- Consumes: the existing harness `dispatch_in_jail(pool, env, vault, params)`, `ready_or_skip()`, `dispatch_runtime()`, `probe_and_pool`, `TestEnv` (already in the file).

- [ ] **Step 1: Write the failing e2e test**

In `core/tests/python_exec_e2e.rs`, add after `scratch_tmp_write_round_trip_inside_jail`:

```rust
/// A params payload larger than the inline env threshold (64 KiB) must reach
/// the agent through the **file channel**: the worker writes
/// `<scratch>/params.json`, sets `KASTELLAN_PYTHON_PARAMS_FILE`, and the agent
/// reads the full value. If the file channel failed, `KASTELLAN_PYTHON_PARAMS`
/// would be the `"{}"` default → `KeyError` → non-zero exit, so a zero exit
/// with the correct length proves end-to-end delivery. Real worker, real jail,
/// production policy — runs on macOS (Seatbelt) + DGX (bwrap).
#[test]
fn large_param_round_trips_via_file_channel() {
    let Some(env) = ready_or_skip() else { return };
    let conn = env.conn.clone();
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&conn).await;
        // 100_000 bytes ≫ the 64 KiB inline threshold, ≪ the 1 MiB default
        // file ceiling → the File channel.
        let blob = "A".repeat(100_000);
        let code = concat!(
            "import json, os\n",
            "p = os.environ.get('KASTELLAN_PYTHON_PARAMS_FILE')\n",
            "if p:\n",
            "    with open(p) as f:\n",
            "        params = json.load(f)\n",
            "else:\n",
            "    params = json.loads(os.environ.get('KASTELLAN_PYTHON_PARAMS', '{}'))\n",
            "b = params['blob']\n",
            "print(len(b), b[:4], b[-4:])\n",
        );
        let params = serde_json::json!({ "code": code, "params": { "blob": blob } });
        let r = dispatch_in_jail(&pool, &env, &kastellan_core::secrets::Vault::new(), params)
            .await
            .expect("python.exec dispatch must succeed");
        assert_eq!(r["exit_code"].as_i64(), Some(0), "stderr: {}", r["stderr"]);
        assert_eq!(
            r["stdout"].as_str().unwrap().trim_end(),
            "100000 AAAA AAAA",
            "agent must read the full 100 KiB payload via the file channel"
        );
    });
}
```

- [ ] **Step 2: Build the workspace so the worker binary is fresh**

Run: `source "$HOME/.cargo/env" && cargo build --workspace`
Expected: builds (the e2e spawns the just-built `kastellan-worker-python-exec`).

- [ ] **Step 3: Run the e2e (macOS, live PG + real Seatbelt jail)**

Run: `source "$HOME/.cargo/env" && KASTELLAN_PYTHON_EXEC_ENABLE=1 KASTELLAN_PG_BIN_DIR='/Applications/Postgres 2.app/Contents/Versions/18/bin' cargo test -p kastellan-core --test python_exec_e2e large_param_round_trips_via_file_channel -- --nocapture`
Expected: PASS (not `[SKIP]`). If it prints a `[SKIP]` line, the env gate / PG isn't satisfied — set `KASTELLAN_PYTHON_EXEC_ENABLE=1` and the PG bin dir per the memory note, then re-run. (Per the standing macOS gotcha, run this suite individually, not in a full-workspace parallel run.)

- [ ] **Step 4: Run the full python_exec_e2e suite to confirm no regression**

Run: `source "$HOME/.cargo/env" && KASTELLAN_PYTHON_EXEC_ENABLE=1 KASTELLAN_PG_BIN_DIR='/Applications/Postgres 2.app/Contents/Versions/18/bin' cargo test -p kastellan-core --test python_exec_e2e -- --nocapture`
Expected: PASS — `print_round_trip`, `socket_attempt_is_contained`, `scratch_tmp_write_round_trip`, `materialized_secret_param_is_scrubbed_from_output`, and the new `large_param_round_trips_via_file_channel`.

- [ ] **Step 5: Commit**

```bash
git add core/tests/python_exec_e2e.rs
git commit -m "test(python-exec): e2e >64 KiB param round-trip via the file channel

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Workspace verification + clippy

Confirm the whole workspace is green and lint-clean before handing back.

**Files:** none (verification only).

- [ ] **Step 1: Worker + core unit suites**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec && cargo test -p kastellan-core --lib`
Expected: PASS.

- [ ] **Step 2: Clippy across the workspace**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Full workspace test (macOS skip-as-pass policy)**

Run: `source "$HOME/.cargo/env" && cargo test --workspace`
Expected: green (live-PG suites skip-as-pass on a full parallel run per the standing gotcha; the targeted e2e in Task 5 is the real gate for this feature).

- [ ] **Step 4: DGX native-Linux confirmation of the e2e (Linux path)**

The file-write path is cross-platform but only macOS was exercised above. Confirm the Linux jail on the DGX (real bwrap + Landlock + seccomp + live PG). From the Mac:

```bash
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && git fetch && git checkout feat/python-exec-scratch-file-params && git pull && cargo build --workspace && KASTELLAN_PYTHON_EXEC_ENABLE=1 cargo test -p kastellan-core --test python_exec_e2e large_param_round_trips_via_file_channel -- --nocapture'
```

Expected: PASS (not `[SKIP]`). This proves the worker writes/reads `params.json` in the bwrap `/tmp` tmpfs identically. (The branch must be pushed/relayed to the DGX first — see the Mac→github push memory note if a direct push from the Mac times out.)

---

## Notes for the implementer

- **Don't widen the dispatch path.** Params already flow through `tool_host::dispatch` → `worker.call` over stdio; nothing in `core/src/tool_host.rs` changes. Secret substitution stays where it is (host-side, before the worker), so the file holds the same materialized params the env var would — the output secret-scrub is unaffected.
- **Fail closed.** Every new error arm (over-ceiling, scratch-write failure) returns an error, never a silent fallback to the inline env (which would breach the execve wall).
- **Keep the two ceilings straight.** `INLINE_PARAMS_MAX` (64 KiB, fixed) = env-vs-file split. `params_file_max` (env-configured, default 1 MiB, clamp [64 KiB, 16 MiB]) = worker reject ceiling. `HOST_PARAMS_HARD_MAX` (16 MiB, fixed) = host structural backstop.
- **`git add` specific files only** (per the repo convention) — never `git add -A`; the untracked `assets/agent_with_the_keys.png` and any lock files must stay out.
