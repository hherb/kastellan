# python-exec output secret-scrub Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Redact every secret materialized into a `python-exec` dispatch's params out of the worker's result before it is screened, audited, or shown to the operator — closing the secret-in-param → stdout → audit/JSONL/CLI leak path for the agent-authored-code worker.

**Architecture:** A new pure `redact()` in `kastellan-leak-scan` finds all occurrences of a secret's verbatim bytes in a bounded buffer (reusing the Rabin pre-filter + SHA-256 confirm the streaming matcher already uses) and replaces each with a marker. A new `core/src/tool_host/secret_scrub.rs` sibling (mirroring `egress_provision.rs`) walks the worker's result JSON, scrubs every string leaf against the fingerprints of this dispatch's redeemed secrets (computed via `Vault::value_fingerprint`, no plaintext copy), and emits a redacted `secret.output_scrubbed` audit row. `dispatch_with_sink` calls it on the `Ok(v)` arm, gated to `python-exec` only (default-off for every other worker → byte-identical).

**Tech Stack:** Rust (workspace crates `kastellan-leak-scan`, `kastellan-core`), `sha2`, `serde_json`, `tokio`/`async-trait` for the audit sink.

**Spec:** `docs/superpowers/specs/2026-06-17-python-exec-output-secret-scrub-design.md`

---

## File Structure

- `leak-scan/src/redact.rs` — **create**. Pure `redact(input, &[SecretFingerprint]) -> RedactOutcome` + `RedactHit`/`RedactOutcome` + unit tests.
- `leak-scan/src/lib.rs` — **modify**. Register `mod redact;` + `pub use`.
- `core/src/tool_host/secret_scrub.rs` — **create**. `worker_redacts_output`, `fingerprints_for_dispatch`, `scrub_result_value`, `emit_scrub_audit` + unit tests.
- `core/src/tool_host.rs` — **modify**. Declare `mod secret_scrub;`; call the scrub on the `Ok(v)` arm of `dispatch_with_sink`.
- `core/src/workers/python_exec.rs` — **modify**. Widen `const TOOL_NAME` to `pub(crate)` so the gate can name it.
- `core/tests/cli_memory_l3py_run_daemon_e2e.rs` — **modify**. Add an env-clobber confirming e2e (real jail).

---

## Task 1: `redact` in `kastellan-leak-scan`

**Files:**
- Create: `leak-scan/src/redact.rs`
- Modify: `leak-scan/src/lib.rs:11-17`

- [ ] **Step 1: Write the failing tests**

Create `leak-scan/src/redact.rs` with the test module first (the impl in Step 3 follows). The tests use the public `fingerprint_value`.

