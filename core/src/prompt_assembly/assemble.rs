//! Pure prompt assembler. No I/O, no async, no errors.
//!
//! Output framing (always L0 → L1 → recalled → base in this order):
//!
//! ```text
//! <l0_meta_rules>
//! - {body of newest L0 row per l0_rule_id}
//! - {next L0 row body}
//! </l0_meta_rules>
//!
//! <l1_insights>
//! - {body of L1 row, newest-first}
//! </l1_insights>
//!
//! <recalled>
//! - {body of recall row #1 (RRF-ranked-first)}
//! - {body of recall row #2}
//! </recalled>
//!
//! <base>
//! {agent_planner.md verbatim}
//! </base>
//! ```
//!
//! Rules:
//!
//! 1. Empty layers omit their entire tag block — no `<l1_insights>`
//!    when L1 has zero rows. The `<base>` block is always present.
//! 2. One blank line between sections.
//! 3. Each row renders as `- {body}` (one row per line).
//! 4. Bodies pass through verbatim (no HTML-style escaping of `<` `>`).
//!    Operators curate L0/L1 content; trust posture matches the rest
//!    of the memory store.
//!
//!    **SAFETY — prompt-injection seam.** This contract holds *only*
//!    while every body fed into the assembler is operator-curated. If
//!    any agent-writable layer (e.g. a future L1 promotion writer that
//!    sources content from agent output) flows rows in here, the lack
//!    of escaping becomes a prompt-injection vector: agent-controlled
//!    text could close the `<l1_insights>` block and inject new
//!    framing the model trusts at meta-rule level. See the L1-writer
//!    follow-up in `docs/devel/handovers/HANDOVER.md` ("recall lane
//!    wiring" / future "L3/L4 writers" — if any *promotion* writer is
//!    added that pulls from agent-authored content, revisit this
//!    contract before merging). Threat-model reference:
//!    `docs/threat-model.md` (LLM-compromise scenario).
//!
//!    **Note for `<recalled>` bodies:** Unlike L0/L1, recall bodies are
//!    *not* operator-curated — any process with `INSERT` privilege on
//!    `memories` writes them, and recall surfaces whatever the lanes
//!    return. Phase 1 trusts the model's tokeniser for recall rows on
//!    the same basis as L0/L1; if an adversarial-input scenario is
//!    identified (e.g. attacker-supplied content in `memories` flowing
//!    here via the recall lane), sanitise before passing to this
//!    function. The `recalled_block_passes_xml_chars_in_body_verbatim`
//!    test pins the current pass-through posture so any future
//!    sanitiser is a deliberate behaviour change, not a silent fix.
//! 5. The `<recalled>` block is omitted when the
//!    [`RecalledContext`] is empty (the
//!    failure-degraded state). Recall is enrichment, not policy —
//!    this asymmetry is deliberate.
//! 6. Deterministic: same `(l0, l1, recalled, base)` produces the
//!    same bytes.

use crate::recall_assembly::RecalledContext;
use hhagent_db::memories::Memory;

