# 8 — Hard constraints

These are not stylistic preferences. They are load-bearing rules that the
project's security guarantees depend on. A PR that violates any of them will
be rejected regardless of how good the code is otherwise.

Read this chapter before writing your first line of code.

Rules 1–5 are the hard constraints listed in the repository's `CLAUDE.md`.
Rules 6–8 are architecture invariants that reviewers hold to the same
standard. The long-form versions live in
[`docs/architecture.md`](../../architecture.md) and
[`docs/threat-model.md`](../../threat-model.md).

---

## 1. AGPL-3.0 — AGPL-compatible dependencies only

The project is AGPL-3.0. Every library you add must have a compatible license.

**Allowed:** Apache-2.0, MIT, BSD-2, BSD-3, MPL-2.0, LGPL, GPL, AGPL.

**Not allowed:** CDDL, BUSL, SSPL, Elastic License, or any "source-available"
license.

Before adding a new `[dependencies]` entry, check its license in
`Cargo.toml` or on crates.io. If in doubt, open an issue and ask before adding
the crate.

Why this matters: an incompatible license would make the combined work
impossible to distribute under the AGPL, and "source-available" terms can be
changed by their owner at any time. Every dependency is also code running with
the core's privileges, so license hygiene and supply-chain review go together.
`Cargo.lock` is committed and CI builds with `--locked`, so a new dependency
always shows up in the diff.

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

No Linux-only or macOS-only code without a counterpart of equivalent
guarantee on the other OS. The sandbox layer is the canonical example: both
`sandbox/src/linux_bwrap.rs` and `sandbox/src/macos_seatbelt.rs` implement the
`SandboxBackend` trait from the same `SandboxPolicy` struct, and the opt-in
micro-VM tier exists on both (Firecracker on Linux, Apple `container` on
macOS).

Platform-specific code is acceptable only if:
- It is behind `#[cfg(target_os = "linux")]` / `#[cfg(target_os = "macos")]`.
- The other platform has a counterpart giving an equivalent guarantee.
- Where the guarantees genuinely differ, the gap is stated openly in
  `docs/threat-model.md` (its "Asymmetric platform note" is the model) rather
  than implied away.

A practical trap: each host compiles only its own `cfg` arm, so a Mac build
never type-checks Linux-only code and vice versa. CI type-checks and lints the
whole workspace on Linux on every pull request; macOS-only code is checked only
by building on a Mac.

---

## 4. No NVIDIA hard dependency

The primary development machine is an NVIDIA DGX Spark, but nothing in the
codebase may require NVIDIA hardware, CUDA, or any NVIDIA-specific library.

LLM serving is abstracted behind the `llm-router` crate, which speaks the
OpenAI-compatible HTTP API. Local inference typically uses vLLM/SGLang on
Linux and llama.cpp/Ollama on macOS — any OpenAI-compatible server works,
on any hardware. The optional guard-model service (`kastellan-guard`) offloads
to a GPU when there is one and runs on the CPU otherwise.

---

## 5. Every worker invocation is sandboxed

Workers are spawned only through the configured `SandboxBackend`, and every
tool call goes through `tool_host::dispatch()`. Every spawn applies a
`SandboxPolicy`. There is no `spawn_unsandboxed` function and no "run this one
unjailed" flag; do not add one. This holds for the opt-in micro-VM backends too
— they are another `SandboxBackend`, not a bypass — and a backend that cannot
run (a failed probe, a missing lockdown shim) fails closed rather than falling
back to no sandbox.

If you need to debug a worker without sandboxing, run the worker binary
directly in a terminal — that is the appropriate development workflow.

---

## 6. Audit what matters, through the one insert path

Every tool call, plan, verdict, channel message, memory write, egress decision
and secret redemption that has security or privacy consequences must be
logged to `audit_log`. Two rules keep that reliable:

- **Tool calls are audited by the chokepoint.** The `tool:<name>` row (and
  any `policy / secret.redeemed` or `policy / injection.blocked` row) is
  written inside `tool_host::dispatch()` — not by the caller, not by the
  worker. A new entry point that runs a tool calls `dispatch()` rather than
  writing its own row.
- **Every audit row goes through `kastellan_db::audit::insert`.** That
  function applies the NUL escape and the 4 KiB payload cap, and writes as the
  runtime role whose `UPDATE`/`DELETE`/`TRUNCATE` grants are revoked. Do not
  write `INSERT INTO audit_log` anywhere else.

Audit rows are best-effort with respect to the action they describe (a failed
insert does not undo a completed tool call), but a lost row is never silent:
it is reported on stderr or in the tracing log.

---

## 7. Secrets never leave the host boundary unmasked

Secrets are stored encrypted (AES-256-GCM) and the planner only ever sees an
opaque reference of the form `secret://<8-hex>`. Inside `tool_host::dispatch()`,
immediately before the call, every parameter that is exactly such a reference
is replaced with the plaintext (`core/src/secrets/substitute.rs`). Plaintext
secrets are never:
- Written to any log — the audit row records the request *before*
  substitution, and redemption rows carry only a hash of the reference.
- Sent to the LLM — the worker's result and error message are scrubbed of the
  secrets materialized into that call before anyone sees them.
- Sent out of a force-routed network worker unnoticed — the egress proxy scans
  intercepted TLS traffic for that worker's own secrets.

If you are building a feature that uses credentials, take a `secret://`
reference in the tool's parameters and let the chokepoint substitute it. Do not
read the vault yourself and pass plaintext around, and a failed redemption
must stop the call (it does today: the worker is never called).

---

## 8. Dispatcher chokepoint — no side doors

`WorkerCommand` (the sealed JSON-RPC request) has module-private fields and a
module-private constructor, and `SupervisedWorker::call` is module-private too.
Only the `tool_host` module (`tool_host.rs` and the files under `tool_host/`)
can build a request or call a worker. This is enforced by Rust's visibility
rules — a sibling module that tries fails to compile, and a `compile_fail`
doctest pins it — not by convention.

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
- [ ] Every new worker call goes through `tool_host::dispatch()`; no
      unsandboxed spawn path.
- [ ] Any audit-relevant action writes to `audit_log` via
      `kastellan_db::audit::insert`.
- [ ] Credentials arrive as `secret://` references; no secret is written to a
      log or sent to the LLM.
