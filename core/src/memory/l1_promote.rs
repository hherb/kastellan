//! Writer for `MemoryLayer::Index` (L1) rows. Two callers:
//!
//! 1. **Operator** — via `kastellan-cli memory l1 add <body>` →
//!    `crate::cli_audit::l1_add_and_audit`.
//! 2. **Agent-raised** — via `Plan.l1_insight` consumed by
//!    `crate::scheduler::runner::drain_lane` on `Outcome::Completed`.
//!
//! Both callers share the same validation + dedup discipline:
//! validate via [`validate_l1_body`], compute SHA-256, EXISTS-check
//! at `layer = 1` keyed on `metadata->>'body_sha256'`, insert on
//! miss via [`kastellan_db::memories::insert_memory_at_layer`].
//!
//! ## Side effect: entity auto-link
//!
//! On the agent-raised path (`runner::write_l1_promoted_row`), every
//! newly-inserted L1 row is passed to
//! [`crate::memory::entity_link::link_memory_entities`] in
//! degrade-and-warn posture; the resulting `Option<LinkOutcome>`
//! rides along in [`L1WriteOutcome::Inserted::link_outcome`]. The
//! operator path (`cli_audit::l1_add_and_audit`) injects a
//! [`crate::entity_extraction::NoOpEntityExtractor`] so operator-added
//! rows stay un-auto-linked by design (batch-relink subcommand is the
//! future workflow).
//!
//! See `docs/superpowers/specs/2026-05-17-l1-promotion-writer-design.md`
//! for the full design.

use kastellan_db::memories::{insert_memory_at_layer, load_layer, Memory, MemoryLayer};
use kastellan_db::DbError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use crate::entity_extraction::EntityExtractor;
use crate::memory::embedder::Embedder;
use crate::memory::entity_link::{link_memory_entities, LinkOutcome};
use crate::memory::layers::load_l1_default;

/// Maximum body length in bytes for an L1 row. Half of L0's
/// `L0_MAX_BODY_BYTES = 1024`; the L1 read cap is 4 KiB across
/// all rows so 512 leaves room for ~8 typical-length rows.
pub const L1_MAX_BODY_BYTES: usize = 512;

/// Reserved substring that would close the `<l1_insights>` block
/// rendered by the prompt assembler. An agent-raised body cannot
/// embed this without prompt-injection risk (threat-model §6).
const RESERVED_TAG_CLOSE: &str = "</l1_insights>";

/// Reserved substring for the open tag; symmetric defence even
/// though a stray open tag is less directly exploitable.
const RESERVED_TAG_OPEN: &str = "<l1_insights>";

/// Provenance for an L1 row write. The audit-row `source` field
/// is **never** producer-supplied; only the writer constructs this
/// variant (mirrors `ClassificationFloorSource::AgentRaised`
/// from issue #71).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum L1Source {
    /// Operator-explicit write via `kastellan-cli memory l1 add`.
    Operator,
    /// Agent-raised write from `runner::drain_lane` after
    /// `Outcome::Completed`. The originating `task_id` is carried
    /// in the audit-row payload for cross-restart trace stitching.
    AgentRaised { task_id: i64 },
}

/// Error kinds the L1 writer can produce.
#[derive(Debug, thiserror::Error)]
pub enum L1Error {
    #[error("L1 body validation failed: {0}")]
    Validation(String),

