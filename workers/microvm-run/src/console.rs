//! The guest console: where it goes, and what the launcher says about it when
//! the boot fails (#666).
//!
//! # Why this module exists
//!
//! The guest kernel boots with `console=ttyS0`, and firecracker presents that
//! serial console as **its own stdout**. Every `kastellan-microvm-init`
//! diagnostic — "worker privileges dropped", "chown … failed", "relay UDS bind
//! failed … channel disabled" — and the worker's own stderr ride it. The
//! launcher used to spawn firecracker with `Stdio::null()` on both streams, so
//! all of it was discarded, and `--log-path` did not cover it: that flag
//! catches firecracker's *own* logs, and catches nothing at all when
//! firecracker fails before opening the file.
//!
//! The cost was measured. Three independent production defects (a refused VMM
//! jail, a Landlock rule the guest kernel cannot enforce, and root-owned relay
//! sockets) each surfaced as the identical, contentless `Protocol(EarlyExit)`
//! — "worker exited before responding" — and finding them took most of a
//! session, almost all of it spent getting the message out of the machine
//! rather than fixing anything.
//!
//! # Shape
//!
//! Everything here is a **pure function**: the syscalls live in `main`. That is
//! what lets the redaction, the tail budget and the failure report be unit
//! tested on any platform, including a Mac with no KVM.
//!
//! # What is safe to echo
//!
//! The console file itself lives inside the per-spawn run dir, which is already
//! owner-private 0700 (audit H3) — the same dir that holds `fc.json`, which
//! carries the boot args verbatim. So writing the console there exposes nothing
//! that dir did not already hold.
//!
//! **Echoing a tail of it to the launcher's own stderr is a different
//! question**, because that stream is piped by the sandbox backend and drained
//! into the daemon log. Two kinds of content are on the console:
//!
//! - The **kernel command line**, which the kernel prints at every boot and
//!   which carries `kastellan.env=<hex>` — the worker's whole environment,
//!   secrets included. That is a guaranteed leak on every single boot, so
//!   [`elide_env_cmdline_value`] removes it before anything is echoed.
//! - The guest init's diagnostics and **the worker's own stderr** (the init
//!   redirects only fd 0 and 1 onto the vsock). This is the same content that,
//!   for a bwrap worker, the backend already pipes and `worker_stderr` already
//!   logs — so echoing it makes the VM path match the non-VM path rather than
//!   opening a new sink.

use std::path::{Path, PathBuf};

/// Filename, inside the per-spawn run dir, that receives the guest console.
/// Sits beside `fc.json` and `fc.log`, and is removed with them on a graceful
/// exit unless [`KEEP_RUN_DIR_ENV`] is set.
pub const CONSOLE_LOG_FILE: &str = "console.log";

/// Cmdline token whose value is the hex-encoded worker environment. Kept in
/// sync by hand with `kastellan-sandbox`'s `ENV_CMDLINE_KEY` and
/// `kastellan-microvm-init`'s `ENV_CMDLINE_KEY` — the launcher depends on
/// neither crate, the same manual contract the vsock ports already use.
const ENV_CMDLINE_KEY: &str = "kastellan.env";

/// How many trailing console lines the boot-failure report carries.
pub const TAIL_LINES: usize = 40;

/// Byte ceiling on that report's console excerpt, applied after the line
/// budget. A guest that writes one enormous line must not be able to push an
/// unbounded string through the launcher's stderr pipe.
pub const TAIL_BYTES: usize = 8 * 1024;

/// Where the guest console should be written, given the launcher's `--run-dir`.
///
/// `None` — a legacy caller that passes no run dir — keeps the historical
/// behaviour (the console is discarded), because there is no owner-private
/// directory to put it in and inventing one would be a surprise.
pub fn console_log_path(run_dir: Option<&str>) -> Option<PathBuf> {
    run_dir.map(|d| Path::new(d).join(CONSOLE_LOG_FILE))
}

