# Automatic Classification-Floor Inference Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land automatic classification-floor inference so `hhagent-cli ask` no longer requires `--classification-floor` for clinical work, plus a defence-in-depth `Plan.floor_request` channel that lets the agent raise (never lower) the floor mid-task.

**Architecture:** Three additions in one branch.
(1) NEW pure module `core/src/classification_inference.rs` exporting `InferredFloor { class, signals }` + `infer_floor(instruction) -> InferredFloor`. Tiered per-class keyword scan (Secret > Clinical > Personal > Public); first class with a matching signal wins; `contains_word` whole-word matching mirrors the `ConstitutionalGuard` post-review precedent.
(2) NEW optional field `Plan.floor_request: Option<DataClass>` + inner-loop check that elevates `ctx.classification_floor` to `max(producer_floor, agent_request)` before reviewer runs; agent can only raise.
(3) Provenance metadata (`classification_floor_source` ∈ `{operator, cli_inferred, agent_raised, default}` + `classification_floor_signals: Vec<&'static str>`) lands as pure-additive keys on `task.payload` JSONB and on the `agent/plan.formulate` audit-row payload (14 keys default, 15 when source is `cli_inferred`).

**Tech Stack:** Rust 2021. Pure functions where possible (the classifier is non-async); `serde` for round-tripping the new Plan field. No new external deps. Existing test harness (`#[test]` for pure helpers, `#[tokio::test]` for integration).

**Spec:** [docs/superpowers/specs/2026-05-16-automatic-floor-inference-design.md](../specs/2026-05-16-automatic-floor-inference-design.md)

**Branch:** `feat/automatic-floor-inference` (already created; carries `02638c5` spec commit at the start).

---

## File Structure

- **Create:** `core/src/classification_inference.rs` — pure tiered keyword classifier; target ~300 LOC (~150 production + ~150 tests). Public surface: `InferredFloor` struct, `ClassificationFloorSource` enum, `infer_floor(instruction) -> InferredFloor`, private `contains_word` helper.
- **Modify:** `core/src/lib.rs` — add `pub mod classification_inference;` declaration (1 line).
- **Modify:** `core/src/cassandra/types.rs` — add `Plan.floor_request: Option<DataClass>` field with `#[serde(default, skip_serializing_if = "Option::is_none")]`. Update all existing `Plan { ... }` struct-literal sites across the workspace to include `floor_request: None,`. +2 unit tests.
- **Modify:** `core/src/scheduler/inner_loop.rs` — widen `TaskContext` with `classification_floor_source: ClassificationFloorSource` and `classification_floor_signals: Vec<String>` fields; add floor-raise check before `write_audit_plan_formulate`; widen `build_plan_formulate_payload` to emit the two new keys; +6 unit tests.
- **Modify:** `core/src/scheduler/runner.rs` — read `classification_floor_source` (default `"default"`) and `classification_floor_signals` (default empty) from `task.payload`; fail-closed on unrecognised source string; thread into `TaskContext`.
- **Modify:** `core/src/bin/hhagent-cli.rs` — wire `infer_floor` into `run_ask`/`ask_async`; emit `tracing::warn!` on operator-explicit-suppression-with-elevation; thread source + signals into the submitted payload; +3 unit tests.
- **Modify:** `core/tests/scheduler_inner_loop_e2e.rs` — +1 integration test for agent-raise → DP-block chain.
- **Modify:** `core/tests/cli_ask_e2e.rs` — extend happy-path assertions for the new payload keys.
- **Modify:** `prompts/agent_planner.md` — add `floor_request: null` to JSON-schema example + one explanatory paragraph.
- **Modify:** `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — end-of-session update.

**File-size watch:** `core/src/classification_inference.rs` ≤ 500 LOC. `core/src/scheduler/inner_loop.rs` was 700 LOC before this slice (pre-existing soft-cap breach); this slice adds ~80 LOC. `core/src/bin/hhagent-cli.rs` was 1089 LOC (pre-existing breach); this slice adds ~50 LOC. No splits warranted today.

---

## Background — reading list for the engineer

Before starting, skim these in order so the surrounding contract is in context:

1. **Spec:** [docs/superpowers/specs/2026-05-16-automatic-floor-inference-design.md](../specs/2026-05-16-automatic-floor-inference-design.md) — the why, the catalogues, the provenance contract.
2. **Type surface:** [core/src/cassandra/types.rs](../../../core/src/cassandra/types.rs) — read `DataClass` enum + `rank()` (lines 21-56), `Plan` struct (lines 98-119), the invariant comment (lines 121-126), `DECISION_TERMINAL` / `DECISION_REFUSED` constants (lines 13-15).
3. **Mirror module — `ConstitutionalGuard`:** [core/src/cassandra/constitutional.rs](../../../core/src/cassandra/constitutional.rs) — note the private `contains_word` helper at the bottom of the file. The new `classification_inference.rs` reuses the same matching idiom; copy it (the helper is private to `constitutional.rs`, so duplication is the right answer for now — extract to a shared module only when a third caller materialises).
4. **Inner-loop floor read path:** [core/src/scheduler/inner_loop.rs:202-213](../../../core/src/scheduler/inner_loop.rs#L202-L213) — `write_audit_plan_formulate` + `ReviewStageContext` construction. The floor-raise check lands between `plan_count += 1` (line 191) and `write_audit_plan_formulate` (line 203) so the audit row + reviewer both see the elevated floor.
5. **Audit-payload builder:** [core/src/scheduler/inner_loop.rs:332-395](../../../core/src/scheduler/inner_loop.rs#L332-L395) — the existing pure `build_plan_formulate_payload` (13 keys post-Slice-A). Add the two new keys per spec §5.
6. **Runner payload-read pattern:** [core/src/scheduler/runner.rs:278-298](../../../core/src/scheduler/runner.rs#L278-L298) — the existing `classification_floor` reader is the template. Read `classification_floor_source` and `classification_floor_signals` with the same fail-closed shape.
7. **CLI flag wiring pattern:** [core/src/bin/hhagent-cli.rs:271-310](../../../core/src/bin/hhagent-cli.rs#L271-L310) (`run_ask` arg loop) + [core/src/bin/hhagent-cli.rs:329-378](../../../core/src/bin/hhagent-cli.rs#L329-L378) (`ask_async` payload builder).
8. **`Plan` struct-literal call sites:** run `grep -rn "Plan {" core/ workspace/` (also check `core/tests/`) and update every literal to include `floor_request: None,` in Task 1. Don't miss any — the build will fail loudly if you do, but the iteration is faster if you find them up front.

**Build/test:**
```sh
source "$HOME/.cargo/env"
cargo build --workspace
cargo test --workspace                            # 557 expected at start
cargo test -p hhagent-core classification_inference  # fast inner loop
cargo test -p hhagent-core --test scheduler_inner_loop_e2e
cargo test -p hhagent-core --test cli_ask_e2e
```

**Branch already created and currently checked out; the spec commit `02638c5` is its tip.**

---

### Task 1: Add `Plan.floor_request` field + serde round-trip tests

**Files:**
- Modify: `core/src/cassandra/types.rs` (struct + 2 tests)
- Modify: every `Plan { ... }` struct-literal site across the workspace (add `floor_request: None,`)

This is the foundational type change. After this task, `Plan.floor_request` is reachable but the inner loop ignores it. Subsequent tasks consume the field.

- [ ] **Step 1: Locate every `Plan { ... }` struct-literal site**

Run this and write down the file:line list:
```sh
grep -rn "Plan {$\|Plan {[^}]*$" core/ db/ workspace/ tests-common/ 2>/dev/null | grep -v "/target/" | grep -v ".lock"
```
Plus any single-line struct literals:
```sh
grep -rn "data_ceiling:" core/ db/ tests-common/ 2>/dev/null | grep -v "/target/"
```
Expected sites (approximate, verify): ~8 in `core/src/cassandra/types.rs::tests`, several in `core/src/scheduler/inner_loop.rs::tests`, `core/tests/scheduler_inner_loop_e2e.rs`, `core/tests/scheduler_lanes_e2e.rs`, `core/src/scheduler/agent.rs` (if it constructs Plans for testing).

- [ ] **Step 2: Write the failing serde round-trip tests**

Add to `core/src/cassandra/types.rs` in the existing `#[cfg(test)] mod tests { ... }` block:

