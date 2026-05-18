//! Idle-timeout lifecycle runtime — slice 2.
//!
//! Spec: `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`.
//! Plan: `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-2.md`.
//!
//! Slice 2 fills in `IdleTimeoutLifecycle::acquire` (the slice-1 stub) with the
//! warm-cache runtime: spawn-on-demand, post-completion cap evaluation, idle teardown,
//! crash detection, exponential restart backoff, and request serialisation.

use std::time::Duration;

use hhagent_protocol::client::ClientError;

use crate::tool_host::ToolHostError;

/// Exponential restart-backoff calculator.
///
/// `next_delay(n)` is the cooldown between restart attempts — applied to *spawn*, not
/// to dispatch. Sequence (in seconds): `1, 2, 4, 8, 16, 32, 60, 60, …`. Resets to 0 on
/// any successful dispatch. Defaults match the spec's "Open questions" §3 recommendation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartBackoff {
    /// Base delay for the first restart (default 1 s).
    pub base: Duration,
    /// Multiplicative factor between restarts (default 2 — exponential).
    /// Stored as integer numerator/denominator to keep the type `Eq`/`Hash`-friendly.
    pub factor_num: u32,
    pub factor_den: u32,
    /// Maximum delay regardless of restart count (default 60 s).
    pub cap: Duration,
}

impl Default for RestartBackoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            factor_num: 2,
            factor_den: 1,
            cap: Duration::from_secs(60),
        }
    }
}

impl RestartBackoff {
    /// Pure: compute the next delay after `consecutive_restarts` restarts have already
    /// happened. `consecutive_restarts = 0` returns `base`; each subsequent value
    /// multiplies by `factor_num/factor_den`, capped at `cap`. Saturating on overflow.
    pub fn next_delay(&self, consecutive_restarts: u32) -> Duration {
        let base_ms = self.base.as_millis() as u64;
        let factor_num = self.factor_num.max(1) as u64;
        let factor_den = self.factor_den.max(1) as u64;
        let cap_ms = self.cap.as_millis() as u64;
        let mut delay_ms = base_ms;
        for _ in 0..consecutive_restarts {
            delay_ms = delay_ms.saturating_mul(factor_num) / factor_den;
            if delay_ms >= cap_ms {
                return self.cap;
            }
        }
        Duration::from_millis(delay_ms.min(cap_ms))
    }
}

/// Pure: classify a dispatch error as "worker died" or "worker still alive".
///
/// The spec's "Cap-check semantics" §"Mid-flight termination" §2 says a worker process
/// reported dead by the OS triggers restart; v1 slice 2 detects death *passively* on
/// the next dispatch attempt, classifying error variants:
///
/// | Variant                                          | Classification |
/// | ------------------------------------------------ | -------------- |
/// | `Ok(_)`                                          | alive          |
/// | `Err(Sandbox(_))`                                | n/a (no worker exists; pre-spawn) |
/// | `Err(Io(_))`                                     | dead           |
/// | `Err(Protocol(Rpc(_)))`                          | alive (worker rejected the call) |
/// | `Err(Protocol(Io(_)))`                           | dead           |
/// | `Err(Protocol(Decode(_)))`                       | dead           |
/// | `Err(Protocol(EarlyExit))`                       | dead           |
/// | `Err(Protocol(IdMismatch { .. }))`               | dead           |
pub fn dispatch_indicates_worker_dead<T>(result: &Result<T, ToolHostError>) -> bool {
    match result {
        Ok(_) => false,
        Err(ToolHostError::Sandbox(_)) => false, // pre-spawn; no worker to be dead
        Err(ToolHostError::Io(_)) => true,
        Err(ToolHostError::Protocol(ClientError::Rpc(_))) => false,
        Err(ToolHostError::Protocol(_)) => true,
    }
}

/// Pure: has this warm worker hit `max_requests`?
///
/// `max_requests == 0` disables the cap (the canonical "0 = unlimited" idiom used by
/// `cpu_quota_pct`/`tasks_max` in `SandboxPolicy`).
pub fn is_request_capped(request_count: u64, max_requests: u64) -> bool {
    max_requests > 0 && request_count >= max_requests
}

