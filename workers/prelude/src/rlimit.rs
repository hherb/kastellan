//! Worker-side `setrlimit` enforcement for `policy.cpu_ms`.
//!
//! Cross-platform — `setrlimit` is POSIX, so the same code runs on
//! Linux and macOS. This module is the cross-platform companion to
//! [`crate::lock_down`] (which is Linux-only).
//!
//! ## How it composes with seccomp
//!
//! `apply_from_env` is called by [`crate::serve_stdio`] **before**
//! `lock_down`. Some future seccomp profiles may ban `prlimit64`; setting
//! the cap earlier guarantees the cap is in place before any syscall
//! restrictions land.
//!
//! ## Why `RLIMIT_CPU` and not cgroup CPU-seconds
//!
//! cgroup v2 has no direct "total CPU-seconds budget" primitive — its
//! CPU primitive is bandwidth (`CPUQuota=N%`). `RLIMIT_CPU` is the
//! natural enforcement for `policy.cpu_ms`. Resolution is integer
//! seconds (with `SIGXCPU` on soft, `SIGKILL` on hard); the worker has
//! no `SIGXCPU` handler installed so the soft hit terminates the
//! process immediately — equivalent to a clean kill.

/// Env var read by [`apply_from_env`]. Set by
/// `hhagent_core::tool_host::derive_lockdown_env` from
/// `policy.cpu_ms`.
pub const ENV_CPU_MS: &str = "HHAGENT_CPU_MS";

/// Status of the rlimit layer after [`apply_from_env`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlimitReport {
    /// `RLIMIT_CPU` set successfully at `cpu_seconds` (soft = hard).
    Applied { cpu_seconds: u64 },
    /// Env var was unset or `"0"`. No rlimit applied. The worker still
    /// runs but has no CPU-time ceiling beyond cgroup bandwidth (Linux)
    /// or whatever the parent supervisor enforces.
    Disabled,
}

/// Errors from [`apply_from_env`]. Both variants are fail-closed:
/// `serve_stdio` propagates them as `io::Error` and the worker exits
/// before serving any request.
#[derive(Debug, thiserror::Error)]
pub enum RlimitError {
    /// `HHAGENT_CPU_MS` was set but couldn't be parsed as `u64`.
    #[error("env {ENV_CPU_MS}: {0}")]
    Env(String),
    /// `setrlimit(RLIMIT_CPU, …)` returned a non-zero error code.
    #[error("setrlimit RLIMIT_CPU: {0}")]
    SetRlimit(String),
}

/// Convert a millisecond CPU budget to integer seconds for
/// `RLIMIT_CPU`. Ceiling division with a 1-second floor when `ms > 0`;
/// `ms == 0` → `0` (the "no rlimit" sentinel).
///
/// `RLIMIT_CPU`'s resolution is integer seconds, so any fractional
/// millisecond budget needs to be rounded *up* — rounding down would
/// effectively halve a 500 ms budget to 0. The 1-second floor ensures
/// even a 1 ms budget produces a meaningful kill (after at least one
/// second of CPU time, which is the kernel's resolution).
///
/// Saturates on overflow rather than panicking: the `+ 999` step uses
/// `saturating_add`, so a caller passing `u64::MAX` divides the
/// saturated intermediate by 1000 and gets back `u64::MAX / 1000` (≈
/// 1.84 × 10¹⁶ seconds — effectively unlimited), not a runtime panic.
///
/// ```text
/// 0        → 0
/// 1        → 1
/// 999      → 1
/// 1000     → 1
/// 1001     → 2
/// 1999     → 2
/// 2000     → 2
/// u64::MAX → u64::MAX / 1000  (saturating intermediate, then div)
/// ```
pub fn cpu_ms_to_seconds(ms: u64) -> u64 {
    if ms == 0 {
        return 0;
    }
    // (ms + 999) / 1000 with saturating add to defend against
    // u64::MAX + 999 overflow.
    ms.saturating_add(999) / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_ms_to_seconds_zero_yields_zero() {
        assert_eq!(cpu_ms_to_seconds(0), 0);
    }

    #[test]
    fn cpu_ms_to_seconds_one_yields_one() {
        // A 1 ms budget rounds up to the 1-second floor.
        assert_eq!(cpu_ms_to_seconds(1), 1);
    }

    #[test]
    fn cpu_ms_to_seconds_just_under_one_second_yields_one() {
        assert_eq!(cpu_ms_to_seconds(999), 1);
    }

    #[test]
    fn cpu_ms_to_seconds_exactly_one_second_yields_one() {
        assert_eq!(cpu_ms_to_seconds(1_000), 1);
    }

    #[test]
    fn cpu_ms_to_seconds_just_over_one_second_yields_two() {
        // 1001 ms rounds up to 2 s.
        assert_eq!(cpu_ms_to_seconds(1_001), 2);
    }

    #[test]
    fn cpu_ms_to_seconds_saturates_on_overflow() {
        // u64::MAX must not panic.
        assert_eq!(cpu_ms_to_seconds(u64::MAX), u64::MAX / 1_000);
    }
}