```rust
#[test]
fn plan_floor_request_round_trips_when_absent() {
    // Plan without floor_request should round-trip with no `floor_request` key.
    let p = Plan {
        context:      "c".into(),
        decision:     "task_complete".into(),
        rationale:    "r".into(),
        steps:        vec![],
        result:       None,
        data_ceiling: DataClass::Public,
        refused:      None,
        floor_request: None,
    };
    let s = serde_json::to_string(&p).unwrap();
    assert!(!s.contains("floor_request"),
        "absent floor_request must not serialise (skip_serializing_if); got: {s}");
    let back: Plan = serde_json::from_str(&s).unwrap();
    assert_eq!(back.floor_request, None);
}

#[test]
fn plan_floor_request_round_trips_when_set() {
    let p = Plan {
        context:      "c".into(),
        decision:     "task_complete".into(),
        rationale:    "r".into(),
        steps:        vec![],
        result:       None,
        data_ceiling: DataClass::Public,
        refused:      None,
        floor_request: Some(DataClass::ClinicalConfidential),
    };
    let s = serde_json::to_string(&p).unwrap();
    assert!(s.contains(r#""floor_request":"ClinicalConfidential""#),
        "set floor_request must serialise as PascalCase string; got: {s}");
    let back: Plan = serde_json::from_str(&s).unwrap();
    assert_eq!(back.floor_request, Some(DataClass::ClinicalConfidential));
}
```

- [ ] **Step 3: Run tests; verify compile-fail (RED)**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-core cassandra::types::tests::plan_floor_request 2>&1 | tail -20
```
Expected: build fails with `no field 'floor_request' on type 'Plan'`.

- [ ] **Step 4: Add the field to `Plan`**

In `core/src/cassandra/types.rs`, find the `Plan` struct and add after the `refused` field:

```rust
    /// Agent-side request to raise the producer-set classification floor
    /// for the rest of the task. `None` (the default) leaves the floor
    /// unchanged. A `Some(class)` whose rank is ≤ the current floor is
    /// honoured as a no-op (never lowers; pinned by
    /// `agent_floor_request_lower_than_producer_is_ignored` in
    /// `scheduler::inner_loop::tests`).
    ///
    /// Round-trips through serde with `skip_serializing_if = Option::is_none`
    /// so existing fixtures stay byte-stable when the agent doesn't
    /// emit a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_request: Option<DataClass>,
```

- [ ] **Step 5: Add `floor_request: None,` to every existing struct-literal site**

For each site identified in Step 1, append `floor_request: None,` (after `refused`). Compiler errors will fail the build until every site is patched.

- [ ] **Step 6: Run tests; verify GREEN**

```sh
cargo build --workspace 2>&1 | tail -5
cargo test --workspace 2>&1 | grep -E "(test result|FAIL|error)" | tail -20
```
Expected: 0 failed; total passed = 559 (557 baseline + 2 new tests).

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(cassandra): Plan.floor_request — agent-side floor-raise channel

New optional field on Plan, default None, serde-skipped when absent so
existing fixtures stay byte-stable. Semantic: agent can REQUEST a
higher classification floor mid-task; the inner loop enforces
max(producer_floor, agent_request) so lowering is never honoured.

The field is currently unused (no consumer wired in). Subsequent
tasks land the inner-loop check and the planner-prompt instruction.

+2 unit tests round-tripping the absent + set cases. Workspace test
count 557 → 559.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Build the `classification_inference` pure module

**Files:**
- Create: `core/src/classification_inference.rs`
- Modify: `core/src/lib.rs` (add `pub mod classification_inference;`)

The full implementation in one task — the patterns are small enough to land atomically and the test suite is the load-bearing artifact. Each class catalogue is grouped and tested together.

- [ ] **Step 1: Write the failing scaffold tests**

Create `core/src/classification_inference.rs` with this exact content (production code in this step is a placeholder; tests are the load-bearing target):

```rust
//! CLI-side automatic classification-floor inference.
//!
//! Pure tiered keyword classifier called from `hhagent-cli ask` before
//! task submission. Maps the user instruction to a [`DataClass`] floor
//! plus a list of grep-friendly signal tags that explain WHY the
//! floor was elevated.
//!
//! ## Scope
//!
//! - **In scope:** deterministic case-insensitive keyword matching
//!   over a small per-class catalogue. No regex, no NLP, no ML.
//!   English-only.
//! - **Out of scope:** anonymisation, declassification, multilingual
//!   support, learned classifiers, daemon-side re-inference.
//!
//! ## Design
//!
//! - Per-class pattern catalogues for the three non-Public classes
//!   (Secret, ClinicalConfidential, Personal). Public is the default
//!   (no patterns; catch-all).
//! - **Tiered scan:** check classes in order from highest to lowest;
//!   the first class with ≥ 1 matched signal becomes the result, and
//!   ALL matched signals from that winning class are collected.
//!   Lower-class patterns are NOT consulted once a winning class is
//!   found.
//! - **Matching style:** `contains_word` (whole-word, ASCII alphanumeric
//!   byte boundaries) for single-word patterns that have substring
//!   collision risk (e.g. `password` would otherwise match `password
//!   strength` AND `mypasswordispublic`). Multi-word phrases (e.g.
//!   `ct scan`) use bare `contains` since they have no whole-word
//!   collision shape.
//! - **Signal tags:** snake_case identifiers chosen to be grep-friendly
//!   in audit logs. Aliases (`ekg` ⇒ `ecg`, `x-ray` ⇒ `xray`) collapse
//!   to a canonical tag so operators querying logs don't have to
//!   enumerate variants.

use crate::cassandra::types::DataClass;

/// Result of running the keyword classifier against an instruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferredFloor {
    /// The highest matching class. `Public` when no signals matched.
    pub class:   DataClass,
    /// Snake_case tags of the pattern phrases that triggered the match.
    /// Empty when `class == Public`.
    pub signals: Vec<&'static str>,
}

/// Run the tiered keyword scan against `instruction` and return the
/// inferred floor + matched signal tags. Pure function; no I/O.
pub fn infer_floor(instruction: &str) -> InferredFloor {
    // PLACEHOLDER — replaced in Step 4.
    InferredFloor { class: DataClass::Public, signals: vec![] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_public_default() {
        let r = infer_floor("");
        assert_eq!(r.class, DataClass::Public);
        assert!(r.signals.is_empty());
    }

    #[test]
    fn whitespace_only_returns_public_default() {
        let r = infer_floor("   \n\t  ");
        assert_eq!(r.class, DataClass::Public);
        assert!(r.signals.is_empty());
    }

    #[test]
    fn benign_coding_question_stays_public() {
        let r = infer_floor("How do I write a quicksort in Rust?");
        assert_eq!(r.class, DataClass::Public);
        assert!(r.signals.is_empty());
    }

    // ===== Secret class =====

    #[test]
    fn password_signal_matches_secret() {
        let r = infer_floor("Rotate the database password for the prod cluster.");
        assert_eq!(r.class, DataClass::Secret);
        assert!(r.signals.contains(&"password"),
            "expected 'password' signal; got {:?}", r.signals);
    }

    #[test]
    fn api_key_signal_matches_secret() {
        let r = infer_floor("Where do I store the api key for OpenAI?");
        assert_eq!(r.class, DataClass::Secret);
        assert!(r.signals.contains(&"api_key"));
    }

    #[test]
    fn private_key_signal_matches_secret() {
        let r = infer_floor("Generate a new private key pair.");
        assert_eq!(r.class, DataClass::Secret);
        assert!(r.signals.contains(&"private_key"));
    }

    #[test]
    fn passworded_passive_form_does_not_match_secret() {
        // contains_word should reject the substring inside other words.
        let r = infer_floor("This document is passworded.");
        assert_eq!(r.class, DataClass::Public,
            "'passworded' is a different word; substring match would be a false positive");
    }

    // ===== ClinicalConfidential class =====

    #[test]
    fn patient_signal_matches_clinical() {
        let r = infer_floor("Summarise the patient's recent imaging.");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        assert!(r.signals.contains(&"patient"));
    }

    #[test]
    fn pathology_signal_matches_clinical() {
        let r = infer_floor("Translate this pathology report for the patient.");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        // both `pathology` and `patient` should fire
        assert!(r.signals.contains(&"pathology"));
        assert!(r.signals.contains(&"patient"));
    }

    #[test]
    fn ct_scan_multi_word_signal_matches_clinical() {
        let r = infer_floor("Compare this CT scan to last week's.");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        assert!(r.signals.contains(&"ct_scan"));
    }

    #[test]
    fn ecg_signal_matches_clinical() {
        let r = infer_floor("Read this ECG strip.");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        assert!(r.signals.contains(&"ecg"));
    }

    #[test]
    fn ekg_alias_collapses_to_ecg_tag() {
        // ekg and ecg are aliases; canonical tag is `ecg`.
        let r = infer_floor("Read this EKG.");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        assert!(r.signals.contains(&"ecg"),
            "ekg should produce the canonical 'ecg' tag; got {:?}", r.signals);
    }

    #[test]
    fn xray_alias_collapses_to_xray_tag() {
        let r = infer_floor("Order an x-ray.");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        assert!(r.signals.contains(&"xray"));
    }

    // ===== Personal class =====

    #[test]
    fn my_email_signal_matches_personal() {
        let r = infer_floor("Draft a reply on my email about the conference.");
        assert_eq!(r.class, DataClass::Personal);
        assert!(r.signals.contains(&"my_email"));
    }

    #[test]
    fn family_member_signal_matches_personal() {
        let r = infer_floor("Help me plan a holiday with my family member.");
        assert_eq!(r.class, DataClass::Personal);
        assert!(r.signals.contains(&"family_member"));
    }

    // ===== Tiered priority =====

    #[test]
    fn secret_wins_over_clinical_in_mixed_prompt() {
        // Both `password` and `patient` match; Secret is higher tier.
        let r = infer_floor("Rotate the patient portal password.");
        assert_eq!(r.class, DataClass::Secret);
        // Only Secret-class signals are collected (lower classes not consulted).
        assert!(r.signals.contains(&"password"));
        assert!(!r.signals.contains(&"patient"),
            "lower-class signals must not appear once a winning class is found; got {:?}", r.signals);
    }

    #[test]
    fn clinical_wins_over_personal_in_mixed_prompt() {
        let r = infer_floor("Update my email with the patient's discharge summary.");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        assert!(r.signals.contains(&"patient"));
        assert!(!r.signals.contains(&"my_email"));
    }

    #[test]
    fn case_insensitive_matching() {
        let r = infer_floor("RECORD THE PATIENT'S MEDICATION");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        assert!(r.signals.contains(&"patient"));
        assert!(r.signals.contains(&"medication"));
    }
}
```

Then declare the module in `core/src/lib.rs`:

```rust
pub mod classification_inference;
```
(slot alphabetically between `cassandra` and `cli_audit`)

- [ ] **Step 2: Run the tests; verify failures (RED)**

```sh
cargo test -p hhagent-core classification_inference:: 2>&1 | grep -E "test result|FAIL" | tail -20
```
Expected: most tests fail (only `empty_input` / `whitespace_only` / `benign_coding_question` / `passworded_passive_form_does_not_match_secret` pass because the placeholder returns Public).

- [ ] **Step 3: Implement `contains_word` private helper**

Add to `core/src/classification_inference.rs` BEFORE the `infer_floor` function:

```rust
/// Whole-word ASCII-case-insensitive substring search.
///
/// Returns true iff `needle` appears in `haystack` with non-alphanumeric
/// (or string-boundary) bytes immediately before and after. Defends
/// against substring collisions like `password` matching `passworded`.
///
/// Note: the haystack/needle are compared lowercase-first via
/// `to_ascii_lowercase`. This is correct for English-only catalogues
/// (the spec's explicit scope); for future multilingual support the
/// match path would need Unicode case folding.
///
/// Mirrors the `contains_word` helper in `cassandra::constitutional`
/// (commit `5d48e3e`'s post-review precedent). Duplicated here rather
/// than lifted to a shared module — the third caller (if it ever
/// arrives) is the cue to extract.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() { return false; }
    let h = haystack.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    let n_bytes = n.as_bytes();
    let h_bytes = h.as_bytes();
    for (i, _) in h.match_indices(&n) {
        let before_ok = i == 0
            || !h_bytes[i - 1].is_ascii_alphanumeric();
        let after_idx = i + n_bytes.len();
        let after_ok = after_idx == h_bytes.len()
            || !h_bytes[after_idx].is_ascii_alphanumeric();
        if before_ok && after_ok { return true; }
    }
    false
}