```rust
//! Bounded-buffer, all-hits secret redaction.
//!
//! The streaming [`crate::RollingMatcher`] reports the FIRST leak and is built
//! to BLOCK a tunnel. python-exec output is a bounded in-memory buffer that must
//! instead be SCRUBBED in place, so this finds EVERY non-overlapping occurrence
//! of a secret's verbatim bytes and replaces it with a marker. It reuses the
//! same Rabin pre-filter + SHA-256 confirm as the matcher (so detection cannot
//! drift between the two), specialized to a contiguous slice (direct indexing,
//! no ring buffer).

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::fingerprint::{poly, SecretFingerprint, RABIN_BASE};

/// One redacted span: which secret (by SHA-256, hex) and where it sat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedactHit {
    pub sha256_hex: String,
    /// Byte offset of the matched span's first byte in the ORIGINAL input.
    pub offset: usize,
    /// Byte length of the matched (now replaced) span.
    pub len: usize,
}

/// Result of [`redact`]: the rewritten bytes + the spans that were replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedactOutcome {
    pub bytes: Vec<u8>,
    pub hits: Vec<RedactHit>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::fingerprint_value;

    fn fp(v: &[u8]) -> SecretFingerprint {
        fingerprint_value(v).expect("test secret >= MIN_SECRET_LEN")
    }

    fn sha8(v: &[u8]) -> String {
        let d: [u8; 32] = Sha256::digest(v).into();
        let mut s = String::new();
        for b in &d[..4] {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    #[test]
    fn no_patterns_returns_input_unchanged() {
        let out = redact(b"nothing to hide here", &[]);
        assert_eq!(out.bytes, b"nothing to hide here");
        assert!(out.hits.is_empty());
    }

    #[test]
    fn no_match_returns_input_unchanged() {
        let out = redact(b"clean output line", &[fp(b"super-secret-value")]);
        assert_eq!(out.bytes, b"clean output line");
        assert!(out.hits.is_empty());
    }

    #[test]
    fn single_occurrence_is_replaced_with_marker() {
        let secret = b"super-secret-value";
        let out = redact(b"x super-secret-value y", &[fp(secret)]);
        let expect = format!("x [redacted:{}] y", sha8(secret));
        assert_eq!(String::from_utf8(out.bytes).unwrap(), expect);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].offset, 2);
        assert_eq!(out.hits[0].len, secret.len());
    }

    #[test]
    fn multiple_occurrences_all_replaced() {
        let secret = b"super-secret-value";
        let out = redact(b"super-secret-value and super-secret-value", &[fp(secret)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert!(!body.contains("super-secret-value"));
        assert_eq!(body.matches("[redacted:").count(), 2);
        assert_eq!(out.hits.len(), 2);
    }

    #[test]
    fn adjacent_occurrences_both_replaced() {
        let secret = b"super-secret-value";
        let mut input = secret.to_vec();
        input.extend_from_slice(secret);
        let out = redact(&input, &[fp(secret)]);
        assert!(!String::from_utf8(out.bytes).unwrap().contains("super-secret-value"));
        assert_eq!(out.hits.len(), 2);
    }

    #[test]
    fn match_at_start_and_at_end() {
        let secret = b"super-secret-value";
        let mut input = secret.to_vec();
        input.extend_from_slice(b" mid ");
        input.extend_from_slice(secret);
        let out = redact(&input, &[fp(secret)]);
        assert_eq!(out.hits.len(), 2);
        assert_eq!(out.hits[0].offset, 0);
    }

    #[test]
    fn two_secrets_different_lengths_both_redacted() {
        let short = b"short-one"; // len 9
        let long = b"a-much-longer-secret-string"; // len 27
        let input = b"..short-one..a-much-longer-secret-string..";
        let out = redact(input, &[fp(short), fp(long)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert!(!body.contains("short-one"));
        assert!(!body.contains("a-much-longer-secret-string"));
        assert_eq!(out.hits.len(), 2);
    }

    #[test]
    fn overlapping_candidates_resolve_earliest_longest() {
        // "abcdefghij" contains "abcdefgh" (len 8) and "cdefghij" (len 8);
        // the earliest non-overlapping match wins and scanning resumes past it.
        let a = b"abcdefgh";
        let b = b"cdefghij";
        let out = redact(b"abcdefghij", &[fp(a), fp(b)]);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].offset, 0);
        assert_eq!(out.hits[0].sha256_hex.len(), 64);
    }

    #[test]
    fn marker_carries_first_8_hex_of_sha256() {
        let secret = b"super-secret-value";
        let out = redact(secret, &[fp(secret)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(body, format!("[redacted:{}]", sha8(secret)));
    }

    #[test]
    fn sub_min_len_value_is_never_fingerprinted_so_never_redacted() {
        // 7 bytes < MIN_SECRET_LEN: fingerprint_value returns None, so it can
        // never be a pattern and "secret7" stays in the output.
        assert!(fingerprint_value(b"secret7").is_none());
        let out = redact(b"leak secret7 here", &[]);
        assert_eq!(out.bytes, b"leak secret7 here");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-leak-scan redact 2>&1 | tail -20
```
Expected: FAIL — `cannot find function redact in this scope` (impl not written yet).

- [ ] **Step 3: Write the implementation**

Insert this ABOVE the `#[cfg(test)] mod tests` block in `leak-scan/src/redact.rs`:

