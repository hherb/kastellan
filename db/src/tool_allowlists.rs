//! Per-tool argv allowlist storage and validators.
//!
//! The `tool_allowlists` table (migration `0009_tool_allowlists.sql`) is
//! the source-of-truth for which absolute `argv[0]` paths each registered
//! tool worker may exec. Replaces the previous
//! `KASTELLAN_SHELL_EXEC_ALLOWLIST` env-var-driven shape.
//!
//! Validators here are the user-facing gate — they produce typed errors
//! that surface as readable CLI messages. The SQL-layer CHECK constraints
//! on the table (migration `0009_tool_allowlists.sql`) provide
//! defense-in-depth for callers that bypass these validators: the table
//! rejects empty `argv0`, non-absolute paths, and `..` *segments*. NUL
//! bytes are rejected at the Postgres protocol layer (TEXT columns
//! refuse the 0x00 byte). The SQL layer does NOT cover the full charset
//! validation on `tool` names — keep `validate_tool_name` as the single
//! authoritative source there.

use std::net::Ipv6Addr;

use sqlx::PgPool;
use time::OffsetDateTime;

/// Maximum length (UTF-8 bytes) for a tool name. 64 bytes is generous
/// for the foreseeable shape of worker names (`shell-exec`, `web-fetch`,
/// `python-exec`, …) and bounds the size of audit-row payloads.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Which shape an entry in `tool_allowlists` takes for a given tool. A tool is
/// entirely one kind or the other — it is a function of the tool, never mixed:
/// `shell-exec` stores argv0 exec paths; the web workers store domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Absolute `argv[0]` exec path — validated by [`validate_argv0`].
    Argv0,
    /// Host / domain allowlist entry — validated by [`validate_domain`].
    Domain,
}

/// Errors that can come out of this module.
#[derive(thiserror::Error, Debug)]
pub enum ToolAllowlistError {
    #[error("tool name empty or invalid; expected ASCII alphanumeric plus '-' or '_', max {MAX_TOOL_NAME_LEN} bytes")]
    InvalidToolName,

    #[error("argv0 must be a non-empty absolute path (starting with '/')")]
    InvalidArgv0,

    #[error("argv0 contains a NUL byte")]
    Argv0HasNul,

    #[error("argv0 contains a '..' segment; reject path-confusion bypasses by sending exact canonical paths")]
    Argv0HasDotDot,

    #[error("allowlist entry is not a valid host/domain; expected a bare domain \
             (example.org), a wildcard (.example.org), a bare IPv4, or a bracketed \
             IPv6 literal ([::1]) — no scheme, port, path, '@', or whitespace")]
    InvalidDomain,

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// One row in `tool_allowlists`. Returned by [`list_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowlistEntry {
    pub tool: String,
    pub argv0: String,
    pub created_at: OffsetDateTime,
    pub created_by: String,
}

/// Validate a tool name. Accepts ASCII alphanumeric plus `-` and `_`,
/// non-empty, ≤ [`MAX_TOOL_NAME_LEN`] bytes. The charset matches the
/// conservative shape used by [`crate::secrets::validate_name`] and
/// the supervisor's service-name validators — names flow through to
/// log lines and audit payloads without escaping.
pub fn validate_tool_name(name: &str) -> Result<(), ToolAllowlistError> {
    if name.is_empty() || name.len() > MAX_TOOL_NAME_LEN {
        return Err(ToolAllowlistError::InvalidToolName);
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ToolAllowlistError::InvalidToolName);
    }
    Ok(())
}

