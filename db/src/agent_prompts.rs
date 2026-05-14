//! Agent-prompt traceability ledger.
//!
//! Source of truth for prompt content is git (`prompts/*.md`).
//! Every daemon startup reads each prompt file, hashes it, and
//! upserts a row keyed by sha256. `plan.formulate` audit-log rows
//! carry the (name, sha256) pair so CASSANDRA's reviewer (when real
//! impls land) can correlate behavioural drift to specific prompt
//! versions via this table.
//!
//! Append-only by GRANT (migration 0006): runtime role has
//! SELECT, INSERT only. Old rows persist forever.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::Row;

use crate::DbError;

/// Compute the canonical SHA-256 of prompt content. Hex-encoded
/// lowercase, 64 chars — fits the `agent_prompts.sha256 CHAR(64)`
/// column.
pub fn hash_content(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(s, "{:02x}", b).expect("write to String cannot fail");
    }
    s
}

/// Upsert a prompt row. Idempotent on the composite key
/// `(sha256, name)`: if the row already exists for that pair, this is
/// a no-op (no UPDATE, since the GRANT shape forbids it — the
/// `ON CONFLICT DO NOTHING` shape stays within the runtime role's
/// permissions). Returns the sha256 either way so the caller can
/// record it in the prompt cache.
///
/// Migration 0011 (issue #20) bumped the PK from `(sha256)` to
/// `(sha256, name)`: two prompt files with identical content but
/// different names now each get their own row, and a rename creates a
/// fresh row instead of silently aliasing to the first-seen name.
pub async fn upsert_prompt(
    pool: &PgPool,
    name: &str,
    content: &str,
) -> Result<String, DbError> {
    let sha = hash_content(content);
    sqlx::query(
        "INSERT INTO agent_prompts (sha256, name, content) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (sha256, name) DO NOTHING",
    )
    .bind(&sha)
    .bind(name)
    .bind(content)
    .execute(pool)
    .await
    .map_err(|e| DbError::Query(format!("agent_prompts upsert: {e}")))?;
    Ok(sha)
}

/// Fetch prompt content by hash. Used by the future CASSANDRA
/// reviewer for forensic correlation; not called by the scheduler
/// runtime path (which keeps content in the in-memory PromptCache).
pub async fn get_by_hash(
    pool: &PgPool,
    sha256: &str,
) -> Result<Option<String>, DbError> {
    let row = sqlx::query("SELECT content FROM agent_prompts WHERE sha256 = $1")
        .bind(sha256)
        .fetch_optional(pool)
        .await
        .map_err(|e| DbError::Query(format!("agent_prompts get_by_hash: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let content = row
        .try_get::<String, _>("content")
        .map_err(|e| DbError::Query(format!("decode agent_prompts.content: {e}")))?;
    Ok(Some(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_content_is_64_chars_lowercase_hex() {
        let h = hash_content("hello");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn hash_content_is_deterministic() {
        assert_eq!(hash_content("abc"), hash_content("abc"));
        assert_ne!(hash_content("abc"), hash_content("abcd"));
    }

    #[test]
    fn hash_content_known_vector() {
        // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            hash_content("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
