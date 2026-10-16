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
//     the streams registered at install hold it, so `wait` returns at once;
//   * without a signal, `wait` does not return.
//
// They raise real signals at this test process. That is safe only because
// `install_shutdown_signals` has already replaced the default disposition
// (terminate) with tokio's handler, which stays installed for the life of
// the process — so each test installs FIRST, and raises only after the
// install succeeded. A process-wide lock serialises them, because a signal
// raised by one test would otherwise wake another test's "no signal" wait.
// (A stream registered after a signal was forwarded never sees it, so
// serialising is enough: no test inherits an earlier test's signal.)
//
// A third test pins the other half of #810, which no runtime test can reach:
// WHERE `main` calls the install. It reads `main.rs` itself.

use super::install_shutdown_signals;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};

/// Serialises the signal tests (see the section comment above). A tokio
/// mutex, so the guard may be held across `.await` without tripping clippy's
/// `await_holding_lock`; it is runtime-agnostic, so each `#[tokio::test]`'s
/// own runtime can use it; and it does not poison, so one failing signal test
/// does not fail the others with it.
static SIGNAL_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Long enough that a `wait` which is going to return has returned; short
/// enough to keep the suite fast.
const WAIT_BOUND: Duration = Duration::from_secs(5);

/// Sends `sig` to the calling thread. It is not blocked, so tokio's
/// process-wide handler (once installed) has recorded it before `raise`
/// returns; the runtime forwards it to the registered streams later, on its
/// next driver turn.
fn raise(sig: libc::c_int) {
    // SAFETY: `raise` has no memory-safety preconditions; the only hazard is
    // the signal's default action, which the caller has replaced first.
    let rc = unsafe { libc::raise(sig) };
    assert_eq!(rc, 0, "raise({sig}) failed");
}

/// Installs, raises `sig`, and asserts the wait still ends.
///
/// The probe stream is what makes this a test of "held from the install": it
/// is awaited BEFORE `wait` is ever polled, so by then the runtime has
/// already forwarded the signal — as `main`'s multi-thread runtime does while
/// the channels are still starting. Without it, this current-thread runtime
/// forwards the signal only once `wait` is first polled, and a `wait` that
/// registered its own streams (dropping the installed ones) would pass too;
/// it survived 10 of 40 runs in the review of PR #856. With the probe, such a
/// `wait` subscribes after the forwarding and hangs into the timeout.
async fn assert_held_from_the_install(kind: SignalKind, sig: libc::c_int, name: &str) {
    let _serial = SIGNAL_TESTS.lock().await;
    let signals = install_shutdown_signals().expect("install shutdown signals");
    let mut forwarded = signal(kind).expect("register the probe stream");
    // Delivered in the window main uses for starting the scheduler and the
    // channels — after the install, before anyone awaits.
    raise(sig);
    tokio::time::timeout(WAIT_BOUND, forwarded.recv())
        .await
        .unwrap_or_else(|_| panic!("tokio never forwarded the raised {name}"));
    tokio::time::timeout(WAIT_BOUND, signals.wait())
        .await
        .unwrap_or_else(|_| panic!("a {name} delivered after install must end the wait"));
}

#[tokio::test]
async fn sigterm_raised_before_the_wait_is_not_lost() {
    assert_held_from_the_install(SignalKind::terminate(), libc::SIGTERM, "SIGTERM").await;
}

#[tokio::test]
async fn sigint_raised_before_the_wait_is_not_lost() {
    assert_held_from_the_install(SignalKind::interrupt(), libc::SIGINT, "SIGINT").await;
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

/// The daemon entrypoint, read as text: the ordering #810 fixed lives in
/// `main`'s statement order, which no unit test can call.
const MAIN_RS: &str = include_str!("../main.rs");

/// `src` without its whole-line `//` comments, so prose that names a call or
/// a `?` cannot satisfy or trip the checks below.
fn code_only(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Byte offset of `needle` in `code`, which must contain it exactly once —
/// so a rename or a second call fails here, loudly, rather than letting the
/// ordering check compare the wrong occurrence.
fn sole_offset(code: &str, needle: &str) -> usize {
    let hits: Vec<usize> = code.match_indices(needle).map(|(at, _)| at).collect();
    assert_eq!(
        hits.len(),
        1,
        "expected `{needle}` exactly once in main.rs's code, found it {} times",
        hits.len()
    );
    hits[0]
}

#[test]
fn main_installs_the_signals_before_anything_audits() {
    let code = code_only(MAIN_RS);
    let install_call = "bootstrap::install_shutdown_signals()?";
    let install = sole_offset(&code, install_call);
    for writer in [
        "spawn_scheduler(",
        "supervise_matrix_channel(",
        "supervise_email_channel(",
    ] {
        assert!(
            install < sole_offset(&code, writer),
            "main must register the stop signals before `{writer}` (#810)"
        );
    }
    let wait = sole_offset(&code, "shutdown_signals.wait().await");
    let drain = sole_offset(&code, "audit_sink::drain()");
    assert!(
        install < wait && wait < drain,
        "main must install, then wait, then drain"
    );
    // From the install to the drain, nothing may leave `main` early.
    let window = &code[install + install_call.len()..drain];
    for exit in ["?", ".unwrap()", ".expect(", "return", "panic!", "unreachable!", "todo!"] {
        assert!(
            !window.contains(exit),
            "`{exit}` between the signal install and the audit drain in main.rs \
             could skip the drain (#810)"
        );
    }
}
