# Teach the planner to call `fetch_handoff` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface the `fetch_handoff` protocol in the assembled system prompt so the planner can expand an oversized tool result that the dispatcher stashed.

**Architecture:** One pure helper `render_handoff_block()` in `core/src/prompt_assembly/assemble.rs` emits an always-present `<handoff>` block, positioned between `<recalled>` and `<base>`. The block interpolates the source-of-truth constants `HANDOFF_TOOL` / `HANDOFF_METHOD_FETCH` so the instruction and the mechanism cannot drift. Pure-function change; unit tests only.

**Tech Stack:** Rust (kastellan-core crate), `cargo test`, `cargo clippy`.

**Spec:** [`docs/superpowers/specs/2026-06-09-teach-planner-fetch-handoff-design.md`](../specs/2026-06-09-teach-planner-fetch-handoff-design.md)

---

## Background the engineer needs

- `core/src/prompt_assembly/assemble.rs` holds `assemble_system_prompt(l0, l1, skills, recalled, base) -> String`, a **pure** function. It currently emits, in order, optional `<l0_meta_rules>`, `<l1_insights>`, `<skills>`, `<recalled>` blocks, then an always-present `<base>` block. Each block is separated by one blank line. Several tests assert byte-exact output.
- The handoff mechanism (PR #199): when a tool result's serialized JSON exceeds 64 KiB, the dispatcher stashes it and the planner instead sees a placeholder `{handoff_ref, byte_len, summary_head, truncated: true}`. The planner can fetch slices back via a reserved built-in step `tool="handoff" method="fetch" parameters={handoff_ref, offset?, len?}`, which returns `{handoff_ref, offset, len, data, encoding, eof}`.
- Source-of-truth constants (already `pub`): `HANDOFF_TOOL = "handoff"` and `HANDOFF_METHOD_FETCH = "fetch"` in `core/src/scheduler/tool_dispatch.rs`. The placeholder builder is `crate::handoff::build_handoff_placeholder(value, &HandoffRef, byte_len)`; `crate::handoff::HandoffRef::of(bytes)` makes a ref.
- **Run cargo from a sourced env:** every `cargo` command must be preceded by `source "$HOME/.cargo/env"` (cargo is not on the non-interactive PATH).

## File Structure

- **Modify:** `core/src/prompt_assembly/assemble.rs` — add `render_handoff_block()`, call it unconditionally in `assemble_system_prompt`, update the module docstring framing, add new tests, update four deliberately-broken byte-exact pins.
- **Possibly create:** `core/src/prompt_assembly/assemble/tests.rs` — only if the file exceeds 500 LOC after the change (Task 4). Production code stays byte-identical across the lift.

---

### Task 1: New failing tests for the `<handoff>` block

**Files:**
- Modify/Test: `core/src/prompt_assembly/assemble.rs` (inside `#[cfg(test)] mod tests`)

- [ ] **Step 1: Add the new tests at the end of `mod tests` (before its closing `}`)**

```rust
    #[test]
    fn handoff_block_present_even_when_all_layers_empty() {
        let out = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE");
        assert!(
            out.contains("<handoff>\n") && out.contains("</handoff>\n\n"),
            "handoff block must always be present; got:\n{out}"
        );
    }

    #[test]
    fn handoff_block_sits_after_recalled_and_before_base() {
        let recalled = RecalledContext::new(
            vec![100],
            vec!["RECALL ONE".into()],
            "f".repeat(64),
        );
        let out = assemble_system_prompt(&[], &[], &[], &recalled, "BASE");
        let recalled_end = out.find("</recalled>").expect("recalled end tag");
        let handoff_start = out.find("<handoff>").expect("handoff start tag");
        let handoff_end = out.find("</handoff>").expect("handoff end tag");
        let base_start = out.find("<base>").expect("base start tag");
        assert!(recalled_end < handoff_start, "handoff must follow recalled; got:\n{out}");
        assert!(handoff_end < base_start, "handoff must precede base; got:\n{out}");
    }

    #[test]
    fn handoff_block_names_the_real_protocol_tokens() {
        // Drift guard: the instruction must reference the actual built-in
        // tool/method constants and every field the mechanism emits, so a
        // change to those shapes fails this test instead of silently leaving
        // the planner with a stale protocol description.
        use crate::handoff::{build_handoff_placeholder, HandoffRef};
        use crate::scheduler::tool_dispatch::{HANDOFF_METHOD_FETCH, HANDOFF_TOOL};

        let out = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE");

        // Tool + method come from their source-of-truth constants.
        assert!(
            out.contains(&format!("tool=\"{HANDOFF_TOOL}\"")),
            "block must show the real step form tool=\"{HANDOFF_TOOL}\"; got:\n{out}"
        );
        assert!(
            out.contains(&format!("method=\"{HANDOFF_METHOD_FETCH}\"")),
            "block must show the real step form method=\"{HANDOFF_METHOD_FETCH}\"; got:\n{out}"
        );

        // Every field the placeholder actually carries must be named.
        let placeholder =
            build_handoff_placeholder(&serde_json::json!({ "k": "v" }), &HandoffRef::of(b"x"), 99);
        for key in placeholder.as_object().expect("placeholder is a JSON object").keys() {
            assert!(
                out.contains(key.as_str()),
                "handoff block must name placeholder field {key:?}; got:\n{out}"
            );
        }

        // Fetch params the planner can set.
        for param in ["offset", "len"] {
            assert!(
                out.contains(param),
                "handoff block must name fetch param {param:?}; got:\n{out}"
            );
        }
    }

    #[test]
    fn handoff_block_is_deterministic() {
        let a = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE");
        let b = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE");
        assert_eq!(a, b, "same inputs must yield identical bytes");
    }
```

- [ ] **Step 2: Run the new tests, verify they fail (block doesn't exist yet)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly::assemble::tests::handoff_ 2>&1 | tail -20`
Expected: FAIL — `handoff_block_present_even_when_all_layers_empty` and the others fail their `assert!`/`find` on the missing `<handoff>` tag. (Compilation succeeds — they only reference existing public items.)

- [ ] **Step 3: Commit the failing tests**

```bash
git add core/src/prompt_assembly/assemble.rs
git commit -m "test(prompt): failing tests for the <handoff> protocol block"
```

---

### Task 2: Implement `render_handoff_block` and wire it in

**Files:**
- Modify: `core/src/prompt_assembly/assemble.rs`

- [ ] **Step 1: Add the import and the helper above `assemble_system_prompt`**

Add to the `use` block near the top (after the existing `use` lines, around line 79):

```rust
use crate::scheduler::tool_dispatch::{HANDOFF_METHOD_FETCH, HANDOFF_TOOL};
```

Add this helper immediately before `pub fn assemble_system_prompt` (around line 89):

```rust
/// Render the always-present `<handoff>` block.
///
/// Teaches the planner the `fetch_handoff` protocol: how to recognise the
/// placeholder the dispatcher leaves in place of an oversized tool result, and
/// how to pull the full body back in slices. Compiled-in next to the mechanism
/// ([`crate::handoff`] / [`crate::scheduler::tool_dispatch`]); the tool and
/// method names come from their source-of-truth constants so the instruction
/// cannot drift from the code that serves it. Constant text — no empty state —
/// so unlike the memory-layer blocks it is emitted unconditionally.
fn render_handoff_block() -> String {
    format!(
        "<handoff>\n\
         Some tool results are too large for the context window. When a tool \
         result is the placeholder object {{handoff_ref, byte_len, summary_head, \
         truncated: true}}, the full output was stashed and only summary_head — \
         the readable first ~1 KiB — is shown inline. That head is often enough \
         to proceed without fetching anything more.\n\
         To read more of the body, emit a step with tool=\"{tool}\" \
         method=\"{method}\" and parameters={{handoff_ref, offset?, len?}} \
         (offset defaults to 0; len defaults to and is clamped at 256 KiB). The \
         step returns {{handoff_ref, offset, len, data, encoding, eof}}, where \
         len is the number of bytes actually returned. To read the whole body, \
         repeat the fetch with offset increased by len until eof is true.\n\
         </handoff>\n\n",
        tool = HANDOFF_TOOL,
        method = HANDOFF_METHOD_FETCH,
    )
}
```

- [ ] **Step 2: Call the helper unconditionally, between the recalled block and the base block**

In `assemble_system_prompt`, the recalled block ends around line 135 and the base block begins at `out.push_str("<base>\n");` (around line 137). Insert the handoff call between them:

```rust
    if !recalled.is_empty() {
        out.push_str("<recalled>\n");
        for body in &recalled.bodies {
            out.push_str("- ");
            out.push_str(body);
            out.push('\n');
        }
        out.push_str("</recalled>\n\n");
    }

    // Always-present capability instruction for the reserved `handoff` built-in.
    // Compiled-in trusted text; sits flush before the base operating prompt so
    // `<base>` stays the terminal block.
    out.push_str(&render_handoff_block());

    out.push_str("<base>\n");
```

- [ ] **Step 3: Update the module docstring framing to include `<handoff>`**

In the top-of-file `//!` doc, change the order sentence (line 2) from:

```rust
//! Output framing (always L0 → L1 → skills → recalled → base in this order):
```

to:

```rust
//! Output framing (always L0 → L1 → skills → recalled → handoff → base in this order):
```

And add the `<handoff>` block to the ASCII framing diagram, immediately before the `<base>` block (after the `</recalled>` example, around line 24):

```rust
//! <handoff>
//! {protocol for expanding a stashed oversized tool result via the
//!  reserved `handoff`/`fetch` built-in — always present}
//! </handoff>
//!
```

Update framing rule 1 (around line 32) to note the handoff exception. Change:

```rust
//! 1. Empty layers omit their entire tag block — no `<l1_insights>`
//!    when L1 has zero rows. The `<base>` block is always present.
```

to:

```rust
//! 1. Empty layers omit their entire tag block — no `<l1_insights>`
//!    when L1 has zero rows. The `<handoff>` and `<base>` blocks are
//!    always present (`<handoff>` is constant compiled-in text).
```

- [ ] **Step 4: Run the new tests, verify they now pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly::assemble::tests::handoff_ 2>&1 | tail -20`
Expected: PASS — all four `handoff_*` tests green.

- [ ] **Step 5: Commit the implementation**

```bash
git add core/src/prompt_assembly/assemble.rs
git commit -m "feat(prompt): surface the fetch_handoff protocol to the planner (ROADMAP:129)"
```

---

### Task 3: Update the four deliberately-broken byte-exact pins

The always-present `<handoff>` block changes four existing tests that asserted "empty everything → `<base>` alone" or pinned a full byte-exact shape. Update them to expect the handoff block, reusing `render_handoff_block()` so the expectation stays DRY and self-updating.

**Files:**
- Modify/Test: `core/src/prompt_assembly/assemble.rs`

- [ ] **Step 1: Confirm exactly these four tests now fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly 2>&1 | tail -30`
Expected: FAIL in `empty_l0_l1_recalled_emits_base_block_only`, `skills_block_absent_when_empty_is_byte_identical`, `both_layers_assembled_in_order_with_blank_separators`, `base_trailing_newlines_are_normalized_to_exactly_one`. All other `prompt_assembly` tests (including the Task 1 four) pass.

- [ ] **Step 2: Update `empty_l0_l1_recalled_emits_base_block_only`**

Replace the assertion body. The test becomes:

```rust
    #[test]
    fn empty_l0_l1_recalled_emits_handoff_then_base() {
        let out = assemble_system_prompt(
            &[],
            &[],
            &[],
            &RecalledContext::empty(),
            "BASE BODY",
        );
        assert_eq!(
            out,
            format!("{}<base>\nBASE BODY\n</base>\n", render_handoff_block()),
            "no L0/L1/recalled → handoff block then base; got:\n{out}"
        );
    }
```

- [ ] **Step 3: Update `skills_block_absent_when_empty_is_byte_identical`**

```rust
    #[test]
    fn skills_block_absent_when_empty() {
        let out = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE");
        assert!(!out.contains("<skills>"), "no skills → no <skills> tag; got:\n{out}");
        assert_eq!(
            out,
            format!("{}<base>\nBASE\n</base>\n", render_handoff_block()),
            "empty everything → handoff block then base; got:\n{out}"
        );
    }
```

- [ ] **Step 4: Update `both_layers_assembled_in_order_with_blank_separators`**

Replace its `expected`/assert with a `format!` that splices the handoff block before base:

```rust
    #[test]
    fn both_layers_assembled_in_order_with_blank_separators() {
        let l0 = vec![mem(1, "rule one", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "insight one", MemoryLayer::Index)];
        let out = assemble_system_prompt(&l0, &l1, &[], &RecalledContext::empty(), "BASE");
        let expected = format!(
            concat!(
                "<l0_meta_rules>\n",
                "- rule one\n",
                "</l0_meta_rules>\n",
                "\n",
                "<l1_insights>\n",
                "- insight one\n",
                "</l1_insights>\n",
                "\n",
                "{handoff}",
                "<base>\n",
                "BASE\n",
                "</base>\n",
            ),
            handoff = render_handoff_block(),
        );
        assert_eq!(out, expected, "full shape pin");
    }
```

- [ ] **Step 5: Update `base_trailing_newlines_are_normalized_to_exactly_one`**

Each expected string now carries the handoff block prefix. Replace the four `assert_eq!` calls' expected values with `format!`:

```rust
        assert_eq!(
            out_no_nl,
            format!("{}<base>\nno trailing nl\n</base>\n", render_handoff_block()),
            "no-trailing-newline input must be normalized; got {out_no_nl:?}"
        );
        assert_eq!(
            out_one_nl,
            format!("{}<base>\nwith trailing nl\n</base>\n", render_handoff_block()),
            "single-trailing-newline input passes through; got {out_one_nl:?}"
        );
        assert_eq!(
            out_two_nl,
            format!("{}<base>\nwith two trailing nl\n</base>\n", render_handoff_block()),
            "two trailing newlines must collapse to one (no blank line before close tag); got {out_two_nl:?}"
        );
        assert_eq!(
            out_many_nl,
            format!("{}<base>\nmany trailing nls\n</base>\n", render_handoff_block()),
            "many trailing newlines must collapse to one; got {out_many_nl:?}"
        );
```

- [ ] **Step 6: Run the whole prompt_assembly suite, verify all green**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly 2>&1 | tail -20`
Expected: PASS — every `prompt_assembly` test green (the four updated pins + the four new handoff tests + all untouched ones).

- [ ] **Step 7: Commit the pin updates**

```bash
git add core/src/prompt_assembly/assemble.rs
git commit -m "test(prompt): update byte-exact pins for the always-present <handoff> block"
```

---

### Task 4: File-size check + conditional test-lift

**Files:**
- Possibly create: `core/src/prompt_assembly/assemble/tests.rs`
- Possibly modify: `core/src/prompt_assembly/assemble.rs`

- [ ] **Step 1: Measure the file**

Run: `wc -l core/src/prompt_assembly/assemble.rs`
Expected: a line count. If **≤ 500**, this task is a no-op — skip to Step 4 and record "under cap, no lift". If **> 500**, do Steps 2–3.

- [ ] **Step 2 (only if > 500): Lift the test module to a sibling**

Move the entire `#[cfg(test)] mod tests { ... }` block (everything from `#[cfg(test)]` to its matching closing `}` at end of file) out of `assemble.rs` into a new file `core/src/prompt_assembly/assemble/tests.rs` — paste the **inner** contents of the module (i.e. everything between the module's `{` and `}`, not the `mod tests {` wrapper itself). In `assemble.rs`, replace the lifted block with the declaration:

```rust
#[cfg(test)]
mod tests;
```

The lifted `tests.rs` begins with the same `use super::*;` and other `use` lines the module already had — they resolve identically from the child module, including the parent-private `render_handoff_block` and `mem` helper. No production lines change.

- [ ] **Step 3 (only if lifted): Verify the suite still passes from the new location**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly 2>&1 | tail -20`
Expected: PASS — identical test set, now compiled from `assemble/tests.rs`.

- [ ] **Step 4: Commit (only if a lift happened)**

```bash
git add core/src/prompt_assembly/assemble.rs core/src/prompt_assembly/assemble/tests.rs
git commit -m "refactor(prompt): lift assemble.rs test module to a sibling (under 500-LOC cap)"
```

---

### Task 5: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Clippy with warnings-as-errors**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -15`
Expected: exit 0, no warnings.

- [ ] **Step 2: Full workspace build**

Run: `source "$HOME/.cargo/env" && cargo build --workspace 2>&1 | tail -5`
Expected: `Finished` — clean build of all crates.

- [ ] **Step 3: Full core lib test count**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib 2>&1 | tail -5`
Expected: PASS — the prior baseline (715) **+4** new handoff tests = **719 / 0 / 0** (the four updated pins are modified-in-place, not added, so they don't change the count).

- [ ] **Step 4: Record the result** — note the final lib test count and whether the test-lift fired, for the HANDOVER update.

---

## Self-review notes

- **Spec coverage:** unconditional `<handoff>` block (Task 2) ✓; placement recalled→handoff→base (Task 1 position test + Task 2 Step 2) ✓; drift-proof via real constants + field-name cross-check (Task 1 drift test + Task 2 helper) ✓; deliberate update of the four pins (Task 3) ✓; docstring framing update (Task 2 Step 3) ✓; file-size lift contingency (Task 4) ✓; verification commands (Task 5) ✓; no `agent_planner.md` edit, no e2e (out of scope — honoured) ✓.
- **Type consistency:** `render_handoff_block()` defined in Task 2, referenced in Tasks 3–4 with the same signature/name throughout. `HANDOFF_TOOL` / `HANDOFF_METHOD_FETCH` / `build_handoff_placeholder` / `HandoffRef::of` match the source files.
- **No placeholders:** every code step shows complete code; the only conditional is the LOC-gated lift, which includes its full procedure.
