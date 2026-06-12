# Egress Slice #3b — Credential-Leak Scanner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a co-located credential-leak scanner to the per-worker egress proxy: scan MITM-terminated plaintext for the verbatim bytes of any secret materialized for the calling worker, killing + auditing the connection on a hit (hash + offset only, never plaintext).

**Architecture:** A tiny pure crate `kastellan-leak-scan` (deps: `serde` + `sha2`) holds the single source of truth — the `SecretFingerprint` type, the `fingerprint_value` computation, the streaming `RollingMatcher` (Rabin-Karp pre-filter + SHA-256 confirm with cross-read carry-over), and the `secret_hashes.json` serde shape. Both `core` (host: `Vault::value_fingerprint` + scratch-file writer + spawn-wiring) and `workers/egress-proxy` (sidecar: lazy per-connection file read + scanning relay replacing `copy_bidirectional`) depend on it. Detection is hashes-only; the block is best-effort streaming; provisioning is via a host-written scratch file the proxy re-reads per connection; spawn-wiring lands now, the dispatch-chokepoint live-append is deferred.

**Tech Stack:** Rust (rustc 1.96.0), `sha2`, `serde`/`serde_json`, `tokio` (async relay in the proxy), `tokio-rustls` (existing MITM). No new third-party deps beyond what `core`/`egress-proxy` already use.

**Spec:** `docs/superpowers/specs/2026-06-12-egress-proxy-slice3b-credential-leak-scanner-design.md`

**Branch:** `feat/egress-slice3b-leak-scanner` (already created; the spec is committed on it).

---

## Preliminaries (read before starting)

- Source the cargo env in every shell: `source "$HOME/.cargo/env"`.
- Workspace tests today: macOS skip-as-pass (no `KASTELLAN_PG_BIN_DIR`). The new unit tests are all PG-free and cross-platform. The proxy's `scan_relay` test uses in-memory `tokio::io::duplex` — no sandbox, no TLS, deterministic.
- **Never `git add -A`** (the untracked `docs/essay-medium-draft.md` and `.claude/scheduled_tasks.lock` must stay out). Stage explicit paths only.
- LOC discipline: keep every touched/created file < 500 LOC.
- Commit message footer for every commit:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  ```

## File structure (decomposition)

**New crate `leak-scan/`** (top-level workspace member, pure lib):
- `leak-scan/Cargo.toml` — deps `serde` (derive), `serde_json`, `sha2`.
- `leak-scan/src/lib.rs` — module docs + re-exports.
- `leak-scan/src/fingerprint.rs` — `SecretFingerprint`, `MIN_SECRET_LEN`, `RABIN_BASE`, `fingerprint_value`, `poly`.
- `leak-scan/src/wire.rs` — `serialize_hashes` / `parse_hashes` (the `secret_hashes.json` shape).
- `leak-scan/src/matcher.rs` — `RollingMatcher`, `LeakHit`.

**core:**
- `core/src/secrets/vault.rs` — add `Vault::value_fingerprint`.
- `core/src/egress/leak_provision.rs` (new) — `write_secret_hashes` (atomic) + `provision_audit_row`.
- `core/src/egress/mod.rs` — register `leak_provision`.
- `core/src/egress/net_worker.rs` — `secret_fingerprints` param on the two spawn fns; write the file after sidecar spawn.
- `core/src/egress/audit.rs` — map the credential-leak line.
- `core/Cargo.toml` — depend on `kastellan-leak-scan`.
- `core/tests/egress_leak_scan_e2e.rs` (new) — spawn-wiring writes the file a real sidecar can read; clean round-trip still works.

**egress-proxy:**
- `workers/egress-proxy/Cargo.toml` — depend on `kastellan-leak-scan`.
- `workers/egress-proxy/src/report.rs` — `Verdict::BlockedCredentialLeak` + `Decision.leak`.
- `workers/egress-proxy/src/mitm.rs` — `intercept` takes patterns, returns `Result<Option<LeakReport>, String>`.
- `workers/egress-proxy/src/mitm/relay.rs` (new) — `scan_relay` + `LeakReport` + `Direction`.
- `workers/egress-proxy/src/proxy.rs` — `MitmCtx.secret_hashes_path`, `run_mitm` loads patterns + maps a leak to a decision.
- `workers/egress-proxy/src/main.rs` — set `MitmCtx.secret_hashes_path`.

---

## Task 0: Scaffold the `kastellan-leak-scan` crate

**Files:**
- Create: `leak-scan/Cargo.toml`
- Create: `leak-scan/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Inspect the workspace members list**

Run: `grep -n "members" -A 20 Cargo.toml`
Expected: a `[workspace] members = [ ... ]` array listing `core`, `db`, `workers/web-common`, etc. Note the exact formatting/quoting style to match it.

- [ ] **Step 2: Create the crate manifest**

Create `leak-scan/Cargo.toml` (mirror the field style of `workers/web-common/Cargo.toml` — workspace-inherited version/edition/license/etc.):

```toml
[package]
name        = "kastellan-leak-scan"
description = "Pure credential-leak scanner: secret-value fingerprinting + streaming rolling-window matcher, shared by the egress proxy (detect) and core (provision)."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme      = "../README.md"

[dependencies]
serde      = { workspace = true }
serde_json = { workspace = true }
sha2       = { workspace = true }
```

- [ ] **Step 3: Create the lib root**

Create `leak-scan/src/lib.rs`:

```rust
//! Pure, dependency-light credential-leak scanner shared by the egress proxy
//! (which *detects* leaks on MITM-terminated plaintext) and `core` (which
//! *provisions* the per-worker secret-value fingerprints).
//!
//! Single source of truth: the fingerprint algorithm here MUST stay identical
//! on both sides, so it lives in exactly one crate. Detection is hashes-only —
//! a [`SecretFingerprint`] carries only one-way hashes (a SHA-256 + a 64-bit
//! Rabin fingerprint) plus the length, never the secret value. See the design
//! doc `docs/superpowers/specs/2026-06-12-egress-proxy-slice3b-credential-leak-scanner-design.md`.

mod fingerprint;
mod matcher;
mod wire;

pub use fingerprint::{fingerprint_value, SecretFingerprint, MIN_SECRET_LEN};
pub use matcher::{LeakHit, RollingMatcher};
pub use wire::{parse_hashes, serialize_hashes};
```

- [ ] **Step 4: Add the crate to the workspace members**

Modify `Cargo.toml`: add `"leak-scan",` to the `[workspace] members` array, preserving the existing quoting/indentation style.

- [ ] **Step 5: Create empty module files so the crate compiles**

Create `leak-scan/src/fingerprint.rs`, `leak-scan/src/matcher.rs`, `leak-scan/src/wire.rs` each containing only a doc comment placeholder (filled in the next tasks):

`leak-scan/src/fingerprint.rs`:
```rust
//! Secret-value fingerprinting (filled in Task 1).
```
`leak-scan/src/matcher.rs`:
```rust
//! Streaming rolling-window matcher (filled in Task 3).
```
`leak-scan/src/wire.rs`:
```rust
//! `secret_hashes.json` serde shape (filled in Task 2).
```

