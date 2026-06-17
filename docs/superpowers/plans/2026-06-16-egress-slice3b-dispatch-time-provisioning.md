# Egress #3b dispatch-time secret-hash provisioning — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `tool_host::dispatch` materializes a `secret://` reference for a force-routed net worker, write that secret's value-fingerprint into the worker's egress-sidecar `secret_hashes.json` *before* the worker can egress, so the proxy's leak scanner catches exfiltration (issue [#268](https://github.com/hherb/kastellan/issues/268)).

**Architecture:** Three small, independently-tested pieces glued at the dispatch chokepoint. (1) a pure `collect_refs_in_params` recovers which `secret://` refs a call carries (the one-way `RedemptionEvent.ref_hash` can't be reversed). (2) `merge_secret_hashes` keeps the union of fingerprints across worker reuse (read-merge-atomic-write, dedup by sha256) and returns the newly-added set. (3) a thin `egress_provision` helper under `tool_host/` computes the outcome synchronously (so no borrow is held across `.await`) and emits audit rows; fail-closed on write error.

**Tech Stack:** Rust (workspace, rustc 1.96), `kastellan-leak-scan` (fingerprints + wire format), `serde_json`, `#[async_trait]` audit seam, `thiserror`.

**Ratified decisions:** D1 fail-closed on provision-write failure; D2 union accumulation across reused workers; D3 audit only newly-added fingerprints. See `docs/superpowers/specs/2026-06-16-egress-slice3b-dispatch-time-provisioning-design.md`.

**Build/test prelude (run once per shell):**
```sh
source "$HOME/.cargo/env"
```

---

### Task 1: Pure `collect_refs_in_params`

Recover the `SecretRef`s a params tree carries, without redeeming them. Pure, no vault, no I/O.

**Files:**
- Create: `core/src/secrets/collect.rs`
- Modify: `core/src/secrets/mod.rs` (add `mod collect;` + re-export)
- Modify: `core/src/secrets/substitute.rs:57` (widen `is_well_formed_ref` visibility)

- [ ] **Step 1: Widen the shared ref-shape helper**

In `core/src/secrets/substitute.rs` line 57, change:
```rust
fn is_well_formed_ref(s: &str) -> bool {
```
to:
```rust
pub(crate) fn is_well_formed_ref(s: &str) -> bool {
```

- [ ] **Step 2: Write `collect.rs` with the failing tests first**

Create `core/src/secrets/collect.rs`:
```rust
//! Pure helper: enumerate the `secret://` references a params tree carries,
//! WITHOUT redeeming them. Used by the dispatch chokepoint (egress slice #3b,
//! #268) to learn which secrets a worker is about to receive — the one-way
//! `RedemptionEvent.ref_hash` cannot be reversed to a `SecretRef`, so we
//! re-scan the pre-substitution params instead.

use std::collections::HashSet;

use super::substitute::is_well_formed_ref;
use super::vault::SecretRef;

/// Walk `value` and return every well-formed `secret://<8-hex>` reference it
/// contains, dedup'd by `ref_hash`, in first-seen order (deterministic for
/// tests). Pure: no vault, no I/O, no mutation. Mirrors the JSON walk shape of
/// [`super::substitute`].
pub fn collect_refs_in_params(value: &serde_json::Value) -> Vec<SecretRef> {
    let mut out: Vec<SecretRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    walk(value, &mut out, &mut seen);
    out
}

fn walk(value: &serde_json::Value, out: &mut Vec<SecretRef>, seen: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(s) => {
            if is_well_formed_ref(s) {
                let r = SecretRef::from_raw(s.clone());
                if seen.insert(r.ref_hash()) {
                    out.push(r);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for it in items {
                walk(it, out, seen);
            }
        }
        serde_json::Value::Object(map) => {
            for (_k, v) in map.iter() {
                walk(v, out, seen);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hashes(refs: &[SecretRef]) -> Vec<String> {
        refs.iter().map(|r| r.ref_hash()).collect()
    }

    #[test]
    fn finds_refs_nested_in_objects_and_arrays() {
        let v = json!({
            "a": "secret://deadbeef",
            "b": ["plain", {"c": "secret://cafef00d"}],
        });
        let got = collect_refs_in_params(&v);
        assert_eq!(got.len(), 2);
        assert_eq!(
            hashes(&got),
            hashes(&[
                SecretRef::from_raw("secret://deadbeef".into()),
                SecretRef::from_raw("secret://cafef00d".into()),
            ])
        );
    }

    #[test]
    fn dedups_repeated_ref_first_seen_order() {
        let v = json!(["secret://deadbeef", "secret://deadbeef"]);
        let got = collect_refs_in_params(&v);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn ignores_non_ref_strings_and_malformed_refs() {
        let v = json!({
            "plain": "hello",
            "almost": "secret://nothex!!",
            "short": "secret://dead",
        });
        assert!(collect_refs_in_params(&v).is_empty());
    }

    #[test]
    fn empty_params_yield_no_refs() {
        assert!(collect_refs_in_params(&json!({})).is_empty());
        assert!(collect_refs_in_params(&serde_json::Value::Null).is_empty());
    }
}
```

- [ ] **Step 3: Register + re-export the module**

In `core/src/secrets/mod.rs`, after the `pub mod substitute;` / `pub mod vault;` lines (around line 23), add:
```rust
pub mod collect;
```
and extend the re-export block (after the `pub use substitute::{...};` block, ~line 27) with:
```rust
pub use collect::collect_refs_in_params;
```

- [ ] **Step 4: Run the tests — expect them to fail first, then pass**

Run: `cargo test -p kastellan-core --lib secrets::collect -- --nocapture`
Expected: PASS (4 tests). If `is_well_formed_ref` / `from_raw` / `ref_hash` visibility is wrong, the compile error names the item — fix the `pub(crate)` in Step 1.

- [ ] **Step 5: Commit**
```bash
git add core/src/secrets/collect.rs core/src/secrets/mod.rs core/src/secrets/substitute.rs
git commit -m "feat(#268): pure collect_refs_in_params for dispatch-time provisioning"
```

---

### Task 2: `merge_secret_hashes` + fail-closed audit row

Union accumulation (D2) and the fail-closed audit shape (D1).

**Files:**
- Modify: `core/src/egress/leak_provision.rs`

- [ ] **Step 1: Add the `parse_hashes` import**

In `core/src/egress/leak_provision.rs` line 14, change:
```rust
use kastellan_leak_scan::{serialize_hashes, SecretFingerprint};
```
to:
```rust
use kastellan_leak_scan::{parse_hashes, serialize_hashes, SecretFingerprint};
```

- [ ] **Step 2: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `leak_provision.rs` (the module already imports `fingerprint_value` and `parse_hashes` inside it; the new tests reuse `super::*`):
```rust
    #[test]
    fn merge_into_empty_writes_and_reports_all_added() {
        let dir = tempfile::tempdir().unwrap();
        let a = fingerprint_value(b"first-secret-value").unwrap();
        let added = merge_secret_hashes(dir.path(), &[a.clone()]).unwrap();
        assert_eq!(added, vec![a.clone()]);
        let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
        assert_eq!(parse_hashes(&s), vec![a]);
    }

    #[test]
    fn merge_unions_across_calls_dedup_by_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let a = fingerprint_value(b"first-secret-value").unwrap();
        let b = fingerprint_value(b"second-secret-value").unwrap();
        merge_secret_hashes(dir.path(), &[a.clone()]).unwrap();
        // Second call adds b but NOT a again.
        let added = merge_secret_hashes(dir.path(), &[a.clone(), b.clone()]).unwrap();
        assert_eq!(added, vec![b.clone()]);
        let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
        let got = parse_hashes(&s);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&a) && got.contains(&b));
    }

    #[test]
    fn merge_of_already_present_adds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let a = fingerprint_value(b"first-secret-value").unwrap();
        merge_secret_hashes(dir.path(), &[a.clone()]).unwrap();
        let added = merge_secret_hashes(dir.path(), &[a.clone()]).unwrap();
        assert!(added.is_empty());
    }

    #[test]
    fn merge_treats_corrupt_existing_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SECRET_HASHES_FILE_NAME), b"not json").unwrap();
        let a = fingerprint_value(b"first-secret-value").unwrap();
        let added = merge_secret_hashes(dir.path(), &[a.clone()]).unwrap();
        assert_eq!(added, vec![a]);
    }

    #[test]
    fn merge_errors_when_scratch_unwritable() {
        // A path whose parent does not exist makes the atomic write fail,
        // which drives the dispatch fail-closed (D1).
        let missing = std::path::Path::new("/nonexistent-kastellan-268/scratch");
        let a = fingerprint_value(b"first-secret-value").unwrap();
        assert!(merge_secret_hashes(missing, &[a]).is_err());
    }

    #[test]
    fn provision_failed_row_shape() {
        let row = provision_failed_audit_row("web-fetch", 2, "disk full");
        assert_eq!(row.actor, "egress_proxy");
        assert_eq!(row.action, "egress.secret_hash.provision_failed");
        assert_eq!(row.payload["worker"], "web-fetch");
        assert_eq!(row.payload["pending"], 2);
        assert_eq!(row.payload["error"], "disk full");
    }