#[cfg(test)]
mod contains_word_tests {
    use super::contains_word;

    #[test] fn whole_word_match() { assert!(contains_word("rotate the password please", "password")); }
    #[test] fn substring_no_match() { assert!(!contains_word("this is passworded", "password")); }
    #[test] fn case_insensitive() { assert!(contains_word("ROTATE THE PASSWORD", "password")); }
    #[test] fn empty_needle_no_match() { assert!(!contains_word("anything", "")); }
    #[test] fn punctuation_boundary() { assert!(contains_word("password!", "password")); }
}
```

- [ ] **Step 4: Implement `infer_floor` with the per-class catalogues**

Replace the placeholder `infer_floor` body in `core/src/classification_inference.rs` with:

```rust
/// Pattern catalogue entry: `(phrase, signal_tag, use_contains_word)`.
///
/// `use_contains_word = true` for single-word patterns with substring
/// collision risk; `false` for multi-word phrases (the inner space
/// already guards the match).
type CatalogueEntry = (&'static str, &'static str, bool);

/// Highest tier — credentials, tokens, certificates.
const SECRET_PATTERNS: &[CatalogueEntry] = &[
    ("password",      "password",      true),
    ("secret",        "secret",        true),
    ("credential",    "credential",    true),
    ("credentials",   "credential",    true),
    ("api key",       "api_key",       false),
    ("private key",   "private_key",   false),
    ("bearer token",  "bearer_token",  false),
    ("access token",  "access_token",  false),
    ("certificate",   "certificate",   true),
];

/// Clinical confidential — patient data, imaging, medication, codes.
const CLINICAL_PATTERNS: &[CatalogueEntry] = &[
    ("patient",            "patient",            true),
    ("diagnosis",          "diagnosis",          true),
    ("pathology",          "pathology",          true),
    ("radiology",          "radiology",          true),
    ("histology",          "histology",          true),
    ("biopsy",             "biopsy",             true),
    ("mri",                "mri",                true),
    ("ct scan",            "ct_scan",            false),
    ("x-ray",              "xray",               false),
    ("xray",               "xray",               true),
    ("ecg",                "ecg",                true),
    ("ekg",                "ecg",                true),     // alias → canonical tag
    ("medication",         "medication",         true),
    ("prescription",       "prescription",       true),
    ("dosage",             "dosage",             true),
    ("discharge summary",  "discharge_summary",  false),
    ("medical record",     "medical_record",     false),
    ("clinical",           "clinical",           true),
    ("hl7",                "hl7",                true),
    ("dicom",              "dicom",              true),
    ("icd-10",             "icd_10",             false),
    ("snomed",             "snomed",             true),
];

/// Personal data — operator's own scope.
const PERSONAL_PATTERNS: &[CatalogueEntry] = &[
    ("my email",           "my_email",           false),
    ("my address",         "my_address",         false),
    ("my phone",           "my_phone",           false),
    ("my calendar",        "my_calendar",        false),
    ("family member",      "family_member",      false),
    ("personal calendar",  "personal_calendar",  false),
    ("private contact",    "private_contact",    false),
];

pub fn infer_floor(instruction: &str) -> InferredFloor {
    // Empty / whitespace fast-path.
    if instruction.trim().is_empty() {
        return InferredFloor { class: DataClass::Public, signals: vec![] };
    }

    // Tiered scan: check each class in order from highest to lowest.
    // The first class with at least one match wins; collect all
    // matched signal tags from that class (deduplicated, insertion
    // order preserved).
    for (class, catalogue) in &[
        (DataClass::Secret,               SECRET_PATTERNS),
        (DataClass::ClinicalConfidential, CLINICAL_PATTERNS),
        (DataClass::Personal,             PERSONAL_PATTERNS),
    ] {
        let signals = match_catalogue(instruction, catalogue);
        if !signals.is_empty() {
            return InferredFloor { class: *class, signals };
        }
    }
    InferredFloor { class: DataClass::Public, signals: vec![] }
}

/// Match every catalogue entry against the instruction; return the
/// signal tags of every entry that fired, in catalogue order, with
/// duplicates removed (an alias like `ekg`→`ecg` would otherwise
/// produce two `ecg` entries for an `ecg ekg` input).
fn match_catalogue(instruction: &str, catalogue: &[CatalogueEntry]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for (phrase, tag, use_word) in catalogue {
        let hit = if *use_word {
            contains_word(instruction, phrase)
        } else {
            instruction.to_ascii_lowercase().contains(&phrase.to_ascii_lowercase())
        };
        if hit && !out.contains(tag) {
            out.push(tag);
        }
    }
    out
}
```

- [ ] **Step 5: Run tests; verify GREEN**

```sh
cargo test -p hhagent-core classification_inference:: 2>&1 | grep -E "test result|FAIL" | tail -10
```
Expected: all tests pass (≥ 19 new tests added: 3 default + 4 Secret + 6 Clinical + 2 Personal + 3 tiered priority + 1 case-insensitive + 5 contains_word; total 21–24 in classification_inference module). Workspace total ~580.

- [ ] **Step 6: Commit**

```bash
git add core/src/classification_inference.rs core/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(core): classification_inference — tiered keyword floor classifier

New pure module `core::classification_inference`. Public surface:

- `InferredFloor { class, signals }` carries the inferred DataClass
  plus a Vec<&'static str> of snake_case signal tags identifying which
  catalogue phrases fired.
