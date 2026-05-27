//! Pure unit tests for the `idle_timeout` lifecycle module.
//!
//! Lifted from an inline `#[cfg(test)] mod tests` block in `idle_timeout.rs`
//! to keep the production file under the 500-LOC soft cap. The body is
//! byte-identical to what it was inline; `use super::*` still resolves to
//! the parent `idle_timeout` module per the Rust 2018 sibling-directory
//! module pattern.

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

/// Pins the IdleTimeoutLifecycle warm-cache key invariant (issue #121).
///
/// The warm-cache key is `tool_name` only; `ToolEntry.container_image` is
/// deliberately NOT in the key signature. Two `slot_for` calls under the same
/// tool name MUST return the same `Arc<ToolSlot>` regardless of any
/// hypothetical image-tag variation in the caller's `ToolEntry`. This is
/// safe today because image tags are baked in at daemon startup and a
/// restart flushes the registry; a future live-reconfigure path that allows
/// the same tool name to swap image tags without a restart would silently
/// serve requests through a worker spawned under the stale image.
///
/// If this test fires:
///   - You widened `slot_for`'s key signature → either intentional (then
///     update this test + every call site) or accidental (revert).
///   - You introduced a live-reconfigure path → either widen the key as
///     above, OR explicitly evict the warm slot for the tool before
///     serving requests through the re-registered entry.
#[test]
fn slot_for_key_excludes_container_image() {
    let registry: WarmRegistry = empty_registry();
    let slot1 = slot_for(&registry, "twice-name");
    let slot2 = slot_for(&registry, "twice-name");
    assert!(
        Arc::ptr_eq(&slot1, &slot2),
        "warm-cache widened: second slot_for under same tool_name returned a different Arc. \
         If this is intentional (live-reconfigure path landed), the warm-cache key MUST \
         widen to (tool_name, container_image) — see issue #121 and the slot_for doc."
    );
    // Sibling tool names get distinct slots (sanity check that the key is
    // not collapsing everything).
    let other = slot_for(&registry, "other-tool");
    assert!(
        !Arc::ptr_eq(&slot1, &other),
        "warm-cache collapsed: distinct tool names returned the same slot. \
         The HashMap<String, Arc<ToolSlot>> shape is violated."
    );
}