/// Pure: has this warm worker exceeded `max_age_seconds`?
///
/// `max_age_seconds == 0` disables the cap.
pub fn is_aged_out(age: Duration, max_age_seconds: u64) -> bool {
    max_age_seconds > 0 && age.as_secs() >= max_age_seconds
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_protocol::RpcError;
    use std::io;

    #[test]
    fn restart_backoff_default_starts_at_one_second() {
        let bo = RestartBackoff::default();
        assert_eq!(bo.next_delay(0), Duration::from_secs(1));
    }

    #[test]
    fn restart_backoff_default_doubles_per_step() {
        let bo = RestartBackoff::default();
        assert_eq!(bo.next_delay(0), Duration::from_secs(1));
        assert_eq!(bo.next_delay(1), Duration::from_secs(2));
        assert_eq!(bo.next_delay(2), Duration::from_secs(4));
        assert_eq!(bo.next_delay(3), Duration::from_secs(8));
        assert_eq!(bo.next_delay(4), Duration::from_secs(16));
        assert_eq!(bo.next_delay(5), Duration::from_secs(32));
    }

    #[test]
    fn restart_backoff_caps_at_default_60s() {
        let bo = RestartBackoff::default();
        assert_eq!(bo.next_delay(6), Duration::from_secs(60));
        assert_eq!(bo.next_delay(100), Duration::from_secs(60));
        // Saturating on overflow — even u32::MAX is bounded by cap.
        assert_eq!(bo.next_delay(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn restart_backoff_custom_cap_honoured() {
        let bo = RestartBackoff {
            base: Duration::from_millis(500),
            factor_num: 2,
            factor_den: 1,
            cap: Duration::from_secs(5),
        };
        assert_eq!(bo.next_delay(0), Duration::from_millis(500));
        assert_eq!(bo.next_delay(1), Duration::from_secs(1));
        assert_eq!(bo.next_delay(2), Duration::from_secs(2));
        assert_eq!(bo.next_delay(3), Duration::from_secs(4));
        assert_eq!(bo.next_delay(4), Duration::from_secs(5));
        assert_eq!(bo.next_delay(10), Duration::from_secs(5));
    }

    #[test]
    fn dispatch_classifier_ok_is_alive() {
        let r: Result<(), ToolHostError> = Ok(());
        assert!(!dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_rpc_error_is_alive() {
        // Worker returned a structured RPC error; it's still listening on stdio.
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(
            ClientError::Rpc(RpcError {
                code: -32001,
                message: "POLICY_DENIED".into(),
                data: None,
            }),
        ));
        assert!(!dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_io_error_is_dead() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "stdio closed",
        )));
        assert!(dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_protocol_io_is_dead() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(ClientError::Io(
            io::Error::new(io::ErrorKind::UnexpectedEof, "eof"),
        )));
        assert!(dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_early_exit_is_dead() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(
            ClientError::EarlyExit,
        ));
        assert!(dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_sandbox_is_not_a_warm_worker_crash() {
        // Sandbox errors come from a failed spawn — no worker existed; this is the
        // SPAWN_FAILED path, not a warm-worker crash. The classifier returns false so
        // the restart-backoff counter doesn't increment.
        let r: Result<(), ToolHostError> = Err(ToolHostError::Sandbox(
            hhagent_sandbox::SandboxError::Backend("test".into()),
        ));
        assert!(!dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn is_request_capped_at_threshold() {
        assert!(!is_request_capped(0, 3));
        assert!(!is_request_capped(2, 3));
        assert!(is_request_capped(3, 3));
        assert!(is_request_capped(99, 3));
    }

    #[test]
    fn is_request_capped_zero_max_means_unlimited() {
        // A zero `max_requests` disables the cap (matches the "0 = unlimited" idiom
        // used by `cpu_quota_pct`/`tasks_max` in `SandboxPolicy`).
        assert!(!is_request_capped(u64::MAX, 0));
    }

    #[test]
    fn is_aged_out_at_threshold() {
        assert!(!is_aged_out(Duration::from_secs(9), 10));
        assert!(is_aged_out(Duration::from_secs(10), 10));
        assert!(is_aged_out(Duration::from_secs(11), 10));
    }

    #[test]
    fn is_aged_out_zero_max_means_unlimited() {
        assert!(!is_aged_out(Duration::from_secs(u64::MAX / 2), 0));
    }
}
