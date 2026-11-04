//! A `sift` call's parameters (#922), the scrapbook refs they may name, and
//! the read allowlist (K3a). Pure.

use serde::Deserialize;

use super::config::SiftConfig;
use super::SIFT_TOOL;

/// The most fields one sift may extract: each one is a column of the table
/// and a key in every sorter reply, so a long list costs on every item.
pub(crate) const MAX_FIELDS: usize = 16;

/// The most characters of `keep_if`: it is repeated in every sorter message.
pub(crate) const KEEP_IF_MAX_CHARS: usize = 1024;

/// The most reads one sift may name: each item takes the first that applies
/// to it (an attachment, else the message itself, DGX task 315).
pub(crate) const MAX_READS_NAMED: usize = 3;

/// The most characters of one field name: the turn record keeps names
/// whole (`conversation::record`), so a longer one is refused here, not cut.
pub(crate) const FIELD_NAME_MAX_CHARS: usize = crate::scheduler::conversation::record::FIELD_NAME_MAX_CHARS;

/// What a sift goes through: the last result of a notebook step, or an
/// earlier scrapbook (a follow-up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Over {
    Step(u32),
    Scrapbook(ScrapRef),
}

/// The read each item gets: a tool call whose parameter values may name the
/// item's own fields as `"{path}"` placeholders.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadTemplate {
    pub tool: String,
    pub method: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SiftParams {
    pub over: Over,
    pub items: Option<String>,
    pub from: usize,
    /// The reads, in order: each item takes the first that gives it any
    /// read ([`super::template::first_reads`]). Empty: decided on the hit.
    pub read: Vec<ReadTemplate>,
    pub keep_if: String,
    pub fields: Vec<String>,
}

/// A scrapbook's name, `sb:<task_id>:<sift>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrapRef {
    pub task_id: i64,
    pub sift: i32,
}

impl ScrapRef {
    /// Read `sb:<task_id>:<sift>`, both whole numbers from 1. Anything else
    /// — a sign, a missing or extra part — is refused: a ref is matched
    /// against the turns a task may read (K11), so it must mean one thing.
    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("`{s}` is not a scrapbook ref (`sb:<task>:<sift>`)");
        let rest = s.strip_prefix("sb:").ok_or_else(bad)?;
        let (task, sift) = rest.split_once(':').ok_or_else(bad)?;
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        if !digits(task) || !digits(sift) {
            return Err(bad());
        }
        let task_id: i64 = task.parse().map_err(|_| bad())?;
        let sift: i32 = sift.parse().map_err(|_| bad())?;
        if task_id < 1 || sift < 1 {
            return Err(bad());
        }
        Ok(ScrapRef { task_id, sift })
    }
}

impl std::fmt::Display for ScrapRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sb:{}:{}", self.task_id, self.sift)
    }
}

/// `over` as written: `{"step": n}` or `{"scrapbook": "sb:t:n"}`. An
/// externally tagged enum, so a second key is refused.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawOver {
    Step(u32),
    Scrapbook(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParams {
    over: RawOver,
    #[serde(default)]
    items: Option<String>,
    #[serde(default)]
    from: usize,
    /// One read, or a list of them ([`parse_read`]).
    #[serde(default)]
    read: Option<serde_json::Value>,
    keep_if: String,
    fields: Vec<String>,
    /// A summary's `continues` and `chain`, copied back by the model:
    /// ignored (the chain is code's to say, [`super::chain`]).
    #[serde(default, rename = "continues")]
    _continues: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "chain")]
    _chain: Option<serde::de::IgnoredAny>,
}

/// Parse and check a `sift.run` call's `parameters`: a non-blank criterion
/// of at most [`KEEP_IF_MAX_CHARS`], 1 to [`MAX_FIELDS`] distinct, non-blank
/// field names of at most [`FIELD_NAME_MAX_CHARS`], and a well-formed
/// `over`. The read is checked separately ([`check_read`]).
pub(crate) fn parse_params(v: &serde_json::Value) -> Result<SiftParams, String> {
    let raw: RawParams =
        serde_json::from_value(v.clone()).map_err(|e| format!("the sift's parameters are not usable: {e}"))?;
    if raw.keep_if.trim().is_empty() {
        return Err("`keep_if` is blank: say what an item must contain to be kept".into());
    }
    if raw.keep_if.chars().count() > KEEP_IF_MAX_CHARS {
        return Err(format!("`keep_if` is longer than {KEEP_IF_MAX_CHARS} characters: say it shorter"));
    }
    if raw.fields.is_empty() || raw.fields.len() > MAX_FIELDS {
        return Err(format!("`fields` must name 1 to {MAX_FIELDS} fields, not {}", raw.fields.len()));
    }
    let mut seen = std::collections::HashSet::new();
    for f in &raw.fields {
        if f.trim().is_empty() {
            return Err("a field name in `fields` is blank".into());
        }
        if f.chars().count() > FIELD_NAME_MAX_CHARS {
            return Err(format!("the field name `{f}` is longer than {FIELD_NAME_MAX_CHARS} characters"));
        }
        if !seen.insert(f.as_str()) {
            return Err(format!("`fields` names `{f}` twice"));
        }
    }
    let over = match raw.over {
        RawOver::Step(n) => Over::Step(n),
        RawOver::Scrapbook(r) => Over::Scrapbook(ScrapRef::parse(&r)?),
    };
    let read = parse_read(raw.read)?;
    Ok(SiftParams { over, items: raw.items, from: raw.from, read, keep_if: raw.keep_if, fields: raw.fields })
}

/// `read` as written: absent (or `null`), one read, or a list of 1 to
/// [`MAX_READS_NAMED`] (models write both, as for an answer's refs).
fn parse_read(v: Option<serde_json::Value>) -> Result<Vec<ReadTemplate>, String> {
    let one = |v: serde_json::Value| {
        serde_json::from_value::<ReadTemplate>(v).map_err(|e| format!("the sift's `read` is not usable: {e}"))
    };
    match v {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(list)) => {
            if list.is_empty() || list.len() > MAX_READS_NAMED {
                return Err(format!("`read` must list 1 to {MAX_READS_NAMED} reads, not {}", list.len()));
            }
            list.into_iter().map(one).collect()
        }
        Some(v) => Ok(vec![one(v)?]),
    }
}

/// A read's name as the allowlist spells it, `tool.method`: a worker's
/// method already carries its namespace (`mail.get_message`); a built-in's
/// does not (`handoff` + `fetch`).
pub(crate) fn read_key(tool: &str, method: &str) -> String {
    if method.starts_with(&format!("{tool}.")) {
        method.to_string()
    } else {
        format!("{tool}.{method}")
    }
}

/// K3a: a sift may read only with an allowlisted method, named in fixed
/// text — never a placeholder, never another sift or a skill, whatever the
/// list says.
pub(crate) fn check_read(t: &ReadTemplate, cfg: &SiftConfig) -> Result<(), String> {
    if [&t.tool, &t.method].iter().any(|s| s.contains('{') || s.contains('}')) {
        return Err("a read's `tool` and `method` are fixed text, never placeholders".into());
    }
    let key = read_key(&t.tool, &t.method);
    if t.tool == SIFT_TOOL || super::config::never_a_read(&key) {
        return Err(format!("`{key}` can never be a sift's read"));
    }
    if !cfg.read_methods.contains(&key) {
        return Err(format!("`{key}` is not a method a sift may read with (allowed: {})", cfg.read_methods.join(", ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