/// Render the supplied memory slices, recall context, and base prompt
/// into a single LLM-ready system message.
///
/// See the module-level docstring for the framing rules. The
/// `recalled` argument follows L1 and precedes `base`; an empty
/// [`RecalledContext`] omits the `<recalled>` tag entirely so the
/// output is byte-identical to the v1 (no-recall) assembler.
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    recalled: &RecalledContext,
    base: &str,
) -> String {
    let mut out = String::new();

    if !l0.is_empty() {
        out.push_str("<l0_meta_rules>\n");
        for row in l0 {
            out.push_str("- ");
            out.push_str(&row.body);
            out.push('\n');
        }
        out.push_str("</l0_meta_rules>\n\n");
    }

    if !l1.is_empty() {
        out.push_str("<l1_insights>\n");
        for row in l1 {
            out.push_str("- ");
            out.push_str(&row.body);
            out.push('\n');
        }
        out.push_str("</l1_insights>\n\n");
    }

    if !recalled.is_empty() {
        out.push_str("<recalled>\n");
        for body in &recalled.bodies {
            out.push_str("- ");
            out.push_str(body);
            out.push('\n');
        }
        out.push_str("</recalled>\n\n");
    }

    out.push_str("<base>\n");
    out.push_str(base);
    if !base.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("</base>\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall_assembly::RecalledContext;
    use hhagent_db::memories::{Memory, MemoryLayer};
    use time::OffsetDateTime;

    /// Construct a minimal `Memory` for tests. `id` is set to a stable
    /// 1-based index so test failures are debuggable; `created_at` is
    /// pinned to the Unix epoch so the value is deterministic.
    fn mem(id: i64, body: &str, layer: MemoryLayer) -> Memory {
        Memory {
            id,
            body: body.to_string(),
            metadata: serde_json::json!({}),
            layer,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn empty_l0_l1_recalled_emits_base_block_only() {
        let out = assemble_system_prompt(
            &[],
            &[],
            &RecalledContext::empty(),
            "BASE BODY",
        );
        assert_eq!(
            out,
            "<base>\nBASE BODY\n</base>\n",
            "no L0/L1/recalled → base block alone; got:\n{out}"
        );
    }

    #[test]
    fn empty_recalled_omits_recalled_section() {
        // Same input as the L0+L1 happy-path tests below — proves the
        // empty `RecalledContext` produces byte-identical output to the
        // v1 assembler (regression pin for the migration).
        let l0 = vec![mem(1, "L0 RULE ONE", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "L1 INSIGHT ONE", MemoryLayer::Index)];
        let out = assemble_system_prompt(
            &l0,
            &l1,
            &RecalledContext::empty(),
            "BASE BODY",
        );
        assert!(!out.contains("<recalled>"),
                "empty recalled context must not emit a <recalled> tag; got:\n{out}");
        assert!(out.contains("<l0_meta_rules>"), "L0 section still required");
        assert!(out.contains("<l1_insights>"), "L1 section still required");
    }

    #[test]
    fn renders_recalled_block_between_l1_and_base() {
        let l0 = vec![mem(1, "L0 RULE", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "L1 INSIGHT", MemoryLayer::Index)];
        let recalled = RecalledContext::new(
            vec![100, 101],
            vec!["RECALL ONE".into(), "RECALL TWO".into()],
            "f".repeat(64),
        );
        let out = assemble_system_prompt(&l0, &l1, &recalled, "BASE");

        // Positional ordering pin.
        let l0_end = out.find("</l0_meta_rules>").expect("L0 end tag");
        let l1_start = out.find("<l1_insights>").expect("L1 start tag");
        let l1_end = out.find("</l1_insights>").expect("L1 end tag");
        let recalled_start = out.find("<recalled>").expect("recalled start tag");
        let recalled_end = out.find("</recalled>").expect("recalled end tag");
        let base_start = out.find("<base>").expect("base start tag");

        assert!(l0_end < l1_start, "L0 must come before L1; out:\n{out}");
        assert!(l1_end < recalled_start, "L1 must come before recalled; out:\n{out}");
        assert!(recalled_end < base_start, "recalled must come before base; out:\n{out}");

        // Body rendering pin: one bullet per row.
        assert!(out.contains("<recalled>\n- RECALL ONE\n- RECALL TWO\n</recalled>"),
                "recalled rows must render `- {{body}}` newest-first; got:\n{out}");
    }

    #[test]
    fn recalled_block_passes_xml_chars_in_body_verbatim() {
        // Threat-model note: bodies are not operator-curated (any process
        // with INSERT on `memories` writes them), but Phase 1's posture
        // is to trust the model's tokeniser. Pin the pass-through so a
        // future "escape `<`" patch is a deliberate decision, not a
        // silent regression.
        let recalled = RecalledContext::new(
            vec![1],
            vec!["body with <closing> tag".into()],
            "0".repeat(64),
        );
        let out = assemble_system_prompt(&[], &[], &recalled, "BASE");
        assert!(out.contains("- body with <closing> tag\n"),
                "body must pass through verbatim; got:\n{out}");
    }

    #[test]
    fn l0_only_skips_l1_section() {
        let l0 = vec![mem(1, "rule one", MemoryLayer::Meta)];
        let out = assemble_system_prompt(&l0, &[], &RecalledContext::empty(), "BASE");
        assert!(out.starts_with("<l0_meta_rules>\n"), "L0 section first; got:\n{out}");
        assert!(!out.contains("<l1_insights>"), "L1 must be skipped when empty; got:\n{out}");
        assert!(out.contains("<base>\nBASE\n</base>\n"), "base must be present; got:\n{out}");
    }

    #[test]
    fn l1_only_skips_l0_section() {
        let l1 = vec![mem(1, "insight one", MemoryLayer::Index)];
        let out = assemble_system_prompt(&[], &l1, &RecalledContext::empty(), "BASE");
        assert!(!out.contains("<l0_meta_rules>"), "L0 must be skipped when empty; got:\n{out}");
        assert!(out.contains("<l1_insights>\n- insight one\n</l1_insights>"),
                "L1 section present; got:\n{out}");
    }

    #[test]
    fn both_layers_assembled_in_order_with_blank_separators() {
        let l0 = vec![mem(1, "rule one", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "insight one", MemoryLayer::Index)];
        let out = assemble_system_prompt(&l0, &l1, &RecalledContext::empty(), "BASE");
        let expected = concat!(
            "<l0_meta_rules>\n",
            "- rule one\n",
            "</l0_meta_rules>\n",
            "\n",
            "<l1_insights>\n",
            "- insight one\n",
            "</l1_insights>\n",
            "\n",
            "<base>\n",
            "BASE\n",
            "</base>\n",
        );
        assert_eq!(out, expected, "full shape pin");
    }

    #[test]
    fn every_row_renders_with_bullet_prefix() {
        let l0 = vec![
            mem(1, "first", MemoryLayer::Meta),
            mem(2, "second", MemoryLayer::Meta),
            mem(3, "third", MemoryLayer::Meta),
        ];
        let out = assemble_system_prompt(&l0, &[], &RecalledContext::empty(), "BASE");
        for needle in ["- first\n", "- second\n", "- third\n"] {
            assert!(out.contains(needle), "missing {needle:?} in {out}");
        }
    }

    #[test]
    fn multi_line_body_renders_verbatim_without_re_bulleting() {
        // A body with an internal newline is rendered as-is. The contract
        // is "bullet on the first line; continuation lines pass through"
        // — a future refactor that tries to indent continuation lines
        // would break this test deliberately.
        let l0 = vec![mem(1, "line one\nline two", MemoryLayer::Meta)];
        let out = assemble_system_prompt(&l0, &[], &RecalledContext::empty(), "BASE");
        assert!(out.contains("- line one\nline two\n"),
                "multi-line body must pass through verbatim; got:\n{out}");
    }

    #[test]
    fn body_with_xml_chars_is_not_escaped() {
        // Operator-curated content. < and > pass through. A future
        // refactor that adds HTML escaping would break this test
        // deliberately so the team can re-evaluate the trust posture.
        let l0 = vec![mem(1, "guard <secret> and </tag>", MemoryLayer::Meta)];
        let out = assemble_system_prompt(&l0, &[], &RecalledContext::empty(), "BASE");
        assert!(out.contains("- guard <secret> and </tag>\n"),
                "XML chars must pass through; got:\n{out}");
    }

    #[test]
    fn output_is_deterministic_for_same_inputs() {
        let l0 = vec![mem(1, "rule one", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "insight", MemoryLayer::Index)];
        let a = assemble_system_prompt(&l0, &l1, &RecalledContext::empty(), "BASE");
        let b = assemble_system_prompt(&l0, &l1, &RecalledContext::empty(), "BASE");
        assert_eq!(a, b, "same inputs must yield same bytes");
    }

    #[test]
    fn row_order_matches_input_order() {
        // The assembler does not re-sort. Callers are responsible for
        // input ordering (loaders return newest-first today).
        let l0 = vec![
            mem(3, "third-newest", MemoryLayer::Meta),
            mem(2, "second-newest", MemoryLayer::Meta),
            mem(1, "oldest", MemoryLayer::Meta),
        ];
        let out = assemble_system_prompt(&l0, &[], &RecalledContext::empty(), "BASE");
        let idx_a = out.find("- third-newest").expect("first row present");
        let idx_b = out.find("- second-newest").expect("second row present");
        let idx_c = out.find("- oldest").expect("third row present");
        assert!(idx_a < idx_b && idx_b < idx_c,
                "rows must appear in input order; offsets {idx_a}/{idx_b}/{idx_c}");
    }

    #[test]
    fn base_without_trailing_newline_is_normalized() {
        // If the caller passes a base prompt without a terminating
        // newline, the assembler inserts one before `</base>\n` so the
        // closing tag always sits on its own line. This keeps the
        // output shape stable regardless of how the prompt file ends.
        let out_no_nl = assemble_system_prompt(&[], &[], &RecalledContext::empty(), "no trailing nl");
        let out_with_nl = assemble_system_prompt(&[], &[], &RecalledContext::empty(), "with trailing nl\n");
        assert_eq!(
            out_no_nl, "<base>\nno trailing nl\n</base>\n",
            "no-trailing-newline input must be normalized; got {out_no_nl:?}"
        );
        assert_eq!(
            out_with_nl, "<base>\nwith trailing nl\n</base>\n",
            "with-trailing-newline input passes through; got {out_with_nl:?}"
        );
    }
}
