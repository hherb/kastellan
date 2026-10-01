//! One ack call from the polled driver, and what its failure means for the
//! rest of the batch (#769). Split out of [`super`] to keep it under the
//! 500-LOC soft cap.

use super::outage::OutageLog;
use super::refusal::{accepted, is_refusal, refused, RefusalRun};
use super::audit::record_skipped;
use super::{AckOnlyAudit, EncodeAck, PolledWorkerSpec, WorkerCalls};
use crate::channel::{ChannelId, SkippedId};

/// Ack one id. `Ok(true)` when accepted; `Ok(false)` when refused — already
/// logged, and the ack's backoff set, so the caller stops acking this batch;
/// `Err` when the worker is down, which stays non-fatal (the message is
/// redelivered) and is returned for the caller's own line, with the
/// worker-written text neutralised.
pub(super) fn ack(
    calls: &dyn WorkerCalls,
    spec: &PolledWorkerSpec,
    method: &str,
    params: serde_json::Value,
    outage: &mut OutageLog,
    run: &mut RefusalRun,
) -> Result<bool, String> {
    match calls.call(method, params) {
        Ok(_) => {
            accepted(run, outage, spec.label, method, None);
            Ok(true)
        }
        Err(e) if is_refusal(&e) => {
            refused(run, outage, spec, method, None, &e);
            Ok(false)
        }
        Err(e) => Err(crate::untrusted_text::neutralise_controls(&e.to_string())),
    }
}

/// Ack the ids that never became a `PolledEvent` (e.g. email's `skipped` list)
/// — see the module docs' "Acking ids that never become an event". Only once
/// the same batch's events reached the bus, which the caller guarantees by
/// calling this from `parse_poll`'s `Ok` arm; and like the event acks, it stops
/// at the first refusal. A spec without `ack_method` acks nothing.
///
/// `channel` is the bus's id for the channel, which the audit row names.
#[allow(clippy::too_many_arguments)] // the ack's own inputs, plus the audit hook and the id it names
pub(super) fn ack_skipped(
    calls: &dyn WorkerCalls,
    spec: &PolledWorkerSpec,
    enc: EncodeAck,
    ids: Vec<(String, String)>,
    audit: Option<&AckOnlyAudit>,
    channel: &ChannelId,
    outage: &mut OutageLog,
    run: &mut RefusalRun,
) {
    let Some(method) = spec.ack_method else { return };
    for (id, reason) in ids {
        tracing::warn!(
            label = spec.label,
            message_id = %id,
            reason = %reason,
            "discarding a message that never became an event; acking it \
             so the worker's cursor advances"
        );
        // Best-effort audit trail: never FAILS the ack itself — a real hook
        // reports its own insert error (the daemon's, on `[audit-lost]`) (see AckOnlyAudit's docs — `None`
        // when the caller has no durable sink to write to). It must NOT block:
        // this thread is the one every conversation, the poll and the ack wait
        // on, so a hook that waited for its insert stalled the whole channel
        // for a pool-acquire timeout per skipped id (#789). The daemon's hook
        // spawns its insert and returns; the call is timed, and one that
        // held the thread anyway is reported (#793).
        let skipped = SkippedId {
            channel,
            message_id: &id,
            reason: &reason,
            observed_at: time::OffsetDateTime::now_utc(),
        };
        record_skipped(audit, spec.label, skipped);
        match ack(calls, spec, method, enc(&id), outage, run) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => tracing::warn!(
                label = spec.label,
                error = %e,
                message_id = %id,
                "ack of skipped id failed; will retry next poll"
            ),
        }
    }
}
