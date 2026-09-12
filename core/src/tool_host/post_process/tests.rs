//! Unit tests for [`super::build_tool_audit_payload`] — the pure half of the
//! dispatch chokepoint's audit-row construction.

use super::*;

/// The whole cross-crate point of issue #617: the row an oversized dispatch
/// stores still says what ran.
///
/// This runs the **real** [`kastellan_db::audit::truncate_payload`] over the
/// **real** producer's output, because a recording or mock `AuditSink` sees
/// the payload the caller *passed*, never the one the database *stored* —
/// truncation happens inside `db::audit::insert`, past every sink double in
/// this tree. A test built on a sink double would assert the request is
/// present and prove nothing at all about the stored row.
#[test]
fn an_oversized_tool_row_still_records_what_ran() {
    let req = serde_json::json!({"argv": ["/usr/bin/env", "bash", "-c", "make test"]});
    let bulk = "o".repeat(kastellan_db::audit::PAYLOAD_MAX_BYTES * 2);
    let result = serde_json::json!({"stdout": bulk});

    let stored = kastellan_db::audit::truncate_payload(build_tool_audit_payload(
        &req,
        Ok(&result),
        None,
        12,
    ));

    assert!(
        kastellan_db::audit::is_truncation_envelope(&stored),
        "fixture must actually be over the cap"
    );
    let head = stored[kastellan_db::audit::REQ_SUMMARY_KEY]["head"]
        .as_str()
        .expect("an oversized row with a request must carry its summary");
    assert!(head.contains("/usr/bin/env"), "the head must name the command, got: {head}");
    assert!(head.contains("make test"), "and its arguments, got: {head}");
}

/// The success arm's shape is the wire contract stated in
/// [`crate::tool_host::dispatch`]'s rustdoc: `{req, result, ms}`, and the
/// request sits under the key `kastellan-db` looks for.
#[test]
fn the_success_arm_carries_req_result_and_ms() {
    let req = serde_json::json!({"argv": ["/bin/true"]});
    let result = serde_json::json!({"code": 0});

    let payload = build_tool_audit_payload(&req, Ok(&result), None, 7);

    assert_eq!(payload[kastellan_db::audit::REQ_KEY], req);
    assert_eq!(payload["result"], result);
    assert_eq!(payload["ms"], 7);
    assert!(payload.get("err").is_none(), "a success arm carries no error");
    let mut keys: Vec<&str> = payload.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["ms", "req", "result"]);
}

/// The failure arm swaps `result` for `err`, and keeps the request: a call
/// that failed is still a call that was made.
#[test]
fn the_failure_arm_carries_err_instead_of_result() {
    let req = serde_json::json!({"argv": ["/bin/false"]});

    let payload = build_tool_audit_payload(&req, Err("worker exited before responding"), None, 9);

    assert_eq!(payload[kastellan_db::audit::REQ_KEY], req);
    assert_eq!(payload["err"], "worker exited before responding");
    assert_eq!(payload["ms"], 9);
    assert!(payload.get("result").is_none(), "a failure arm carries no result");
}

/// The guard verdict is attached under the key `db::audit` preserves through
/// truncation — `GUARD_KEY`, not a second literal spelling of it. Spelled
/// twice, a rename on the database side would compile here and cleared
/// scores would silently stop surviving the cap, which is the defect
/// `PRESERVED_KEYS` was created to fix.
#[test]
fn a_guard_verdict_is_attached_under_the_preserved_key() {
    let req = serde_json::json!({"argv": ["/bin/cat", "/etc/hosts"]});
    let result = serde_json::json!({"stdout": "127.0.0.1 localhost"});
    let verdict = serde_json::json!({"state": "clear", "p": 0.11, "tau": 0.79552656});

    let payload =
        build_tool_audit_payload(&req, Ok(&result), Some(verdict.clone()), 4);

    assert_eq!(payload[kastellan_db::audit::GUARD_KEY], verdict);
}

/// Both preserved records survive an oversized row together — the verdict,
/// and the act it was a verdict about.
#[test]
fn an_oversized_row_keeps_the_guard_verdict_beside_the_request_summary() {
    let req = serde_json::json!({"argv": ["/bin/cat", "/etc/hosts"]});
    let bulk = "h".repeat(kastellan_db::audit::PAYLOAD_MAX_BYTES * 2);
    let result = serde_json::json!({"stdout": bulk});
    let verdict = serde_json::json!({"state": "clear", "p": 0.11});

    let stored = kastellan_db::audit::truncate_payload(build_tool_audit_payload(
        &req,
        Ok(&result),
        Some(verdict),
        4,
    ));

    assert_eq!(stored[kastellan_db::audit::GUARD_KEY]["state"], "clear");
    assert!(stored[kastellan_db::audit::REQ_SUMMARY_KEY]["head"]
        .as_str()
        .unwrap()
        .contains("/bin/cat"));
}

/// No verdict, no key — an absent guard record must not render as a present
/// one carrying nothing, which is the distinction `DROPPED_PRESERVED_KEY`
/// draws for the truncation case.
#[test]
fn no_guard_verdict_means_no_guard_key() {
    let payload =
        build_tool_audit_payload(&serde_json::json!({"argv": []}), Ok(&serde_json::json!({})), None, 1);

    assert!(payload.get(kastellan_db::audit::GUARD_KEY).is_none());
}
