# Planner `<now>` Context Injection — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the agent planner an authoritative current date/time so date-relative questions stop looping to the plan cap.

**Architecture:** A pure renderer formats a `jiff::Zoned` into a trusted `<now>` block; the prompt assembler emits it first; the DB-backed builder captures the instant per plan formulation in an operator-configured timezone; the planner prompt is told to use it instead of web-searching for the date.

**Tech Stack:** Rust, `jiff` (timezone-aware datetime, auto-DST), existing `kastellan-core` prompt-assembly.

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible deps only.** `jiff` is `Unlicense OR MIT` ✓ — add with a license comment in `[workspace.dependencies]`.
- **Cross-platform (Linux + macOS first-class).** `jiff` reads the system tz DB on both. No OS-gated code.
- **Pure functions in reusable modules over complex code.** The renderer + resolver are pure and separately tested.
- **TDD.** Every unit lands test-first.
- **Files under ~500 LOC.** `now.rs` is small; `assemble.rs` grows by one guarded block.
- **Byte-identical when disabled.** With no timezone configured (`now: None`), assembled output is unchanged from today.
- **Trusted block — never escaped.** `<now>` is system-generated, rendered verbatim (like `<l0>`/`<tools>`), NOT through `escape_untrusted_body`.

---

### Task 1: `jiff` dependency + `render_now_block` pure renderer

**Files:**
- Modify: `Cargo.toml` (root — add `jiff` to `[workspace.dependencies]`)
- Modify: `core/Cargo.toml` (reference `jiff = { workspace = true }`)
- Create: `core/src/prompt_assembly/now.rs`
- Modify: `core/src/prompt_assembly/mod.rs` (add `mod now;`)

**Interfaces:**
- Produces: `pub(crate) fn render_now_block(now: &jiff::Zoned) -> String` — the `<now>…</now>\n` block (trailing newline included so the caller concatenates cleanly), minute resolution.

- [ ] **Step 1: Add the dependency**

In root `Cargo.toml`, under `[workspace.dependencies]`, after the `# Time` group add:

```toml
# Timezone-aware datetime with automatic DST + IANA tz DB. Used to inject a
# trusted current-date/time block into the planner prompt (and, later, travel /
# timezone features). License: Unlicense OR MIT — AGPL-compatible. Safe in
# multithreaded processes (unlike time::now_local()).
jiff = "0.2"
```

In `core/Cargo.toml`, after the `time = { workspace = true }` line add:

```toml
jiff               = { workspace = true }
```

- [ ] **Step 2: Write the failing test**

Create `core/src/prompt_assembly/now.rs` with only the test module (renderer stubbed to `unimplemented!()` so it compiles-then-fails):

```rust
//! Trusted current-date/time (`<now>`) block for the planner system prompt.
//!
//! The planner is otherwise date-blind: for any date-relative question
//! ("yesterday", "latest") it web-searches to *guess* the date and loops to the
//! plan cap. This module supplies an authoritative, system-generated timestamp
//! it can trust. Pure renderer + pure timezone resolver; the instant is
//! captured by the caller so the render is deterministic and testable.

use jiff::Zoned;

/// Render the trusted `<now>` grounding block. Pure — the caller supplies the
/// instant. Minute resolution (no seconds) keeps the assembled system prompt —
/// and its `system_prompt_sha256` — stable within a plan iteration so the local
/// model's KV-cache prefix is not churned each second. Verbatim, NOT escaped:
/// system-generated, not adversary-influenced.
pub(crate) fn render_now_block(now: &Zoned) -> String {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    // NOTE: 2026-07-12 is a SUNDAY (verified). Australia/Sydney is AEST=UTC+10
    // in July (southern-hemisphere winter → no DST). Named-zone construction
    // relies on the system tz DB, present on the dev Mac, the DGX, and CI Linux.

    #[test]
    fn renders_weekday_date_minute_and_offset() {
        let z = date(2026, 7, 12)
            .at(14, 5, 0, 0)
            .in_tz("Australia/Sydney")
            .expect("valid Sydney datetime");
        assert_eq!(
            render_now_block(&z),
            "<now>\nCurrent date and time: Sunday, 12 July 2026, 14:05 (AEST, UTC+10:00).\n</now>\n"
        );
    }

    #[test]
    fn utc_instant_renders_utc_label() {
        let z = date(2026, 7, 12).at(4, 5, 0, 0).in_tz("UTC").expect("utc");
        let block = render_now_block(&z);
        assert_eq!(
            block,
            "<now>\nCurrent date and time: Sunday, 12 July 2026, 04:05 (UTC, UTC+00:00).\n</now>\n"
        );
    }

    #[test]
    fn seconds_are_not_rendered() {
        let with_secs = date(2026, 7, 12).at(14, 5, 59, 0).in_tz("Australia/Sydney").unwrap();
        let block = render_now_block(&with_secs);
        assert!(block.contains("14:05"), "minute resolution only");
        assert!(!block.contains(":59"), "seconds must not appear");
    }
}
```