- `infer_floor(instruction: &str) -> InferredFloor` — tiered scan:
  Secret > Clinical > Personal > Public; first class with ≥1 match
  wins; all matching signals from the winning class are collected.

Matching uses a private `contains_word` whole-word helper (ASCII
alphanumeric byte boundaries, lowercase-fold) for single-word patterns
with substring collision risk; multi-word phrases (`ct scan`,
`discharge summary`) use bare `contains`. Mirrors the post-review
precedent from `cassandra::constitutional` (commit 5d48e3e).

Pattern catalogues per non-Public class seed initial coverage:
- Secret: password, secret, credential, api key, private key, bearer
  token, access token, certificate
- ClinicalConfidential: patient, diagnosis, pathology, radiology,
  histology, biopsy, mri, ct scan, xray/x-ray (alias), ecg/ekg
  (alias), medication, prescription, dosage, discharge summary,
  medical record, clinical, hl7, dicom, icd-10, snomed
- Personal: my email, my address, my phone, my calendar, family
  member, personal calendar, private contact

+21 unit tests (catalogue coverage + tier priority + case
insensitivity + contains_word edge cases + alias collapse).

The classifier has no in-tree caller yet; subsequent tasks wire it
into `hhagent-cli ask` and add the `floor_request` consumer.

Workspace test count 559 → 580.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Inner-loop wiring — `TaskContext` provenance fields + `ClassificationFloorSource` enum + payload widening (no behavior change yet)

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (type defs, payload builder, +3 unit tests)
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` (TaskContext literals if any)
- Modify: `core/tests/scheduler_lanes_e2e.rs` (TaskContext literals if any)

This task lands the new types and widens `build_plan_formulate_payload` to emit the two new keys — but does NOT add the floor-raise check yet. The behavior change comes in Task 4.

- [ ] **Step 1: Write the failing `build_plan_formulate_payload` shape tests**

Add to `core/src/scheduler/inner_loop.rs::tests` (find the existing `build_plan_formulate_payload_carries_full_plan_and_classification_floor` test and add these three siblings):

```rust
#[test]
fn build_plan_formulate_payload_default_source_has_14_keys() {
    let plan = Plan {
        context: "c".into(), decision: "task_complete".into(), rationale: "r".into(),
        steps: vec![], result: Some(serde_json::json!({"kind":"text","body":"ok"})),
        data_ceiling: DataClass::Public, refused: None, floor_request: None,
    };
    let meta = FormulationMeta {
        prompt_name: "p".into(), prompt_sha256: "h".into(),
        llm_model: "m".into(), llm_backend: "local".into(),
        latency_ms: 1, retry_count: 0,
    };
    let payload = build_plan_formulate_payload(
        1, 1, DataClass::Public, ClassificationFloorSource::Default, &[],
        &plan, &meta,
    );
    let obj = payload.as_object().expect("payload is an object");
    assert_eq!(obj.len(), 14,
        "default-source payload should have 14 keys (13 Slice-A + classification_floor_source); got {} keys: {:?}",
        obj.len(), obj.keys().collect::<Vec<_>>());
    assert_eq!(obj["classification_floor_source"], serde_json::Value::String("default".into()));
    assert!(obj.get("classification_floor_signals").is_none(),
        "signals key must be ABSENT when source is not cli_inferred");
}

#[test]
fn build_plan_formulate_payload_cli_inferred_source_has_15_keys() {
    let plan = Plan {
        context: "c".into(), decision: "task_complete".into(), rationale: "r".into(),
        steps: vec![], result: Some(serde_json::json!({"kind":"text","body":"ok"})),
        data_ceiling: DataClass::ClinicalConfidential, refused: None, floor_request: None,
    };
    let meta = FormulationMeta {
        prompt_name: "p".into(), prompt_sha256: "h".into(),
        llm_model: "m".into(), llm_backend: "local".into(),
        latency_ms: 1, retry_count: 0,
    };
    let signals = vec!["patient".to_string(), "pathology".to_string()];
    let payload = build_plan_formulate_payload(
        1, 1, DataClass::ClinicalConfidential, ClassificationFloorSource::CliInferred,
        &signals, &plan, &meta,
    );
    let obj = payload.as_object().expect("payload is an object");
    assert_eq!(obj.len(), 15,
        "cli_inferred payload should have 15 keys (default 14 + signals); got {} keys: {:?}",
        obj.len(), obj.keys().collect::<Vec<_>>());
    assert_eq!(obj["classification_floor_source"], serde_json::Value::String("cli_inferred".into()));
    let arr = obj["classification_floor_signals"].as_array()
        .expect("signals key is an array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0], serde_json::Value::String("patient".into()));
    assert_eq!(arr[1], serde_json::Value::String("pathology".into()));
}

#[test]
fn build_plan_formulate_payload_agent_raised_source_omits_signals() {
    // After an agent raise, signals are cleared — they only explain the
    // original CLI inference, not the elevated floor.
    let plan = Plan {
        context: "c".into(), decision: "task_complete".into(), rationale: "r".into(),
        steps: vec![], result: None,
        data_ceiling: DataClass::ClinicalConfidential, refused: None,
        floor_request: Some(DataClass::ClinicalConfidential),
    };
    let meta = FormulationMeta {
        prompt_name: "p".into(), prompt_sha256: "h".into(),
        llm_model: "m".into(), llm_backend: "local".into(),
        latency_ms: 1, retry_count: 0,
    };
    let payload = build_plan_formulate_payload(
        1, 1, DataClass::ClinicalConfidential, ClassificationFloorSource::AgentRaised,
        &[],  // empty: signals are cleared on raise
        &plan, &meta,
    );
    let obj = payload.as_object().expect("payload is an object");
    assert_eq!(obj.len(), 14, "agent_raised should have 14 keys (no signals); got: {:?}", obj.keys().collect::<Vec<_>>());
    assert_eq!(obj["classification_floor_source"], serde_json::Value::String("agent_raised".into()));
    assert!(obj.get("classification_floor_signals").is_none());
}
```

- [ ] **Step 2: Run tests; verify compile-fail (RED)**

```sh
cargo test -p hhagent-core scheduler::inner_loop::tests::build_plan_formulate_payload 2>&1 | tail -10
```
Expected: build fails — `ClassificationFloorSource` not yet defined; `build_plan_formulate_payload` signature doesn't match.

- [ ] **Step 3: Define `ClassificationFloorSource` enum**

Add to `core/src/scheduler/inner_loop.rs`, near the top (after the existing `use` block):

```rust
/// Provenance of the current `classification_floor` value.
///
/// Carried in [`TaskContext`] and emitted into the
/// `agent/plan.formulate` audit-row payload so operators can trace
/// any DP-blocked plan back to how the floor was set.
///
/// Wire form (lowercase snake_case) matches the operator-visible
/// audit-log token — renaming any branch is an audit-trail contract
/// break. Mirrors the `as_pascal_str` shape on `DataClass`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationFloorSource {
    /// Operator explicitly passed `--classification-floor X`.
    Operator,
    /// CLI keyword classifier elevated above Public.
    CliInferred,
    /// Agent raised the floor mid-task via `Plan.floor_request`.
    AgentRaised,
    /// No inference matched and no operator flag was set.
    Default,
}