/// Replace the value of every `kastellan.env=` token with a placeholder.
///
/// The kernel prints its whole command line at boot, and that line carries the
/// worker's entire environment hex-encoded — including any secret the host
/// redeemed for it. Redacting the **value** while keeping the **key** is
/// deliberate: an operator reading the excerpt still sees that the env was
/// passed (its absence is itself a defect signature), without the contents.
///
/// Tokens are whitespace-separated, which is exactly how the kernel renders a
/// command line and how `kastellan-microvm-init` parses it back.
///
/// ⚠️ **The name deliberately avoids the word "secret", and putting it back
/// turns CI red.** CodeQL's `rust/cleartext-logging` rule decides what is
/// sensitive from *identifier names*, so a sanitiser called
/// `scrub_cmdline_..._secrets` makes its own return value look like a credential
/// and every `eprint!` of the result becomes a high-severity alert — four of
/// them, on the one function whose entire job is removing the secret. The
/// 2026-09-02 audit hit the same rule for interpolating a numeric `uid`. The
/// guarantee lives in this doc comment and in the tests, not in the name.
pub fn elide_env_cmdline_value(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Newlines are whitespace, so one pass over whitespace-separated tokens
    // covers the whole text — no separate line loop, and the original spacing
    // (including line breaks) is preserved because `split_inclusive` hands each
    // token its own trailing separator.
    for tok in text.split_inclusive(char::is_whitespace) {
        let (word, sep) = split_trailing_ws(tok);
        if word.starts_with(ENV_CMDLINE_KEY)
            && word[ENV_CMDLINE_KEY.len()..].starts_with('=')
        {
            out.push_str(ENV_CMDLINE_KEY);
            out.push_str("=<redacted>");
            out.push_str(sep);
        } else {
            out.push_str(tok);
        }
    }
    out
}

/// Split a `split_inclusive`-produced token into its word and its trailing
/// whitespace separator, so a redaction can preserve the original spacing.
fn split_trailing_ws(tok: &str) -> (&str, &str) {
    let cut = tok
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_whitespace())
        .last()
        .map(|(i, _)| i)
        .unwrap_or(tok.len());
    tok.split_at(cut)
}

/// The last `max_lines` lines of `text`, then truncated to `max_bytes` from the
/// END (the tail is where a failure's cause is), on a char boundary.
///
/// Returns the empty string for empty input, so a caller can distinguish "the
/// console said nothing" from "the console said this" and report the former as
/// its own, differently-actionable fact.
pub fn tail_lines(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    let joined = lines[start..].join("\n");
    if joined.len() <= max_bytes {
        return joined;
    }
    // Keep the END. Walk forward to the next char boundary so the slice is valid.
    let mut cut = joined.len() - max_bytes;
    while cut < joined.len() && !joined.is_char_boundary(cut) {
        cut += 1;
    }
    joined[cut..].to_string()
}