Then wire the module: in `core/src/prompt_assembly/mod.rs`, add near the other `mod` lines:

```rust
mod now;
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly::now:: -- --nocapture`
Expected: compiles, then FAILS at `unimplemented!()` (panic in `render_now_block`).

- [ ] **Step 4: Implement the renderer**

Replace the `unimplemented!()` body:

```rust
pub(crate) fn render_now_block(now: &Zoned) -> String {
    // %A weekday, %-d no-pad day, %B month, %Y year, %H:%M 24h minute,
    // %Z tz abbreviation (e.g. AEST), %:z offset with colon (e.g. +10:00).
    let stamp = now.strftime("%A, %-d %B %Y, %H:%M (%Z, UTC%:z)").to_string();
    format!("<now>\nCurrent date and time: {stamp}.\n</now>\n")
}
```

- [ ] **Step 5: Run the tests; correct the format string against real jiff output if needed**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly::now:: -- --nocapture`
Expected: PASS. If a specifier differs (e.g. jiff renders the day space-padded, or `%Z`/`%:z` formatting differs), adjust the **format string** so the rendered block matches the intended human-readable form asserted in the tests — the asserted output is the contract, the format string is the implementation.

- [ ] **Step 6: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`
Then:

```bash
git add Cargo.toml Cargo.lock core/Cargo.toml core/src/prompt_assembly/now.rs core/src/prompt_assembly/mod.rs
git commit -m "feat(planner): render_now_block — pure trusted <now> block (jiff)"
```

---

### Task 2: `resolve_timezone` + `TzSource`

**Files:**
- Modify: `core/src/prompt_assembly/now.rs`
- Modify: `core/src/prompt_assembly/mod.rs` (`pub use now::{resolve_timezone, TzSource};`)

**Interfaces:**
- Consumes: `jiff::tz::TimeZone`
- Produces: `pub fn resolve_timezone(configured: Option<&str>) -> (jiff::tz::TimeZone, TzSource)`; `pub enum TzSource { Configured, System, UtcFallback }`

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `now.rs`:

```rust
    #[test]
    fn configured_iana_name_resolves() {
        let (_tz, src) = resolve_timezone(Some("Australia/Sydney"));
        assert_eq!(src, TzSource::Configured);
    }

    #[test]
    fn unset_uses_system_zone() {
        let (_tz, src) = resolve_timezone(None);
        assert_eq!(src, TzSource::System);
    }

    #[test]
    fn blank_is_treated_as_unset() {
        let (_tz, src) = resolve_timezone(Some("   "));
        assert_eq!(src, TzSource::System);
    }

    #[test]
    fn invalid_name_falls_back_to_utc() {
        let (tz, src) = resolve_timezone(Some("Not/AZone"));
        assert_eq!(src, TzSource::UtcFallback);
        // The UTC fallback must still render a valid block, not panic.
        let z = jiff::Timestamp::UNIX_EPOCH.to_zoned(tz);
        assert!(render_now_block(&z).contains("UTC+00:00"));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly::now:: -- --nocapture`
Expected: compile error (`resolve_timezone`/`TzSource` not found).

