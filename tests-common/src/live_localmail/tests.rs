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
    assert_eq!((c.endpoint(), c.token()), (URL, "tok"));
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
    let c = credentials_from(some(URL), some(TOKEN)).unwrap();
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
        assert_eq!(c.as_ref().map(Credentials::endpoint), Some(URL));
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

// --- #768: the wiring between the environment and `decide` ---

/// A fake environment: the variables it holds, and every name it was asked for.
struct FakeEnv {
    vars: Vec<(&'static str, Result<String, std::env::VarError>)>,
    asked: std::cell::RefCell<Vec<String>>,
}

impl FakeEnv {
    fn new(vars: Vec<(&'static str, Result<String, std::env::VarError>)>) -> Self {
        Self { vars, asked: std::cell::RefCell::new(Vec::new()) }
    }

    fn var(&self, name: &str) -> Result<String, std::env::VarError> {
        self.asked.borrow_mut().push(name.to_string());
        self.vars
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.clone())
            .unwrap_or(Err(std::env::VarError::NotPresent))
    }
}

/// `credentials_or_skip` reads exactly the two documented variables — the
/// names `scripts/mail/live-shape-gate.sh` exports — and hands what it found
/// to `decide`. Before #768 only the `mail-live` profile's run-time floors
/// could notice a misread name.
#[test]
fn the_environment_is_read_by_the_two_documented_names() {
    let env = FakeEnv::new(vec![(ENDPOINT_ENV, Ok(URL.into())), (TOKEN_ENV, Ok(TOKEN.into()))]);
    let got = credentials_from_env(UnmetAction::Skip, |n| env.var(n), &mut Vec::new());
    assert_eq!(got.as_ref().map(|c| (c.endpoint(), c.token())), Some((URL, TOKEN)));
    let mut asked = env.asked.into_inner();
    asked.sort();
    assert_eq!(asked, [ENDPOINT_ENV, TOKEN_ENV]);
}

/// The action is passed through, not re-derived: a demanded run with a
/// missing variable fails, naming it.
#[test]
#[should_panic(expected = "KASTELLAN_MAIL_TOKEN")]
fn a_demanded_run_with_a_missing_variable_fails_naming_it() {
    let env = FakeEnv::new(vec![(ENDPOINT_ENV, Ok(URL.into()))]);
    let _ = credentials_from_env(UnmetAction::Fail, |n| env.var(n), &mut Vec::new());
}

/// A set-but-not-UTF-8 variable is named as that, not as unset — which would
/// send the operator to set a variable that is already set.
#[test]
fn a_non_utf8_variable_is_named_as_set_but_unreadable() {
    let env = FakeEnv::new(vec![
        (ENDPOINT_ENV, Err(std::env::VarError::NotUnicode(std::ffi::OsString::from("x")))),
        (TOKEN_ENV, Ok(TOKEN.into())),
    ]);
    let got = written(|out| {
        assert!(credentials_from_env(UnmetAction::Skip, |n| env.var(n), out).is_none());
    });
    assert!(got.contains(&format!("{ENDPOINT_ENV} is set but is not UTF-8")), "{got:?}");
}
