# Generic Forced-Localhost Guard Implementation Plan (#459 slice 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One generic post-resolve check that refuses (all-dead) or warns about (subset-dead) `localhost`-name `Net::Allowlist` entries for every force-routed worker, plus the equivalent check at the matrix channel bring-up seam.

**Architecture:** A pure `screen_net_allowlist` in `core/src/workers/endpoint_guard.rs` (string-level, no DNS), wired into `assemble_registry`'s `Register` arm in `core/src/registry_build.rs` (Refuse ⇒ treated exactly like `Resolution::Misconfigured`; Warn ⇒ `tracing::warn!` + register). Matrix gets a pure pub helper in `core/src/channel/matrix/policy.rs` (delegating to the existing shared `forced_localhost_misconfig` builder) called from the bin's `core/src/main/matrix_boot.rs` before any spawn. web-research's now-redundant `content_localhost_warnings` is removed.

**Tech Stack:** Rust workspace; crate `kastellan-core` only. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-07-17-generic-forced-localhost-guard-design.md` (read it first).

## Global Constraints

- AGPL-compatible deps only — this plan adds **no** dependencies.
- No DNS at resolve time; classify only what is statically knowable (RFC 6761 `localhost`/`*.localhost` names). Literal IPs are NEVER flagged (the egress proxy's operator-allowlisted-literal carve-out dials them).
- Cross-platform: everything compiles + tests on macOS; the one cfg(linux) spot (`entry_is_vm`, `vm_mode`) mirrors existing cfg arms.
- Pure functions take env via injected closures (`ctx.get_env`) — never `std::env` inside the lib paths touched here.
- Keep every touched file under 500 prod LOC (all stay well under; verify with `wc -l` in Task 5).
- Run all cargo commands in the **foreground** (no background waits). Cargo needs `source "$HOME/.cargo/env"` first in non-interactive shells.
- `git add` **specific files only** — never `git add -A`.
- Behavior with force-routing off must be byte-identical: every existing test stays green.

---

### Task 1: `screen_net_allowlist` + `host_of_entry` in `endpoint_guard.rs`

**Files:**
- Modify: `core/src/workers/endpoint_guard.rs` (prod code above the `#[cfg(test)]` module, tests inside it)

**Interfaces:**
- Consumes: existing `host_is_localhost_name` (same file).
- Produces (Task 2 relies on these exact names):
  - `pub(crate) enum NetScreen { Ok, Warn { dead: Vec<String> }, Refuse { detail: String } }` (derives `Debug`)
  - `pub(crate) fn screen_net_allowlist(tool: &str, entries: &[String], force_routed: bool) -> NetScreen`

- [ ] **Step 1: Write the failing tests** — append inside the existing `mod tests` in `core/src/workers/endpoint_guard.rs`:

```rust
    #[test]
    fn host_of_entry_strips_only_a_trailing_digit_port() {
        assert_eq!(host_of_entry("localhost:443"), "localhost");
        assert_eq!(host_of_entry("localhost"), "localhost"); // bare DB row (browser-driver)
        assert_eq!(host_of_entry("example.org:8080"), "example.org");
        assert_eq!(host_of_entry("[::1]:443"), "[::1]"); // bracketed v6 stays whole
        assert_eq!(host_of_entry("[::1]"), "[::1]");
        // Defensive: a non-digit "port" is not a port — return the entry as-is
        // (it then classifies false, which is the safe no-flag direction).
        assert_eq!(host_of_entry("example.org:https"), "example.org:https");
    }

    #[test]
    fn screen_is_ok_when_not_forced_or_list_is_empty() {
        let dead_only = vec!["localhost:443".to_string()];
        assert!(matches!(screen_net_allowlist("t", &dead_only, false), NetScreen::Ok));
        // Empty allowlist = the broker/zero-egress posture: deliberately exempt.
        assert!(matches!(screen_net_allowlist("t", &[], true), NetScreen::Ok));
    }

    #[test]
    fn screen_is_ok_when_no_entry_is_a_localhost_name() {
        let entries = vec![
            "searx.example.org:8888".to_string(),
            "docs.example.org".to_string(), // bare row form
            "127.0.0.1:8888".to_string(),   // literal: carve-out, never flagged
            "[::1]:443".to_string(),        // v6 literal: carve-out
        ];
        assert!(matches!(screen_net_allowlist("t", &entries, true), NetScreen::Ok));
    }

    #[test]
    fn screen_warns_on_a_proper_subset_of_dead_entries() {
        let entries = vec![
            "docs.example.org:443".to_string(),
            "localhost:443".to_string(),
            "foo.localhost".to_string(),
        ];
        match screen_net_allowlist("t", &entries, true) {
            NetScreen::Warn { dead } => {
                assert_eq!(dead, vec!["localhost:443".to_string(), "foo.localhost".to_string()]);
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn screen_refuses_when_every_entry_is_dead() {
        let entries = vec!["localhost:443".to_string(), "svc.localhost:8080".to_string()];
        match screen_net_allowlist("deadtool", &entries, true) {
            NetScreen::Refuse { detail } => {
                assert!(detail.contains("deadtool"), "detail: {detail}");
                assert!(detail.contains("localhost:443"), "detail: {detail}");
                assert!(detail.contains("svc.localhost:8080"), "detail: {detail}");
                assert!(detail.contains("literal"), "remedy missing: {detail}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::endpoint_guard`
