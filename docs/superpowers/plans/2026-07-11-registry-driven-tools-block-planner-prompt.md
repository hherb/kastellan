# Registry-driven `<tools>` block for the planner prompt — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Advertise every successfully-registered worker to the planner LLM via a dynamic `<tools>` block assembled from the live registry, so the agent can decide to call `web-search`/`web-research` (and every other registered tool) instead of refusing web-answerable questions.

**Architecture:** A new static `ToolDoc`/`ToolParam` pair, one per worker, surfaced through a defaulted `WorkerManifest::tool_doc()` method. The registry builder collects docs only for tools that reach the `Register` resolution, threads the resulting `Vec<ToolDoc>` into the production `PgSystemPromptBuilder`, and the pure prompt assembler renders them into a `<tools>` block positioned between `<recalled>` and `<handoff>`. `agent_planner.md` is edited to point at that block and drop its "no web-search tool" prohibitions.

**Tech Stack:** Rust (kastellan-core), rustc 1.96, `cargo test`/`cargo clippy`. No new dependencies.

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible dependencies only.** This plan adds no dependencies.
- **Cross-platform Linux + macOS.** All code here is platform-neutral (pure Rust string assembly + trait plumbing); no `#[cfg(target_os)]` involved.
- **Run every `cargo` command in the FOREGROUND.** Never background a `cargo test`/`clippy`/`build`; wait for it to finish before the next step.
- **`git add` specific files only — never `git add -A`.** Untracked files in the tree must stay untracked.
- **TDD:** failing test first, minimal impl, green, commit. Keep files focused.
- **`ToolDoc` bodies are trusted compiled-in text** — the renderer does NOT escape them (unlike L1/recalled). Do not add escaping.
- Source `$HOME/.cargo/env` first in any shell that runs cargo (`source "$HOME/.cargo/env"`).
- Build/test the crate with `cargo test -p kastellan-core` and lint with `cargo clippy -p kastellan-core --all-targets -- -D warnings`.

---

## File Structure

- `core/src/worker_manifest.rs` — **Modify.** Add `ToolDoc` + `ToolParam` structs and the defaulted `WorkerManifest::tool_doc()` method.
- `core/src/prompt_assembly/assemble.rs` — **Modify.** New `render_tools_block`; add `tools: &[ToolDoc]` param to `assemble_system_prompt`; render between `<recalled>` and `<handoff>`; module-doc framing update.
- `core/src/prompt_assembly/assemble/tests.rs` — **Modify.** Update existing call sites (append `&[]`); add `<tools>` render tests.
- `core/src/workers/shell_exec.rs`, `gliner_relex.rs`, `python_exec.rs`, `web_fetch.rs`, `web_search.rs`, `web_research.rs`, `browser_driver.rs` — **Modify.** Implement `tool_doc()`.
- `core/src/registry_build.rs` — **Modify.** Collect docs in the `Register` arm; extend `assemble_registry` + `build_tool_registry` return tuples; drift-guard test.
- `core/src/prompt_assembly/pg_builder.rs` — **Modify.** `PgSystemPromptBuilder` gains a `tool_docs` field + widened `new`; pass docs into `assemble_system_prompt`.
- `core/src/main.rs` — **Modify.** Destructure the 3-tuple; pass docs into `PgSystemPromptBuilder::new`.
- `prompts/agent_planner.md` — **Modify.** Remove prohibitions; point at `<tools>`.
- `core/tests/` — **Modify.** Extend an inner-loop / prompt e2e to assert `<tools>` presence (Task 6).

---

## Task 1: `ToolDoc` / `ToolParam` types + trait method

**Files:**
- Modify: `core/src/worker_manifest.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct ToolDoc {
      pub name: &'static str,
      pub method: &'static str,
      pub summary: &'static str,
      pub params: &'static [ToolParam],
  }
  pub struct ToolParam {
      pub name: &'static str,
      pub description: &'static str,
      pub required: bool,
  }
  // on trait WorkerManifest:
  fn tool_doc(&self) -> Option<ToolDoc> { None }
  ```

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block at the bottom of `core/src/worker_manifest.rs` (the module already has test helpers; if a `FakeManifest` exists reuse it, otherwise add this minimal one):