impl ClassificationFloorSource {
    pub fn as_snake_str(self) -> &'static str {
        match self {
            ClassificationFloorSource::Operator    => "operator",
            ClassificationFloorSource::CliInferred => "cli_inferred",
            ClassificationFloorSource::AgentRaised => "agent_raised",
            ClassificationFloorSource::Default     => "default",
        }
    }
}
```

- [ ] **Step 4: Widen `TaskContext`**

In the existing `TaskContext` struct (around line 19), add two new fields:

```rust
pub struct TaskContext {
    pub task_id: i64,
    pub lane: hhagent_db::tasks::Lane,
    pub instruction: String,
    pub classification_floor: DataClass,
    /// Provenance of `classification_floor`. Set at task entry by
    /// `runner::run_inner_loop_for_task`; mutated to `AgentRaised` on
    /// successful agent floor-raise (Task 4).
    pub classification_floor_source: ClassificationFloorSource,
    /// Matched signal tags. Non-empty iff
    /// `classification_floor_source == CliInferred`. Cleared on
    /// agent raise.
    pub classification_floor_signals: Vec<String>,
    pub plans: Vec<(Plan, Vec<StepOutcome>)>,
    pub advisories: Vec<String>,
    pub blocks: Vec<String>,
    pub plan_count: u32,
    pub max_plans: u32,
}
```

- [ ] **Step 5: Widen `build_plan_formulate_payload` signature + body**

Replace the existing function body with:

```rust
pub(crate) fn build_plan_formulate_payload(
    task_id: i64,
    plan_count: u32,
    classification_floor: DataClass,
    classification_floor_source: ClassificationFloorSource,
    classification_floor_signals: &[String],
    plan: &Plan,
    meta: &FormulationMeta,
) -> serde_json::Value {
    let decision_kind = if plan.is_refused() {
        crate::cassandra::types::DECISION_REFUSED
    } else if plan.is_terminal() {
        crate::cassandra::types::DECISION_TERMINAL
    } else {
        "act"
    };
    let refused = plan.refused.as_ref()
        .map(|r| serde_json::json!({ "principle": r.principle, "reason": r.reason }))
        .unwrap_or(serde_json::Value::Null);
    let plan_json = serde_json::to_value(plan)
        .expect("Plan serialisation cannot fail (no non-string keys, no NaN)");
    let classification_floor_json = serde_json::to_value(classification_floor)
        .expect("DataClass serialisation cannot fail (closed enum, no payloads)");

    let mut obj = serde_json::Map::new();
    obj.insert("task_id".into(),              serde_json::json!(task_id));
    obj.insert("plan_count".into(),           serde_json::json!(plan_count));
    obj.insert("prompt_name".into(),          serde_json::json!(meta.prompt_name));
    obj.insert("prompt_sha256".into(),        serde_json::json!(meta.prompt_sha256));
    obj.insert("llm_model".into(),            serde_json::json!(meta.llm_model));
    obj.insert("llm_backend".into(),          serde_json::json!(meta.llm_backend));
    obj.insert("latency_ms".into(),           serde_json::json!(meta.latency_ms));
    obj.insert("retry_count".into(),          serde_json::json!(meta.retry_count));
    obj.insert("plan_step_count".into(),      serde_json::json!(plan.steps.len()));
    obj.insert("decision_kind".into(),        serde_json::json!(decision_kind));
    obj.insert("refused".into(),              refused);
    obj.insert("plan".into(),                 plan_json);
    obj.insert("classification_floor".into(), classification_floor_json);
    obj.insert("classification_floor_source".into(),
               serde_json::json!(classification_floor_source.as_snake_str()));
    // Signals key only appears when source is CliInferred and we have signals.
    // (An empty signals slice + CliInferred source would still be a bug in
    // the caller — pinned by the absence assertion in the agent_raised test.)
    if classification_floor_source == ClassificationFloorSource::CliInferred
        && !classification_floor_signals.is_empty()
    {
        obj.insert("classification_floor_signals".into(),
                   serde_json::json!(classification_floor_signals));
    }
    serde_json::Value::Object(obj)
}
```

- [ ] **Step 6: Update the existing caller `write_audit_plan_formulate`**

```rust
async fn write_audit_plan_formulate(
    pool: &PgPool,
    ctx: &TaskContext,
    plan: &Plan,
    meta: &FormulationMeta,
) -> Result<(), InnerLoopError> {
    let payload = build_plan_formulate_payload(
        ctx.task_id,
        ctx.plan_count,
        ctx.classification_floor,
        ctx.classification_floor_source,
        &ctx.classification_floor_signals,
        plan,
        meta,
    );
    hhagent_db::audit::insert(pool, "agent", "plan.formulate", payload).await?;
    Ok(())
}
```

- [ ] **Step 7: Update existing `TaskContext` construction sites**

Find every `TaskContext { ... }` literal:
```sh
grep -rn "TaskContext {" core/ 2>/dev/null | grep -v "/target/"
```
At each site (likely `scheduler::runner::run_inner_loop_for_task` plus any tests in `scheduler::inner_loop::tests` or in the e2e files), add:
```rust
classification_floor_source: ClassificationFloorSource::Default,
classification_floor_signals: vec![],
```
For tests that explicitly want `CliInferred` or `Operator`, set those explicitly. For now, every site uses `Default` (correct since the runner is the canonical setter and Task 5 will update it).

Also update the existing `build_plan_formulate_payload_carries_full_plan_and_classification_floor` test to pass the new two parameters explicitly (use `Default` + empty slice).

- [ ] **Step 8: Run tests; verify GREEN**

```sh
cargo build --workspace 2>&1 | tail -5
cargo test --workspace 2>&1 | grep -E "test result|FAIL|error" | tail -15
```
Expected: 0 failed; +3 new unit tests. Total ~583.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(scheduler): TaskContext provenance fields + audit-payload widening

Adds the `ClassificationFloorSource` enum (Operator / CliInferred /
AgentRaised / Default) plus two new fields on `TaskContext` carrying
the source and (when applicable) the matched signal tags.

`build_plan_formulate_payload` widened to emit two new audit-row keys:
- `classification_floor_source` — always present, snake_case wire form.
- `classification_floor_signals` — present iff source is cli_inferred
  AND signals is non-empty. Pure-additive: existing JSONB consumers
  (replay harness, observation capture) keep working unchanged.

Payload key count: 13 → 14 (default) / 15 (cli_inferred). +3 unit
tests pinning the shape per source. No behaviour change yet — the
runner is still wired to `Default`, and the agent-raise check arrives
in the next task.

Workspace test count 580 → 583.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Inner-loop floor-raise check + unit tests

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (raise check + 3 tests)

This lands the actual behaviour: if `plan.floor_request` is `Some` and higher than the current floor, elevate `ctx.classification_floor` and set `ctx.classification_floor_source = AgentRaised`.

- [ ] **Step 1: Write the failing raise-check unit tests**

Add to `core/src/scheduler/inner_loop.rs::tests`:

```rust
/// Helper: pure raise check, extracted so the unit tests don't have to
/// spin up the full async inner loop. Returns true iff `ctx` was
/// updated.
fn apply_floor_raise_for_test(ctx: &mut TaskContext, plan: &Plan) -> bool {
    if let Some(req) = plan.floor_request {
        if req.rank() > ctx.classification_floor.rank() {
            ctx.classification_floor = req;
            ctx.classification_floor_source = ClassificationFloorSource::AgentRaised;
            ctx.classification_floor_signals.clear();
            return true;
        }
    }
    false
}

#[test]
fn agent_floor_request_higher_than_producer_elevates_ctx() {
    let mut ctx = TaskContext {
        task_id: 1, lane: hhagent_db::tasks::Lane::Fast,
        instruction: "".into(),
        classification_floor: DataClass::Public,
        classification_floor_source: ClassificationFloorSource::Default,
        classification_floor_signals: vec![],
        plans: vec![], advisories: vec![], blocks: vec![],
        plan_count: 0, max_plans: 3,
    };
    let plan = Plan {
        context: "c".into(), decision: "d".into(), rationale: "r".into(),
        steps: vec![], result: None,
        data_ceiling: DataClass::ClinicalConfidential, refused: None,
        floor_request: Some(DataClass::ClinicalConfidential),
    };
    let raised = apply_floor_raise_for_test(&mut ctx, &plan);
    assert!(raised);
    assert_eq!(ctx.classification_floor, DataClass::ClinicalConfidential);
    assert_eq!(ctx.classification_floor_source, ClassificationFloorSource::AgentRaised);
    assert!(ctx.classification_floor_signals.is_empty());
}

#[test]
fn agent_floor_request_lower_than_producer_is_ignored() {
    let mut ctx = TaskContext {
        task_id: 1, lane: hhagent_db::tasks::Lane::Fast,
        instruction: "".into(),
        classification_floor: DataClass::ClinicalConfidential,
        classification_floor_source: ClassificationFloorSource::Operator,
        classification_floor_signals: vec![],
        plans: vec![], advisories: vec![], blocks: vec![],
        plan_count: 0, max_plans: 3,
    };
    let plan = Plan {
        context: "c".into(), decision: "d".into(), rationale: "r".into(),
        steps: vec![], result: None,
        data_ceiling: DataClass::Public, refused: None,
        floor_request: Some(DataClass::Public),  // lower than Clinical
    };
    let raised = apply_floor_raise_for_test(&mut ctx, &plan);
    assert!(!raised, "lower floor_request must be ignored");
    assert_eq!(ctx.classification_floor, DataClass::ClinicalConfidential);
    assert_eq!(ctx.classification_floor_source, ClassificationFloorSource::Operator);
}

