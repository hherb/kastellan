# 9 — Rust patterns used here

You do not need to be a Rust expert to contribute. This chapter explains the
handful of Rust patterns that appear repeatedly in this codebase so you can
read and modify existing code with confidence.

---

## The `Result<T, E>` return type

Almost every function that can fail returns `Result<T, E>`. The `?` operator
is used heavily to propagate errors up the call stack:

```rust
fn read_config(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path)?;  // ? returns early on error
    let config: Config = toml::from_str(&text)?; // ? again
    Ok(config)
}
```

If you see `?` on a line, it means "if this fails, return the error from this
function immediately." You rarely need to handle errors inline in this codebase
— just propagate them with `?` and let the caller or the top-level handler
deal with them.

Library crates define typed error enums with `thiserror` (`DbError`,
`SandboxError`, `ToolHostError`); the daemon's `main.rs` uses `anyhow` at its
edge. Some
error enums deliberately have **no** `#[from]` conversion, so that a stray `?`
cannot silently put a new failure into a bucket other code relies on — when a
conversion is missing, construct the variant explicitly rather than adding
one.

---

## `async` / `await` and Tokio

The agent core is async. Most database and network calls use `async fn` and
`.await`:

```rust
async fn task_state(pool: &PgPool, id: i64) -> Result<Option<String>, DbError> {
    let row = sqlx::query("SELECT state FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await   // wait for the DB query to complete
        .map_err(|e| DbError::Query(format!("tasks select: {e}")))?;
    row.map(|r| r.try_get("state"))
        .transpose()
        .map_err(|e| DbError::Query(format!("decode tasks.state: {e}")))
}
```

The tree uses `sqlx`'s runtime `query(…)` with `.bind(…)` and `try_get`, not
the compile-time `query!` macros, so building never needs a live database.

You do not need to understand the async runtime internals. The rule of thumb:
if a function is `async fn`, call it with `.await`. The Tokio runtime (started
in `main.rs`) drives everything.

---

## Traits as interfaces

Rust uses traits where other languages use interfaces or abstract base classes.
The sandbox crate defines a `SandboxBackend` trait:

```rust
pub trait SandboxBackend: Send + Sync {
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> Result<std::process::Child, SandboxError>;
}
```

`LinuxBwrap` and `MacosSeatbelt` implement this trait, and so do the opt-in
micro-VM backends (`LinuxFirecracker`, and the Apple `container` backend on
macOS). Code that wants to spawn a worker holds an `Arc<dyn SandboxBackend>`
— it does not know or care which backend it is using. The daemon builds one
`SandboxBackends` bundle at startup and resolves each worker's
`SandboxBackendKind` against it.

The trait must stay **`dyn`-safe**: do not add generic methods to it. A new
sandboxing strategy is a new type implementing the trait. When you add a
feature that must work on both platforms, implement it in both backends.

---

## `Arc` and shared ownership

`Arc<T>` is a reference-counted smart pointer for sharing data across async
tasks or threads. You will see it when the same value needs to be held in
multiple places:

```rust
let backend: Arc<dyn SandboxBackend> = Arc::new(LinuxBwrap::new());
// backend can now be cloned cheaply and shared across tasks
let backend2 = Arc::clone(&backend);
```

You almost never need to think about when to drop an `Arc` — it frees the
underlying value when the last reference is dropped.

---

## The newtype pattern (sealed types)

`WorkerCommand` (in `core/src/tool_host.rs`) is a good example. The type is
public, but its fields and its constructor have no visibility modifier at all:

```rust
pub struct WorkerCommand {
    method: String,
    params: serde_json::Value,
}

impl WorkerCommand {
    fn new(method: impl Into<String>, params: serde_json::Value) -> Self { … }
}
```

An item with no modifier is private to its module (and that module's
children). That is narrower than `pub(crate)`, which would let any module in
the crate use it. So not even another module inside `kastellan-core` — the
scheduler, say — can build a `WorkerCommand`; only `tool_host` can. This is how
the dispatcher chokepoint is enforced at the compiler level, and a
`compile_fail` doctest pins it: it is not a convention, it is a type-system
guarantee.

---

## `#[cfg(target_os = "linux")]` — conditional compilation

Platform-specific code uses Rust's conditional compilation attributes:

```rust
#[cfg(target_os = "linux")]
fn apply_landlock() { … }

#[cfg(target_os = "macos")]
fn apply_seatbelt() { … }
```

You will also see `#[cfg(test)]` on test modules — those blocks are only
compiled when running tests, not in production builds.

---

## Test structure

Unit tests sit next to the code they test, in a `#[cfg(test)] mod tests`
module. Many keep it inline; larger modules move it to a **sibling file** so
the production file stays short:

```rust
// In src/my_module.rs — at the bottom
#[cfg(test)]
mod tests;
```

```rust
// In src/my_module/tests.rs
use super::*;   // import everything from the parent module

#[test]
fn it_works() {
    assert_eq!(2 + 2, 4);
}
```

Integration tests live in separate files under `<crate>/tests/`.

Shared test helpers live in the `kastellan-tests-common` dev-dependency crate
(`tests-common/`). Reach for it before writing a helper of your own — a
byte-copied helper is how skip logic drifts:

- `pg::bring_up_pg_cluster` — a throwaway Postgres cluster for a test,
  stopped and wiped when the returned value drops (even on panic).
- `skip::skip_line`, `skip::warn_line`, `skip::e2e_line` — build the
  `[SKIP]`, `[WARN]` and `[E2E]` evidence lines. Each starts with a newline and
  must be printed in **one** write, so the gate script's anchored counts see it.
- `require::RequireKnob` — the `KASTELLAN_*_REQUIRE_E2E` knobs that turn a
  tier's skips into failures. A new tier is one `RequireKnob::new(…)` constant
  plus an entry in `require::KNOBS`, never a hand-rolled copy.
- `panic_hook` — a panic hook that stops a panic message from forging an
  evidence line at column 0.

See [chapter 5](./05-build-test-run.md) for what the markers mean and how
`scripts/run-e2e-gate.sh` uses them.

---

## File-size conventions

The project uses a soft 500-LOC cap per source file. When a file grows past
it, split it into sibling modules — the test block first, then cohesive groups
of production code:

- `graph.rs` (main file, stays at its path) with `mod tests;` at the bottom
- `graph/tests.rs` (the former inline test block)
- `graph/<topic>.rs` for a lifted helper family, declared with `mod <topic>;`

This Rust 2018 sibling-directory pattern keeps the main file at its path, so
`git log --follow` keeps tracking it.

Two habits go with it. **Split before the change that grows a file**, in a
separate movement-only commit whose set of `#[test]` names is identical on
both sides, so a reviewer can verify nothing changed. And treat the cap as
soft but real: a handful of files are over it today and are tracked in the
handover as owed splits, not as precedent. [Chapter 14](./14-test-lift-refactors.md)
walks through a lift step by step.
