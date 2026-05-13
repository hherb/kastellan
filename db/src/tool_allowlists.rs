//! Per-tool argv allowlist storage and validators.
//!
//! The `tool_allowlists` table (migration `0009_tool_allowlists.sql`) is
//! the source-of-truth for which absolute `argv[0]` paths each registered
//! tool worker may exec. Replaces the previous
//! `HHAGENT_SHELL_EXEC_ALLOWLIST` env-var-driven shape.
//!
//! Validators here are the user-facing gate — they produce typed errors
//! that surface as readable CLI messages. The SQL-layer CHECK constraints
//! on the table are the last-line-of-defence pin (a future caller that
//! bypassed these validators would still get rejected by Postgres).

use sqlx::PgPool;
use time::OffsetDateTime;

/// Maximum length (UTF-8 bytes) for a tool name. 64 bytes is generous
/// for the foreseeable shape of worker names (`shell-exec`, `web-fetch`,
/// `python-exec`, …) and bounds the size of audit-row payloads.
pub const MAX_TOOL_NAME_LEN: usize = 64;

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

// --- I/O layer (filled in by Task 3) ----------------------------------

/// Add one allowlist entry. Idempotent — returns `Ok(true)` if a row
/// was INSERTed, `Ok(false)` if the entry was already present.
pub async fn add(
    pool: &PgPool,
    tool: &str,
    argv0: &str,
    created_by: &str,
) -> Result<bool, ToolAllowlistError> {
    validate_tool_name(tool)?;
    validate_argv0(argv0)?;
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
    argv0: &str,
) -> Result<bool, ToolAllowlistError> {
    validate_tool_name(tool)?;
    validate_argv0(argv0)?;
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
        validate_argv0("/opt/hhagent/bin/web-fetch-worker").unwrap();
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
}
