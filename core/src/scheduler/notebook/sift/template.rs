//! The candidate list a sift goes through, and each item's reads (#922). Pure.

use serde_json::Value;

use super::params::ReadTemplate;

/// Attachment extensions `{….*}` skips: the reads cannot extract text from
/// an image, and a read that fails costs one of the sift's reads (K3b).
pub(crate) const IMAGE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "bmp", "webp", "heic"];

/// The list a sift goes through: the value at `path` in `result` (a dot
/// path, as in [`value_at`]), or `result` itself when no path is given. It
/// must be a list; anything else is refused in words naming the path.
pub(crate) fn items_at(result: &Value, path: Option<&str>) -> Result<Vec<Value>, String> {
    let (list, named) = match path {
        Some(p) => (value_at(result, p).ok_or_else(|| format!("the result has no `{p}`"))?, format!("`{p}`")),
        None => (result, "the result (name where its list is with `items`)".to_string()),
    };
    list.as_array().cloned().ok_or_else(|| format!("{named} is not a list"))
}

/// The value at a dot path: an object key per segment, or an array index
/// when the segment is a number (`attachments.0`).
pub(crate) fn value_at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |v, seg| match v {
        Value::Array(a) => a.get(seg.parse::<usize>().ok()?),
        Value::Object(o) => o.get(seg),
        _ => None,
    })
}

/// An earlier sift's kept row as a sift `over` its scrapbook sees it:
/// `(source, fields)`, or `None` for any other item.
pub(crate) fn scrap_parts(item: &Value) -> Option<(&Value, &serde_json::Map<String, Value>)> {
    Some((item.get("source").filter(|s| s.is_object())?, item.get("fields")?.as_object()?))
}

/// The value a placeholder `path` names in `item`: [`value_at`] the item;
/// for a scrapbook row, failing that, its `source` (the message id a mail
/// read needs), then its `fields` — the exact key first, as a kept field's
/// name may hold a dot (`from.address`). The planner writes a follow-up
/// sift's reads as for the original items (DGX tasks 486/487: every item
/// went unread on `{message_id}` and `{attachments.*}`).
fn item_at<'a>(item: &'a Value, path: &str) -> Option<&'a Value> {
    value_at(item, path).or_else(|| {
        let (source, fields) = scrap_parts(item)?;
        value_at(source, path).or_else(|| fields.get(path)).or_else(|| {
            let (key, rest) = path.split_once('.')?;
            value_at(fields.get(key)?, rest)
        })
    })
}