/// Validate an argv0. Must be a non-empty absolute path, contain no
/// NUL byte, and contain no `..` path segment.
///
/// The `..` rejection is security-motivated: a path-confusion bypass
/// like `/usr/bin/../bin/echo` resolves to `/bin/echo` at exec time
/// but would not string-match an allowlist that contains only
/// `/bin/echo`. Rejecting `..` segments closes that gap.
///
/// `.` segments and trailing slashes are NOT rejected — they don't
/// enable an allowlist bypass (string match still fails) and the
/// kernel resolves them identically at exec time. The filesystem is
/// not consulted; canonicalisation is the operator's responsibility.
pub fn validate_argv0(argv0: &str) -> Result<(), ToolAllowlistError> {
    if argv0.is_empty() || !argv0.starts_with('/') {
        return Err(ToolAllowlistError::InvalidArgv0);
    }
    if argv0.contains('\0') {
        return Err(ToolAllowlistError::Argv0HasNul);
    }
    // A literal ".." anywhere as a path *segment* (between '/'s or at
    // an end). Reject `/usr/bin/../bin/echo` but allow `/usr/bin/foo..bar`
    // (no separator on either side of the dotdot).
    for seg in argv0.split('/') {
        if seg == ".." {
            return Err(ToolAllowlistError::Argv0HasDotDot);
        }
    }
    Ok(())
}

/// Validate a domain-kind allowlist entry: a bare domain (`example.org`), a
/// wildcard (`.example.org`), a bare IPv4 (`203.0.113.5`), or a **bracketed**
/// IPv6 literal (`[::1]`). Rejects anything carrying a scheme, embedded port,
/// path, userinfo (`@`), or whitespace — so `localhost:8888` (the #459
/// residual-#3 footgun) is rejected here at the source, before it can become
/// the dead net entry `localhost:8888:443`.
///
/// Brackets are REQUIRED for IPv6 so the downstream `host:443` mapping
/// (`allowlist_to_net_entries`) yields a valid `[::1]:443` and the bracket-aware
/// `host_of_entry` strips it back cleanly. A bare `::1` is rejected — it would
/// map to the ambiguous `::1:443`.
///
/// Hand-rolled (no `url`/idna dependency — IPv6 via `std::net::Ipv6Addr`), LDH
/// label rules, matching the style of [`validate_argv0`]. The SQL CHECK in
/// migration `0021` is a coarser shape backstop; this is the authoritative gate.
pub fn validate_domain(entry: &str) -> Result<(), ToolAllowlistError> {
    if entry.is_empty() {
        return Err(ToolAllowlistError::InvalidDomain);
    }
    // No control chars, whitespace, or NUL anywhere (bytes 0x00..=0x20 and DEL).
    if entry.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(ToolAllowlistError::InvalidDomain);
    }
    // Bracketed IPv6 literal: the inner text must parse as an Ipv6Addr.
    if let Some(inner) = entry.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return match inner.parse::<Ipv6Addr>() {
            Ok(_) => Ok(()),
            Err(_) => Err(ToolAllowlistError::InvalidDomain),
        };
    }
    // Domain / IPv4 branch. Strip one optional wildcard leading dot and one
    // optional FQDN trailing dot, then validate the remaining LDH labels.
    let host = entry.strip_prefix('.').unwrap_or(entry);
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 {
        return Err(ToolAllowlistError::InvalidDomain);
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(ToolAllowlistError::InvalidDomain);
        }
        if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(ToolAllowlistError::InvalidDomain);
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(ToolAllowlistError::InvalidDomain);
        }
    }
    Ok(())
}

/// Dispatch to the right validator for a tool's [`EntryKind`]. `add`/`remove`
/// call this so the DB layer applies argv0 rules to argv0 tools and domain
/// rules to domain tools.
pub fn validate_entry(kind: EntryKind, entry: &str) -> Result<(), ToolAllowlistError> {
    match kind {
        EntryKind::Argv0 => validate_argv0(entry),
        EntryKind::Domain => validate_domain(entry),
    }
}

// --- I/O layer ---------------------------------------------------------

/// Add one allowlist entry. Idempotent — returns `Ok(true)` if a row
/// was INSERTed, `Ok(false)` if the entry was already present.
pub async fn add(
    pool: &PgPool,
    tool: &str,
    kind: EntryKind,
    argv0: &str,
    created_by: &str,
) -> Result<bool, ToolAllowlistError> {
    validate_tool_name(tool)?;
    validate_entry(kind, argv0)?;
    let rows = sqlx::query(
        "INSERT INTO tool_allowlists (tool, argv0, created_by)
         VALUES ($1, $2, $3)
         ON CONFLICT (tool, argv0) DO NOTHING",
    )
    .bind(tool)
    .bind(argv0)
    .bind(created_by)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() == 1)
}