```rust
/// `RABIN_BASE^(len-1)`, wrapping. `len >= 1` (callers filter `len == 0`).
fn pow_base(len: usize) -> u64 {
    let mut p = 1u64;
    for _ in 0..len.saturating_sub(1) {
        p = p.wrapping_mul(RABIN_BASE);
    }
    p
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The replacement written in place of a matched span. Carries the first 8 hex
/// chars of the secret's SHA-256 so a redaction correlates to the matching
/// `secret.redeemed` audit row WITHOUT leaking any plaintext.
fn marker(sha256_hex: &str) -> Vec<u8> {
    format!("[redacted:{}]", &sha256_hex[..8]).into_bytes()
}

/// Find every non-overlapping occurrence of any `patterns` value in `input` and
/// replace it with [`marker`]. Earliest match wins; on equal start the longer
/// span wins; scanning resumes past a chosen span. Empty `patterns` (or none
/// matching) returns `input` unchanged with no hits. Bounded full-buffer scan:
/// O(input.len()) per distinct pattern length.
pub fn redact(input: &[u8], patterns: &[SecretFingerprint]) -> RedactOutcome {
    // Group target SHA-256s by (len, fp64), skipping patterns longer than the
    // input (they cannot match) and any defensive len == 0.
    let mut by_len: HashMap<usize, HashMap<u64, Vec<[u8; 32]>>> = HashMap::new();
    for p in patterns
        .iter()
        .filter(|p| p.len > 0 && p.len <= input.len())
    {
        by_len
            .entry(p.len)
            .or_default()
            .entry(p.fp64)
            .or_default()
            .push(p.sha256);
    }

    // Collect all confirmed (offset, len, sha256) hits with a per-length rolling
    // Rabin scan (cheap pre-filter) confirmed by SHA-256 (eliminates collisions).
    let mut raw: Vec<(usize, usize, [u8; 32])> = Vec::new();
    for (len, targets) in &by_len {
        let len = *len;
        let pow = pow_base(len);
        let mut cur = poly(&input[0..len]);
        let mut i = 0usize;
        loop {
            if let Some(shas) = targets.get(&cur) {
                let digest: [u8; 32] = Sha256::digest(&input[i..i + len]).into();
                if shas.contains(&digest) {
                    raw.push((i, len, digest));
                }
            }
            if i + len >= input.len() {
                break;
            }
            // Roll the window forward one byte: drop input[i], add input[i+len].
            let out = input[i] as u64;
            cur = cur
                .wrapping_sub(out.wrapping_mul(pow))
                .wrapping_mul(RABIN_BASE)
                .wrapping_add(input[i + len] as u64);
            i += 1;
        }
    }

    // Resolve overlaps: earliest start first, longer span first on a tie; then
    // greedily keep non-overlapping spans.
    raw.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut next_free = 0usize;
    let mut chosen: Vec<(usize, usize, [u8; 32])> = Vec::new();
    for (off, len, sha) in raw {
        if off >= next_free {
            next_free = off + len;
            chosen.push((off, len, sha));
        }
    }

    if chosen.is_empty() {
        return RedactOutcome {
            bytes: input.to_vec(),
            hits: Vec::new(),
        };
    }

    // Splice the markers in, recording one RedactHit per replaced span.
    let mut bytes = Vec::with_capacity(input.len());
    let mut hits = Vec::with_capacity(chosen.len());
    let mut cursor = 0usize;
    for (off, len, sha) in chosen {
        bytes.extend_from_slice(&input[cursor..off]);
        let sha256_hex = hex(&sha);
        bytes.extend_from_slice(&marker(&sha256_hex));
        hits.push(RedactHit {
            sha256_hex,
            offset: off,
            len,
        });
        cursor = off + len;
    }
    bytes.extend_from_slice(&input[cursor..]);
    RedactOutcome { bytes, hits }
}
```

- [ ] **Step 4: Register the module**

In `leak-scan/src/lib.rs`, add `mod redact;` after `mod matcher;` (line 12) and extend the `pub use` block:

```rust
mod fingerprint;
mod matcher;
mod redact;
mod wire;

pub use fingerprint::{fingerprint_value, SecretFingerprint, MAX_SECRET_LEN, MIN_SECRET_LEN};
pub use matcher::{LeakHit, RollingMatcher};
pub use redact::{redact, RedactHit, RedactOutcome};
pub use wire::{parse_hashes, serialize_hashes};
```

- [ ] **Step 5: Run the tests to verify they pass**

```sh
cargo test -p kastellan-leak-scan 2>&1 | tail -20
cargo clippy -p kastellan-leak-scan --all-targets -- -D warnings 2>&1 | tail -5
```
Expected: all tests PASS; clippy clean.

- [ ] **Step 6: Commit**

