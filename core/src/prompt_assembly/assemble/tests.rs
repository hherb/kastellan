    use super::*;
    use crate::recall_assembly::RecalledContext;
    use kastellan_db::memories::{Memory, MemoryLayer};
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
            &[],
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
        let out = assemble_system_prompt(&l0, &l1, &[], &recalled, "BASE");

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
        let out = assemble_system_prompt(&[], &[], &[], &recalled, "BASE");
        assert!(out.contains("- body with <closing> tag\n"),
                "body must pass through verbatim; got:\n{out}");
    }

    #[test]
    fn l0_only_skips_l1_section() {
        let l0 = vec![mem(1, "rule one", MemoryLayer::Meta)];
        let out = assemble_system_prompt(&l0, &[], &[], &RecalledContext::empty(), "BASE");
        assert!(out.starts_with("<l0_meta_rules>\n"), "L0 section first; got:\n{out}");
        assert!(!out.contains("<l1_insights>"), "L1 must be skipped when empty; got:\n{out}");
        assert!(out.contains("<base>\nBASE\n</base>\n"), "base must be present; got:\n{out}");
    }

    #[test]
    fn l1_only_skips_l0_section() {
        let l1 = vec![mem(1, "insight one", MemoryLayer::Index)];
        let out = assemble_system_prompt(&[], &l1, &[], &RecalledContext::empty(), "BASE");
        assert!(!out.contains("<l0_meta_rules>"), "L0 must be skipped when empty; got:\n{out}");
        assert!(out.contains("<l1_insights>\n- insight one\n</l1_insights>"),
                "L1 section present; got:\n{out}");
    }

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

    #[test]
    fn every_row_renders_with_bullet_prefix() {
        let l0 = vec![
            mem(1, "first", MemoryLayer::Meta),
            mem(2, "second", MemoryLayer::Meta),
            mem(3, "third", MemoryLayer::Meta),
        ];
        let out = assemble_system_prompt(&l0, &[], &[], &RecalledContext::empty(), "BASE");
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
        let out = assemble_system_prompt(&l0, &[], &[], &RecalledContext::empty(), "BASE");
        assert!(out.contains("- line one\nline two\n"),
                "multi-line body must pass through verbatim; got:\n{out}");
    }

    #[test]
    fn body_with_xml_chars_is_not_escaped() {
        // Operator-curated content. < and > pass through. A future
        // refactor that adds HTML escaping would break this test
        // deliberately so the team can re-evaluate the trust posture.
        let l0 = vec![mem(1, "guard <secret> and </tag>", MemoryLayer::Meta)];
        let out = assemble_system_prompt(&l0, &[], &[], &RecalledContext::empty(), "BASE");
        assert!(out.contains("- guard <secret> and </tag>\n"),
                "XML chars must pass through; got:\n{out}");
    }

    #[test]
    fn output_is_deterministic_for_same_inputs() {
        let l0 = vec![mem(1, "rule one", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "insight", MemoryLayer::Index)];
        let a = assemble_system_prompt(&l0, &l1, &[], &RecalledContext::empty(), "BASE");
        let b = assemble_system_prompt(&l0, &l1, &[], &RecalledContext::empty(), "BASE");
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
        let out = assemble_system_prompt(&l0, &[], &[], &RecalledContext::empty(), "BASE");
        let idx_a = out.find("- third-newest").expect("first row present");
        let idx_b = out.find("- second-newest").expect("second row present");
        let idx_c = out.find("- oldest").expect("third row present");
        assert!(idx_a < idx_b && idx_b < idx_c,
                "rows must appear in input order; offsets {idx_a}/{idx_b}/{idx_c}");
    }

    #[test]
    fn base_trailing_newlines_are_normalized_to_exactly_one() {
        // Whatever the caller passes — zero, one, or many trailing
        // newlines — the assembler collapses to exactly one before
        // `</base>\n`. The closing tag always sits on its own line with
        // no blank line in front of it, regardless of how the prompt
        // file ends. This keeps the output deterministic across
        // editor / prompt-file conventions.
        let out_no_nl = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "no trailing nl");
        let out_one_nl = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "with trailing nl\n");
        let out_two_nl = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "with two trailing nl\n\n");
        let out_many_nl = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "many trailing nls\n\n\n\n");
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
    }

    fn surfaced(name: &str, desc: &str) -> SurfacedSkill {
        SurfacedSkill { name: name.into(), description: desc.into(), params: vec![], invocable: false }
    }

    #[test]
    fn skills_block_present_with_one_skill() {
        let skills = vec![surfaced("foo", "does foo.")];
        let out = assemble_system_prompt(&[], &[], &skills, &RecalledContext::empty(), "BASE");
        assert!(
            out.contains("<skills>\n- foo: does foo.\n</skills>\n\n"),
            "skills block rendered; got:\n{out}"
        );
    }

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

    #[test]
    fn skills_render_after_l1_and_before_recalled() {
        let l1 = vec![mem(2, "L1 INSIGHT", MemoryLayer::Index)];
        let recalled = RecalledContext::new(
            vec![100],
            vec!["RECALL ONE".into()],
            "f".repeat(64),
        );
        let skills = vec![surfaced("skillname", "skill desc.")];
        let out = assemble_system_prompt(&[], &l1, &skills, &recalled, "BASE");
        let l1_end = out.find("</l1_insights>").expect("l1 end tag");
        let skills_start = out.find("<skills>").expect("skills start tag");
        let recalled_start = out.find("<recalled>").expect("recalled start tag");
        assert!(l1_end < skills_start, "skills must come after l1; got:\n{out}");
        assert!(skills_start < recalled_start, "skills must come before recalled; got:\n{out}");
    }

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
        // tool/method constants, every field the mechanism emits, and the byte
        // caps it enforces, so a change to those shapes fails this test instead
        // of silently leaving the planner with a stale protocol description.
        use crate::handoff::{
            build_handoff_placeholder, FetchResult, HandoffCache, HandoffRef, MAX_FETCH_BYTES,
            SUMMARY_HEAD_BYTES,
        };
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

        // Every field a real `fetch` response carries must be named, so a
        // renamed return key (e.g. `data` -> `bytes`) fails here rather than
        // misinforming the planner about the shape it gets back.
        let cache = HandoffCache::new();
        let value = serde_json::json!({ "k": "v".repeat(100) });
        let stashed = cache.stash_if_oversized(1, &value, 8).expect("stashed (cap=8)");
        let params = serde_json::json!({
            "handoff_ref": stashed.handoff_ref.as_str(), "offset": 0, "len": 1_000_000,
        });
        match cache.fetch(1, &params) {
            FetchResult::Ok(resp) => {
                for key in resp.as_object().expect("fetch response is a JSON object").keys() {
                    assert!(
                        out.contains(key.as_str()),
                        "handoff block must name fetch return field {key:?}; got:\n{out}"
                    );
                }
            }
            other => panic!("expected Ok fetch, got {other:?}"),
        }

        // Byte caps are interpolated from their constants (in KiB), not typed by
        // hand — a retuned cap rewrites the prose and this assertion follows it.
        assert!(
            out.contains(&format!("{} KiB", SUMMARY_HEAD_BYTES / 1024)),
            "block must show the summary-head size from SUMMARY_HEAD_BYTES; got:\n{out}"
        );
        assert!(
            out.contains(&format!("{} KiB", MAX_FETCH_BYTES / 1024)),
            "block must show the fetch cap from MAX_FETCH_BYTES; got:\n{out}"
        );
    }

    #[test]
    fn handoff_block_is_deterministic() {
        let a = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE");
        let b = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE");
        assert_eq!(a, b, "same inputs must yield identical bytes");
    }
