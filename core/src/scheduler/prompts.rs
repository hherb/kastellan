//! Prompt loading + ledger.
//!
//! At daemon startup, every `prompts/*.md` file is read, hashed, and
//! upserted into `agent_prompts` (idempotent on existing sha256). The
//! runtime caches `name → (sha256, content)` in memory; the inner
//! loop's `formulate_plan` reads from the cache, never from disk.
//!
//! Editing a prompt is a commit + daemon restart. The next startup
//! observes a new sha256 and inserts a new ledger row; old rows are
//! preserved forever (append-only by GRANT, migration 0006).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use sqlx::PgPool;
use thiserror::Error;
use tokio::fs;

use hhagent_db::agent_prompts;

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("io error reading {path:?}: {source}")]
    Io { path: std::path::PathBuf, source: std::io::Error },
    #[error("db error: {0}")]
    Db(#[from] hhagent_db::DbError),
    #[error("prompt name has invalid characters: {0:?}")]
    InvalidName(String),
}

/// Load all `.md` files under `dir` into a `PromptCache`. Each file's
/// stem (without the `.md`) becomes its `name`; its content is read,
/// hashed, and upserted into `agent_prompts`. Non-`.md` files are
/// ignored.
pub async fn load_prompts_from_dir(
    pool: &PgPool,
    dir: &Path,
) -> Result<Arc<PromptCache>, PromptError> {
    let mut cache = PromptCache::default();
    let mut rd = fs::read_dir(dir).await
        .map_err(|e| PromptError::Io { path: dir.to_path_buf(), source: e })?;
    while let Some(entry) = rd.next_entry().await
        .map_err(|e| PromptError::Io { path: dir.to_path_buf(), source: e })?
    {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str())
            .ok_or_else(|| PromptError::InvalidName(format!("{:?}", path)))?
            .to_string();
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(PromptError::InvalidName(name));
        }
        let content = fs::read_to_string(&path).await
            .map_err(|e| PromptError::Io { path: path.clone(), source: e })?;
        let sha256 = agent_prompts::upsert_prompt(pool, &name, &content).await?;
        cache.entries.insert(name, PromptEntry { sha256, content });
    }
    Ok(Arc::new(cache))
}

#[derive(Clone, Debug)]
pub struct PromptEntry {
    pub sha256: String,
    pub content: String,
}

/// In-memory cache of every prompt loaded at daemon startup. Shared
/// across both lane runners via `Arc<PromptCache>`.
#[derive(Debug, Default)]
pub struct PromptCache {
    entries: HashMap<String, PromptEntry>,
}

impl PromptCache {
    pub fn get(&self, name: &str) -> Option<&PromptEntry> {
        self.entries.get(name)
    }

    /// Construct an in-memory cache directly without touching disk or
    /// the DB. Used by inner-loop integration tests that don't need
    /// the ledger round-trip.
    pub fn new_for_test(entries: Vec<(String, PromptEntry)>) -> Self {
        Self { entries: entries.into_iter().collect() }
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_get_returns_entry() {
        let cache = PromptCache::new_for_test(vec![(
            "agent_planner".into(),
            PromptEntry { sha256: "abc".into(), content: "hello".into() },
        )]);
        let e = cache.get("agent_planner").unwrap();
        assert_eq!(e.sha256, "abc");
        assert_eq!(e.content, "hello");
        assert!(cache.get("missing").is_none());
    }

    #[test]
    fn cache_names_iterates_all() {
        let cache = PromptCache::new_for_test(vec![
            ("a".into(), PromptEntry { sha256: "1".into(), content: "x".into() }),
            ("b".into(), PromptEntry { sha256: "2".into(), content: "y".into() }),
        ]);
        let mut names: Vec<&str> = cache.names().collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }
}