```

- [ ] **Step 3: Implement `merge_secret_hashes` + `provision_failed_audit_row`**

Add to `leak_provision.rs` (after `provision_audit_row`, before the `#[cfg(test)]` block):
```rust
/// Merge `new` fingerprints into `<scratch>/secret_hashes.json`, keeping the
/// UNION across the worker's lifetime (dedup by `sha256`), and return the
/// newly-added fingerprints (those not already present). Reads the existing
/// file leniently (missing/corrupt ⇒ empty), then atomically rewrites only if
/// the union grew. Union (not overwrite) means a later connection reusing an
/// earlier secret is still scanned (egress slice #3b, #268, decision D2).
///
/// A write failure surfaces as `Err` so the dispatch caller can fail closed
/// (decision D1) rather than let a secret-bearing worker egress unscanned.
pub fn merge_secret_hashes(
    scratch: &Path,
    new: &[SecretFingerprint],
) -> io::Result<Vec<SecretFingerprint>> {
    let existing = read_existing(scratch);
    let mut have: std::collections::HashSet<[u8; 32]> =
        existing.iter().map(|f| f.sha256).collect();
    let mut added: Vec<SecretFingerprint> = Vec::new();
    for fp in new {
        if have.insert(fp.sha256) {
            added.push(fp.clone());
        }
    }
    if !added.is_empty() {
        let mut union = existing;
        union.extend(added.iter().cloned());
        write_secret_hashes(scratch, &union)?;
    }
    Ok(added)
}

/// Lenient read of the existing fingerprint file: missing or corrupt ⇒ empty
/// (fail-safe; the merge then provisions at least this dispatch's secrets —
/// never silently fewer than the current call requires).
fn read_existing(scratch: &Path) -> Vec<SecretFingerprint> {
    match std::fs::read_to_string(scratch.join(SECRET_HASHES_FILE_NAME)) {
        Ok(s) => parse_hashes(&s),
        Err(_) => Vec::new(),
    }
}

/// Build the fail-closed `policy / egress.secret_hash.provision_failed` audit
/// row for when dispatch could not write the scanner patterns for a
/// secret-bearing net worker (the dispatch is then refused; the worker never
/// egresses). Carries the worker name, how many fingerprints were pending, and
/// the error string — no plaintext. Pure; the caller inserts it.
pub fn provision_failed_audit_row(worker: &str, pending: usize, err: &str) -> EgressAuditRow {
    EgressAuditRow {
        actor: ACTOR,
        action: "egress.secret_hash.provision_failed".to_string(),
        payload: serde_json::json!({
            "worker": worker,
            "pending": pending,
            "error": err,
        }),
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kastellan-core --lib egress::leak_provision -- --nocapture`
Expected: PASS (existing 3 + new 6 = 9 tests).

