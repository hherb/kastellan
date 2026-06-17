# Design: python-exec output secret-scrub (params battle-test follow-up)

**Date:** 2026-06-17
**Status:** Approved (brainstorm), pre-plan
**Phase:** 4 (python-exec arc) — the recorded "battle-test the runtime-params
free-form passthrough for risk slip-throughs" follow-up.

## Problem

L3 Python skills (`python-exec` worker) accept free-form runtime params via the
env-var channel (`KASTELLAN_PYTHON_PARAMS`, 64 KiB, shipped in `02ccb57`). A
param value may be a `secret://<hex>` reference; at dispatch `tool_host`
substitutes it to **plaintext** before the params JSON reaches the worker.

`python-exec` runs **agent-authored** Python (SHA-256-pinned + operator-approved,
but *not* curated like the Rust workers). If the approved code echoes the secret
— e.g. `print(os.environ['KASTELLAN_PYTHON_PARAMS'])` — the plaintext flows back
in the worker's RPC result `{exit_code, stdout, stderr}` and lands, **unredacted**,
in:

- `audit_log.payload.result.stdout` (when the serialized tool-row payload is
  ≤ 4 KiB; larger payloads are hash-enveloped by `db::audit`, which only
  obscures), then mirrored to the persistent daily JSONL under
  `~/.local/state/kastellan/`;
- the operator-visible `InvokeReport` printed by `kastellan-cli memory l3 run --execute`.

