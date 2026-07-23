# Audit findings #11 (#389) + #12 (#388) — hardening design

**Date:** 2026-07-23
**Issues:** [#388](https://github.com/hherb/kastellan/issues/388) (audit #12), [#389](https://github.com/hherb/kastellan/issues/389) (audit #11)
**Source:** `docs/security-audit-2026-07-02.md`, findings #11 + #12. Both **Low** severity, defense-in-depth / robustness — the last two siblings of the audit-#7 remediation family. Not release-blockers; not attacker-reachable at the OS-user trust boundary; each is a latent footgun with no in-code backstop today.

This spec covers four self-contained sub-fixes across the two issues, shipped as one PR. Each is a **pure function in a reusable module** with unit tests, plus a thin wiring layer at the relevant chokepoint. Nothing here changes the containment boundary — these close *observability* and *robustness* gaps around it.

---

## Sub-fix A — #388.1: worker-discovery install-dir trust probe

### Problem
`core/src/worker_manifest.rs::discover_binary` accepts any `exists && !is_dir` sibling of the daemon binary with no ownership/writability check. The threat model documents "install dir must not be user-writable" as a deploy assumption, but there is no in-code backstop: a daemon mis-deployed into a dir writable by a *different* OS user would let that user drop a malicious `kastellan-worker-*` sibling that gets registered on restart (sandboxed, but attacker-chosen code in the worker slot).

**Critical constraint:** the real production install is **per-user** (`~/.local/lib/kastellan/`, owned by the daemon's own user, mode 0755). That dir is writable by its owner *by design*, and a compromise at the daemon's own OS-user level already owns the worker slot (it can replace the binary directly — the threat model's own boundary). So the probe must flag only writability by a principal **other than root or the daemon's own euid** — never the self-owned normal install.

### Design
A pure classifier in `core/src/worker_manifest.rs` (co-located with `discover_binary`):

```rust
/// Trust verdict for the directory the daemon's workers are discovered from.
pub enum InstallDirTrust {
    Trusted,
    Untrusted { reason: String },
}

/// Facts read from the install directory's metadata (Unix). Kept as a plain
/// struct of integers so the classifier is pure and testable without a real FS.
pub struct InstallDirFacts {
    pub owner_uid: u32,
    pub mode: u32, // st_mode permission bits
}

/// Classify the install dir. Untrusted iff writable by a principal other than
/// root or the daemon's own euid:
///   - world-writable (mode & 0o002), OR
///   - group-writable  (mode & 0o020), OR
///   - owned by a uid that is neither 0 (root) nor `self_euid`.
/// The self-owned per-user install (owner == self_euid, mode 0755) is Trusted.
pub fn assess_install_dir(self_euid: u32, facts: &InstallDirFacts) -> InstallDirTrust
```

Rationale for the three rules:
- **world-writable** — any user can drop a sibling. Always untrusted.
- **group-writable** — any member of the dir's group can drop a sibling. The normal install has a private primary group with no group-write (0755), so this is conservative and correct; a shared-group-writable dir is a genuine misdeployment.
- **owner != {root, self}** — a *different* user owns (and thus can write) the dir.

### Wiring (posture: warn-default + opt-in strict)
In `core/src/main.rs`, immediately after `exe_dir` is derived (`current_exe().parent()`), under `#[cfg(unix)]`:
1. `std::fs::metadata(&dir)` → `InstallDirFacts { owner_uid: meta.uid(), mode: meta.mode() }` (via `std::os::unix::fs::MetadataExt`).
2. `assess_install_dir(unsafe { libc::geteuid() } as u32, &facts)` — or read euid via a small helper; `nix`/`libc` is already a dep (confirm during plan; `rustix`/`libc` present transitively).
3. On `Untrusted { reason }`: log a loud `ERROR` naming the dir + reason. If `KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR` is truthy (via the shared `env_flag_enabled` dialect — `1|true|yes|on`), return `Err` and abort startup; otherwise continue.

Non-unix targets: the probe is a no-op (the daemon is Unix-only; `#[cfg(unix)]` guards the metadata read). The pure `assess_install_dir` stays platform-agnostic (operates on integers), so it is unit-tested on any host.

**Cross-platform:** `MetadataExt::{uid,mode}` is available on both Linux and macOS (both Unix) — no Linux-only / macOS-only asymmetry.

### Tests (pure, both hosts)
- self-owned 0755 → Trusted (the normal install; regression guard).
- root-owned 0755 → Trusted (system install).
- world-writable (0757/0777) → Untrusted.
- group-writable (0775) → Untrusted.
- owned by a different non-root uid → Untrusted.
- root-owned but world-writable → still Untrusted (writability dominates ownership).

---

## Sub-fix B — #388.2: manifest under-lock via `policy.env` precedence

### Problem
`core/src/tool_host/lockdown_env.rs::derive_lockdown_env` honors a same-named entry already present in `policy.env` instead of deriving the safe default (pinned today by `derive_does_not_overwrite_caller_supplied_env`, which asserts a pre-set `KASTELLAN_SECCOMP_PROFILE=none` is kept verbatim). A future manifest author could set `KASTELLAN_SECCOMP_PROFILE=none` (which the prelude maps to `SeccompReport::Disabled` — no seccomp filter) or `KASTELLAN_LANDLOCK_PROFILE=none` and silently under-lock a worker. Not attacker-reachable (manifests are host code), but there is no guard.

### Why warn, not reject
`core/src/channel/matrix.rs:249` legitimately sets **both** to `none` when `!cfg.enforce_sandbox` — a deliberate operator dev opt-out (`--enforce-sandbox` CLI flag / `KASTELLAN_MATRIX_ENFORCE_SANDBOX` env, default off in the CLI dev path). A blanket "seccomp may never be `none`" assertion would break that legitimate path. So the fix is **derive-then-warn**: honor the value (unchanged behavior), but surface any sandbox-disabling override as a loud WARN. In production the vars are never set, so the safe path stays silent; when a worker runs with the sandbox disabled, a warning is exactly what an operator should see.

### Design
A pure detector in `lockdown_env.rs`, next to `derive_lockdown_env`:

```rust
/// A lockdown env entry that weakens the profile-derived default.
pub struct LockdownOverride {
    pub var: String,   // "KASTELLAN_SECCOMP_PROFILE" | "KASTELLAN_LANDLOCK_PROFILE"
    pub value: String, // the overriding value (e.g. "none")
}

/// Inspect a *finalized* policy for sandbox-disabling env entries. Flags:
///   - KASTELLAN_SECCOMP_PROFILE == "none"  (seccomp disabled), and/or
///   - KASTELLAN_LANDLOCK_PROFILE == "none" (Landlock disabled).
/// Empty vec ⇒ nothing weakened. Pure; no comparison against the profile is
/// needed — "none" is unambiguously sandbox-off regardless of Profile.
pub fn detect_lockdown_overrides(policy: &SandboxPolicy) -> Vec<LockdownOverride>
```

Scope note: we flag the unambiguous **sandbox-off** value (`none`) rather than "any value differing from the profile-derived default", because a *different valid* profile could be intentional and is not an under-lock. `none` is the concrete case the audit names. Over-broad `KASTELLAN_LANDLOCK_RW` is subjective (no crisp "too broad" predicate) and stays out of scope — documented, not code-guarded.

### Wiring
Call `detect_lockdown_overrides(&derived)` and log each override as a `WARN` (including the worker's program path for context) at **both** derive+spawn paths where a manifest-driven or channel policy is finalized:
1. `core/src/tool_host.rs::spawn_worker` — the chokepoint every **manifest-driven** tool worker spawns through (shell-exec, web-*, python-exec, browser-driver, gliner-relex).
2. `core/src/worker_lifecycle/persistent.rs` (after its `derive_lockdown_env` at line ~54) — the path the **matrix** channel worker spawns through, i.e. the one live *dynamic* `none` case (`--enforce-sandbox=false` dev opt-out). This ensures a sandbox-disabled matrix worker is loudly logged, not silent.

Broker + egress sidecars (`broker/spawn.rs`, `egress/spawn.rs`) run host-fixed policies (never manifest-authored `none`) and are explicitly out of scope — documented, not guarded. To avoid drift between the two insertion points, factor the "detect + log each as WARN" into a tiny shared helper (e.g. `warn_lockdown_overrides(program, &derived)` next to `detect_lockdown_overrides`) that both call, so the log format and the detector stay in one place.

### Tests (pure, both hosts)
- default policy (no override) → empty.
- seccomp=none → one override flagged.
- landlock=none → one override flagged.
- both none → two flagged.
- seccomp=strict (the derived default) → empty.

---

## Sub-fix C — #389.1: keyring first-init overwrite race

### Problem
`db/src/secrets/key_provider.rs::OsKeyringProvider::ensure_initialized_for` is get-then-set: two processes both observing `NoEntry` both generate distinct keys, and the second `set_secret` overwrites the first — rendering any secret the first already encrypted undecryptable (GCM auth fails). Documented as a single-daemon concurrency contract, but a real data-loss path with no guard.

### Design (read-back verify + converge)
Extract the decision logic into a pure function over a small ops seam so it is unit-testable without a real keyring:

```rust
/// The minimal keyring surface the init logic needs. The real impl wraps
/// `keyring::Entry`; tests fake it (and can flip the stored value mid-sequence
/// to simulate a racing writer).
pub trait KeyringOps {
    fn get_secret(&self) -> Result<Vec<u8>, KeyringOpsError>;
    fn set_secret(&self, bytes: &[u8]) -> Result<(), KeyringOpsError>;
}
pub enum KeyringOpsError { NoEntry, Other(String) }

/// How first-init resolved (for caller-side logging).
pub enum FirstInit { ExistingKey, FreshKey, RacedConverged }

/// Pure init logic. `gen` supplies fresh key bytes (OsRng in prod, fixed in
/// tests). On NoEntry: generate → set → **read back**; if the read-back bytes
/// differ from what we wrote, another process won the race — adopt the winner's
/// key (`RacedConverged`) so both processes converge on ONE key before any
/// encryption happens. No new deps, no lock file, works on any keyring backend.
fn resolve_or_init(
    ops: &dyn KeyringOps,
    gen: impl FnOnce() -> [u8; KEY_LEN],
) -> Result<([u8; KEY_LEN], FirstInit), SecretsError>
```

Logic:
1. `get_secret()` → `Ok(existing)`: validate length (existing `KeyLengthInvalid` path) → `(key, ExistingKey)`.
2. `Err(NoEntry)`: `let fresh = gen(); set_secret(&fresh)?;` then `get_secret()?` (must exist now); validate length:
   - equal to `fresh` → `(fresh, FreshKey)`.
   - different → `(read_back, RacedConverged)` — adopt the winner.
3. `Err(Other(s))` → `SecretsError::Keyring`.

`ensure_initialized_for` becomes a thin adapter: build a `KeyringEntryOps(keyring::Entry)` mapping `keyring::Error::NoEntry → NoEntry` and other errors → `Other`, call `resolve_or_init` with an `OsRng` generator, and **log a WARN on `RacedConverged`** ("concurrent keyring first-init detected; converged on the winning key"). Public behavior for the single-daemon case is unchanged (fresh key on first run, same key after).

### Tests (pure, both hosts; scripted fake ops)
- existing valid key → `ExistingKey`, returns it, `set` never called.
- NoEntry, set succeeds, read-back matches → `FreshKey`.
- NoEntry, set succeeds, but read-back returns a *different* valid key (raced) → `RacedConverged`, returns the winner's key (not ours).
- existing wrong-length → `KeyLengthInvalid`.
- get hard-errors → `Keyring` error.
- NoEntry, read-back also wrong-length → error (defensive).

### Honest limitation (not full serialization)
Read-back-verify catches the race **only when the competing `set` lands before our read-back**. The unfavorable interleaving still diverges: if B's `get` precedes A's `set`, then A `set`s K_A, reads back K_A (match, commits K_A), and B afterwards `set`s K_B (overwriting) and reads back K_B (match, commits K_B) — the store ends on K_B while A holds K_A. This is strictly better than today (the common "B wins before A's read-back" case now converges + logs instead of silently overwriting), but it is **not** a mutex. Only an advisory lock would fully serialize. The operator chose converge-over-lock; this residual is acceptable under the documented single-daemon contract and must be stated plainly in the code doc-comment (audit-#7 house rule: never sell a control as more than it is). The window is narrow because `ensure_initialized_for` runs once at daemon startup, before any secret is encrypted.

### Non-goals
No advisory file-lock (rejected: adds a cross-platform lock-path dependency, only serializes same-host processes agreeing on the path). No claim of full mutual exclusion — see "Honest limitation" above.

---

## Sub-fix D — #389.2: overlapping-distinct-secrets scrub gap

### Problem
`leak-scan/src/redact.rs::redact` resolves overlaps greedily: earliest-start wins, later overlapping spans are dropped. When two *distinct* secrets overlap (tail of one == head of the other), the dropped span's non-overlapping suffix survives in plaintext (pinned by the characterization test `overlapping_distinct_secrets_leave_second_suffix`). Non-adversarial (agent code can't align two vault secret values) and negligible-probability, but it violates a clean "no secret byte ever survives" invariant.

### Design (merge overlapping spans)
Replace greedy drop-on-overlap with **merge into maximal runs**. After the existing sort (offset asc, len desc), fold spans into maximal runs where each next span *strictly overlaps* the run so far (`next.offset < run_end`; adjacent `next.offset == run_end` stays a separate redaction — preserves `adjacent_occurrences_both_replaced`). Each run redacts its union `[run_start, run_end)`; the result is a strict superset of today's bytes (over-redaction is always safe).

Marker + hits:
- **Single-contributor run** (the overwhelmingly common case — no overlap): unchanged. `[redacted:<8hex>]`, one `RedactHit { sha256_hex, offset, len }`. All existing single-span tests pass byte-identically.
- **Multi-contributor run** (rare overlap): marker lists each distinct contributor's 8-hex in run order: `[redacted:<8hex1>+<8hex2>]`. One `RedactHit` **per contributing span** (original offsets/lens preserved), so `secret_scrub`'s audit trail records every secret that appeared. Consumers only ever `.contains("[redacted:")` or iterate `hits`, so this is compatible (verified: `core/src/tool_host/secret_scrub.rs` accumulates hits and emits one audit row per hit; the `[redacted:` prefix is all any test checks).

### Tests (pure, both hosts)
- **Update** `overlapping_candidates_resolve_earliest_start`: now merges → region covers both, `hits.len() == 2`, no plaintext survives.
- **Update** `overlapping_candidates_resolve_longer_span_on_tie`: merged region `[0,16)`, both contributors recorded.
- **Replace** `overlapping_distinct_secrets_leave_second_suffix` → `overlapping_distinct_secrets_are_fully_redacted`: `a="abcdefgh"[0,8)` + `b="fghijklm"[5,13)` → redact `[0,13)`, output `[redacted:<8hex_a>+<8hex_b>]`, **no `ijklm` suffix**, `hits.len() == 2`.
- **Keep** `adjacent_occurrences_both_replaced` (adjacent, not overlapping → still two separate markers).
- New: three-way overlapping chain merges into one run; two disjoint overlapping pairs stay two runs.

### Encoded-secret gap — documented, no code
A base64/hex/url-encoded appearance of a secret is not scrubbed (inherent to verbatim value-fingerprint scanning; shared with the streaming egress matcher). The containment boundary for encoded egress is the sandbox + egress proxy, not the scanner. Tighten the module doc comment to state this explicitly; no code change.

---

## Cross-cutting

- **Testing:** every pure function is unit-tested on the Mac; final `cargo test --workspace` + `clippy --workspace --all-targets -D warnings` on the DGX (native aarch64, real bwrap + live PG). None of these changes is `cfg(linux)`-gated (Sub-fix A is `cfg(unix)`, exercised on both hosts), so the Mac covers the logic; the DGX is the acceptance gate per the standing convention.
- **File sizes:** all four touched files stay well under 500 LoC after the additions (`lockdown_env.rs` ~325 → ~370; `redact.rs` ~290 → ~340; `key_provider.rs` ~197 → ~270; `worker_manifest.rs` gains ~60). No split needed.
- **No new dependencies.** `libc`/`rustix` for `geteuid` is already transitively present (confirm the crate to use during the plan); everything else is std + existing crates.
- **Security house rule (audit-#7 family):** each security assertion must be **proved to fail against un-hardened code** before the fix is trusted — e.g. delete the read-back verify and watch the race test fail; feed a world-writable dir and watch the probe stay silent without the check; drop the merge and watch the suffix survive.
- **PR:** one PR closing #388 and #389, linked to both issues.
