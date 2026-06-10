//! Helpers shared across more than one `kastellan-cli` subcommand.
//!
//! Kept deliberately small. If a helper is used by exactly one subcommand
//! it belongs in that subcommand's module, not here.

use std::fmt::Write;
use std::process::ExitCode;

/// Build a [`kastellan_db::conn::ConnectSpec`] from `$KASTELLAN_DATA_DIR`
/// (if set) or the XDG default. Fails with a human-readable error string
/// when `$HOME` is unset (needed by `ConnectSpec::default_for`).
pub(crate) fn resolve_connect_spec() -> Result<kastellan_db::conn::ConnectSpec, String> {
    let data_dir = match std::env::var_os("KASTELLAN_DATA_DIR") {
        Some(p) => std::path::PathBuf::from(p),
        None => kastellan_db::default_data_dir()
            .ok_or_else(|| "$HOME unset; cannot resolve cluster data dir".to_string())?,
    };
    kastellan_db::conn::ConnectSpec::default_for(&data_dir)
        .map_err(|e| format!("resolving Postgres connection: {e}"))
}

/// Parse a `--classification-floor` CLI value into a `DataClass`.
///
/// Case-insensitive; accepts canonical `PascalCase`, lowercase,
/// `UPPERCASE`, hyphen-separated, snake_case, and space-separated
/// forms (`clinical_confidential`, `clinical-confidential`,
/// `clinical confidential` all map to
/// `DataClass::ClinicalConfidential`).
///
/// Returns `Err(message)` on unknown values or empty input; the
/// message lists every valid value so the operator can correct in
/// one step.
pub(crate) fn parse_classification_floor(
    raw: &str,
) -> Result<kastellan_core::cassandra::DataClass, String> {
    use kastellan_core::cassandra::DataClass;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(
            "--classification-floor: empty value; valid values: Public, Personal, ClinicalConfidential, Secret"
                .to_string(),
        );
    }
    // Normalise: drop all `_`, `-`, and ASCII whitespace; lowercase.
    let normalised: String = trimmed
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect();
    match normalised.as_str() {
        "public" => Ok(DataClass::Public),
        "personal" => Ok(DataClass::Personal),
        "clinicalconfidential" => Ok(DataClass::ClinicalConfidential),
        "secret" => Ok(DataClass::Secret),
        _ => Err(format!(
            "--classification-floor: unknown value {raw:?}; valid values: Public, Personal, ClinicalConfidential, Secret"
        )),
    }
}

/// Build a multi-thread tokio runtime, returning an `Err(ExitCode)` on
/// failure with a `<prefix>: failed to build tokio runtime: {e}` line
/// already printed to stderr.
///
/// Centralises the boilerplate every async-using subcommand repeats.
pub(crate) fn multi_thread_runtime(prefix: &str) -> Result<tokio::runtime::Runtime, ExitCode> {
    match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => Ok(rt),
        Err(e) => {
            eprintln!("{prefix}: failed to build tokio runtime: {e}");
            Err(ExitCode::from(1))
        }
    }
}

/// Build a multi-thread tokio runtime and run `fut` to completion,
/// returning the future's `ExitCode` (or `ExitCode::from(1)` if the
/// runtime itself failed to build, with the diagnostic already on
/// stderr).
///
/// Lets each dispatcher write the per-action arms as one line
/// (`"act" => with_runtime("X", act_fn(&args[1..]))`) **inside the
/// known-action match**, so a typo at the dispatch site (Issue #97)
/// no longer pays the cost of spawning worker threads it never uses.
///
/// Note: `prefix` flows verbatim into the failure diagnostic so
/// operators can tell which dispatcher hit the (rare) build error.
pub(crate) fn with_runtime<F>(prefix: &str, fut: F) -> ExitCode
where
    F: std::future::Future<Output = ExitCode>,
{
    let rt = match multi_thread_runtime(prefix) {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    rt.block_on(fut)
}

/// One row of an operator-facing `(kind, created_at, description)`
/// table. Used by [`format_kinds_table`] to render the shared shape
/// across `entities kinds list` and `relations kinds list` without
/// duplicating the column-alignment logic.
///
/// Borrowing references rather than owning strings keeps callers from
/// having to clone the underlying `EntityKindEntry` /
/// `RelationKindEntry` rows.
pub(crate) struct KindRow<'a> {
    pub kind: &'a str,
    pub created_at_display: &'a str,
    pub description: Option<&'a str>,
}

