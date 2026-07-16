# Forced-Loopback Endpoint Guard Implementation Plan

> **⚠️ SUPERSEDED IN PART (2026-07-16, post-review):** the shipped predicate is
> `endpoint_is_localhost_name` (localhost/*.localhost NAMES only) — the
> literal-flagging `endpoint_host_is_local` this plan specifies was narrowed
> after the final review found the egress proxy's allowlisted-literal
> carve-out. See the spec's "Revision after final review" section. Core no
> longer depends on net-classify. (Also: ssrf.rs had 12 tests, not 13.)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Detect at `resolve()` time when an operator points a force-routed net worker (web-search / web-research) at a loopback/private endpoint the egress proxy will SSRF-block, and refuse to register it (`Misconfigured`) — or warn, for web-research's optional embed endpoint (#452 + #429).

**Architecture:** Move the security-critical IP-range classifier `is_denied_range` out of the bin-only egress-proxy crate into a new pure shared crate `kastellan-net-classify` (the `leak-scan` no-drift precedent). Core gains a small cross-platform `workers/endpoint_guard` module (URL-host classification + a force-routing predicate mirroring `force_route`'s own flag semantics); each worker's `resolve()` composes those predicates at both its host and Linux micro-VM paths.

**Tech Stack:** Rust workspace; `url` crate (typed `Host` enum); `tracing::warn!` for the #429 warning; no new external deps.

**Spec:** `docs/superpowers/specs/2026-07-16-vm-loopback-endpoint-guard-design.md`

## Global Constraints

- AGPL-compatible deps only; the new crate has **zero external deps** (pure `std`).
- Cross-platform: helpers + host-path guards compile and test on macOS **and** Linux; only the VM-branch call sites and their tests are `#[cfg(target_os = "linux")]`.
- **No DNS at resolve time** — classify IP literals + `localhost`/`*.localhost` only.
- **No behaviour change** when the guard doesn't fire: entries stay byte-identical (existing resolve tests must keep passing unmodified).
- `cargo clippy --workspace --all-targets -- -D warnings` must stay clean.
- Rust source files stay under ~500 LOC (`endpoint_guard.rs` inline tests are fine — the file is small).
- Commit convention: `type(scope): summary` + the Co-Authored-By trailer used on this branch.

---

### Task 1: `kastellan-net-classify` crate (pure move of `is_denied_range`)

**Files:**
- Create: `net-classify/Cargo.toml`
- Create: `net-classify/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)
- Modify: `workers/egress-proxy/Cargo.toml` (add dep)
- Modify: `workers/egress-proxy/src/main.rs:28` (drop `mod ssrf;`)
- Modify: `workers/egress-proxy/src/proxy.rs:14` (re-point the `use`)
- Delete: `workers/egress-proxy/src/ssrf.rs`

**Interfaces:**
- Produces: `kastellan_net_classify::is_denied_range(ip: std::net::IpAddr) -> bool` (public; used by egress-proxy now, by core in Task 2).

This is a **behaviour-preserving move**: the 13 existing ssrf tests move with the
function and are the test cycle (red/green does not apply to a pure move; the
gate is: new-crate tests pass, egress-proxy compiles, no test lost).

- [ ] **Step 1: Create the crate**

`net-classify/Cargo.toml` (mirrors `leak-scan/Cargo.toml`):

```toml
[package]
name        = "kastellan-net-classify"
description = "Pure IP-range classifier: the SSRF / DNS-rebinding deny predicate shared by the egress proxy (connect-time containment) and core (resolve-time endpoint sanity checks)."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme      = "../README.md"

[dependencies]
```

`net-classify/src/lib.rs`: the **entire content** of
`workers/egress-proxy/src/ssrf.rs` (function bodies, helpers, and the full
`#[cfg(test)] mod tests` block, byte-identical), except the module header
comment is replaced by this crate doc:

```rust
//! Pure IP-range classifier shared by the egress proxy and core.
//!
//! [`is_denied_range`] is the single security-critical predicate: it returns
//! true for every address class a *hostname* must not be permitted to resolve
//! to (the DNS-rebinding defense). The egress proxy applies it at connect time
//! to resolved addresses (its literal-IP CONNECT targets get a carve-out in the
//! proxy itself, not here); core applies it at manifest-resolve time to
//! operator-configured endpoint literals (`workers/endpoint_guard`). One home
//! for the range list means the two checks cannot drift.
```

- [ ] **Step 2: Wire the workspace + egress-proxy**

`Cargo.toml` members — insert after `"llm-router",`:

```toml
    "net-classify",
```

`workers/egress-proxy/Cargo.toml` — add below the `kastellan-leak-scan` line:

```toml
kastellan-net-classify = { path = "../../net-classify", version = "0.1.0" }
```

`workers/egress-proxy/src/main.rs` — delete the line `mod ssrf;`.

`workers/egress-proxy/src/proxy.rs` — change
`use crate::ssrf::is_denied_range;` → `use kastellan_net_classify::is_denied_range;`.

Delete the old file: `git rm workers/egress-proxy/src/ssrf.rs`.

- [ ] **Step 3: Verify the move**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-net-classify
```
Expected: **12 passed** (public_v4_is_allowed … sixtofour_embedded_private_is_denied).

```sh
cargo test -p kastellan-worker-egress-proxy
cargo build --workspace
```
Expected: egress-proxy tests pass with the ssrf tests **gone from this crate**
(moved, not lost); workspace builds clean.

- [ ] **Step 4: Commit**

```sh
git add -A net-classify Cargo.toml workers/egress-proxy
git commit -m "refactor(net-classify): extract is_denied_range into a shared pure crate

Pure move of the SSRF range classifier (+ its 13 tests) out of the bin-only
egress-proxy so core can reuse the exact same range list at resolve time
(#452) — the leak-scan no-drift precedent. Behaviour byte-identical."
```

---

### Task 2: core `workers/endpoint_guard` module + `force_route` visibility

**Files:**
- Modify: `core/Cargo.toml` (add `kastellan-net-classify` dep)
- Modify: `core/src/worker_lifecycle/force_route.rs:32` (`const ENV_ENABLE` → `pub(crate)`), `:419` (`fn env_flag_enabled` → `pub(crate)`)
- Create: `core/src/workers/endpoint_guard.rs`
- Modify: `core/src/workers/mod.rs` (module decl)

**Interfaces:**
- Consumes: `kastellan_net_classify::is_denied_range` (Task 1).
- Produces (used by Tasks 3–4):
  - `pub(crate) fn endpoint_host_is_local(endpoint: &str) -> bool`
  - `pub(crate) fn egress_will_force_route(is_microvm: bool, get_env: &dyn Fn(&str) -> Option<String>) -> bool`
  - `pub(crate) fn embed_local_warning(force_routed: bool, use_broker: bool, embed_endpoint: Option<&str>) -> Option<String>`

- [ ] **Step 1: Write the failing tests** — create `core/src/workers/endpoint_guard.rs` with the module doc, `use` lines, **empty stubs that `todo!()`**, and this test module (TDD: tests first, stubs make them compile then fail):

```rust
//! Resolve-time endpoint locality guard (#452 / #429).
//!
//! When a `Net::Allowlist` worker's egress is force-routed through the host
//! egress proxy, the proxy SSRF-blocks loopback / RFC1918 / CGNAT destinations
//! (the `kastellan-net-classify` range list — the same one the proxy enforces
//! at connect time). An operator endpoint pointing at such an address is
//! therefore unreachable in a force-routed mode: the worker registers and
//! looks healthy, but every request fails — a silent footgun (#452). The
//! helpers here let a manifest detect that dead configuration at `resolve()`
//! time and refuse to register, or — for web-research's *optional* embed
//! endpoint, where a local address only degrades ranking — warn (#429).
//!
//! Deliberately **no DNS at resolve time**: a real hostname that later
//! resolves to loopback (DNS rebinding) is caught by the authoritative
//! connect-time proxy SSRF check. These helpers classify only what is knowable
//! statically — IP literals and the RFC 6761 `localhost` names.

use std::net::IpAddr;

use kastellan_net_classify::is_denied_range;
use url::{Host, Url};

use crate::worker_lifecycle::force_route;
```

Test module (bottom of the same file):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_private_ip_literals_are_local() {
        for ep in [
            "http://127.0.0.1:8888/search",
            "https://127.1.2.3/",
            "http://[::1]:8888/search",
            "http://10.0.0.5:8080/",
            "http://172.16.5.5/",
            "http://192.168.1.1:11434/v1/embeddings",
            "http://169.254.1.1/",
            "http://100.64.0.1/", // CGNAT
            "http://0.0.0.0:9/",
            "http://[fd12:3456::1]/", // ULA
        ] {
            assert!(endpoint_host_is_local(ep), "{ep} should be local");
        }
    }

    #[test]
    fn localhost_names_are_local() {
        assert!(endpoint_host_is_local("http://localhost:8888/search"));
        assert!(endpoint_host_is_local("http://LOCALHOST/"));
        assert!(endpoint_host_is_local("https://searx.localhost/search"));
        assert!(endpoint_host_is_local("http://localhost./")); // FQDN trailing dot
    }

    #[test]
    fn public_hosts_and_ips_are_not_local() {
        assert!(!endpoint_host_is_local("https://searx.example.org/search"));
        assert!(!endpoint_host_is_local("https://searx.example.org:8888/search"));
        assert!(!endpoint_host_is_local("http://203.0.113.5:8888/"));
        assert!(!endpoint_host_is_local("http://[2606:4700:4700::1111]/"));
    }

    #[test]
    fn rebinding_lookalike_domain_is_not_local() {
        // A hostname merely *containing* a loopback string is still a domain —
        // what it resolves to is the connect-time proxy's job, not ours.
        assert!(!endpoint_host_is_local("http://127.0.0.1.attacker.example/"));
        assert!(!endpoint_host_is_local("http://localhost.attacker.example/"));
    }

    #[test]
    fn unset_or_unparseable_endpoints_are_not_local() {
        assert!(!endpoint_host_is_local(""));
        assert!(!endpoint_host_is_local("not a url"));
        assert!(!endpoint_host_is_local("127.0.0.1:8888/search")); // no scheme
    }

    #[test]
    fn microvm_always_force_routes_regardless_of_flag() {
        let unset = |_k: &str| None;
        assert!(egress_will_force_route(true, &unset));
        let off = |_k: &str| Some("0".to_string());
        assert!(egress_will_force_route(true, &off));
    }

    #[test]
    fn host_mode_follows_the_force_routing_flag_truthiness() {
        // Mirrors force_route::env_flag_enabled: 1|true|yes|on, trimmed,
        // case-insensitive.
        for v in ["1", "true", "yes", "on", " TRUE "] {
            let on =
                move |k: &str| (k == "KASTELLAN_EGRESS_FORCE_ROUTING").then(|| v.to_string());
            assert!(egress_will_force_route(false, &on), "{v:?} should enable");
        }
        for v in ["0", "false", "off", "", "banana"] {
            let off =
                move |k: &str| (k == "KASTELLAN_EGRESS_FORCE_ROUTING").then(|| v.to_string());
            assert!(!egress_will_force_route(false, &off), "{v:?} must not enable");
        }
        let unset = |_k: &str| None;
        assert!(!egress_will_force_route(false, &unset));
    }

    #[test]
    fn warns_only_when_forced_unbrokered_and_local() {
        let local = Some("http://127.0.0.1:11434/v1/embeddings");
        assert!(embed_local_warning(true, false, local).is_some());
        // Not force-routed: host egress reaches loopback fine.
        assert!(embed_local_warning(false, false, local).is_none());
        // Brokered: the embed-broker reaches the backend over its UDS.
        assert!(embed_local_warning(true, true, local).is_none());
        // Routable or unset endpoint: nothing to warn about.
        let routable = Some("http://embed.example.org:11434/v1/embeddings");
        assert!(embed_local_warning(true, false, routable).is_none());
        assert!(embed_local_warning(true, false, None).is_none());
    }

    #[test]
    fn warning_names_the_env_and_the_remedies() {
        let w = embed_local_warning(true, false, Some("http://localhost:11434/v1/embeddings"))
            .expect("should warn");
        assert!(w.contains("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT"), "warning: {w}");
        assert!(w.contains("KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1"), "warning: {w}");
    }
}
```

Also in this step:
- `core/src/workers/mod.rs`: insert `pub mod endpoint_guard;` after `pub mod browser_driver;` (alphabetical).
- `core/Cargo.toml`: add `kastellan-net-classify = { path = "../net-classify", version = "0.1.0" }` next to the `kastellan-leak-scan` dependency line.
- `core/src/worker_lifecycle/force_route.rs`: change
  `const ENV_ENABLE: &str = "KASTELLAN_EGRESS_FORCE_ROUTING";` →
  `pub(crate) const ENV_ENABLE: &str = "KASTELLAN_EGRESS_FORCE_ROUTING";`
  and `fn env_flag_enabled(value: Option<String>) -> bool {` →
  `pub(crate) fn env_flag_enabled(value: Option<String>) -> bool {`
  (no behaviour change; keep both doc comments).

- [ ] **Step 2: Run tests to verify they fail**

```sh
cargo test -p kastellan-core endpoint_guard
```
Expected: compile OK, tests FAIL (panic on `todo!()`).

- [ ] **Step 3: Implement the three functions** (replacing the stubs):

```rust
/// True iff `endpoint`'s host is a loopback/private/link-local/CGNAT IP literal
/// or a `localhost` / `*.localhost` name (RFC 6761) — i.e. an address the
/// force-routed egress proxy will refuse to CONNECT to.
///
/// A real remote hostname returns `false` (resolve-time cannot know its address
/// without DNS; the connect-time proxy check owns that case), and so does an
/// unset / unparseable endpoint (those keep today's fail-closed worker startup
/// behaviour instead of a guard message about the wrong problem).
pub(crate) fn endpoint_host_is_local(endpoint: &str) -> bool {
    let Ok(url) = Url::parse(endpoint) else { return false };
    match url.host() {
        Some(Host::Ipv4(a)) => is_denied_range(IpAddr::V4(a)),
        Some(Host::Ipv6(a)) => is_denied_range(IpAddr::V6(a)),
        Some(Host::Domain(d)) => is_local_domain(d),
        None => false,
    }
}

/// RFC 6761: `localhost` and any `*.localhost` name always resolve to loopback.
fn is_local_domain(domain: &str) -> bool {
    let d = domain.trim_end_matches('.'); // tolerate a FQDN trailing dot
    d.eq_ignore_ascii_case("localhost") || d.to_ascii_lowercase().ends_with(".localhost")
}

/// True iff a `Net::Allowlist` worker spawned by this daemon will have its
/// egress force-routed through the host egress proxy: always in micro-VM mode
/// (`linux_firecracker/plan.rs` refuses to give a `Net::Allowlist` VM worker a
/// NIC), and in host mode iff the operator enabled
/// `KASTELLAN_EGRESS_FORCE_ROUTING`. The flag name and truthiness parse are
/// `force_route`'s own (`ENV_ENABLE` / `env_flag_enabled`), so this mirror
/// cannot drift from the spawn path.
pub(crate) fn egress_will_force_route(
    is_microvm: bool,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> bool {
    is_microvm || force_route::env_flag_enabled(get_env(force_route::ENV_ENABLE))
}

/// `Some(warning)` iff web-research's *optional* embed endpoint is configured
/// but unreachable: egress is force-routed (the proxy SSRF-blocks local
/// addresses), the embed-broker is not enabled, and the endpoint host is
/// local. The worker still functions — ranking silently degrades
/// hybrid→lexical — so this is an operator warning, not `Misconfigured`
/// (#429). `None` when not force-routed (host egress reaches loopback fine),
/// when brokered (the embed-broker reaches the backend over its UDS), or when
/// the endpoint is routable/unset.
pub(crate) fn embed_local_warning(
    force_routed: bool,
    use_broker: bool,
    embed_endpoint: Option<&str>,
) -> Option<String> {
    if !force_routed || use_broker {
        return None;
    }
    let embed = embed_endpoint?;
    if !endpoint_host_is_local(embed) {
        return None;
    }
    Some(format!(
        "web-research: KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT ({embed}) points at a \
         loopback/private host while egress is force-routed (the egress proxy \
         SSRF-blocks it): the query embed will fail and ranking degrades \
         hybrid->lexical. Point it at a routable host or set \
         KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1."
    ))
}
```

- [ ] **Step 4: Run tests to verify they pass**

```sh
cargo test -p kastellan-core endpoint_guard
```
Expected: **9 passed**.

- [ ] **Step 5: Commit**

```sh
git add core/src/workers/endpoint_guard.rs core/src/workers/mod.rs \
        core/src/worker_lifecycle/force_route.rs core/Cargo.toml Cargo.lock
git commit -m "feat(core): endpoint_guard — resolve-time locality + force-routing predicates

endpoint_host_is_local (typed url::Host over the shared net-classify range
list + RFC 6761 localhost), egress_will_force_route (mirrors force_route's
own ENV_ENABLE/env_flag_enabled, widened to pub(crate)), and the #429
embed_local_warning composer. Pure, cross-platform, unit-tested; wired into
the worker manifests in the next commits."
```

---

### Task 3: web-search `resolve()` guard (#452)

**Files:**
- Modify: `core/src/workers/web_search.rs` (private helper + two call sites + docstring)
- Test: `core/src/workers/web_search/tests.rs`

**Interfaces:**
- Consumes: `endpoint_guard::{egress_will_force_route, endpoint_host_is_local}` (Task 2).
- Produces: no new public API — `resolve()` now returns `Resolution::Misconfigured` for a forced-loopback direct config.

- [ ] **Step 1: Write the failing tests** — append to `core/src/workers/web_search/tests.rs`:

```rust
#[test]
fn resolve_forced_host_loopback_endpoint_is_misconfigured() {
    // Host mode + KASTELLAN_EGRESS_FORCE_ROUTING + loopback endpoint + no
    // broker: the egress proxy would SSRF-block every CONNECT — refuse to
    // register rather than register a dead tool (#452).
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Misconfigured { detail } => {
            assert!(detail.contains("KASTELLAN_WEB_SEARCH_ENDPOINT"), "detail: {detail}");
            assert!(detail.contains("KASTELLAN_WEB_SEARCH_USE_BROKER=1"), "detail: {detail}");
        }
        other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_forced_host_loopback_with_broker_still_registers() {
    // The search-broker is the loopback escape hatch — broker mode must be
    // exempt from the #452 guard.
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_BROKER" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(entry.broker.is_some(), "broker mode entry expected");
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_forced_host_routable_endpoint_still_registers() {
    // Flag on + routable endpoint: no false positive.
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
        "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(_) => {}
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_direct_microvm_loopback_endpoint_is_misconfigured() {
    // A Net::Allowlist VM worker force-routes unconditionally (no flag needed):
    // direct VM + loopback endpoint is always dead (#452). Broker-VM + loopback
    // stays allowed — pinned by resolve_uses_broker_microvm_entry_when_both_opted_in.
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_MICROVM" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Misconfigured { detail } => {
            assert!(detail.contains("KASTELLAN_WEB_SEARCH_USE_BROKER=1"), "detail: {detail}");
        }
        other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
    }
}
```

- [ ] **Step 2: Run tests to verify the new ones fail**

```sh
cargo test -p kastellan-core --lib workers::web_search
```
Expected: the 3 new cross-platform tests FAIL (`expected Misconfigured, got register`
for two of them; the broker/routable ones may pass — that's fine, they are pins);
all pre-existing tests PASS.

- [ ] **Step 3: Implement the guard** in `core/src/workers/web_search.rs`.

Add the private helper (place it next to `host_allowlist_from_endpoint`):

```rust
/// The #452 resolve-time guard: `Some(detail)` iff this worker's egress will be
/// force-routed (micro-VM always; host iff `KASTELLAN_EGRESS_FORCE_ROUTING`),
/// broker mode is off (the search-broker is the loopback escape hatch), and the
/// operator endpoint is a loopback/private address the egress proxy would
/// SSRF-block — i.e. a config where the tool registers but every search fails.
fn forced_loopback_misconfig(
    use_broker: bool,
    is_microvm: bool,
    endpoint: &str,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    use crate::workers::endpoint_guard::{egress_will_force_route, endpoint_host_is_local};
    if use_broker
        || !egress_will_force_route(is_microvm, get_env)
        || !endpoint_host_is_local(endpoint)
    {
        return None;
    }
    Some(format!(
        "{ENDPOINT_ENV} ({endpoint}) points at a loopback/private host, but this \
         worker's egress is force-routed through the egress proxy, which \
         SSRF-blocks loopback/private addresses — every search would fail at \
         request time. Set {USE_BROKER_ENV}=1 (the host-side search-broker \
         reaches a loopback SearxNG) or point the endpoint at a routable host."
    ))
}
```

In `resolve()`, add the VM call site at the top of the `if use_microvm` block
(before `let binary = ...`):

```rust
                if let Some(detail) =
                    forced_loopback_misconfig(use_broker, true, &endpoint, ctx.get_env)
                {
                    return Resolution::Misconfigured { detail };
                }
```

and the host call site immediately after the `#[cfg(target_os = "linux")]` block
(before `let binary = match discover_binary(...)`):

```rust
        if let Some(detail) =
            forced_loopback_misconfig(use_broker, false, &endpoint, ctx.get_env)
        {
            return Resolution::Misconfigured { detail };
        }
```

Docstring: in `web_search_firecracker_entry`'s "Loopback-SearxNG caveat"
paragraph, replace the sentence
"use the broker VM entry ([`web_search_firecracker_broker_entry`], `USE_BROKER=1`) for a loopback SearxNG."
with
"`resolve()` refuses to register that dead config (`Misconfigured`, #452); use the broker VM entry ([`web_search_firecracker_broker_entry`], `USE_BROKER=1`) for a loopback SearxNG."

- [ ] **Step 4: Run tests to verify they pass**

```sh
cargo test -p kastellan-core --lib workers::web_search
```
Expected: all pass (existing + 3 new here; the linux test runs on the DGX in Task 5).

- [ ] **Step 5: Commit**

```sh
git add core/src/workers/web_search.rs core/src/workers/web_search/tests.rs
git commit -m "feat(web-search): refuse a force-routed loopback endpoint at resolve time (#452)

Direct mode with egress force-routed (micro-VM always; host iff
KASTELLAN_EGRESS_FORCE_ROUTING) and a loopback/private SearxNG endpoint is a
dead config — the proxy SSRF-blocks every CONNECT. resolve() now returns
Misconfigured pointing at USE_BROKER=1; broker mode and routable endpoints
are exempt (pinned by tests)."
```

---

### Task 4: web-research `resolve()` guard + #429 embed warning

**Files:**
- Modify: `core/src/workers/web_research.rs` (private helper + guard/warn call sites + docstring + tests in the inline `mod tests`)

**Interfaces:**
- Consumes: `endpoint_guard::{egress_will_force_route, endpoint_host_is_local, embed_local_warning}` (Task 2).
- Produces: no new public API — `resolve()` returns `Misconfigured` for a forced-loopback SearxNG endpoint (any broker mode: the embed-broker carries no search traffic) and `tracing::warn!`s on a forced-unbrokered-loopback embed endpoint.

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `core/src/workers/web_research.rs`:

```rust
    #[test]
    fn resolve_forced_host_loopback_searxng_is_misconfigured() {
        // Host mode + force-routing flag + loopback SearxNG: web-research has
        // no search-broker, so this config reaches nothing (#452).
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("KASTELLAN_WEB_RESEARCH_ENDPOINT"), "detail: {detail}");
                assert!(detail.contains("routable"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_forced_host_loopback_embed_only_warns_and_registers() {
        // Loopback *embed* endpoint under force-routing degrades ranking but
        // does not break the tool: warn-only, registration proceeds (#429).
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://127.0.0.1:11434/v1/embeddings".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist =
            |_t: &str| vec!["searx.example.org".to_string(), "127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // The embed env is still injected — the entry itself is unchanged
                // (the warning is a log line, not a policy change).
                assert!(entry.policy.env.iter().any(|(k, _)| k == EMBED_ENDPOINT_ENV));
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_microvm_loopback_searxng_is_misconfigured() {
        // A VM worker force-routes unconditionally: loopback SearxNG is dead
        // in VM mode regardless of the host force-routing flag (#452).
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("KASTELLAN_WEB_RESEARCH_ENDPOINT"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_vm_embed_broker_does_not_rescue_loopback_searxng() {
        // The embed-broker carries only embed traffic — it must NOT exempt a
        // loopback SearxNG endpoint from the #452 guard.
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://127.0.0.1:11434/v1/embeddings".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { .. } => {}
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }
```

- [ ] **Step 2: Run tests to verify the new ones fail**

```sh
cargo test -p kastellan-core --lib workers::web_research
```
Expected: `resolve_forced_host_loopback_searxng_is_misconfigured` FAILS
(`expected Misconfigured, got register`);
`resolve_forced_host_loopback_embed_only_warns_and_registers` PASSES (it pins
warn-only behaviour); pre-existing tests PASS.

- [ ] **Step 3: Implement** in `core/src/workers/web_research.rs`.

Private helper (place next to `endpoint_net_entry`):

```rust
/// The #452 resolve-time guard, web-research flavour: unlike web-search there
/// is no search-broker escape hatch — this worker's only broker is the
/// embed-broker, which carries no search traffic — so a force-routed worker
/// with a loopback/private SearxNG endpoint reaches nothing in ANY broker mode.
fn forced_loopback_misconfig(
    is_microvm: bool,
    endpoint: &str,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    use crate::workers::endpoint_guard::{egress_will_force_route, endpoint_host_is_local};
    if !egress_will_force_route(is_microvm, get_env) || !endpoint_host_is_local(endpoint) {
        return None;
    }
    Some(format!(
        "{ENDPOINT_ENV} ({endpoint}) points at a loopback/private host, but this \
         worker's egress is force-routed through the egress proxy, which \
         SSRF-blocks loopback/private addresses — every search would fail at \
         request time. web-research has no search-broker yet (its broker carries \
         only embed traffic); point the endpoint at a routable SearxNG host."
    ))
}
```

In `resolve()`, at the top of the `if use_microvm` block (before
`let binary = ...`):

```rust
                if let Some(detail) = forced_loopback_misconfig(true, &endpoint, ctx.get_env) {
                    return Resolution::Misconfigured { detail };
                }
                if let Some(w) = crate::workers::endpoint_guard::embed_local_warning(
                    true,
                    use_broker,
                    embed_endpoint.as_deref(),
                ) {
                    tracing::warn!(target: "web_research.resolve", "{w}");
                }
```

and immediately after the `#[cfg(target_os = "linux")]` block (before
`let binary = match discover_binary(...)`):

```rust
        if let Some(detail) = forced_loopback_misconfig(false, &endpoint, ctx.get_env) {
            return Resolution::Misconfigured { detail };
        }
        if let Some(w) = crate::workers::endpoint_guard::embed_local_warning(
            crate::workers::endpoint_guard::egress_will_force_route(false, ctx.get_env),
            use_broker,
            embed_endpoint.as_deref(),
        ) {
            tracing::warn!(target: "web_research.resolve", "{w}");
        }
```

Docstring: in `web_research_firecracker_entry`'s "Loopback-embed caveat"
paragraph, replace the sentence
"A resolve-time operator warning for this loopback+VM misconfiguration is tracked in issue #429."
with
"`resolve()` emits an operator warning for this loopback+forced misconfiguration (#429), and refuses a loopback *SearxNG* endpoint outright (#452 — no search-broker exists for web-research)."

- [ ] **Step 4: Run tests to verify they pass**

```sh
cargo test -p kastellan-core --lib workers::web_research
cargo test -p kastellan-core --lib workers::web_search
cargo test -p kastellan-core --lib endpoint_guard
```
Expected: all pass on macOS (the two `#[cfg(target_os = "linux")]` tests run in Task 5).

- [ ] **Step 5: Commit**

```sh
git add core/src/workers/web_research.rs
git commit -m "feat(web-research): forced-loopback SearxNG guard + embed warning (#452, #429)

resolve() refuses a loopback/private SearxNG endpoint whenever egress is
force-routed — web-research's only broker is the embed-broker, which carries
no search traffic, so no broker mode rescues it. A loopback embed endpoint
in a forced, unbrokered mode now logs an operator warning (hybrid->lexical
downgrade) instead of staying silent; the entry itself is unchanged."
```

---

### Task 5: Full verification (Mac + DGX) and docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md` (session-end updates)

- [ ] **Step 1: Mac full gate**

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: build + clippy clean; tests green (macOS skip-as-pass conventions
apply; the known `embedding_recall_e2e` PG-bring-up flake is pre-existing —
see HANDOVER "Standing macOS test-infra gotcha").

- [ ] **Step 2: DGX gate (native Linux, real PG)** — run each as exactly
`ssh dgx '<cmd>'` (prefix-match allow rule):

```sh
ssh dgx 'cd ~/src/kastellan && git fetch origin && git checkout feat/vm-loopback-endpoint-guard && git pull'
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo build --workspace'
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-net-classify && cargo test -p kastellan-core --lib endpoint_guard && cargo test -p kastellan-core --lib workers::web_search && cargo test -p kastellan-core --lib workers::web_research'
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo test --workspace 2>&1 | tail -40'
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5'
```
Expected: the Linux-gated guard tests run and pass (they're `[SKIP]`-free unit
tests); full workspace ≥ the 2520-passed baseline + the new tests, 0 failed;
clippy clean. If the DGX checkout has local state, use a worktree or reset —
never commit from the DGX.

- [ ] **Step 3: Session-end docs + PR** — update HANDOVER.md (header + Next
TODO) and ROADMAP.md per the HANDOVER checklist; commit
`docs(handover): ...`; push and open the PR with `Closes #452` / `Closes #429`
and the DGX + Mac verification evidence.

---

## Self-review notes

- **Spec coverage:** crate move (Task 1), helpers + visibility (Task 2),
  web-search guard host+VM (Task 3), web-research guard + warning host+VM
  (Task 4), docstrings (Tasks 3–4), Mac + DGX verification (Task 5). The
  spec's "Deploy consequence" is operator documentation, carried in the PR
  body and HANDOVER.
- **Existing-test safety (verified against the tree):** every pre-existing
  resolve test either uses a routable endpoint, uses loopback without the
  force-routing flag (host mode, guard silent), or uses loopback in web-search
  broker mode (guard exempt). No existing test changes needed.
- **Type consistency:** `get_env: &dyn Fn(&str) -> Option<String>` matches
  `ResolveCtx.get_env`'s field type; `ctx.get_env` is passed directly.
