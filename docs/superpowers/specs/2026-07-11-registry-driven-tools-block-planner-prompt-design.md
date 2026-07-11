# Registry-driven `<tools>` block for the planner prompt

**Date:** 2026-07-11
**Status:** Design approved, ready for implementation plan
**Branch:** `feat/planner-tools-block` (off `main`)

## Problem

The agent can receive a Matrix message, run its plan-based ReAct loop, dispatch a
sandboxed `web-search`/`web-research` worker, and reply — every stage of that
pipeline is built and wired. Yet asked "what happened in Germany yesterday?" it
answers *"I do not have access to real-time news or a web search tool."*

Root cause: **the planner LLM is never told which tools exist.** Tools are
advertised only through static prose in `prompts/agent_planner.md`, which:

- names only `shell-exec` (and `document-reader` as an illustrative example),
- never mentions `web-search`/`web-research`/`web-fetch`/`python-exec`/etc., and
- **actively forbids** them:
  - line 162 — *"do not shell out to compute it or search the web."*
  - lines 168–172 — *"Only the tools described to you exist … Never invent a
    tool (there is no `google_search`, no web-search tool unless one is listed)."*

Since nothing ever "lists" the web tools, the model correctly concludes it has
none. The registry (`WORKER_MANIFESTS` → `ToolRegistry`) already knows every
dispatchable tool; the prompt assembler simply never renders that knowledge.

## Goal

Make the planner's capability list **dynamic and authoritative**: every worker
that successfully registers is described to the LLM — name, JSON-RPC method, and
parameters — via a new `<tools>` block assembled from the live registry. A tool
that fails to resolve (e.g. `web-search` with no SearxNG endpoint) is neither
dispatchable *nor* advertised, so the planner is never invited to call something
that would return `UNKNOWN_TOOL` or fail to spawn.

**Decided scope:** advertise **all** successfully-registered tools. The
CASSANDRA reviewer, the argv/host allowlists, and the classification floor
remain the security gate — tool obscurity was never the boundary (threat-model
invariant unchanged).

## Non-goals

- **No native LLM function/tool-calling.** The loop stays plan-JSON ReAct; we
  only change what the planner is *told*, not how it emits steps.
- **No change to dispatch, allowlists, or the reviewer.** Advertisement only.
- **Does not configure any backend.** Actually answering a web question still
  requires the operator to stand up SearxNG and set
  `KASTELLAN_WEB_SEARCH_ENDPOINT` (+ the `tool_allowlists` row). This slice
  removes the *code/prompt* blocker; the deployment step is separate and called
  out in "Operational note" below.

## Design

### 1. New data type — `ToolDoc`

Defined next to the manifest trait in `core/src/worker_manifest.rs`. Fully
static, `const`-friendly, authored in Rust source:

```rust
pub struct ToolDoc {
    pub name:    &'static str,          // == manifest name() / registry key
    pub method:  &'static str,          // JSON-RPC method, e.g. "web.search"
    pub summary: &'static str,          // one line: what it does + when to reach for it
    pub params:  &'static [ToolParam],
}

pub struct ToolParam {
    pub name:        &'static str,
    pub description: &'static str,
    pub required:    bool,
}
```

Because every `ToolDoc` is compiled-in code authored by us, it is **trusted**
(same trust tier as the L0 meta-rules and surfaced skills) — the renderer does
**no** HTML-style escaping of its bodies.

### 2. `WorkerManifest` trait extension

Add one defaulted method:

```rust
fn tool_doc(&self) -> Option<ToolDoc> { None }
```

Default `None` = "not advertised" (nothing breaks; a future worker can opt out).
All seven current workers implement it in this slice:

| Tool            | Method(s)                        | Notes                                  |
|-----------------|----------------------------------|----------------------------------------|
| `shell-exec`    | `shell.exec`                     | argv[0] must be an absolute path        |
| `gliner-relex`  | `extract` (relation extraction)  | disabled by default; only advertised when enabled |
| `python-exec`   | `python.exec`, `python.evaluate` | advertise the primary method(s)         |
| `web-fetch`     | `web.fetch`                      | fetch one known URL                     |
| `web-search`    | `web.search`                     | query → result titles/URLs/snippets     |
| `web-research`  | `web.research`                   | search + fetch + rank passages          |
| `browser-driver`| `browser.render`                 | disabled by default                     |

The exact `summary`/`params` wording is authored during implementation by
reading each worker's handler; a worker exposing multiple methods either emits
one `ToolDoc` for the primary method or the plan splits it — decided per worker.

### 3. Collection — only registered tools

`assemble_registry` (`core/src/registry_build.rs`) already iterates the
manifests and, in the `Resolution::Register` arm, inserts the entry and records
a `LoadedToolRecord`. That same arm additionally collects `m.tool_doc()` (when
`Some`) into a `Vec<ToolDoc>`. `Disabled`/`Misconfigured` workers contribute
nothing — so a tool that can't run is never advertised.

`assemble_registry` and `build_tool_registry` gain the `Vec<ToolDoc>` in their
return tuple. The reserved `handoff` built-in is skipped here (as today) — it is
documented by the separate always-present `<handoff>` block, not `<tools>`.

### 4. Rendering — the `<tools>` block