/// Render an aligned `(KIND, CREATED_AT, DESCRIPTION)` table with
/// dynamically-sized columns. The returned string always ends with a
/// trailing newline so callers can `print!` it directly.
///
/// Column widths are `max(header_width, longest_value_width)` for the
/// first two columns; `DESCRIPTION` is the last column and not
/// padded. Replaces the original `{:<24}` fixed-width formatter
/// flagged as a truncation footgun by
/// [#111](https://github.com/hherb/kastellan/issues/111) item 2 — with
/// `MAX_{ENTITY,RELATION}_KIND_LEN = 64`, a 64-byte kind under the
/// old code crowded the `CREATED_AT` column out of alignment. The
/// dynamic widths absorb the longest row cleanly.
///
/// Empty `rows` still emits the header line — callers that want
/// no-output-on-empty must check `rows.is_empty()` before calling.
pub(crate) fn format_kinds_table(rows: &[KindRow<'_>]) -> String {
    const KIND_H: &str = "KIND";
    const CREATED_AT_H: &str = "CREATED_AT";
    const DESCRIPTION_H: &str = "DESCRIPTION";

    let kind_w = rows
        .iter()
        .map(|r| r.kind.len())
        .max()
        .unwrap_or(0)
        .max(KIND_H.len());
    let created_at_w = rows
        .iter()
        .map(|r| r.created_at_display.len())
        .max()
        .unwrap_or(0)
        .max(CREATED_AT_H.len());

    let mut out = String::new();
    // Header.
    let _ = writeln!(
        &mut out,
        "{:<kind_w$}  {:<created_at_w$}  {}",
        KIND_H, CREATED_AT_H, DESCRIPTION_H,
        kind_w = kind_w,
        created_at_w = created_at_w,
    );
    // Data rows.
    for r in rows {
        let _ = writeln!(
            &mut out,
            "{:<kind_w$}  {:<created_at_w$}  {}",
            r.kind,
            r.created_at_display,
            r.description.unwrap_or(""),
            kind_w = kind_w,
            created_at_w = created_at_w,
        );
    }
    out
}

#[cfg(test)]
mod format_kinds_table_tests {
    use super::{format_kinds_table, KindRow};

    #[test]
    fn empty_input_emits_header_line_only() {
        let out = format_kinds_table(&[]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1, "expected header line only; got: {out:?}");
        assert!(lines[0].starts_with("KIND"), "header: {:?}", lines[0]);
        assert!(lines[0].contains("CREATED_AT"), "header: {:?}", lines[0]);
        assert!(lines[0].contains("DESCRIPTION"), "header: {:?}", lines[0]);
    }

    #[test]
    fn short_kinds_align_at_header_width() {
        // All kinds are <= "KIND" header width — column width should
        // collapse to the header width (no over-padding).
        let rows = vec![
            KindRow {
                kind: "a",
                created_at_display: "2026-05-23",
                description: Some("first"),
            },
            KindRow {
                kind: "ab",
                created_at_display: "2026-05-23",
                description: Some("second"),
            },
        ];
        let out = format_kinds_table(&rows);
        // Header KIND column = max(len("KIND")=4, len("ab")=2) = 4.
        // Data rows use the same width; "a" + 3 spaces, "ab" + 2 spaces.
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 rows");
        assert!(lines[1].starts_with("a   "), "expected 'a' + 3 trailing spaces; got: {:?}", lines[1]);
        assert!(lines[2].starts_with("ab  "), "expected 'ab' + 2 trailing spaces; got: {:?}", lines[2]);
    }

    #[test]
    fn long_kind_expands_column_without_truncation() {
        // A 64-byte kind (the MAX_{ENTITY,RELATION}_KIND_LEN cap) must
        // print in full, and a shorter kind in the same batch must be
        // padded out to match the long row's width. This is the
        // headline #111-item-2 regression pin.
        let long = "a".repeat(64);
        let rows = vec![
            KindRow {
                kind: "short",
                created_at_display: "ts1",
                description: None,
            },
            KindRow {
                kind: &long,
                created_at_display: "ts2",
                description: Some("desc"),
            },
        ];
        let out = format_kinds_table(&rows);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        // The long kind must appear in full — no truncation.
        assert!(
            lines[2].contains(&long),
            "long kind must be present verbatim; got: {:?}",
            lines[2]
        );
        // The short row must be padded out to the long row's kind
        // column width — header column width = max("KIND"=4,
        // "short"=5, 64) = 64. Both data rows therefore start with a
        // 64-byte left-padded kind column followed by exactly two
        // spaces (the column separator).
        let short_kind_col_with_sep = format!("{:<64}  ", "short");
        assert!(
            lines[1].starts_with(&short_kind_col_with_sep),
            "short row must be padded to 64 chars; got: {:?}",
            lines[1]
        );
    }

    #[test]
    fn header_dictates_minimum_width_when_data_is_shorter() {
        // No data row reaches the "CREATED_AT" header's 10 chars, so
        // the column width must collapse to the header width — not the
        // data width.
        let rows = vec![KindRow {
            kind: "k",
            created_at_display: "t",
            description: None,
        }];
        let out = format_kinds_table(&rows);
        let lines: Vec<&str> = out.lines().collect();
        // Data row's CREATED_AT cell must be padded to at least
        // "CREATED_AT".len() = 10.
        let data_row = lines[1];
        // After the kind column (4 chars: "k" + 3 padding for "KIND"
        // header width) and 2 spaces, the next 10 chars are the
        // padded "t" cell. Then 2 more spaces, then the (empty)
        // description.
        assert_eq!(
            data_row, "k     t           ",
            "expected dynamic-width padding using header widths as floor; got: {:?}",
            data_row,
        );
    }

    #[test]
    fn missing_description_renders_as_empty_column() {
        let rows = vec![KindRow {
            kind: "kind1",
            created_at_display: "stamp",
            description: None,
        }];
        let out = format_kinds_table(&rows);
        let line = out.lines().nth(1).expect("data row");
        // The DESCRIPTION column is the last column — `None` renders
        // as the empty string, so the line ends right after the
        // last column-separator with no description bytes.
        assert!(line.ends_with("  "), "expected trailing separator; got: {:?}", line);
    }
}

#[cfg(test)]
mod parse_classification_floor_tests {
    use super::parse_classification_floor;
    use kastellan_core::cassandra::DataClass;

    #[test]
    fn accepts_canonical_pascal_case() {
        assert_eq!(parse_classification_floor("Public").unwrap(), DataClass::Public);
        assert_eq!(parse_classification_floor("Personal").unwrap(), DataClass::Personal);
        assert_eq!(parse_classification_floor("ClinicalConfidential").unwrap(), DataClass::ClinicalConfidential);
        assert_eq!(parse_classification_floor("Secret").unwrap(), DataClass::Secret);
    }

    #[test]
    fn accepts_lowercase() {
        assert_eq!(parse_classification_floor("public").unwrap(), DataClass::Public);
        assert_eq!(parse_classification_floor("clinical_confidential").unwrap(), DataClass::ClinicalConfidential);
    }

    #[test]
    fn accepts_uppercase() {
        assert_eq!(parse_classification_floor("PUBLIC").unwrap(), DataClass::Public);
        assert_eq!(parse_classification_floor("CLINICAL_CONFIDENTIAL").unwrap(), DataClass::ClinicalConfidential);
    }

    #[test]
    fn accepts_mixed_case_and_separator_variants() {
        // Hyphen-separated common in CLIs; spaces unusual but cheap to allow.
        assert_eq!(parse_classification_floor("clinical-confidential").unwrap(), DataClass::ClinicalConfidential);
        assert_eq!(parse_classification_floor("Clinical Confidential").unwrap(), DataClass::ClinicalConfidential);
    }

    #[test]
    fn rejects_unknown_value_with_helpful_message() {
        let err = parse_classification_floor("topsecret").unwrap_err();
        assert!(err.contains("topsecret"), "expected input echoed; got: {err}");
        assert!(err.contains("valid values"), "expected 'valid values' phrase; got: {err}");
        assert!(err.contains("Public"), "expected list of valid values; got: {err}");
        assert!(err.contains("ClinicalConfidential"), "expected list of valid values; got: {err}");
    }

    #[test]
    fn rejects_empty_string() {
        let err = parse_classification_floor("").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(parse_classification_floor("  Public  ").unwrap(), DataClass::Public);
    }
}