```rust
#[cfg(test)]
mod tool_doc_tests {
    use super::*;

    struct BareManifest;
    impl WorkerManifest for BareManifest {
        fn name(&self) -> &'static str { "bare" }
        fn resolve(&self, _ctx: &ResolveCtx<'_>) -> Resolution {
            Resolution::Disabled { detail: "n/a".into() }
        }
    }

    struct DocManifest;
    impl WorkerManifest for DocManifest {
        fn name(&self) -> &'static str { "documented" }
        fn resolve(&self, _ctx: &ResolveCtx<'_>) -> Resolution {
            Resolution::Disabled { detail: "n/a".into() }
        }
        fn tool_doc(&self) -> Option<ToolDoc> {
            Some(ToolDoc {
                name: "documented",
                method: "doc.run",
                summary: "does a thing",
                params: &[ToolParam { name: "q", description: "the query", required: true }],
            })
        }
    }

    #[test]
    fn default_tool_doc_is_none() {
        assert!(BareManifest.tool_doc().is_none());
    }

    #[test]
    fn overridden_tool_doc_carries_fields() {
        let d = DocManifest.tool_doc().expect("Some");
        assert_eq!(d.name, "documented");
        assert_eq!(d.method, "doc.run");
        assert_eq!(d.params.len(), 1);
        assert!(d.params[0].required);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib worker_manifest::tool_doc_tests 2>&1 | tail -20`
Expected: FAIL — `cannot find type ToolDoc` / `no method named tool_doc`.

- [ ] **Step 3: Write minimal implementation**

Add, immediately after the `WorkerManifest` trait's existing methods but inside the trait, the new defaulted method:

```rust
    /// Optional human/LLM-facing description used to advertise this tool to
    /// the planner in the `<tools>` prompt block. `None` (the default) ⇒ the
    /// tool is dispatchable but not advertised. Only collected for workers that
    /// reach `Resolution::Register`, so a disabled/misconfigured worker is
    /// never advertised. Static compiled-in text ⇒ trusted (no escaping at the
    /// render site).
    fn tool_doc(&self) -> Option<ToolDoc> {
        None
    }
```

And add the two structs near the top of the file, after the `use` lines:

```rust
/// A worker's planner-facing self-description (name + JSON-RPC method + params).
/// Rendered into the `<tools>` block by the prompt assembler. All-`'static`
/// so each worker declares it as a `const`-style literal.
pub struct ToolDoc {
    /// Tool name; MUST equal `WorkerManifest::name()` (drift-guarded by a test).
    pub name: &'static str,
    /// JSON-RPC method the planner emits for this tool (e.g. `"web.search"`).
    pub method: &'static str,
    /// One line: what the tool does and when to reach for it.
    pub summary: &'static str,
    /// Ordered parameter descriptions.
    pub params: &'static [ToolParam],
}

