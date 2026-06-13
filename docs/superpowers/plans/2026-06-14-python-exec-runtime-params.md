# python-exec runtime params — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give an approved/pinned Python skill runtime params — named JSON values supplied at invocation and delivered to the skill's verbatim, SHA-bound code through a side channel — without weakening the SHA binding or the `Net::Deny` containment ceiling.

**Architecture:** Params travel as a single JSON object in the env var `KASTELLAN_PYTHON_PARAMS` (the worker always sets it, defaulting to `{}`). The worker accepts an optional `params` field on `python.exec`; the core threads a validated params object through `l3py_invoke` (operator + agent paths), the daemon `l3_run` python branch, the inner-loop `invoke_skill` arm, and the CLI. Secret refs in params materialise for free through the existing `tool_host::dispatch` → `substitute_refs_in_params` recursive walker. Two slices: **A** (worker accepts params) lands first; **B** (core threading + CLI + e2e) depends on A.

**Tech Stack:** Rust (workspace), serde_json, sqlx/Postgres, the in-tree sandbox (Linux bwrap / macOS Seatbelt), CPython child over stdin.

**Spec:** `docs/superpowers/specs/2026-06-14-python-exec-runtime-params-design.md`

**Conventions for every task:** run `source "$HOME/.cargo/env"` first in any shell that runs cargo. Stage only the exact files listed (never `git add -A` — untracked `assets/*.png` and `.gitignore` must stay out). End commit messages with the `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` trailer.

---

## File structure

**Slice A — worker (`workers/python-exec`)**
- Modify `workers/python-exec/src/exec.rs` — add `PARAMS_ENV` + `MAX_PARAMS_BYTES` consts, pure `serialize_params`, and a `params_json` argument on `run_code`.
- Modify `workers/python-exec/src/handler.rs` — `ExecParams.params` field, validate + serialize, pass to `run_code`.
- Modify `workers/python-exec/tests/real_python.rs` — real-interpreter param round-trip + empty default + oversize.

**Slice B — core (`core`)**
- Modify `core/src/memory/l3py_invoke/pure.rs` — `MAX_PARAMS_BYTES`, `PyParamError`, `validate_python_params`, `params_is_empty`, `python_exec_step(code, params)`.
- Modify `core/src/memory/l3py_invoke/operator.rs` — params on `prepare_python_steps` + `invoke_python_skill`.
- Modify `core/src/memory/l3py_invoke/agent.rs` — params on `expand_python_for_agent`.
- Modify `core/src/cassandra/types.rs` — `InvokeDirective.params` field.
- Modify `core/src/cassandra/types/tests.rs` — update `InvokeDirective` struct literals.
- Modify `core/src/scheduler/inner_loop.rs` — thread directive params into the python `expand_python_for_agent` arm.
- Modify `core/src/scheduler/l3_run.rs` — `L3RunRequest.params`, parse, forward to the python branch.
- Modify `core/src/bin/kastellan-cli/memory_l3/run.rs` — `--param`/`--params-json` flags + `build_params` merge + payload `params`.
- Modify `core/tests/cli_memory_l3py_run_daemon_e2e.rs` — param round-trip + secret param + oversize.

---

# SLICE A — worker accepts `params`

### Task A1: pure params serialization in the worker

**Files:**
- Modify: `workers/python-exec/src/exec.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `workers/python-exec/src/exec.rs`:

```rust
    #[test]
    fn serialize_params_none_is_empty_object() {
        assert_eq!(serialize_params(&None).unwrap(), "{}");
    }

    #[test]
    fn serialize_params_object_round_trips() {
        let v = serde_json::json!({"a": 1, "b": "x"});
        let s = serialize_params(&Some(v)).unwrap();
        // serde_json sorts object keys deterministically only with a feature;
        // assert by re-parsing rather than string equality.
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
        // One key with a value just past the cap.
        let big = "x".repeat(MAX_PARAMS_BYTES);
        let v = serde_json::json!({ "k": big });
        assert!(matches!(
            serialize_params(&Some(v)),
            Err(ParamsError::TooLarge { .. })
        ));
    }

    #[test]
    fn serialize_params_allows_newlines_in_values() {
        // serde escapes control chars, so multi-line text passes (long-text use case).
        let v = serde_json::json!({ "text": "line1\nline2" });
        let s = serialize_params(&Some(v)).unwrap();
        assert!(!s.contains('\n'), "raw newline must be escaped inside JSON");
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["text"], "line1\nline2");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec serialize_params`
Expected: FAIL to compile — `serialize_params` / `ParamsError` / `MAX_PARAMS_BYTES` not defined.

- [ ] **Step 3: Write the implementation**

Add near the top of `workers/python-exec/src/exec.rs` (after the existing consts, before `python_args`):

```rust
use serde_json::Value;

/// Env var carrying the runtime params JSON object to the skill. The worker
/// ALWAYS sets it (default `{}`) so the author's
/// `json.loads(os.environ["KASTELLAN_PYTHON_PARAMS"])` never KeyErrors on the
/// lookup. Survives `-I` (which drops only `PYTHON*` names).
pub const PARAMS_ENV: &str = "KASTELLAN_PYTHON_PARAMS";

/// Byte cap on the serialized params object. Sits under the Linux
/// `MAX_ARG_STRLEN` (128 KiB) per-env-string `execve` wall with headroom;
/// the host-side `core` enforces the same cap early (keep the two in sync —
/// see `core/src/memory/l3py_invoke/pure.rs::MAX_PARAMS_BYTES`).
pub const MAX_PARAMS_BYTES: usize = 64 * 1024;

