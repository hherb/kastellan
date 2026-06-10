//! Fixture-driven regression for the per-tool injection guard (issue #142).
//!
//! Exercises the public `screen_with_profile` API against committed
//! corpora: realistic benign documentation that carries chat-template
//! tokens (must Allow under Relaxed, would Block under Strict) and genuine
//! corroborated injections (must Block under both profiles). The fixtures
//! are the source-of-truth regression pins; a live spot-check that they
//! match reality lives in `web_fetch_e2e.rs` (an `#[ignore]` real fetch).

use kastellan_core::cassandra::injection_guard::{
    extract_scannable_text, screen_with_profile, GuardProfile, InjectionDecision, SCAN_BYTE_CAP,
};
use std::path::PathBuf;

fn fixture(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/injection_guard");
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

const BENIGN_CHAT_TEMPLATE_DOCS: &[&str] = &[
    "benign_modelcard_chatml.md",
    "benign_tokenizer_config.json",
    "benign_multi_template_tutorial.md",
];

const ATTACK_SAMPLES: &[&str] = &[
    "attack_role_hijack.txt",
    "attack_instruction_override.txt",
];

#[test]
fn benign_chat_template_docs_allowed_under_relaxed() {
    for name in BENIGN_CHAT_TEMPLATE_DOCS {
        let v = screen_with_profile(&fixture(name), GuardProfile::Relaxed);
        assert_eq!(
            v.decision,
            InjectionDecision::Allow,
            "{name} should Allow under Relaxed; score={} codes={:?}",
            v.score,
            v.reason_codes,
        );
    }
}

#[test]
fn benign_chat_template_docs_would_block_under_strict() {
    // Demonstrates that the Relaxed profile is what saves these: the same
    // bytes Block on a Strict worker (e.g. shell-exec).
    for name in BENIGN_CHAT_TEMPLATE_DOCS {
        let v = screen_with_profile(&fixture(name), GuardProfile::Strict);
        assert_eq!(
            v.decision,
            InjectionDecision::Block,
            "{name} is expected to Block under Strict (lone chat-template token)",
        );
    }
}

#[test]
fn corroborated_attacks_block_under_both_profiles() {
    for name in ATTACK_SAMPLES {
        for profile in [GuardProfile::Strict, GuardProfile::Relaxed] {
            let v = screen_with_profile(&fixture(name), profile);
            assert_eq!(
                v.decision,
                InjectionDecision::Block,
                "{name} must Block under {profile:?}; score={} codes={:?}",
                v.score,
                v.reason_codes,
            );
        }
    }
}

#[test]
fn extract_then_relaxed_allows_web_fetch_shaped_tokenizer_config() {
    // The other tests scan the raw fixture bytes directly. Production runs
    // two steps: `extract_scannable_text` walks the worker's JSON-RPC value
    // and concatenates its string leaves, then `screen_with_profile` scans
    // that. This pins the FULL production pipeline on the Relaxed path
    // against a realistic web-fetch response shape, so a future change to
    // `extract_scannable_text` cannot silently reintroduce the #142
    // false-positive without failing a test.
    let body_text = fixture("benign_tokenizer_config.json");
    let response = serde_json::json!({
        "status": 200,
        "final_url": "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct/raw/main/tokenizer_config.json",
        "content_type": "application/json",
        "text": body_text,
        "truncated": false,
    });
    let (body, _truncated) = extract_scannable_text(&response, SCAN_BYTE_CAP);
    let v = screen_with_profile(&body, GuardProfile::Relaxed);
    assert_eq!(
        v.decision,
        InjectionDecision::Allow,
        "web-fetch-shaped tokenizer config must Allow under Relaxed; score={} codes={:?}",
        v.score,
        v.reason_codes,
    );
}
