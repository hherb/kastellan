//! Trusted current-date/time (`<now>`) block for the planner system prompt.
//!
//! The planner is otherwise date-blind: for any date-relative question
//! ("yesterday", "latest") it web-searches to *guess* the date and loops to the
//! plan cap. This module supplies an authoritative, system-generated timestamp
//! it can trust. Pure renderer + pure timezone resolver; the instant is
//! captured by the caller so the render is deterministic and testable.

use jiff::tz::TimeZone;
use jiff::Zoned;

/// Render the trusted `<now>` grounding block. Pure — the caller supplies the
/// instant. Minute resolution (no seconds) keeps the assembled system prompt —
/// and its `system_prompt_sha256` — stable within a plan iteration so the local
/// model's KV-cache prefix is not churned each second. Verbatim, NOT escaped:
/// system-generated, not adversary-influenced.
pub(crate) fn render_now_block(now: &Zoned) -> String {
    // %A weekday, %-d no-pad day, %B month, %Y year, %H:%M 24h minute,
    // %Z tz abbreviation (e.g. AEST), %:z offset with colon (e.g. +10:00).
    let stamp = now.strftime("%A, %-d %B %Y, %H:%M (%Z, UTC%:z)").to_string();
    format!("<now>\nCurrent date and time: {stamp}.\n</now>\n")
}

/// Capture the current instant in `tz` and render the `<now>` block. The one
/// impure hop (`Timestamp::now()`); the formatting it delegates to is the pure,
/// tested [`render_now_block`]. Called per plan formulation by the builder so
/// the block is always current.
pub(crate) fn current_now_block(tz: &TimeZone) -> String {
    let now = jiff::Timestamp::now().to_zoned(tz.clone());
    render_now_block(&now)
}

/// Where the planner's timezone came from — logged once at startup so a
/// misconfigured `KASTELLAN_TIMEZONE` is visible rather than silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TzSource {
    /// A valid `KASTELLAN_TIMEZONE` IANA name.
    Configured,
    /// Unset/blank → the host system timezone.
    System,
    /// A set-but-unresolvable name → UTC (fail-safe: the block still renders).
    UtcFallback,
}

/// Resolve the operator's configured timezone. `configured` is the
/// `KASTELLAN_TIMEZONE` value: an IANA name (e.g. "Australia/Sydney"); unset or
/// blank → the host system zone; a set-but-invalid name → UTC. DST is handled
/// automatically by `jiff` at render time.
pub fn resolve_timezone(configured: Option<&str>) -> (TimeZone, TzSource) {
    match configured.map(str::trim) {
        Some(name) if !name.is_empty() => match TimeZone::get(name) {
            Ok(tz) => (tz, TzSource::Configured),
            Err(_) => (TimeZone::UTC, TzSource::UtcFallback),
        },
        _ => (TimeZone::system(), TzSource::System),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    // NOTE: 2026-07-12 is a SUNDAY (verified). Australia/Sydney is AEST=UTC+10
    // in July (southern-hemisphere winter → no DST). Named-zone construction
    // relies on the system tz DB, present on the dev Mac, the DGX, and CI Linux.

    #[test]
    fn renders_weekday_date_minute_and_offset() {
        let z = date(2026, 7, 12)
            .at(14, 5, 0, 0)
            .in_tz("Australia/Sydney")
            .expect("valid Sydney datetime");
        assert_eq!(
            render_now_block(&z),
            "<now>\nCurrent date and time: Sunday, 12 July 2026, 14:05 (AEST, UTC+10:00).\n</now>\n"
        );
    }

    #[test]
    fn utc_instant_renders_utc_label() {
        let z = date(2026, 7, 12).at(4, 5, 0, 0).in_tz("UTC").expect("utc");
        assert_eq!(
            render_now_block(&z),
            "<now>\nCurrent date and time: Sunday, 12 July 2026, 04:05 (UTC, UTC+00:00).\n</now>\n"
        );
    }

    #[test]
    fn seconds_are_not_rendered() {
        let with_secs = date(2026, 7, 12).at(14, 5, 59, 0).in_tz("Australia/Sydney").unwrap();
        let block = render_now_block(&with_secs);
        assert!(block.contains("14:05"), "minute resolution only");
        assert!(!block.contains(":59"), "seconds must not appear");
    }

    #[test]
    fn configured_iana_name_resolves() {
        let (_tz, src) = resolve_timezone(Some("Australia/Sydney"));
        assert_eq!(src, TzSource::Configured);
    }

    #[test]
    fn unset_uses_system_zone() {
        let (_tz, src) = resolve_timezone(None);
        assert_eq!(src, TzSource::System);
    }

    #[test]
    fn blank_is_treated_as_unset() {
        let (_tz, src) = resolve_timezone(Some("   "));
        assert_eq!(src, TzSource::System);
    }

    #[test]
    fn invalid_name_falls_back_to_utc() {
        let (tz, src) = resolve_timezone(Some("Not/AZone"));
        assert_eq!(src, TzSource::UtcFallback);
        // The UTC fallback must still render a valid block, not panic.
        let z = date(2026, 1, 1).at(0, 0, 0, 0).to_zoned(tz).unwrap();
        assert!(render_now_block(&z).contains("UTC+00:00"));
    }
}
