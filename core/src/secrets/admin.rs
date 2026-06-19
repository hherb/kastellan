//! Operator-facing write path for `db::secrets`, used by the
//! `kastellan-cli secret` command. Kept in the library (not the CLI
//! binary) so the PG integration test can drive it with a
//! `MapKeyProvider` instead of the real OS keyring.
//!
//! All calls use `extra_aad = None` to match the Vault's
//! `materialize(.., None)` convention — a non-None AAD here would make
//! the daemon's bootstrap materialize fail. Audit rows are
//! metadata-only (name + key_id); the plaintext never appears.

use sqlx::PgPool;

use kastellan_db::secrets::KeyProvider;

/// Whether a `store_secret` created a new row or updated an existing one.
/// The label is best-effort (existence is checked before the upsert,
/// which is itself atomic) — purely for the operator-facing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Updated,
}

/// Errors from the secret admin path.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("secret admin: {0}")]
    Secrets(#[from] kastellan_db::secrets::SecretsError),

    /// Audit write failed. No `#[from]`: `DbError` is the crate-wide
    /// error for `kastellan_db`; an explicit map keeps a future DbError
    /// from being swallowed silently (mirrors `VaultError::Audit`).
    #[error("secret admin: audit insert failed: {0}")]
    Audit(kastellan_db::DbError),
}

/// UPSERT a named secret, then write a metadata-only `secret.put` audit
/// row. Returns whether the row was created or updated.
pub async fn store_secret(
    pool: &PgPool,
    key_provider: &dyn KeyProvider,
    name: &str,
    value: &[u8],
) -> Result<Outcome, AdminError> {
    // Existence pre-check for the created/updated label. `list` is
    // metadata-only and cheap (single-user server, few secrets).
    let existed = kastellan_db::secrets::list(pool)
        .await?
        .iter()
        .any(|s| s.name == name);

    kastellan_db::secrets::put(pool, key_provider, name, value, None).await?;

    let key_id = key_provider.current_id();
    kastellan_db::audit::insert(
        pool,
        "cli",
        "secret.put",
        serde_json::json!({ "name": name, "key_id": key_id }),
    )
    .await
    .map_err(AdminError::Audit)?;

    Ok(if existed {
        Outcome::Updated
    } else {
        Outcome::Created
    })
}

/// Delete a named secret. Writes a `secret.deleted` audit row only when
/// a row was actually removed. Returns whether anything was deleted.
pub async fn remove_secret(pool: &PgPool, name: &str) -> Result<bool, AdminError> {
    let deleted = kastellan_db::secrets::delete(pool, name).await?;
    if deleted {
        kastellan_db::audit::insert(
            pool,
            "cli",
            "secret.deleted",
            serde_json::json!({ "name": name }),
        )
        .await
        .map_err(AdminError::Audit)?;
    }
    Ok(deleted)
}