- [ ] **Step 5: Commit**
```bash
git add core/src/egress/leak_provision.rs
git commit -m "feat(#268): merge_secret_hashes union accumulation + fail-closed audit row"
```

---

### Task 3: `EgressSidecar::provision_dispatch_secrets` + error variant

The thin delegate that resolves the sidecar scratch dir, and the fail-closed error type.

**Files:**
- Modify: `core/src/egress/net_worker.rs` (add method on `EgressSidecar`)
- Modify: `core/src/tool_host.rs:35-50` (add `ToolHostError` variant)

- [ ] **Step 1: Add the error variant**

In `core/src/tool_host.rs`, inside `pub enum ToolHostError` (after the `SecretRedemptionFailed` variant, before the closing `}` at line 50), add:
```rust

    /// Egress slice #3b (#268). Dispatch-time leak-scanner provisioning failed
    /// for a secret-bearing force-routed net worker. Fail-CLOSED: the worker is
    /// never called, so a secret can never reach a net worker the scanner
    /// cannot watch. The fail-closed audit row was already emitted before this
    /// error. Scheduler treats it like POLICY_DENIED (fail fast, no retry).
    #[error("tool_host: egress leak-scanner provisioning failed: {0}")]
    EgressProvisionFailed(String),
```

- [ ] **Step 2: Add the `provision_dispatch_secrets` method**