    #[error("L1 db error: {0}")]
    Db(#[from] DbError),
}

/// Outcome of a single `promote_l1` call.
#[derive(Clone, Debug)]
pub enum L1WriteOutcome {
    /// New L1 row inserted at the carried `memory_id`. `link_outcome`
    /// is `Some(_)` on auto-link success — INCLUDING the NoOp
    /// extractor case, which carries an empty
    /// [`LinkOutcome`] (`n_entities_linked = 0`, empty seeds). It is
    /// `None` ONLY when auto-link errored after the memory row was
    /// committed (extract or DB failure; a WARN was already logged).
    /// In short: `Some` means "the link step ran to completion"; the
    /// distinction between "0 entities linked because NoOp" and "link
    /// step errored out" is the `Some` / `None` split.
    ///
    /// Operator + agent callers can both ignore the new field — the
    /// variant widening is purely additive at the match level via `..`.
    Inserted {
        memory_id: i64,
        link_outcome: Option<LinkOutcome>,
    },
    /// A row with the same `body_sha256` already exists at
    /// `layer = 1` (carrying the existing `memory_id`). No new row
    /// was written; no link attempt was made.
    SkippedDuplicate { memory_id: i64 },
}

impl L1WriteOutcome {
    pub fn memory_id(&self) -> i64 {
        match self {
            L1WriteOutcome::Inserted { memory_id, .. }
            | L1WriteOutcome::SkippedDuplicate { memory_id } => *memory_id,
        }
    }
}

/// Validates an L1 body string. On success returns the trimmed slice
/// (so the writer never inserts leading/trailing whitespace). On
/// failure returns [`L1Error::Validation`] with a human-readable reason.
///
/// Rejections (in actual runtime order; first hit wins):
/// 1. Contains any newline (`\n` or `\r`) — checked on the **raw**
///    body BEFORE trim, so a trailing `\n` cannot silently slip
///    past via `.trim()` stripping it.
/// 2. Empty after trim.
/// 3. Contains any other ASCII control character (< 0x20, excluding
///    `\n`/`\r` which are caught by item 1; includes `\t` since
///    indented bullets would break the flat-insight format).
/// 4. Contains the literal substring `<l1_insights>` or `</l1_insights>`
///    (threat-model §6 defence — an agent-raised body cannot close
///    the trust-marked block early).
/// 5. Trimmed length exceeds [`L1_MAX_BODY_BYTES`].
pub fn validate_l1_body(body: &str) -> Result<&str, L1Error> {
    // Check newlines on the raw body first, before trimming, so that
    // leading/trailing newlines ("trailing\n", "\nleading") are caught
    // even though trim() would strip them.
    if body.contains('\n') || body.contains('\r') {
        return Err(L1Error::Validation("body contains newline".into()));
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(L1Error::Validation("body is empty after trim".into()));
    }
    if trimmed.bytes().any(|b| b < 0x20) {
        return Err(L1Error::Validation("body contains control character".into()));
    }
    if trimmed.contains(RESERVED_TAG_OPEN) || trimmed.contains(RESERVED_TAG_CLOSE) {
        return Err(L1Error::Validation("body contains reserved tag substring".into()));
    }
    if trimmed.len() > L1_MAX_BODY_BYTES {
        return Err(L1Error::Validation(format!(
            "body exceeds {L1_MAX_BODY_BYTES} bytes ({})",
            trimmed.len()
        )));
    }
    Ok(trimmed)
}

/// SHA-256 of the body, lowercase 64-char hex. Mirrors
/// [`crate::memory::l0_seed::compute_body_sha256`].
pub fn compute_body_sha256(body: &str) -> String {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    format!("{:x}", h.finalize())
}

/// Build the `metadata` JSONB blob for a new L1 row. Schema:
/// `{source, body_sha256, created_at, task_id?}`. `task_id` is
/// present iff `source` is `L1Source::AgentRaised`.
///
/// **Coupling note:** The literal strings `"operator"` and
/// `"agent_raised"` MUST match `L1Source`'s serde
/// `rename_all = "snake_case"` output. If you add a new `L1Source`
/// variant, update this function in lockstep. Cross-pinned by the
/// `build_l1_metadata_serde_agrees_with_l1_source` test below.
pub(crate) fn build_l1_metadata(
    source: &L1Source,
    body_sha256: &str,
    created_at_rfc3339: &str,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match source {
        L1Source::Operator => {
            obj.insert("source".into(), serde_json::Value::String("operator".into()));
        }
        L1Source::AgentRaised { task_id } => {
            obj.insert("source".into(), serde_json::Value::String("agent_raised".into()));
            obj.insert(
                "task_id".into(),
                serde_json::Value::Number(serde_json::Number::from(*task_id)),
            );
        }
    }
    obj.insert(
        "body_sha256".into(),
        serde_json::Value::String(body_sha256.into()),
    );
    obj.insert(
        "created_at".into(),
        serde_json::Value::String(created_at_rfc3339.into()),
    );
    serde_json::Value::Object(obj)
}

/// Promote a single L1 row. Validates, computes SHA-256, EXISTS-checks
/// against `layer = 1` rows by `metadata->>'body_sha256'`, inserts on
/// miss. Idempotent on body SHA-256 across all source variants — the
/// dedup is source-agnostic (a body the operator added is not promoted
/// again when the agent later raises it, and vice versa).
///
/// The `metadata` blob carries `{source, body_sha256, created_at, task_id?}`
/// per [`build_l1_metadata`].
///
/// **Embedding:** populated lazily via the injected [`Embedder`] — but
/// only after the dedup EXISTS-check passes, so a duplicate body never
/// triggers an embed call. The agent-raised path injects a
/// [`crate::memory::RouterEmbedder`] (truncated to `EMBEDDING_DIM`,
/// unit-norm, with an `action='embed'` audit row); the operator CLI path
/// injects a [`crate::memory::NoOpEmbedder`] so operator rows stay
/// embedding-free. On embed failure the row is stored with a NULL
/// embedding (graceful degradation, mirroring the entity auto-linker
/// below). A NULL-embedding row is simply skipped by `semantic_search`
/// (`WHERE embedding IS NOT NULL`); it stays retrievable via the lexical
/// and graph lanes.
pub async fn promote_l1(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    embedder: &dyn Embedder,
    body: &str,
    source: L1Source,
) -> Result<L1WriteOutcome, L1Error> {
    let trimmed = validate_l1_body(body)?;
    let body_sha256 = compute_body_sha256(trimmed);

    // EXISTS-check keyed on metadata->>'body_sha256' at layer = 1.
    // No `ORDER BY` — dedup doesn't care which matching row's id we get,
    // just whether ANY row exists. A future partial unique index on
    // `(metadata->>'body_sha256') WHERE layer = 1` would also benefit
    // from the omission (no forced index-ordered scan).
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM memories \
         WHERE layer = $1 AND metadata->>'body_sha256' = $2 \
         LIMIT 1",
    )
    .bind(MemoryLayer::Index.as_db())
    .bind(&body_sha256)
    .fetch_optional(pool)
    .await
    .map_err(|e| L1Error::Db(kastellan_db::DbError::Query(
        format!("promote_l1 EXISTS-check body_sha256={body_sha256}: {e}")
    )))?;