/// Why a params payload was rejected. The handler maps both arms to
/// JSON-RPC `INVALID_PARAMS`.
#[derive(Debug)]
pub enum ParamsError {
    /// Present but not a JSON object (array / scalar / null).
    NotObject,
    /// Serialized object exceeds [`MAX_PARAMS_BYTES`].
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

/// Serialize the optional params object to the env-var string.
///
/// * `None` ⇒ `"{}"` (the stable empty-default contract).
/// * `Some(obj)` where `obj` is a JSON object ⇒ its compact serialization,
///   rejected if it exceeds [`MAX_PARAMS_BYTES`].
/// * `Some(non-object)` ⇒ [`ParamsError::NotObject`].
///
/// Pure (no I/O) so it is unit-testable without an interpreter. The worker is
/// the AUTHORITATIVE enforcer of these checks — a direct or malformed call must
/// never reach `execve` with an oversize/garbage env var.
pub fn serialize_params(params: &Option<Value>) -> Result<String, ParamsError> {
    match params {
        None => Ok("{}".to_string()),
        Some(Value::Object(_)) => {
            // Safe: a `Value` always serializes.
            let s = serde_json::to_string(params.as_ref().unwrap()).unwrap_or_default();
            if s.len() > MAX_PARAMS_BYTES {
                return Err(ParamsError::TooLarge { got: s.len(), max: MAX_PARAMS_BYTES });
            }
            Ok(s)
        }
        Some(_) => Err(ParamsError::NotObject),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec serialize_params`
Expected: PASS (5 new tests).

- [ ] **Step 5: Commit**

```bash
git add workers/python-exec/src/exec.rs
git commit -m "feat(python-exec): pure serialize_params + 64 KiB cap for runtime params

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task A2: `run_code` sets the params env var

**Files:**
- Modify: `workers/python-exec/src/exec.rs`
- Modify: `workers/python-exec/src/handler.rs` (the sole caller — keep it compiling)

- [ ] **Step 1: Change `run_code`'s signature**

In `workers/python-exec/src/exec.rs`, change the `run_code` signature and the `.env` block. Replace:

```rust
pub fn run_code(python: &Path, code: &str) -> std::io::Result<ExecOutcome> {
    let mut cmd = Command::new(python);
    cmd.args(python_args())
        .env_clear()
        .env("TMPDIR", SCRATCH_DIR)
        .env("HOME", SCRATCH_DIR)
        .stdin(Stdio::piped())
```

with:

```rust
pub fn run_code(python: &Path, code: &str, params_json: &str) -> std::io::Result<ExecOutcome> {
    let mut cmd = Command::new(python);
    cmd.args(python_args())
        .env_clear()
        .env("TMPDIR", SCRATCH_DIR)
        .env("HOME", SCRATCH_DIR)
        .env(PARAMS_ENV, params_json)
        .stdin(Stdio::piped())
```

Also update the doc comment above `run_code` to add a sentence: `Runtime params arrive as the JSON string \`params_json\` in the \`KASTELLAN_PYTHON_PARAMS\` env var (already validated + serialized by the caller).`

- [ ] **Step 2: Update the sole caller in `handler.rs`**

In `workers/python-exec/src/handler.rs`, the existing call (line ~64) is `run_code(&self.python, &p.code)`. Task A3 rewrites this method fully; for now, to keep the tree compiling between commits, change it to `run_code(&self.python, &p.code, "{}")`.

- [ ] **Step 3: Verify the crate builds and existing tests pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec`
Expected: PASS — all existing worker unit tests still green (the `unspawnable_interpreter_is_operation_failed` handler test still reaches `run_code` with `"{}"`).

- [ ] **Step 4: Commit**

```bash
git add workers/python-exec/src/exec.rs workers/python-exec/src/handler.rs
git commit -m "feat(python-exec): run_code injects KASTELLAN_PYTHON_PARAMS env var

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task A3: handler accepts + validates `params`

**Files:**
- Modify: `workers/python-exec/src/handler.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `handler.rs`:

```rust
    #[test]
    fn non_object_params_is_invalid_params() {
        let err = handler()
            .call("python.exec", serde_json::json!({"code": "print(1)", "params": [1, 2]}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("object"), "got: {}", err.message);
    }

    #[test]
    fn over_cap_params_is_invalid_params() {
        let big = "x".repeat(crate::exec::MAX_PARAMS_BYTES);
        let err = handler()
            .call(
                "python.exec",
                serde_json::json!({"code": "print(1)", "params": {"k": big}}),
            )
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("cap"), "got: {}", err.message);
    }

    #[test]
    fn absent_params_is_accepted_and_reaches_spawn() {
        // No `params` key: validation passes, so we fall through to the spawn,
        // which fails on the dummy interpreter → OPERATION_FAILED (not
        // INVALID_PARAMS). Proves absent params is the `{}` default, not a reject.
        let err = handler()
            .call("python.exec", serde_json::json!({"code": "print(1)"}))
            .unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec --lib params`
Expected: FAIL — `params` is not a field of `ExecParams`, so `non_object_params` / `over_cap_params` currently succeed-parse and reach the spawn (wrong code), and there is no validation.

- [ ] **Step 3: Implement**

In `handler.rs`, change the `ExecParams` struct:

```rust
#[derive(Deserialize)]
struct ExecParams {
    code: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}
```

Add `serialize_params` + `MAX_PARAMS_BYTES` to the import:

```rust
use crate::exec::{run_code, serialize_params, MAX_CODE_BYTES};
```

(`MAX_PARAMS_BYTES` is referenced fully-qualified in tests; no import needed for the impl.)

Replace the body after the `MAX_CODE_BYTES` check (the `let outcome = run_code(...)` line) with:

```rust
        let params_json = serialize_params(&p.params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, e.to_string()))?;

        let outcome = run_code(&self.python, &p.code, &params_json)
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("spawn failed: {e}")))?;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec`
Expected: PASS — all worker unit tests, including the 3 new param ones.

- [ ] **Step 5: Commit**

```bash
git add workers/python-exec/src/handler.rs
git commit -m "feat(python-exec): python.exec accepts + validates optional params

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task A4: real-interpreter param round-trip

**Files:**
- Modify: `workers/python-exec/tests/real_python.rs`

- [ ] **Step 1: Inspect the existing test harness**

Read `workers/python-exec/tests/real_python.rs` top-to-bottom (especially the helper around line 42 that builds `PythonExecHandler::with_python(python)` and the interpreter-discovery/skip helper). Reuse that exact harness — do not invent a new interpreter finder.

- [ ] **Step 2: Write the failing tests**

Add three tests modelled on the existing ones (use the file's established interpreter-resolve-or-skip helper; the snippet below assumes a `resolve_python_or_skip()`-style helper returning a `PathBuf` and a `call(handler, code, params)` pattern — adapt names to the file's actual helpers):

```rust
    #[test]
    fn params_round_trip_to_stdout() {
        let python = match resolve_python_or_skip() { Some(p) => p, None => return };
        let mut h = PythonExecHandler::with_python(python);
        let code = "import os, json\n\
                    p = json.loads(os.environ['KASTELLAN_PYTHON_PARAMS'])\n\
                    print(p['greeting'], p['n'])\n";
        let out = h
            .call("python.exec", serde_json::json!({"code": code, "params": {"greeting": "hi", "n": 7}}))
            .expect("dispatch ok");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "hi 7");
    }

    #[test]
    fn absent_params_defaults_to_empty_object() {
        let python = match resolve_python_or_skip() { Some(p) => p, None => return };
        let mut h = PythonExecHandler::with_python(python);
        let code = "import os, json\n\
                    p = json.loads(os.environ['KASTELLAN_PYTHON_PARAMS'])\n\
                    print(len(p))\n";
        let out = h.call("python.exec", serde_json::json!({"code": code})).expect("dispatch ok");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "0");
    }

    #[test]
    fn over_cap_params_rejected_before_spawn() {
        let python = match resolve_python_or_skip() { Some(p) => p, None => return };
        let mut h = PythonExecHandler::with_python(python);
        let big = "x".repeat(kastellan_worker_python_exec::exec::MAX_PARAMS_BYTES);
        let err = h
            .call("python.exec", serde_json::json!({"code": "print(1)", "params": {"k": big}}))
            .unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::INVALID_PARAMS);
    }
```

If the file's helper names differ (e.g. the existing tests inline the interpreter discovery), copy that exact pattern instead of `resolve_python_or_skip()`.

- [ ] **Step 3: Run tests to verify they pass (or skip cleanly)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec --test real_python -- --nocapture`
Expected: PASS on a host with a discoverable CPython (the param round-trip prints `hi 7`); a clean self-skip on a host without one — matching the existing tests' posture.

- [ ] **Step 4: Commit**

```bash
git add workers/python-exec/tests/real_python.rs
git commit -m "test(python-exec): real-interpreter params round-trip + empty-default + over-cap

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task A5: Slice A verification gate

- [ ] **Step 1: Build + test the whole worker crate**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec`
Expected: PASS (all unit + real_python).

- [ ] **Step 2: Clippy the crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-worker-python-exec --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Confirm `core` still builds against the new worker (no API break)**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core`
Expected: PASS (core does not yet call the new worker field; the worker change is additive).

> **Slice A is independently mergeable here.** If landing as a separate PR, open it now (`feat/python-exec-runtime-params` → `main`, title "feat(phase4): python-exec worker — accept runtime params (slice A)"), then continue Slice B on the same branch or a follow-up branch. If landing both slices together, proceed.

---

# SLICE B — core threads params end-to-end

### Task B1: `validate_python_params` + `params_is_empty` (pure)

**Files:**
- Modify: `core/src/memory/l3py_invoke/pure.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `pure.rs`:

```rust
    #[test]
    fn validate_params_accepts_object_with_snake_case_keys() {
        let v = serde_json::json!({"repo_path": "/tmp/x", "limit": 5, "tags": ["a", "b"]});
        let got = validate_python_params(&v).expect("valid");
        assert_eq!(got, v);
    }

