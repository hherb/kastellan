//! The planner-summary sink screen, what it records about a block, and the
//! stripping of a screen placeholder's audit-only fields.
//!
//! Moved out of `summary.rs` (over the 500-LOC cap) as #699/#700 grew it.

use std::borrow::Cow;

use sha2::{Digest, Sha256};

use crate::cassandra::injection_guard::{screen_with_profile, GuardProfile, InjectionDecision};
use crate::tool_host::{AUDIT_ONLY_KEYS, INJECTION_BLOCKED_KEY};

/// Most characters of the plan-authored `tool` and `method` a sink audit row
/// carries; a longer value is cut and marked with `…`.
pub(super) const AUDIT_LABEL_MAX_CHARS: usize = 64;

/// The `tier` of the `policy / injection.blocked` row written for a block the
/// planner-summary sink screen made, beside `tool_host::post_process`'s
/// `catalogue` and `guard_model` on the same event name.
pub(crate) const TIER_SINK: &str = "sink";

/// What the sink screen recorded about a string it blocked — a plan's
/// `decision`, a step's call, or a step outcome (#699, #700): enough for the
/// forensic `policy / injection.blocked` row, and never the screened text.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SinkBlock {
    pub(super) score: f32,
    pub(super) reason_codes: Vec<&'static str>,
    pub(super) body_sha256: String,
    pub(super) body_byte_len: usize,
}

/// `s` cut to [`AUDIT_LABEL_MAX_CHARS`] characters plus `…` when longer.
pub(super) fn clamp_audit_label(s: &str) -> String {
    if s.chars().count() <= AUDIT_LABEL_MAX_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(AUDIT_LABEL_MAX_CHARS).collect();
    format!("{head}…")
}

/// Screen `text` with `tool`'s own guard profile; `Some` if it must be
/// withheld, carrying what the forensic row needs. The **single, mandatory sink
/// screen** for step outcomes: every worker-influenced string [`super`] places
/// into the planner prompt passes through here, so the
/// "nothing-unscreened-reaches-the-planner" invariant is *enforced* at one
/// point rather than *relied upon* across the source chokepoints (`tool_host`,
/// `tool_dispatch::fetch_screen`). The planner's own `decision` and step calls
/// pass through [`sink_screen_with`] under a fixed profile (#699, #700; see
/// `summary::call`).
///
/// Not a pure re-run of the source screen: since #677 the text includes object
/// keys, which the source never sees, so the sink can block what the source
/// allowed, and [`super::PlanRecord::sink_block_audit_payloads`] records it. It uses
/// the same per-tool profile, so it cannot over-block a Relaxed-profile
/// doc-fetch worker on quoted chat templates (issue #142).
pub(super) fn sink_screen(tool: &str, text: &str) -> Option<SinkBlock> {
    sink_screen_with(GuardProfile::for_tool(tool), text)
}

/// [`sink_screen`] under an explicit `profile`, for text no tool produced.
pub(super) fn sink_screen_with(profile: GuardProfile, text: &str) -> Option<SinkBlock> {
    let verdict = screen_with_profile(text, profile);
    (verdict.decision == InjectionDecision::Block).then(|| SinkBlock {
        score: verdict.score,
        reason_codes: verdict.reason_codes,
        body_sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
        body_byte_len: text.len(),
    })
}

/// `v` without the audit-only fields of an injection-screen placeholder.
///
/// Both upstream screens (`tool_host`'s placeholder and
/// `tool_dispatch::fetch_screen`) replace a blocked result with an object
/// carrying `injection_blocked: true`, a note, the screen's score and its
/// reason codes, and on the fetch path the continuation fields. The
/// [`AUDIT_ONLY_KEYS`] are kept there for the audit log. The planner gets none
/// of them: they would tell a compromised planner which defence fired. Any
/// other value passes through untouched, borrowed rather than cloned.
pub(super) fn without_screen_audit_fields(v: &serde_json::Value) -> Cow<'_, serde_json::Value> {
    match v {
        serde_json::Value::Object(map) if map.get(INJECTION_BLOCKED_KEY).and_then(serde_json::Value::as_bool) == Some(true) => {
            let mut map = map.clone();
            for key in AUDIT_ONLY_KEYS {
                map.remove(key);
            }
            Cow::Owned(serde_json::Value::Object(map))
        }
        _ => Cow::Borrowed(v),
    }
}