- [ ] **Step 3: Implement**

Add to `now.rs` (above the `tests` module):

```rust
use jiff::tz::TimeZone;

/// Where the planner's timezone came from — logged once at startup so a
/// misconfigured `KASTELLAN_TIMEZONE` is visible rather than silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TzSource {
    /// A valid `KASTELLAN_TIMEZONE` IANA name.
    Configured,
    /// Unset/blank → the host system timezone.
    System,
    /// A set-but-unresolvable name → UTC (fail-safe: the block still renders).
    UtcFallback,
}

/// Resolve the operator's configured timezone. `configured` is the
/// `KASTELLAN_TIMEZONE` value: an IANA name (e.g. "Australia/Sydney"); unset or
/// blank → the host system zone; a set-but-invalid name → UTC. DST is handled
/// automatically by `jiff` at render time.
pub fn resolve_timezone(configured: Option<&str>) -> (TimeZone, TzSource) {
    match configured.map(str::trim) {
        Some(name) if !name.is_empty() => match TimeZone::get(name) {
            Ok(tz) => (tz, TzSource::Configured),
            Err(_) => (TimeZone::UTC, TzSource::UtcFallback),
        },
        _ => (TimeZone::system(), TzSource::System),
    }
}
```

In `mod.rs`, alongside the existing `pub use`s, add:

```rust
pub use now::{resolve_timezone, TzSource};
```

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly::now:: -- --nocapture`
Expected: PASS (7 tests total in the module).

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`

```bash
git add core/src/prompt_assembly/now.rs core/src/prompt_assembly/mod.rs
git commit -m "feat(planner): resolve_timezone + TzSource (KASTELLAN_TIMEZONE, UTC fallback)"
```

---

### Task 3: `assemble_system_prompt` gains `now: Option<&str>`, rendered first

**Files:**
- Modify: `core/src/prompt_assembly/assemble.rs` (signature + body + module-doc order line)
- Modify: `core/src/prompt_assembly/assemble/tests.rs` (append `None` to every existing call; add new tests)
- Modify: `core/src/prompt_assembly/pg_builder.rs:79` (append `None` — Task 4 replaces it)

**Interfaces:**
- Produces: `assemble_system_prompt(l0, l1, skills, recalled, base, tools, now: Option<&str>) -> String` — `<now>` emitted first when `Some(non-empty)`, omitted when `None`.

- [ ] **Step 1: Write the failing tests**

Append to `core/src/prompt_assembly/assemble/tests.rs`:

```rust
    #[test]
    fn now_block_is_emitted_first_when_some() {
        let out = assemble_system_prompt(
            &[], &[], &[], &RecalledContext::empty(), "BASE", &[],
            Some("<now>\nCurrent date and time: Sunday, 12 July 2026, 14:05 (AEST, UTC+10:00).\n</now>\n"),
        );
        assert!(out.starts_with("<now>\n"), "now block must be first; got: {out}");
        assert!(out.contains("12 July 2026"));
        // <base> still terminal.
        assert!(out.trim_end().ends_with("</base>"));
    }

    #[test]
    fn now_none_is_byte_identical_to_prior_output() {
        let with_none = assemble_system_prompt(
            &[], &[], &[], &RecalledContext::empty(), "BASE", &[], None,
        );
        // Reconstruct the pre-<now> expectation: no <now> block at all.
        assert!(!with_none.contains("<now>"), "None must emit no now block");
        assert!(with_none.starts_with("<handoff>") || with_none.starts_with("<base>")
            || with_none.starts_with("<l0_meta_rules>"),
            "output shape unchanged when now is None; got: {with_none}");
    }

    #[test]
    fn now_precedes_l0_when_both_present() {
        let l0 = vec![mem("L0 rule")];
        let out = assemble_system_prompt(
            &l0, &[], &[], &RecalledContext::empty(), "BASE", &[],
            Some("<now>\nNOWLINE\n</now>\n"),
        );
        let now_at = out.find("<now>").expect("now present");
        let l0_at = out.find("<l0_meta_rules>").expect("l0 present");
        assert!(now_at < l0_at, "now must precede l0");
    }
```

