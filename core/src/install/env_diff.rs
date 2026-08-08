//! What an install is about to destroy, named before it destroys it.
//!
//! `kastellan-cli install` regenerates `kastellan.env` from CLI flags, so any
//! hand-added key is dropped and any hand-tuned value reverts to the flag
//! default. On 2026-08-08 that silently removed the deployed agent's mail
//! capability for two days: with `KASTELLAN_MAIL_ENDPOINT` gone the `mail.*`
//! tools never registered, the planner fell back to filesystem probing, and the
//! only symptom was a wrong answer. See [#458].
//!
//! This module is the pure half of the fix: compare the file about to be
//! overwritten against the freshly rendered content and report the difference by
//! **key name only**. Values stay out of the install transcript — the operator
//! reads them from the `.bak` copy the caller writes — because an env file may
//! one day hold something that should not be echoed to a terminal.
//!
//! [#458]: https://github.com/hherb/kastellan/issues/458

use kastellan_supervisor::env_file::parse_env_file;

/// Keys an install would drop or change, in the old file's order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvDiff {
    /// Present in the old file, absent from the new one.
    pub lost: Vec<String>,
    /// Present in both with a different value.
    pub changed: Vec<String>,
}

impl EnvDiff {
    /// True when the install destroys nothing — the common case, and the
    /// condition under which the caller writes no backup and prints nothing.
    pub fn is_empty(&self) -> bool {
        self.lost.is_empty() && self.changed.is_empty()
    }
}

/// Diff two `EnvironmentFile` buffers by key.
///
/// Only uncommented `KEY=value` lines count, via the shared
/// [`kastellan_supervisor::env_file::parse_env_file`] grammar — so the commented
/// defaults `render_env_file` emits are not mistaken for keys, and a key the
/// operator *uncommented* is correctly reported as lost.
///
/// Keys present only in `new` are not reported: that is the installer adding
/// something, not destroying it. Output follows `old`'s line order so the
/// operator-facing message is deterministic, and each key is reported at most
/// once even if the source file repeats it.
///
/// A key's operative value is its **last** occurrence in the file, matching
/// systemd's precedence and the behaviour of `merge_env` elsewhere in this crate.
pub fn diff_env_files(old: &str, new: &str) -> EnvDiff {
    use std::collections::HashMap;

    let new_pairs = parse_env_file(new);
    // Both sides are bound to locals first: `parse_env_file` returns an owned
    // Vec, and iterating it inline would drop the temporary while the borrowed
    // &str keys are still in use.
    let old_pairs = parse_env_file(old);

    // Build maps of key -> last_value for efficient lookup and correct comparison.
    // Multiple occurrences of the same key use the last one (systemd behaviour).
    let mut new_values: HashMap<&str, &str> = HashMap::new();
    for (key, value) in new_pairs.iter() {
        new_values.insert(key.as_str(), value.as_str());
    }

    let mut old_values: HashMap<&str, &str> = HashMap::new();
    for (key, value) in old_pairs.iter() {
        old_values.insert(key.as_str(), value.as_str());
    }

    let mut diff = EnvDiff::default();
    let mut seen: Vec<&str> = Vec::new();

    // Iterate through old_pairs in order to preserve first-appearance ordering
    for (key, _) in old_pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())) {
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);

        let old_value = old_values[key];
        match new_values.get(key) {
            None => diff.lost.push(key.to_string()),
            Some(&new_value) if new_value != old_value => diff.changed.push(key.to_string()),
            Some(_) => {}
        }
    }
    diff
}

#[cfg(test)]
mod tests;