In `core/src/egress/net_worker.rs`, add an `impl` block for `EgressSidecar` (place it right after the existing `impl Drop for EgressSidecar { ... }` block, ~line 95):
```rust
impl EgressSidecar {
    /// Dispatch-time live provisioning (egress slice #3b, #268): merge `fps`
    /// into this worker's sidecar `secret_hashes.json` (union across reuse) and
    /// return the newly-added fingerprints for audit. The scratch dir holding
    /// that file is always the parent of the sidecar UDS — present for the
    /// sidecar's whole lifetime. Reuses [`super::leak_provision::merge_secret_hashes`].
    pub(crate) fn provision_dispatch_secrets(
        &self,
        fps: &[SecretFingerprint],
    ) -> std::io::Result<Vec<SecretFingerprint>> {
        let dir = self.sidecar.uds_path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "sidecar uds_path has no parent dir",
            )
        })?;
        super::leak_provision::merge_secret_hashes(dir, fps)
    }
}
```
(`SecretFingerprint` is already imported at the top of `net_worker.rs`; `self.sidecar.uds_path` is `pub`.)

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p kastellan-core`
Expected: builds clean (no test yet — this 5-line delegate is covered by Task 4's `compute_provision` glue plus Task 2's `merge_secret_hashes` units; constructing a real `EgressSidecar` needs a spawned sidecar, out of scope for a unit test).

- [ ] **Step 4: Commit**
```bash
git add core/src/egress/net_worker.rs core/src/tool_host.rs
git commit -m "feat(#268): EgressSidecar::provision_dispatch_secrets + EgressProvisionFailed"
```

---

### Task 4: `tool_host/egress_provision` helper (compute + emit)

Keeps the dispatch chokepoint tiny (respects the 500-LOC cap on `tool_host.rs`) and makes the fail-closed (D1) + audit (D3) decision unit-testable with a fake sink.

**Files:**
- Create: `core/src/tool_host/egress_provision.rs`
- Modify: `core/src/tool_host.rs` (add `mod egress_provision;`)

- [ ] **Step 1: Register the submodule**

In `core/src/tool_host.rs`, alongside the other `mod` declarations (~line 19-31, e.g. after `mod audit_sink;`), add:
```rust
mod egress_provision;
```

- [ ] **Step 2: Write the helper with failing unit tests**

Create `core/src/tool_host/egress_provision.rs`:
```rust
//! Dispatch-time egress leak-scanner provisioning (egress slice #3b, #268).
//!
//! Pulled out of the dispatch chokepoint so `tool_host.rs` stays near the
//! 500-LOC cap and so the fail-closed (D1) + audit (D3) decision is testable
//! with a fake [`AuditSink`]. [`compute_provision`] runs **synchronously** (no
//! `.await`) so the `&EgressSidecar` borrow of the worker is released before
//! `worker.call`; [`emit_provision`] then writes the audit rows.