Expected: COMPILE ERROR — `host_of_entry`, `NetScreen`, `screen_net_allowlist` not found.

- [ ] **Step 3: Implement** — add above the `#[cfg(test)]` module in `core/src/workers/endpoint_guard.rs` (after `forced_localhost_misconfig`):

```rust
/// Outcome of the generic #459 screen over a registered worker's
/// `Net::Allowlist` entries. Severity is data-driven — no per-worker hooks
/// (per-manifest copies are how the #452 gap happened in the first place):
/// the whole list dead ⇒ the tool is statically unreachable ⇒ refuse; a
/// proper subset dead ⇒ the tool still works for the live hosts ⇒ warn.
#[derive(Debug)]
pub(crate) enum NetScreen {
    /// Nothing to flag: not force-routed, empty list (the broker/zero-egress
    /// posture), or no `localhost`-name entries.
    Ok,
    /// A proper subset of entries is statically dead: register the tool but
    /// warn, naming the dead entries.
    Warn { dead: Vec<String> },
    /// Every entry is statically dead: the caller must treat this exactly
    /// like [`crate::worker_manifest::Resolution::Misconfigured`].
    Refuse { detail: String },
}

/// Host part of a `Net::Allowlist` entry. Entries today are `host:port`
/// (the entry builders' `format!` / web-fetch's `allowlist_to_net_entries`),
/// bare domains (browser-driver passes DB rows verbatim), or bracketed IPv6
/// forms. Strips one trailing `:<digits>` port; anything else is returned
/// whole — an unrecognized shape then classifies `false`, which is the safe
/// no-flag direction (the connect-time proxy check still owns it).
fn host_of_entry(entry: &str) -> &str {
    if entry.starts_with('[') {
        // Bracketed IPv6 literal, with or without a port suffix.
        if let Some(end) = entry.find(']') {
            return &entry[..=end];
        }
    }
    match entry.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            host
        }
        _ => entry,
    }
}

/// The generic #459 screen: classify every allowlist entry with
/// [`host_is_localhost_name`] (via [`host_of_entry`]) when `force_routed`.
/// See [`NetScreen`] for the severity policy. Like the rest of this module:
/// **no DNS** — only the statically-dead RFC 6761 name class is flagged, and
/// literal IPs are never flagged (the proxy's allowlisted-literal carve-out
/// dials them).
pub(crate) fn screen_net_allowlist(
    tool: &str,
    entries: &[String],
    force_routed: bool,
) -> NetScreen {
    if !force_routed || entries.is_empty() {
        return NetScreen::Ok;
    }
    let dead: Vec<String> = entries
        .iter()
        .filter(|e| host_is_localhost_name(host_of_entry(e)))
        .cloned()
        .collect();
    if dead.is_empty() {
        return NetScreen::Ok;
    }
    if dead.len() == entries.len() {
        let hosts = dead.join(", ");
        return NetScreen::Refuse {
            detail: format!(
                "every Net::Allowlist entry for {tool} uses a `localhost` name \
                 ({hosts}), but its egress is force-routed through the egress \
                 proxy, which range-denies what a localhost name resolves to \
                 (SSRF/DNS-rebinding defense) — the tool would register but \
                 every request would fail. Use literal-IP entries (the proxy \
                 dials an operator-allowlisted literal) or routable hostnames, \
                 and update the matching tool_allowlists rows / endpoint env \
                 vars to agree."
            ),
        };
    }
    NetScreen::Warn { dead }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::endpoint_guard`