    #[test]
    fn validate_params_rejects_non_object() {
        assert!(validate_python_params(&serde_json::json!([1, 2])).is_err());
        assert!(validate_python_params(&serde_json::json!("flat")).is_err());
    }

    #[test]
    fn validate_params_rejects_non_snake_case_top_level_key() {
        // Top-level keys are param NAMES; nested keys are opaque author data.
        let v = serde_json::json!({"BadKey": 1});
        assert!(validate_python_params(&v).is_err());
    }

    #[test]
    fn validate_params_allows_arbitrary_nested_keys() {
        // Nested object keys are data, NOT param names — no snake_case rule.
        let v = serde_json::json!({"payload": {"CamelCase": 1, "with space": 2}});
        assert!(validate_python_params(&v).is_ok());
    }

    #[test]
    fn validate_params_rejects_over_cap() {
        let big = "x".repeat(MAX_PARAMS_BYTES);
        let v = serde_json::json!({"k": big});
        assert!(validate_python_params(&v).is_err());
    }

    #[test]
    fn params_is_empty_is_true_for_null_and_empty_object() {
        assert!(params_is_empty(&serde_json::Value::Null));
        assert!(params_is_empty(&serde_json::json!({})));
        assert!(!params_is_empty(&serde_json::json!({"a": 1})));
    }

    #[test]
    fn step_omits_params_when_empty() {
        let step = python_exec_step("print(1)\n", &serde_json::json!({}));
        assert_eq!(step.parameters, serde_json::json!({"code": "print(1)\n"}));
    }