/// One parameter of a [`ToolDoc`].
pub struct ToolParam {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib worker_manifest::tool_doc_tests 2>&1 | tail -20`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add core/src/worker_manifest.rs
git commit -m "feat(agent): ToolDoc/ToolParam types + WorkerManifest::tool_doc()

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `render_tools_block` + `<tools>` in `assemble_system_prompt`

**Files:**
- Modify: `core/src/prompt_assembly/assemble.rs`
- Modify: `core/src/prompt_assembly/assemble/tests.rs`

**Interfaces:**
- Consumes: `crate::worker_manifest::{ToolDoc, ToolParam}` (Task 1).
- Produces: `assemble_system_prompt(l0, l1, skills, recalled, base, tools: &[ToolDoc]) -> String`.
  The `tools` param is appended LAST (after `base`) so every existing call site
  updates with a pure `, &[]` append. Render order is unchanged by param order:
  the block is emitted between `<recalled>` and `<handoff>`.

- [ ] **Step 1: Write the failing tests**

Add to `core/src/prompt_assembly/assemble/tests.rs`:

```rust
#[test]
fn tools_block_renders_between_recalled_and_handoff() {
    use crate::worker_manifest::{ToolDoc, ToolParam};
    let tools = [ToolDoc {
        name: "web-search",
        method: "web.search",
        summary: "Search the web.",
        params: &[
            ToolParam { name: "query", description: "the query", required: true },
            ToolParam { name: "count", description: "max results", required: false },
        ],
    }];
    let out = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE", &tools);
    assert!(out.contains("<tools>\n"), "block present: {out}");
    assert!(out.contains("- web-search (method: web.search): Search the web.\n"), "{out}");
    assert!(
        out.contains("  params: query (the query) [required], count (max results) [optional]\n"),
        "{out}"
    );
    // Ordering: <tools> after </recalled> region and before <handoff>.
    let tools_at = out.find("<tools>").expect("tools present");
    let handoff_at = out.find("<handoff>").expect("handoff present");
    assert!(tools_at < handoff_at, "tools must precede handoff: {out}");
}

#[test]
fn tools_block_omitted_when_empty() {
    let out = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE", &[]);
    assert!(!out.contains("<tools>"), "empty tools ⇒ no block: {out}");
}

#[test]
fn tool_with_no_params_omits_params_line() {
    use crate::worker_manifest::ToolDoc;
    let tools = [ToolDoc {
        name: "noparams",
        method: "np.run",
        summary: "No params.",
        params: &[],
    }];
    let out = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE", &tools);
    assert!(out.contains("- noparams (method: np.run): No params.\n"), "{out}");
    // The next line after the entry must be the closing tag, not a params line.
    assert!(out.contains("- noparams (method: np.run): No params.\n</tools>"), "{out}");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib prompt_assembly::assemble 2>&1 | tail -30`
Expected: FAIL — `assemble_system_prompt` takes 5 args not 6 (all new + existing calls mismatch).

- [ ] **Step 3: Implement**

In `core/src/prompt_assembly/assemble.rs`:

(a) Add the import at the top (with the other `use crate::` lines):
```rust
use crate::worker_manifest::ToolDoc;
```

(b) Add the render helper (place it next to `render_handoff_block`):
```rust
/// Render the `<tools>` block: one entry per advertised tool. Trusted
/// compiled-in text (authored in each worker's `tool_doc()`), so — unlike the
/// L1/recalled blocks — bodies are NOT escaped. Emitted only when non-empty.
fn render_tools_block(tools: &[ToolDoc]) -> String {
    let mut out = String::from("<tools>\n");
    for t in tools {
        out.push_str("- ");
        out.push_str(t.name);
        out.push_str(" (method: ");
        out.push_str(t.method);
        out.push_str("): ");
        out.push_str(t.summary);
        out.push('\n');
        if !t.params.is_empty() {
            out.push_str("  params: ");
            let rendered: Vec<String> = t
                .params
                .iter()
                .map(|p| {
                    format!(
                        "{} ({}) [{}]",
                        p.name,
                        p.description,
                        if p.required { "required" } else { "optional" }
                    )
                })
                .collect();
            out.push_str(&rendered.join(", "));
            out.push('\n');
        }
    }
    out.push_str("</tools>\n\n");
    out
}
```

(c) Widen the signature and render the block. Change:
```rust
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    skills: &[SurfacedSkill],
    recalled: &RecalledContext,
    base: &str,
) -> String {
```
to:
```rust
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    skills: &[SurfacedSkill],
    recalled: &RecalledContext,
    base: &str,
    tools: &[ToolDoc],
) -> String {
```
Then, immediately AFTER the `if !recalled.is_empty() { … }` block and BEFORE the `out.push_str(&render_handoff_block());` line, insert:
```rust
    // Advertised tools (trusted, compiled-in). Grouped with <handoff> as the
    // two capability-describing blocks; omitted entirely when nothing registered.
    if !tools.is_empty() {
        out.push_str(&render_tools_block(tools));
    }
```

(d) Update the module-doc framing comment at the top of the file: in the ASCII layout add a `<tools>` block between `<recalled>` and `<handoff>`, and note it is trusted/omitted-when-empty (mirror the `<skills>` wording).

- [ ] **Step 4: Update existing call sites in the tests file**

In `core/src/prompt_assembly/assemble/tests.rs`, every existing `assemble_system_prompt(...)` call currently passes 5 args. Append `, &[]` as the 6th argument to each. (There are ~20 such calls — update all; a call left at 5 args won't compile.)

- [ ] **Step 5: Run to verify pass**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib prompt_assembly::assemble 2>&1 | tail -30`
Expected: PASS (existing tests + 3 new).

Also confirm the whole crate still compiles — `pg_builder.rs` calls `assemble_system_prompt` and now needs the 6th arg:

Run: `source "$HOME/.cargo/env"; cargo build -p kastellan-core 2>&1 | tail -20`
Expected: it will FAIL here with a missing-arg error at `pg_builder.rs`. That's fixed in Task 5 — for THIS task, make `pg_builder.rs` compile by passing an empty slice temporarily: in `core/src/prompt_assembly/pg_builder.rs`, change the `assemble_system_prompt(&l0, &l1, &skills, recalled, base)` call to `assemble_system_prompt(&l0, &l1, &skills, recalled, base, &[])`. Re-run the build; expected PASS.

- [ ] **Step 6: Clippy + commit**

Run: `source "$HOME/.cargo/env"; cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -20`
Expected: clean.

```bash
git add core/src/prompt_assembly/assemble.rs core/src/prompt_assembly/assemble/tests.rs core/src/prompt_assembly/pg_builder.rs
git commit -m "feat(agent): render <tools> block in the prompt assembler

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Implement `tool_doc()` for all seven workers + drift guard

**Files:**
- Modify: `core/src/workers/shell_exec.rs`, `gliner_relex.rs`, `python_exec.rs`, `web_fetch.rs`, `web_search.rs`, `web_research.rs`, `browser_driver.rs`
- Modify: `core/src/registry_build.rs` (drift-guard test only; the collection change is Task 4)

**Interfaces:**
- Consumes: `ToolDoc`/`ToolParam` (Task 1), `WorkerManifest::tool_doc()` (Task 1).
- Produces: each of the seven `impl WorkerManifest` blocks gains a `tool_doc()` returning `Some(...)`.

Add `use crate::worker_manifest::{ToolDoc, ToolParam};` to each worker module if not already importing those names (they already `use crate::worker_manifest::...` for the trait — extend the import).

- [ ] **Step 1: Write the failing drift-guard + presence test**

Add to `core/src/registry_build.rs`'s `#[cfg(test)] mod tests`:

```rust
#[test]
fn every_registered_worker_docs_name_matches_registry_key() {
    // A ToolDoc's name must equal its manifest's name(), else the planner is
    // told a tool name it can't dispatch. Guards against copy-paste drift.
    for m in WORKER_MANIFESTS {
        if let Some(doc) = m.tool_doc() {
            assert_eq!(doc.name, m.name(), "tool_doc name drift for {}", m.name());
            assert!(!doc.method.is_empty(), "{} has empty method", m.name());
            assert!(!doc.summary.is_empty(), "{} has empty summary", m.name());
        }
    }
}

#[test]
fn core_web_and_shell_workers_advertise_a_tool_doc() {
    let by_name = |want: &str| {
        WORKER_MANIFESTS
            .iter()
            .find(|m| m.name() == want)
            .and_then(|m| m.tool_doc())
    };
    assert_eq!(by_name("web-search").expect("web-search doc").method, "web.search");
    assert_eq!(by_name("web-research").expect("web-research doc").method, "web.research");
    assert_eq!(by_name("web-fetch").expect("web-fetch doc").method, "web.fetch");
    assert_eq!(by_name("shell-exec").expect("shell-exec doc").method, "shell.exec");
    assert_eq!(by_name("python-exec").expect("python-exec doc").method, "python.exec");
    assert_eq!(by_name("browser-driver").expect("browser-driver doc").method, "browser.render");
    assert_eq!(by_name("gliner-relex").expect("gliner-relex doc").method, "extract");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib registry_build 2>&1 | tail -20`
Expected: FAIL — `core_web_and_shell_workers_advertise_a_tool_doc` panics on the first `.expect(...)` (no worker overrides `tool_doc()` yet).

- [ ] **Step 3: Implement `tool_doc()` in each worker**

Add each of these inside the corresponding `impl WorkerManifest for ...Manifest` block. Params are taken from each worker's live handler (verified against the source).

`core/src/workers/shell_exec.rs`:
```rust
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "shell.exec",
            summary: "Run one allowlisted command and capture stdout/stderr/exit code. \
                      No shell interpretation; argv[0] MUST be an absolute path.",
            params: &[ToolParam {
                name: "argv",
                description: "command and arguments as a JSON array; argv[0] an absolute path",
                required: true,
            }],
        })
    }
```

`core/src/workers/web_search.rs`:
```rust
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "web.search",
            summary: "Search the web via a SearxNG backend; returns a list of result \
                      titles, URLs, and snippets. Use for questions needing current or \
                      external facts.",
            params: &[
                ToolParam { name: "query", description: "the search query", required: true },
                ToolParam { name: "count", description: "max results, default 10 (cap 20)", required: false },
            ],
        })
    }
```

`core/src/workers/web_research.rs`:
```rust
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "web.research",
            summary: "Search the web, fetch the top results, and return the passages \
                      most relevant to the query (ranked). Prefer this over web.search \
                      when you need the answer content, not just links.",
            params: &[
                ToolParam { name: "query", description: "the research question", required: true },
                ToolParam { name: "max_sources", description: "max pages to fetch (optional)", required: false },
                ToolParam { name: "max_passages", description: "max ranked passages to return (optional)", required: false },
            ],
        })
    }
```

`core/src/workers/web_fetch.rs`:
```rust
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "web.fetch",
            summary: "Fetch one known https URL and return its readable extracted text. \
                      Use when you already have the exact URL; use web.search/web.research \
                      to discover URLs.",
            params: &[ToolParam {
                name: "url",
                description: "absolute https URL (http allowed only for loopback)",
                required: true,
            }],
        })
    }
```

`core/src/workers/python_exec.rs`:
```rust
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "python.exec",
            summary: "Execute a short Python program in a sandboxed interpreter and \
                      capture stdout/stderr/exit code.",
            params: &[
                ToolParam { name: "code", description: "the Python source to run", required: true },
                ToolParam { name: "params", description: "optional JSON object exposed to the program", required: false },
            ],
        })
    }
```

`core/src/workers/browser_driver.rs`:
```rust
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "browser.render",
            summary: "Render a URL in a headless browser (executes JavaScript) and return \
                      the resulting page text. Use for pages that need JS; web.fetch is \
                      cheaper for static pages.",
            params: &[
                ToolParam { name: "url", description: "absolute https URL to render", required: true },
                ToolParam { name: "timeout_ms", description: "render timeout in ms (optional, clamped)", required: false },
                ToolParam { name: "wait_until", description: "load condition, e.g. \"networkidle\" (optional)", required: false },
            ],
        })
    }
```

`core/src/workers/gliner_relex.rs`:
```rust
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "extract",
            summary: "Extract named entities and relations from text against caller-supplied \
                      label sets (zero-shot GLiNER).",
            params: &[
                ToolParam { name: "text", description: "the text to analyse", required: true },
                ToolParam { name: "entity_labels", description: "non-empty array of entity types to find", required: true },
                ToolParam { name: "relation_labels", description: "array of relation types (may be empty)", required: true },
                ToolParam { name: "threshold", description: "entity confidence in [0,1], default 0.5 (optional)", required: false },
                ToolParam { name: "relation_threshold", description: "relation confidence in [0,1] (optional)", required: false },
            ],
        })
    }
```

**Note on `TOOL_NAME`:** each worker module defines a `TOOL_NAME` const used by `fn name()`. Confirm the const exists in each file (grep `TOOL_NAME`); shell-exec/web-search/web-fetch/web-research/python-exec/browser-driver use it. If any worker returns a string literal from `name()` instead of a const, use that same literal in the `ToolDoc.name` (the drift test enforces equality).

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib registry_build 2>&1 | tail -20`
Expected: PASS (drift guard + presence test green).

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env"; cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -20`
Expected: clean.

```bash
git add core/src/workers/shell_exec.rs core/src/workers/web_search.rs core/src/workers/web_research.rs core/src/workers/web_fetch.rs core/src/workers/python_exec.rs core/src/workers/browser_driver.rs core/src/workers/gliner_relex.rs core/src/registry_build.rs
git commit -m "feat(agent): each worker advertises a ToolDoc + registry drift guard

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Collect docs in the registry builder

**Files:**
- Modify: `core/src/registry_build.rs`
- Modify: `core/src/main.rs` (destructure the new tuple element)

**Interfaces:**
- Consumes: `WorkerManifest::tool_doc()` (Task 1), the seven overrides (Task 3).
- Produces:
  - `assemble_registry(manifests, ctx) -> (ToolRegistry, Vec<LoadedToolRecord>, Vec<ToolDoc>)`
  - `build_tool_registry(pool, exe_dir) -> Result<(ToolRegistry, Vec<LoadedToolRecord>, Vec<ToolDoc>), DbError>`

- [ ] **Step 1: Write the failing test**

Add to `core/src/registry_build.rs`'s test module:

```rust
#[test]
fn assemble_collects_docs_only_for_registered_tools() {
    // Register a real worker (shell-exec has a ToolDoc) via the exe-sibling path,
    // plus a Disabled fake with no doc. Only the registered one's doc appears.
    let exe_dir = PathBuf::from("/install/bin");
    let sibling = exe_dir.join("kastellan-worker-shell-exec");
    let get_env = |_k: &str| None;
    let exists = { let s = sibling.clone(); move |p: &Path| p == s.as_path() };
    let allowlist = |_t: &str| Vec::new();
    let ctx = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &|_p: &Path| false,
        exe_dir: Some(exe_dir.as_path()),
        canonicalize: &|_p| None,
        allowlist: &allowlist,
    };
    let (_reg, _loaded, docs) = assemble_registry(WORKER_MANIFESTS, &ctx);
    // gliner/browser/python/web-* are Disabled (no enable flag / no endpoint) in
    // this ctx; shell-exec registers via the sibling and contributes its doc.
    assert!(docs.iter().any(|d| d.name == "shell-exec"), "shell-exec doc collected");
    assert!(!docs.iter().any(|d| d.name == "web-search"), "disabled web-search not advertised");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib registry_build::tests::assemble_collects_docs 2>&1 | tail -20`
Expected: FAIL — `assemble_registry` returns a 2-tuple; pattern `(_, _, docs)` mismatches / method returns wrong arity.

- [ ] **Step 3: Implement**

In `core/src/registry_build.rs`:

(a) Add `use crate::worker_manifest::ToolDoc;` to the imports at the top (extend the existing `use crate::worker_manifest::{...}` line).

(b) Change `assemble_registry`'s signature return type to `(ToolRegistry, Vec<LoadedToolRecord>, Vec<ToolDoc>)`. Inside, add `let mut docs: Vec<ToolDoc> = Vec::new();` next to `let mut loaded`. In the `Resolution::Register(entry)` arm, after `reg.insert(name, entry);`, add:
```rust
                if let Some(doc) = m.tool_doc() {
                    docs.push(doc);
                }
```
Change the final `(reg, loaded)` to `(reg, loaded, docs)`.

(c) In `build_tool_registry`, change the return type to
`Result<(ToolRegistry, Vec<LoadedToolRecord>, Vec<ToolDoc>), kastellan_db::DbError>`
and the final line stays `Ok(assemble_registry(WORKER_MANIFESTS, &ctx))` (now a 3-tuple).

(d) Update the four existing in-file tests that destructure `let (reg, loaded) = assemble_registry(...)` → `let (reg, loaded, _docs) = assemble_registry(...)` (lines ~249, 275, 316, 342).

(e) In `core/src/main.rs`, change the destructure at line ~176:
```rust
    let (registry, loaded_tool_records) =
        kastellan_core::registry_build::build_tool_registry(&pool, exe_dir).await?;
```
to:
```rust
    let (registry, loaded_tool_records, tool_docs) =
        kastellan_core::registry_build::build_tool_registry(&pool, exe_dir).await?;
    let tool_docs = std::sync::Arc::<[kastellan_core::worker_manifest::ToolDoc]>::from(tool_docs);
```
(`tool_docs` is unused until Task 5 — add `#[allow(unused_variables)]`? No: instead, prefix it and use it in Task 5. To keep THIS task's build clean without a warning, temporarily bind `let _tool_docs = ...` WITHOUT the Arc line, then Task 5 replaces it. Simplest: in this task write `let (registry, loaded_tool_records, _tool_docs) = ...` and defer the Arc + naming to Task 5.)

Use the `_tool_docs` form in this task:
```rust
    let (registry, loaded_tool_records, _tool_docs) =
        kastellan_core::registry_build::build_tool_registry(&pool, exe_dir).await?;
```

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib registry_build 2>&1 | tail -20`
Expected: PASS.

Run: `source "$HOME/.cargo/env"; cargo build -p kastellan-core 2>&1 | tail -20`
Expected: PASS (main.rs destructures the 3-tuple).

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env"; cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -20`
Expected: clean.

```bash
git add core/src/registry_build.rs core/src/main.rs
git commit -m "feat(agent): collect ToolDocs for registered tools in the registry builder

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Wire docs into `PgSystemPromptBuilder` + `main.rs`

**Files:**
- Modify: `core/src/prompt_assembly/pg_builder.rs`
- Modify: `core/src/main.rs`

**Interfaces:**
- Consumes: `Vec<ToolDoc>`/`Arc<[ToolDoc]>` from `build_tool_registry` (Task 4).
- Produces: `PgSystemPromptBuilder::new(pool: PgPool, tool_docs: Arc<[ToolDoc]>)`.

> Design note: the docs live on `PgSystemPromptBuilder` (the type that calls
> `assemble_system_prompt`), not on `RouterAgent` as the spec sketch suggested —
> tighter boundary, and `RouterAgent` needs no change. `StaticSystemPromptBuilder`
> is unaffected (it never assembles).

- [ ] **Step 1: Write the failing test**

Add to `core/src/prompt_assembly/pg_builder.rs`'s `#[cfg(test)] mod tests` a construction/plumbing test that does not need PG — assert the builder stores the docs via a small accessor. Add a `#[cfg(test)]` accessor and the test:

```rust
    #[test]
    fn pg_builder_retains_tool_docs() {
        use crate::worker_manifest::ToolDoc;
        let docs: std::sync::Arc<[ToolDoc]> = std::sync::Arc::from(vec![ToolDoc {
            name: "web-search",
            method: "web.search",
            summary: "s",
            params: &[],
        }]);
        // A dummy pool isn't needed: test the doc field via the test-only ctor.
        let b = PgSystemPromptBuilder::from_docs_for_test(docs.clone());
        assert_eq!(b.tool_docs_for_test().len(), 1);
        assert_eq!(b.tool_docs_for_test()[0].name, "web-search");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib prompt_assembly::pg_builder 2>&1 | tail -20`
Expected: FAIL — `from_docs_for_test` / `tool_docs_for_test` not found.

- [ ] **Step 3: Implement**

In `core/src/prompt_assembly/pg_builder.rs`:

(a) Add the import: `use crate::worker_manifest::ToolDoc;` and `use std::sync::Arc;`.

(b) Add the field + widen `new`:
```rust
pub struct PgSystemPromptBuilder {
    pool: PgPool,
    tool_docs: Arc<[ToolDoc]>,
}

impl PgSystemPromptBuilder {
    /// Construct a builder pinned to the supplied pool and advertised tool set.
    pub fn new(pool: PgPool, tool_docs: Arc<[ToolDoc]>) -> Self {
        Self { pool, tool_docs }
    }

    #[cfg(test)]
    fn from_docs_for_test(tool_docs: Arc<[ToolDoc]>) -> Self {
        // A lazily-connected pool that is never queried in this unit test.
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        Self { pool, tool_docs }
    }

    #[cfg(test)]
    fn tool_docs_for_test(&self) -> &[ToolDoc] {
        &self.tool_docs
    }
}
```

(c) In `build_with_recalled`, pass the docs to the assembler. Change:
```rust
        let system_prompt = assemble_system_prompt(&l0, &l1, &skills, recalled, base, &[]);
```
(the temporary `&[]` added in Task 2) to:
```rust
        let system_prompt =
            assemble_system_prompt(&l0, &l1, &skills, recalled, base, &self.tool_docs);
```

(d) In `core/src/main.rs`, replace the Task-4 placeholder. Change:
```rust
    let (registry, loaded_tool_records, _tool_docs) =
        kastellan_core::registry_build::build_tool_registry(&pool, exe_dir).await?;
```
to:
```rust
    let (registry, loaded_tool_records, tool_docs) =
        kastellan_core::registry_build::build_tool_registry(&pool, exe_dir).await?;
    let tool_docs: std::sync::Arc<[kastellan_core::worker_manifest::ToolDoc]> =
        std::sync::Arc::from(tool_docs);
```
and change the builder construction (line ~316):
```rust
            Arc::new(kastellan_core::prompt_assembly::PgSystemPromptBuilder::new(pool.clone())),
```
to:
```rust
            Arc::new(kastellan_core::prompt_assembly::PgSystemPromptBuilder::new(
                pool.clone(),
                tool_docs.clone(),
            )),
```

(e) `main.rs` resolves `kastellan_core::worker_manifest::ToolDoc` because `worker_manifest` is `pub mod` (`core/src/lib.rs:34`) and `ToolDoc` is `pub` (Task 1). No re-export needed. `PgSystemPromptBuilder` is already re-exported at `kastellan_core::prompt_assembly::PgSystemPromptBuilder` (`core/src/prompt_assembly/mod.rs:41`).

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib prompt_assembly::pg_builder 2>&1 | tail -20`
Expected: PASS.

Run: `source "$HOME/.cargo/env"; cargo build -p kastellan-core 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env"; cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -20`
Expected: clean.

```bash
git add core/src/prompt_assembly/pg_builder.rs core/src/main.rs
git commit -m "feat(agent): thread advertised ToolDocs into the production prompt builder

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Update `agent_planner.md` + integration assertion

**Files:**
- Modify: `prompts/agent_planner.md`
- Modify: an existing prompt/inner-loop integration test (see Step 3)

**Interfaces:**
- Consumes: the `<tools>` block now rendered in production (Task 5).

- [ ] **Step 1: Write the failing test**

The cleanest end-to-end assertion that doesn't need a live LLM is to render the
production prompt via `PgSystemPromptBuilder` against a per-test PG cluster with a
non-empty tool set and assert the `<tools>` block appears. If a prompt-assembly
integration test with a PG fixture already exists (search
`core/tests/` for `PgSystemPromptBuilder`), extend it. Otherwise add a focused
pure test that exercises the same rendering path through `assemble_system_prompt`
with a realistic `web.search` doc and asserts the planner would see it:

Add to `core/src/prompt_assembly/assemble/tests.rs`:
```rust
#[test]
fn web_search_doc_reaches_assembled_prompt() {
    use crate::worker_manifest::{ToolDoc, ToolParam};
    let tools = [ToolDoc {
        name: "web-search",
        method: "web.search",
        summary: "Search the web via a SearxNG backend.",
        params: &[ToolParam { name: "query", description: "the search query", required: true }],
    }];
    let out = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE", &tools);
    assert!(out.contains("<tools>"), "planner prompt advertises tools: {out}");
    assert!(out.contains("web.search"), "web.search advertised: {out}");
}
```

- [ ] **Step 2: Run to verify it fails, then passes**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib prompt_assembly::assemble::tests::web_search_doc_reaches_assembled_prompt 2>&1 | tail -20`
Expected: PASS already (the render path exists from Task 2). This test is a regression pin; if it fails, the render path regressed — fix before continuing.

- [ ] **Step 3: Edit `prompts/agent_planner.md`**

(a) Line ~162 — remove the "or search the web" prohibition. The sentence currently reads (approximately): *"…compute it directly; do not shell out to compute it or search the web."* Change it to end at the legitimate point, e.g.: *"…compute it directly; do not shell out to compute something you can determine yourself."* (Keep the "don't shell out for trivial arithmetic" intent; drop the blanket web ban.)

(b) Lines ~168–172 — replace the "Only the tools described to you exist … there is no `google_search`, no web-search tool unless one is listed" paragraph with:
```markdown
The tools available to you are listed in the `<tools>` block above. Only those
tools exist — never invent one that is not listed. Each entry gives the tool
name, its JSON-RPC `method`, and its parameters; emit steps using exactly those
names, methods, and parameter shapes. If you need a capability and no listed
tool provides it, say so in a terminal plan rather than inventing a tool. A step
naming an unlisted tool returns `err: UNKNOWN_TOOL`.
```

(c) Keep the step-JSON schema, the `handoff`/`fetch_handoff` guidance, the "read the ok/err head and answer" paragraph, and the shell-exec absolute-path rule unchanged.

- [ ] **Step 4: Verify the prompt still parses / crate builds**

The prompt is loaded as text at runtime; there's nothing to compile, but run the prompt-cache / planner tests to be safe:

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib prompt 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Full crate test + clippy + commit**

Run: `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib 2>&1 | tail -20`
Expected: PASS (whole lib).

Run: `source "$HOME/.cargo/env"; cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -20`
Expected: clean.

```bash
git add prompts/agent_planner.md core/src/prompt_assembly/assemble/tests.rs
git commit -m "feat(agent): advertise <tools> in agent_planner.md; drop no-web-search ban

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (controller, after all tasks)

- [ ] `source "$HOME/.cargo/env"; cargo build --workspace 2>&1 | tail -20` — exit 0.
- [ ] `source "$HOME/.cargo/env"; cargo test -p kastellan-core --lib 2>&1 | tail -20` — green.
- [ ] `source "$HOME/.cargo/env"; cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20` — clean.
- [ ] Manual sanity: temporarily point `KASTELLAN_WEB_SEARCH_ENDPOINT` at the dev SearxNG (`scripts/web-search/setup-searxng.sh`), add `127.0.0.1` to the `web-search` allowlist, boot the daemon, and confirm the `registry.loaded` log lists `web-search` — then the `<tools>` block will carry it. (Optional; not a blocking gate for the code slice.)
- [ ] Update `docs/devel/handovers/HANDOVER.md` + `ROADMAP.md` per the session-end checklist.

## Notes carried from the spec

- **This slice is advertisement only.** Answering a live web question additionally
  needs a reachable SearxNG + `KASTELLAN_WEB_SEARCH_ENDPOINT` + the `tool_allowlists`
  row, and the Matrix worker built `--features live-matrix` with homeserver env.
  Absent those, `web-search` won't register and — correctly — won't be advertised.
- No dispatch / allowlist / reviewer change. No new dependencies. Cross-platform neutral.