    if let Some(existing_id) = existing {
        return Ok(L1WriteOutcome::SkippedDuplicate { memory_id: existing_id });
    }

    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 format");
    let metadata = build_l1_metadata(&source, &body_sha256, &created_at);

    // Embed AFTER the dedup miss so a duplicate body never triggers an
    // embed call. On embed failure the embedder returns None (it logs the
    // WARN); the row is stored with a NULL embedding rather than blocking
    // the insight write.
    let embedding = embedder.embed_for_storage(trimmed).await;

    let new_id = insert_memory_at_layer(
        pool,
        trimmed,
        &metadata,
        embedding.as_deref(),
        MemoryLayer::Index,
    )
    .await?;

    // Auto-link entities. Same degrade-and-warn posture as L0:
    // a failure here leaves the L1 row unlinked but otherwise intact.
    let link_outcome = match link_memory_entities(
        pool, extractor, new_id, "L1", trimmed,
    )
    .await
    {
        Ok(outcome) => Some(outcome),
        Err(e) => {
            tracing::warn!(
                error = %e, memory_id = new_id, layer = "L1",
                "auto-linker degraded; memory survives unlinked"
            );
            None
        }
    };

    Ok(L1WriteOutcome::Inserted { memory_id: new_id, link_outcome })
}

/// Operator-facing list view.
///
/// - `all = false` returns the **in-prompt** slice via `load_l1_default`
///   (newest-first, capped at 32 rows / 4 KiB). What the prompt
///   assembler will actually render.
/// - `all = true` returns every row at `layer = 1` (newest-first,
///   no byte cap, no row cap). For operator audit / cleanup.
pub async fn list_l1(pool: &PgPool, all: bool) -> Result<Vec<Memory>, DbError> {
    if all {
        load_layer(pool, MemoryLayer::Index, usize::MAX).await
    } else {
        load_l1_default(pool).await
    }
}