`python-exec` is `Net::Deny`, so the egress slice-#3b leak scanner **never runs**
on it (it only scans force-routed net workers' egress). The injection guard runs
on the result but is a *blocker*, not a secret redactor. So there is no existing
control on this path.

### Why this is in scope (vs the #147 "authorised consumer" posture)

The #147 privacy invariant is deliberately scoped to `actor='policy'` audit rows;
`secret_vault_e2e` Test 7 documents that a *tool row's* `payload.result` may legitimately
carry plaintext because "the worker is the authorised consumer." That posture is
correct for **curated** Rust workers. `python-exec` is the exception: it executes
LLM-authored code, so we do **not** extend that trust to its output. For a
`Net::Deny` worker the **returned stdout/stderr is its only output channel** — the
direct analog of egress — so the symmetric control is to scan that output for the
secrets materialized into this dispatch's params, exactly as #3b scans egress.

`req_for_audit` is already snapshotted **pre-substitution** ([tool_host.rs:255](../../../core/src/tool_host.rs#L255)),
so the request side logs `secret://` refs, not plaintext. This design closes the
**result** side for `python-exec` only.

## Goals

1. A materialized secret's bytes never surface as plaintext in `python-exec`'s
   result, the `audit_log`/JSONL mirror, or the operator-visible `InvokeReport`.
2. Reuse the existing fingerprint machinery (`kastellan-leak-scan`,
   `Vault::value_fingerprint`) — never hold a second plaintext copy for the scrub.
3. **Zero behavior change for every other worker** (byte-identical;
   `shell_exec_e2e` proves the no-op).
4. Forensic visibility: a redacted audit row records that a scrub happened
   (hash/offset/count only, never plaintext).

## Non-goals

- Redacting curated Rust workers' results (web-fetch/web-search/shell-exec) — out
  of scope; their result-plaintext stays trusted per #147.
- Blocking-instead-of-redacting (the considered "fail-closed" variant) — rejected;
  redaction preserves the rest of the useful output.
- Screening param *values* for prompt-injection on input — for a `Net::Deny`,
  fixed-approved-code worker an injection payload in a param can do nothing, and
  the worker's output is already injection-screened.
- Catching secrets shorter than `MIN_SECRET_LEN` (8 bytes) — same accepted limit
  as #3b; trivially-short values are not real credentials.

## Approach (selected: A — pure `redact()` in `kastellan-leak-scan`)

`RollingMatcher` is streaming / first-hit / block-oriented. python-exec output is
a **bounded buffer** (≤ 256 KiB, capped by the worker) needing **all hits +
in-place replacement**. Add a sibling pure function in the crate that already owns
secret-byte detection, reusing `fingerprint::poly` + the SHA-256 confirm.

Rejected alternatives: (B) extend `RollingMatcher` to all-hits — couples a
streaming type to a buffer use-case and grows core; (C) direct plaintext-substring
search in core — simplest but holds a 2nd plaintext copy and abandons the
fingerprint symmetry.

## Components

### 1. `kastellan-leak-scan`: new `redact` module (pure)

```rust
/// One redaction span found in the input.
pub struct RedactHit { pub sha256: [u8; 32], pub offset: usize, pub len: usize }

pub struct RedactOutcome { pub bytes: Vec<u8>, pub hits: Vec<RedactHit> }

/// Scan `input` for the verbatim bytes of any fingerprint in `patterns`,
/// replacing every non-overlapping matched span with a marker. Bounded
/// full-buffer scan: reuses the Rabin pre-filter + SHA-256 confirm. Earliest
/// match wins on overlap; scanning resumes past the replaced span.
pub fn redact(input: &[u8], patterns: &[SecretFingerprint]) -> RedactOutcome;
```

- Marker: `[redacted:<first-8-hex-of-sha256>]` — correlates a redaction to the
  secret via a one-way hash without leaking plaintext. (Length differs from the
  secret; output is no longer a verbatim copy — intended.)
- No patterns ⇒ `bytes == input.to_vec()`, `hits == []` (no-op).
- Unit tests: single hit; multiple; adjacent; at start; at end; two fingerprints
  of different lengths; overlap resolution; no-match→unchanged; marker shape;
  sub-`MIN_SECRET_LEN` value never fingerprinted ⇒ never matched.

### 2. `core/src/tool_host/secret_scrub.rs` (new sibling)

Mirrors `egress_provision.rs` (keeps `tool_host.rs`, already 609 LOC / over cap,
growing by only the call site). Pure where possible.

- `fn fingerprints_for_dispatch(req_for_audit: &Value, vault: &Vault) -> Vec<SecretFingerprint>`
  — identical pairing to `compute_provision`: `collect_refs_in_params` →
  `vault.value_fingerprint` (no plaintext; sub-8-byte skipped).
- `fn scrub_result_value(v: &mut Value, fps: &[SecretFingerprint]) -> Vec<RedactHit>`
  — walks every JSON **string leaf** of `v`, applies `redact`, replaces the string
  with the redacted bytes (UTF-8), accumulates hits. Pure / no `.await`.
- `async fn emit_scrub_audit(sink, tool, hits)` — when `!hits.is_empty()`, insert
  one `policy / secret.output_scrubbed` row: `{tool, count, hits:[{sha256_hex, offset, len}]}`
  — redacted, symmetric with `egress.blocked.credential_leak`. Best-effort
  (log-on-failure), like `secret.redeemed`.
- Unit tests (fake vault / direct fps): secret in `stdout` redacted + hit list;
  secret in a nested string redacted; **no secrets ⇒ byte-identical passthrough**;
  empty fps ⇒ no-op.

### 3. Opt-in gate

`WorkerManifest::redact_materialized_secrets_in_output(&self) -> bool` — default
`false` (trait default method; dyn-safe, non-generic). `PythonExecManifest`
overrides to `true`. Surfaced onto the resolved tool entry so
`dispatch_with_sink` reads it by tool. Fallback if wiring a flag onto `ToolEntry`
proves heavy: a `tool == python_exec::TOOL_NAME` check in the gate (confirm the
seam in planning). Default-off ⇒ every other worker byte-identical.

### 4. Wiring in `dispatch_with_sink`

On the `Ok(v)` arm at [tool_host.rs:340](../../../core/src/tool_host.rs#L340),
**before** the injection screen:

```text
if gate_on_for(tool) {
    let fps = secret_scrub::fingerprints_for_dispatch(&req_for_audit, vault);
    if !fps.is_empty() {
        let hits = secret_scrub::scrub_result_value(&mut v, &fps);
        secret_scrub::emit_scrub_audit(sink, tool, hits).await; // best-effort
    }
}
// → injection screen sees the redacted v → tool audit row + return value redacted
```

The scrubbed `v` then flows unchanged through the injection screen, the tool audit
row, and the returned result — so plaintext cannot surface in InvokeReport, CLI,
`audit_log.result`, or the JSONL mirror.

## Data flow (secret-bearing python-exec call)

```
operator: memory l3 run <id> --param token=secret://abc12345 --execute
  → daemon l3_run → l3py_invoke → step {tool:"python-exec", params:{token:"secret://abc12345"}}
  → tool_host::dispatch_with_sink:
      req_for_audit = clone (refs intact)            # request side already safe (#147)
      substitute_refs_in_params(params)              # token → PLAINTEXT for the worker
      worker.call → Ok({stdout:"...PLAINTEXT...", stderr, exit_code})
      [GATE on python-exec]
        fps = fingerprints_for_dispatch(req_for_audit, vault)   # no plaintext copy
        scrub_result_value(&mut v, fps)              # stdout → "...[redacted:ab12cd34]..."
        emit secret.output_scrubbed (hash/offset only)
      injection screen(v) → tool audit row(result=redacted) → return redacted v
  → InvokeReport.stdout = redacted ; audit_log.result.stdout = redacted ; JSONL = redacted
```

## Error handling

- Scrub audit insert failure: best-effort (log via `tracing`), like
  `secret.redeemed` — the result is already redacted; failing the dispatch because
  the audit log is unreachable is strictly worse.
- Non-UTF-8 string leaves: JSON strings are UTF-8 by construction; `redact`
  operates on the UTF-8 bytes and the marker is ASCII, so the result stays valid
  UTF-8.
- Fingerprinting never exposes plaintext (`value_fingerprint` reads under the
  vault lock and returns only the one-way `SecretFingerprint`).

## Testing

| Level | Suite | Asserts |
|---|---|---|
| unit | `kastellan-leak-scan` `redact` | all hit patterns above + marker shape + sub-8-byte no-match |
| unit | `core` `secret_scrub` | stdout/nested redaction, hit list, no-secret byte-identical, gate-off no-op |
| e2e (headline) | `core/tests/cli_memory_l3py_run_daemon_e2e.rs` | approved skill prints its `secret://` param → InvokeReport stdout shows `[redacted:…]`, no plaintext; `audit_log.result.stdout` has no plaintext; a `secret.output_scrubbed` row exists |
| e2e (confirming) | same file | python child `os.environ` keys == `{TMPDIR, HOME, KASTELLAN_PYTHON_PARAMS}` (env-clobber containment) |

Skip anything already unit-covered (NUL/control-char escaping, non-object reject,
>64 KiB reject — those exist; do not duplicate).

**Verification:** `cargo test --workspace` (Mac, live PG) +
`cargo clippy --workspace --all-targets -D warnings`. DGX native-Linux only if the
gate touches sandbox behavior (it should not — the scrub is a pure result-value
transform after the worker returns).

## Accepted limitations

- Secrets `< MIN_SECRET_LEN` (8 bytes) are unscannable — same as #3b.
- **Narrow TTL-expiry race.** Fingerprints are read post-`worker.call` via `Vault::value_fingerprint`. If a secret's vault TTL expires in the window between substitution (which injected the plaintext into the worker's params) and that post-call read, `value_fingerprint` returns `None`, the fingerprint set omits it, and that one secret's plaintext would survive into the result unscrubbed. In practice a dispatch is far shorter than any sane TTL, and this is the **same race already present in the egress #3b dispatch-time provisioning** (#268) — it is not introduced here. Closing it would require capturing fingerprints at substitution time (when the plaintext is in hand) rather than re-reading the vault; deferred as consistent-with-#268 and vanishingly narrow.
- The marker changes output length; the result is not a verbatim copy of what the
  code printed (intended — the point is to remove the secret bytes).
- Only `python-exec` opts in; curated workers' result-plaintext stays trusted per
  #147 (revisit per-worker if a future worker also runs untrusted code).

## File-cap note

The scrub logic lives in a **new** `core/src/tool_host/secret_scrub.rs`, not inline,
so `tool_host.rs` (609 LOC, already over the 500 cap) grows only by the call site —
consistent with the `egress_provision.rs` precedent.