/// The parameter objects of `item`'s reads: `t.parameters` with every
/// placeholder filled from the item. A placeholder is a whole string value
/// `"{path}"` and takes the item's value at that path ([`item_at`]), of whatever JSON type
/// (a `"x {id}"` inside other text is left as written). One placeholder may
/// fan out with a `*` segment (`{attachments.*}`): one read per element of
/// that list, skipping image names. Any placeholder that does not resolve —
/// or a second `*` — gives no reads from this template ([`first_reads`] then
/// tries the next).
pub(crate) fn expand_reads(t: &ReadTemplate, item: &Value) -> Vec<Value> {
    let mut stars = Vec::new();
    collect_star_paths(&t.parameters, &mut stars);
    match stars.as_slice() {
        [] => fill(&t.parameters, item, None).into_iter().collect(),
        [path] => {
            let (prefix, suffix) = split_star(path);
            let Some(list) = item_at(item, prefix).and_then(Value::as_array) else { return Vec::new() };
            list.iter()
                .filter_map(|el| {
                    let picked = if suffix.is_empty() { Some(el) } else { value_at(el, suffix) }?;
                    if picked.as_str().is_some_and(is_image) {
                        return None;
                    }
                    fill(&t.parameters, item, Some(picked))
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// An item's reads from the first of `reads` that gives it any
/// ([`expand_reads`]), each with that read's index: an attachment read
/// yields nothing for a mail with no readable attachment, so the next read
/// (the message itself) applies. Empty when none does.
pub(crate) fn first_reads(reads: &[ReadTemplate], item: &Value) -> Vec<(usize, Value)> {
    reads
        .iter()
        .enumerate()
        .map(|(i, t)| expand_reads(t, item).into_iter().map(|v| (i, v)).collect::<Vec<_>>())
        .find(|r| !r.is_empty())
        .unwrap_or_default()
}

/// The first placeholder of `t` that names nothing in `item`, as written
/// (`` `{message_id}` ``), or a second `*` — named first, as
/// [`expand_reads`] refuses it before resolving anything. A `*`
/// placeholder names nothing when the path before the `*` is not a list,
/// or no element of a non-empty list has the path after it. `None` when
/// every placeholder resolves: an empty list, or one of images only, gives
/// no reads with nothing misspelled. Mirrors [`expand_reads`]'s rules, so a
/// sift that made no reads can say why (#956: DGX tasks 438 and 448 were
/// told only `unread`).
pub(crate) fn unresolved(t: &ReadTemplate, item: &Value) -> Option<String> {
    let mut paths = Vec::new();
    collect_paths(&t.parameters, &mut paths);
    if let Some(second) = paths.iter().filter(|p| p.contains('*')).nth(1) {
        return Some(format!("`{{{second}}}` is a second `*` (a read fans out over one list only)"));
    }
    let missing = |path: &&&str| {
        if !path.contains('*') {
            return item_at(item, path).is_none();
        }
        let (prefix, suffix) = split_star(path);
        match item_at(item, prefix).and_then(Value::as_array) {
            None => true,
            Some(list) => !suffix.is_empty() && !list.is_empty() && list.iter().all(|el| value_at(el, suffix).is_none()),
        }
    };
    paths.iter().find(missing).map(|path| format!("`{{{path}}}`"))
}

/// Most of an item's field names [`unread_why`] lists.
pub(crate) const WHY_MAX_FIELDS: usize = 12;

/// [`unread_why`] when no placeholder named nothing in any item looked at.
pub(crate) const NOTHING_READABLE: &str =
    "every placeholder named something, but each list was empty or held only images";

/// Why none of `reads` applied to `items` (the items looked at; `items[0]`
/// is item `from` of the list), for a sift whose every item went unread:
/// for the first item where any read's placeholder names nothing, each
/// such read's first one ([`unresolved`]), then that item's field names,
/// so the planner can fix the placeholder (#956). The names are
/// third-party text, so only short identifier-like ones are listed, at
/// most [`WHY_MAX_FIELDS`]; the rest are counted. [`NOTHING_READABLE`]
/// when every placeholder resolves in every item.
pub(crate) fn unread_why(reads: &[ReadTemplate], items: &[Value], from: usize) -> String {
    items
        .iter()
        .enumerate()
        .find_map(|(i, item)| item_why(reads, item, from + i))
        .unwrap_or_else(|| NOTHING_READABLE.to_string())
}

fn item_why(reads: &[ReadTemplate], item: &Value, n: usize) -> Option<String> {
    let parts: Vec<String> = reads
        .iter()
        .filter_map(|t| {
            let what = unresolved(t, item)?;
            Some(format!("{}: {what} names nothing in item {n}", super::params::read_key(&t.tool, &t.method)))
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    // A scrapbook row lists the names a placeholder is meant for: its source's
    // keys, then its fields' (a key in both, once).
    let keys: Option<Vec<&String>> = match scrap_parts(item) {
        Some((source, fields)) => Some(
            source.as_object().into_iter().flat_map(|s| s.keys()).chain(fields.keys().filter(|k| source.get(k.as_str()).is_none())).collect(),
        ),
        None => item.as_object().map(|o| o.keys().collect()),
    };
    let fields = match keys {
        None => "it is not an object".to_string(),
        Some(o) if o.is_empty() => "it has no fields".to_string(),
        Some(o) => {
            let named = |k: &&&String| k.len() <= 40 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            let shown: Vec<&str> = o.iter().filter(named).take(WHY_MAX_FIELDS).map(|k| k.as_str()).collect();
            let more = o.len() - shown.len();
            match (shown.is_empty(), more) {
                (true, _) => format!("its {more} fields have no listable names"),
                (false, 0) => format!("its fields: {}", shown.join(", ")),
                (false, _) => format!("its fields: {}, and {more} more", shown.join(", ")),
            }
        }
    };
    Some(format!("{} ({fields})", parts.join("; ")))
}

/// Is `name` an image, by its extension (case-insensitive)?
pub(crate) fn is_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    match lower.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && IMAGE_EXTENSIONS.contains(&ext),
        None => false,
    }
}

/// The path inside a placeholder string, or `None` for any other string.
fn placeholder(s: &str) -> Option<&str> {
    let inner = s.strip_prefix('{')?.strip_suffix('}')?;
    let plain = !inner.is_empty() && !inner.contains(['{', '}']) && !inner.contains(char::is_whitespace);
    plain.then_some(inner)
}

fn split_star(path: &str) -> (&str, &str) {
    let (prefix, suffix) = path.split_once('*').unwrap_or((path, ""));
    (prefix.trim_end_matches('.'), suffix.trim_start_matches('.'))
}

fn collect_star_paths<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    let mut all = Vec::new();
    collect_paths(v, &mut all);
    out.extend(all.into_iter().filter(|p| p.contains('*')));
}

/// Every placeholder's path in `v`, in [`fill`]'s order.
fn collect_paths<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.extend(placeholder(s)),
        Value::Array(a) => a.iter().for_each(|x| collect_paths(x, out)),
        Value::Object(o) => o.values().for_each(|x| collect_paths(x, out)),
        _ => {}
    }
}

/// `v` with its placeholders filled; `star` is the fan-out element for the
/// one `*` placeholder. `None` when a placeholder does not resolve.
fn fill(v: &Value, item: &Value, star: Option<&Value>) -> Option<Value> {
    Some(match v {
        Value::String(s) => match placeholder(s) {
            Some(p) if p.contains('*') => star?.clone(),
            Some(p) => item_at(item, p)?.clone(),
            None => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(|x| fill(x, item, star)).collect::<Option<_>>()?),
        Value::Object(o) => Value::Object(
            o.iter().map(|(k, x)| Some((k.clone(), fill(x, item, star)?))).collect::<Option<_>>()?,
        ),
        other => other.clone(),
    })
}

#[cfg(test)]
mod tests;