Expected: PASS — 9 pre-existing + 5 new = 14 tests, 0 failed.

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/endpoint_guard.rs
git commit -m "feat(core): generic Net::Allowlist localhost-name screen (#459)"
```

---

### Task 2: wire the screen into `assemble_registry`

**Files:**
- Modify: `core/src/registry_build.rs` (the `Resolution::Register` arm ~line 155, plus new helper + tests)

**Interfaces:**
- Consumes (from Task 1): `crate::workers::endpoint_guard::{screen_net_allowlist, NetScreen}`; existing `crate::workers::endpoint_guard::egress_will_force_route(is_microvm: bool, get_env: &dyn Fn(&str) -> Option<String>) -> bool`.
- Produces: no new public surface — `assemble_registry`'s signature is unchanged.

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `core/src/registry_build.rs`. First extend the existing fake (replace the current `FakeOutcome` enum and its `resolve` match arm additions):

```rust
    enum FakeOutcome {
        Register,
        /// Register, but with `policy.net = Net::Allowlist(these entries)` —
        /// exercises the #459 generic screen.
        RegisterWithNet(Vec<String>),
        Disabled,
        Misconfigured,
    }
```

and in `FakeManifest::resolve`, add the arm (below `FakeOutcome::Register`):

```rust
                FakeOutcome::RegisterWithNet(entries) => {
                    let mut entry = crate::workers::shell_exec::shell_exec_entry(
                        PathBuf::from(format!("/fake/{}", self.name)),
                        &(ctx.allowlist)(self.name),
                    );
                    entry.policy.net = kastellan_sandbox::Net::Allowlist(entries.clone());
                    Resolution::Register(entry)
                }
