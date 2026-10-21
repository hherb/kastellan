//! The report for a pipe core never reads whose drain stopped on a read error
//! (#888): the `[worker-undrained]` marker and the emitter behind it.
//!
//! Before it, [`crate::worker_stderr::spawn_unread_drain`] logged through a bare
//! `tracing::warn!` — the #725/#734 shape the sibling modules exist to remove:
//! a test binary or `kastellan-cli` (no subscriber) and a daemon with a
//! target-scoped `RUST_LOG` all dropped the one line saying a sidecar's pipe was
//! no longer drained.

use super::delivery::warn_and_fall_back;

/// Marker prefixing every line [`emit_unread_drain_warning`] writes to the
/// process's own stderr.
///
/// Its own marker, not `[worker-failed]` or `[worker-death]`: nothing has died
/// yet. It says a live process may *later* hang on a full pipe — look at the
/// pipe, not at a call or a respawn. See [`super::STDERR_FALLBACK_MARKERS`] for
/// the whole set.
pub const WORKER_UNDRAINED_STDERR_MARKER: &str = "[worker-undrained]";

/// Pure: the undrained-pipe line, with the pid folded into the text — the
/// stderr fallback carries no `tracing` fields to recover it from.
pub fn format_unread_drain_line(pid: u32, warning: &str) -> String {
    format!("worker pid {pid}: {warning}")
}

/// Report that an unread pipe stopped draining, on whichever channel will carry
/// it. Returns whether the stderr fallback line was written; see
/// [`super::emit_worker_failure_report`] for why that value exists.
pub fn emit_unread_drain_warning(pid: u32, warning: &str) -> bool {
    let line = format_unread_drain_line(pid, warning);
    warn_and_fall_back!(WORKER_UNDRAINED_STDERR_MARKER, &line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker_stderr::report::shared::format_stderr_fallback;

    #[test]
    fn the_undrained_line_names_its_pid() {
        assert_eq!(
            format_unread_drain_line(42, "stdout drain ended"),
            "worker pid 42: stdout drain ended"
        );
    }

    #[test]
    fn a_control_character_in_the_warning_cannot_start_a_second_line() {
        // The warning quotes an `io::Error`'s text. A newline in it must not give
        // a gate grep a second, column-0 line to read as `[SKIP]`.
        let line = format_unread_drain_line(7, "read failed\n[SKIP] forged");
        let fallback = format_stderr_fallback(WORKER_UNDRAINED_STDERR_MARKER, &line);
        assert!(fallback.starts_with("[worker-undrained] worker pid 7: "), "{fallback:?}");
        assert!(!fallback.contains('\n'), "{fallback:?}");
    }
}