New pure `render_tools_block(tools: &[ToolDoc]) -> String` in
`core/src/prompt_assembly/assemble.rs`, mirroring the existing `<skills>` shape:

```text
<tools>
- web-search (method: web.search): Search the web via a SearxNG backend; returns result titles, URLs, and snippets. Use for questions needing current or external facts.
  params: query (the search query) [required], count (max results, default 10) [optional]
- shell-exec (method: shell.exec): Run an allowlisted command with an absolute argv[0]; no shell interpretation.
  params: argv (command and arguments, argv[0] absolute) [required]
</tools>
```

Rules:
- **Omitted entirely when the slice is empty** (zero registered tools),
  consistent with the empty-layer rule for `<l1_insights>`/`<recalled>`.
- **No escaping** — trusted compiled-in text.
- One `-` entry per tool; a `params:` continuation line lists each param as
  `name (description) [required|optional]`. A tool with no params omits the
  `params:` line.
- Deterministic: same `&[ToolDoc]` → same bytes.

**Ordering.** The assembler's framing becomes:

```text
L0 → L1 → skills → recalled → tools → handoff → base
```

`<tools>` sits immediately before `<handoff>` — grouping the two
capability-describing blocks (both tell the planner how to call things), after
the untrusted `recalled`/L1 layers and before the verbatim `<base>`
(`agent_planner.md`).

### 5. `agent_planner.md` edits

Required regardless of the block. Replace the two prohibitions:

- Remove line 162's "do not … search the web" clause.
- Replace lines 168–172 with, in substance:
  > *"The tools available to you are listed in the `<tools>` block. Only those
  > tools exist — never invent others. Each entry gives the tool name, its
  > `method`, and its parameters; emit steps using exactly those names and
  > shapes. If a step returns `err: UNKNOWN_TOOL`, the tool is not available."*

Keep the step-JSON schema, the `handoff`/`fetch_handoff` guidance, and the
shell-exec absolute-path rule. The `<tools>` block — not prose — is the
authoritative list of what exists.

### 6. Wiring / data flow

The advertised tool set is fixed for the daemon's lifetime (the registry is
built once at startup), so the `Vec<ToolDoc>` is computed once and threaded
through as static context:

```
build_tool_registry  →  (ToolRegistry, Vec<LoadedToolRecord>, Vec<ToolDoc>)
        │
   main.rs  →  spawn_scheduler(..., tool_docs)
        │
   RouterAgent { …, tool_docs: Arc<[ToolDoc]> }        // daemon-lifetime field
        │
   RouterAgent::formulate_plan  →  assemble_system_prompt(…, tools: &[ToolDoc])
```

`assemble_system_prompt` gains a `tools: &[ToolDoc]` parameter. The CLI path
that rebuilds the registry (`memory l3 run`) ignores the docs (it never assembles
a planner prompt) — the extra tuple element is simply unused there.

### 7. Testing (TDD)

- **Pure render tests** (`assemble.rs`): block shape; required vs optional param
  markers; a param-less tool omits the `params:` line; empty slice → block
  omitted; ordering (`recalled` before `tools` before `handoff`); trusted text
  passes through unescaped.
- **Collection test** (`registry_build.rs`): a `Register` fake with a `ToolDoc`
  appears in the returned `Vec<ToolDoc>`; a `Disabled`/`Misconfigured` fake does
  not; the `handoff` reserved name never contributes.
- **Drift guard**: a table-driven test over `WORKER_MANIFESTS` asserting that
  every manifest returning `Some(doc)` has `doc.name == m.name()` (prevents a
  ToolDoc name drifting from the registry key). Optionally assert the method
  string is non-empty.
- **Integration**: extend `scheduler_inner_loop_e2e` (or the prompt-assembly
  golden) to assert the assembled system prompt contains a `<tools>` block that
  mentions `web.search` when web-search is registered.

## Operational note (not part of this code slice)

After this lands, the Matrix → web-search → reply flow is code-complete, but
answering a live question additionally needs, at deploy time:

1. A reachable SearxNG instance and `KASTELLAN_WEB_SEARCH_ENDPOINT` (and/or the
   `web-research` equivalent) — `scripts/web-search/setup-searxng.sh` stands one
   up on `127.0.0.1:8888`.
2. The endpoint host added to the `tool_allowlists` table for `web-search`.
3. The Matrix worker built `--features live-matrix` with
   `KASTELLAN_MATRIX_HOMESERVER_URL` + bot creds set.

If (1)/(2) are absent, `web-search` won't register and — by design — won't be
advertised; the planner simply won't have a web tool, rather than failing on one.

## Files touched

- `core/src/worker_manifest.rs` — `ToolDoc`/`ToolParam` types, trait method.
- `core/src/registry_build.rs` — collect docs from `Register` arm; extend return.
- `core/src/prompt_assembly/assemble.rs` — `render_tools_block`, new param,
  ordering, module doc update.
- `core/src/scheduler/agent.rs` — `RouterAgent` field + pass docs to assembler.
- `core/src/main.rs` — thread docs from `build_tool_registry` into
  `spawn_scheduler`.
- `core/src/workers/{shell_exec,gliner_relex,python_exec,web_fetch,web_search,web_research,browser_driver}.rs`
  — implement `tool_doc()`.
- `prompts/agent_planner.md` — remove prohibitions, point at `<tools>`.
- Tests as above.