#[test]
fn agent_floor_request_equal_to_producer_is_no_op() {
    let mut ctx = TaskContext {
        task_id: 1, lane: hhagent_db::tasks::Lane::Fast,
        instruction: "".into(),
        classification_floor: DataClass::Personal,
        classification_floor_source: ClassificationFloorSource::CliInferred,
        classification_floor_signals: vec!["my_email".into()],
        plans: vec![], advisories: vec![], blocks: vec![],
        plan_count: 0, max_plans: 3,
    };
    let plan = Plan {
        context: "c".into(), decision: "d".into(), rationale: "r".into(),
        steps: vec![], result: None,
        data_ceiling: DataClass::Personal, refused: None,
        floor_request: Some(DataClass::Personal),
    };
    let raised = apply_floor_raise_for_test(&mut ctx, &plan);
    assert!(!raised, "equal-rank floor_request must be a no-op");
    assert_eq!(ctx.classification_floor, DataClass::Personal);
    assert_eq!(ctx.classification_floor_source, ClassificationFloorSource::CliInferred);
    assert_eq!(ctx.classification_floor_signals, vec!["my_email"]);
}
```

- [ ] **Step 2: Run; verify RED**

```sh
cargo test -p hhagent-core scheduler::inner_loop::tests::agent_floor 2>&1 | tail -10
```
Expected: build fails — `apply_floor_raise_for_test` not yet a thing. Actually it IS defined inside the tests block, so the failure is in the test bodies themselves if the production code doesn't honor the elevation. Wait — the test helper IS in tests, so the test will compile and pass trivially (the helper just does the right thing). RED comes from a different angle: we need a pin that the PRODUCTION code calls the same logic.

Better: write the tests using a helper that's actually a module-private function in production, then the test asserts via the helper. Refactor:

Add to `core/src/scheduler/inner_loop.rs` (production code, NOT in `tests`):

```rust
/// Apply `plan.floor_request` to `ctx` if it raises the current floor.
/// Pure side-effect on `ctx`. Returns true iff `ctx` was mutated.
///
/// Never lowers the floor: a `floor_request` whose rank is ≤ the
/// current floor is a no-op (pinned by
/// `agent_floor_request_lower_than_producer_is_ignored`).
fn apply_floor_raise(ctx: &mut TaskContext, plan: &Plan) -> bool {
    if let Some(req) = plan.floor_request {
        if req.rank() > ctx.classification_floor.rank() {
            ctx.classification_floor = req;
            ctx.classification_floor_source = ClassificationFloorSource::AgentRaised;
            ctx.classification_floor_signals.clear();
            return true;
        }
    }
    false
}
```

And update the tests to call `apply_floor_raise` directly (drop the `_for_test` helper).

- [ ] **Step 3: Wire `apply_floor_raise` into the inner loop**

In `core/src/scheduler/inner_loop.rs::run_to_terminal`, find the section right after `plan_count += 1` and the DB mirror (line ~201), and BEFORE `write_audit_plan_formulate` (line ~203). Insert:

```rust
        // Agent-side floor-raise: if the plan requests a higher floor than
        // the producer set, elevate ctx before the audit row is written and
        // before the reviewer chain runs (so DP sees the elevated floor).
        let raised = apply_floor_raise(&mut ctx, &plan);
        if raised {
            tracing::info!(
                task_id = ctx.task_id,
                plan_count = ctx.plan_count,
                new_floor = ctx.classification_floor.as_pascal_str(),
                "agent raised classification floor"
            );
        }

        write_audit_plan_formulate(pool, &ctx, &plan, &meta).await?;
```

(The `let raised = ...` is OK to read even when unused; tracing::info is cheap.)

- [ ] **Step 4: Run tests; verify GREEN**

```sh
cargo test -p hhagent-core scheduler::inner_loop:: 2>&1 | grep -E "test result|FAIL" | tail -5
cargo test --workspace 2>&1 | grep -E "test result" | awk '
  /test result/ { match($0, /[0-9]+ passed/); p += substr($0, RSTART, RLENGTH-7)+0
                  match($0, /[0-9]+ failed/); f += substr($0, RSTART, RLENGTH-7)+0 }
  END { print "passed=" p " failed=" f }'
```
Expected: 0 failed; +3 new tests. Total ~586.

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/inner_loop.rs
git commit -m "$(cat <<'EOF'
feat(scheduler): inner-loop floor-raise — Plan.floor_request takes effect

The inner loop now consults `plan.floor_request` after each plan
formulation, before the audit row is written and before the reviewer
chain runs. If the request is higher than the current floor, ctx is
elevated and the source flips to `AgentRaised` (signals cleared).
Lower-or-equal requests are no-ops — the agent can RAISE but never
LOWER the producer-set floor.

DeterministicPolicy (Stage 0) now sees the elevated floor for its I1
+ I2 invariant checks; the elevated floor sticks for subsequent plan
iterations in the same task.

+3 unit tests pinning:
- higher request elevates ctx and flips source to AgentRaised
- lower request is ignored
- equal-rank request is a no-op

tracing::info! line on successful raise so operators grepping the
journal see the elevation event.

Workspace test count 583 → 586.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: `runner.rs` reads source + signals from `task.payload`

**Files:**
- Modify: `core/src/scheduler/runner.rs` (parse new fields)

The runner is the canonical site that translates DB-shaped payload into a typed `TaskContext`. Today it reads `classification_floor` and `max_plans` from `task.payload`. This task adds the two new fields.

- [ ] **Step 1: Write the failing integration test (the runner change is integration-only)**

Skip a unit test for `runner.rs` — the function is tightly coupled to a real DB. The behaviour is exercised by the integration test added in Task 8 (`agent_raise_chain_blocks_low_step`) plus the existing `cli_ask_e2e` happy path. Just write the production code carefully and rely on those tests.

- [ ] **Step 2: Add the read paths**

In `core/src/scheduler/runner.rs::run_inner_loop_for_task` (find the existing `classification_floor` block around line 283-298), add immediately after that block:

```rust
    // Provenance: source defaults to "default" when absent. An
    // unrecognised value is a producer bug — fail closed parallel to
    // classification_floor.
    let classification_floor_source = match task.payload.get("classification_floor_source") {
        None => crate::scheduler::inner_loop::ClassificationFloorSource::Default,
        Some(v) => {
            let Some(s) = v.as_str() else {
                return failed_result(format!(
                    "classification_floor_source in payload is not a string: {v:?}"
                ));
            };
            // Wire form is lowercase snake_case via serde rename. Use
            // the same `from_str` pattern as classification_floor.
            match serde_json::from_str::<crate::scheduler::inner_loop::ClassificationFloorSource>(
                &format!("\"{}\"", s)
            ) {
                Ok(src) => src,
                Err(_) => return failed_result(format!(
                    "unknown classification_floor_source: {s:?} (expected one of \
                     operator, cli_inferred, agent_raised, default)"
                )),
            }
        }
    };
    // Signals: empty array iff absent or not an array. Each entry must
    // be a string; non-string entries are skipped silently (better than
    // failing the task on a non-load-bearing presentation field).
    let classification_floor_signals: Vec<String> = task.payload
        .get("classification_floor_signals")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect())
        .unwrap_or_default();
```

Then thread the two fields into the `TaskContext { ... }` constructor below (line ~309-319):

```rust
    let ctx = TaskContext {
        task_id: task.id,
        lane: task.lane,
        instruction,
        classification_floor,
        classification_floor_source,
        classification_floor_signals,
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: max_plans_override,
    };
```

- [ ] **Step 3: Confirm the build is green**

```sh
cargo build --workspace 2>&1 | tail -3
cargo test --workspace 2>&1 | grep -E "test result|FAIL" | tail -5
```
Expected: 0 failed; same 586 (no new tests in this task).

- [ ] **Step 4: Commit**

```bash
git add core/src/scheduler/runner.rs
git commit -m "$(cat <<'EOF'
feat(scheduler): runner reads classification_floor_source + signals from payload

`run_inner_loop_for_task` now extracts the two new provenance fields
from `task.payload` and threads them into `TaskContext`. Source
defaults to `Default` when absent; unrecognised value is a hard error
(parallel to the existing classification_floor handling).

Signals default to an empty array on any unparseable shape — these
are presentation metadata, not security-load-bearing, so soft failure
is appropriate.

No new tests in this task: the runner is exercised by the integration
tests in subsequent tasks.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: `hhagent-cli ask` — wire `infer_floor` into the producer path

**Files:**
- Modify: `core/src/bin/hhagent-cli.rs` (`ask_async` payload builder + `tracing::warn!` + 3 unit tests)

This task wires the classifier into the CLI submission path. After this, real prompts run through `infer_floor` and the resulting source + signals land in `task.payload`.

- [ ] **Step 1: Write the failing unit tests**

Add a new `cfg(test) mod ask_payload_tests` at the bottom of `core/src/bin/hhagent-cli.rs`. The CLI is a binary, so testing the payload-building logic in isolation is the cleanest seam. Refactor the floor-resolution + payload-key write into a pure helper first:

```rust
/// Pure builder for the producer-side payload entries that this slice
/// adds. Extracted from `ask_async` so the wire shape is unit-testable
/// without spinning up a DB.
///
/// Returns `(class, source, signals)` where:
/// - `class` is the floor that will be written into the payload.
/// - `source` is the provenance tag.
/// - `signals` is the matched-pattern tags (empty unless source is
///   CliInferred).
///
/// When `operator_flag` is Some, the operator wins; if inference would
/// have elevated above the operator's value, the `warn_on_suppress`
/// callback fires (in production: a `tracing::warn!`).
fn resolve_floor_for_submission(
    instruction: &str,
    operator_flag: Option<hhagent_core::cassandra::DataClass>,
    warn_on_suppress: &mut dyn FnMut(hhagent_core::cassandra::DataClass, &[&'static str]),
) -> (
    hhagent_core::cassandra::DataClass,
    hhagent_core::scheduler::inner_loop::ClassificationFloorSource,
    Vec<&'static str>,
) {
    use hhagent_core::cassandra::DataClass;
    use hhagent_core::classification_inference::infer_floor;
    use hhagent_core::scheduler::inner_loop::ClassificationFloorSource as Src;

    if let Some(op) = operator_flag {
        // Operator wins. Optionally warn if inference would have elevated.
        let inferred = infer_floor(instruction);
        if inferred.class.rank() > op.rank() {
            warn_on_suppress(inferred.class, &inferred.signals);
        }
        return (op, Src::Operator, vec![]);
    }
    let inferred = infer_floor(instruction);
    if inferred.class == DataClass::Public && inferred.signals.is_empty() {
        return (DataClass::Public, Src::Default, vec![]);
    }
    (inferred.class, Src::CliInferred, inferred.signals)
}

#[cfg(test)]
mod ask_payload_tests {
    use super::resolve_floor_for_submission;
    use hhagent_core::cassandra::DataClass;
    use hhagent_core::scheduler::inner_loop::ClassificationFloorSource as Src;

    #[test]
    fn no_operator_flag_no_signals_returns_default() {
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "How do I write a quicksort in Rust?",
            None,
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::Public);
        assert_eq!(src, Src::Default);
        assert!(sigs.is_empty());
        assert!(!suppressed);
    }

    #[test]
    fn no_operator_flag_clinical_signals_returns_cli_inferred() {
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "Translate the patient's pathology report.",
            None,
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::ClinicalConfidential);
        assert_eq!(src, Src::CliInferred);
        assert!(sigs.contains(&"patient"));
        assert!(sigs.contains(&"pathology"));
        assert!(!suppressed);
    }

    #[test]
    fn operator_flag_wins_and_warns_when_inference_would_elevate() {
        let mut suppressed_class: Option<DataClass> = None;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "Translate the patient's pathology report.",
            Some(DataClass::Public),  // operator explicitly pinned Public
            &mut |c, _| { suppressed_class = Some(c); },
        );
        assert_eq!(cls, DataClass::Public, "operator wins");
        assert_eq!(src, Src::Operator);
        assert!(sigs.is_empty());
        assert_eq!(suppressed_class, Some(DataClass::ClinicalConfidential),
            "warn should fire because inference would have elevated to Clinical");
    }

    #[test]
    fn operator_flag_wins_and_no_warn_when_inference_does_not_elevate() {
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "How do I write a quicksort in Rust?",
            Some(DataClass::ClinicalConfidential),
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::ClinicalConfidential);
        assert_eq!(src, Src::Operator);
        assert!(sigs.is_empty());
        assert!(!suppressed, "inference inferred Public (not elevating); no warn");
    }
}
```

- [ ] **Step 2: Run; verify RED**

```sh
cargo test -p hhagent-core --bin hhagent-cli ask_payload_tests 2>&1 | tail -10
```
Expected: build fails — `resolve_floor_for_submission` doesn't exist yet.

- [ ] **Step 3: Add `resolve_floor_for_submission` to `core/src/bin/hhagent-cli.rs`**

Add the helper above the `ask_async` function. Just paste the implementation from the test block above (the production code is identical to what the test imports).

- [ ] **Step 4: Wire `resolve_floor_for_submission` into `ask_async`**

In `ask_async`, replace the existing payload-building block (lines ~359-378). Find:
```rust
    let mut payload = serde_json::json!({"instruction": instruction, "kind": "ask"});
    if let Some(f) = floor {
        // ...
        let v = serde_json::to_value(f).expect("DataClass serialises");
        if let serde_json::Value::Object(ref mut m) = payload {
            m.insert("classification_floor".to_string(), v);
        }
    }
```

Replace with:
```rust
    let mut payload = serde_json::json!({"instruction": instruction, "kind": "ask"});
    let mut suppression_fires: Option<(hhagent_core::cassandra::DataClass, Vec<&'static str>)> = None;
    let (resolved_floor, resolved_source, resolved_signals) =
        resolve_floor_for_submission(&instruction, floor, &mut |c, s| {
            suppression_fires = Some((c, s.to_vec()));
        });
    if let Some((c, sigs)) = &suppression_fires {
        tracing::warn!(
            inferred_class = c.as_pascal_str(),
            inferred_signals = ?sigs,
            operator_floor = floor.map(|f| f.as_pascal_str()).unwrap_or(""),
            "--classification-floor explicitly suppressed an elevation the keyword classifier would have made"
        );
    }
    if let serde_json::Value::Object(ref mut m) = payload {
        m.insert("classification_floor".into(),
                 serde_json::to_value(resolved_floor).expect("DataClass serialises"));
        m.insert("classification_floor_source".into(),
                 serde_json::json!(resolved_source.as_snake_str()));
        if !resolved_signals.is_empty() {
            m.insert("classification_floor_signals".into(),
                     serde_json::json!(resolved_signals));
        }
    }
```

- [ ] **Step 5: Run tests; verify GREEN**

```sh
cargo build --workspace 2>&1 | tail -5
cargo test -p hhagent-core --bin hhagent-cli 2>&1 | grep -E "test result" | tail -3
cargo test --workspace 2>&1 | grep -E "test result|FAIL" | tail -5
```
Expected: 0 failed; +4 new tests. Total ~590.

- [ ] **Step 6: Commit**

```bash
git add core/src/bin/hhagent-cli.rs
git commit -m "$(cat <<'EOF'
feat(cli): wire classification_inference into `hhagent-cli ask`

Adds `resolve_floor_for_submission` pure helper that maps
(instruction, operator_flag) to (floor, source, signals):

- Operator flag wins unconditionally. If inference would have elevated
  above the operator's value, a `tracing::warn!` line fires so the
  suppression is operator-visible in the daemon journal.
- No operator flag + no signals → (Public, Default, []).
- No operator flag + matched signals → (inferred, CliInferred, signals).

`ask_async` now:
- Writes `classification_floor` (existing) + `classification_floor_source`
  (new) + `classification_floor_signals` (new, only when source is
  cli_inferred).
- Emits the suppression warn when the operator-explicit floor silences
  inference.

+4 unit tests pin the helper's four cases (no-op/elevate/operator-wins/
operator-wins-no-warn).

Workspace test count 586 → 590.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Planner-prompt update

**Files:**
- Modify: `prompts/agent_planner.md`

The planner needs to know about the new `floor_request` field. The `agent_prompts` SHA-256 ledger (migrations 0006 + 0011) picks up the new hash on next daemon start automatically.

- [ ] **Step 1: Update the JSON-schema example**

Find the existing JSON example (around line 37-56 of `prompts/agent_planner.md`). Add `"floor_request": null,` between `"refused": null,` and `"data_ceiling": ...`:

```diff
     "result":      null,
     "refused":     null,
+    "floor_request": null,
     "data_ceiling": "<Public | Personal | ClinicalConfidential | Secret>"
 }
```

- [ ] **Step 2: Add an explanatory paragraph**

Find the paragraph after the schema (the one explaining `refused`). Add a new paragraph after it:

```markdown
The `floor_request` field is normally `null`. Populate it as a
`DataClass` string ("Personal" | "ClinicalConfidential" | "Secret")
if, while planning, you observe that the work involves data above the
floor the producer set. This RAISES the task's classification floor
for all subsequent reviewer checks and plan iterations. You cannot
LOWER the floor this way — a request below the current floor is
silently a no-op. This is distinct from `data_ceiling` (which records
the highest class of data the plan TOUCHES). `floor_request` is the
agent's view of how strictly the OUTPUTS should be governed.
```

- [ ] **Step 3: Verify no compile breakage (build only — prompt is data, not code)**

```sh
cargo build --workspace 2>&1 | tail -3
```
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add prompts/agent_planner.md
git commit -m "$(cat <<'EOF'
feat(prompt): planner — add Plan.floor_request to the input schema

The planner can now request a higher classification floor for the
rest of the task when it observes that the work involves data above
the producer-set floor. JSON-schema example adds the `floor_request`
field; one new paragraph explains the semantic distinction from
`data_ceiling` (touches vs. governs outputs) and pins the raise-only
constraint.

The `agent_prompts` SHA-256 ledger records the new hash on next
daemon start automatically (no migration).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: Integration tests

**Files:**
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` (+1 scenario)
- Modify: `core/tests/cli_ask_e2e.rs` (extend payload assertions)