    #[test]
    fn step_carries_params_when_present() {
        let step = python_exec_step("print(1)\n", &serde_json::json!({"n": 3}));
        assert_eq!(
            step.parameters,
            serde_json::json!({"code": "print(1)\n", "params": {"n": 3}})
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3py_invoke::pure`
Expected: FAIL to compile — `validate_python_params` / `params_is_empty` / `MAX_PARAMS_BYTES` undefined, and `python_exec_step` takes one arg.

- [ ] **Step 3: Implement**

In `pure.rs`, add `use serde_json::Value;` if not present. Add the const + error + functions:

```rust
/// Byte cap on serialized runtime params. Keep in sync with the worker's
/// authoritative cap (`workers/python-exec/src/exec.rs::MAX_PARAMS_BYTES`);
/// core enforces it early for a clean refusal, the worker enforces it as the
/// real boundary.
pub const MAX_PARAMS_BYTES: usize = 64 * 1024;

/// Why a runtime params object was rejected at the core gate.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PyParamError {
    #[error("params must be a JSON object")]
    NotObject,
    #[error("params name '{0}' is not snake_case")]
    BadKey(String),
    #[error("params serialize to {got} bytes; cap is {max}")]
    TooLarge { got: usize, max: usize },
}

/// `true` iff `s` is a strict snake_case identifier (`[a-z][a-z0-9_]*`).
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// PURE gate for a runtime params object: must be a JSON object, every
/// TOP-LEVEL key snake_case (param names; nested structure is opaque author
/// data), serialized ≤ [`MAX_PARAMS_BYTES`]. Returns the validated object
/// unchanged. Unlike the templated arg guard there is NO newline/control-char
/// rejection — serde escapes control chars inside JSON strings, so long
/// multi-line text passes freely.
pub fn validate_python_params(params: &Value) -> Result<Value, PyParamError> {
    let obj = params.as_object().ok_or(PyParamError::NotObject)?;
    for key in obj.keys() {
        if !is_snake_ident(key) {
            return Err(PyParamError::BadKey(key.clone()));
        }
    }
    let serialized = serde_json::to_string(params).unwrap_or_default();
    if serialized.len() > MAX_PARAMS_BYTES {
        return Err(PyParamError::TooLarge { got: serialized.len(), max: MAX_PARAMS_BYTES });
    }
    Ok(params.clone())
}

/// `true` iff `params` carries no values (JSON null or an empty object) — used
/// to decide whether the `python.exec` step omits the `params` key entirely
/// (back-compat with param-less rows + their tests).
pub fn params_is_empty(params: &Value) -> bool {
    match params {
        Value::Null => true,
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}
```

Then change `python_exec_step` to take params:

```rust
/// Build the single `python.exec` step that runs `code` verbatim. When
/// `params` is non-empty it is added as a `params` key on the step's
/// `parameters` (where the dispatch chokepoint's recursive secret-ref walker
/// will materialise any `secret://` leaves); an empty params object is omitted
/// so a no-param call is byte-identical to the pre-params shape.
pub fn python_exec_step(code: &str, params: &Value) -> L3TemplateStep {
    let mut parameters = serde_json::json!({ "code": code });
    if !params_is_empty(params) {
        parameters
            .as_object_mut()
            .expect("parameters is an object")
            .insert("params".to_string(), params.clone());
    }
    L3TemplateStep {
        tool: PY_EXEC_TOOL.to_string(),
        method: PY_EXEC_METHOD.to_string(),
        parameters,
    }
}
```

Update the existing `builds_one_python_exec_step` test in `pure.rs` to pass empty params: change `python_exec_step("print(1)\n")` to `python_exec_step("print(1)\n", &serde_json::json!({}))`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3py_invoke::pure`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/memory/l3py_invoke/pure.rs
git commit -m "feat(l3py): validate_python_params + params-bearing python_exec_step

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B2: thread params through the operator path

**Files:**
- Modify: `core/src/memory/l3py_invoke/operator.rs`

- [ ] **Step 1: Write the failing tests**

Add to `operator.rs` tests:

```rust
    #[test]
    fn approved_with_params_builds_step_carrying_params() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let steps = prepare_python_steps(
            &c, SkillTrust::UserApproved, &sha, &serde_json::json!({"n": 3}),
        )
        .unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].parameters,
            serde_json::json!({"code": "print('hi')\n", "params": {"n": 3}})
        );
    }

    #[test]
    fn bad_params_yields_refusal() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let err = prepare_python_steps(
            &c, SkillTrust::UserApproved, &sha, &serde_json::json!([1, 2]),
        )
        .unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("object")), "{err:?}");
    }
```

Update the existing `approved_builds_exactly_one_python_exec_step` and `untrusted_yields_refusal` / `sha_drift_yields_refusal` calls to pass `&serde_json::json!({})` as the new 4th arg.

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3py_invoke::operator`
Expected: FAIL to compile — `prepare_python_steps` takes 3 args.

- [ ] **Step 3: Implement**

In `operator.rs`, import the validator + step builder. Change the `use super::pure::...` line to:

```rust
use super::pure::{
    prepare_python_invocation, python_exec_step, validate_python_params, with_python_kind,
};
```

Change `prepare_python_steps`:

```rust
pub fn prepare_python_steps(
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
    params: &serde_json::Value,
) -> Result<Vec<L3TemplateStep>, InvokeRefusal> {
    let code = prepare_python_invocation(candidate, stored_trust, stored_sha256)?;
    let params = validate_python_params(params)
        .map_err(|e| InvokeRefusal { reasons: vec![e.to_string()] })?;
    Ok(vec![python_exec_step(&code, &params)])
}
```

Change `invoke_python_skill` to take params and forward them. Add `params: &serde_json::Value` as the parameter immediately before `execute: bool`, and change the `prepare_python_steps(...)` call to pass `params`:

```rust
    let steps = match prepare_python_steps(candidate, stored_trust, stored_sha256, params) {
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3py_invoke::operator`
Expected: PASS. (The crate won't fully build yet — `l3_run.rs` calls `invoke_python_skill` with the old arity; B5 fixes that. Use `--lib l3py_invoke::operator` which still compiles the module's own tests. If the whole-crate compile is needed here, temporarily pass `&serde_json::json!({})` at the `l3_run.rs` call site and finalize in B5.)

> **Note for the implementer:** because changing `invoke_python_skill`'s arity breaks `l3_run.rs`, do B2→B5 as a tight sequence and run the whole-crate build only after B5. To keep each commit compiling, update the `l3_run.rs` call site to pass `&serde_json::json!({})` inside THIS task, then thread the real value in B5.

- [ ] **Step 5: Keep the tree compiling — patch the l3_run call site**

In `core/src/scheduler/l3_run.rs`, change the `invoke_python_skill(...)` call to add `&serde_json::json!({})` before `req.execute`:

```rust
        return invoke_python_skill(
            pool, req.memory_id, dispatcher, &candidate, trust, body_sha256,
            &serde_json::json!({}), req.execute,
        )
        .await;
```

- [ ] **Step 6: Commit**

```bash
git add core/src/memory/l3py_invoke/operator.rs core/src/scheduler/l3_run.rs
git commit -m "feat(l3py): operator invoke threads validated params into the step

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B3: thread params through the agent path

**Files:**
- Modify: `core/src/memory/l3py_invoke/agent.rs`

- [ ] **Step 1: Write the failing tests**

Add to `agent.rs` tests:

```rust
    #[test]
    fn pinned_with_params_expands_step_carrying_params() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let steps = expand_python_for_agent(
            &c, SkillTrust::Pinned, &sha, DataClass::Secret, &serde_json::json!({"id": 9}),
        )
        .expect("pinned expands");
        assert_eq!(
            steps[0].parameters,
            serde_json::json!({"code": "print('hi')\n", "params": {"id": 9}})
        );
        assert_eq!(steps[0].classification, DataClass::Secret);
    }

    #[test]
    fn bad_params_refuses() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let err = expand_python_for_agent(
            &c, SkillTrust::Pinned, &sha, DataClass::Public, &serde_json::json!("flat"),
        )
        .unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("object")), "{err:?}");
    }
```

Update the existing `pinned_expands_to_one_python_exec_planned_step`, `user_approved_is_not_autonomously_invocable`, and `pinned_with_sha_drift_refuses` calls to pass `&serde_json::json!({})` as the new trailing arg.

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3py_invoke::agent`
Expected: FAIL to compile — `expand_python_for_agent` takes 4 args.

- [ ] **Step 3: Implement**

In `agent.rs`, import the validator + the empty-check + the step builder. Change the `use super::pure::...` to:

```rust
use super::pure::{params_is_empty, prepare_python_invocation, python_exec_step, PY_EXEC_METHOD, PY_EXEC_TOOL};
use super::pure::validate_python_params;
```

(Keep `PY_EXEC_METHOD`/`PY_EXEC_TOOL` imports only if still referenced after the rewrite below; if you switch fully to `python_exec_step`, drop them and the manual `PlannedStep`-from-tool/method construction.)

Replace `expand_python_for_agent` with a version that validates params and reuses `python_exec_step` for the parameters, wrapping it in a `PlannedStep` at the data ceiling:

```rust
pub fn expand_python_for_agent(
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
    data_ceiling: DataClass,
    params: &serde_json::Value,
) -> Result<Vec<PlannedStep>, InvokeRefusal> {
    if !matches!(stored_trust, SkillTrust::Pinned) {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "skill is not autonomously invocable (trust='{}'; requires pinned)",
                stored_trust.as_str()
            )],
        });
    }
    let code = prepare_python_invocation(candidate, stored_trust, stored_sha256)?;
    let params = validate_python_params(params)
        .map_err(|e| InvokeRefusal { reasons: vec![e.to_string()] })?;
    let step = python_exec_step(&code, &params);
    Ok(vec![PlannedStep {
        tool: step.tool,
        method: step.method,
        parameters: step.parameters,
        returns: String::new(),
        done_when: String::new(),
        classification: data_ceiling,
    }])
}
```

If `PY_EXEC_TOOL`/`PY_EXEC_METHOD`/`params_is_empty` become unused after this, remove them from the import to keep clippy clean.

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3py_invoke::agent`
Expected: PASS. (Whole-crate compile still blocked by the inner_loop call site — fixed in B5.)

