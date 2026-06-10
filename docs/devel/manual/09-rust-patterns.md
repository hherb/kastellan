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

---

## `async` / `await` and Tokio

The agent core is async. Most database and network calls use `async fn` and
`.await`:

```rust
async fn fetch_task(pool: &PgPool) -> Result<Option<Task>, DbError> {
    sqlx::query_as!(Task, "SELECT * FROM tasks LIMIT 1")
        .fetch_optional(pool)
        .await   // wait for the DB query to complete
}
```

You do not need to understand the async runtime internals. The rule of thumb:
if a function is `async fn`, call it with `.await`. The Tokio runtime (started
in `main.rs`) drives everything.

---

## Traits as interfaces

Rust uses traits where other languages use interfaces or abstract base classes.
The sandbox crate defines a `SandboxBackend` trait:

```rust
pub trait SandboxBackend {
    fn spawn_under_policy(&self, cmd: &str, policy: &SandboxPolicy)
        -> Result<Child, SandboxError>;
}
```

`LinuxBwrap` and `MacosSeatbelt` both implement this trait. Code that wants
to spawn a worker holds an `Arc<dyn SandboxBackend>` — it does not know or
care which platform backend it is using.

When you add a new feature that must work on both platforms, implement the
same trait on both structs.

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

`WorkerCommand` is a good example. It is a struct with no public fields and
no public constructor:

```rust
pub(crate) struct WorkerCommand {
    method: String,
    params: Value,
}

impl WorkerCommand {
    pub(crate) fn new(method: impl Into<String>, params: Value) -> Self { … }
}
```

`pub(crate)` means "visible only inside this crate". Code outside `kastellan-core`
cannot construct a `WorkerCommand`. This is how the dispatcher chokepoint is
enforced at the compiler level — it is not a convention; it is a type-system
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

Unit tests live inside the source file they test, in a `#[cfg(test)] mod tests`
block. Integration tests live in separate files under `crate/tests/`.

```rust
// In src/my_module.rs
#[cfg(test)]
mod tests {
    use super::*;   // import everything from the parent module

    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
```

For database integration tests, the project uses per-test Postgres clusters
(see `tests-common/`). Each test gets its own database that is created
and destroyed within the test. This makes tests independent and safe to run
in parallel.

---

## File-size conventions

The project uses a soft 500-LOC cap per source file. When a file grows beyond
this, split it into sibling modules:

- `graph.rs` (main file, stays at its path) with `mod tests;` at the bottom
- `graph/tests.rs` (sibling file containing the `#[cfg(test)] mod tests` block)

This Rust 2018 sibling-directory pattern lets `git log --follow` continue
tracking the main production file unchanged.