(If the existing test file has no `mem(...)` helper for building an L0 `Memory`, reuse whatever helper the file already uses to construct `Memory` rows — check the top of `tests.rs` and match it.)

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly::assemble -- --nocapture`
Expected: compile error — `assemble_system_prompt` takes 6 args, tests pass 7.

- [ ] **Step 3: Update the signature + body**

In `assemble.rs`, change the signature to append `now`:

```rust
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    skills: &[SurfacedSkill],
    recalled: &RecalledContext,
    base: &str,
    tools: &[ToolDoc],
    now: Option<&str>,
) -> String {
    let mut out = String::new();

    // Trusted, system-generated grounding fact — rendered FIRST, verbatim (not
    // escaped). Omitted entirely when None so output is byte-identical to the
    // pre-<now> assembler.
    if let Some(block) = now {
        if !block.is_empty() {
            out.push_str(block);
            out.push('\n');
        }
    }

    if !l0.is_empty() {
        // …unchanged…
```

(Leave the rest of the function body exactly as-is.) Update the module-doc order line near the top of `assemble.rs` from `L0 → L1 → skills → recalled → tools → handoff → base` to `now → L0 → L1 → skills → recalled → tools → handoff → base`, and add one sentence noting `<now>` is trusted/verbatim and first.

- [ ] **Step 4: Append `None` to every existing call site**

In `assemble/tests.rs`, every existing `assemble_system_prompt(...)` call currently ends with `&[])` or `&tools)` (the `tools` arg). Append `, None` before the closing paren of each existing call (≈30 calls). In `pg_builder.rs` line 79, change:

```rust
        let system_prompt =
            assemble_system_prompt(&l0, &l1, &skills, recalled, base, &self.tool_docs, None);
```

- [ ] **Step 5: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly:: -- --nocapture`
Expected: PASS — all existing assemble tests unchanged in behavior + the 3 new ones green.

- [ ] **Step 6: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`

```bash
git add core/src/prompt_assembly/assemble.rs core/src/prompt_assembly/assemble/tests.rs core/src/prompt_assembly/pg_builder.rs
git commit -m "feat(planner): assemble_system_prompt emits a leading <now> block (append-last param)"
```

---

### Task 4: `PgSystemPromptBuilder::with_timezone` + per-build capture

**Files:**
- Modify: `core/src/prompt_assembly/now.rs` (add `current_now_block`)
- Modify: `core/src/prompt_assembly/pg_builder.rs` (field, `new` default, `with_timezone`, `build_with_recalled` wiring, test accessor + test)

**Interfaces:**
- Consumes: `render_now_block`, `jiff::tz::TimeZone`
- Produces: `pub(crate) fn current_now_block(tz: &jiff::tz::TimeZone) -> String`; `PgSystemPromptBuilder::with_timezone(self, tz) -> Self`.

- [ ] **Step 1: Add `current_now_block` (impure capture) to `now.rs`**

```rust
/// Capture the current instant in `tz` and render the `<now>` block. The one
/// impure hop (`Timestamp::now()`); the formatting logic it delegates to is the
/// pure, tested `render_now_block`.
pub(crate) fn current_now_block(tz: &TimeZone) -> String {
    let now = jiff::Timestamp::now().to_zoned(tz.clone());
    render_now_block(&now)
}
```

- [ ] **Step 2: Write the failing builder test**

Append to the `tests` module in `pg_builder.rs`:

```rust
    #[tokio::test]
    async fn builder_defaults_to_no_timezone() {
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        let b = PgSystemPromptBuilder::new(pool);
        assert!(b.timezone_for_test().is_none(), "new() must not inject <now>");
    }

    #[tokio::test]
    async fn with_timezone_sets_the_zone() {
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        let (tz, _src) = crate::prompt_assembly::resolve_timezone(Some("Australia/Sydney"));
        let b = PgSystemPromptBuilder::new(pool).with_timezone(tz);
        // The block the builder would inject is well-formed and current-year.
        let block = super::super::now::current_now_block(b.timezone_for_test().unwrap());
        assert!(block.starts_with("<now>\n") && block.trim_end().ends_with("</now>"));
        assert!(block.contains("202"), "renders a plausible year; got: {block}");
    }