/// Operator-facing remove. Layer-guarded via
/// `kastellan_db::memories::delete_memory_at_layer`: cannot delete
/// an L0 / L2 / L3 row even if the operator typoed the id.
///
/// Returns `true` iff a row was deleted.
pub async fn remove_l1(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    kastellan_db::memories::delete_memory_at_layer(pool, id, MemoryLayer::Index).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_after_trim() {
        let err = validate_l1_body("   \t  ").expect_err("empty");
        match err {
            L1Error::Validation(msg) => assert!(msg.contains("empty"), "{msg}"),
            _ => panic!("wrong error kind"),
        }
    }

    #[test]
    fn validate_rejects_newlines() {
        for s in &["foo\nbar", "foo\r\nbar", "trailing\n", "\nleading", "foo\rbar"] {
            let err = validate_l1_body(s).expect_err(s);
            match err {
                L1Error::Validation(msg) => assert!(msg.contains("newline"), "got: {msg}"),
                _ => panic!("wrong error kind for {s:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_control_chars() {
        for s in &["foo\tbar", "foo\x00bar", "foo\x07bar"] {
            let err = validate_l1_body(s).expect_err(s);
            match err {
                L1Error::Validation(msg) => {
                    assert!(msg.contains("control character") || msg.contains("newline"), "got: {msg}");
                }
                _ => panic!("wrong error kind for {s:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_reserved_tag_substring() {
        for s in &[
            "innocuous <l1_insights> not so innocuous",
            "before</l1_insights>after",
            "</l1_insights>",
        ] {
            let err = validate_l1_body(s).expect_err(s);
            match err {
                L1Error::Validation(msg) => assert!(msg.contains("reserved tag"), "got: {msg}"),
                _ => panic!("wrong error kind for {s:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_over_length() {
        let body = "a".repeat(L1_MAX_BODY_BYTES + 1);
        let err = validate_l1_body(&body).expect_err("over-length");
        match err {
            L1Error::Validation(msg) => {
                assert!(msg.contains("exceeds 512 bytes"), "got: {msg}");
                assert!(msg.contains(&format!("({})", L1_MAX_BODY_BYTES + 1)), "got: {msg}");
            }
            _ => panic!("wrong error kind"),
        }
    }

    #[test]
    fn validate_accepts_exact_cap() {
        let body = "a".repeat(L1_MAX_BODY_BYTES);
        let trimmed = validate_l1_body(&body).expect("at-cap");
        assert_eq!(trimmed.len(), L1_MAX_BODY_BYTES);
    }

    #[test]
    fn validate_returns_trimmed_slice() {
        let body = "   shell-exec /bin/ls works   ";
        let trimmed = validate_l1_body(body).expect("ok");
        assert_eq!(trimmed, "shell-exec /bin/ls works");
    }

    #[test]
    fn validate_accepts_typical_body() {
        let body = "shell-exec /usr/bin/ls reliably enumerates dir contents";
        let trimmed = validate_l1_body(body).expect("ok");
        assert_eq!(trimmed, body);
    }

    #[test]
    fn compute_body_sha256_is_deterministic_and_64_hex() {
        let s1 = compute_body_sha256("hello");
        let s2 = compute_body_sha256("hello");
        assert_eq!(s1, s2, "deterministic");
        assert_eq!(s1.len(), 64, "64-char hex");
        assert!(s1.chars().all(|c| c.is_ascii_hexdigit() && (!c.is_ascii_alphabetic() || c.is_ascii_lowercase())), "lowercase hex");
    }

    #[test]
    fn compute_body_sha256_distinct_for_distinct_inputs() {
        assert_ne!(compute_body_sha256("hello"), compute_body_sha256("hellp"));
    }

    #[test]
    fn build_l1_metadata_operator_has_no_task_id() {
        let m = build_l1_metadata(
            &L1Source::Operator,
            "abc123",
            "2026-05-17T12:00:00Z",
        );
        let obj = m.as_object().expect("object");
        assert_eq!(obj.get("source").unwrap(), "operator");
        assert_eq!(obj.get("body_sha256").unwrap(), "abc123");
        assert_eq!(obj.get("created_at").unwrap(), "2026-05-17T12:00:00Z");
        assert!(obj.get("task_id").is_none(), "Operator must NOT carry task_id");
        assert_eq!(obj.len(), 3, "exactly 3 keys for Operator");
    }

    #[test]
    fn build_l1_metadata_agent_raised_carries_task_id() {
        let m = build_l1_metadata(
            &L1Source::AgentRaised { task_id: 42 },
            "def456",
            "2026-05-17T12:00:01Z",
        );
        let obj = m.as_object().expect("object");
        assert_eq!(obj.get("source").unwrap(), "agent_raised");
        assert_eq!(obj.get("task_id").unwrap(), 42);
        assert_eq!(obj.get("body_sha256").unwrap(), "def456");
        assert_eq!(obj.get("created_at").unwrap(), "2026-05-17T12:00:01Z");
        assert_eq!(obj.len(), 4, "exactly 4 keys for AgentRaised");
    }

    #[test]
    fn l1_source_serializes_as_snake_case_internally_tagged() {
        let op = serde_json::to_value(&L1Source::Operator).expect("serialize");
        assert_eq!(op, serde_json::json!({"source": "operator"}));

        let ag = serde_json::to_value(&L1Source::AgentRaised { task_id: 7 }).expect("serialize");
        assert_eq!(ag, serde_json::json!({"source": "agent_raised", "task_id": 7}));
    }

    #[test]
    fn promote_l1_signature_compile_pin() {
        // Compile-only smoke: verify the function signature stays
        // (pool: &PgPool, extractor: &dyn EntityExtractor, embedder: &dyn Embedder,
        //  body: &str, source: L1Source)
        // -> Result<L1WriteOutcome, L1Error>.
        // Full DB-backed coverage lives in core/tests/memory_l1_promote_e2e.rs.
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            extractor: &'a dyn crate::entity_extraction::EntityExtractor,
            embedder: &'a dyn crate::memory::embedder::Embedder,
            body: &'a str,
            source: L1Source,
        ) -> impl std::future::Future<Output = Result<L1WriteOutcome, L1Error>> + 'a {
            promote_l1(pool, extractor, embedder, body, source)
        }
        // No call — just ensure the function exists and has this signature.
        let _ = _signature_pin;
    }

    #[test]
    fn build_l1_metadata_serde_agrees_with_l1_source() {
        // Cross-pin: the source string emitted by build_l1_metadata
        // must equal the string emitted by L1Source's serde representation.
        // A future variant addition that updates one but not the other
        // trips this test.
        for (source, expected_source_str) in &[
            (L1Source::Operator, "operator"),
            (L1Source::AgentRaised { task_id: 42 }, "agent_raised"),
        ] {
            let serde_value = serde_json::to_value(source).expect("serialize");
            let serde_source = serde_value.get("source").unwrap().as_str().unwrap();
            assert_eq!(serde_source, *expected_source_str, "L1Source serde drift");

            let metadata = build_l1_metadata(source, "sha", "2026-05-17T00:00:00Z");
            let metadata_source = metadata.get("source").unwrap().as_str().unwrap();
            assert_eq!(metadata_source, *expected_source_str, "build_l1_metadata drift");
        }
    }

    #[test]
    fn list_l1_signature_compile_pin() {
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            all: bool,
        ) -> impl std::future::Future<Output = Result<Vec<kastellan_db::memories::Memory>, kastellan_db::DbError>> + 'a {
            list_l1(pool, all)
        }
        let _ = _signature_pin;
    }

    #[test]
    fn remove_l1_signature_compile_pin() {
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            id: i64,
        ) -> impl std::future::Future<Output = Result<bool, kastellan_db::DbError>> + 'a {
            remove_l1(pool, id)
        }
        let _ = _signature_pin;
    }
}