/// Remove one allowlist entry. Idempotent — returns `Ok(true)` if a
/// row was deleted, `Ok(false)` if nothing matched.
pub async fn remove(
    pool: &PgPool,
    tool: &str,
    kind: EntryKind,
    argv0: &str,
) -> Result<bool, ToolAllowlistError> {
    validate_tool_name(tool)?;
    validate_entry(kind, argv0)?;
    let rows = sqlx::query(
        "DELETE FROM tool_allowlists WHERE tool = $1 AND argv0 = $2",
    )
    .bind(tool)
    .bind(argv0)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() == 1)
}

/// List the argv0 entries for one tool, ordered by argv0 ascending.
///
/// Returns only the argv0 string — the cheap shape used by
/// `build_tool_registry` at daemon bring-up. Callers that need the full
/// row (created_at / created_by) should use [`list_for_tool_full`].
pub async fn list_for_tool(
    pool: &PgPool,
    tool: &str,
) -> Result<Vec<String>, ToolAllowlistError> {
    validate_tool_name(tool)?;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT argv0 FROM tool_allowlists WHERE tool = $1 ORDER BY argv0 ASC",
    )
    .bind(tool)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

/// Like [`list_for_tool`] but returns the full [`AllowlistEntry`] shape
/// (`tool`, `argv0`, `created_at`, `created_by`). Used by the
/// `kastellan-cli tools allowlist list --tool <name>` path so the WHERE
/// predicate runs on the PK-indexed server side instead of the CLI
/// filtering client-side over [`list_all`].
pub async fn list_for_tool_full(
    pool: &PgPool,
    tool: &str,
) -> Result<Vec<AllowlistEntry>, ToolAllowlistError> {
    validate_tool_name(tool)?;
    let rows: Vec<(String, String, OffsetDateTime, String)> = sqlx::query_as(
        "SELECT tool, argv0, created_at, created_by
         FROM tool_allowlists
         WHERE tool = $1
         ORDER BY argv0 ASC",
    )
    .bind(tool)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(tool, argv0, created_at, created_by)| AllowlistEntry {
            tool,
            argv0,
            created_at,
            created_by,
        })
        .collect())
}

/// List every entry across every tool, ordered by `(tool, argv0)`.
pub async fn list_all(pool: &PgPool) -> Result<Vec<AllowlistEntry>, ToolAllowlistError> {
    let rows: Vec<(String, String, OffsetDateTime, String)> = sqlx::query_as(
        "SELECT tool, argv0, created_at, created_by
         FROM tool_allowlists
         ORDER BY tool ASC, argv0 ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(tool, argv0, created_at, created_by)| AllowlistEntry {
            tool,
            argv0,
            created_at,
            created_by,
        })
        .collect())
}

