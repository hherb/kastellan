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

/// The parameter objects of `item`'s reads: `t.parameters` with every
/// placeholder filled from the item. A placeholder is a whole string value
/// `"{path}"` and takes the item's value at that path, of whatever JSON type
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
            let Some(list) = value_at(item, prefix).and_then(Value::as_array) else { return Vec::new() };
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
    match v {
        Value::String(s) => out.extend(placeholder(s).filter(|p| p.contains('*'))),
        Value::Array(a) => a.iter().for_each(|x| collect_star_paths(x, out)),
        Value::Object(o) => o.values().for_each(|x| collect_star_paths(x, out)),
        _ => {}
    }
}

/// `v` with its placeholders filled; `star` is the fan-out element for the
/// one `*` placeholder. `None` when a placeholder does not resolve.
fn fill(v: &Value, item: &Value, star: Option<&Value>) -> Option<Value> {
    Some(match v {
        Value::String(s) => match placeholder(s) {
            Some(p) if p.contains('*') => star?.clone(),
            Some(p) => value_at(item, p)?.clone(),
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