This will NOT compile yet (lib.rs re-exports names that don't exist). That's fine — the next tasks add them. Do not run the build at this step.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml leak-scan/Cargo.toml leak-scan/src/lib.rs leak-scan/src/fingerprint.rs leak-scan/src/matcher.rs leak-scan/src/wire.rs
git commit -m "feat(leak-scan): scaffold shared credential-leak-scanner crate

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 1: `SecretFingerprint` + `fingerprint_value`

**Files:**
- Modify: `leak-scan/src/fingerprint.rs`

- [ ] **Step 1: Write the failing tests**

Replace `leak-scan/src/fingerprint.rs` with:

```rust
//! Secret-value fingerprinting: a one-way [`SecretFingerprint`] (length +
//! 64-bit Rabin polynomial hash + SHA-256) computed from a secret's plaintext
//! bytes. Provisioned to the egress proxy so it can detect the verbatim bytes
//! without ever holding the secret value.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Secrets shorter than this are never fingerprinted/provisioned: trivially
/// short values produce high false-positive match rates against arbitrary
/// egress traffic and are not real credentials.
pub const MIN_SECRET_LEN: usize = 8;

/// Base of the Rabin-Karp polynomial rolling hash. MUST be identical on the
/// provisioning side ([`fingerprint_value`]) and the scanning side
/// ([`super::matcher::RollingMatcher`]) — they live in this one crate so they
/// cannot drift. Arithmetic is wrapping (mod 2^64).
pub(crate) const RABIN_BASE: u64 = 257;

/// One-way fingerprint of a secret value. Carries no plaintext: `fp64` and
/// `sha256` are both irreversible for a high-entropy secret; only `len` leaks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretFingerprint {
    /// Byte length of the secret value (== the scan window width).
    pub len: usize,
    /// 64-bit Rabin polynomial hash of the value — the cheap pre-filter.
    pub fp64: u64,
    /// SHA-256 of the value — the confirmation that eliminates Rabin collisions.
    pub sha256: [u8; 32],
}

/// Direct Rabin polynomial hash of `bytes`: `sum(b_k * BASE^(len-1-k))`, wrapping.
/// The [`super::matcher::RollingMatcher`] rolling state converges to this exact
/// value for any window equal to `bytes`.
pub(crate) fn poly(bytes: &[u8]) -> u64 {
    let mut h = 0u64;
    for &b in bytes {
        h = h.wrapping_mul(RABIN_BASE).wrapping_add(b as u64);
    }
    h
}

/// Fingerprint `value`. Returns `None` if it is below [`MIN_SECRET_LEN`].
pub fn fingerprint_value(value: &[u8]) -> Option<SecretFingerprint> {
    if value.len() < MIN_SECRET_LEN {
        return None;
    }
    let mut h = Sha256::new();
    h.update(value);
    let sha256: [u8; 32] = h.finalize().into();
    Some(SecretFingerprint {
        len: value.len(),
        fp64: poly(value),
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_a_long_enough_value() {
        let fp = fingerprint_value(b"super-secret-token-1234").expect("long enough");
        assert_eq!(fp.len, 23);
        // sha256 matches an independent computation.
        let mut h = Sha256::new();
        h.update(b"super-secret-token-1234");
        let expected: [u8; 32] = h.finalize().into();
        assert_eq!(fp.sha256, expected);
        // fp64 matches the direct polynomial.
        assert_eq!(fp.fp64, poly(b"super-secret-token-1234"));
    }

    #[test]
    fn rejects_values_below_min_len() {
        assert!(fingerprint_value(b"").is_none());
        assert!(fingerprint_value(b"1234567").is_none()); // 7 < 8
        assert!(fingerprint_value(b"12345678").is_some()); // 8 == MIN
    }

    #[test]
    fn poly_is_position_sensitive() {
        // Anagram inputs must not collide trivially (sanity on the base choice).
        assert_ne!(poly(b"ab"), poly(b"ba"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass (this module is self-contained)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-leak-scan fingerprint -- --nocapture`
Expected: 3 tests pass. (The crate still won't fully build until Tasks 2–3 fill `wire`/`matcher`; if `cargo test -p` fails to compile due to the empty sibling modules, that's expected — proceed to Task 2 and run the crate test suite at the end of Task 3. If you want an isolated check now, temporarily comment the `pub use` lines in `lib.rs` for `matcher`/`wire`, run, then restore.)

- [ ] **Step 3: Commit**

```bash
git add leak-scan/src/fingerprint.rs
git commit -m "feat(leak-scan): SecretFingerprint + fingerprint_value (Rabin + SHA-256)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `secret_hashes.json` serde shape

**Files:**
- Modify: `leak-scan/src/wire.rs`

- [ ] **Step 1: Write the implementation + failing tests**

Replace `leak-scan/src/wire.rs` with:

```rust
//! The on-disk `secret_hashes.json` provisioning shape. `core` writes it into
//! the sidecar scratch dir; the proxy lazily re-reads it per connection. The
//! hex string encoding for `fp64`/`sha256` avoids JSON `u64`-precision pitfalls
//! and keeps the file human-auditable.

use serde::{Deserialize, Serialize};

use crate::fingerprint::SecretFingerprint;

/// File envelope. `version` lets a future format change be detected rather than
/// silently mis-parsed.
#[derive(Serialize, Deserialize)]
struct HashesFile {
    version: u32,
    secrets: Vec<WireFp>,
}

/// Wire form of one fingerprint: `len` plus hex-encoded `fp64` (16 hex chars)
/// and `sha256` (64 hex chars).
#[derive(Serialize, Deserialize)]
struct WireFp {
    len: usize,
    fp64: String,
    sha256: String,
}

const VERSION: u32 = 1;

/// Serialize fingerprints to the `secret_hashes.json` string.
pub fn serialize_hashes(fps: &[SecretFingerprint]) -> String {
    let secrets = fps
        .iter()
        .map(|f| WireFp {
            len: f.len,
            fp64: format!("{:016x}", f.fp64),
            sha256: hex32(&f.sha256),
        })
        .collect();
    let file = HashesFile {
        version: VERSION,
        secrets,
    };
    serde_json::to_string(&file).expect("HashesFile serialization never fails")
}

/// Parse the `secret_hashes.json` string. Lenient: a malformed file, an unknown
/// version, or a malformed entry yields an empty/partial list rather than an
/// error — a missing or corrupt provisioning file must degrade to "no scanning",
/// never crash the proxy mid-connection.
pub fn parse_hashes(s: &str) -> Vec<SecretFingerprint> {
    let Ok(file) = serde_json::from_str::<HashesFile>(s) else {
        return Vec::new();
    };
    if file.version != VERSION {
        return Vec::new();
    }
    file.secrets.into_iter().filter_map(decode_one).collect()
}

fn decode_one(w: WireFp) -> Option<SecretFingerprint> {
    let fp64 = u64::from_str_radix(&w.fp64, 16).ok()?;
    let sha256 = dehex32(&w.sha256)?;
    Some(SecretFingerprint {
        len: w.len,
        fp64,
        sha256,
    })
}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn dehex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::fingerprint_value;

    #[test]
    fn round_trips() {
        let fps = vec![
            fingerprint_value(b"first-secret-value").unwrap(),
            fingerprint_value(b"second-secret-value-xyz").unwrap(),
        ];
        let s = serialize_hashes(&fps);
        let back = parse_hashes(&s);
        assert_eq!(back, fps);
    }

    #[test]
    fn empty_round_trips() {
        assert_eq!(parse_hashes(&serialize_hashes(&[])), Vec::new());
    }

    #[test]
    fn garbage_yields_empty() {
        assert!(parse_hashes("not json").is_empty());
        assert!(parse_hashes(r#"{"version":999,"secrets":[]}"#).is_empty());
    }

    #[test]
    fn malformed_entry_is_skipped_not_fatal() {
        let s = r#"{"version":1,"secrets":[{"len":5,"fp64":"zz","sha256":"short"}]}"#;
        assert!(parse_hashes(s).is_empty());
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-leak-scan wire -- --nocapture`
Expected: 4 tests pass (the crate now compiles `fingerprint` + `wire`; `matcher` is still a placeholder — if `lib.rs`'s `pub use matcher::...` blocks compilation, temporarily comment it, run, restore, then proceed to Task 3).

- [ ] **Step 3: Commit**

```bash
git add leak-scan/src/wire.rs
git commit -m "feat(leak-scan): secret_hashes.json serde shape (hex-encoded, lenient parse)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `RollingMatcher` streaming scanner (the heart)

**Files:**
- Modify: `leak-scan/src/matcher.rs`

- [ ] **Step 1: Write the implementation + failing tests**

Replace `leak-scan/src/matcher.rs` with:

```rust
//! Streaming credential-leak matcher. Feeds an arbitrarily-chunked byte stream
//! through a per-length Rabin-Karp rolling hash (cheap pre-filter) confirmed by
//! SHA-256 (eliminates collisions). State persists across [`RollingMatcher::feed`]
//! calls via a ring buffer of the last `maxLen` bytes, so a secret split across
//! a read boundary (`…AB | CD…`) still matches on the same logical pass.
//!
//! Memory is O(maxLen) regardless of stream size, so the whole connection can be
//! scanned with no body cap.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::fingerprint::{poly, SecretFingerprint, RABIN_BASE};

/// A confirmed leak: which secret (by its SHA-256, hex) and where in the stream
/// its first byte sat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeakHit {
    pub sha256_hex: String,
    /// Absolute byte offset (0-based) of the matched window's first byte.
    pub offset: u64,
}

/// Per-distinct-length rolling state.
struct LenGroup {
    len: usize,
    /// `RABIN_BASE^(len-1)`, wrapping — the weight of the byte leaving the window.
    pow: u64,
    /// Current rolling hash over the last `len` bytes (valid once `primed`).
    cur: u64,
    primed: bool,
    /// fp64 → the SHA-256(s) of secrets of this length sharing that fp64.
    targets: HashMap<u64, Vec<[u8; 32]>>,
}

/// Streaming matcher over one direction of a tunnel.
pub struct RollingMatcher {
    groups: Vec<LenGroup>,
    /// Ring buffer of the last `cap` bytes; `cap = maxLen + 1` so the byte
    /// *leaving* the widest window is still present for the rolling subtraction.
    ring: Vec<u8>,
    cap: usize,
    /// Total bytes fed so far. The most recent byte sits at absolute index `fed-1`.
    fed: u64,
}

impl RollingMatcher {
    /// Build a matcher for `patterns`. Patterns below the minimum length never
    /// reach here (the provisioner filters them), but any with `len == 0` are
    /// defensively dropped. An empty pattern set makes [`Self::feed`] a near no-op.
    pub fn new(patterns: Vec<SecretFingerprint>) -> Self {
        let mut by_len: HashMap<usize, HashMap<u64, Vec<[u8; 32]>>> = HashMap::new();
        for p in patterns.into_iter().filter(|p| p.len > 0) {
            by_len
                .entry(p.len)
                .or_default()
                .entry(p.fp64)
                .or_default()
                .push(p.sha256);
        }
        let max_len = by_len.keys().copied().max().unwrap_or(0);
        let groups = by_len
            .into_iter()
            .map(|(len, targets)| LenGroup {
                len,
                pow: pow_base(len),
                cur: 0,
                primed: false,
                targets,
            })
            .collect();
        let cap = max_len.saturating_add(1).max(1);
        RollingMatcher {
            groups,
            ring: vec![0u8; cap],
            cap,
            fed: 0,
        }
    }

    /// True when there is nothing to scan for — the caller skips the scanning
    /// relay entirely and uses the plain copy.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Feed a chunk; return the first confirmed leak in it (if any). Stateful.
    pub fn feed(&mut self, chunk: &[u8]) -> Option<LeakHit> {
        if self.groups.is_empty() {
            self.fed = self.fed.wrapping_add(chunk.len() as u64);
            return None;
        }
        for &b in chunk {
            let i = self.fed; // absolute index of this byte
            self.ring[(i as usize) % self.cap] = b;
            // Update each length group now that byte `i` has been stored.
            for g in &mut self.groups {
                let l = g.len as u64;
                if i + 1 < l {
                    continue; // window not yet full
                }
                if !g.primed {
                    // First full window [i-l+1 ..= i]: compute directly.
                    g.cur = window_poly(&self.ring, self.cap, i, g.len);
                    g.primed = true;
                } else {
                    // Roll: drop the byte at i-l, shift, add the new byte b.
                    let out = self.ring[((i - l) as usize) % self.cap];
                    g.cur = g
                        .cur
                        .wrapping_sub((out as u64).wrapping_mul(g.pow))
                        .wrapping_mul(RABIN_BASE)
                        .wrapping_add(b as u64);
                }
                if let Some(shas) = g.targets.get(&g.cur) {
                    // fp64 pre-filter hit → confirm with SHA-256 of the window.
                    let window = read_window(&self.ring, self.cap, i, g.len);
                    let mut h = Sha256::new();
                    h.update(&window);
                    let digest: [u8; 32] = h.finalize().into();
                    if shas.iter().any(|s| *s == digest) {
                        return Some(LeakHit {
                            sha256_hex: hex(&digest),
                            offset: i + 1 - l,
                        });
                    }
                }
            }
            self.fed = i + 1;
        }
        None
    }
}

/// `RABIN_BASE^(len-1)`, wrapping. `len >= 1` guaranteed by the caller.
fn pow_base(len: usize) -> u64 {
    let mut p = 1u64;
    for _ in 0..len.saturating_sub(1) {
        p = p.wrapping_mul(RABIN_BASE);
    }
    p
}

/// Direct poly hash of the `len`-byte window ending at absolute index `i`.
fn window_poly(ring: &[u8], cap: usize, i: u64, len: usize) -> u64 {
    poly(&read_window(ring, cap, i, len))
}

/// Copy the `len`-byte window ending at absolute index `i` out of the ring.
fn read_window(ring: &[u8], cap: usize, i: u64, len: usize) -> Vec<u8> {
    let start = i + 1 - len as u64; // absolute index of the first window byte
    (0..len)
        .map(|k| ring[((start + k as u64) as usize) % cap])
        .collect()
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::fingerprint_value;

    fn fp(v: &[u8]) -> SecretFingerprint {
        fingerprint_value(v).expect("test secret long enough")
    }

    fn sha_hex(v: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(v);
        hex(&h.finalize().into())
    }

    #[test]
    fn detects_secret_in_a_single_chunk() {
        let secret = b"alpha-bravo-charlie";
        let mut m = RollingMatcher::new(vec![fp(secret)]);
        let hit = m.feed(b"GET /?x=alpha-bravo-charlie HTTP/1.1").expect("hit");
        assert_eq!(hit.sha256_hex, sha_hex(secret));
        assert_eq!(hit.offset, 8); // index where "alpha..." starts
    }

    #[test]
    fn clean_stream_no_hit() {
        let mut m = RollingMatcher::new(vec![fp(b"alpha-bravo-charlie")]);
        assert!(m.feed(b"nothing to see here, move along please").is_none());
    }

    #[test]
    fn detects_secret_split_across_two_feeds() {
        // The boundary pin: the secret straddles the read boundary.
        let secret = b"split-secret-value";
        let mut m = RollingMatcher::new(vec![fp(secret)]);
        assert!(m.feed(b"prefix split-secret").is_none());
        let hit = m.feed(b"-value suffix").expect("hit across boundary");
        assert_eq!(hit.sha256_hex, sha_hex(secret));
    }

    #[test]
    fn detects_secret_split_byte_by_byte() {
        let secret = b"drip-fed-secret-xy";
        let mut m = RollingMatcher::new(vec![fp(secret)]);
        let mut hit = None;
        for b in b"zz".iter().chain(secret).chain(b"qq") {
            if let Some(h) = m.feed(&[*b]) {
                hit = Some(h);
            }
        }
        assert_eq!(hit.expect("byte-fed hit").sha256_hex, sha_hex(secret));
    }

    #[test]
    fn two_secrets_same_length() {
        let a = b"secret-aaa-1234"; // len 15
        let b = b"secret-bbb-5678"; // len 15
        let mut m = RollingMatcher::new(vec![fp(a), fp(b)]);
        assert_eq!(m.feed(b"xx secret-bbb-5678 yy").unwrap().sha256_hex, sha_hex(b));
    }

    #[test]
    fn two_secrets_different_lengths() {
        let short = b"short-one"; // len 9
        let long = b"a-much-longer-secret-string"; // len 27
        let mut m = RollingMatcher::new(vec![fp(short), fp(long)]);
        assert_eq!(
            m.feed(b"...a-much-longer-secret-string...").unwrap().sha256_hex,
            sha_hex(long)
        );
    }

    #[test]
    fn empty_patterns_is_noop() {
        let mut m = RollingMatcher::new(vec![]);
        assert!(m.is_empty());
        assert!(m.feed(b"anything at all including secrets").is_none());
    }

    #[test]
    fn fp64_collision_is_rejected_by_sha256() {
        // Forge a fingerprint with the SAME fp64 + len as a real secret but a
        // different SHA-256. The pre-filter fires; the SHA-256 confirm rejects it.
        let real = b"real-secret-value-9";
        let mut forged = fp(real);
        forged.sha256 = [0u8; 32]; // wrong digest, identical len + fp64
        let mut m = RollingMatcher::new(vec![forged]);
        assert!(
            m.feed(b"xx real-secret-value-9 yy").is_none(),
            "SHA-256 confirm must reject an fp64-only match"
        );
    }
}
```

- [ ] **Step 2: Run the full crate test suite**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-leak-scan -- --nocapture`
Expected: all tests across `fingerprint` (3) + `wire` (4) + `matcher` (8) pass. If the offset assertion in `detects_secret_in_a_single_chunk` fails, the literal index of `"alpha"` in `b"GET /?x=alpha-bravo-charlie HTTP/1.1"` is 8 — recount and fix the expectation, not the code.

- [ ] **Step 3: Clippy the new crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-leak-scan --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add leak-scan/src/matcher.rs
git commit -m "feat(leak-scan): streaming RollingMatcher (Rabin pre-filter + SHA-256 confirm + carry-over)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Wire the crate into core + egress-proxy

**Files:**
- Modify: `core/Cargo.toml`
- Modify: `workers/egress-proxy/Cargo.toml`

- [ ] **Step 1: Add the dep to core**

In `core/Cargo.toml` `[dependencies]`, add alongside the other `kastellan-*` path deps:

```toml
kastellan-leak-scan  = { path = "../leak-scan", version = "0.1.0" }
```

- [ ] **Step 2: Add the dep to egress-proxy**

In `workers/egress-proxy/Cargo.toml` `[dependencies]`, add:

```toml
kastellan-leak-scan = { path = "../../leak-scan", version = "0.1.0" }
```

- [ ] **Step 3: Verify both crates still build**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core -p kastellan-worker-egress-proxy`
Expected: builds (the dep is present but unused yet — `cargo` does not warn on an unused dependency).

- [ ] **Step 4: Commit**

```bash
git add core/Cargo.toml workers/egress-proxy/Cargo.toml Cargo.lock
git commit -m "build: depend on kastellan-leak-scan from core + egress-proxy

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `Vault::value_fingerprint`

**Files:**
- Modify: `core/src/secrets/vault.rs`

- [ ] **Step 1: Write the failing test**

The tests live in the sibling file `core/src/secrets/vault/tests.rs` (declared via `#[cfg(test)] mod tests;` at the bottom of `vault.rs`). That file already has a `use super::*;` and a `_test_insert(vault: &Vault, r: SecretRef, plaintext: Vec<u8>)` helper that stages an entry directly into the map — use it (matching the file's idiom). Add:

```rust
#[test]
fn value_fingerprint_matches_plaintext_hash() {
    use sha2::{Digest, Sha256};
    let vault = Vault::with_ttl(Duration::from_secs(60));
    let value = b"a-real-secret-value-1234";
    let r = SecretRef::from_raw("secret://aabbccdd".to_string());
    _test_insert(&vault, r.clone(), value.to_vec());
    let fp = vault.value_fingerprint(&r).expect("fingerprint");
    let mut h = Sha256::new();
    h.update(value);
    let expected: [u8; 32] = h.finalize().into();
    assert_eq!(fp.sha256, expected);
    assert_eq!(fp.len, value.len());
}

#[test]
fn value_fingerprint_none_for_absent_or_short() {
    let vault = Vault::with_ttl(Duration::from_secs(60));
    // Absent ref.
    assert!(vault
        .value_fingerprint(&SecretRef::from_raw("secret://00000000".to_string()))
        .is_none());
    // Present but below MIN_SECRET_LEN.
    let r = SecretRef::from_raw("secret://11111111".to_string());
    _test_insert(&vault, r.clone(), b"short".to_vec());
    assert!(vault.value_fingerprint(&r).is_none());
}
```

(`Duration`, `SecretRef`, `_test_insert` are all in scope via the existing `use super::*;` + the file's `use std::time::Duration;`.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib secrets::vault::tests::value_fingerprint -- --nocapture`
Expected: FAIL — `no method named value_fingerprint`.

- [ ] **Step 3: Implement `value_fingerprint`**

In `core/src/secrets/vault.rs`, add a `use`:

```rust
use kastellan_leak_scan::{fingerprint_value, SecretFingerprint};
```

and add the method inside `impl Vault` (place it after `redeem`):

```rust
/// Compute a one-way [`SecretFingerprint`] of the secret's value for the
/// egress credential-leak scanner (slice #3b), **without exposing the
/// plaintext**. Returns `None` if the ref is absent/expired or the value is
/// below `MIN_SECRET_LEN`. Takes the read lock and fingerprints in place; the
/// plaintext never leaves this method.
pub fn value_fingerprint(&self, r: &SecretRef) -> Option<SecretFingerprint> {
    let now = Instant::now();
    let map = self.map.read().expect("vault map poisoned");
    let entry = map.get(r)?;
    if now >= entry.expires_at {
        return None;
    }
    fingerprint_value(&entry.plaintext)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib secrets::vault::tests::value_fingerprint -- --nocapture`
Expected: both tests PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/secrets/vault.rs core/src/secrets/vault/tests.rs
git commit -m "feat(secrets/vault): value_fingerprint for the egress leak scanner

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

(If the tests live inline in `vault.rs` rather than a sibling file, stage only `core/src/secrets/vault.rs`.)

---

## Task 6: `core/src/egress/leak_provision.rs` — file writer + audit row

**Files:**
- Create: `core/src/egress/leak_provision.rs`
- Modify: `core/src/egress/mod.rs`

- [ ] **Step 1: Inspect `egress/mod.rs` for the module-declaration style**

Run: `cat core/src/egress/mod.rs`
Expected: a list of `pub mod audit; pub mod net_worker;` etc. Note whether it re-exports names.

- [ ] **Step 2: Create the module with implementation + tests**

Create `core/src/egress/leak_provision.rs`:

```rust
//! Host side of egress slice #3b: provision per-worker secret-value
//! fingerprints into the sidecar scratch dir (the proxy lazily re-reads them)
//! and shape the provisioning audit row. The fingerprints themselves are
//! computed by [`crate::secrets::vault::Vault::value_fingerprint`]; this module
//! writes the `secret_hashes.json` file and builds the audit row.
//!
//! Spawn-wiring writes the file from the worker's known secret set (empty for
//! today's egress workers); the dispatch-chokepoint live-append is the tracked
//! follow-up.

use std::io;
use std::path::Path;

use kastellan_leak_scan::{serialize_hashes, SecretFingerprint};

use super::audit::{EgressAuditRow, ACTOR};

/// File name written into the sidecar scratch dir (sibling of the UDS + ca.pem).
/// MUST match the name the proxy reads (`egress-proxy::main`).
pub const SECRET_HASHES_FILE_NAME: &str = "secret_hashes.json";

/// Atomically write `fps` to `<scratch>/secret_hashes.json`. Writes to a temp
/// file in the same dir then renames, so the proxy's lazy per-connection read
/// never observes a torn file. An empty slice writes an empty list (the proxy
/// treats that as "no scanning").
pub fn write_secret_hashes(scratch: &Path, fps: &[SecretFingerprint]) -> io::Result<()> {
    let final_path = scratch.join(SECRET_HASHES_FILE_NAME);
    let tmp_path = scratch.join(format!("{SECRET_HASHES_FILE_NAME}.tmp"));
    std::fs::write(&tmp_path, serialize_hashes(fps))?;
    std::fs::rename(&tmp_path, &final_path)
}

/// Build the `policy / egress.secret_hash.provisioned` audit row that lets a
/// later leak hash (which carries only the value SHA-256) be correlated back to
/// a secret *name*. The value hash is one-way, so `name + value_sha256` is safe
/// to persist. Pure — the caller inserts it.
pub fn provision_audit_row(worker: &str, name: &str, fp: &SecretFingerprint) -> EgressAuditRow {
    let value_sha256 = fp
        .sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    EgressAuditRow {
        actor: ACTOR,
        action: "egress.secret_hash.provisioned".to_string(),
        payload: serde_json::json!({
            "worker": worker,
            "name": name,
            "value_sha256": value_sha256,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_leak_scan::{fingerprint_value, parse_hashes};

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let fps = vec![fingerprint_value(b"provisioned-secret-1").unwrap()];
        write_secret_hashes(dir.path(), &fps).unwrap();
        let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
        assert_eq!(parse_hashes(&s), fps);
        // No leftover temp file.
        assert!(!dir.path().join("secret_hashes.json.tmp").exists());
    }

    #[test]
    fn empty_write_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        write_secret_hashes(dir.path(), &[]).unwrap();
        let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
        assert!(parse_hashes(&s).is_empty());
    }

    #[test]
    fn provision_audit_row_shape() {
        let fp = fingerprint_value(b"provisioned-secret-2").unwrap();
        let row = provision_audit_row("web-fetch", "OPENAI_KEY", &fp);
        assert_eq!(row.actor, "egress_proxy");
        assert_eq!(row.action, "egress.secret_hash.provisioned");
        assert_eq!(row.payload["worker"], "web-fetch");
        assert_eq!(row.payload["name"], "OPENAI_KEY");
        assert_eq!(row.payload["value_sha256"].as_str().unwrap().len(), 64);
    }
}
```

Note: `EgressAuditRow` fields (`actor: &'static str`, `action: String`, `payload`) and `ACTOR` are defined in `core/src/egress/audit.rs` — confirm `ACTOR` is `pub` (it is: `pub const ACTOR`).

- [ ] **Step 3: Register the module**

In `core/src/egress/mod.rs`, add (matching the existing style):

```rust
pub mod leak_provision;
```

- [ ] **Step 4: Run the tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::leak_provision -- --nocapture`
Expected: 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/egress/leak_provision.rs core/src/egress/mod.rs
git commit -m "feat(egress): leak_provision — atomic secret_hashes.json writer + audit row

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: `report.rs` — `BlockedCredentialLeak` verdict + `Decision.leak`

**Files:**
- Modify: `workers/egress-proxy/src/report.rs`

- [ ] **Step 1: Write the failing test**

In `workers/egress-proxy/src/report.rs`, add to `mod tests`:

```rust
#[test]
fn credential_leak_verdict_and_fields_serialize() {
    let d = Decision {
        worker: "secret-worker".into(),
        host: "evil.example.com".into(),
        port: 443,
        resolved_ip: Some("203.0.113.9".into()),
        verdict: Verdict::BlockedCredentialLeak,
        reason: "credential leak in request".into(),
        tls_intercepted: true,
        leak: Some(LeakDecision {
            sha256: "ab".repeat(32),
            offset: 42,
            direction: "request".into(),
        }),
    };
    let v: serde_json::Value = serde_json::from_str(&d.to_line()).unwrap();
    assert_eq!(v["verdict"], "blocked_credential_leak");
    assert_eq!(v["leak"]["offset"], 42);
    assert_eq!(v["leak"]["direction"], "request");
}

#[test]
fn leak_absent_when_none() {
    let d = Decision {
        worker: "w".into(), host: "h".into(), port: 1, resolved_ip: None,
        verdict: Verdict::Allowed, reason: "ok".into(), tls_intercepted: false,
        leak: None,
    };
    assert!(!d.to_line().contains("\"leak\""));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy report -- --nocapture`
Expected: FAIL to compile — `Verdict::BlockedCredentialLeak` and `LeakDecision` don't exist, and existing `Decision` literals lack `leak`.

- [ ] **Step 3: Add the verdict variant, the `LeakDecision` struct, and the `Decision.leak` field**

In `workers/egress-proxy/src/report.rs`:

Add the variant to `Verdict`:
```rust
pub enum Verdict {
    Allowed,
    BlockedAllowlist,
    BlockedSsrf,
    /// A materialized secret's verbatim bytes were detected in this connection's
    /// MITM-terminated plaintext (slice #3b). The connection is killed.
    BlockedCredentialLeak,
}
```

Add the `LeakDecision` struct above `Decision`:
```rust
/// Leak detail attached to a [`Verdict::BlockedCredentialLeak`] decision. Carries
/// only the leaked secret's value-hash (hex), the byte offset, and the direction
/// — never any plaintext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LeakDecision {
    /// SHA-256 of the leaked secret value (hex). Matches the provisioned hash.
    pub sha256: String,
    /// Byte offset of the secret's first byte in the scanned direction.
    pub offset: u64,
    /// `"request"` (worker→origin) or `"response"` (origin→worker).
    pub direction: String,
}
```

Add the field to `Decision` (after `tls_intercepted`):
```rust
    /// Present only on [`Verdict::BlockedCredentialLeak`]. Omitted from the wire
    /// otherwise so slice #1/#2/#3a lines are byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leak: Option<LeakDecision>,
```

- [ ] **Step 4: Add `leak: None` to every existing `Decision` literal in this file**

Update each existing `Decision { ... }` in `report.rs` tests (`allowed_line_shape`, `blocked_verdicts_serialize_snake_case` closure, `vec_reporter_collects`, `tls_intercepted_serializes_and_defaults_false`) to add `leak: None,` as the final field.

- [ ] **Step 5: Run the report tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy report -- --nocapture`
Expected: compiles within `report.rs`; the new 2 tests pass and existing 4 still pass. (The crate as a whole won't build until Task 9 updates `proxy.rs` literals — if `cargo test -p` fails to compile `proxy.rs`, that's expected; you can scope-check just this module by temporarily running `cargo build -p kastellan-worker-egress-proxy 2>&1 | grep report.rs` to confirm no errors originate in `report.rs`.)

- [ ] **Step 6: Commit**

```bash
git add workers/egress-proxy/src/report.rs
git commit -m "feat(egress-proxy/report): BlockedCredentialLeak verdict + LeakDecision field

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: `egress/audit.rs` — map the credential-leak line

**Files:**
- Modify: `core/src/egress/audit.rs`

- [ ] **Step 1: Write the failing tests**

In `core/src/egress/audit.rs` `mod tests`, add:

```rust
#[test]
fn credential_leak_maps_to_action_with_redacted_fields() {
    let line = r#"{"worker":"sw","host":"evil.com","port":443,"resolved_ip":"203.0.113.9","verdict":"blocked_credential_leak","reason":"credential leak in request","tls_intercepted":true,"leak":{"sha256":"abab","offset":42,"direction":"request"}}"#;
    let row = decision_to_audit(line).unwrap();
    assert_eq!(row.action, "egress.blocked.credential_leak");
    assert_eq!(row.payload["leaked_sha256"], "abab");
    assert_eq!(row.payload["leak_offset"], 42);
    assert_eq!(row.payload["leak_direction"], "request");
    // Never any plaintext-bearing field.
    assert!(row.payload.get("plaintext").is_none());
}

#[test]
fn credential_leak_without_leak_object_still_maps() {
    // Defensive: a leak verdict line missing the nested object maps with nulls,
    // never panics.
    let line = r#"{"worker":"sw","host":"h","port":443,"resolved_ip":null,"verdict":"blocked_credential_leak","reason":"r"}"#;
    let row = decision_to_audit(line).unwrap();
    assert_eq!(row.action, "egress.blocked.credential_leak");
    assert!(row.payload["leaked_sha256"].is_null());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::audit::tests::credential_leak -- --nocapture`
Expected: FAIL — unknown verdict returns `None`, so `.unwrap()` panics.

- [ ] **Step 3: Extend `DecisionLine` + `decision_to_audit`**

In `core/src/egress/audit.rs`:

Add a nested struct and field to `DecisionLine`:
```rust
#[derive(Debug, Deserialize)]
struct LeakLine {
    sha256: String,
    offset: u64,
    direction: String,
}
```
and inside `struct DecisionLine`, after `tls_intercepted`:
```rust
    /// Present only on a credential-leak line (slice #3b). Absent otherwise.
    #[serde(default)]
    leak: Option<LeakLine>,
```

Add the verdict arm in `decision_to_audit`:
```rust
        "blocked_credential_leak" => "egress.blocked.credential_leak",
```

Extend the payload to carry the redacted leak fields (pull from `d.leak`):
```rust
        payload: serde_json::json!({
            "worker": d.worker,
            "host": d.host,
            "port": d.port,
            "resolved_ip": d.resolved_ip,
            "reason": d.reason,
            "tls_intercepted": d.tls_intercepted,
            "leaked_sha256": d.leak.as_ref().map(|l| l.sha256.clone()),
            "leak_offset": d.leak.as_ref().map(|l| l.offset),
            "leak_direction": d.leak.as_ref().map(|l| l.direction.clone()),
        }),
```

Note: the existing non-leak rows now also carry `leaked_sha256: null` etc. — harmless additive nulls. If you prefer to keep non-leak payloads byte-identical, gate the three fields behind `if d.leak.is_some()`; the simple always-present-null form above is acceptable and is what the tests assert.

- [ ] **Step 4: Run the audit tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::audit -- --nocapture`
Expected: the 2 new tests pass and all existing `audit` tests still pass.

- [ ] **Step 5: Commit**

```bash
git add core/src/egress/audit.rs
git commit -m "feat(egress/audit): map blocked_credential_leak (redacted hash+offset+direction)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Proxy scanning relay + `intercept` rewrite + `run_mitm` mapping + `main.rs`

**Files:**
- Create: `workers/egress-proxy/src/mitm/relay.rs`
- Modify: `workers/egress-proxy/src/mitm.rs`
- Modify: `workers/egress-proxy/src/proxy.rs`
- Modify: `workers/egress-proxy/src/main.rs`

- [ ] **Step 1: Write the failing relay test (hermetic, in-memory duplex)**

Create `workers/egress-proxy/src/mitm/relay.rs`:

```rust
//! Scanning bidirectional relay for the MITM path. Replaces a plain
//! `copy_bidirectional` when secret fingerprints are provisioned: each
//! direction is scanned with its own [`RollingMatcher`] *before* the bytes are
//! forwarded, so the chunk that completes a secret is never relayed. A confirmed
//! hit aborts the relay (best-effort block — earlier bytes may already have been
//! forwarded; the kill denies completion + the response round-trip).

use kastellan_leak_scan::{RollingMatcher, SecretFingerprint};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Which half of the tunnel a leak was found on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// worker → origin (the exfil vector).
    Request,
    /// origin → worker.
    Response,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Request => "request",
            Direction::Response => "response",
        }
    }
}

/// A confirmed leak surfaced by [`scan_relay`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeakReport {
    pub sha256_hex: String,
    pub offset: u64,
    pub direction: Direction,
}

const RELAY_BUF: usize = 16 * 1024;

/// Relay `client` ↔ `upstream`, scanning both directions for `patterns`.
/// Returns `Ok(Some(report))` on a confirmed leak (caller kills the connection),
/// `Ok(None)` on clean EOF of both directions, `Err` on a transport error.
pub async fn scan_relay<C, U>(
    client: C,
    upstream: U,
    patterns: &[SecretFingerprint],
) -> Result<Option<LeakReport>, String>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut ur, mut uw) = tokio::io::split(upstream);
    let mut req = RollingMatcher::new(patterns.to_vec());
    let mut resp = RollingMatcher::new(patterns.to_vec());
    let mut req_buf = vec![0u8; RELAY_BUF];
    let mut resp_buf = vec![0u8; RELAY_BUF];
    let mut req_done = false;
    let mut resp_done = false;

    while !(req_done && resp_done) {
        tokio::select! {
            r = cr.read(&mut req_buf), if !req_done => match r {
                Ok(0) => { let _ = uw.shutdown().await; req_done = true; }
                Ok(n) => {
                    if let Some(hit) = req.feed(&req_buf[..n]) {
                        return Ok(Some(LeakReport {
                            sha256_hex: hit.sha256_hex, offset: hit.offset,
                            direction: Direction::Request,
                        }));
                    }
                    uw.write_all(&req_buf[..n]).await.map_err(|e| format!("relay req write: {e}"))?;
                }
                Err(e) => return Err(format!("relay req read: {e}")),
            },
            r = ur.read(&mut resp_buf), if !resp_done => match r {
                Ok(0) => { let _ = cw.shutdown().await; resp_done = true; }
                Ok(n) => {
                    if let Some(hit) = resp.feed(&resp_buf[..n]) {
                        return Ok(Some(LeakReport {
                            sha256_hex: hit.sha256_hex, offset: hit.offset,
                            direction: Direction::Response,
                        }));
                    }
                    cw.write_all(&resp_buf[..n]).await.map_err(|e| format!("relay resp write: {e}"))?;
                }
                Err(e) => return Err(format!("relay resp read: {e}")),
            },
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_leak_scan::fingerprint_value;
    use tokio::io::AsyncWriteExt;

    fn fp(v: &[u8]) -> SecretFingerprint {
        fingerprint_value(v).unwrap()
    }

    #[tokio::test]
    async fn detects_secret_in_request_direction() {
        let secret = b"exfiltrated-secret-1";
        // client<->upstream wired with in-memory duplex pipes.
        let (client, mut client_peer) = tokio::io::duplex(4096);
        let (upstream, mut upstream_peer) = tokio::io::duplex(4096);
        let patterns = vec![fp(secret)];

        let relay = tokio::spawn(async move { scan_relay(client, upstream, &patterns).await });

        // Worker side writes a request body carrying the secret.
        client_peer
            .write_all(b"POST / HTTP/1.1\r\n\r\nleak=exfiltrated-secret-1")
            .await
            .unwrap();
        // Drop the peers so the relay's reads eventually unblock.
        drop(client_peer);
        drop(upstream_peer);

        let report = relay.await.unwrap().unwrap();
        assert!(report.is_some(), "expected a leak report");
        let report = report.unwrap();
        assert_eq!(report.direction, Direction::Request);
    }

    #[tokio::test]
    async fn clean_traffic_relays_without_report() {
        let (client, mut client_peer) = tokio::io::duplex(4096);
        let (upstream, mut upstream_peer) = tokio::io::duplex(4096);
        let patterns = vec![fp(b"never-sent-secret-9")];
        let relay = tokio::spawn(async move { scan_relay(client, upstream, &patterns).await });

        client_peer.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        // Upstream answers, then both close.
        upstream_peer.write_all(b"HTTP/1.1 200 OK\r\n\r\nbody").await.unwrap();
        drop(client_peer);
        drop(upstream_peer);

        let report = relay.await.unwrap().unwrap();
        assert!(report.is_none(), "clean traffic must produce no report");
    }
}
```

- [ ] **Step 2: Declare the `relay` submodule under `mitm`**

`mitm.rs` currently ends with `#[cfg(test)] mod tests;`. Convert `mitm` into a module with children by adding near the top of `workers/egress-proxy/src/mitm.rs` (after the file doc comment):

```rust
pub mod relay;
```

This makes `crate::mitm::relay` resolve to `workers/egress-proxy/src/mitm/relay.rs`. (Rust resolves `mitm.rs` + `mitm/` dir together — no `mod.rs` rename needed.)

- [ ] **Step 3: Run the relay tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy relay -- --nocapture`
Expected: 2 relay tests PASS. (The crate may not fully build yet because `intercept`/`proxy.rs` are unchanged; if so, the relay module itself should compile — confirm errors are only in `proxy.rs`/`mitm.rs` signatures, then continue.)

- [ ] **Step 4: Rewrite `intercept` to take patterns and return a leak report**

In `workers/egress-proxy/src/mitm.rs`, add the import:
```rust
use kastellan_leak_scan::SecretFingerprint;
use crate::mitm::relay::{scan_relay, LeakReport};
```

Change the `intercept` signature + body. Replace the current signature and step 3 (the `copy_bidirectional` block) with:

```rust
pub async fn intercept(
    worker_side: UnixStream,
    upstream_addr: SocketAddr,
    host: &str,
    ca: &CaMaterial,
    leaf_cache: &mut LeafCache,
    upstream_tls: Arc<rustls::ClientConfig>,
    patterns: &[SecretFingerprint],
) -> Result<Option<LeakReport>, String> {
    use tokio::io::copy_bidirectional;

    // 1. Server-side: present a leaf for `host`, handshake with the worker.
    let server_cfg = leaf_cache.get_or_issue(ca, host)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(server_cfg);
    let mut client_tls = acceptor
        .accept(worker_side)
        .await
        .map_err(|e| format!("worker TLS handshake: {e}"))?;

    // 2. Client-side: re-originate to the pinned origin, validating its real cert.
    let upstream_tcp = tokio::time::timeout(ORIGIN_CONNECT_TIMEOUT, TcpStream::connect(upstream_addr))
        .await
        .map_err(|_| format!("dial origin {upstream_addr}: timed out after {ORIGIN_CONNECT_TIMEOUT:?}"))?
        .map_err(|e| format!("dial origin {upstream_addr}: {e}"))?;
    let connector = tokio_rustls::TlsConnector::from(upstream_tls);
    let sni = upstream_server_name(host)?;
    let mut upstream_tls_stream = connector
        .connect(sni, upstream_tcp)
        .await
        .map_err(|e| format!("origin TLS handshake: {e}"))?;

    // 3. Plaintext flows here. With no provisioned secrets, use the plain copy
    //    (zero scan overhead); otherwise scan both directions (slice #3b).
    if patterns.is_empty() {
        copy_bidirectional(&mut client_tls, &mut upstream_tls_stream)
            .await
            .map_err(|e| format!("tunnel copy: {e}"))?;
        Ok(None)
    } else {
        scan_relay(client_tls, upstream_tls_stream, patterns).await
    }
}
```

- [ ] **Step 5: Add `secret_hashes_path` to `MitmCtx` and load + map in `run_mitm`**

In `workers/egress-proxy/src/proxy.rs`:

Add a field to `MitmCtx`:
```rust
pub struct MitmCtx<'a> {
    pub ca: &'a crate::ca::CaMaterial,
    pub leaf_cache: &'a mut crate::leaf_cache::LeafCache,
    pub upstream_tls: std::sync::Arc<rustls::ClientConfig>,
    /// Path to the host-provisioned `secret_hashes.json` (slice #3b). Re-read
    /// per MITM connection so dispatch-time additions are picked up. `None`
    /// disables scanning entirely.
    pub secret_hashes_path: Option<std::path::PathBuf>,
}
```

Add a loader helper near `run_mitm`:
```rust
/// Lazily load the provisioned secret fingerprints for this connection. A
/// missing/unreadable/empty file degrades to "no scanning" (never an error).
fn load_patterns(path: &Option<std::path::PathBuf>) -> Vec<kastellan_leak_scan::SecretFingerprint> {
    let Some(p) = path else { return Vec::new() };
    match std::fs::read_to_string(p) {
        Ok(s) => kastellan_leak_scan::parse_hashes(&s),
        Err(_) => Vec::new(),
    }
}
```

Rewrite `run_mitm`'s block_on result handling. Replace the `let res = rt.block_on(...)` + trailing `if let Err(e) = res` with:

```rust
    let upstream_tls = std::sync::Arc::clone(&mitm.upstream_tls);
    let patterns = load_patterns(&mitm.secret_hashes_path);
    let res = rt.block_on(async move {
        client.set_nonblocking(true).map_err(|e| format!("client nonblocking: {e}"))?;
        let client = tokio::net::UnixStream::from_std(client)
            .map_err(|e| format!("client from_std: {e}"))?;
        crate::mitm::intercept(
            client,
            SocketAddr::new(ip, port),
            host,
            mitm.ca,
            mitm.leaf_cache,
            upstream_tls,
            &patterns,
        )
        .await
    });
    match res {
        Ok(None) => {} // clean tunnel; the allow decision was already emitted.
        Ok(Some(report)) => {
            reporter.report(Decision {
                worker: worker.into(),
                host: host.into(),
                port,
                resolved_ip: Some(ip.to_string()),
                verdict: Verdict::BlockedCredentialLeak,
                reason: format!("credential leak in {}", report.direction.as_str()),
                tls_intercepted: true,
                leak: Some(crate::report::LeakDecision {
                    sha256: report.sha256_hex,
                    offset: report.offset,
                    direction: report.direction.as_str().to_string(),
                }),
            });
        }
        Err(e) => {
            reporter.report(Decision {
                worker: worker.into(),
                host: host.into(),
                port,
                resolved_ip: Some(ip.to_string()),
                verdict: Verdict::Allowed,
                reason: format!("mitm_failed: {e}"),
                tls_intercepted: true,
                leak: None,
            });
        }
    }
```

- [ ] **Step 6: Add `leak: None` to every other `Decision` literal in `proxy.rs`**

Update each existing `Decision { ... }` literal in `proxy.rs` (`handle_conn`'s connect-failed arm, the `allowed_but_200_write_failed` arm, the TLS-allowed arm, the non-TLS-allowed arm, the `mitm_runtime_failed` arm in `run_mitm`, and the `blocked` helper) to add `leak: None,` as the final field. There are 6 such literals besides the two you wrote in Step 5.

- [ ] **Step 7: Set `secret_hashes_path` in `main.rs`**

In `workers/egress-proxy/src/main.rs`, where `MitmCtx` is constructed inside the accept loop, derive the path as a sibling of the UDS (same dir as `ca.pem`) and set the field. Find the `let mut mitm = MitmCtx { ... };` line and add:

```rust
                    secret_hashes_path: Some(
                        Path::new(&uds).parent().unwrap().join("secret_hashes.json"),
                    ),
```

Confirm `use std::path::Path;` is in scope in `main.rs` (the CA path code already uses `Path::new(&uds).parent()`). The file name string MUST equal `kastellan_core`'s `leak_provision::SECRET_HASHES_FILE_NAME` value `"secret_hashes.json"` — they are independent string literals on the two sides of the spawn boundary, pinned by the e2e in Task 11.

- [ ] **Step 8: Build + test the whole proxy crate**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy -- --nocapture`
Expected: all proxy tests pass (report, audit-free; relay; existing mitm/proxy/ssrf/ca/leaf_cache units).

- [ ] **Step 9: Clippy the proxy crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-worker-egress-proxy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 10: Commit**

```bash
git add workers/egress-proxy/src/mitm.rs workers/egress-proxy/src/mitm/relay.rs workers/egress-proxy/src/proxy.rs workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): scanning MITM relay + leak decision wiring

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Spawn-wiring — provision the file at sidecar spawn

**Files:**
- Modify: `core/src/egress/net_worker.rs`

- [ ] **Step 1: Write the failing test**

In `core/src/egress/net_worker.rs` `mod tests`, add a test that provisioning writes the file into the scratch dir. Because the real spawn needs a sandbox, test the smaller, pure write seam wired through a helper. Add:

```rust
#[test]
fn provision_writes_secret_hashes_into_scratch() {
    use kastellan_leak_scan::{fingerprint_value, parse_hashes};
    let dir = tempfile::tempdir().expect("scratch");
    let fps = vec![fingerprint_value(b"a-spawn-time-secret").unwrap()];
    provision_secret_hashes(dir.path(), &fps).expect("write");
    let s = std::fs::read_to_string(dir.path().join("secret_hashes.json")).unwrap();
    assert_eq!(parse_hashes(&s), fps);
}

#[test]
fn provision_empty_writes_empty_list() {
    use kastellan_leak_scan::parse_hashes;
    let dir = tempfile::tempdir().expect("scratch");
    provision_secret_hashes(dir.path(), &[]).expect("write");
    let s = std::fs::read_to_string(dir.path().join("secret_hashes.json")).unwrap();
    assert!(parse_hashes(&s).is_empty());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::net_worker::tests::provision -- --nocapture`
Expected: FAIL — `provision_secret_hashes` not defined.

- [ ] **Step 3: Add the helper + thread the param through both spawn fns**

In `core/src/egress/net_worker.rs`:

Add imports near the top:
```rust
use kastellan_leak_scan::SecretFingerprint;
use super::leak_provision::write_secret_hashes;
```

Add a thin internal helper (so the test doesn't need a real spawn):
```rust
/// Write the per-worker secret-value fingerprints into the sidecar scratch dir
/// for the proxy to read (slice #3b spawn-wiring). Thin wrapper over
/// [`super::leak_provision::write_secret_hashes`] kept here so the spawn path
/// has one provisioning call site.
fn provision_secret_hashes(scratch: &Path, fps: &[SecretFingerprint]) -> std::io::Result<()> {
    write_secret_hashes(scratch, fps)
}
```

Add `secret_fingerprints: &[SecretFingerprint]` as a parameter to `spawn_net_worker` (after `worker_name`) and to `spawn_forced_net_worker` (after `worker_name`). In `spawn_net_worker`, immediately after the sidecar spawns successfully (after `let stdout = sidecar.stdout();`), provision the file into the scratch dir (the UDS's parent):

```rust
    // Provision the credential-leak scanner's fingerprints into the sidecar
    // scratch dir (slice #3b). Best-effort: a provisioning write failure must
    // not abort an otherwise-healthy worker — it only disables leak scanning,
    // which is defense-in-depth, not a containment boundary. Today's callers
    // pass an empty slice (no egress worker carries secrets yet).
    if let Some(scratch_dir) = sidecar.uds_path.parent() {
        if let Err(e) = provision_secret_hashes(scratch_dir, secret_fingerprints) {
            tracing::warn!(error = %e, "egress leak-scan provisioning write failed; scanning disabled for this worker");
        }
    }
```

In `spawn_forced_net_worker`, forward the new argument to the inner `spawn_net_worker(... worker_name, secret_fingerprints, on_decision)` call.

- [ ] **Step 4: Update existing callers to pass `&[]`**

Find every caller of `spawn_net_worker` / `spawn_forced_net_worker`:

Run: `grep -rn "spawn_net_worker\|spawn_forced_net_worker" --include=*.rs core/ | grep -v "fn spawn_"`
Expected: the two `mod tests` call sites in `net_worker.rs` (`spawn_net_worker_fails_closed_when_sidecar_unavailable`, `spawn_forced_net_worker_fails_closed_when_sidecar_unavailable`, `spawn_forced_net_worker_cleans_scratch_on_failure`) plus any in `core/src/worker_lifecycle/force_route.rs` and `core/tests/egress_force_routing_e2e.rs`. Add `&[]` as the new argument (after `worker_name`, i.e. before the `on_decision` closure) at each call site. Example edit for the test arms in `net_worker.rs`:

```rust
        let res = spawn_net_worker(
            &backend,
            Path::new("/nonexistent/egress-proxy"),
            &spec,
            &["api.example.com:443".to_string()],
            Path::new("/tmp/kastellan-net-worker-test"),
            "web-fetch",
            &[],            // secret_fingerprints — none for this fail-closed test
            |_row| {},
        );
```

For `spawn_forced_net_worker` call sites add `&[]` after `worker_name` likewise.

If `core/src/worker_lifecycle/force_route.rs` calls `spawn_forced_net_worker` (it wires the live auto-flip), update that call to pass `&[]` too, with a comment: `// secret_fingerprints: dispatch-time provisioning is the deferred follow-up (#NNN)`.

- [ ] **Step 5: Run the net_worker tests + the force_route tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::net_worker worker_lifecycle::force_route -- --nocapture`
Expected: all pass, including the 2 new provisioning tests.

- [ ] **Step 6: Build the whole workspace to catch any missed call site**

Run: `source "$HOME/.cargo/env" && cargo build --workspace`
Expected: builds. If a call site was missed, the compiler names it — add `&[]` there.

- [ ] **Step 7: Commit**

```bash
git add core/src/egress/net_worker.rs core/src/worker_lifecycle/force_route.rs core/tests/egress_force_routing_e2e.rs
git commit -m "feat(egress/net_worker): provision secret_hashes.json at sidecar spawn

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

(Stage only the files you actually changed — drop `force_route.rs`/`egress_force_routing_e2e.rs` from the `git add` if they had no call site.)

---

## Task 11: End-to-end provisioning test

**Files:**
- Create: `core/tests/egress_leak_scan_e2e.rs`

The deterministic through-the-MITM leak-block is already proven by the hermetic `scan_relay` duplex tests (Task 9) and the exhaustive `RollingMatcher` units (Task 3). This e2e pins the **cross-boundary contract**: the file `core` writes is the file the proxy reads, under the real spawn path, with the agreed name + shape.

- [ ] **Step 1: Write the test**

Create `core/tests/egress_leak_scan_e2e.rs`:

```rust
//! Egress slice #3b — cross-boundary provisioning contract.
//!
//! Pins that `core`'s `leak_provision::write_secret_hashes` produces exactly the
//! file shape + name the egress proxy reads back via `kastellan_leak_scan`'s
//! wire parser. The streaming detection itself is covered hermetically by the
//! egress-proxy `scan_relay` duplex tests and the `RollingMatcher` units; this
//! guards the contract those two sides agree on (the file name is an independent
//! string literal in `egress-proxy::main` and `core::egress::leak_provision`).

use kastellan_core::egress::leak_provision::{write_secret_hashes, SECRET_HASHES_FILE_NAME};
use kastellan_leak_scan::{fingerprint_value, parse_hashes};

#[test]
fn provisioned_file_round_trips_through_the_proxy_parser() {
    let dir = tempfile::tempdir().expect("scratch");
    let fps = vec![
        fingerprint_value(b"cross-boundary-secret-1").unwrap(),
        fingerprint_value(b"cross-boundary-secret-22").unwrap(),
    ];
    write_secret_hashes(dir.path(), &fps).expect("provision");

    // The proxy reads exactly this file name from the UDS sibling dir.
    let path = dir.path().join(SECRET_HASHES_FILE_NAME);
    assert_eq!(SECRET_HASHES_FILE_NAME, "secret_hashes.json");
    let body = std::fs::read_to_string(&path).expect("read provisioned file");

    // The proxy's parser recovers the exact fingerprints core wrote.
    let recovered = parse_hashes(&body);
    assert_eq!(recovered, fps);
}

#[test]
fn empty_provisioning_is_safe_no_scanning() {
    let dir = tempfile::tempdir().expect("scratch");
    write_secret_hashes(dir.path(), &[]).expect("provision empty");
    let body = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
    assert!(parse_hashes(&body).is_empty(), "empty file => proxy scans nothing");
}
```

Note: this requires `leak_provision` and `SECRET_HASHES_FILE_NAME` to be reachable as `kastellan_core::egress::leak_provision::*`. Confirm `core/src/lib.rs` exposes `pub mod egress;` (it does — `egress::audit` etc. are used by e2e tests already) and that `leak_provision` is `pub mod` (Task 6). `SECRET_HASHES_FILE_NAME` and `write_secret_hashes` are already `pub` (Task 6).

- [ ] **Step 2: Run the e2e**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test egress_leak_scan_e2e -- --nocapture`
Expected: 2 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add core/tests/egress_leak_scan_e2e.rs
git commit -m "test(egress): slice #3b cross-boundary provisioning contract e2e

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: Full-workspace verification + clippy

**Files:** none (verification only).

- [ ] **Step 1: Full workspace test**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -30`
Expected: all green on macOS skip-as-pass. Record the passed/failed/ignored counts for the HANDOVER update. (Live-PG suites skip-as-pass without `KASTELLAN_PG_BIN_DIR` — that's expected; the standing macOS embedding_recall flake under a full PG run is documented in HANDOVER and is not a regression.)

- [ ] **Step 2: Full workspace clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20`
Expected: clean.

- [ ] **Step 3: LOC census of touched files**

Run: `wc -l leak-scan/src/*.rs core/src/egress/leak_provision.rs workers/egress-proxy/src/mitm/relay.rs workers/egress-proxy/src/{mitm,proxy,report,main}.rs core/src/egress/{audit,net_worker}.rs core/src/secrets/vault.rs`
Expected: every file < 500 LOC. If `proxy.rs` crossed the cap (it was 339 + the run_mitm rewrite), lift the `run_mitm` leak-decision construction into a small `fn leak_decision(...) -> Decision` helper or move `load_patterns` + the result-mapping into a `proxy/mitm_run.rs` sibling; otherwise leave as-is.

- [ ] **Step 4: Cross-compile clippy the Linux-gated paths (Mac-side pre-CI check)**

The new code is platform-agnostic, but verify the pure crate + supervisor still cross-clippy (per the cross-clippy memory; `core` cannot cross-compile due to `ring`):

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-leak-scan --target aarch64-unknown-linux-gnu -- -D warnings`
Expected: clean (pure Rust, no linker needed).

---

## Task 13: File the deferred follow-up issue + update HANDOVER/ROADMAP

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: File the dispatch-append deferral issue**

```bash
gh issue create --title "Egress #3b: dispatch-time live-append of secret-value hashes" \
  --body "Slice #3b (PR <this PR>) shipped the credential-leak scanner mechanism + spawn-time provisioning (empty for today's secret-free egress workers). The correct live wiring — append a secret's value-hash to the worker's <scratch>/secret_hashes.json the moment \`tool_host::dispatch\` → \`substitute_refs_in_params\` materializes it — is deferred until the first secret-bearing egress worker lands (which will shape its exact threading of the sidecar scratch path onto the worker handle). See docs/superpowers/specs/2026-06-12-egress-proxy-slice3b-credential-leak-scanner-design.md §9."
```

Record the issue number; replace the `#NNN` placeholder in the `force_route.rs` comment (Task 10 Step 4) if you added one, in a follow-up commit.

- [ ] **Step 2: Update HANDOVER.md**

Follow the HANDOVER "How to update" checklist: bump `Last updated`, `Session-end verification` (the Task 12 counts), move this slice into "Recently completed" with file paths + the deferred-issue link, write a fresh "Next TODO" (slice #3b done → next egress is slice #4 TLS pinning, or browser-driver Phase 2), and add the new `leak-scan` crate to the "Working state" crate tree (now 14 crates).

- [ ] **Step 3: Tick ROADMAP:142**

Mark the credential-leak-scanner sub-item of ROADMAP:142 done with the merge commit hash (fill after merge).

- [ ] **Step 4: Commit the docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): egress slice #3b leak scanner shipped; next = slice #4

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Push + open the PR**

```bash
git push -u origin feat/egress-slice3b-leak-scanner
gh pr create --base main --title "Egress slice #3b: co-located credential-leak scanner" --body "$(cat <<'EOF'
Implements egress proxy slice #3b (ROADMAP:142): a co-located credential-leak scanner on slice #3a's MITM-terminated plaintext.

## What
- New pure crate `kastellan-leak-scan` (single source of truth): `SecretFingerprint` + `fingerprint_value` (Rabin + SHA-256), streaming `RollingMatcher` (rolling pre-filter + SHA-256 confirm + cross-read carry-over), `secret_hashes.json` serde.
- Host: `Vault::value_fingerprint` (one-way, no plaintext), `core::egress::leak_provision` (atomic file writer + provisioning audit row), spawn-time provisioning into the sidecar scratch dir.
- Proxy: lazy per-connection file read, scanning bidirectional relay replacing `copy_bidirectional`, `Verdict::BlockedCredentialLeak` + redacted decision (hash+offset+direction, never plaintext), host-side audit mapping.

## Decisions (see spec)
Hashes-only detection; scratch-file lazy-re-read provisioning; best-effort streaming block with carry-over; mechanism + spawn-wire with dispatch-append deferred (#NNN).

## Limitations (defense-in-depth, not a perfect exfil barrier)
Exact-contiguous-byte detection only (encoding / cross-request splitting evade it); best-effort block doesn't recall an already-relayed in-body prefix. Spawn-wiring provisions an empty set today — no current egress worker carries secrets.

Spec: docs/superpowers/specs/2026-06-12-egress-proxy-slice3b-credential-leak-scanner-design.md

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-review checklist (completed during plan authoring)

- **Spec coverage:** §5.1 RollingMatcher → Task 3; §5.2 relay → Task 9; §5.3 proxy wiring → Task 9; §5.4 report → Task 7; §5.5 leak_provision → Task 6; §5.6 Vault → Task 5; §5.7 spawn-wiring → Task 10; §5.8 audit → Task 8; §6 tests → embedded per task + Task 11; §9 deferral → Task 13. Shared-crate decision (the web-common dep-tree finding) → Task 0/4.
- **Type consistency:** `SecretFingerprint{len,fp64,sha256}` identical across crates (single definition); `LeakHit{sha256_hex,offset}` (matcher) maps to `LeakReport{sha256_hex,offset,direction}` (relay) maps to `LeakDecision{sha256,offset,direction}` (wire) maps to `leaked_sha256/leak_offset/leak_direction` (audit payload) — names intentionally differ per layer, conversions shown explicitly. `intercept` return type changed to `Result<Option<LeakReport>,String>` and every caller (`run_mitm`) updated.
- **No placeholders:** all code shown in full; the only `#NNN` is the not-yet-filed issue number (Task 13 Step 1 creates it).
