# L3 `run` daemon reroute — the #179 Opt-3 structural fix

**Date:** 2026-06-04
**Issue:** [#179](https://github.com/hherb/hhagent/issues/179) — `memory l3 run`: live registry rebuilt from operator env diverges from the daemon's `registry.loaded` snapshot.
**Status:** design approved, ready for plan.

## Problem

`hhagent-cli memory l3 run <id>` closes the invocation TOCTOU window by
**rebuilding the tool registry in-process** (`registry_build::build_tool_registry`)
and re-validating the approved skill against *that* registry. The rebuild reads
the operator's environment (`HHAGENT_SHELL_EXEC_BIN`, the gliner-relex vars, …).

The daemon, by contrast, builds its registry once at startup from *its* unit-file
environment and holds it in an `Arc<ToolRegistry>` for the whole process lifetime.

When the two environments differ — the daemon registered `shell-exec` via its
systemd/launchd unit, but the operator runs `memory l3 run` from a plain shell
without those vars — the in-process rebuild yields a reduced registry and a
legitimately-approved skill is **refused** ("tool `shell-exec` not in registry"),
even though the daemon would dispatch it happily.

This fails *safe* (a refusal, never an unintended run), but it is a **usability
cliff**: `run` only works when the operator replicates the daemon's registry
environment. The interim diagnostic (PR #180, `diagnose_registry_divergence`)
softened the *symptom* with an actionable hint; this slice removes the *cause*.

The env-dependence is intrinsic to the CLI-spawns-in-process execution model.
The only fix that removes it (issue Opt 3) is to make the **daemon** execute, so
there is exactly one tool registry. The autonomous-door slice (PR #181) already
built daemon-side L3 execution; this slice routes the *operator* `run` path
through the daemon too, retiring the in-process execution path entirely.

## Goals

- `memory l3 run` executes against the daemon's single live `ToolRegistry`; the
  env-divergence refusal can no longer occur.
- One execution path for L3 skills (operator-triggered and agent-autonomous both
  run in the daemon).
- The operator's `run` argv contract and rendered output (Refused / DryRun /
  Executed, exit codes) are unchanged.

## Non-goals

- **`memory l3 approve` is out of scope.** It keeps validating against the
  `registry.loaded` audit snapshot — a best-effort gate that is re-checked live
  at run time. #179 is specifically about `run`.
- No new general-purpose daemon IPC socket. The existing Postgres `tasks` queue
  + `LISTEN/NOTIFY` *is* the operator→daemon command channel (it already carries
  `ask`); `run` becomes its second user.
- No change to the agent-autonomous invoke path (`expand_for_agent`,
  pinned-only, CASSANDRA-reviewed) — it already runs in the daemon correctly.

## Approach (decided)

**Transport:** reuse the Postgres task queue. `run` enqueues an `l3_run` task;
the daemon claims it on a lane loop, executes it against its live dispatcher, and
finalizes the task with a serialized result the CLI renders. No new infrastructure.

**Scope:** retire the in-process execution path entirely. `run` *requires* a
running daemon; if none is consuming the lane, it fails fast and cancels the task.
There is no `--local` fallback and no auto-fallback — one path, one registry.

### Data flow

```
operator: hhagent-cli memory l3 run <id> [--arg k=v]… [--execute]
   │  parse argv (parse_run_argv, unchanged)
   │  INSERT tasks: kind="l3_run", payload={memory_id, args, execute}, lane=long
   │     + audit actor='cli' action='task.submitted'
   │  LISTEN tasks_completed   (wait-loop lifted from ask.rs)
   ▼
daemon lane_loop (long):
   │  claim_one() → task
   │  branch on payload.kind:
   │     "l3_run" → run_l3_skill(pool, dispatcher, task)
   │        • load L3 row by memory_id, layer-guard to Skill
   │        • invoke_l3(pool, id, dispatcher, template, trust, body_sha256,
   │                    args, live_tools = dispatcher.known_tools(), execute)
   │              — operator semantics: NO CASSANDRA review, user_approved|pinned
   │              — dispatches through the daemon's live ToolRegistry
   │              — audits l3.invoked / l3.invoke_outcome / l3.invoke_rejected
   │        • finalize(task, completed, result = serde_json::to_value(report))
   │     else → run_to_terminal(...)   (unchanged ask path)
   ▼  finalize fires NOTIFY tasks_completed
operator CLI: wakes → reads tasks.result → render_invoke_report() → stdout + exit code
```

Dry-run (`execute == false`) also goes through the daemon: `invoke_l3` validates
against the live registry and returns `DryRun { steps }` without dispatching.
That is deliberate — an accurate preview must reflect the daemon's real registry,
which is the whole point of the fix.

## Components

### CLI — `core/src/bin/hhagent-cli/memory_l3/run.rs` (shrinks)

- **Keep** `parse_run_argv` verbatim (the `RunArgv { id, arg_tokens, execute }`
  contract is unchanged).
- **Remove** the in-process machinery: `build_gliner_relex_entry` +
  `build_tool_registry` rebuild, the `DryRunNeverDispatches` / `ToolHostStepDispatcher`
  construction, and the direct `invoke_l3` call. `registry_build.rs` itself stays
  (the daemon still uses it) — only the CLI's *use* is removed.
- **Add** the submit-and-wait flow:
  1. `parse_args(arg_tokens)` → `BTreeMap<String,String>` (unchanged helper).
  2. Build the `l3_run` payload, `tasks::insert_pending(pool, Lane::Long, payload)`,
     audit `task.submitted` (reuse `submit_and_audit` or an `l3_run` variant).
  3. Wait-loop lifted from `ask.rs`: `LISTEN tasks_completed`; poll task state;
     grace-timeout for no-daemon (below); on completion read `tasks.result`.
  4. Deserialize `tasks.result` into `InvokeReport`; call a new pure
     `render_invoke_report(&report) -> (String, i32)` and print + exit.
- **Extract** the existing Refused/DryRun/Executed printing logic into the pure
  `render_invoke_report` helper so the CLI and unit tests share one renderer.

### Payload schema (`tasks.payload`, `kind="l3_run"`)

```json
{ "kind": "l3_run", "memory_id": 42, "args": { "k": "v" }, "execute": false }
```

A small pure builder (`build_l3_run_payload(memory_id, args, execute) -> Value`)
+ a parser (`parse_l3_run_payload(&Value) -> Result<(i64, BTreeMap, bool), _>`)
on the daemon side, unit-tested for round-trip.

### Result schema (`tasks.result`)

The serialized `InvokeReport`. Add `Serialize, Deserialize` to `InvokeReport`
(its fields — `Vec<String>`, `Vec<L3TemplateStep>`, `Vec<StepOutcome>`,
`usize` — already serialize; `L3TemplateStep` and `StepOutcome` already derive
both). The daemon writes `serde_json::to_value(report)`; the CLI reads it back.
No parallel DTO.

### Daemon — `core/src/scheduler/runner.rs`

In `lane_loop`, immediately after `claim_one` returns a task, branch on
`claimed.payload.get("kind")`:
- `"l3_run"` → new `run_l3_skill(pool, dispatcher, &claimed)`, then
  `tasks::finalize(pool, claimed.id, Completed, result)`. A **refusal still
  finalizes `completed`** — a refused run is a valid outcome the CLI renders, not
  a task failure. Per-step errors under `--execute` are carried inside the report
  (matching today's `Executed { outcomes }`); the task itself completes.
- anything else → the existing `run_to_terminal` flow, untouched.

`run_l3_skill` loads the L3 row (id-parse → fetch → layer-guard, reusing the
shared loader from `memory_l3/shared.rs` / db helpers as appropriate), then calls
the **existing** `invoke_l3` with the daemon's `dispatcher` and
`dispatcher.known_tools()` as `live_tools`, and returns the `InvokeReport`.

### Audit provenance

`invoke_l3` audits with `CLI_AUDIT_ACTOR` (`actor='cli'`). Keeping that when the
daemon executes is **intentional**: the `l3.invoked` / `l3.invoke_outcome` /
`l3.invoke_rejected` rows stay attributed to the operator-initiated CLI path
(distinct from the agent path's `actor='scheduler'`), preserving provenance even
though the steps physically dispatch inside the daemon. The submit-time
`actor='cli' action='task.submitted'` row records the operator's intent up front.

### Removed: the interim divergence diagnostic

With the in-process rebuild gone, the env-divergence case it classified can no
longer arise on the `run` path. Delete `diagnose_registry_divergence` and
`RegistryDivergence` (+ `Display` impl + their unit tests) from
`core/src/memory/l3_invoke/pure.rs`, and the `hint:` wiring from the CLI refusal
arm. No other consumer exists (grep-verified before deletion). Its job — soften
the cliff until Opt-3 — is now done by Opt-3 itself.

## Error handling & edge cases

- **No daemon running:** after submit, if the task stays `pending` past a grace
  window (default ~5 s, env-overridable, e.g. `HHAGENT_L3_RUN_GRACE_SECS`), no
  lane loop is consuming it → the CLI prints a clear "daemon does not appear to be
  running" message, **marks the task cancelled** (reuse `mark_cancelled` +
  `task.cancelled` audit, as `ask` does on interrupt), and exits non-zero. The
  cancel is load-bearing: it prevents a still-`pending` `--execute` task from
  being silently claimed and executed later when the daemon next starts.
- **Pending → running observed:** the daemon is alive; the CLI waits for
  completion, honouring an overall cap derived from the long-lane deadline
  (env-overridable), then reports a timeout if exceeded.
- **Daemon dies mid-run:** the existing crash-recovery sweep marks the leased
  task `crashed` on next startup; the CLI's overall-cap wait also surfaces it.
- **Skill not found / wrong layer / untrusted / unknown tool / bad template:**
  `invoke_l3` already returns `Refused { reasons }`; the daemon finalizes with it;
  the CLI renders the refusal and exits non-zero. (Unknown-tool can no longer be a
  *false* refusal caused by env divergence — that is the fix.)
- **Bad argv:** rejected CLI-side before any submit (unchanged).

## Testing

### Unit
- `render_invoke_report`: Refused / DryRun / Executed → expected text + exit code
  (0 for DryRun and all-ok Executed; non-zero for Refused and any-err Executed).
- `InvokeReport` serde round-trip (each variant).
- `build_l3_run_payload` / `parse_l3_run_payload` round-trip + malformed-payload
  rejection.

### Live-PG e2e — rewrite `core/tests/cli_memory_l3_run_e2e.rs` to drive a real daemon

Each scenario brings up a daemon (with `shell-exec` registered) and runs the
`hhagent-cli` binary as a subprocess:
1. **Dry-run preview** — approved skill, no `--execute` → report lists the
   concrete steps; nothing dispatched.
2. **Execute** — `--execute` → steps dispatch through the daemon; outcomes Ok.
3. **Untrusted refuses** — a crystallised-but-unapproved skill → Refused.
4. **Unknown tool refuses** — a skill referencing a tool the daemon never
   registered → Refused (a *genuine* unknown, not env divergence).
5. **Stop at first error** — multi-step skill whose first step fails → only the
   first outcome present, `any_err` set.
6. **★ Divergence fixed (the #179 regression pin)** — the daemon has `shell-exec`
   registered; the CLI subprocess runs in an env *without* `HHAGENT_SHELL_EXEC_BIN`;
   `run --execute` now **succeeds** (previously refused). This is the test that
   proves the cause is removed.
7. **No daemon** — submit with no daemon consuming the lane → grace-timeout →
   the task is `cancelled` and the CLI reports the no-daemon error.

## Verification checklist (session end)

- `cargo test --workspace` green (count = baseline − deleted-diagnostic-tests +
  new unit/e2e; record the exact delta).
- `cargo clippy --workspace --all-targets --locked -- -D warnings` exit 0.
- Doc-links unchanged vs `main`.
- Live PG: the rewritten `cli_memory_l3_run_e2e` scenarios green (zero `[SKIP]`),
  on the DGX where `core` runs natively.
- Grep-confirm `diagnose_registry_divergence` / `RegistryDivergence` have no
  remaining references after deletion.

## File-size watch

`run.rs` shrinks (good). `runner.rs` gains the `l3_run` branch + `run_l3_skill`;
keep an eye on its LOC and lift `run_l3_skill` into a sibling (e.g.
`scheduler/runner/l3_run.rs` or `memory/l3_invoke/daemon.rs`) if `runner.rs`
approaches the 500-LOC soft cap.
