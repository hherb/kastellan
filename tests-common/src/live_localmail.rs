//! The live-localmail tier: the one test that checks our reading of localmail
//! against the real service (#763).
//!
//! `core/tests/mail_daemon_e2e.rs::mock_localmail_shapes_match_real_localmail`
//! is the only proof of several claims the mail worker is built on — the
//! minimum `api_minor`, the exact projected search hit, the paging fields,
//! the per-occurrence header list, the index route's bytes hashing to the
//! listed sha256. Every other mail test is written from the same reading of the
//! service, so a consistent misreading passes all of them (#527, #500).
//!
//! It needs a live localmail and a token, so it skips without them. Before
//! #763 that skip could not be turned into a failure, and nothing counted
//! whether the test ran at all: its evidence depended on someone remembering
//! `scripts/mail/live-shape-gate.sh`, and a run filtered down to nothing looked
//! the same as a pass. This module gives it the same contract as every other
//! gated tier:
//!
//! * [`KNOB`] turns a missing credential into a failure naming what is missing;
//! * [`RequireKnob::announce`] prints the `[E2E] live-localmail:` line that
//!   the `mail-live` profile in `scripts/run-e2e-gate.sh` counts.
//!
//! `scripts/mail/live-shape-gate.sh` reads the two credentials out of the
//! daemon's own config and then runs that profile, so the knob is set for you.
//! Only the Mac and the DGX have a localmail to point it at.

use std::io::Write;

use crate::require::{RequireKnob, UnmetAction};

/// Turns the live gate's skips into failures.
pub const KNOB: RequireKnob = RequireKnob::new("KASTELLAN_MAIL_LIVE_REQUIRE_E2E", "live-localmail");

/// The base URL of the live localmail, e.g. `https://10.0.0.3:8443`.
pub const ENDPOINT_ENV: &str = "KASTELLAN_MAIL_ENDPOINT";

/// A localmail api-user login token, sent as a bearer token.
pub const TOKEN_ENV: &str = "KASTELLAN_MAIL_TOKEN";

/// Where the live localmail is, and how to authenticate to it.
///
/// `Debug` is written by hand so the token cannot reach a panic message or a
/// log through a `{:?}`.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub endpoint: String,
    pub token: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("endpoint", &self.endpoint)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Pure: the credentials, or a reason naming every one that is missing.
///
/// A blank or whitespace-only value counts as missing. An exported-but-empty
/// variable is the usual shape of a config lookup that found nothing, and
/// treating it as a real endpoint would turn "not configured" into a confusing
/// `curl` failure against the empty string.
pub fn credentials_from(endpoint: Option<String>, token: Option<String>) -> Result<Credentials, String> {
    let present = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    match (present(endpoint), present(token)) {
        (Some(endpoint), Some(token)) => Ok(Credentials { endpoint, token }),
        (endpoint, token) => {
            let missing: Vec<&str> = [(endpoint.is_none(), ENDPOINT_ENV), (token.is_none(), TOKEN_ENV)]
                .into_iter()
                .filter_map(|(gone, name)| gone.then_some(name))
                .collect();
            Err(format!(
                "no live localmail: set {} (scripts/mail/live-shape-gate.sh reads them from \
                 kastellan.env and the token file)",
                missing.join(" + ")
            ))
        }
    }
}

/// The live localmail's credentials from the environment, or `None` after a
/// `[SKIP]` line.
///
/// # Panics
///
/// When [`KNOB`] is set and a credential is missing, naming it.
pub fn credentials_or_skip() -> Option<Credentials> {
    let action = KNOB.action();
    decide(
        action,
        credentials_from(std::env::var(ENDPOINT_ENV).ok(), std::env::var(TOKEN_ENV).ok()),
        &mut std::io::stderr(),
    )
}

/// [`credentials_or_skip`] after the environment is read, writing its marker
/// line to `out` — so a test can prove each arm emits the right line without
/// inflating a real run's `[SKIP]` or `[E2E]` count.
///
/// The `[E2E]` detail names the endpoint and never the token.
pub fn decide(
    action: UnmetAction,
    found: Result<Credentials, String>,
    out: &mut dyn Write,
) -> Option<Credentials> {
    match found {
        Err(reason) => KNOB.report_unmet_to(action, &reason, out),
        Ok(credentials) => {
            KNOB.announce_to(action, &format!("credentials for {}", credentials.endpoint), out);
            Some(credentials)
        }
    }
}

#[cfg(test)]
mod tests;
