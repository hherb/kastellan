//! Entity extraction: query-time NER for the recall graph lane.
//!
//! This module owns the `EntityExtractor` trait and its production
//! impl `GlinerRelexExtractor` (in `gliner_relex.rs`), plus the
//! `NoOpEntityExtractor` used when the gliner-relex worker isn't
//! configured.
//!
//! See `docs/superpowers/specs/2026-05-19-entity-extraction-v2-gliner-relex-design.md`
//! for the architecture rationale (single-pass joint NER+RE via the
//! gliner-relex worker; quarantine-on-upsert; Rust-side normalization
//! for case/whitespace/Unicode-insensitive dedup).

pub mod gliner_relex;

/// Canonical form for entity-name dedup. Done on the Rust side so the
/// normalization is the same on every host and PostgreSQL doesn't need
/// a locale-sensitive `lower()` call.
///
/// Pipeline:
///   1. Unicode NFC composition (`café` == `cafe\u{0301}`)
///   2. ASCII/Unicode lowercase (`Smith` == `SMITH` == `smith`)
///   3. Whitespace-run collapse to a single space + edge trim
///
/// Punctuation is NOT stripped — `Dr. Smith` and `Dr Smith` stay
/// distinct (stripping `.` would conflate `U.S.` and `US`).
pub fn normalize_entity_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.nfc()
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_basic_ascii() {
        assert_eq!(normalize_entity_name("Smith"), "smith");
        assert_eq!(normalize_entity_name("SMITH"), "smith");
        assert_eq!(normalize_entity_name("smith"), "smith");
    }

    #[test]
    fn normalize_trims_and_collapses_whitespace() {
        assert_eq!(normalize_entity_name("  Dr   Smith  "), "dr smith");
        assert_eq!(normalize_entity_name("Dr\tSmith"), "dr smith");
        assert_eq!(normalize_entity_name("Dr\n\nSmith"), "dr smith");
    }

    #[test]
    fn normalize_preserves_punctuation() {
        // Important: punctuation NOT stripped (U.S. vs US conflation risk).
        assert_eq!(normalize_entity_name("Dr. Smith"), "dr. smith");
        assert_ne!(
            normalize_entity_name("Dr. Smith"),
            normalize_entity_name("Dr Smith"),
            "punctuation must distinguish forms"
        );
    }

    #[test]
    fn normalize_applies_nfc_to_unicode() {
        // "café" composed (1 char é) vs decomposed (e + combining acute).
        let composed = "café";
        let decomposed = "cafe\u{0301}";
        assert_ne!(composed, decomposed, "raw inputs differ in NFC vs NFD");
        assert_eq!(
            normalize_entity_name(composed),
            normalize_entity_name(decomposed),
            "NFC normalization must collapse composition forms"
        );
    }

    #[test]
    fn normalize_empty_and_whitespace_only() {
        assert_eq!(normalize_entity_name(""), "");
        assert_eq!(normalize_entity_name("   "), "");
        assert_eq!(normalize_entity_name("\t\n"), "");
    }
}
