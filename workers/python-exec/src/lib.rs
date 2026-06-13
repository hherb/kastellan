//! kastellan-worker-python-exec: run agent-authored Python source in the
//! strictest jail any kastellan worker has — no network, no persistent
//! filesystem, hard CPU/memory/wall-clock caps enforced by the
//! [`SandboxPolicy`] the host built (see
//! `core/src/workers/python_exec.rs`).
//!
//! The worker itself is a dumb, policy-free pipe (like shell-exec): it
//! spawns the operator-resolved CPython interpreter as a **child**
//! process — which inherits the jail, the seccomp filter, the Landlock
//! ruleset, and the rlimits — pipes the source over stdin, and returns
//! `{exit_code, stdout, stderr}`. A Python exception is *not* an RPC
//! error: it comes back as a nonzero `exit_code` + traceback on
//! `stderr`, which is what the planner needs to iterate on its own code.
//!
//! Design: `docs/superpowers/specs/2026-06-12-python-exec-worker-design.md`.
//!
//! [`SandboxPolicy`]: https://docs.rs/kastellan-sandbox

pub mod exec;
pub mod handler;