```sh
git add leak-scan/src/redact.rs leak-scan/src/lib.rs
git commit -m "feat(leak-scan): bounded-buffer all-hits redact() for output scrubbing

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `secret_scrub.rs` in core (pure scrub + gate + audit)

**Files:**
- Create: `core/src/tool_host/secret_scrub.rs`
- Modify: `core/src/workers/python_exec.rs:29` (widen `TOOL_NAME` to `pub(crate)`)
- Modify: `core/src/tool_host.rs:22` (declare `mod secret_scrub;`)

- [ ] **Step 1: Widen `TOOL_NAME`**

In `core/src/workers/python_exec.rs`, change line 29 from:

```rust
const TOOL_NAME: &str = "python-exec";
```
to:
```rust
pub(crate) const TOOL_NAME: &str = "python-exec";
```

- [ ] **Step 2: Write `secret_scrub.rs` with failing tests**

Create `core/src/tool_host/secret_scrub.rs`:

```rust
//! python-exec result secret-scrub (design 2026-06-17).
//!
//! `python-exec` runs agent-authored code, so — unlike the curated Rust workers
//! whose result-plaintext is trusted by design (#147) — we do NOT trust its
//! output to handle a materialized secret responsibly. It is `Net::Deny`, so its
//! returned stdout/stderr is its only output channel (the direct analog of
//! egress). We scan that output for the fingerprints of the secrets materialized
//! into THIS dispatch's params and redact them before the result is screened,
//! audited, or shown to the operator. Symmetric with egress slice #3b, which
//! scans force-routed net workers' egress.
//!
//! Pulled into a sibling (like `egress_provision.rs`) so `tool_host.rs` stays
//! near the 500-LOC cap and the pure pieces are unit-testable with a fake sink.

use kastellan_leak_scan::{redact, RedactHit, SecretFingerprint};
use serde_json::Value;

use super::audit_sink::AuditSink;
use crate::secrets::{collect_refs_in_params, Vault};

/// True iff `tool`'s result must be scrubbed of materialized-secret plaintext.
/// Only `python-exec` opts in (it runs agent-authored code). The dispatch
/// chokepoint only carries the tool name, and there is exactly one such worker
/// today, so the gate keys on the name rather than threading a manifest flag
/// through the dispatch signature (YAGNI; revisit if a second untrusted-code
/// worker appears).
pub(crate) fn worker_redacts_output(tool: &str) -> bool {
    tool == crate::workers::python_exec::TOOL_NAME
}

/// Fingerprints of every scannable secret materialized into this dispatch.
/// `req_for_audit` is the PRE-substitution snapshot, so its `secret://` refs are
/// still present. `Vault::value_fingerprint` reads under the vault lock and
/// never exposes plaintext; values below `MIN_SECRET_LEN` yield `None` and are
/// skipped (unscannable by design — same limit as egress #3b).
pub(crate) fn fingerprints_for_dispatch(
    req_for_audit: &Value,
    vault: &Vault,
) -> Vec<SecretFingerprint> {
    collect_refs_in_params(req_for_audit)
        .iter()
        .filter_map(|r| vault.value_fingerprint(r))
        .collect()
}

/// Walk every JSON string leaf of `result` and redact any occurrence of the
/// `fps` secrets in place, returning the hits accumulated across all leaves.
/// Pure (no I/O). A no-op (and no allocation churn beyond the walk) when `fps`
/// is empty or nothing matches.
pub(crate) fn scrub_result_value(result: &mut Value, fps: &[SecretFingerprint]) -> Vec<RedactHit> {
    let mut hits = Vec::new();
    scrub_value(result, fps, &mut hits);
    hits
}