```

- [ ] **Step 3: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly::pg_builder -- --nocapture`
Expected: compile error (`timezone_for_test`/`with_timezone` not found).

- [ ] **Step 4: Implement the builder changes**

In `pg_builder.rs`, add the field to the struct:

```rust
pub struct PgSystemPromptBuilder {
    pool: PgPool,
    tool_docs: Arc<[ToolDoc]>,
    /// Configured planner timezone. `None` (the `new()` default) → no `<now>`
    /// block, keeping output byte-identical to the pre-<now> builder. Set by
    /// the daemon via [`with_timezone`](Self::with_timezone).
    timezone: Option<jiff::tz::TimeZone>,
}
```

Update `new`:

```rust
    pub fn new(pool: PgPool) -> Self {
        Self { pool, tool_docs: Arc::from(Vec::new()), timezone: None }
    }
```

Add the setter + test accessor after `with_tool_docs`:

```rust
    /// Attach the planner timezone (enables the `<now>` block). Threaded from
    /// `resolve_timezone(KASTELLAN_TIMEZONE)` at daemon startup.
    pub fn with_timezone(mut self, tz: jiff::tz::TimeZone) -> Self {
        self.timezone = Some(tz);
        self
    }

    #[cfg(test)]
    fn timezone_for_test(&self) -> Option<&jiff::tz::TimeZone> {
        self.timezone.as_ref()
    }
```

In `build_with_recalled`, replace the `assemble_system_prompt(...)` call (currently ending `, None`) with a captured now block:

```rust
        let now_block = self.timezone.as_ref().map(|tz| super::now::current_now_block(tz));
        let system_prompt = assemble_system_prompt(
            &l0, &l1, &skills, recalled, base, &self.tool_docs, now_block.as_deref(),
        );
```

- [ ] **Step 5: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib prompt_assembly::pg_builder -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`

```bash
git add core/src/prompt_assembly/now.rs core/src/prompt_assembly/pg_builder.rs
git commit -m "feat(planner): PgSystemPromptBuilder::with_timezone captures <now> per build"
```

---

### Task 5: Daemon wiring + planner-prompt guidance + env docs

**Files:**
- Modify: `core/src/main.rs` (read `KASTELLAN_TIMEZONE`, resolve, log source, `.with_timezone(...)`)
- Modify: `prompts/agent_planner.md` (the "use `<now>`, never search for the date" rule)
- Modify: any documented env sample if present (e.g. `kastellan.env` template / docs) — grep first.

**Interfaces:**
- Consumes: `kastellan_core::prompt_assembly::{resolve_timezone, PgSystemPromptBuilder}`

- [ ] **Step 1: Wire the daemon**

In `core/src/main.rs`, before the builder construction at lines 325-326, resolve the zone:

```rust
    let (planner_tz, tz_source) =
        kastellan_core::prompt_assembly::resolve_timezone(std::env::var("KASTELLAN_TIMEZONE").ok().as_deref());
    tracing::info!(?tz_source, "planner timezone resolved for <now> block");
```

Then extend the builder chain:

```rust
                kastellan_core::prompt_assembly::PgSystemPromptBuilder::new(pool.clone())
                    .with_tool_docs(tool_docs.clone())
                    .with_timezone(planner_tz),
