# Operator egress cert-pin plumbing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an operator configure TLS cert pins (via `KASTELLAN_EGRESS_CERT_PINS`) that force-routed tool workers enforce against their upstream hosts, completing the slice-#4 seam that dead-ends at `spawn_worker_maybe_forced`'s hard-coded `cert_pins_json: None`.

**Architecture:** A new pure module `core/src/egress/cert_pins.rs` parses the operator pin JSON (structural validation; the proxy stays the authoritative strict validator) and selects the per-worker subset by allowlist host (least-privilege). `force_route::from_env` reads + validates the env at daemon startup (fail-closed), stores a `CertPinMap` in `ForceRoutingConfig`, and `spawn_worker_maybe_forced` hands each sidecar only the pins for hosts that worker may dial.

**Tech Stack:** Rust (edition 2021, rustc 1.96), `serde_json`, `thiserror`. No new dependencies.

## Global Constraints

- AGPL-3.0 project; AGPL-compatible deps only. **No new dependencies are introduced by this plan.**
- Cross-platform: no Linux-only or macOS-only code. This change is OS-agnostic (pure parsing + config threading; the OS-specific sidecar spawn is unchanged).
- Keep files under 500 LOC where feasible; `force_route.rs` is already 583 LOC, so all new pure logic goes in the new `cert_pins.rs`.
- TDD: failing test first, minimal implementation, green, commit.
- Pin JSON shape (operator-facing, identical to the proxy's `KASTELLAN_EGRESS_PROXY_PINS`): `{"host":["sha256/<base64-SPKI>", ...], ...}`.
- Cert pins are public integrity data, **not secrets** — env var is the deliberate, approved source.
- All tests must pass (`cargo test --workspace`) and `cargo clippy --workspace --all-targets -D warnings` must be clean before the final commit.
- Source the cargo env first in every shell: `source "$HOME/.cargo/env"`.

---

### Task 1: `cert_pins.rs` — types + structural parse

**Files:**
- Create: `core/src/egress/cert_pins.rs`
- Modify: `core/src/egress/mod.rs` (add the module declaration + a doc line)
- Test: inline `#[cfg(test)] mod tests` in `core/src/egress/cert_pins.rs`

**Interfaces:**
- Consumes: nothing (leaf module).
- Produces:
  - `pub struct CertPinMap` (newtype over `BTreeMap<String, Vec<String>>`) with `pub fn is_empty(&self) -> bool`; derives `Debug, Clone, PartialEq, Eq, Default`.
  - `pub enum CertPinError` (derives `Debug, thiserror::Error, PartialEq, Eq`): variants `Shape(String)`, `EmptyPinList(String)`, `BadPrefix { host: String, pin: String }`.
  - `pub fn parse_cert_pins(json: &str) -> Result<CertPinMap, CertPinError>`.

- [ ] **Step 1: Add the module declaration to `core/src/egress/mod.rs`**

Add a doc bullet under the existing list (after the `net_worker` bullet, before the closing paragraph):

```rust
//!   - [`cert_pins`]: parse the operator `KASTELLAN_EGRESS_CERT_PINS` config and
//!     select the per-worker pin subset handed to each sidecar (slice #4).
```

And add the module to the `pub mod` list (keep alphabetical-ish with siblings):

```rust
pub mod audit;
pub mod cert_pins;
pub mod leak_provision;
pub mod net_worker;
pub mod spawn;
```

- [ ] **Step 2: Write the failing test (create `core/src/egress/cert_pins.rs` with tests + empty impl stubs)**

Create `core/src/egress/cert_pins.rs`:

```rust
//! Operator cert-pin config: parse `KASTELLAN_EGRESS_CERT_PINS` (the same
//! `{host:["sha256/<b64>"]}` JSON the egress-proxy sidecar enforces) into a
//! host-keyed map, and select the per-worker subset to hand each sidecar.
//!
//! Layering: this host-side parse is **structural only** — it checks the JSON
//! shape and the `sha256/` prefix so a malformed config fails the daemon closed
//! at startup, and so pins can be selected per worker. The authoritative strict
//! validation (base64 decode, 32-byte SPKI length) lives in the egress-proxy's
//! `PinSet::parse`; a pin with a good prefix but bad base64 passes here and
//! fails closed one layer later, at sidecar startup. Keeping one strict
//! validator (the proxy's) avoids drift.

use std::collections::BTreeMap;

/// Prefix every pin string must carry (RFC-7469 `sha256/<base64-SPKI>`).
const PIN_PREFIX: &str = "sha256/";

/// A parsed, structurally-valid operator pin config: lowercased host → its
/// non-empty list of `sha256/<b64>` pin strings.
///
/// Invariant: every value vec is non-empty (empty arrays are rejected by
/// [`parse_cert_pins`]). An all-empty *map* is possible only from `{}`; callers
/// normalize that to "no pins".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CertPinMap(BTreeMap<String, Vec<String>>);

impl CertPinMap {
    /// True when no hosts are pinned (`{}`).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Structural failure parsing `KASTELLAN_EGRESS_CERT_PINS`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CertPinError {
    /// Not valid JSON, or not a JSON object of host -> array-of-strings.
    #[error("cert-pin config must be a JSON object of host -> [\"sha256/...\"]: {0}")]
    Shape(String),
    /// A host mapped to an empty pin array — unsatisfiable; almost always a
    /// misconfiguration (matches the proxy's own rejection).
    #[error("host {0:?} has an empty pin list")]
    EmptyPinList(String),
    /// A pin string did not start with the required `sha256/` prefix.
    #[error("host {host:?} pin {pin:?} is missing the `sha256/` prefix")]
    BadPrefix { host: String, pin: String },
}

/// Parse + structurally validate the operator pin JSON. See the module doc for
/// the layering (structural here; strict validation in the proxy).
pub fn parse_cert_pins(_json: &str) -> Result<CertPinMap, CertPinError> {
    todo!("implemented in step 4")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_map_and_lowercases_hosts() {
        let m = parse_cert_pins(r#"{"API.Example.com":["sha256/AAAA"]}"#).unwrap();
        assert!(!m.is_empty());
        // Host key is lowercased.
        let round = parse_cert_pins(r#"{"api.example.com":["sha256/AAAA"]}"#).unwrap();
        assert_eq!(m, round);
    }

    #[test]
    fn empty_object_is_empty_map() {
        let m = parse_cert_pins("{}").unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn rejects_empty_pin_array() {
        let err = parse_cert_pins(r#"{"h.com":[]}"#).unwrap_err();
        assert_eq!(err, CertPinError::EmptyPinList("h.com".to_string()));
    }

    #[test]
    fn rejects_missing_sha256_prefix() {
        let err = parse_cert_pins(r#"{"h.com":["nope"]}"#).unwrap_err();
        assert_eq!(
            err,
            CertPinError::BadPrefix { host: "h.com".to_string(), pin: "nope".to_string() }
        );
    }

    #[test]
    fn rejects_non_object_shape() {
        assert!(matches!(parse_cert_pins("[]").unwrap_err(), CertPinError::Shape(_)));
        assert!(matches!(parse_cert_pins("\"x\"").unwrap_err(), CertPinError::Shape(_)));
        assert!(matches!(parse_cert_pins("{\"h\":5}").unwrap_err(), CertPinError::Shape(_)));
    }

    #[test]
    fn accepts_multiple_pins_per_host() {
        let m = parse_cert_pins(r#"{"h.com":["sha256/A","sha256/B"]}"#).unwrap();
        assert!(!m.is_empty());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::cert_pins -- --nocapture`
Expected: the tests compile but panic at `todo!("implemented in step 4")` (FAIL).

- [ ] **Step 4: Implement `parse_cert_pins`**

Replace the `todo!` body with:

```rust
pub fn parse_cert_pins(json: &str) -> Result<CertPinMap, CertPinError> {
    // serde rejects any non-object / non-array-of-strings shape for us.
    let raw: BTreeMap<String, Vec<String>> =
        serde_json::from_str(json).map_err(|e| CertPinError::Shape(e.to_string()))?;
    let mut out = BTreeMap::new();
    for (host, pins) in raw {
        if pins.is_empty() {
            return Err(CertPinError::EmptyPinList(host));
        }
        for pin in &pins {
            if !pin.starts_with(PIN_PREFIX) {
                return Err(CertPinError::BadPrefix { host: host.clone(), pin: pin.clone() });
            }
        }
        // DNS is case-insensitive; the proxy matches lowercased hosts.
        out.insert(host.to_ascii_lowercase(), pins);
    }
    Ok(CertPinMap(out))
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::cert_pins -- --nocapture`
Expected: PASS (6 tests).

- [ ] **Step 6: Commit**

```bash
git add core/src/egress/cert_pins.rs core/src/egress/mod.rs
git commit -m "feat(egress): structural parse for operator cert-pin config

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `cert_pins.rs` — host extraction + per-worker selection

**Files:**
- Modify: `core/src/egress/cert_pins.rs` (add two functions + tests)
- Test: inline `#[cfg(test)] mod tests` in the same file

**Interfaces:**
- Consumes: `CertPinMap`, `parse_cert_pins` (Task 1).
- Produces:
  - `pub fn host_of_endpoint(endpoint: &str) -> &str`.
  - `pub fn select_pins_for_allowlist(map: &CertPinMap, allowlist: &[String]) -> Option<String>` — returns the `{host:[...]}` JSON for the allowlist∩pinned hosts, or `None` when empty.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `core/src/egress/cert_pins.rs`:

```rust
    #[test]
    fn host_of_endpoint_strips_port() {
        assert_eq!(host_of_endpoint("api.example.com:443"), "api.example.com");
    }

    #[test]
    fn host_of_endpoint_handles_bracketed_ipv6() {
        assert_eq!(host_of_endpoint("[2001:db8::1]:8443"), "2001:db8::1");
        assert_eq!(host_of_endpoint("[::1]:443"), "::1");
    }

    #[test]
    fn host_of_endpoint_bare_host_unchanged() {
        assert_eq!(host_of_endpoint("example.com"), "example.com");
    }

    #[test]
    fn select_keeps_only_allowlisted_hosts() {
        let map = parse_cert_pins(
            r#"{"a.com":["sha256/A"],"b.com":["sha256/B"]}"#,
        )
        .unwrap();
        let json = select_pins_for_allowlist(&map, &["a.com:443".to_string()]).unwrap();
        // Round-trips to exactly the a.com subset.
        let expected = parse_cert_pins(r#"{"a.com":["sha256/A"]}"#).unwrap();
        assert_eq!(parse_cert_pins(&json).unwrap(), expected);
    }

    #[test]
    fn select_is_case_insensitive_on_host() {
        let map = parse_cert_pins(r#"{"a.com":["sha256/A"]}"#).unwrap();
        let json = select_pins_for_allowlist(&map, &["A.COM:443".to_string()]).unwrap();
        assert_eq!(parse_cert_pins(&json).unwrap(), map);
    }

    #[test]
    fn select_no_intersection_is_none() {
        let map = parse_cert_pins(r#"{"a.com":["sha256/A"]}"#).unwrap();
        assert!(select_pins_for_allowlist(&map, &["z.com:443".to_string()]).is_none());
        assert!(select_pins_for_allowlist(&map, &[]).is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::cert_pins`
Expected: FAIL to compile — `host_of_endpoint` / `select_pins_for_allowlist` not found.

- [ ] **Step 3: Implement the two functions**

Add to `core/src/egress/cert_pins.rs` (after `parse_cert_pins`, before the `tests` module). Note the added `use`:

```rust
use std::collections::HashSet;

/// Extract the host from an allowlist endpoint (`host:port`).
///
/// Allowlist entries are `host:port` (the shape the proxy + web-common use);
/// pins are keyed by bare host, so selection matches on the host alone. IPv6
/// literals must be bracketed (`[2001:db8::1]:443`) — the same convention the
/// allowlist uses. A bare host with no port is returned unchanged.
pub fn host_of_endpoint(endpoint: &str) -> &str {
    if let Some(rest) = endpoint.strip_prefix('[') {
        // `[ipv6]:port` or `[ipv6]` — host is between the brackets.
        if let Some(end) = rest.find(']') {
            return &rest[..end];
        }
        return endpoint; // malformed bracket; hand back as-is
    }
    match endpoint.rsplit_once(':') {
        Some((host, _port)) => host,
        None => endpoint,
    }
}

/// Select the subset of `map` whose hosts appear in this worker's `allowlist`,
/// serialized back to the proxy's `{host:[...]}` JSON. Returns `None` when no
/// pinned host is in the allowlist, so the sidecar gets no pin env and the
/// no-pin path stays byte-identical.
///
/// Least-privilege: a worker's sidecar only learns pins for hosts that worker
/// may actually dial.
pub fn select_pins_for_allowlist(map: &CertPinMap, allowlist: &[String]) -> Option<String> {
    let hosts: HashSet<String> = allowlist
        .iter()
        .map(|ep| host_of_endpoint(ep).to_ascii_lowercase())
        .collect();
    let selected: BTreeMap<&String, &Vec<String>> =
        map.0.iter().filter(|(host, _)| hosts.contains(*host)).collect();
    if selected.is_empty() {
        return None;
    }
    // BTreeMap<&String,&Vec<String>> serializes as the same {host:[...]} object
    // the proxy parses. Serialization of an owned, in-memory map cannot fail.
    Some(serde_json::to_string(&selected).expect("pin submap serializes"))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::cert_pins`
Expected: PASS (12 tests total).

- [ ] **Step 5: Commit**

```bash
git add core/src/egress/cert_pins.rs
git commit -m "feat(egress): per-worker cert-pin selection by allowlist host

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: thread `cert_pins` into `ForceRoutingConfig` + `from_env`

**Files:**
- Modify: `core/src/worker_lifecycle/force_route.rs` (field, `new` param, `ForceRoutingError`, `resolve_force_routing` param, `from_env` parse, `parse_cert_pins_env` helper, update existing test helpers)
- Modify: `core/src/worker_lifecycle/mod.rs:26` (re-export `ForceRoutingError`)
- Test: inline `#[cfg(test)] mod tests` in `force_route.rs`

**Interfaces:**
- Consumes: `CertPinMap`, `CertPinError`, `parse_cert_pins` (Tasks 1–2).
- Produces:
  - `ForceRoutingConfig` gains `pub(crate) cert_pins: Option<CertPinMap>` (`Some` ⇒ non-empty).
  - `ForceRoutingConfig::new(proxy_bin, scratch_root, make_sink, cert_pins: Option<CertPinMap>)`.
  - `pub enum ForceRoutingError { ProxyBinaryNotFound(#[from] ProxyBinaryNotFound), CertPins(#[from] CertPinError) }`.
  - `resolve_force_routing(enabled, proxy_bin, scratch_root, make_sink, cert_pins: Option<CertPinMap>) -> Result<Option<ForceRoutingConfig>, ProxyBinaryNotFound>`.
  - `from_env(...) -> Result<Option<Arc<ForceRoutingConfig>>, ForceRoutingError>`.
  - `fn parse_cert_pins_env(value: Option<&str>) -> Result<Option<CertPinMap>, CertPinError>` (module-private; the testable seam).

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `core/src/worker_lifecycle/force_route.rs`:

```rust
    #[test]
    fn parse_cert_pins_env_handles_absent_blank_and_empty() {
        assert!(parse_cert_pins_env(None).unwrap().is_none());
        assert!(parse_cert_pins_env(Some("")).unwrap().is_none());
        assert!(parse_cert_pins_env(Some("   ")).unwrap().is_none());
        // `{}` is valid but empty → normalized to None (no pins).
        assert!(parse_cert_pins_env(Some("{}")).unwrap().is_none());
    }

    #[test]
    fn parse_cert_pins_env_parses_valid_map() {
        let got = parse_cert_pins_env(Some(r#"{"a.com":["sha256/A"]}"#))
            .unwrap()
            .expect("non-empty map => Some");
        assert!(!got.is_empty());
    }

    #[test]
    fn parse_cert_pins_env_fails_closed_on_malformed() {
        let err = parse_cert_pins_env(Some(r#"{"a.com":[]}"#)).unwrap_err();
        assert!(matches!(err, crate::egress::cert_pins::CertPinError::EmptyPinList(_)));
    }

    #[test]
    fn resolve_force_routing_stores_cert_pins() {
        let pins = parse_cert_pins_env(Some(r#"{"a.com":["sha256/A"]}"#)).unwrap();
        let cfg = resolve_force_routing(
            true,
            Some(PathBuf::from("/opt/egress-proxy")),
            PathBuf::from("/tmp"),
            noop_sink_factory(),
            pins.clone(),
        )
        .expect("ok")
        .expect("some");
        assert_eq!(cfg.cert_pins, pins);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib force_route`
Expected: FAIL to compile — `parse_cert_pins_env` not found, `resolve_force_routing` arity mismatch, `cfg.cert_pins` missing.

- [ ] **Step 3: Add imports + the `ENV_CERT_PINS` constant**

At the top of `core/src/worker_lifecycle/force_route.rs`, add to the `use` block:

```rust
use crate::egress::cert_pins::{parse_cert_pins, CertPinError, CertPinMap};
```

Add alongside the other env-var constants (after `ENV_SCRATCH_DIR`):

```rust
/// Optional operator cert-pin config for force-routed workers (slice #4). Same
/// `{host:["sha256/<b64>"]}` JSON the egress-proxy sidecar enforces. Validated
/// fail-closed at startup; selected per worker by allowlist host.
const ENV_CERT_PINS: &str = "KASTELLAN_EGRESS_CERT_PINS";
```

- [ ] **Step 4: Add the `cert_pins` field + update `new`**

In the `ForceRoutingConfig` struct, add the field after `make_sink`:

```rust
    /// Operator cert pins for force-routed workers (slice #4). `Some` ⇒
    /// non-empty (an empty/`{}` config normalizes to `None` in [`from_env`]).
    /// Selected per worker by allowlist host in [`ForceRoutingConfig::pins_for`]
    /// (Task 4) and handed to the sidecar via `cert_pins_json`.
    pub(crate) cert_pins: Option<CertPinMap>,
```

Update `ForceRoutingConfig::new`:

```rust
    pub fn new(
        proxy_bin: PathBuf,
        scratch_root: PathBuf,
        make_sink: DecisionSinkFactory,
        cert_pins: Option<CertPinMap>,
    ) -> Self {
        Self { proxy_bin, scratch_root, make_sink, cert_pins }
    }
```

- [ ] **Step 5: Add `ForceRoutingError` + the `parse_cert_pins_env` helper**

Add after the `ProxyBinaryNotFound` definition:

```rust
/// Error building the force-routing config from the environment. Either the
/// proxy binary was missing (fail-closed) or the cert-pin config was malformed
/// (fail-closed). Mapped to `anyhow` at the `main.rs` startup call site.
#[derive(Debug, thiserror::Error)]
pub enum ForceRoutingError {
    #[error(transparent)]
    ProxyBinaryNotFound(#[from] ProxyBinaryNotFound),
    #[error("invalid {} config: {0}", ENV_CERT_PINS)]
    CertPins(#[from] CertPinError),
}

/// Pure: turn the raw `KASTELLAN_EGRESS_CERT_PINS` env value into an optional
/// parsed map. Unset, blank, or `{}` → `None` (no pins); a non-empty valid map →
/// `Some(map)`; malformed → `Err` (the daemon fails closed at startup).
fn parse_cert_pins_env(value: Option<&str>) -> Result<Option<CertPinMap>, CertPinError> {
    let Some(raw) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let map = parse_cert_pins(raw)?;
    Ok(if map.is_empty() { None } else { Some(map) })
}
```

- [ ] **Step 6: Thread `cert_pins` through `resolve_force_routing`**

Update the signature + body:

```rust
pub fn resolve_force_routing(
    enabled: bool,
    proxy_bin: Option<PathBuf>,
    scratch_root: PathBuf,
    make_sink: DecisionSinkFactory,
    cert_pins: Option<CertPinMap>,
) -> Result<Option<ForceRoutingConfig>, ProxyBinaryNotFound> {
    if !enabled {
        return Ok(None);
    }
    let proxy_bin = proxy_bin.ok_or(ProxyBinaryNotFound)?;
    Ok(Some(ForceRoutingConfig::new(proxy_bin, scratch_root, make_sink, cert_pins)))
}
```

- [ ] **Step 7: Read + parse the pins in `from_env`**

Update `from_env`'s signature (return type) and body:

```rust
pub fn from_env(
    pool: sqlx::PgPool,
    handle: tokio::runtime::Handle,
    exe_dir: Option<&Path>,
) -> Result<Option<Arc<ForceRoutingConfig>>, ForceRoutingError> {
    if !env_flag_enabled(std::env::var(ENV_ENABLE).ok()) {
        return Ok(None);
    }
    let cert_pins = parse_cert_pins_env(std::env::var(ENV_CERT_PINS).ok().as_deref())?;
    let proxy_bin = discover_egress_proxy_bin(exe_dir);
    let scratch_root = std::env::var_os(ENV_SCRATCH_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(default_egress_scratch_root);
    let make_sink: DecisionSinkFactory =
        Box::new(move || Box::new(pg_decision_sink(pool.clone(), handle.clone())));
    Ok(resolve_force_routing(true, proxy_bin, scratch_root, make_sink, cert_pins)?.map(Arc::new))
}
```

- [ ] **Step 8: Update the existing test helpers + resolver test call sites**

In the `tests` module of `force_route.rs`, update `config_with` to pass `None` for the new param:

```rust
    fn config_with(scratch_root: PathBuf) -> ForceRoutingConfig {
        ForceRoutingConfig::new(
            PathBuf::from("/nonexistent/egress-proxy"),
            scratch_root,
            noop_sink_factory(),
            None,
        )
    }
```

Update the three existing `resolve_force_routing(...)` calls in the tests `disabled_resolves_to_none_even_with_a_binary`, `enabled_with_binary_resolves_to_some`, and `enabled_without_binary_fails_closed` to pass a trailing `None` argument. For example `enabled_with_binary_resolves_to_some` becomes:

```rust
        let out = resolve_force_routing(
            true,
            Some(PathBuf::from("/opt/egress-proxy")),
            PathBuf::from("/tmp"),
            noop_sink_factory(),
            None,
        )
        .expect("enabled + binary => Ok(Some)");
```

Apply the same trailing `None` to the `resolve_force_routing` calls in `disabled_resolves_to_none_even_with_a_binary` and `enabled_without_binary_fails_closed`.

- [ ] **Step 9: Re-export `ForceRoutingError`**

In `core/src/worker_lifecycle/mod.rs:26`, add `ForceRoutingError` to the re-export:

```rust
pub use force_route::{
    resolve_force_routing, ForceRoutingConfig, ForceRoutingError, ProxyBinaryNotFound,
};
```

- [ ] **Step 10: Run the tests + confirm `main.rs` still compiles**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib force_route`
Expected: PASS (existing force_route tests + the 4 new ones).

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core`
Expected: builds clean — `main.rs` uses `.context(...)?`, which accepts the widened `ForceRoutingError` (it implements `std::error::Error`), so no `main.rs` change is required.

- [ ] **Step 11: Commit**

```bash
git add core/src/worker_lifecycle/force_route.rs core/src/worker_lifecycle/mod.rs
git commit -m "feat(egress): read+validate operator cert pins in force_route::from_env

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: select pins per worker at `spawn_worker_maybe_forced`

**Files:**
- Modify: `core/src/worker_lifecycle/force_route.rs` (add `pins_for` method + wire the spawn site)
- Test: inline `#[cfg(test)] mod tests` in `force_route.rs`

**Interfaces:**
- Consumes: `select_pins_for_allowlist` (Task 2), `ForceRoutingConfig.cert_pins` (Task 3).
- Produces: `ForceRoutingConfig::pins_for(&self, allowlist: &[String]) -> Option<String>` (`pub(crate)`); the production wiring that replaces `cert_pins_json: None`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `force_route.rs`:

```rust
    #[test]
    fn pins_for_selects_allowlisted_subset() {
        let pins = parse_cert_pins_env(Some(r#"{"a.com":["sha256/A"]}"#)).unwrap();
        let cfg = ForceRoutingConfig::new(
            PathBuf::from("/nonexistent/egress-proxy"),
            PathBuf::from("/tmp"),
            noop_sink_factory(),
            pins,
        );
        let json = cfg.pins_for(&["a.com:443".to_string()]).expect("pinned host in allowlist");
        assert!(json.contains("a.com"));
        assert!(json.contains("sha256/A"));
    }

    #[test]
    fn pins_for_none_when_unconfigured() {
        let cfg = config_with(PathBuf::from("/tmp"));
        assert!(cfg.pins_for(&["a.com:443".to_string()]).is_none());
    }

    #[test]
    fn pins_for_none_when_no_allowlist_match() {
        let pins = parse_cert_pins_env(Some(r#"{"a.com":["sha256/A"]}"#)).unwrap();
        let cfg = ForceRoutingConfig::new(
            PathBuf::from("/nonexistent/egress-proxy"),
            PathBuf::from("/tmp"),
            noop_sink_factory(),
            pins,
        );
        assert!(cfg.pins_for(&["z.com:443".to_string()]).is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib force_route`
Expected: FAIL to compile — `pins_for` not found.

- [ ] **Step 3: Add the `pins_for` method**

Add to the `impl ForceRoutingConfig` block (after `new`). Add the import to the top `use` line from Task 3 — change it to also bring in `select_pins_for_allowlist`:

```rust
use crate::egress::cert_pins::{parse_cert_pins, select_pins_for_allowlist, CertPinError, CertPinMap};
```

Method:

```rust
    /// The pin JSON to hand a force-routed worker's sidecar, given the worker's
    /// allowlist. `None` when no pins are configured or none of the worker's
    /// allowlisted hosts are pinned (→ byte-identical no-pin path).
    pub(crate) fn pins_for(&self, allowlist: &[String]) -> Option<String> {
        self.cert_pins.as_ref().and_then(|m| select_pins_for_allowlist(m, allowlist))
    }
```

- [ ] **Step 4: Wire the spawn site**

In `spawn_worker_maybe_forced`, replace the `cert_pins_json: None,` line. The `ForceRouteAction::Sidecar` arm becomes:

```rust
        ForceRouteAction::Sidecar => {
            let cfg = force.expect("Sidecar action implies force-routing is configured");
            let allowlist = match &spec.policy.net {
                Net::Allowlist(hosts) => hosts.clone(),
                _ => return spawn_worker(backend, spec),
            };
            let pins_json = cfg.pins_for(&allowlist);
            let params = crate::egress::net_worker::NetWorkerSpawn {
                backend,
                proxy_bin: &cfg.proxy_bin,
                spec,
                allowlist: &allowlist,
                worker_name,
                secret_fingerprints: &[],
                cert_pins_json: pins_json.as_deref(),
                // The browser does end-to-end TLS + can't trust our CA → its
                // sidecar transparently tunnels (slice #2).
                disable_mitm: worker_name == BROWSER_DRIVER_TOOL,
            };
            spawn_forced_net_worker(&params, &cfg.scratch_root, (cfg.make_sink)())
        }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib force_route`
Expected: PASS — the 3 new `pins_for` tests plus all existing spawn tests (which use `config_with` ⇒ `cert_pins: None` ⇒ `pins_for` returns `None` ⇒ `cert_pins_json: None`, byte-identical to before).

- [ ] **Step 6: Commit**

```bash
git add core/src/worker_lifecycle/force_route.rs
git commit -m "feat(egress): hand each force-routed sidecar its allowlist's cert pins

Replaces the hard-coded cert_pins_json: None at spawn_worker_maybe_forced
with per-worker pin selection — completing the slice-#4 host-side seam.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: full-workspace verification + docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` (header + Recently-merged + Next-TODO; reconcile #299 already done this session)
- Modify: `docs/devel/ROADMAP.md` (tick the slice-#4 operator-pin item with the commit hash)
- File a follow-up issue (the deferred real-sandbox pin-enforcement e2e)

- [ ] **Step 1: Run the full workspace test suite + clippy**

Run: `source "$HOME/.cargo/env" && cargo test --workspace`
Expected: all green on macOS (live-PG suites skip-as-pass; the standing `embedding_recall_e2e` full-workspace flake may need single-threaded re-run per HANDOVER).

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

If the `embedding_recall_e2e` flake trips, re-run that suite alone to confirm it's the known PG-bring-up flake, not a regression:
Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test embedding_recall_e2e -- --test-threads=1`

- [ ] **Step 2: File the deferred e2e follow-up issue**

```bash
gh issue create --title "Real-sandbox cert-pin enforcement e2e for force-routed workers" \
  --body "$(cat <<'EOF'
Operator cert-pin plumbing (KASTELLAN_EGRESS_CERT_PINS → per-worker sidecar
cert_pins_json) shipped structurally + unit-tested. Still missing: an
end-to-end test where a force-routed worker dials a host whose served cert
does NOT match the configured pin and is blocked at the sidecar with a
`tls_pin`/`pin_mismatch` decision.

Needs a real sandbox + a controllable TLS origin (mirrors
egress_force_routing_e2e's `#[ignore]` real-net tests). Deferred because there
is no frontier consumer yet to justify the harness; the proxy-side
PinningVerifier is already unit-covered in workers/egress-proxy.
EOF
)"
```

Capture the issue number it prints for the HANDOVER/ROADMAP notes.

- [ ] **Step 3: Update HANDOVER.md + ROADMAP.md**

Follow the HANDOVER "How to update this document at session end" checklist (header first): bump `Last updated`, move this work into "Recently merged"/"Recently completed", refresh the slice-#4 egress follow-up bullet (operator pin config DONE; frontier routing still Phase-5-deferred), add the `cert_pins.rs` module to "Working state", and tick the ROADMAP slice-#4 operator-pin line with the commit hash. Reference the new follow-up issue number from Step 2.

- [ ] **Step 4: Commit the docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): operator egress cert-pin plumbing shipped

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Push + open the PR**

```bash
git push -u origin feat/egress-operator-cert-pins
gh pr create --base main --title "feat(egress): operator cert-pin plumbing (slice-#4 host last mile)" \
  --body "$(cat <<'EOF'
Completes the slice-#4 host-side seam: an operator-configurable
`KASTELLAN_EGRESS_CERT_PINS` env var, parsed fail-closed on the daemon and
selected per-worker (least-privilege, by allowlist host) into each force-routed
sidecar's `cert_pins_json`. Replaces the hard-coded `None` at
`spawn_worker_maybe_forced`.

Decoupled from the Phase-5 frontier path (which doesn't exist yet — `Router`
denies all frontier calls and runs in-core, not via a sidecar). No new deps,
OS-agnostic, byte-identical when the env var is unset.

Spec: `docs/superpowers/specs/2026-06-17-operator-egress-cert-pins-design.md`
Plan: `docs/superpowers/plans/2026-06-17-operator-egress-cert-pins.md`

Deferred: real-sandbox pin-enforcement e2e (filed as #<issue from Step 2>).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-Review

**Spec coverage:**
- Config source = env var `KASTELLAN_EGRESS_CERT_PINS` → Task 3 (`ENV_CERT_PINS`, `from_env`). ✓
- Structural host-side parse, proxy authoritative → Task 1 (`parse_cert_pins`, module doc). ✓
- Least-privilege per-worker selection → Task 2 (`select_pins_for_allowlist`) + Task 4 (`pins_for`, spawn wiring). ✓
- Fail closed at startup → Task 3 (`parse_cert_pins_env` → `ForceRoutingError::CertPins`). ✓
- `CertPinMap` non-empty invariant / empty→None normalization → Task 1 (`is_empty`) + Task 3 (`parse_cert_pins_env`). ✓
- `host_of_endpoint` IPv6-aware → Task 2. ✓
- `ForceRoutingError` enum + re-export → Task 3. ✓
- Tests for every pure fn → Tasks 1, 2, 3, 4. ✓
- Verification (`cargo test --workspace`, clippy) → Task 5. ✓
- Deferred real-sandbox e2e filed as issue → Task 5. ✓
- Docs (HANDOVER/ROADMAP) → Task 5. ✓

**Placeholder scan:** No "TBD"/"handle edge cases"/"similar to" — every code step shows full code; the only `todo!` is a deliberate, named stub in Task 1 Step 2 that Step 4 replaces. ✓

**Type consistency:** `CertPinMap`, `CertPinError`, `parse_cert_pins`, `select_pins_for_allowlist`, `host_of_endpoint`, `parse_cert_pins_env`, `ForceRoutingError`, `ForceRoutingConfig::{new, pins_for, cert_pins}`, `resolve_force_routing` — signatures match across the tasks that define and consume them. The `use` import in Task 3 is widened in Task 4 to add `select_pins_for_allowlist` (called out explicitly). ✓
