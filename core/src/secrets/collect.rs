//! Pure helper: enumerate the `secret://` references a params tree carries,
//! WITHOUT redeeming them. Used by the dispatch chokepoint (egress slice #3b,
//! #268) to learn which secrets a worker is about to receive — the one-way
//! `RedemptionEvent.ref_hash` cannot be reversed to a `SecretRef`, so we
//! re-scan the pre-substitution params instead.

use std::collections::HashSet;

use super::substitute::is_well_formed_ref;
use super::vault::SecretRef;

/// Walk `value` and return every well-formed `secret://<8-hex>` reference it
/// contains, dedup'd by `ref_hash`, in first-seen order (deterministic for
/// tests). Pure: no vault, no I/O, no mutation. Mirrors the JSON walk shape of
/// [`super::substitute`].
pub fn collect_refs_in_params(value: &serde_json::Value) -> Vec<SecretRef> {
    let mut out: Vec<SecretRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    walk(value, &mut out, &mut seen);
    out
}

fn walk(value: &serde_json::Value, out: &mut Vec<SecretRef>, seen: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(s) => {
            if is_well_formed_ref(s) {
                let r = SecretRef::from_raw(s.clone());
                if seen.insert(r.ref_hash()) {
                    out.push(r);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for it in items {
                walk(it, out, seen);
            }
        }
        serde_json::Value::Object(map) => {
            for (_k, v) in map.iter() {
                walk(v, out, seen);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hashes(refs: &[SecretRef]) -> Vec<String> {
        refs.iter().map(|r| r.ref_hash()).collect()
    }

    #[test]
    fn finds_refs_nested_in_objects_and_arrays() {
        let v = json!({
            "a": "secret://deadbeef",
            "b": ["plain", {"c": "secret://cafef00d"}],
        });
        let got = collect_refs_in_params(&v);
        assert_eq!(got.len(), 2);
        assert_eq!(
            hashes(&got),
            hashes(&[
                SecretRef::from_raw("secret://deadbeef".into()),
                SecretRef::from_raw("secret://cafef00d".into()),
            ])
        );
    }

    #[test]
    fn dedups_repeated_ref_first_seen_order() {
        let v = json!(["secret://deadbeef", "secret://deadbeef"]);
        let got = collect_refs_in_params(&v);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn ignores_non_ref_strings_and_malformed_refs() {
        let v = json!({
            "plain": "hello",
            "almost": "secret://nothex!!",
            "short": "secret://dead",
        });
        assert!(collect_refs_in_params(&v).is_empty());
    }

    #[test]
    fn empty_params_yield_no_refs() {
        assert!(collect_refs_in_params(&json!({})).is_empty());
        assert!(collect_refs_in_params(&serde_json::Value::Null).is_empty());
    }
}
