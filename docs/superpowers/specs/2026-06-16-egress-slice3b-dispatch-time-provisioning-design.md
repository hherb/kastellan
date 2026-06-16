# Egress #3b — dispatch-time live provisioning of secret-value hashes

**Issue:** [#268](https://github.com/hherb/kastellan/issues/268)
**Date:** 2026-06-16
**Status:** design approved (decisions below ratified by the operator)
**Mechanism PR this builds on:** [#269](https://github.com/hherb/kastellan/pull/269) (the credential-leak scanner)
**Spec it completes:** `docs/superpowers/specs/2026-06-12-egress-proxy-slice3b-credential-leak-scanner-design.md` §9 (Deferrals)

---

## 1. Problem

Slice #3b shipped the credential-leak scanner in the egress proxy plus
*spawn-time* provisioning: a force-routed net worker's sidecar reads a
`secret_hashes.json` file from its scratch dir and, per CONNECT, scans both
relay directions for any provisioned secret value (Rabin pre-filter →
SHA-256 confirm). On a hit it kills the flow and emits
`egress.blocked.credential_leak`.

The proxy already **lazily re-reads** `secret_hashes.json` on every
connection, so the file can be updated *after* the sidecar starts. But the
spawn path provisions only what it is handed, and every caller passes
`secret_fingerprints: &[]` — because at **spawn** time we do not yet know
which secrets the worker will be given. The result: the scanner runs with an
empty pattern set and protects nothing.

The place we *do* know which secrets a worker will see is the single dispatch
chokepoint, `tool_host::dispatch_with_sink`, where
`secrets::substitute_refs_in_params` replaces `secret://<8-hex>` references in
the call params with plaintext immediately before `worker.call`. The missing
link (issue #268): after substitution, fingerprint each materialized secret
and write it into the sidecar's `secret_hashes.json` **before** the worker
egresses.

### Scope reality

No egress worker carries secrets *today* (web-fetch / web-search /
browser-driver take none), so this change is **mechanism + hermetic tests**;
it activates automatically the first time a secret-bearing egress worker is
configured. The `NetWorkerSpawn` params-struct consolidation issue #268 also
mentions was already completed in slice #4 (PR for TLS pinning), so that part
is out of scope here. The spawn-time `secret_fingerprints` field stays as-is
(harmless, may carry static per-worker secrets later); this work adds the
*dispatch-time* append alongside it.

---

## 2. Key facts that shape the design

- `RedemptionEvent` carries only `ref_hash` (`SHA-256(ref_string)`, one-way).
  It **cannot** be reversed to a `SecretRef`, so we cannot use the redemption
  events to look up fingerprints. We instead re-scan the *pre-substitution*
  params (which `dispatch_with_sink` already snapshots for audit as
  `req_for_audit`) to recover the `SecretRef`s.
- `Vault::value_fingerprint(&SecretRef) -> Option<SecretFingerprint>` already
  exists and fingerprints **in place under the read lock, never exposing
  plaintext**. Returns `None` if the ref is absent, expired, or the value is
  below `MIN_SECRET_LEN` (8 bytes) — i.e. exactly the values the scanner would
  not match anyway.
- A `SupervisedWorker` carries `egress: Option<EgressSidecar>` (`pub(crate)`).
  The scratch dir holding `secret_hashes.json` is always
  `egress.sidecar.uds_path.parent()`. Plain (`Net::Deny` / legacy) workers
  have `egress == None`.
- `dispatch_with_sink` takes `&mut SupervisedWorker` — exclusive access, so a
  worker's `secret_hashes.json` is never written concurrently.
- `write_secret_hashes` (atomic temp-file + rename) and `parse_hashes` /
  `serialize_hashes` already exist and round-trip through the proxy's parser.

---

## 3. Ratified decisions

| # | Decision | Choice |
|---|----------|--------|
| D1 | Provisioning-write failure while a real secret is being handed to a net worker | **Fail closed** — refuse the dispatch with an error + audit row; the worker never egresses with a secret the scanner cannot watch. |
| D2 | Accumulation across reused workers (IdleTimeout lifecycle) | **Union across the worker's lifetime** — read-merge-write, dedup by `sha256`. A later connection carrying an earlier secret is still scanned. The file is the state (stateless in core). |
| D3 | Audit rows | **Only newly-added fingerprints** — one `egress.secret_hash.provisioned` row per fp the merge actually added. Quiet on reuse, full record of exposure. |

Fail-closed (D1) fires **only** on an `io::Error` from the merge/write. A
`None` from `value_fingerprint` (too-short / expired secret) is *not* a
failure — that value is simply unscannable, consistent with the scanner's own
`MIN_SECRET_LEN`, and the dispatch proceeds.

---

## 4. Components

All new logic is pure or thin; each unit is independently testable.

### 4.1 `secrets::collect_refs_in_params(&serde_json::Value) -> Vec<SecretRef>`
New **pure** function (new file `core/src/secrets/collect.rs`, re-exported
from the `secrets` facade). Walks the JSON tree (same shape as
`substitute::walk`), and for every well-formed `secret://<8-hex>` string
constructs a `SecretRef` via `SecretRef::from_raw`. Returns the **dedup'd**
set (dedup by `ref_hash`, preserving first-seen order for deterministic
tests). Reuses `is_well_formed_ref` (lift it to a shared, crate-visible helper
if it is currently private to `substitute.rs`). No vault dependency, no I/O.

### 4.2 `Vault::value_fingerprint` — reused as-is
For each collected ref, `value_fingerprint(&ref)`; keep the `Some(fp)` results.
No change to the vault.

### 4.3 `egress::leak_provision::merge_secret_hashes(scratch, new) -> io::Result<Vec<SecretFingerprint>>`
New function in `leak_provision.rs`:
1. Read `<scratch>/secret_hashes.json` if present; `parse_hashes` it (lenient —
   missing/corrupt ⇒ empty existing set, never an error).
2. Compute the union of existing ∪ `new`, dedup by `sha256`.
3. If the union differs from existing, `write_secret_hashes(scratch, &union)`
   (atomic).
4. Return the **newly-added** fingerprints (those in `new` not already in
   existing), for audit emission (D3).

A write failure surfaces as `Err(io::Error)` (drives D1 fail-closed).

### 4.4 `EgressSidecar::provision_dispatch_secrets(&self, fps: &[SecretFingerprint]) -> io::Result<Vec<SecretFingerprint>>`
New `pub(crate)` method on `EgressSidecar` (keeps the scratch path private to
the egress module). Resolves the dir = `self.sidecar.uds_path.parent()` and
delegates to `merge_secret_hashes`. Returns the newly-added set.

### 4.5 Wiring in `tool_host::dispatch_with_sink`
After `substitute_refs_in_params` succeeds and **before** `worker.call`:

```text
if worker.egress.is_some() {
    let refs = secrets::collect_refs_in_params(&req_for_audit); // pre-substitution snapshot
    if !refs.is_empty() {
        let fps: Vec<_> = refs.iter()
            .filter_map(|r| vault.value_fingerprint(r))
            .collect();
        if !fps.is_empty() {
            match worker.egress.provision_dispatch_secrets(&fps) {
                Ok(added) => emit one egress.secret_hash.provisioned row per `added` (D3),
                Err(e)    => emit a refusal audit row + return Err (D1 fail-closed),
            }
        }
    }
}
```

Notes:
- `req_for_audit` is the existing pre-substitution params snapshot — no extra
  clone.
- No-op when `egress == None` (all non-net workers), when the call carries no
  refs, or when every secret is sub-`MIN_SECRET_LEN`.
- The audit row reuses `provision_audit_row(worker, name, fp)`. We do not have
  a human secret *name* at dispatch (only `ref_hash`), so we pass the tool name
  as `worker` and the `ref_hash` as `name` — both are non-secret identifiers;
  the row's `value_sha256` is the one-way fingerprint hash.

---

## 5. Data flow (end to end)

```
params {…, "token": "secret://deadbeef"}        (planner-supplied)
  │  dispatch_with_sink:
  ├─ snapshot req_for_audit (pre-substitution)   ← collect refs from here
  ├─ substitute_refs_in_params → plaintext in params, RedemptionEvents
  ├─ IF worker.egress.is_some():
  │     collect_refs_in_params(req_for_audit) → [secret://deadbeef]
  │     vault.value_fingerprint(deadbeef) → SecretFingerprint{len,fp64,sha256}
  │     egress.provision_dispatch_secrets([fp])
  │        → merge_secret_hashes(<scratch>, [fp])
  │             read existing ∪ [fp] dedup → write_secret_hashes (atomic)
  │             return newly-added
  │        Ok(added) → audit egress.secret_hash.provisioned ×|added|
  │        Err       → audit refusal + RETURN Err  (fail closed, no egress)
  ├─ worker.call(cmd with plaintext)             ← worker now egresses
  │     proxy per-connection re-reads secret_hashes.json → RollingMatcher
  │     match on outbound bytes → kill flow + egress.blocked.credential_leak
  └─ emit secret.redeemed + tool:<name> rows (unchanged)
```

---

## 6. Error handling

- **Provision write fails (D1):** return a `ToolHostError`; emit an audit row
  (proposed action `policy` / `egress.secret_hash.provision_failed`) carrying
  the worker name and ref_hash (never plaintext). The worker is **not** called.
- **`value_fingerprint` returns `None`:** skip that secret silently (not a
  failure; unscannable by design).
- **Existing `secret_hashes.json` corrupt:** `parse_hashes` returns empty, so
  the merge proceeds with just the new fps (fail-safe — the scanner gets *more*
  patterns, never silently fewer than this dispatch requires).
- **Non-net worker (`egress == None`):** entire block skipped — byte-identical
  behaviour to today.

---

## 7. Testing (TDD — tests first)

### Unit (no PG, no sandbox)
1. `collect_refs_in_params`: nested object/array, dedup of repeated ref,
   non-ref strings ignored, malformed `secret://` ignored, empty params ⇒ `[]`.
2. `merge_secret_hashes`: empty-existing write; union dedups by sha256;
   newly-added return value correct; idempotent re-merge returns empty added;
   corrupt existing file ⇒ treated as empty; atomic (no `.tmp` left behind).
3. `value_fingerprint` interaction: a sub-`MIN_SECRET_LEN` secret yields no fp
   (reuse vault unit coverage; assert the dispatch helper filters `None`).

### Integration — hermetic (`core/tests/egress_leak_scan_e2e.rs` extension or a new sibling)
4. **Dispatch provisions the file:** a force-routed net worker (fake sidecar /
   real `spawn_forced_net_worker` per existing harness), dispatch a call whose
   params carry a `secret://` ref backed by a vault entry; assert the worker's
   `<scratch>/secret_hashes.json` contains that value's sha256 after dispatch
   and **before** the worker would have connected.
5. **Union on reuse:** two dispatches with two different secrets on the same
   worker → the file holds **both** fingerprints.
6. **Fail-closed (D1):** make the scratch unwritable (or point at a bogus dir)
   → dispatch returns `Err`, `worker.call` is **not** reached, a
   `provision_failed` audit row is emitted.
7. **No-op for non-net workers:** dispatch against a `Net::Deny` worker with a
   `secret://` ref → no file written, behaviour unchanged.

### Audit (PG-gated, skip-as-pass)
8. `egress.secret_hash.provisioned` rows appear once per newly-added fp and
   carry `value_sha256` but no plaintext; a repeat dispatch of the same secret
   adds **no** new row (D3).

The existing `egress_leak_scan_e2e.rs` contract tests (file round-trips,
empty=no-scan) stay green unchanged.

---

## 8. Files touched

| File | Change |
|------|--------|
| `core/src/secrets/collect.rs` (new) | pure `collect_refs_in_params` + tests |
| `core/src/secrets/mod.rs` | re-export `collect_refs_in_params`; make `is_well_formed_ref` crate-visible |
| `core/src/egress/leak_provision.rs` | add `merge_secret_hashes` + tests |
| `core/src/egress/net_worker.rs` | add `EgressSidecar::provision_dispatch_secrets` |
| `core/src/tool_host.rs` | wire provisioning into `dispatch_with_sink` (D1/D3) |
| `core/tests/egress_leak_scan_e2e.rs` (or new sibling) | integration tests 4–8 |

Watch the 500-LOC cap on `tool_host.rs` (currently 519, already over by 19 —
keep the wired block tight; if it pushes meaningfully further, lift the
provisioning helper into a small `tool_host` sibling rather than inlining).

---

## 9. Non-goals / deferrals

- Zeroizing the plaintext fanout in params (tracked separately, secrets slice
  2 — unchanged by this work).
- Provisioning for non-force-routed (`--share-net` legacy) net workers — those
  have no sidecar; out of scope and not a regression (they predate the proxy).
- Any change to the proxy side (the lazy re-read + scanner already exist).
- A real secret-bearing egress worker (this enables it; it does not add one).