fn scrub_value(v: &mut Value, fps: &[SecretFingerprint], hits: &mut Vec<RedactHit>) {
    match v {
        Value::String(s) => {
            let outcome = redact(s.as_bytes(), fps);
            if !outcome.hits.is_empty() {
                // The input was valid UTF-8 and the marker is ASCII, so the
                // redacted bytes are valid UTF-8; the lossy fallback is purely
                // defensive and never expected to fire.
                *s = String::from_utf8(outcome.bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                hits.extend(outcome.hits);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|e| scrub_value(e, fps, hits)),
        Value::Object(o) => o.values_mut().for_each(|e| scrub_value(e, fps, hits)),
        _ => {}
    }
}

/// Emit one redacted `policy / secret.output_scrubbed` audit row when a scrub
/// removed at least one secret. Records hash/offset/len only — NEVER plaintext —
/// symmetric with the egress `egress.blocked.credential_leak` row. Best-effort
/// (logged, not propagated): the result is already redacted, so a transient
/// audit failure must not fail the dispatch (consistent with `secret.redeemed`).
pub(crate) async fn emit_scrub_audit(sink: &dyn AuditSink, tool: &str, hits: &[RedactHit]) {
    if hits.is_empty() {
        return;
    }
    let payload = serde_json::json!({
        "tool":  tool,
        "count": hits.len(),
        "hits":  hits
            .iter()
            .map(|h| serde_json::json!({
                "sha256_hex": h.sha256_hex,
                "offset":     h.offset,
                "len":        h.len,
            }))
            .collect::<Vec<_>>(),
    });
    if let Err(e) = sink.insert("policy", "secret.output_scrubbed", payload).await {
        tracing::error!(tool = %tool, error = %e, "secret.output_scrubbed audit insert failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_db::DbError;
    use std::sync::Mutex;

    fn fp(v: &[u8]) -> SecretFingerprint {
        kastellan_leak_scan::fingerprint_value(v).expect("test secret >= MIN_SECRET_LEN")
    }

    /// Records every (actor, action) inserted; always succeeds.
    #[derive(Default)]
    struct RecordingSink {
        rows: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl AuditSink for RecordingSink {
        async fn insert(&self, actor: &str, action: &str, _payload: Value) -> Result<i64, DbError> {
            self.rows
                .lock()
                .unwrap()
                .push((actor.to_string(), action.to_string()));
            Ok(1)
        }
    }

    #[test]
    fn gate_is_on_only_for_python_exec() {
        assert!(worker_redacts_output("python-exec"));
        assert!(!worker_redacts_output("web-fetch"));
        assert!(!worker_redacts_output("shell-exec"));
    }

    #[test]
    fn scrubs_secret_in_stdout_leaf() {
        let secret = b"super-secret-token-123";
        let mut v = serde_json::json!({
            "exit_code": 0,
            "stdout": "leak: super-secret-token-123 done",
            "stderr": "",
        });
        let hits = scrub_result_value(&mut v, &[fp(secret)]);
        assert_eq!(hits.len(), 1);
        let stdout = v["stdout"].as_str().unwrap();
        assert!(!stdout.contains("super-secret-token-123"));
        assert!(stdout.contains("[redacted:"));
    }

    #[test]
    fn scrubs_secret_in_nested_string() {
        let secret = b"super-secret-token-123";
        let mut v = serde_json::json!({
            "nested": {"list": ["x", "super-secret-token-123"]},
        });
        let hits = scrub_result_value(&mut v, &[fp(secret)]);
        assert_eq!(hits.len(), 1);
        assert!(!serde_json::to_string(&v).unwrap().contains("super-secret-token-123"));
    }

    #[test]
    fn no_secret_leaves_value_byte_identical() {
        let mut v = serde_json::json!({"exit_code": 0, "stdout": "clean", "stderr": ""});
        let before = v.clone();
        let hits = scrub_result_value(&mut v, &[fp(b"super-secret-token-123")]);
        assert!(hits.is_empty());
        assert_eq!(v, before);
    }

    #[test]
    fn empty_fingerprints_is_a_noop() {
        let mut v = serde_json::json!({"stdout": "anything at all"});
        let before = v.clone();
        let hits = scrub_result_value(&mut v, &[]);
        assert!(hits.is_empty());
        assert_eq!(v, before);
    }

    #[tokio::test]
    async fn emit_writes_one_policy_row_on_hits_and_nothing_when_empty() {
        let sink = RecordingSink::default();
        emit_scrub_audit(&sink, "python-exec", &[]).await;
        assert!(sink.rows.lock().unwrap().is_empty());

        let hits = vec![RedactHit {
            sha256_hex: "ab".repeat(32),
            offset: 3,
            len: 22,
        }];
        emit_scrub_audit(&sink, "python-exec", &hits).await;
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], ("policy".to_string(), "secret.output_scrubbed".to_string()));
    }
}
```

- [ ] **Step 3: Declare the module**

In `core/src/tool_host.rs`, add `mod secret_scrub;` next to the existing submodule declarations (after `mod egress_provision;` at line 22):

```rust
mod egress_provision;
mod secret_scrub;
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-core --lib secret_scrub 2>&1 | tail -25
```
Expected: PASS for `gate_is_on_only_for_python_exec`, `scrubs_secret_in_stdout_leaf`, `scrubs_secret_in_nested_string`, `no_secret_leaves_value_byte_identical`, `empty_fingerprints_is_a_noop`, `emit_writes_one_policy_row_on_hits_and_nothing_when_empty`.

> If `kastellan_db::DbError` is not the type the `AuditSink::insert` signature uses, copy the exact error type and `async_trait` usage from the `RecordingSink` in `core/src/tool_host/egress_provision.rs`'s `#[cfg(test)] mod tests`.

- [ ] **Step 5: Commit**

```sh
git add core/src/tool_host/secret_scrub.rs core/src/tool_host.rs core/src/workers/python_exec.rs
git commit -m "feat(tool_host): secret_scrub helper — scrub materialized secrets from python-exec output

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Wire the scrub into `dispatch_with_sink`

**Files:**
- Modify: `core/src/tool_host.rs` (the `Ok(v)` arm of the `match call_result`, ~line 333-345)

- [ ] **Step 1: Edit the `Ok` arm**

In `dispatch_with_sink`, find the prompt-injection-screen match. Change the arm head from `Ok(v) => {` to `Ok(mut v) => {` and insert the scrub block BEFORE the `extract_scannable_text` call:

```rust
    let (final_result, blocked_meta) = match call_result {
        Ok(mut v) => {
            // ── python-exec output secret-scrub (design 2026-06-17). ──
            // For a worker that runs agent-authored code, redact every secret
            // materialized into THIS dispatch's params out of the result before
            // it is screened, audited (tool row + JSONL mirror), or returned to
            // the operator's InvokeReport. No-op (byte-identical) for every other
            // worker and for any call with no scannable secrets. `req_for_audit`
            // is the pre-substitution snapshot, so its `secret://` refs are still
            // present for fingerprinting.
            if secret_scrub::worker_redacts_output(tool) {
                let fps = secret_scrub::fingerprints_for_dispatch(&req_for_audit, vault);
                if !fps.is_empty() {
                    let hits = secret_scrub::scrub_result_value(&mut v, &fps);
                    secret_scrub::emit_scrub_audit(sink, tool, &hits).await;
                }
            }

            let (body, truncated) = crate::cassandra::injection_guard::extract_scannable_text(
                &v,
                crate::cassandra::injection_guard::SCAN_BYTE_CAP,
            );
            // ... rest of the arm unchanged ...
```

Leave everything from `let verdict = ...` onward exactly as it is.

- [ ] **Step 2: Build and verify the no-op invariant**

```sh
cargo build -p kastellan-core 2>&1 | tail -10
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```
Expected: builds clean; clippy clean.

- [ ] **Step 3: Run the cross-platform dispatch round-trip (proves no behavior change for non-python-exec)**

```sh
# Requires a sandbox; skip-as-pass if unavailable. Proves shell-exec dispatch is byte-identical (gate off).
cargo test -p kastellan-core --test shell_exec_e2e 2>&1 | tail -15
```
Expected: PASS (or `[SKIP]` lines if no sandbox — still green).

- [ ] **Step 4: Commit**

```sh
git add core/src/tool_host.rs
git commit -m "feat(tool_host): scrub python-exec result secrets before screen/audit/return

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Confirming env-clobber e2e (real jail)

The secret-scrub logic is fully covered hermetically by Tasks 1-2; the substitution→worker plumbing for a *real* materialized secret through the daemon is deferred (see the pre-existing `TODO(params-e2e)` at `core/tests/cli_memory_l3py_run_daemon_e2e.rs:613` — it needs a vault harness). This task instead pins the OTHER half of the "battle-test the params passthrough" follow-up: that a param can never become (or clobber) an env var in the python-exec child.

**Files:**
- Modify: `core/tests/cli_memory_l3py_run_daemon_e2e.rs`

- [ ] **Step 1: Add the skill factory**

Next to `param_echo_skill()` (~line 263), add:

```rust
/// A Python skill that prints the SORTED keys of its own process environment,
/// so the test can pin EXACTLY which env vars the python-exec child sees —
/// proving runtime params (JSON inside `KASTELLAN_PYTHON_PARAMS`) never become
/// env vars and that no host lockdown env var (`KASTELLAN_LANDLOCK_*`, `PATH`,
/// `KASTELLAN_PYTHON_EXEC_PYTHON`, …) leaks into the child.
fn env_keys_skill() -> PythonSkillCandidate {
    PythonSkillCandidate {
        name: "env_keys_py".into(),
        description: "Print the python-exec child's environment keys".into(),
        code: "import os\nprint('ENVKEYS:' + ','.join(sorted(os.environ.keys())))\n".into(),
    }
}
```

- [ ] **Step 2: Add the failing test**

After `python_skill_params_round_trip_through_jail` (~line 619), add (this mirrors that test's harness exactly, changing only the skill, the params, and the assertion):

```rust
// ---------------------------------------------------------------------------
// The python-exec child env is clobber-proof: runtime params are JSON inside
// KASTELLAN_PYTHON_PARAMS, never separate env vars, and the worker's env_clear()
// keeps every host lockdown var out of the child. We pass params named like
// dangerous env vars (`path`, `ld_preload`) and assert the child's env keys are
// EXACTLY {HOME, KASTELLAN_PYTHON_PARAMS, TMPDIR}.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_exec_child_env_is_clobber_proof() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if missing_prereqs(true) {
        return;
    }
    let Some(python) = find_python() else {
        return;
    };
    let worker_bin = workspace_target_binary("kastellan-worker-python-exec");

    let suffix = unique_suffix();
    let user = current_username();
    let cluster = tokio::task::block_in_place(|| cluster_for(&suffix));

    let pool = prepare_db(&cluster).await;
    let id = seed_and_approve_skill(&pool, &env_keys_skill(), &cluster.data_dir, &user).await;

    let mock = spawn_inert_mock().await;
    let (daemon, _daemon_guards) = bring_up_daemon(
        &suffix,
        &cluster.data_dir,
        &mock.base_url,
        &user,
        &worker_bin,
        &python,
        true, // python-exec enabled
    );

    let output = Command::new(cli_binary())
        .args([
            "memory",
            "l3",
            "run",
            &id.to_string(),
            "--param",
            "path=/evil/bin",
            "--param",
            "ld_preload=/evil.so",
            "--execute",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", &user)
        .env("KASTELLAN_DATA_DIR", cluster.data_dir.to_string_lossy().as_ref())
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run env_keys --execute");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "env-keys run must exit 0; got {:?}\n--- CLI stdout ---\n{}\n--- CLI stderr ---\n{}\n\
         --- daemon stdout ({}) ---\n{}\n--- daemon stderr ({}) ---\n{}\n",
        output.status,
        stdout,
        stderr,
        daemon.stdout_path.display(),
        std::fs::read_to_string(&daemon.stdout_path).unwrap_or_default(),
        daemon.stderr_path.display(),
        std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
    );
    // env_clear() leaves exactly TMPDIR + HOME + KASTELLAN_PYTHON_PARAMS; the
    // `path`/`ld_preload` params live INSIDE KASTELLAN_PYTHON_PARAMS as JSON keys,
    // not as env vars, so they never appear here.
    assert!(
        stdout.contains("ENVKEYS:HOME,KASTELLAN_PYTHON_PARAMS,TMPDIR"),
        "python-exec child env must be exactly {{HOME, KASTELLAN_PYTHON_PARAMS, TMPDIR}}; got:\n{stdout}",
    );

    pool.close().await;
    drop(cluster);
}
```

- [ ] **Step 3: Run the test**

```sh
# macOS dev box (Seatbelt + live PG via the session-local PG override) or DGX (bwrap).
cargo test -p kastellan-core --test cli_memory_l3py_run_daemon_e2e python_exec_child_env_is_clobber_proof -- --nocapture 2>&1 | tail -30
```
Expected: PASS, or an early `return` (skip-as-pass) if the sandbox/PG/python prereqs are missing — both are green. If it runs, the `ENVKEYS:HOME,KASTELLAN_PYTHON_PARAMS,TMPDIR` assertion must hold.

> If the env key set differs on a given host (e.g. the worker sets an extra var), that is a real finding — record the actual key set and confirm none of them is operator-supplied or a leaked host secret before adjusting the assertion.

- [ ] **Step 4: Commit**

```sh
git add core/tests/cli_memory_l3py_run_daemon_e2e.rs
git commit -m "test(python-exec): pin child env is clobber-proof (params never become env vars)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Full verification, docs, PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full workspace test + clippy**

```sh
source "$HOME/.cargo/env"
# macOS: bring up the session-local PG (see memory note postgres-app-bin-paths) for the live-PG suites,
# or skip-as-pass per the standing macOS test-infra gotcha.
cargo test --workspace 2>&1 | tail -30
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
```
Expected: all green (record passed/failed/ignored/`[SKIP]` counts). No new failures vs the 1859/0/13 baseline; +~16 new unit tests (Task 1: 10, Task 2: 6) and +1 e2e.

- [ ] **Step 2: DGX native-Linux check (if reachable)**

The change is a pure result-value transform after the worker returns — it does not touch sandbox/seccomp/Landlock. A DGX run is belt-and-braces; run it if convenient (per the memory note, drive as `ssh dgx '<cmd>'`):

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --workspace && cargo test -p kastellan-core --test cli_memory_l3py_run_daemon_e2e python_exec_child_env_is_clobber_proof 2>&1 | tail -20'
```

- [ ] **Step 3: Update HANDOVER.md + ROADMAP.md**

- HANDOVER header: move #268 reconciliation context aside; add a "Recently completed (this session)" block for the python-exec output secret-scrub (files, the gate rationale, the deferred real-secret daemon e2e, test-count delta). Refresh the `core/src/tool_host/` line in "Working state" to mention `secret_scrub.rs` + the `kastellan-leak-scan` line to mention `redact`. Write a fresh "Next TODO".
- ROADMAP: tick the Phase-4 "battle-test the params free-form passthrough" follow-up with the commit hash; note the deferred real-secret daemon e2e and the `<8-byte` accepted limitation.

- [ ] **Step 4: Commit docs**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): python-exec output secret-scrub shipped

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Push + open PR**

```sh
git push -u origin feat/python-exec-output-secret-scrub
gh pr create --base main --title "feat: scrub materialized secrets from python-exec output (params battle-test)" --body "$(cat <<'EOF'
Closes the Phase-4 "battle-test the runtime-params free-form passthrough" follow-up.

python-exec runs agent-authored code, so a `secret://` param materialized to
plaintext could surface (via the approved code's stdout) in the audit_log, the
JSONL mirror, and the operator's InvokeReport — python-exec is `Net::Deny`, so the
egress #3b leak scanner never runs on it. This treats its returned stdout/stderr
as its only output channel and scrubs the fingerprints of this dispatch's
materialized secrets out of the result before it is screened/audited/returned.

- `kastellan-leak-scan::redact` — bounded-buffer all-hits redaction (reuses the
  Rabin + SHA-256 detection; symmetric with the streaming matcher).
- `core/src/tool_host/secret_scrub.rs` — scrub the result JSON's string leaves
  against `Vault::value_fingerprint` (no plaintext copy); emit a redacted
  `secret.output_scrubbed` audit row (hash/offset/len only).
- Gated to `python-exec` only → every other worker byte-identical (`shell_exec_e2e`).
- Confirming e2e: the python-exec child env is exactly {HOME, KASTELLAN_PYTHON_PARAMS, TMPDIR}.

Deferred: the full real-secret daemon round-trip e2e (needs a vault harness — see
the pre-existing TODO in cli_memory_l3py_run_daemon_e2e.rs). Accepted limitation:
secrets < 8 bytes are unscannable (same as egress #3b).

Spec: docs/superpowers/specs/2026-06-17-python-exec-output-secret-scrub-design.md
Plan: docs/superpowers/plans/2026-06-17-python-exec-output-secret-scrub.md

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

> If `git push` from the Mac times out (github firewalled), use the DGX relay: `git format-patch origin/main --stdout | ssh dgx 'cd ~/src/kastellan && git am && git push'`, then `gh pr create` from the Mac (per the memory note `mac-github-push-blocked-relay-via-dgx`).

---

## Self-Review

**Spec coverage:**
- Goal 1 (no plaintext in result/audit/InvokeReport) → Tasks 1-3 (redact + scrub + wire-before-audit). ✓
- Goal 2 (reuse fingerprint machinery, no 2nd plaintext copy) → `fingerprints_for_dispatch` via `value_fingerprint`. ✓
- Goal 3 (zero change for other workers) → gate in Task 2 + `shell_exec_e2e` in Task 3 Step 3. ✓
- Goal 4 (forensic audit row) → `emit_scrub_audit` Task 2 + test. ✓
- `redact` module (component 1) → Task 1. ✓
- `secret_scrub.rs` (component 2) → Task 2. ✓
- opt-in gate (component 3) → Task 2 `worker_redacts_output`. ✓
- dispatch wiring (component 4) → Task 3. ✓
- Accepted limitation `<8 bytes` → covered by `fingerprint_value` returning None (Task 1 test `sub_min_len_value_is_never_fingerprinted...`) + documented in PR/ROADMAP. ✓
- Test plan (leak-scan unit, core unit, e2e) → Tasks 1, 2, 4. The full daemon secret round-trip is explicitly deferred (matches the spec's "headline e2e" being downgraded to the dispatch-layer/hermetic coverage because the #16 worker seal blocks a fake echoing worker and the daemon vault harness is not yet wired — pre-existing TODO at line 613).

**Placeholder scan:** No TBD/TODO-in-steps; every code step shows complete code. The one `TODO(params-e2e)` reference is to a *pre-existing* code comment, not a plan gap.

**Type consistency:** `RedactHit{sha256_hex, offset, len}` and `RedactOutcome{bytes, hits}` defined in Task 1 and used identically in Task 2. `worker_redacts_output`/`fingerprints_for_dispatch`/`scrub_result_value`/`emit_scrub_audit` signatures defined in Task 2 match the call site in Task 3. `emit_scrub_audit(&dyn AuditSink, &str, &[RedactHit])` matches `dispatch_with_sink`'s `sink: &dyn AuditSink`. `python_exec::TOOL_NAME` widened to `pub(crate)` in Task 2 Step 1 before its use in `worker_redacts_output`.