use kastellan_leak_scan::SecretFingerprint;

use super::audit_sink::AuditSink;
use super::ToolHostError;
use crate::egress::leak_provision::{provision_audit_row, provision_failed_audit_row};
use crate::egress::net_worker::EgressSidecar;
use crate::secrets::{collect_refs_in_params, Vault};

/// Outcome of attempting dispatch-time provisioning. Computed without `.await`.
pub(crate) enum Provision {
    /// No egress sidecar, or no scannable secrets in this call — no-op.
    Noop,
    /// The union gained these fingerprints (emit one audit row each — D3).
    Added(Vec<SecretFingerprint>),
    /// Write failed for a secret-bearing net worker — caller fails closed (D1).
    Failed { pending: usize, err: String },
}

/// Decide + perform the file write synchronously. `egress` is the worker's
/// optional sidecar bundle; `req_for_audit` is the pre-substitution params
/// snapshot (so the `secret://` refs are still present). Secrets are
/// fingerprinted via the vault **without exposing plaintext**; sub-`MIN_SECRET_LEN`
/// values yield `None` and are skipped (not a failure — unscannable by design).
pub(crate) fn compute_provision(
    egress: Option<&EgressSidecar>,
    req_for_audit: &serde_json::Value,
    vault: &Vault,
) -> Provision {
    let Some(egress) = egress else {
        return Provision::Noop;
    };
    let refs = collect_refs_in_params(req_for_audit);
    let fps: Vec<SecretFingerprint> = refs
        .iter()
        .filter_map(|r| vault.value_fingerprint(r))
        .collect();
    if fps.is_empty() {
        return Provision::Noop;
    }
    match egress.provision_dispatch_secrets(&fps) {
        Ok(added) => Provision::Added(added),
        Err(e) => Provision::Failed {
            pending: fps.len(),
            err: e.to_string(),
        },
    }
}

