//! The sift's two operator knobs (#922, K3c), read once at boot.

use super::params::read_key;

/// One sift's reads by default: the per-plan cap (security audit F5), so a
/// sift is one reviewed plan no bigger than any other.
pub const DEFAULT_MAX_READS: usize = crate::scheduler::inner_loop::MAX_STEPS_PER_PLAN;

/// The methods a sift may read with by default (K3a): read-only, and each
/// reads one item.
pub const DEFAULT_READ_METHODS: [&str; 3] = ["mail.get_message", "mail.get_attachment_text", "handoff.fetch"];

/// The two knobs, as the daemon runs with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiftConfig {
    pub max_reads: usize,
    pub read_methods: Vec<String>,
}

impl Default for SiftConfig {
    fn default() -> Self {
        Self { max_reads: DEFAULT_MAX_READS, read_methods: DEFAULT_READ_METHODS.map(String::from).to_vec() }
    }
}

/// A read no knob may allow: another sift (a sift inside a sift multiplies
/// the reads past every bound) or a skill (it expands into steps the read
/// template never named).
pub(crate) fn never_a_read(key: &str) -> bool {
    key == super::SIFT_TOOL || key.starts_with("sift.") || key.contains("invoke_skill")
}

/// The config from `KASTELLAN_SIFT_MAX_READS` and `KASTELLAN_SIFT_READ_METHODS`
/// (each `None` when unset), checked against the advertised `(tool, method)`
/// pairs. `handoff.fetch` is a built-in, always known. Any bad value is an
/// error naming its key: the daemon refuses to start rather than guess.
pub fn sift_config(
    max_reads: Option<&str>,
    methods: Option<&str>,
    advertised: &[(String, String)],
) -> Result<SiftConfig, String> {
    let max_reads = match max_reads {
        None => DEFAULT_MAX_READS,
        Some(v) => match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => return Err(format!("KASTELLAN_SIFT_MAX_READS={v:?} is not a whole number of reads above 0")),
        },
    };
    let read_methods = match methods {
        None => SiftConfig::default().read_methods,
        Some(v) => {
            let listed: Vec<String> = v.split(',').map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect();
            if listed.is_empty() {
                return Err(format!("KASTELLAN_SIFT_READ_METHODS={v:?} names no method"));
            }
            for m in &listed {
                if never_a_read(m) {
                    return Err(format!("KASTELLAN_SIFT_READ_METHODS: `{m}` can never be a sift's read (a sift or a skill)"));
                }
                let known = m == "handoff.fetch" || advertised.iter().any(|(t, am)| read_key(t, am) == *m);
                if !known {
                    return Err(format!("KASTELLAN_SIFT_READ_METHODS: `{m}` is not an advertised method"));
                }
            }
            listed
        }
    };
    Ok(SiftConfig { max_reads, read_methods })
}

#[cfg(test)]
mod tests;
