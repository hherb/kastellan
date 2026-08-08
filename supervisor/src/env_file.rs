//! The `EnvironmentFile=` grammar, in one place.
//!
//! Deliberately `cfg`-free and shared rather than per-backend. The launchd
//! backend folds these pairs into a plist (launchd has no `EnvironmentFile=`
//! directive) and `kastellan-core`'s installer uses the same parser to diff the
//! env file it is about to overwrite. A second parser for one file format is
//! the drift shape #479 and #520 each cost a review round; and shared code is
//! compiled and tested on **both** hosts, while per-backend code is invisible
//! to CI (there is no macOS job at all) — the same reasoning that folded the
//! two backends' staging helpers into one `atomic_write` in #511.

/// Parse an `EnvironmentFile`-style buffer into ordered `(KEY, value)` pairs.
///
/// Pure (no I/O). Matches the subset of systemd's `EnvironmentFile=` grammar
/// the installer emits: one `KEY=value` per line, blank lines and `#` comments
/// skipped, surrounding whitespace on the key trimmed. Values are taken
/// verbatim after the first `=` (no shell expansion, no quote stripping) since
/// the installer writes plain values. Lines without `=` are ignored.
pub fn parse_env_file(contents: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                out.push((k.to_string(), v.to_string()));
            }
        }
    }
    out
}

/// Merge `from` into `into`, with `from` winning on key collision (matching
/// systemd's `EnvironmentFile=`-after-`Environment=` override order, and the
/// later-file-wins order between two `EnvironmentFile=` directives — both
/// measured on a live systemd user manager, not assumed). Existing keys keep
/// their position with the value replaced; new keys are appended.
pub fn merge_env(into: &mut Vec<(String, String)>, from: Vec<(String, String)>) {
    for (k, v) in from {
        if let Some(slot) = into.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            into.push((k, v));
        }
    }
}

#[cfg(test)]
mod tests;