```

- [ ] **Step 2: Add the planner guidance**

In `prompts/agent_planner.md`, in the "Answer directly when you can" section (around line 153), add a paragraph:

```markdown
**You already know the current date and time.** It is provided in the `<now>`
block of your system prompt (weekday, date, time, and timezone). Use it directly
for every date/time judgement — "today", "yesterday", "this week", "recent",
"latest", "how long ago". **Never** issue a web search to find out the current
date or time: you already have it, and search-result snippets report
inconsistent dates that will send you into a needless re-search loop.
```

- [ ] **Step 3: Update documented env (if present)**

Run: `grep -rn "KASTELLAN_WEB_SEARCH_ENDPOINT\|KASTELLAN_PROMPTS_DIR" scripts docs 2>/dev/null | grep -i "env\|example\|sample"`
If an env sample/template lists operator vars, add:

```
# Planner "now" timezone (IANA name). Unset → host system tz; invalid → UTC.
KASTELLAN_TIMEZONE=Australia/Sydney
```

If no such template exists, note the var in the deployment doc where other `KASTELLAN_*` vars are documented. (Do not fabricate a file — only edit an existing one.)

- [ ] **Step 4: Build + clippy the workspace touch-points**

Run:
```bash
source "$HOME/.cargo/env"
cargo build -p kastellan-core
cargo clippy -p kastellan-core --all-targets -- -D warnings
```
Expected: exit 0, clean.

- [ ] **Step 5: Commit**

```bash
git add core/src/main.rs prompts/agent_planner.md
# plus any env/doc file edited in Step 3
git commit -m "feat(planner): wire KASTELLAN_TIMEZONE into the builder + guide the planner to use <now>"
```

---

### Task 6: Full verification + PR

**Files:** none (verification + docs)

- [ ] **Step 1: Full workspace build + targeted tests + clippy**

Run:
```bash
source "$HOME/.cargo/env"
cargo build --workspace
cargo test -p kastellan-core --lib prompt_assembly:: -- --nocapture
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: build exit 0; all `prompt_assembly` tests green (incl. the ~12 new); clippy clean. Record the exact pass/fail counts.

- [ ] **Step 2: Update HANDOVER + ROADMAP**

Update `docs/devel/handovers/HANDOVER.md` (new "Last updated" latest block: the `<now>` fix, root cause, what's green) and `docs/devel/ROADMAP.md` (tick the plan-feedback line as the date-loop root cause fixed; note batch-search is the next follow-up). Keep both concise.

- [ ] **Step 3: Commit docs + push + open PR**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(planner): HANDOVER/ROADMAP — <now> injection fixes the date-relative plan-cap loop"
git push -u origin feat/planner-now-context
```
Open a PR to `main` describing the root cause (DGX tasks 88/89/90), the fix, and the live-acceptance step. Link no issue (none filed) or file one if desired.

- [ ] **Step 4: Live DGX acceptance (post-merge or on the branch build)**

Deploy per the runbook (`scripts/build-release.sh` — the `--features live-matrix` caveat; re-add force-routing to the unit; set `KASTELLAN_TIMEZONE=Australia/Sydney` in `~/.config/kastellan/kastellan.env`; restart). Then:

```bash
ssh dgx '~/.local/lib/kastellan/kastellan-cli ask "what were the main news stories in Germany yesterday?" --fast'
```
Expected: completes **without** a date-resolution search loop, and "yesterday" resolves against the true current date (verify via the `plan.formulate` audit rows: the planner's `context` should reference the `<now>` date, not "I need to determine the current date"). Compare plan_count to the pre-fix baseline (task 88 = 5/fail).

---

## Self-review

- **Spec coverage:** renderer (T1) ✓, timezone resolver + fallback (T2) ✓, assembly first-position + None-omit (T3) ✓, builder opt-in per-build capture (T4) ✓, daemon wiring + planner guidance + env doc (T5) ✓, verify + live acceptance (T6) ✓. `jiff` dep + license note (T1) ✓.
- **Placeholder scan:** all code steps carry real code; the one deliberate throwaway (the compile-only assert in T1 Step 2) is explicitly deleted in T1 Step 6.
- **Type consistency:** `render_now_block(&Zoned)->String`, `resolve_timezone(Option<&str>)->(TimeZone,TzSource)`, `current_now_block(&TimeZone)->String`, `with_timezone(self,TimeZone)->Self`, `assemble_system_prompt(…,now:Option<&str>)` — names/types consistent across T1–T5.
- **Known residual:** exact `jiff` strftime specifiers are pinned by the T1 test and corrected there against real output (the asserted human-readable block is the contract). The named-zone tests depend on the system tz DB (present on all our hosts).
