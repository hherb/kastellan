//! Helpers shared across more than one `hhagent-cli` subcommand.
//!
//! Kept deliberately small. If a helper is used by exactly one subcommand
//! it belongs in that subcommand's module, not here.

use std::process::ExitCode;

/// Build a [`hhagent_db::conn::ConnectSpec`] from `$HHAGENT_DATA_DIR`
/// (if set) or the XDG default. Fails with a human-readable error string
/// when `$HOME` is unset (needed by `ConnectSpec::default_for`).
pub(crate) fn resolve_connect_spec() -> Result<hhagent_db::conn::ConnectSpec, String> {
    let data_dir = match std::env::var_os("HHAGENT_DATA_DIR") {
        Some(p) => std::path::PathBuf::from(p),
        None => hhagent_db::default_data_dir()
            .ok_or_else(|| "$HOME unset; cannot resolve cluster data dir".to_string())?,
    };
    hhagent_db::conn::ConnectSpec::default_for(&data_dir)
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
) -> Result<hhagent_core::cassandra::DataClass, String> {
    use hhagent_core::cassandra::DataClass;
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
/// Doc-pin: `prefix` flows verbatim into the failure diagnostic so
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

#[cfg(test)]
mod parse_classification_floor_tests {
    use super::parse_classification_floor;
    use hhagent_core::cassandra::DataClass;

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