/// Compose the launcher's boot-failure report.
///
/// `console` is the raw console text when it could be read; `None` means there
/// was nowhere to read it from (no `--run-dir`) or the read failed. The two are
/// reported differently on purpose — "no console was captured" points at the
/// launcher's invocation, "the console was empty" points at the guest never
/// producing a byte, and conflating them is how #666's blindness read as a code
/// defect for two days.
///
/// The report is what the launcher writes to its **own stderr**, which the
/// sandbox backend pipes and the core daemon drains, so it is the one channel
/// that reaches a host-side reader without anyone going to look at a file.
pub fn boot_failure_report(
    reason: &str,
    console_path: Option<&Path>,
    console: Option<&str>,
) -> String {
    let mut s = format!("kastellan-microvm-run: micro-VM boot failed: {reason}\n");
    match (console_path, console) {
        (Some(p), Some(text)) => {
            let tail = tail_lines(&elide_env_cmdline_value(text), TAIL_LINES, TAIL_BYTES);
            if tail.trim().is_empty() {
                s.push_str(&format!(
                    "kastellan-microvm-run: the guest console at {} is EMPTY — the guest \
                     produced no output at all, so suspect the VMM (a refused jail, a missing \
                     device, a rejected config) rather than the guest init.\n",
                    p.display()
                ));
            } else {
                s.push_str(&format!(
                    "kastellan-microvm-run: last {TAIL_LINES} lines of the guest console \
                     ({}), kastellan.env redacted:\n{tail}\n",
                    p.display()
                ));
            }
        }
        (Some(p), None) => s.push_str(&format!(
            "kastellan-microvm-run: the guest console at {} could not be read.\n",
            p.display()
        )),
        (None, _) => s.push_str(
            "kastellan-microvm-run: no guest console was captured (the launcher was given no \
             --run-dir), so there is nothing to report beyond the reason above.\n",
        ),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_path_is_inside_the_run_dir() {
        assert_eq!(
            console_log_path(Some("/run/kastellan-x")),
            Some(PathBuf::from("/run/kastellan-x/console.log"))
        );
    }

    #[test]
    fn console_path_is_none_without_a_run_dir() {
        // Legacy callers keep the historical discard rather than getting a file
        // in some invented location.
        assert_eq!(console_log_path(None), None);
    }

    #[test]
    fn scrub_removes_the_env_value_and_keeps_the_key() {
        let line = "Kernel command line: console=ttyS0 kastellan.env=6b6579 reboot=k";
        let got = elide_env_cmdline_value(line);
        assert!(
            !got.contains("6b6579"),
            "the hex-encoded worker env must not survive: {got}"
        );
        assert!(
            got.contains("kastellan.env=<redacted>"),
            "the KEY must survive so its absence stays a signal: {got}"
        );
        assert!(got.contains("console=ttyS0"), "other tokens intact: {got}");
        assert!(got.contains("reboot=k"), "trailing token intact: {got}");
    }

    #[test]
    fn scrub_leaves_the_non_secret_mount_manifest_alone() {
        // `kastellan.mounts=` is hex too but carries no secret, and an operator
        // reading a boot failure needs it. Prefix matching must not over-reach.
        let got = elide_env_cmdline_value("kastellan.mounts=7273 kastellan.envx=99");
        assert_eq!(got, "kastellan.mounts=7273 kastellan.envx=99");
    }

    #[test]
    fn scrub_handles_multiple_lines_and_repeats() {
        let got = elide_env_cmdline_value("a kastellan.env=aa b\nc kastellan.env=bb\n");
        assert_eq!(got, "a kastellan.env=<redacted> b\nc kastellan.env=<redacted>\n");
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        let text = (1..=100).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let got = tail_lines(&text, 3, 1024);
        assert_eq!(got, "98\n99\n100");
    }

    #[test]
    fn tail_of_short_input_is_the_whole_input() {
        assert_eq!(tail_lines("one\ntwo", 40, 1024), "one\ntwo");
        assert_eq!(tail_lines("", 40, 1024), "");
    }

    #[test]
    fn tail_byte_cap_keeps_the_end_on_a_char_boundary() {
        // One long line of multi-byte chars: the line budget cannot help, so the
        // byte cap must, and it must not slice a char in half.
        let text = "é".repeat(100); // 200 bytes
        let got = tail_lines(&text, 40, 51);
        assert!(got.len() <= 51, "byte cap not applied: {} bytes", got.len());
        assert!(text.ends_with(&got), "the END must be kept, not the start");
    }

    #[test]
    fn report_carries_a_scrubbed_console_tail() {
        let r = boot_failure_report(
            "guest vsock did not come up",
            Some(Path::new("/run/x/console.log")),
            Some("Kernel command line: kastellan.env=dead\nmicrovm-init: relay UDS bind failed\n"),
        );
        assert!(r.contains("guest vsock did not come up"), "{r}");
        assert!(r.contains("/run/x/console.log"), "must name the path: {r}");
        assert!(r.contains("relay UDS bind failed"), "must carry the tail: {r}");
        assert!(!r.contains("dead"), "must scrub the env value: {r}");
    }

    #[test]
    fn report_distinguishes_empty_console_from_absent_console() {
        let empty = boot_failure_report("x", Some(Path::new("/run/x/console.log")), Some("\n \n"));
        assert!(
            empty.contains("EMPTY"),
            "an empty console points at the VMM, and must say so: {empty}"
        );
        let absent = boot_failure_report("x", None, None);
        assert!(
            absent.contains("no guest console was captured"),
            "no --run-dir is a different fact from an empty console: {absent}"
        );
        assert!(!absent.contains("EMPTY"), "{absent}");
        let unreadable =
            boot_failure_report("x", Some(Path::new("/run/x/console.log")), None);
        assert!(unreadable.contains("could not be read"), "{unreadable}");
    }
}
