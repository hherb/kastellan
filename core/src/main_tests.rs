//! Unit tests for `main.rs`'s pure parse helpers (`parse_bootstrap_secrets_csv`
//! and the debug-only `parse_test_vault_seed`). Lifted out of `main.rs` to keep
//! the binary entrypoint nearer the 500-LOC cap; `super::` resolves to the
//! `kastellan` binary crate root.

use super::parse_bootstrap_secrets_csv;

#[test]
fn parse_empty_string_yields_empty_list() {
    assert!(parse_bootstrap_secrets_csv("").is_empty());
}

#[test]
fn parse_only_whitespace_yields_empty_list() {
    assert!(parse_bootstrap_secrets_csv("   ").is_empty());
    assert!(parse_bootstrap_secrets_csv(" \t \n ").is_empty());
}

#[test]
fn parse_single_name_works() {
    let names = parse_bootstrap_secrets_csv("openai-api-key");
    assert_eq!(names, vec!["openai-api-key"]);
}

#[test]
fn parse_handles_trailing_comma() {
    let names = parse_bootstrap_secrets_csv("a,b,");
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn parse_handles_leading_comma_and_whitespace() {
    let names = parse_bootstrap_secrets_csv(", , a , b ,, c , ");
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn parse_preserves_internal_dashes_and_dots() {
    let names = parse_bootstrap_secrets_csv("openai.api.key, github-token");
    assert_eq!(names, vec!["openai.api.key", "github-token"]);
}

#[cfg(debug_assertions)]
#[test]
fn parse_test_vault_seed_splits_on_first_equals() {
    use super::parse_test_vault_seed;
    assert_eq!(
        parse_test_vault_seed("deadbeef=SCRUBME-value"),
        Some(("deadbeef", "SCRUBME-value")),
    );
}

#[cfg(debug_assertions)]
#[test]
fn parse_test_vault_seed_keeps_equals_in_plaintext() {
    // A real secret may contain `=` (e.g. base64 padding) — only the FIRST
    // `=` separates the ref tail from the plaintext; the rest is verbatim.
    use super::parse_test_vault_seed;
    assert_eq!(
        parse_test_vault_seed("aabbccdd=a=b==c"),
        Some(("aabbccdd", "a=b==c")),
    );
}

#[cfg(debug_assertions)]
#[test]
fn parse_test_vault_seed_none_without_separator() {
    use super::parse_test_vault_seed;
    assert_eq!(parse_test_vault_seed("noseparator"), None);
    assert_eq!(parse_test_vault_seed(""), None);
}

#[cfg(debug_assertions)]
#[test]
fn parse_test_vault_seed_does_not_trim() {
    // The plaintext is a secret — trimming could corrupt it; the value is
    // taken byte-for-byte after the first `=`. (The ref tail's own format
    // is validated downstream by `Vault::seed_known_ref_for_test`.)
    use super::parse_test_vault_seed;
    assert_eq!(
        parse_test_vault_seed("deadbeef= padded "),
        Some(("deadbeef", " padded ")),
    );
}
