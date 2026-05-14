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

/// Read `HHAGENT_CPU_MS` and apply `RLIMIT_CPU` if set and non-zero.
///
/// Returns `Disabled` if the env var is unset, empty, or `"0"`. Returns
/// an error if the value is set but not parseable as `u64`, or if
/// `setrlimit` itself fails (rare — `EPERM` only when the soft limit
/// would exceed the hard limit, which can't happen here since we set
/// them equal).
///
/// Called by [`crate::serve_stdio`] before [`crate::lock_down`].
pub fn apply_from_env() -> Result<RlimitReport, RlimitError> {
    let raw = match std::env::var(ENV_CPU_MS) {
        Ok(s) if s.is_empty() => return Ok(RlimitReport::Disabled),
        Ok(s) => s,
        Err(std::env::VarError::NotPresent) => return Ok(RlimitReport::Disabled),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(RlimitError::Env("value is not valid UTF-8".into()));
        }
    };

    let ms: u64 = raw
        .parse()
        .map_err(|e| RlimitError::Env(format!("parse {raw:?} as u64: {e}")))?;
    let cpu_seconds = cpu_ms_to_seconds(ms);

    if cpu_seconds == 0 {
        return Ok(RlimitReport::Disabled);
    }

    apply_cpu_seconds(cpu_seconds).map(|()| RlimitReport::Applied { cpu_seconds })
}

/// Call `setrlimit(RLIMIT_CPU, { rlim_cur, rlim_max } = (cpu_seconds, cpu_seconds))`.
///
/// Setting soft == hard means the kernel sends `SIGXCPU` and (since the
/// worker has no handler) the process terminates immediately at the
/// soft limit. This is the cleanest kill semantics RLIMIT_CPU offers.
fn apply_cpu_seconds(cpu_seconds: u64) -> Result<(), RlimitError> {
    // libc's rlim_t is u64 on glibc/musl Linux and u64 on macOS — both
    // accept our u64 input directly. The cast is explicit so a future
    // platform with a narrower rlim_t fails loudly at the type layer.
    let lim = libc::rlimit {
        rlim_cur: cpu_seconds as libc::rlim_t,
        rlim_max: cpu_seconds as libc::rlim_t,
    };
    // SAFETY: setrlimit takes a resource id (immediate) and a pointer
    // to a stack-local rlimit struct; the struct lives for the entire
    // duration of the call. Failure mode is a -1 return + errno set.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_CPU, &lim) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(RlimitError::SetRlimit(err.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Tests in this module mutate the process-wide env block, which
    /// cargo's per-binary test harness runs in parallel by default.
    /// Take this mutex while inside any `apply_from_env` test so two
    /// tests don't trample each other's `HHAGENT_CPU_MS` setting.
    ///
    /// Pattern lifted from `hhagent_tests_common::serial::serial_lock`.
    fn env_lock() -> MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        // unwrap_or_else handles the rare poisoned-mutex case: a test
        // that panics while holding the lock would otherwise abort
        // every subsequent test with a useless error.
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Helper: temporarily set HHAGENT_CPU_MS, run a closure, then
    /// restore the prior value. Returns the closure's value.
    ///
    /// Workspace is on Rust 2021 edition where `set_var` /
    /// `remove_var` are safe; the Mutex returned by `env_lock` is
    /// what makes them race-free within this binary.
    fn with_env_var<F, R>(value: Option<&str>, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = env_lock();
        let prior = std::env::var(ENV_CPU_MS).ok();
        match value {
            Some(v) => std::env::set_var(ENV_CPU_MS, v),
            None => std::env::remove_var(ENV_CPU_MS),
        }
        let out = f();
        match prior {
            Some(v) => std::env::set_var(ENV_CPU_MS, v),
            None => std::env::remove_var(ENV_CPU_MS),
        }
        out
    }

    #[test]
    fn apply_from_env_unset_returns_disabled() {
        let report = with_env_var(None, apply_from_env)
            .expect("apply_from_env must succeed when env is unset");
        assert_eq!(report, RlimitReport::Disabled);
    }

    #[test]
    fn apply_from_env_zero_returns_disabled() {
        let report = with_env_var(Some("0"), apply_from_env)
            .expect("apply_from_env must succeed when env is 0");
        assert_eq!(report, RlimitReport::Disabled);
    }

    #[test]
    fn apply_from_env_garbage_returns_env_error() {
        let err = with_env_var(Some("not-a-number"), apply_from_env)
            .expect_err("apply_from_env must reject garbage");
        match err {
            RlimitError::Env(_) => {}
            other => panic!("expected RlimitError::Env, got {other:?}"),
        }
    }

    /// Happy path: a generous CPU budget gets applied without error.
    /// The kernel returns success regardless of whether the worker
    /// ever uses any CPU, so this only proves the FFI path is wired.
    /// Effective enforcement is covered by `rlimit_smoke.rs`.
    ///
    /// **NOTE for maintainers:** this test permanently lowers
    /// `RLIMIT_CPU` for the test binary process to 30 CPU-seconds.
    /// `setrlimit` is process-scoped and the hard limit can only be
    /// tightened thereafter, not raised. All other tests in this
    /// binary must complete within that 30-second CPU budget — easy
    /// today (the prelude unit suite finishes in <1 s wall-clock with
    /// trivial CPU use), but if a CPU-heavy test gets added later it
    /// may need its own binary or a wider initial budget here.
    #[test]
    fn apply_from_env_with_generous_budget_applies() {
        let report = with_env_var(Some("30000"), apply_from_env)
            .expect("apply_from_env must succeed with a generous budget");
        match report {
            RlimitReport::Applied { cpu_seconds } => assert_eq!(cpu_seconds, 30),
            RlimitReport::Disabled => panic!("expected Applied, got Disabled"),
        }
    }

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