/// Emit the audit rows for a provisioning outcome and, on failure, return the
/// fail-closed error (D1). Audit inserts are best-effort (logged, not
/// propagated) — consistent with the other dispatch audit rows — but the
/// fail-closed *decision* is hard: `Failed` always returns `Err`.
pub(crate) async fn emit_provision(
    sink: &dyn AuditSink,
    tool: &str,
    provision: Provision,
) -> Result<(), ToolHostError> {
    match provision {
        Provision::Noop => Ok(()),
        Provision::Added(added) => {
            for fp in &added {
                // No human secret *name* at dispatch — only the one-way value
                // hash. Pass "" for `name`; `worker` + `value_sha256` identify it.
                let row = provision_audit_row(tool, "", fp);
                if let Err(e) = sink.insert(row.actor, &row.action, row.payload).await {
                    tracing::error!(
                        tool = %tool,
                        error = %e,
                        "egress.secret_hash.provisioned audit insert failed"
                    );
                }
            }
            Ok(())
        }
        Provision::Failed { pending, err } => {
            let row = provision_failed_audit_row(tool, pending, &err);
            if let Err(ae) = sink.insert(row.actor, &row.action, row.payload).await {
                tracing::error!(
                    tool = %tool,
                    error = %ae,
                    "egress.secret_hash.provision_failed audit insert failed"
                );
            }
            Err(ToolHostError::EgressProvisionFailed(err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kastellan_db::DbError;
    use kastellan_leak_scan::fingerprint_value;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Records the (actor, action) of every insert; always succeeds.
    #[derive(Default)]
    struct RecordingSink {
        rows: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl AuditSink for RecordingSink {
        async fn insert(&self, actor: &str, action: &str, _payload: Value) -> Result<i64, DbError> {
            self.rows
                .lock()
                .unwrap()
                .push((actor.to_string(), action.to_string()));
            Ok(1)
        }
    }

    #[tokio::test]
    async fn noop_emits_nothing_and_is_ok() {
        let sink = RecordingSink::default();
        emit_provision(&sink, "web-fetch", Provision::Noop)
            .await
            .unwrap();
        assert!(sink.rows.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn added_emits_one_provisioned_row_per_fingerprint() {
        let sink = RecordingSink::default();
        let fps = vec![
            fingerprint_value(b"secret-value-one").unwrap(),
            fingerprint_value(b"secret-value-two").unwrap(),
        ];
        emit_provision(&sink, "web-fetch", Provision::Added(fps))
            .await
            .unwrap();
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|(_, action)| action == "egress.secret_hash.provisioned"));
    }

    #[tokio::test]
    async fn failed_emits_a_failure_row_and_returns_err_d1() {
        let sink = RecordingSink::default();
        let res = emit_provision(
            &sink,
            "web-fetch",
            Provision::Failed {
                pending: 1,
                err: "boom".to_string(),
            },
        )
        .await;
        assert!(matches!(res, Err(ToolHostError::EgressProvisionFailed(_))));
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "egress.secret_hash.provision_failed");
    }
}
```

- [ ] **Step 3: Run the unit tests**

Run: `cargo test -p kastellan-core --lib tool_host::egress_provision -- --nocapture`
Expected: PASS (3 tests). These cover D1 (fail-closed returns `Err`) and D3 (only added rows).

- [ ] **Step 4: Commit**
```bash
git add core/src/tool_host/egress_provision.rs core/src/tool_host.rs
git commit -m "feat(#268): tool_host egress_provision helper (compute + emit, D1/D3 tested)"
```

---

### Task 5: Wire provisioning into the dispatch chokepoint

The actual behaviour change: between successful substitution and `worker.call`.

**Files:**
- Modify: `core/src/tool_host.rs` (`dispatch_with_sink`, between line 299 and line 308)

- [ ] **Step 1: Insert the provisioning call**

In `core/src/tool_host.rs`, in `dispatch_with_sink`, immediately after the `redemption_events` `match` block closes (current line 299, the `};`) and before the comment block at line 301 / the `let cmd = WorkerCommand::new(...)` at line 308, insert:
```rust

    // ── Egress slice #3b (#268): dispatch-time leak-scanner provisioning. ──
    //
    // If this worker has an egress sidecar (a force-routed net worker) and the
    // call carries scannable secret refs, write each secret's value-fingerprint
    // into the sidecar's `secret_hashes.json` BEFORE `worker.call` triggers any
    // egress, so the proxy's per-connection scanner can catch exfiltration.
    // `compute_provision` runs synchronously, releasing the `worker.egress`
    // borrow before `worker.call`; `emit_provision` writes the audit rows and,
    // on a write failure, returns Err — fail CLOSED (D1): a secret never reaches
    // a net worker the scanner cannot watch. No-op for non-net workers
    // (`egress == None`) and for calls with no scannable secrets.
    let provision = egress_provision::compute_provision(worker.egress.as_ref(), &req_for_audit, vault);
    egress_provision::emit_provision(sink, tool, provision).await?;
```

- [ ] **Step 2: Build + clippy**

Run:
```sh
cargo build -p kastellan-core
cargo clippy -p kastellan-core --all-targets -- -D warnings
```
Expected: clean. (If clippy flags `clippy::io_other_error` on the `Error::new(ErrorKind::Other, ...)` from Task 3, switch it to `std::io::Error::other("sidecar uds_path has no parent dir")`.)

- [ ] **Step 3: Run the full core lib test suite**

Run: `cargo test -p kastellan-core --lib -- --nocapture`
Expected: all green (the new units plus the unchanged existing units). The dispatch wiring is exercised end-to-end by Task 6's hermetic e2e and the existing `shell_exec_e2e` (which proves the `egress == None` no-op path stays byte-identical — a non-net worker dispatch).

- [ ] **Step 4: Commit**
```bash
git add core/src/tool_host.rs
git commit -m "feat(#268): wire dispatch-time provisioning into dispatch_with_sink (fail-closed)"
```

---

### Task 6: Hermetic e2e — append format round-trips through the proxy parser

Proves the dispatch-time *append* (union) writes the exact on-disk format the egress proxy reads. Mirrors the existing `egress_leak_scan_e2e.rs` contract style (no sandbox, no PG).

**Files:**
- Modify: `core/tests/egress_leak_scan_e2e.rs`

- [ ] **Step 1: Read the existing file to match its imports/style**

Run: `sed -n '1,40p' core/tests/egress_leak_scan_e2e.rs`
Note how it imports `kastellan_core::egress::leak_provision::*` and `kastellan_leak_scan::*`.

- [ ] **Step 2: Add the failing test**

Append to `core/tests/egress_leak_scan_e2e.rs`:
```rust
/// The dispatch-time append (`merge_secret_hashes`, #268) accumulates the union
/// across calls and writes exactly what the proxy's `parse_hashes` reads back —
/// the same contract the spawn-time `write_secret_hashes` honours.
#[test]
fn dispatch_append_union_round_trips_through_proxy_parser() {
    use kastellan_core::egress::leak_provision::{merge_secret_hashes, SECRET_HASHES_FILE_NAME};
    use kastellan_leak_scan::{fingerprint_value, parse_hashes};

    let dir = tempfile::tempdir().unwrap();
    let a = fingerprint_value(b"dispatch-secret-alpha").unwrap();
    let b = fingerprint_value(b"dispatch-secret-bravo").unwrap();

    // First dispatch provisions `a`; a later dispatch on the same (reused)
    // worker provisions `b` — both must be present (union, decision D2).
    assert_eq!(merge_secret_hashes(dir.path(), &[a.clone()]).unwrap(), vec![a.clone()]);
    assert_eq!(merge_secret_hashes(dir.path(), &[b.clone()]).unwrap(), vec![b.clone()]);

    // The proxy reads the file with the same parser it uses per-connection.
    let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
    let got = parse_hashes(&s);
    assert_eq!(got.len(), 2);
    assert!(got.contains(&a) && got.contains(&b));
}
```
(If `SECRET_HASHES_FILE_NAME` is not re-exported at `kastellan_core::egress::leak_provision::SECRET_HASHES_FILE_NAME`, it is — it is `pub const` in that module; the `use` path above is correct.)

- [ ] **Step 3: Run it**

Run: `cargo test -p kastellan-core --test egress_leak_scan_e2e -- --nocapture`
Expected: PASS (existing tests + the new one).

- [ ] **Step 4: Commit**
```bash
git add core/tests/egress_leak_scan_e2e.rs
git commit -m "test(#268): dispatch-append union round-trips through the proxy parser"
```

---

### Task 7: Full verification + docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full workspace test + clippy (macOS skip-as-pass posture)**

Run:
```sh
cargo test --workspace 2>&1 | tail -20
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
```
Expected: all green; clippy clean. Record the passed/failed/ignored counts.

- [ ] **Step 2: (If a DGX session is available) native-Linux acceptance**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && git fetch && git checkout feat/268-egress-dispatch-time-provisioning && cargo build --workspace && cargo test --workspace 2>&1 | tail -20 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3'`
Expected: native-Linux baseline green (currently 1839/0/15 ± the new tests). This is the real Linux gate; the Mac run is skip-as-pass. If no DGX session this turn, note it in HANDOVER as the carried-forward Linux verification.

- [ ] **Step 3: Update HANDOVER.md**

Per the checklist at the bottom of HANDOVER.md: bump `Last updated`, `Last commit`, the test-count line; move this work into "Recently merged"/"Recently completed"; mark issue #268 wiring done (mechanism live, activates with the first secret-bearing egress worker); fix the stale "Branch `feat/281-gliner-relex-landlock`" phrasing in the header (it merged as PR #295 = `4b42848`); write a fresh "Next TODO". Update the `core/src/egress/leak_provision.rs` description in "Working state" to note the dispatch-time `merge_secret_hashes` + the dispatch wiring.

- [ ] **Step 4: Update ROADMAP.md**

Tick the egress #3b dispatch-time live-append follow-up (#268) with the commit hash; note callers no longer pass `&[]` at dispatch for force-routed net workers (the spawn-time field remains, still `&[]`).

- [ ] **Step 5: Commit docs**
```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): egress #3b dispatch-time provisioning shipped (#268)"
```

- [ ] **Step 6: Push + open PR**
```bash
git push -u origin feat/268-egress-dispatch-time-provisioning
gh pr create --base main --title "feat(#268): egress #3b dispatch-time secret-hash provisioning" --body "$(cat <<'EOF'
Closes #268.

Wires secret-value fingerprints into a force-routed net worker's egress-sidecar
`secret_hashes.json` at **dispatch time** — the moment `tool_host::dispatch`
materializes a `secret://` ref for the worker — so the proxy's credential-leak
scanner (slice #3b) finally has patterns to match. The proxy already lazily
re-reads the file per connection; this closes the §9 deferral.

## How
- Pure `secrets::collect_refs_in_params` recovers which refs a call carries
  (the one-way `RedemptionEvent.ref_hash` can't be reversed).
- `egress::leak_provision::merge_secret_hashes` keeps the **union** of
  fingerprints across worker reuse (dedup by sha256), atomic write.
- `tool_host/egress_provision` computes the outcome synchronously (no borrow
  across `.await`) and emits audit rows.

## Decisions
- **D1 fail-closed:** if the write fails for a secret-bearing net worker, the
  dispatch is refused (worker never egresses unscanned) + a
  `egress.secret_hash.provision_failed` audit row.
- **D2 union** across reused (IdleTimeout) workers.
- **D3 audit** only newly-added fingerprints (`egress.secret_hash.provisioned`).

## Scope
Mechanism + tests. No egress worker carries secrets today, so this activates
automatically with the first secret-bearing egress worker. No-op for non-net
workers and for calls without scannable secrets.

## Tests
- Unit: `collect_refs_in_params` (4), `merge_secret_hashes` + fail-closed row (6),
  `emit_provision` fake-sink D1/D3 (3).
- Hermetic e2e: union append round-trips through the proxy parser.
- Full workspace green + clippy `-D warnings` clean.

Spec: `docs/superpowers/specs/2026-06-16-egress-slice3b-dispatch-time-provisioning-design.md`
Plan: `docs/superpowers/plans/2026-06-16-egress-slice3b-dispatch-time-provisioning.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
(If `git push` from the Mac times out, use the DGX relay: `format-patch | ssh dgx git am` then push from the DGX; `gh pr create` still works from the Mac — see memory `mac-github-push-blocked-relay-via-dgx`.)

---

## Self-review notes

- **Spec coverage:** §4.1→Task 1, §4.3+D2→Task 2, §4.4→Task 3, §4.5+D1/D3→Task 4+5, §6 error handling→Tasks 2/4/5, §7 tests→Tasks 1/2/4/6, §8 files→all tasks. ✓
- **Type consistency:** `collect_refs_in_params`, `merge_secret_hashes(scratch,&[fp])->io::Result<Vec<fp>>`, `provision_dispatch_secrets`, `compute_provision`/`emit_provision`/`Provision`, `ToolHostError::EgressProvisionFailed(String)`, audit actions `egress.secret_hash.provisioned` / `egress.secret_hash.provision_failed` — used identically across tasks. ✓
- **No placeholders:** every code step is complete; the only "read first" step (Task 6 Step 1) is to match import style, with the concrete test still given. ✓
- **Cap watch:** `tool_host.rs` grows by ~14 lines (1 `mod` + ~13 wiring/comment); the bulk lives in the new `egress_provision.rs`. Net `tool_host.rs` ≈ 533. Acceptable per spec §8; the helper module is the mitigation.
