//! Operator approval gate for crystallised L3 skills (the security
//! control that precedes any invocation path).
//!
//! Crystallised skills land `trust:"untrusted"` and non-executable (see
//! [`crate::memory::l3_crystallise`]). This module adds the typed
//! [`SkillTrust`] read boundary and the pure [`evaluate_approval`] gate
//! an operator runs (via `hhagent-cli memory l3 approve`) before a skill
//! is promoted to `user_approved`. **Nothing here executes a skill** —
//! `UserApproved`/`Pinned` are inert until the invocation slice lands.
//!
//! See `docs/superpowers/specs/2026-05-31-l3-skill-approval-gate-design.md`.

/// Trust level of a crystallised L3 skill, stored as the metadata
/// `trust` string. Forward-compat: `Pinned` is defined but no command
/// produces it in the gate slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillTrust {
    Untrusted,
    UserApproved,
    Pinned,
}

impl SkillTrust {
    /// Metadata-string form. Single source of truth for the literals
    /// written to / read from `metadata->>'trust'`.
    pub fn as_str(self) -> &'static str {
        match self {
            SkillTrust::Untrusted => "untrusted",
            SkillTrust::UserApproved => "user_approved",
            SkillTrust::Pinned => "pinned",
        }
    }

    /// TOTAL, fail-safe parse from a metadata string: any unknown or
    /// absent value maps to [`SkillTrust::Untrusted`]. An unrecognised
    /// trust marker must never read as trusted.
    pub fn from_metadata_str(s: &str) -> SkillTrust {
        match s {
            "user_approved" => SkillTrust::UserApproved,
            "pinned" => SkillTrust::Pinned,
            _ => SkillTrust::Untrusted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skilltrust_roundtrips_every_variant() {
        for t in [SkillTrust::Untrusted, SkillTrust::UserApproved, SkillTrust::Pinned] {
            assert_eq!(SkillTrust::from_metadata_str(t.as_str()), t);
        }
    }

    #[test]
    fn skilltrust_unknown_or_empty_is_untrusted() {
        assert_eq!(SkillTrust::from_metadata_str("bogus"), SkillTrust::Untrusted);
        assert_eq!(SkillTrust::from_metadata_str(""), SkillTrust::Untrusted);
        assert_eq!(SkillTrust::from_metadata_str("USER_APPROVED"), SkillTrust::Untrusted);
    }
}