- [ ] **Step 5: Commit**

```bash
git add core/src/memory/l3py_invoke/agent.rs
git commit -m "feat(l3py): agent expansion threads validated params into the step

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B4: `InvokeDirective.params` field

**Files:**
- Modify: `core/src/cassandra/types.rs`
- Modify: `core/src/cassandra/types/tests.rs`

- [ ] **Step 1: Write the failing test**

Add to `core/src/cassandra/types/tests.rs` (near the existing `validate_invoke` tests, ~line 496+):

```rust
    #[test]
    fn invoke_directive_deserializes_optional_params() {
        let json = r#"{"name":"summarise","params":{"text":"hello"}}"#;
        let d: InvokeDirective = serde_json::from_str(json).unwrap();
        assert_eq!(d.name, "summarise");
        assert_eq!(d.params, serde_json::json!({"text": "hello"}));
    }

    #[test]
    fn invoke_directive_params_defaults_to_null_when_absent() {
        let json = r#"{"name":"summarise"}"#;
        let d: InvokeDirective = serde_json::from_str(json).unwrap();
        assert!(d.params.is_null());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib invoke_directive_deserializes_optional_params`
Expected: FAIL to compile — no `params` field.

- [ ] **Step 3: Implement**

In `core/src/cassandra/types.rs`, add the field to `InvokeDirective`:

```rust
pub struct InvokeDirective {
    /// snake_case skill name, exactly as surfaced in the `<skills>` block.
    pub name: String,
    /// Agent-supplied parameter values (param name → literal value). Must
    /// supply exactly the skill's declared parameters; values are guarded
    /// by `substitute_template` (no newline/control/`{{`/`}}`/over-cap).
    /// Used by the TEMPLATED skill path only.
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    /// Agent-supplied runtime params for a PYTHON skill (arbitrary JSON
    /// object). Ignored by the templated path. Defaults to JSON null when the
    /// agent emits no params; the inner loop treats null/empty as "no params".
    #[serde(default)]
    pub params: serde_json::Value,
}
```

- [ ] **Step 4: Fix the broken struct literals**

`InvokeDirective { name: ..., args: ... }` literals now miss `params`. Find them:

Run: `grep -rn "InvokeDirective {" core/src`
Expected hits: `core/src/cassandra/types/tests.rs` (~lines 522, 535, 551, 571) and possibly `core/src/scheduler/inner_loop.rs` tests. For EACH literal, add `params: serde_json::Value::Null,` (these existing tests are all templated-path tests, so null params is correct). Example:

```rust
        invoke_skill: Some(InvokeDirective { name: "s".into(), args: Default::default(), params: serde_json::Value::Null }),
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib cassandra::types`
Expected: PASS, including the 2 new directive tests.

- [ ] **Step 6: Commit**

```bash
git add core/src/cassandra/types.rs core/src/cassandra/types/tests.rs
git commit -m "feat(types): InvokeDirective.params for python-skill runtime params

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B5: inner-loop threads directive params + finalize call sites

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs`

- [ ] **Step 1: Read the invoke arm**

Re-read `core/src/scheduler/inner_loop.rs:341-442` (the `if plan.invoke_skill.is_some()` block). Note `validate_invoke()` currently returns `(name, args)` via `.map(|d| (d.name.clone(), d.args.clone()))`, and the python arm calls `expand_python_for_agent(&py.candidate, SkillTrust::Pinned, &py.body_sha256, plan.data_ceiling)`.

- [ ] **Step 2: Capture directive params**

Change the `validated` binding to also clone `params`:

```rust
            let validated = plan
                .validate_invoke()
                .map(|d| (d.name.clone(), d.args.clone(), d.params.clone()));
```

Change the `match validated` arms to destructure the triple. The `Err(malformed)` arm is unchanged. The `Ok(...)` arm becomes:

```rust
                Ok((name, args, params)) => {
```

In the templated `Some(pinned)` branch, `args` is used as before (params is simply unused there). In the python `None => match load_pinned_python_skill_by_name(...)` branch, pass params to the expansion:

```rust
                            Some(py) => match expand_python_for_agent(
                                &py.candidate,
                                SkillTrust::Pinned,
                                &py.body_sha256,
                                plan.data_ceiling,
                                &params,
                            ) {
```

- [ ] **Step 3: Write a focused test (if the inner_loop test harness supports it)**

If `inner_loop.rs` has a `#[cfg(test)] mod tests` (or a sibling `inner_loop/tests.rs`) that already exercises the python invoke arm, add a test that a directive carrying `params` produces a `plan.steps[0].parameters` with a `params` key. If the harness is too heavyweight (PG/dispatcher), rely on the e2e in Task B8 instead and note that here — do NOT fabricate a brittle harness. Document the choice in the commit message.

- [ ] **Step 4: Whole-crate build + the affected unit tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib`
Expected: PASS. This is the first point the whole `core` lib compiles after B2/B3.

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/inner_loop.rs
git commit -m "feat(scheduler): inner loop threads invoke_skill params to python expansion

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B6: daemon `l3_run` carries operator params

**Files:**
- Modify: `core/src/scheduler/l3_run.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `l3_run.rs` tests:

```rust
    #[test]
    fn parses_python_params_object() {
        let p = serde_json::json!({
            "kind": "l3_run", "memory_id": 9,
            "params": {"greeting": "hi"}, "execute": true
        });
        let got = parse_l3_run_payload(&p).unwrap();
        assert_eq!(got.params, serde_json::json!({"greeting": "hi"}));
    }

    #[test]
    fn params_default_to_null_when_absent() {
        let p = serde_json::json!({"kind": "l3_run", "memory_id": 7});
        let got = parse_l3_run_payload(&p).unwrap();
        assert!(got.params.is_null());
    }

    #[test]
    fn rejects_non_object_params() {
        let p = serde_json::json!({"kind": "l3_run", "memory_id": 1, "params": [1, 2]});
        assert!(parse_l3_run_payload(&p).unwrap_err().contains("object"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3_run`
Expected: FAIL to compile — no `params` field on `L3RunRequest`.

- [ ] **Step 3: Implement**

In `l3_run.rs`, add `params` to `L3RunRequest`:

```rust
pub struct L3RunRequest {
    pub memory_id: i64,
    pub args: BTreeMap<String, String>,
    pub params: Value,
    pub execute: bool,
}
```

In `parse_l3_run_payload`, after building `args`, read `params` (optional; must be an object if present):

```rust
    let params = match payload.get("params") {
        None | Some(Value::Null) => Value::Null,
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err("l3_run payload 'params' is not an object".to_string()),
    };
    Ok(L3RunRequest { memory_id, args, params, execute })
```

Update the existing `parses_full_payload` / `execute_defaults_false_and_args_optional` tests: they construct/inspect `L3RunRequest` only via `parse_l3_run_payload`, so they still pass — but if any test builds `L3RunRequest { ... }` as a literal, add `params: Value::Null,`.

In `run_l3_run_task`, the python branch forwards `req.params` (replacing the `&serde_json::json!({})` placeholder from B2):

```rust
        return invoke_python_skill(
            pool, req.memory_id, dispatcher, &candidate, trust, body_sha256,
            &req.params, req.execute,
        )
        .await;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib l3_run`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/l3_run.rs
git commit -m "feat(scheduler): l3_run payload carries python params to the operator invoke

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B7: CLI `--param` / `--params-json`

**Files:**
- Modify: `core/src/bin/kastellan-cli/memory_l3/run.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `run.rs` tests module:

```rust
    #[test]
    fn build_params_empty_is_empty_object() {
        let v = super::build_params(None, &[]).unwrap();
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn build_params_from_param_tokens_are_string_values() {
        let v = super::build_params(None, &v(&["greeting=hi", "name=world"])).unwrap();
        assert_eq!(v, serde_json::json!({"greeting": "hi", "name": "world"}));
    }

    #[test]
    fn build_params_json_base_merged_with_param_overrides() {
        let v = super::build_params(
            Some(r#"{"n": 5, "greeting": "old"}"#),
            &v(&["greeting=new"]),
        )
        .unwrap();
        assert_eq!(v, serde_json::json!({"n": 5, "greeting": "new"}));
    }

    #[test]
    fn build_params_rejects_non_object_json() {
        assert!(super::build_params(Some("[1,2]"), &[]).is_err());
    }

    #[test]
    fn build_params_rejects_malformed_token() {
        assert!(super::build_params(None, &v(&["noequals"])).is_err());
    }

    #[test]
    fn parse_run_argv_collects_param_and_params_json() {
        let got = parse_run_argv(&v(&[
            "5", "--param", "a=b", "--params-json", r#"{"n":1}"#, "--execute",
        ]))
        .unwrap();
        assert_eq!(got.id, 5);
        assert_eq!(got.param_tokens, v(&["a=b"]));
        assert_eq!(got.params_json.as_deref(), Some(r#"{"n":1}"#));
        assert!(got.execute);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --bin kastellan-cli build_params`
Expected: FAIL — `build_params` undefined, `RunArgv` has no `param_tokens`/`params_json`.

- [ ] **Step 3: Implement the argv fields + parse**

In `run.rs`, extend `RunArgv`:

```rust
struct RunArgv {
    id: i64,
    arg_tokens: Vec<String>,
    param_tokens: Vec<String>,
    params_json: Option<String>,
    execute: bool,
}
```

In `parse_run_argv`, add the locals and match arms (mirroring the `--arg` handling):

```rust
    let mut param_tokens: Vec<String> = Vec::new();
    let mut params_json: Option<String> = None;
```

Add these arms to the `match args[i].as_str()` (before the positional-id arm):

```rust
            "--param" => {
                i += 1;
                match args.get(i) {
                    Some(kv) => param_tokens.push(kv.clone()),
                    None => return Err("memory l3 run: --param requires a name=value".to_string()),
                }
            }
            s if s.starts_with("--param=") => param_tokens.push(s["--param=".len()..].to_string()),
            "--params-json" => {
                i += 1;
                match args.get(i) {
                    Some(j) => params_json = Some(j.clone()),
                    None => return Err("memory l3 run: --params-json requires a JSON object".to_string()),
                }
            }
            s if s.starts_with("--params-json=") => {
                params_json = Some(s["--params-json=".len()..].to_string())
            }
```

Update the final `Ok(RunArgv { ... })` to include the new fields.

- [ ] **Step 4: Implement `build_params`**

Add this pure function to `run.rs`:

```rust
/// Merge `--params-json` (base object) with `--param name=value` overrides into
/// one validated JSON object. Starts from the parsed `--params-json` (or `{}`),
/// then applies each `name=value` token as a STRING value (later wins). Rejects
/// a non-object base, malformed JSON, or a token without `=`. The result is
/// re-validated host-side by `validate_python_params` before dispatch; this
/// function only assembles it.
pub(super) fn build_params(
    params_json: Option<&str>,
    param_tokens: &[String],
) -> Result<serde_json::Value, String> {
    let mut base = match params_json {
        None => serde_json::Map::new(),
        Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => return Err("--params-json must be a JSON object".to_string()),
            Err(e) => return Err(format!("--params-json is not valid JSON: {e}")),
        },
    };
    for tok in param_tokens {
        let (name, value) = tok
            .split_once('=')
            .ok_or_else(|| format!("--param '{tok}' is not of the form name=value"))?;
        base.insert(name.to_string(), serde_json::Value::String(value.to_string()));
    }
    Ok(serde_json::Value::Object(base))
}
```

- [ ] **Step 5: Wire `build_params` into `memory_l3_run` + the payload**

In `memory_l3_run`, after the `RunArgv` destructure (add the new fields) and after the `parse_args` call, build the params and add them to the submitted payload:

```rust
    let RunArgv { id, arg_tokens, param_tokens, params_json, execute } = match parse_run_argv(args) {
        Ok(v) => v,
        Err(msg) => { eprintln!("{msg}"); return ExitCode::from(2); }
    };
```

```rust
    let params = match build_params(params_json.as_deref(), &param_tokens) {
        Ok(p) => p,
        Err(e) => { eprintln!("memory l3 run: {e}"); return ExitCode::from(2); }
    };
```

Add `params` to the submitted payload:

```rust
    let payload = serde_json::json!({
        "kind": "l3_run",
        "memory_id": id,
        "args": args_map,
        "params": params,
        "execute": execute,
    });
```

Update the existing `parse_run_argv` unit tests (`parses_id_args_and_execute`, `accepts_gnu_equals_arg_form_and_repeats`, `yes_is_an_alias_for_execute`, `id_may_follow_flags`) to include the new `RunArgv` fields in their expected literals: `param_tokens: vec![], params_json: None`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --bin kastellan-cli`
Expected: PASS (all run.rs tests).

- [ ] **Step 7: Commit**

```bash
git add core/src/bin/kastellan-cli/memory_l3/run.rs
git commit -m "feat(cli): memory l3 run --param / --params-json for python skills

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B8: end-to-end (param round-trip + secret param + over-cap)

**Files:**
- Modify: `core/tests/cli_memory_l3py_run_daemon_e2e.rs`

- [ ] **Step 1: Read the existing e2e**

Read `core/tests/cli_memory_l3py_run_daemon_e2e.rs` end-to-end. Identify: the PG+sandbox gate/skip helper, how it crystallises + approves a python skill, how it submits the `l3_run` task (or drives the CLI), and how it asserts on the `l3.invoke_outcome` audit row + stdout. Mirror that exact scaffolding — do not invent new bring-up.

- [ ] **Step 2: Add the param round-trip scenario**

Add a test that approves a python skill whose code echoes a param, submits an `l3_run` task with `params: {"greeting": "hi"}` + `execute: true`, and asserts the dispatched step's stdout contains `hi`. Use the existing skill-approval helper; the skill code:

```rust
    let code = "import os, json\n\
                p = json.loads(os.environ['KASTELLAN_PYTHON_PARAMS'])\n\
                print('GOT:' + p['greeting'])\n";
```

Submit the payload with the params object (match the file's existing submit pattern — either `submit_and_audit` with a hand-built payload including `"params": {"greeting": "hi"}`, or by driving the CLI argv with `--param greeting=hi --execute`). Assert the execution succeeded (exit 0) and the captured stdout contains `GOT:hi`.

- [ ] **Step 3: Add the secret-param scenario**

If the e2e harness has a vault/secret-materialization helper (grep the file + `core/tests` for `materialize`/`Vault`/`secret://`), add a scenario: register a secret `api/key=swordfish`, approve a skill that prints `os.environ['KASTELLAN_PYTHON_PARAMS']`, submit with `params: {"token": "secret://api/key"}`, and assert the worker-visible params contained the MATERIALIZED value (the secret resolved through `substitute_refs_in_params`) — proving the free secret threading. If no vault helper exists in this suite, SKIP this scenario and add a one-line `// TODO(params-e2e): secret-param coverage needs the vault harness` note + log it in the task's commit message and the handover (do not hand-roll vault bring-up here).

- [ ] **Step 4: Add the over-cap rejection scenario**

Submit `params` with a value of `"x".repeat(64*1024)` and `execute: true`; assert the run reports a refusal/step error mentioning the cap (the worker rejects with `INVALID_PARAMS` → surfaced as a step error in the outcome). If asserting through the daemon outcome is awkward, assert at the `parse`/dispatch layer instead and note it.

- [ ] **Step 5: Run the e2e (live PG + sandbox)**

Run (macOS, Postgres.app v18 — use the session-local override pattern from memory, NOT a global env):
```sh
source "$HOME/.cargo/env"
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-core --test cli_memory_l3py_run_daemon_e2e -- --nocapture
```
Expected: PASS (param round-trip prints `GOT:hi` through the real jail). Skip-as-pass if sandbox/PG are unavailable on the runner — confirm via `--nocapture` that any green is real, not a silent `[SKIP]`.

- [ ] **Step 6: Commit**

```bash
git add core/tests/cli_memory_l3py_run_daemon_e2e.rs
git commit -m "test(e2e): python-skill runtime params round-trip through the real jail

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B9: full verification + docs + handover

**Files:**
- Modify: `docs/devel/ROADMAP.md`
- Modify: `docs/devel/handovers/HANDOVER.md`

- [ ] **Step 1: Whole-workspace test (macOS skip-as-pass)**

Run: `source "$HOME/.cargo/env" && cargo test --workspace`
Expected: all green / 0 failed (skip-as-pass posture; expect roughly +30 tests over the 1679 Mac baseline). If the full-workspace run flakes the known `embedding_recall_e2e` PG-bring-up tests, re-run those individually per the standing macOS gotcha.

- [ ] **Step 2: Clippy the workspace**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Update ROADMAP**

In `docs/devel/ROADMAP.md`, find the Phase-4 skill-catalog block (~line 263, the "Remaining: params" note). Mark params done with the date and a one-line summary of the env-var channel + 64 KiB cap + free-form passthrough, and the deferred scratch-file/declared-schema follow-ups.

- [ ] **Step 4: Update HANDOVER**

In `docs/devel/handovers/HANDOVER.md`: add a "This session" block summarizing the two slices (worker `params` channel + core threading), the env-var/64 KiB/free-form decisions, the **battle-test-for-risk-slip-throughs follow-up**, the deferred scratch-file channel + declared schema, and the new test counts. Refresh the "Last updated" header and the "Next TODO" (the next Phase-4 picks: the `inner_loop.rs` refactor, then micro-VM backend / tiered delegation).

- [ ] **Step 5: Commit docs**

```bash
git add docs/devel/ROADMAP.md docs/devel/handovers/HANDOVER.md
git commit -m "docs(phase4): python-exec runtime params shipped — roadmap + handover

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 6: Push + open the PR**

```bash
git push -u origin feat/python-exec-runtime-params
gh pr create --base main --title "feat(phase4): python-exec runtime params (env-var channel)" \
  --body "Implements the deferred params piece of the python-exec skill catalog (ROADMAP:263). Spec: docs/superpowers/specs/2026-06-14-python-exec-runtime-params-design.md. Env-var channel (KASTELLAN_PYTHON_PARAMS, 64 KiB cap), arbitrary-JSON values, free-form passthrough, secret refs materialise via the existing dispatch chokepoint (Net::Deny contains). Slice A (worker) + Slice B (core threading + CLI + e2e). Follow-up: battle-test free-form passthrough for risk slip-throughs in test mode.

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

---

## Self-review notes (verified against the spec)

- **Spec §2 (env channel, always-set `{}`)** → A1 (`serialize_params` None⇒`{}`) + A2 (`run_code` `.env`).
- **Spec §2 (64 KiB cap, worker-authoritative)** → A1 (`MAX_PARAMS_BYTES` + `TooLarge`) + A3 (handler maps to `INVALID_PARAMS`); core early cap → B1.
- **Spec §3 (arbitrary JSON values, newlines/long-text allowed)** → A1 `serialize_params_allows_newlines_in_values`, B1 `validate_python_params` (no control-char rejection).
- **Spec §3 (snake_case TOP-LEVEL keys; nested opaque)** → B1 `validate_params_rejects_non_snake_case_top_level_key` + `validate_params_allows_arbitrary_nested_keys`.
- **Spec §4 (secret refs materialise free via chokepoint)** → B8 secret-param e2e (the recursive walker was confirmed in `core/src/secrets/substitute.rs`).
- **Spec §5 (both invocation paths)** → operator (B2 + B6 daemon + B7 CLI) and agent (B3 + B5 inner loop).
- **Spec §6 (free-form passthrough, SHA/approval untouched)** → no change to `PythonSkillCandidate`/crystallise/approval; B1–B3 add only runtime params.
- **Spec security invariants 1–6** → containment unchanged (no policy edits anywhere in the plan); approve==execute (`prepare_python_invocation` SHA check untouched); pinned-only autonomy (B3/B5 keep the `Pinned` gate); fail-closed daemon (B6 leaves the unregistered-tool path intact); code-never-surfaced (`l3_surface` not touched).
- **Spec deferrals** → scratch-file channel + declared schema + `inner_loop.rs` refactor all left out; recorded in B9 docs.
- **Type consistency:** `MAX_PARAMS_BYTES` defined in both `exec.rs` (A1) and `pure.rs` (B1) with keep-in-sync comments; `python_exec_step(code, params)` 2-arg signature used consistently in B1/B2/B3; `validate_python_params` returns the same `Value` shape everywhere; `InvokeDirective.params` (B4) is `serde_json::Value` default-null, matched by the inner-loop `.params.clone()` (B5) and the directive tests.
