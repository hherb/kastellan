# 8 — Hard constraints

These are not stylistic preferences. They are load-bearing rules that the
project's security guarantees depend on. A PR that violates any of them will
be rejected regardless of how good the code is otherwise.

Read this chapter before writing your first line of code.

---

## 1. AGPL-3.0 — AGPL-compatible dependencies only

The project is AGPL-3.0. Every library you add must have a compatible license.

**Allowed:** Apache-2.0, MIT, BSD-2, BSD-3, MPL-2.0, LGPL, GPL, AGPL.

**Not allowed:** CDDL, BUSL, SSPL, Elastic License, or any "source-available"
license.

Before adding a new `[dependencies]` entry, check its license in
`Cargo.toml` or on crates.io. If in doubt, open an issue and ask before adding
the crate.

Why this matters: a permissive dep can re-enter the process under a corporate
fork that the user cannot audit. License hygiene is part of the security
boundary.

---

## 2. Rust core — no Python in-process

Python lives only inside sandboxed worker processes. Do not add `pyo3`,
`rustpython`, or any in-process Python runtime to the core crate or any crate
it depends on.

Workers communicate with the core exclusively over JSON-RPC. The core never
evaluates Python, never imports a Python module, and never loads a `.so` built
from Python.

---

## 3. Cross-platform: Linux + macOS, both first-class

A feature that works on Linux must have a functionally equivalent counterpart
on macOS, and vice versa. The sandbox layer is the canonical example: every
`SandboxPolicy` field has an implementation in both `linux_bwrap.rs` and
`macos_seatbelt.rs`.

"Linux-only" and "macOS-only" code is acceptable only if:
- It is hidden behind `#[cfg(target_os = "linux")]` / `#[cfg(target_os = "macos")]`.
- There is a stub or equivalent path on the other platform.
- The asymmetry is documented in `docs/threat-model.md`.

Do not introduce platform-specific code paths without a corresponding guard
and a companion on the other OS.

---

## 4. No NVIDIA hard dependency

The primary development machine is an NVIDIA DGX Spark, but nothing in the
codebase may require NVIDIA hardware, CUDA, or any NVIDIA-specific library.

LLM serving is abstracted behind the `llm-router` crate, which speaks the
OpenAI HTTP API. Local inference uses vLLM/SGLang on Linux and
llama.cpp/Ollama on macOS — both work on non-NVIDIA hardware too.

---

## 5. Every worker invocation is sandboxed

The `tool_host::dispatch()` function is the **only** path that spawns workers.
It always applies a `SandboxPolicy`. There is no `spawn_unsandboxed` function;
do not add one.

If you need to debug a worker without sandboxing, run the worker binary
directly in a terminal — that is the appropriate development workflow.

---

## 6. One audit-log write site

Every tool call, LLM call, channel message, and memory write that has
security or privacy consequences must be logged to `audit_log`. The write
happens inside `tool_host::dispatch()` — **not** in the caller, not in the
worker, and not in a separate helper that some callers might skip.

New entry points (channel adapters, scheduled routines) call into `dispatch()`
rather than writing their own audit rows.

---

## 7. Secrets never leave the host boundary unmasked

Secrets are decrypted in the core process at the moment of injection into a
worker call. They are never:
- Written to any log (audit or JSONL mirror).
- Sent to the LLM unmasked.
- Readable from inside a worker outside of that single call.

If you are building a feature that uses credentials, read from `db::secrets`
inside the core and pass the decrypted value to the worker as a parameter —
just for that call.

---

## 8. Dispatcher chokepoint — no side doors

`WorkerCommand` is a module-private struct. Only `tool_host.rs` can construct
one. Only `tool_host.rs` can call `worker.call()`. This is enforced by Rust's
visibility rules, not by convention.

If you find yourself wanting to bypass this, reconsider the design. Every
new capability should go through the chokepoint so the audit log, policy gate,
and sandbox setup are always applied.

---

## Quick checklist for PRs

Before opening a PR, confirm:

- [ ] No new non-AGPL-compatible dependency added.
- [ ] No Python in the Rust core process.
- [ ] Feature works (or is stubbed) on both Linux and macOS.
- [ ] No NVIDIA-specific code without a non-NVIDIA path.
- [ ] Every new worker call goes through `tool_host::dispatch()`.
- [ ] Any audit-relevant action writes to `audit_log`.
- [ ] No secret is written to a log or sent to the LLM.