// --- Tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_tool_name_accepts_canonical_shapes() {
        validate_tool_name("shell-exec").unwrap();
        validate_tool_name("shell_exec_v2").unwrap();
        validate_tool_name("web-fetch").unwrap();
        validate_tool_name("a").unwrap();
        validate_tool_name("ABC123").unwrap();
    }

    #[test]
    fn validate_tool_name_rejects_empty_and_oversize_and_invalid_chars() {
        assert!(matches!(
            validate_tool_name(""),
            Err(ToolAllowlistError::InvalidToolName)
        ));
        let too_long: String = "a".repeat(MAX_TOOL_NAME_LEN + 1);
        assert!(matches!(
            validate_tool_name(&too_long),
            Err(ToolAllowlistError::InvalidToolName)
        ));
        assert!(matches!(
            validate_tool_name("shell exec"),
            Err(ToolAllowlistError::InvalidToolName)
        ));
        assert!(matches!(
            validate_tool_name("shell/exec"),
            Err(ToolAllowlistError::InvalidToolName)
        ));
        assert!(matches!(
            validate_tool_name("shell.exec"),
            Err(ToolAllowlistError::InvalidToolName)
        ));
    }

    #[test]
    fn validate_argv0_accepts_typical_absolute_paths() {
        validate_argv0("/usr/bin/echo").unwrap();
        validate_argv0("/bin/sh").unwrap();
        validate_argv0("/opt/kastellan/bin/web-fetch-worker").unwrap();
        validate_argv0("/").unwrap(); // odd but technically absolute
    }

    #[test]
    fn validate_argv0_rejects_relative_paths() {
        assert!(matches!(
            validate_argv0(""),
            Err(ToolAllowlistError::InvalidArgv0)
        ));
        assert!(matches!(
            validate_argv0("echo"),
            Err(ToolAllowlistError::InvalidArgv0)
        ));
        assert!(matches!(
            validate_argv0("./echo"),
            Err(ToolAllowlistError::InvalidArgv0)
        ));
        assert!(matches!(
            validate_argv0("usr/bin/echo"),
            Err(ToolAllowlistError::InvalidArgv0)
        ));
    }

    #[test]
    fn validate_argv0_rejects_nul_byte() {
        assert!(matches!(
            validate_argv0("/usr/bin/echo\0"),
            Err(ToolAllowlistError::Argv0HasNul)
        ));
        assert!(matches!(
            validate_argv0("/usr/\0/echo"),
            Err(ToolAllowlistError::Argv0HasNul)
        ));
    }

    #[test]
    fn validate_argv0_rejects_dotdot_segment_but_accepts_dotdot_within_segment() {
        assert!(matches!(
            validate_argv0("/usr/bin/../bin/echo"),
            Err(ToolAllowlistError::Argv0HasDotDot)
        ));
        assert!(matches!(
            validate_argv0("/.."),
            Err(ToolAllowlistError::Argv0HasDotDot)
        ));
        // `..` *inside* a segment (no slash on either side) is fine —
        // it's a legal filename character.
        validate_argv0("/usr/bin/foo..bar").unwrap();
    }

    #[test]
    fn validate_domain_accepts_domains_wildcards_ipv4_and_bracketed_ipv6() {
        for ok in [
            "example.org",
            "api.example.org",
            ".example.org",   // wildcard
            "example.org.",   // FQDN trailing dot
            "a-b.example.org", // hyphen inside a label
            "203.0.113.5",    // bare IPv4
            "[::1]",          // IPv6 loopback (bracketed)
            "[2606:4700:4700::1111]",
            "[fd12:3456::1]", // ULA
        ] {
            validate_domain(ok).unwrap_or_else(|e| panic!("{ok} should be valid: {e}"));
        }
    }

    #[test]
    fn validate_domain_rejects_ports_schemes_paths_and_malformed() {
        for bad in [
            "",
            "localhost:8888",     // embedded port — the #459 residual-#3 footgun
            "http://example.org", // scheme
            "example.org/search", // path
            "user@example.org",   // userinfo
            "a..b",               // empty label
            "-a.example.org",     // leading hyphen
            "a-.example.org",     // trailing hyphen
            "::1",                // unbracketed IPv6
            "[not-ipv6]",         // brackets but not an IPv6 addr
            "exa mple.org",       // whitespace
            "foo\tbar",           // control char
        ] {
            assert!(
                matches!(validate_domain(bad), Err(ToolAllowlistError::InvalidDomain)),
                "{bad:?} should be InvalidDomain"
            );
        }
    }

    #[test]
    fn validate_entry_dispatches_by_kind() {
        validate_entry(EntryKind::Argv0, "/bin/echo").unwrap();
        assert!(matches!(
            validate_entry(EntryKind::Argv0, "example.org"),
            Err(ToolAllowlistError::InvalidArgv0)
        ));
        validate_entry(EntryKind::Domain, "example.org").unwrap();
        assert!(matches!(
            validate_entry(EntryKind::Domain, "localhost:8888"),
            Err(ToolAllowlistError::InvalidDomain)
        ));
    }
}
