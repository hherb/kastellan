//! Unit tests for `bootstrap.rs`'s pure parse helpers
//! (`parse_bootstrap_secrets_csv` and the debug-only `parse_test_vault_seed`)
//! and for the two-step shutdown-signal contract (`install_shutdown_signals`
//! then `ShutdownSignals::wait`, #810).
//! Colocated with the code under test after the `main.rs` prod-split (Item 9b);
//! `super::` resolves to the `bootstrap` module.

use super::parse_bootstrap_secrets_csv;

#[test]
fn parse_empty_string_yields_empty_list() {
    assert!(parse_bootstrap_secrets_csv("").is_empty());
}

#[test]
fn parse_only_whitespace_yields_empty_list() {
    assert!(parse_bootstrap_secrets_csv("   ").is_empty());
    assert!(parse_bootstrap_secrets_csv(" \t \n ").is_empty());
}

#[test]
fn parse_single_name_works() {
    let names = parse_bootstrap_secrets_csv("openai-api-key");
    assert_eq!(names, vec!["openai-api-key"]);
}

#[test]
fn parse_handles_trailing_comma() {
    let names = parse_bootstrap_secrets_csv("a,b,");
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn parse_handles_leading_comma_and_whitespace() {
    let names = parse_bootstrap_secrets_csv(", , a , b ,, c , ");
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn parse_preserves_internal_dashes_and_dots() {
    let names = parse_bootstrap_secrets_csv("openai.api.key, github-token");
    assert_eq!(names, vec!["openai.api.key", "github-token"]);
}

#[cfg(debug_assertions)]
#[test]
fn parse_test_vault_seed_splits_on_first_equals() {
    use super::parse_test_vault_seed;
    assert_eq!(
        parse_test_vault_seed("deadbeef=SCRUBME-value"),
        Some(("deadbeef", "SCRUBME-value")),
    );
}

#[cfg(debug_assertions)]
#[test]
fn parse_test_vault_seed_keeps_equals_in_plaintext() {
    // A real secret may contain `=` (e.g. base64 padding) — only the FIRST
    // `=` separates the ref tail from the plaintext; the rest is verbatim.
    use super::parse_test_vault_seed;
    assert_eq!(
        parse_test_vault_seed("aabbccdd=a=b==c"),
        Some(("aabbccdd", "a=b==c")),
    );
}

#[cfg(debug_assertions)]
#[test]
fn parse_test_vault_seed_none_without_separator() {
    use super::parse_test_vault_seed;
    assert_eq!(parse_test_vault_seed("noseparator"), None);
    assert_eq!(parse_test_vault_seed(""), None);
}

#[cfg(debug_assertions)]
#[test]
fn parse_test_vault_seed_does_not_trim() {
    // The plaintext is a secret — trimming could corrupt it; the value is
    // taken byte-for-byte after the first `=`. (The ref tail's own format
    // is validated downstream by `Vault::seed_known_ref_for_test`.)
    use super::parse_test_vault_seed;
    assert_eq!(
        parse_test_vault_seed("deadbeef= padded "),
        Some(("deadbeef", " padded ")),
    );
}

// ── Shutdown signals (#810) ─────────────────────────────────────────────────
//
// `main` registers SIGTERM/SIGINT with `install_shutdown_signals` BEFORE it
// starts the scheduler and the channels, and only awaits them (`wait`) after.
// These tests pin the two halves of that contract:
//
//   * a signal that arrives between the install and the wait is not lost —
//     the registered stream holds it, so `wait` returns at once;
//   * without a signal, `wait` does not return.
//
// They raise real signals at this test process. That is safe only because
// `install_shutdown_signals` has already replaced the default disposition
// (terminate) with tokio's handler, which stays installed for the life of
// the process — so each test installs FIRST, and raises only after the
// install succeeded. A process-wide lock serialises them, because a signal
// raised by one test would otherwise wake another test's "no signal" wait.

use super::install_shutdown_signals;
use std::time::Duration;

/// Serialises the signal tests (see the section comment above). A tokio
/// mutex, so the guard may be held across `.await` without tripping clippy's
/// `await_holding_lock`; it is runtime-agnostic, so each `#[tokio::test]`'s
/// own runtime can use it.
static SIGNAL_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Long enough that a `wait` which is going to return has returned; short
/// enough to keep the suite fast.
const WAIT_BOUND: Duration = Duration::from_secs(5);

/// Sends `sig` to this process. `raise` delivers it on the calling thread;
/// the handler is process-wide (tokio's, once installed) and records it for
/// every registered stream.
fn raise(sig: libc::c_int) {
    // SAFETY: `raise` has no memory-safety preconditions; the only hazard is
    // the signal's default action, which the caller has replaced first.
    let rc = unsafe { libc::raise(sig) };
    assert_eq!(rc, 0, "raise({sig}) failed");
}

#[tokio::test]
async fn sigterm_raised_before_the_wait_is_not_lost() {
    let _serial = SIGNAL_TESTS.lock().await;
    let signals = install_shutdown_signals().expect("install shutdown signals");
    // Delivered in the window main uses for starting the scheduler and the
    // channels — after the install, before anyone awaits.
    raise(libc::SIGTERM);
    tokio::time::timeout(WAIT_BOUND, signals.wait())
        .await
        .expect("a SIGTERM delivered after install must end the wait");
}

#[tokio::test]
async fn sigint_raised_before_the_wait_is_not_lost() {
    let _serial = SIGNAL_TESTS.lock().await;
    let signals = install_shutdown_signals().expect("install shutdown signals");
    raise(libc::SIGINT);
    tokio::time::timeout(WAIT_BOUND, signals.wait())
        .await
        .expect("a SIGINT delivered after install must end the wait");
}

#[tokio::test]
async fn wait_does_not_return_without_a_signal() {
    let _serial = SIGNAL_TESTS.lock().await;
    let signals = install_shutdown_signals().expect("install shutdown signals");
    let outcome = tokio::time::timeout(Duration::from_millis(200), signals.wait()).await;
    assert!(
        outcome.is_err(),
        "wait returned with no SIGTERM or SIGINT delivered"
    );
}
