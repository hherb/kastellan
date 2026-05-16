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
//! - **Matching style:** [`contains_word`] (whole-word, ASCII
//!   alphanumeric byte boundaries) for single-word patterns that have
//!   substring collision risk (e.g. `password` would otherwise match
//!   `passworded`). Multi-word phrases (e.g. `ct scan`) use bare
//!   `contains` since they have no whole-word collision shape.
//! - **Signal tags:** snake_case identifiers chosen to be grep-friendly
//!   in audit logs. Aliases (`ekg` → `ecg`, `x-ray` → `xray`) collapse
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

/// Run the tiered keyword scan against `instruction` and return the
/// inferred floor + matched signal tags. Pure function; no I/O.
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
/// duplicates removed (an alias like `ekg` → `ecg` would otherwise
/// produce two `ecg` entries for an `ecg ekg` input).
fn match_catalogue(instruction: &str, catalogue: &[CatalogueEntry]) -> Vec<&'static str> {
    // Lowercase once for the `contains` path; `contains_word` does its
    // own lowering. This avoids re-allocating per-entry.
    let lower = instruction.to_ascii_lowercase();
    let mut out: Vec<&'static str> = Vec::new();
    for (phrase, tag, use_word) in catalogue {
        let hit = if *use_word {
            contains_word(instruction, phrase)
        } else {
            lower.contains(&phrase.to_ascii_lowercase())
        };
        if hit && !out.contains(tag) {
            out.push(tag);
        }
    }
    out
}

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
    if needle.is_empty() {
        return false;
    }
    let h = haystack.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    let n_bytes = n.as_bytes();
    let h_bytes = h.as_bytes();
    for (i, _) in h.match_indices(&n) {
        let before_ok = i == 0 || !h_bytes[i - 1].is_ascii_alphanumeric();
        let after_idx = i + n_bytes.len();
        let after_ok =
            after_idx == h_bytes.len() || !h_bytes[after_idx].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Default / no-signal cases =====

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
        assert!(
            r.signals.contains(&"password"),
            "expected 'password' signal; got {:?}",
            r.signals,
        );
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
    fn certificate_signal_matches_secret() {
        let r = infer_floor("Renew the TLS certificate on the gateway.");
        assert_eq!(r.class, DataClass::Secret);
        assert!(r.signals.contains(&"certificate"));
    }

    #[test]
    fn passworded_passive_form_does_not_match_secret() {
        // contains_word should reject the substring inside other words.
        let r = infer_floor("This document is passworded.");
        assert_eq!(
            r.class,
            DataClass::Public,
            "'passworded' is a different word; substring match would be a false positive",
        );
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
        assert!(
            r.signals.contains(&"ecg"),
            "ekg should produce the canonical 'ecg' tag; got {:?}",
            r.signals,
        );
    }

    #[test]
    fn xray_alias_collapses_to_xray_tag() {
        let r = infer_floor("Order an x-ray.");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        assert!(r.signals.contains(&"xray"));
    }

    #[test]
    fn icd_10_signal_matches_clinical() {
        let r = infer_floor("Look up ICD-10 code R52.");
        assert_eq!(r.class, DataClass::ClinicalConfidential);
        assert!(r.signals.contains(&"icd_10"));
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
        assert!(
            !r.signals.contains(&"patient"),
            "lower-class signals must not appear once a winning class is found; got {:?}",
            r.signals,
        );
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

#[cfg(test)]
mod contains_word_tests {
    use super::contains_word;

    #[test]
    fn whole_word_match() {
        assert!(contains_word("rotate the password please", "password"));
    }

    #[test]
    fn substring_no_match() {
        assert!(!contains_word("this is passworded", "password"));
    }

    #[test]
    fn case_insensitive() {
        assert!(contains_word("ROTATE THE PASSWORD", "password"));
    }

    #[test]
    fn empty_needle_no_match() {
        assert!(!contains_word("anything", ""));
    }

    #[test]
    fn punctuation_boundary() {
        assert!(contains_word("password!", "password"));
    }
}