```

then the tests:

```rust
    /// Build a ResolveCtx whose env has KASTELLAN_EGRESS_FORCE_ROUTING=1
    /// (the test_ctx helper pins get_env to None, so these build their own).
    fn forced_ctx<'a>(allowlist: &'a dyn Fn(&str) -> Vec<String>) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env: &|k| (k == "KASTELLAN_EGRESS_FORCE_ROUTING").then(|| "1".to_string()),
            exists: &|_p: &Path| false,
            is_dir: &|_p: &Path| false,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist,
        }
    }

    #[test]
    fn force_routed_all_localhost_allowlist_is_refused_like_misconfigured() {
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = forced_ctx(&allow);
        let m = FakeManifest {
            name: "deadtool",
            outcome: FakeOutcome::RegisterWithNet(vec![
                "localhost:443".to_string(),
                "svc.localhost:8080".to_string(),
            ]),
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("deadtool").is_none(), "statically dead tool must not register");
        assert!(loaded.is_empty(), "no LoadedToolRecord for a refused tool");
    }

    #[test]
    fn force_routed_subset_localhost_allowlist_warns_but_registers() {
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = forced_ctx(&allow);
        let m = FakeManifest {
            name: "mixedtool",
            outcome: FakeOutcome::RegisterWithNet(vec![
                "docs.example.org:443".to_string(),
                "localhost:443".to_string(),
            ]),
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("mixedtool").is_some(), "subset-dead tool still registers");
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn unforced_localhost_allowlist_registers_exactly_as_today() {
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = test_ctx(&allow); // get_env is None ⇒ not force-routed
        let m = FakeManifest {
            name: "hosttool",
            outcome: FakeOutcome::RegisterWithNet(vec!["localhost:443".to_string()]),
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("hosttool").is_some(), "no force-routing ⇒ untouched");
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn force_routed_non_allowlist_net_is_not_screened() {
        // shell_exec_entry's policy is Net::Deny — the screen only inspects
        // Net::Allowlist, so this registers exactly as before.
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = forced_ctx(&allow);
        let m = FakeManifest {
            name: "denytool",
            outcome: FakeOutcome::Register,
            allowlist_name: None,
        };
        let (reg, _loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("denytool").is_some());
    }
```

Note: if `shell_exec_entry`'s policy turns out NOT to be `Net::Deny`, keep the last test but set `entry.policy.net = kastellan_sandbox::Net::Deny` via a `RegisterWithNet`-style arm — the point is the non-`Allowlist` variant is not screened. Verify with `grep -n "net:" core/src/workers/shell_exec.rs`.

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib registry_build`
Expected: the two new force-routed tests FAIL (`deadtool` registers; no screen exists yet). `unforced_...` and `..._not_screened` may already pass — that is fine; they are the regression pins.

- [ ] **Step 3: Implement** — in `core/src/registry_build.rs`:

Add the helper (module level, near the top after the imports):

```rust
/// True iff this entry runs as a Firecracker micro-VM worker — the
/// always-force-routed case for the #459 screen (`linux_firecracker/plan.rs`
/// fail-closed refuses to boot a `Net::Allowlist` VM without the egress
/// proxy, so a direct route never exists in VM mode). Non-Linux builds have
/// no VM backend variant, so the answer is statically `false` there.
#[cfg(target_os = "linux")]
fn entry_is_vm(entry: &crate::scheduler::tool_dispatch::ToolEntry) -> bool {
    matches!(
        entry.sandbox_backend,
        Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
    )
}
#[cfg(not(target_os = "linux"))]
fn entry_is_vm(_entry: &crate::scheduler::tool_dispatch::ToolEntry) -> bool {
    false
}
```

Then in `assemble_registry`, at the TOP of the `Resolution::Register(entry)` arm (before the existing `let name = m.name();` … register code):

```rust
            Resolution::Register(entry) => {
                let name = m.name();
                // #459 generic guard: a force-routed worker whose
                // Net::Allowlist carries `localhost` NAMES is statically dead
                // for those hosts (proxy resolves the name → loopback →
                // range-denied). All entries dead ⇒ refuse exactly like
                // Misconfigured; a subset ⇒ warn and register. Per-manifest
                // guards (#452/#457) still fire first inside resolve() with
                // their more precise remedies; this screen is the generic
                // backstop covering every current and future manifest.
                let force_routed = crate::workers::endpoint_guard::egress_will_force_route(
                    entry_is_vm(&entry),
                    ctx.get_env,
                );
                if let kastellan_sandbox::Net::Allowlist(net_entries) = &entry.policy.net {
                    use crate::workers::endpoint_guard::{screen_net_allowlist, NetScreen};
                    match screen_net_allowlist(name, net_entries, force_routed) {
                        NetScreen::Refuse { detail } => {
                            tracing::error!(tool = name, %detail, "worker misconfigured; skipping");
                            continue;
                        }
                        NetScreen::Warn { dead } => {
                            tracing::warn!(
                                tool = name,
                                dead = ?dead,
                                "Net::Allowlist entries use `localhost` names that are \
                                 statically dead under force-routing — requests to them \
                                 will fail (use literal IPs or routable hostnames, and \
                                 update the matching tool_allowlists rows)"
                            );
                        }
                        NetScreen::Ok => {}
                    }
                }
                // …existing body continues unchanged (allowlist lookup, info
                // log, LoadedToolRecord push, reg.insert, tool_docs collect)…
```

(Keep the existing body verbatim below the inserted block; the only structural change is the screen + possible `continue`.)

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib registry_build`
Expected: PASS — all pre-existing tests + 4 new, 0 failed. (Pre-existing tests double as the byte-identical regression pin: their ctxs never set the force-routing env.)

- [ ] **Step 5: Commit**

```bash
git add core/src/registry_build.rs
git commit -m "feat(core): screen force-routed Net::Allowlist workers at registry build (#459)"
```

---

### Task 3: matrix bring-up guard

**Files:**
- Modify: `core/src/channel/matrix/policy.rs` (pub helper + its tests)
- Modify: `core/src/channel/matrix.rs` (re-export — check the existing `pub use policy::…` line with `grep -n "pub use" core/src/channel/matrix.rs` and extend it)
- Modify: `core/src/main/matrix_boot.rs` (bin-side wiring, ~line 52, right after `if let Some(spawn_cfg) = …`)

**Interfaces:**
- Consumes (existing): `crate::workers::endpoint_guard::forced_localhost_misconfig(endpoint_env, endpoint, force_routed, remedy) -> Option<String>` (pub(crate) — same crate); `MatrixSpawnConfig.homeserver_url: String` and `.use_microvm: bool` (both pub); `force_routing: &Option<Arc<ForceRoutingConfig>>` in `spawn_matrix_channel`.
- Produces: `pub fn forced_localhost_homeserver(homeserver_url: &str, force_routed: bool) -> Option<String>` re-exported as `kastellan_core::channel::matrix::forced_localhost_homeserver` (the bin consumes the lib as an external crate, so this MUST be `pub` + re-exported, not `pub(crate)`).

- [ ] **Step 1: Write the failing test** — `core/src/channel/matrix/policy.rs` has no `#[cfg(test)]` module today; add one at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_localhost_homeserver_flags_only_names_under_forcing() {
        // The statically-dead class: localhost NAMES, only when force-routed.
        assert!(forced_localhost_homeserver("http://localhost:8008", true).is_some());
        let d = forced_localhost_homeserver("http://conduit.localhost:6167", true)
            .expect("localhost-name homeserver must be flagged when forced");
        assert!(d.contains("KASTELLAN_MATRIX_HOMESERVER_URL"), "detail: {d}");
        assert!(d.contains("http://conduit.localhost:6167"), "detail: {d}");
        // Not forced: the worker resolves localhost itself — fine (dev conduit).
        assert!(forced_localhost_homeserver("http://localhost:8008", false).is_none());
        // Literal IP: the proxy's allowlisted-literal carve-out dials it.
        assert!(forced_localhost_homeserver("http://127.0.0.1:8008", true).is_none());
        // Routable name: connect-time proxy territory, never flagged here.
        assert!(forced_localhost_homeserver("https://matrix.kastellan.dev", true).is_none());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib channel::matrix::policy`
Expected: COMPILE ERROR — `forced_localhost_homeserver` not found.

- [ ] **Step 3: Implement** — in `core/src/channel/matrix/policy.rs` (below `build_matrix_vm_policy`):

```rust
/// #459: resolve-time guard for the one statically-dead homeserver class — a
/// force-routed matrix worker configured with a `localhost`/`*.localhost`
/// NAME homeserver (the egress proxy resolves the name to loopback and
/// range-denies every CONNECT). Without this check the worker spawns,
/// `matrix.init` fails on every CONNECT, and `PersistentWorker` respawn-loops
/// forever with no actionable operator message. `Some(detail)` ⇒ the caller
/// must NOT start the channel (fail-soft: log and skip; the daemon runs on).
/// Literal-IP homeservers (e.g. `http://127.0.0.1:8008`) are never flagged —
/// the proxy's operator-allowlisted-literal carve-out dials them. Message
/// composition delegates to the shared #452 builder; only the env-var name
/// and the matrix remedy live here (this module owns the matrix env surface).
pub fn forced_localhost_homeserver(
    homeserver_url: &str,
    force_routed: bool,
) -> Option<String> {
    crate::workers::endpoint_guard::forced_localhost_misconfig(
        "KASTELLAN_MATRIX_HOMESERVER_URL",
        homeserver_url,
        force_routed,
        "Use a literal-IP homeserver URL (the egress proxy dials an \
         operator-allowlisted literal, e.g. http://127.0.0.1:8008) or a \
         routable hostname; the matrix channel will not start until then.",
    )
}
```

In `core/src/channel/matrix.rs`, extend the existing `pub use policy::…` re-export list with `forced_localhost_homeserver`.

In `core/src/main/matrix_boot.rs`, insert directly after the `if let Some(spawn_cfg) = …from_env(…)` line (before the blocking-login comment):

```rust
        // #459: a `localhost`-NAME homeserver is statically dead once egress
        // is force-routed (the proxy resolves the name → loopback →
        // range-denies every CONNECT), and the spawn path would respawn-loop
        // on it forever. Refuse the channel up front — fail-soft, daemon
        // unaffected. VM mode counts as always-forced: the Firecracker plan
        // refuses to boot a Net::Allowlist worker without the egress proxy.
        #[cfg(target_os = "linux")]
        let vm_mode = spawn_cfg.use_microvm;
        #[cfg(not(target_os = "linux"))]
        let vm_mode = false;
        if let Some(detail) = kastellan_core::channel::matrix::forced_localhost_homeserver(
            &spawn_cfg.homeserver_url,
            force_routing.is_some() || vm_mode,
        ) {
            error!(%detail, "matrix homeserver misconfigured; channel not started");
            return None;
        }
```

(`return None` is correct here: `matrix_bus` is still `None` at this point and every other failure arm in this function follows the same "log + channel not started" fail-soft family. The bin-side wiring carries no test of its own — the pure helper holds the logic and `matrix_boot` has no test seam today; the compile + helper tests cover it, consistent with the module's extract-verbatim posture.)

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib channel::matrix && cargo build -p kastellan-core`
Expected: test PASS (1 new + existing matrix lib tests), build exit 0 (proves the bin wiring compiles on macOS incl. the cfg arms).

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/matrix/policy.rs core/src/channel/matrix.rs core/src/main/matrix_boot.rs
git commit -m "feat(core): refuse localhost-name matrix homeserver at bring-up when force-routed (#459)"
```

---

### Task 4: remove web-research's now-redundant `content_localhost_warnings`

**Files:**
- Modify: `core/src/workers/web_research.rs` (delete fn ~lines 127–150, its call-site loop ~lines 536–538, its test `content_allowlist_localhost_names_warn_only_when_forced` ~lines 1088–1106; adjust the call-site comment)

**Interfaces:**
- Consumes: nothing new. The generic Task-2 screen now produces the equivalent warn: web-research's endpoint is guaranteed non-localhost at this point (its own #457 guard refuses otherwise), so dead content hosts are always a *proper subset* of the union allowlist ⇒ `NetScreen::Warn` — the same warn-tier semantics this fn provided.
- Produces: no API change; `embed_local_warning` and the endpoint guard stay untouched.

- [ ] **Step 1: Delete the function** `content_localhost_warnings` and its doc comment (the block starting `/// #452-adjacent, warn tier: content-allowlist hosts are the THIRD` down to the closing brace of the fn).

- [ ] **Step 2: Delete the call site** — remove these three lines in `resolve()`:

```rust
        for w in content_localhost_warnings(force_routed, &allowlist) {
            tracing::warn!(target: "web_research.resolve", "{w}");
        }
```

and trim the comment above the `embed_local_warning` call from:

```rust
        // #429 — warn tier, never blocks registration: a `localhost`-name
        // embed endpoint without the embed-broker (hybrid→lexical downgrade),
        // and `localhost`-name content-allowlist entries (dead content
        // fetches under force-routing).
```

to:

```rust
        // #429 — warn tier, never blocks registration: a `localhost`-name
        // embed endpoint without the embed-broker (hybrid→lexical downgrade).
        // (`localhost`-name CONTENT-allowlist entries are warned by the
        // generic #459 registry screen — see registry_build's Register arm.)
```

- [ ] **Step 3: Delete the test** `content_allowlist_localhost_names_warn_only_when_forced` (whole `#[test]` fn).

- [ ] **Step 4: Build + fix any now-unused import** — if `endpoint_guard::host_is_localhost_name` (or the `endpoint_guard` import itself) is now unused, the compiler/clippy will say so; remove exactly what it flags. (`egress_will_force_route` and `forced_localhost_misconfig` usage remains — do not touch those.)

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::web_research && cargo clippy -p kastellan-core --lib -- -D warnings`
Expected: tests PASS (previous count minus the one deleted test), clippy clean.

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/web_research.rs
git commit -m "refactor(web-research): drop content_localhost_warnings, subsumed by the #459 registry screen"
```

---

### Task 5: whole-crate verification sweep

**Files:** none (verification only)

- [ ] **Step 1: Full build + core test suite**

Run: `source "$HOME/.cargo/env" && cargo build --workspace && cargo test -p kastellan-core --lib`
Expected: build exit 0; lib tests 0 failed (net test-count delta vs `main`: +5 endpoint_guard, +4 registry_build, +1 matrix policy, −1 web_research = **+9**).

- [ ] **Step 2: Workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: File-size cap check**

Run: `wc -l core/src/workers/endpoint_guard.rs core/src/registry_build.rs core/src/channel/matrix/policy.rs core/src/main/matrix_boot.rs core/src/workers/web_research.rs`
Expected: endpoint_guard ≲ 400, registry_build ≲ 550 (tests included is fine — the 500 cap is prod LOC), policy ≲ 220, matrix_boot ≲ 150, web_research SHRANK vs its 1107 start. No action unless a *prod* portion exceeds 500.

- [ ] **Step 4: No commit** — report results. The DGX gates (cfg(linux) `entry_is_vm` arm compile/tests + full workspace vs the 2545/0/46 baseline) are run by the controller at session end, not by this task.

---

## Self-review notes (done at plan-writing time)

- **Spec coverage:** §1 screen → Task 1; §2 assemble wiring incl. `entry_is_vm` → Task 2; §3 matrix seam → Task 3; §4 dedup → Task 4; §5 testing → embedded per task + Task 5. Deferred items are correctly absent.
- **Type consistency:** `NetScreen`/`screen_net_allowlist(tool, entries, force_routed)` names identical in Tasks 1–2; `forced_localhost_homeserver(homeserver_url, force_routed)` identical in Task 3's test/impl/wiring.
- **Known look-before-you-edit spots:** exact `pub use policy::…` line in `channel/matrix.rs` (Task 3), `shell_exec_entry`'s `net:` variant (Task 2 note), the exact current comment text at web_research's call site (Task 4 — re-read before editing; line numbers drift).
