//! Unit tests for [`super`]: the pure credential check, and each arm of
//! [`super::decide`] read back from a buffer.

use super::*;

const URL: &str = "https://10.0.0.3:8443";
const TOKEN: &str = "s3cret-token-value";

fn some(s: &str) -> Option<String> {
    Some(s.to_string())
}

fn written(f: impl FnOnce(&mut Vec<u8>)) -> String {
    let mut out = Vec::new();
    f(&mut out);
    String::from_utf8(out).unwrap()
}

#[test]
fn both_credentials_present_are_returned_trimmed() {
    let c = credentials_from(some(" https://10.0.0.3:8443\n"), some("tok\n")).unwrap();
    assert_eq!(c, Credentials { endpoint: URL.into(), token: "tok".into() });
}

/// The reason names exactly the variables that are missing, so the operator
/// knows which one to set.
#[test]
fn the_reason_names_each_missing_credential_and_only_those() {
    let e = credentials_from(None, some(TOKEN)).unwrap_err();
    assert!(e.contains(ENDPOINT_ENV) && !e.contains(TOKEN_ENV), "{e}");
    let e = credentials_from(some(URL), None).unwrap_err();
    assert!(e.contains(TOKEN_ENV) && !e.contains(ENDPOINT_ENV), "{e}");
    let e = credentials_from(None, None).unwrap_err();
    assert!(e.contains(ENDPOINT_ENV) && e.contains(TOKEN_ENV), "{e}");
}

/// An exported-but-empty variable is what a config lookup that found nothing
/// leaves behind; it is missing, not an endpoint called "".
#[test]
fn a_blank_credential_is_missing() {
    for blank in ["", "   ", "\n"] {
        assert!(credentials_from(some(blank), some(TOKEN)).is_err(), "{blank:?} endpoint");
        assert!(credentials_from(some(URL), some(blank)).is_err(), "{blank:?} token");
    }
}

#[test]
fn debug_never_prints_the_token() {
    let c = Credentials { endpoint: URL.into(), token: TOKEN.into() };
    let shown = format!("{c:?}");
    assert!(!shown.contains(TOKEN), "{shown}");
    assert!(shown.contains(URL), "{shown}");
}

#[test]
fn missing_credentials_skip_by_default_with_one_skip_line() {
    let got = written(|out| {
        assert!(decide(UnmetAction::Skip, credentials_from(None, None), out).is_none());
    });
    assert!(got.starts_with("\n[SKIP] no live localmail"), "{got:?}");
    assert!(!got.contains("[E2E]"), "{got:?}");
}

#[test]
#[should_panic(expected = "KASTELLAN_MAIL_LIVE_REQUIRE_E2E demanded a real live-localmail end-to-end run")]
fn missing_credentials_fail_when_demanded() {
    let _ = decide(UnmetAction::Fail, credentials_from(some(URL), None), &mut Vec::new());
}

/// The positive control the `mail-live` gate profile counts: one `[E2E]` line,
/// naming the endpoint and never the token.
#[test]
fn a_demanded_run_with_credentials_announces_the_endpoint_not_the_token() {
    let got = written(|out| {
        let c = decide(UnmetAction::Fail, credentials_from(some(URL), some(TOKEN)), out);
        assert_eq!(c.map(|c| c.endpoint), Some(URL.to_string()));
    });
    assert_eq!(got, format!("\n[E2E] live-localmail: credentials for {URL}\n"));
    assert!(!got.contains(TOKEN));
}

/// A plain `cargo test` with the credentials exported stays quiet: `[E2E]` is
/// evidence of a DEMANDED run only.
#[test]
fn an_undemanded_run_with_credentials_prints_nothing() {
    let got = written(|out| {
        assert!(decide(UnmetAction::Skip, credentials_from(some(URL), some(TOKEN)), out).is_some());
    });
    assert!(got.is_empty(), "{got:?}");
}