End-to-end verification:
1. Agent emits a plan with `floor_request: ClinicalConfidential` over a task submitted with `floor=Public` + a single step classified as Public. DP's I2 fires; the chain produces `Verdict::Block`.
2. `cli_ask_e2e` happy path: assert the new `classification_floor_source` key on `agent/plan.formulate` rows.

- [ ] **Step 1: Write the failing scheduler-inner-loop integration scenario**

In `core/tests/scheduler_inner_loop_e2e.rs`, find the existing scenarios (e.g. `refusal_plan_terminates_with_state_refused`). Add a new scenario near the end of the test fn. The pattern is "scripted formulator emits a plan with floor_request + a step below the elevated floor; assert Outcome::Failed/Blocked":

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_floor_raise_chain_blocks_low_classification_step() -> anyhow::Result<()> {
    let Some(_pg) = bring_up_pg_cluster_or_skip("scheduler_inner_loop_e2e") else { return Ok(()); };
    // ... per existing scaffolding pattern in this file:
    //   - bring up PG
    //   - probe, runtime pool
    //   - seed a task with payload {instruction, classification_floor: "Public"}
    //   - install a ScriptedFormulator that returns ONE plan:
    //       floor_request: Some(ClinicalConfidential),
    //       steps: [{classification: Public, ...}],
    //       data_ceiling: ClinicalConfidential,
    //   - call run_to_terminal
    //   - assert Outcome::Blocked or Outcome::Failed with a reason mentioning
    //     "step_classification_below_floor" (the DP I2 reason_tag).
    //   - assert that the audit_log has at least one `agent/plan.formulate`
    //     row carrying classification_floor: "ClinicalConfidential" and
    //     classification_floor_source: "agent_raised".
    // See the existing `refusal_plan_terminates_with_state_refused` test
    // for the exact harness shape; copy it and adjust.
    // [Body deliberately not pre-written here: it depends on the existing
    //  Scripted... stage signatures; engineer reuses the pattern.]
    Ok(())
}
```

Engineer note: the file currently has multiple integration scenarios; mirror one of them (e.g., the constitutional-refusal scenario) and adjust the plan body and assertion to match the spec. The key new bits are setting `floor_request: Some(ClinicalConfidential)` on the formulated plan and asserting that the resulting `agent/plan.formulate` audit row carries `classification_floor: "ClinicalConfidential"` + `classification_floor_source: "agent_raised"`.

- [ ] **Step 2: Run the new scenario; verify RED (or skip on no-PG)**

```sh
cargo test -p hhagent-core --test scheduler_inner_loop_e2e agent_floor_raise 2>&1 | tail -15
```
Expected on a host with PG: test fails because some assertion needs the production code to be correct — confirm the failure is in the assertion path, not in compile/setup.

- [ ] **Step 3: Iterate until GREEN**

Adjust the test (or fix any production-code regression surfaced).

- [ ] **Step 4: Extend `cli_ask_e2e.rs` happy-path assertions**

In `core/tests/cli_ask_e2e.rs`, find the existing assertion block that walks the `agent/plan.formulate` audit rows. Add (or extend) an assertion that each row carries `classification_floor_source` and (when the prompt has clinical keywords) `classification_floor_signals`. The happy path uses `"marker"` as the instruction → expect `classification_floor_source: "default"` and no signals.

```rust
// Pin the new provenance keys (Slice — automatic floor inference, 2026-05-16).
for row in &plan_formulate_rows {
    let src = row.payload.get("classification_floor_source")
        .and_then(|v| v.as_str())
        .expect("plan.formulate row must carry classification_floor_source");
    // happy-path instruction is "marker"; nothing matches the catalogue.
    assert_eq!(src, "default", "expected source=default for non-clinical prompt");
    assert!(row.payload.get("classification_floor_signals").is_none(),
        "default source must omit the signals key");
}
```

- [ ] **Step 5: Run all tests; verify GREEN**

```sh
cargo test --workspace 2>&1 | grep -E "test result" | awk '
  /test result/ { match($0, /[0-9]+ passed/); p += substr($0, RSTART, RLENGTH-7)+0
                  match($0, /[0-9]+ failed/); f += substr($0, RSTART, RLENGTH-7)+0 }
  END { print "passed=" p " failed=" f }'
```
Expected: 0 failed; +1 new test (the e2e scenario). Total ~591.

- [ ] **Step 6: Commit**

```bash
git add core/tests/scheduler_inner_loop_e2e.rs core/tests/cli_ask_e2e.rs
git commit -m "$(cat <<'EOF'
test(scheduler,cli): integration tests for automatic floor inference

Two integration-level pins for the floor-inference slice:

1. `scheduler_inner_loop_e2e::agent_floor_raise_chain_blocks_low_classification_step`
   — scripted formulator emits a plan with floor_request: Clinical over
   a Public-floored task + a Public step. DP's I2 fires; the chain
   blocks. Asserts the audit row carries source=agent_raised.

2. `cli_ask_e2e` happy-path extension: assert
   classification_floor_source=default for the non-clinical "marker"
   instruction, and that classification_floor_signals is absent.

Workspace test count 590 → 591.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: HANDOVER + ROADMAP end-of-session update

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

Document the slice for the next session per CLAUDE.md rule #8.

- [ ] **Step 1: Update HANDOVER header**

Replace the existing `**Last updated**` + `**Last commit (main)**` + session-state lines with a new header reflecting the new branch state. Format follows the existing convention — look at the previous session's header in the file as a template.

- [ ] **Step 2: Add a "Recently completed (this session)" entry**

Insert at the top of the existing Recently-completed entries (after the header block). Include:
- Branch name and base commit.
- Tasks 1-8 summary.
- Test-count delta from baseline 557 to final.
- File-touched list.
- What this slice deliberately does NOT do (mirror the spec's non-goals).
- Any follow-ups discovered during implementation.

- [ ] **Step 3: Tick the matching item in ROADMAP.md**

Find the Phase 1 item that corresponds to "automatic floor inference" (most likely under the constitutional/DP rule sequence). Change `[ ]` → `[x]` and add a one-line summary plus the merge commit hash. If no exact pre-existing item, add one under the appropriate phase.

- [ ] **Step 4: Run final test sweep + commit**

```sh
cargo test --workspace 2>&1 | grep -E "test result|FAIL" | tail -5
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): automatic classification-floor inference shipped

End-of-session update for the feat/automatic-floor-inference branch.
Hybrid design: CLI-side keyword classifier as primary inference site
+ Plan.floor_request raise-only channel as defence in depth.

Per-class catalogues (Secret / Clinical / Personal) with tiered scan;
contains_word whole-word matching mirrors the ConstitutionalGuard
post-review precedent. Provenance metadata
(classification_floor_source + classification_floor_signals) lands as
pure-additive keys on task.payload and on the agent/plan.formulate
audit-row payload (14 keys default, 15 when cli_inferred).

Test count 557 → 591 (+34 across all tasks).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

**Spec coverage** (cross-check against the spec sections):

| Spec section | Task covering it |
| ------------ | ---------------- |
| 1. CLI-side keyword classifier | Task 2 |
| 2. CLI wiring (`run_ask` / payload) | Task 6 |
| 3. Agent-side `Plan.floor_request` | Task 1 (field) + Task 4 (consumer) |
| 4. Inner-loop floor-raise wiring | Task 4 |
| 5. Audit-row payload provenance | Task 3 (mechanism) + Task 6 (CLI writes) |
| 6. Test coverage (unit + integration) | Tasks 1-6 (unit) + Task 8 (integration) |
| Planner-prompt update | Task 7 |
| HANDOVER + ROADMAP | Task 9 |

Every spec requirement maps to a task; no gaps.

**Placeholder scan:** Task 8 deliberately leaves the inner-loop integration test body skeletal because it depends on existing test scaffolding the engineer will copy from. The shape and assertions are pinned in the task description; the wiring is mechanical. This is intentional, not a placeholder failure.

**Type consistency:**
- `InferredFloor { class, signals }` — Task 2 defines it, Task 6 consumes it.
- `ClassificationFloorSource { Operator, CliInferred, AgentRaised, Default }` — Task 3 defines it (with `as_snake_str` method), Tasks 4/5/6 consume it.
- `Plan.floor_request: Option<DataClass>` — Task 1 defines it, Task 4 consumes it.
- `TaskContext.classification_floor_source` + `TaskContext.classification_floor_signals` — Task 3 defines them; Task 4 reads them; Task 5 writes them at task entry.
- `build_plan_formulate_payload` signature — Task 3 widens it to 7 params; Task 4 inherits the wider shape.
- `resolve_floor_for_submission` — Task 6 defines it; only Task 6 uses it.

All type names, method names, and call signatures are consistent across tasks.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-16-automatic-floor-inference.md`. Two execution options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task, review between tasks, fast iteration
2. **Inline Execution** — execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?
