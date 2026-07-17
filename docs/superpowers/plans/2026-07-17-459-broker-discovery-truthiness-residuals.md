# #459 Residuals — Broker-Binary Discovery + Truthiness Unification — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the two open #459 guard residuals — (1) refuse a broker-declaring worker at `resolve()` when its broker binary is absent, and (2) unify every boolean worker env-flag onto the one `env_flag_enabled` (`1|true|yes|on`) dialect.

**Architecture:** Residual #1 adds a drift-proof `broker_bin_present(kind, ctx)` (reusing the existing `discover_broker_bin_with` core) and an **unconditional** broker-presence refuse in `assemble_registry`'s Register arm — no signature change. Residual #2 routes all `USE_*`/`ENABLE`/`USE_CONTAINER` parses through the single `force_route::env_flag_enabled` primitive, via a new `ResolveCtx::flag_enabled(key)` sugar where a ctx is in scope and `env_flag_enabled(env_lookup(key))` directly in the two closure-based resolvers.

**Tech Stack:** Rust (kastellan-core), `cargo test` / `cargo clippy`. Dev box macOS (Seatbelt); DGX (aarch64) for the cfg-linux re-gate.

## Global Constraints

- rustc 1.96.0; keep formatting consistent with the surrounding tree (no `cargo fmt`/clippy config yet).
- `cargo clippy -p kastellan-core --all-targets -- -D warnings` must stay clean.
- AGPL-3.0; no new dependencies.
- The single truthiness primitive is `crate::worker_lifecycle::force_route::env_flag_enabled(value: Option<String>) -> bool` (already `pub(crate)`, `1|true|yes|on`, trimmed, case-insensitive). Do NOT introduce a second dialect.
- Broker-presence refuse is **unconditional** (not force-routing-gated), matching the spawn chokepoint's unconditional fail-closed.
- Source env-var flags to source-of-truth constants already defined in each worker module (`USE_BROKER_ENV`, `USE_MICROVM_ENV`, `ENABLE_ENV`, etc.) — never hardcode the string.
- Commit after every green task. Stage only the files each task names (never `git add -A`).
- macOS cannot compile core's `#[cfg(target_os = "linux")]` paths' behavior — the `USE_MICROVM` re-points (web-fetch/web-search/web-research/python-exec Linux blocks) are verified on the **DGX re-gate**, not the Mac.

---

## Task 1: `broker_bin_present` helper (residual #1 primitive)

**Files:**
- Modify: `core/src/broker/config.rs` (add helper after `discover_broker_bin_with`; add tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing private `discover_broker_bin_with(kind, get_env, exists, is_dir, exe_dir) -> Option<PathBuf>`; `crate::worker_manifest::ResolveCtx`; `super::kind::BrokerKind`.
- Produces: `pub(crate) fn broker_bin_present(kind: BrokerKind, ctx: &ResolveCtx) -> bool` — true iff this kind's broker binary is discoverable from the ctx (same inputs as `BrokerConfigs::from_env`, so drift-proof).

- [ ] **Step 1: Write the failing tests**

Add to `core/src/broker/config.rs`'s `mod tests`:

```rust
    #[test]
    fn broker_bin_present_true_when_sibling_exists() {
        let exe_dir = std::path::PathBuf::from("/install/bin");
        let sibling = exe_dir.join(BrokerKind::Search.broker_bin_default());
        let get_env = |_k: &str| None;
        let exists = {
            let sibling = sibling.clone();
            move |p: &std::path::Path| p == sibling.as_path()
        };
        let is_dir = |_p: &std::path::Path| false;
        let allowlist = |_t: &str| Vec::new();
        let ctx = crate::worker_manifest::ResolveCtx {
            get_env: &get_env,
            exists: &exists,
            is_dir: &is_dir,
            exe_dir: Some(exe_dir.as_path()),
            canonicalize: &|_p| None,
            allowlist: &allowlist,
        };
        assert!(super::broker_bin_present(BrokerKind::Search, &ctx));
    }

    #[test]
    fn broker_bin_present_false_when_absent() {
        let get_env = |_k: &str| None;
        let exists = |_p: &std::path::Path| false;
        let is_dir = |_p: &std::path::Path| false;
        let allowlist = |_t: &str| Vec::new();
        let ctx = crate::worker_manifest::ResolveCtx {
            get_env: &get_env,
            exists: &exists,
            is_dir: &is_dir,
            exe_dir: Some(std::path::Path::new("/install/bin")),
            canonicalize: &|_p| None,
            allowlist: &allowlist,
        };
        assert!(!super::broker_bin_present(BrokerKind::Embed, &ctx));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core broker::config::tests::broker_bin_present -- --nocapture`
Expected: FAIL to compile — `broker_bin_present` not found.

- [ ] **Step 3: Implement the helper**

Add after `discover_broker_bin_with` in `core/src/broker/config.rs` (and add `use crate::worker_manifest::ResolveCtx;` — it is already imported at the top for `discover_binary`; reuse that import line, extend it to `use crate::worker_manifest::{discover_binary, ResolveCtx};` — verify it is not already there):

```rust
/// True iff `kind`'s broker binary is discoverable from `ctx` — the same
/// discovery [`from_env`] runs at daemon startup, driven off the `ResolveCtx`
/// probes instead of `std::env`/the live FS. Because `main.rs` feeds both this
/// path and `from_env` the identical `exe_dir`, the answer here cannot drift
/// from the `BrokerConfig` slot the spawn chokepoint will (or won't) find — the
/// same "can't-drift mirror" shape as `endpoint_guard::egress_will_force_route`.
///
/// Used by `assemble_registry` to refuse a broker-declaring worker at
/// `resolve()` time when its binary is absent, instead of registering a tool
/// that only fails fail-closed on its first dispatch.
pub(crate) fn broker_bin_present(kind: BrokerKind, ctx: &ResolveCtx) -> bool {
    discover_broker_bin_with(kind, ctx.get_env, ctx.exists, ctx.is_dir, ctx.exe_dir).is_some()
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core broker::config::tests -- --nocapture`
Expected: PASS (the two new tests + the existing three `discover_*`).

- [ ] **Step 5: Clippy + commit**

```bash
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --lib -- -D warnings
git add core/src/broker/config.rs
git commit -m "feat(core): broker_bin_present resolve-time discovery helper (#459)"
```

---

## Task 2: Resolve-time broker-presence refuse in `assemble_registry` (residual #1 wiring)

**Files:**
- Modify: `core/src/registry_build.rs` (Register arm of `assemble_registry`; add a broker outcome to the test `FakeManifest`; add tests)

**Interfaces:**
- Consumes: `crate::broker::config::broker_bin_present` (Task 1); `entry.broker: Option<crate::broker::BrokerSpec>`; `crate::broker::{BrokerSpec, BrokerKind}`.
- Produces: no new public surface — behavior only (a broker-declaring worker with an absent binary is skipped, no `LoadedToolRecord`).

- [ ] **Step 1: Write the failing tests**

Add a broker outcome to `FakeOutcome` and a helper in `registry_build.rs`'s `mod tests`. First extend the enum + `resolve` match (in the test module):

```rust
    // add to enum FakeOutcome:
        /// Register with `entry.broker = Some(BrokerSpec::search(endpoint))` and
        /// an EMPTY Net::Allowlist (the broker/zero-egress posture) — exercises
        /// the #459 resolve-time broker-presence refuse.
        RegisterBrokerSearch,
```

```rust
    // add to `fn resolve`'s match on &self.outcome:
                FakeOutcome::RegisterBrokerSearch => {
                    let mut entry = crate::workers::shell_exec::shell_exec_entry(
                        PathBuf::from(format!("/fake/{}", self.name)),
                        &(ctx.allowlist)(self.name),
                    );
                    entry.policy.net = kastellan_sandbox::Net::Allowlist(Vec::new());
                    entry.broker =
                        Some(crate::broker::BrokerSpec::search("https://searx.example.org/search"));
                    Resolution::Register(entry)
                }
```

Then the tests:

```rust
    /// A ctx whose only present binary is `kind`'s broker sibling — so
    /// broker_bin_present is true for Search under exe_dir=/install/bin.
    fn ctx_with_search_broker_binary<'a>(
        allowlist: &'a dyn Fn(&str) -> Vec<String>,
        exe_dir: &'a Path,
    ) -> ResolveCtx<'a> {
        // NOTE: `exists` returns true only for the search-broker sibling path.
        // Kept as a closure capturing exe_dir via a leaked 'static is overkill;
        // instead assert with a get_env-free ctx whose `exists` is always true.
        ResolveCtx {
            get_env: &|_k| None,
            exists: &|_p: &Path| true, // sibling resolves ⇒ broker present
            is_dir: &|_p: &Path| false,
            exe_dir: Some(exe_dir),
            canonicalize: &|_p| None,
            allowlist,
        }
    }

    #[test]
    fn broker_worker_registers_when_broker_binary_present() {
        let allow = |_t: &str| Vec::<String>::new();
        let exe_dir = PathBuf::from("/install/bin");
        let ctx = ctx_with_search_broker_binary(&allow, &exe_dir);
        let m = FakeManifest {
            name: "brokertool",
            outcome: FakeOutcome::RegisterBrokerSearch,
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("brokertool").is_some(), "broker present ⇒ registers");
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn broker_worker_refused_when_broker_binary_absent() {
        // exists=false ⇒ no broker binary discoverable ⇒ refuse.
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = test_ctx(&allow); // exists is |_| false, get_env None
        let m = FakeManifest {
            name: "brokerdead",
            outcome: FakeOutcome::RegisterBrokerSearch,
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("brokerdead").is_none(), "absent broker binary ⇒ refused");
        assert!(loaded.is_empty(), "no LoadedToolRecord for a refused broker worker");
    }

    #[test]
    fn broker_worker_refused_even_when_not_force_routed() {
        // test_ctx has get_env=None ⇒ NOT force-routed. The broker refuse is
        // unconditional (independent of force-routing), so it still fires.
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = test_ctx(&allow);
        let m = FakeManifest {
            name: "brokerdead2",
            outcome: FakeOutcome::RegisterBrokerSearch,
            allowlist_name: None,
        };
        let (reg, _loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("brokerdead2").is_none(), "unconditional broker refuse");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core registry_build::tests::broker_worker -- --nocapture`
Expected: `broker_worker_refused_*` FAIL (the worker still registers — refuse not wired yet); the `_present_` test may pass by accident (no refuse). Compilation of the new `FakeOutcome` arm succeeds.

- [ ] **Step 3: Wire the refuse into the Register arm**

In `assemble_registry`, inside `Resolution::Register(entry) => { let name = m.name(); … }`, add — immediately BEFORE the existing `let force_routed = …` / `NetScreen` block:

```rust
                // #459 residual: a broker-declaring worker whose broker binary
                // is not discoverable would register, be advertised to the
                // planner, and then fail fail-closed on its first dispatch at
                // the spawn chokepoint ("no matching broker config"). Refuse it
                // here instead — the same drift-proof discovery the daemon runs
                // at startup (`BrokerConfigs::from_env`), keyed off this ctx.
                // Unconditional: a missing broker binary is dead in every mode.
                if let Some(spec) = &entry.broker {
                    if !crate::broker::config::broker_bin_present(spec.kind, ctx) {
                        tracing::error!(
                            tool = name,
                            kind = ?spec.kind,
                            "worker declares a broker but its binary is not \
                             discoverable; skipping — it would register but every \
                             dispatch fails fail-closed at the spawn chokepoint"
                        );
                        continue;
                    }
                }
```

- [ ] **Step 4: Run to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core registry_build::tests -- --nocapture`
Expected: PASS (all three new broker tests + every pre-existing registry_build test).

- [ ] **Step 5: Clippy + commit**

```bash
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --all-targets -- -D warnings
git add core/src/registry_build.rs
git commit -m "feat(core): refuse broker-declaring worker with absent broker binary at resolve (#459)"
```

---

## Task 3: `ResolveCtx::flag_enabled` helper (residual #2 primitive)

**Files:**
- Modify: `core/src/worker_manifest.rs` (add `impl ResolveCtx { fn flag_enabled }`; add a test)

**Interfaces:**
- Consumes: `crate::worker_lifecycle::force_route::env_flag_enabled`.
- Produces: `pub(crate) fn ResolveCtx::flag_enabled(&self, key: &str) -> bool`.

- [ ] **Step 1: Write the failing test**

Add to `core/src/worker_manifest.rs`'s `#[cfg(test)] mod tests` (create the module if the file has none — check first; it has resolve tests already):

```rust
    #[test]
    fn flag_enabled_honors_the_unified_truthiness_dialect() {
        let mk = |val: Option<&'static str>| {
            let get_env = move |k: &str| (k == "K").then(|| val.unwrap().to_string());
            // `val: None` ⇒ the closure still returns None for "K".
            let none = move |_k: &str| None::<String>;
            (get_env, none)
        };
        for v in ["1", "true", "yes", "on", " TRUE "] {
            let get_env = move |k: &str| (k == "K").then(|| v.to_string());
            let ctx = ResolveCtx {
                get_env: &get_env,
                exists: &|_p: &Path| false,
                is_dir: &|_p: &Path| false,
                exe_dir: None,
                canonicalize: &|_p| None,
                allowlist: &|_t| Vec::new(),
            };
            assert!(ctx.flag_enabled("K"), "{v:?} should enable");
        }
        for v in ["0", "false", "off", "", "banana"] {
            let get_env = move |k: &str| (k == "K").then(|| v.to_string());
            let ctx = ResolveCtx {
                get_env: &get_env,
                exists: &|_p: &Path| false,
                is_dir: &|_p: &Path| false,
                exe_dir: None,
                canonicalize: &|_p| None,
                allowlist: &|_t| Vec::new(),
            };
            assert!(!ctx.flag_enabled("K"), "{v:?} must not enable");
        }
        // unset ⇒ false
        let _ = mk; // (drop the unused helper if not needed)
        let unset = |_k: &str| None;
        let ctx = ResolveCtx {
            get_env: &unset,
            exists: &|_p: &Path| false,
            is_dir: &|_p: &Path| false,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist: &|_t| Vec::new(),
        };
        assert!(!ctx.flag_enabled("K"));
    }
```

(Simplify the test if the file's existing style is terser — the essential assertions are: `1|true|yes|on| TRUE ` enable; `0|false|off|""|banana|unset` do not. Drop the unused `mk` closure.)

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core worker_manifest::tests::flag_enabled -- --nocapture`
Expected: FAIL to compile — no method `flag_enabled`.

- [ ] **Step 3: Implement the method**

Add near the `ResolveCtx` struct in `core/src/worker_manifest.rs`:

```rust
impl ResolveCtx<'_> {
    /// True iff env var `key` is set to a truthy value under the one daemon-wide
    /// flag dialect (`1|true|yes|on`, trimmed, case-insensitive), shared with
    /// force-routing via [`crate::worker_lifecycle::force_route::env_flag_enabled`].
    /// Every worker `USE_*`/`ENABLE` opt-in flag goes through this so
    /// `…=true` can never silently read as off while a neighbouring
    /// `FORCE_ROUTING=true` reads on (#459).
    pub(crate) fn flag_enabled(&self, key: &str) -> bool {
        crate::worker_lifecycle::force_route::env_flag_enabled((self.get_env)(key))
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core worker_manifest::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Clippy + commit**

```bash
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --lib -- -D warnings
git add core/src/worker_manifest.rs
git commit -m "feat(core): ResolveCtx::flag_enabled unified env-flag truthiness (#459)"
```

---

## Task 4: Re-point ctx-based flag sites onto `ctx.flag_enabled` (residual #2)

**Files:**
- Modify: `core/src/workers/web_fetch.rs`, `core/src/workers/web_search.rs`, `core/src/workers/web_research.rs`, `core/src/workers/python_exec.rs` (parse sites + one widened-truthiness test per Mac-testable flag)

**Interfaces:**
- Consumes: `ResolveCtx::flag_enabled` (Task 3).
- Produces: no new surface — behavior widening only.

The mechanical edit at every site: replace
`(ctx.get_env)(<CONST>).unwrap_or_default().trim() == "1"` with `ctx.flag_enabled(<CONST>)`.

Exact sites (verify line numbers with `grep -n 'trim() == "1"'` before editing — they drift):
- `web_fetch.rs` — `USE_MICROVM_ENV` *(cfg(linux) — DGX-verified)*
- `web_search.rs` — `USE_BROKER_ENV` *(Mac)*, `USE_MICROVM_ENV` *(cfg(linux) — DGX)*
- `web_research.rs` — `USE_SEARCH_BROKER_ENV` *(Mac)*, `USE_EMBED_BROKER_ENV` *(Mac)*, `USE_MICROVM_ENV` *(cfg(linux) — DGX)*
- `python_exec.rs` — `ENABLE_ENV` (every site, incl. the macOS `container` block, the Linux `microvm` block, and the host-mode re-check), `USE_CONTAINER_ENV` *(macOS)*, `USE_MICROVM_ENV` *(cfg(linux) — DGX)*

- [ ] **Step 1: Write the failing widened-truthiness tests (Mac-testable sites)**

Add ONE test per Mac-testable flag, mirroring the module's existing sibling resolve test's fake-ctx construction. Representative (web_search `USE_BROKER`) — put in `web_search.rs`'s test module, adapting the ctx builder to match the existing broker-mode test in that module:

```rust
    #[test]
    fn use_broker_accepts_true_not_just_one() {
        // The existing broker-mode test uses "1"; the unified dialect must also
        // accept "true"/"on". Assert broker mode (entry.broker Some, empty net).
        let get_env = |k: &str| match k {
            k if k == USE_BROKER_ENV => Some("true".to_string()),
            k if k == ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let ctx = ResolveCtx {
            get_env: &get_env,
            exists: &|_p| true,
            is_dir: &|_p| false,
            exe_dir: Some(std::path::Path::new("/install/bin")),
            canonicalize: &|_p| None,
            allowlist: &|_t| Vec::new(),
        };
        match WebSearchManifest.resolve(&ctx) {
            Resolution::Register(entry) => {
                assert!(entry.broker.is_some(), "USE_BROKER=true ⇒ broker mode");
            }
            other => panic!("expected Register, got {other:?}"),
        }
    }
```

Add the analogous one-case tests:
- `web_research.rs`: `use_search_broker_accepts_true` (set `USE_SEARCH_BROKER_ENV="on"` + `ENDPOINT_ENV=…` + a discoverable search-broker binary via `exists:true` if the resolve needs it; assert `entry.broker` is `Some` with `kind == Search`); `use_embed_broker_accepts_true` (set `USE_EMBED_BROKER_ENV="true"` + an embed endpoint; assert embed-broker mode per the module's existing embed-broker test observable).
- `python_exec.rs` (macOS-gated block): `enable_accepts_true` under `#[cfg(target_os = "macos")]` — set `ENABLE_ENV="yes"` (+ `USE_CONTAINER_ENV` unset) and assert the manifest resolves to the host-mode Register (not Disabled), mirroring the existing enable test.

Match each test's ctx/assertion to the **existing sibling test in that module** — read it first so the fake-ctx fields and the registered-entry observable are correct.

- [ ] **Step 2: Run to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core workers::web_search workers::web_research workers::python_exec -- --nocapture`
Expected: the new `*_accepts_true/on/yes` tests FAIL (old `== "1"` rejects the non-"1" value ⇒ wrong branch).

- [ ] **Step 3: Apply the mechanical re-points**

Edit every site listed above: `(ctx.get_env)(<CONST>).unwrap_or_default().trim() == "1"` → `ctx.flag_enabled(<CONST>)`. Leave the surrounding cfg-gates and comments intact (only the parse expression changes).

- [ ] **Step 4: Run to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core workers::web_fetch workers::web_search workers::web_research workers::python_exec -- --nocapture`
Expected: PASS — the new widened tests + every pre-existing resolve test (which use `"1"`, still truthy).

- [ ] **Step 5: Clippy + commit**

```bash
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --all-targets -- -D warnings
git add core/src/workers/web_fetch.rs core/src/workers/web_search.rs core/src/workers/web_research.rs core/src/workers/python_exec.rs
git commit -m "refactor(core): route ctx-based worker env-flags through ResolveCtx::flag_enabled (#459)"
```

---

## Task 5: Re-point closure-based flag sites (browser-driver, gliner-relex) (residual #2)

**Files:**
- Modify: `core/src/workers/browser_driver.rs`, `core/src/workers/gliner_relex/resolve.rs` (parse sites in the `env_lookup`-closure `resolve_env` + one widened test each)

**Interfaces:**
- Consumes: `crate::worker_lifecycle::force_route::env_flag_enabled`.
- Produces: no new surface.

These two resolvers take a ctx-free `env_lookup: impl Fn(&str) -> Option<String>`, so they call the primitive directly. Edit: `env_lookup(<KEY>).unwrap_or_default().trim() == "1"` (and the `.map(|v| v.trim() == "1").unwrap_or(false)` form in gliner) → `env_flag_enabled(env_lookup(<KEY>))`. Add at the top of each file:
`use crate::worker_lifecycle::force_route::env_flag_enabled;`

Sites:
- `browser_driver.rs::resolve_env` — `KASTELLAN_BROWSER_DRIVER_ENABLE` (the `!= "1"` guard near line 93 becomes `if !env_flag_enabled(env_lookup("KASTELLAN_BROWSER_DRIVER_ENABLE"))`).
- `gliner_relex/resolve.rs::resolve_env` — `KASTELLAN_GLINER_RELEX_ENABLE` (near line 189) and `KASTELLAN_GLINER_RELEX_USE_CONTAINER` (macOS-gated, near line 222).

- [ ] **Step 1: Write the failing widened tests**

`browser_driver.rs` test module — mirror the existing enable/disable resolve test:

```rust
    #[test]
    fn enable_accepts_true_not_just_one() {
        // resolve_env with ENABLE="true" must NOT take the Disabled path.
        let env = |k: &str| (k == "KASTELLAN_BROWSER_DRIVER_ENABLE").then(|| "true".to_string());
        let got = resolve_env(
            env,
            /* is_dir */ |_p: &Path| false,
            /* exists */ |_p: &Path| true,
            /* canonicalize */ |_p: &Path| None,
            /* read_closure */ |_p: &Path| Vec::new(),
        );
        // Adapt the arg list + assertion to the actual resolve_env signature and
        // its Resolution/return shape in this module (read the existing test).
        assert!(!matches!(got, Resolution::Disabled { .. }), "ENABLE=true ⇒ not Disabled");
    }
```

`gliner_relex/resolve.rs` test module — mirror the existing enable test with `KASTELLAN_GLINER_RELEX_ENABLE="on"`, asserting it does NOT resolve Disabled (adapt to the module's actual `resolve_env` signature and return type).

- [ ] **Step 2: Run to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core workers::browser_driver workers::gliner_relex -- --nocapture`
Expected: the new `enable_accepts_*` tests FAIL (old `== "1"` ⇒ Disabled path).

- [ ] **Step 3: Apply the re-points + add the `use`**

Add `use crate::worker_lifecycle::force_route::env_flag_enabled;` to each file and swap the two parse forms per the site list above. Keep the surrounding comments (the `trim()` rationale comments become slightly stale — update them to note the value is now `1|true|yes|on`, not strictly `"1"`).

- [ ] **Step 4: Run to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core workers::browser_driver workers::gliner_relex -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Clippy + commit**

```bash
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --all-targets -- -D warnings
git add core/src/workers/browser_driver.rs core/src/workers/gliner_relex/resolve.rs
git commit -m "refactor(core): route closure-based worker env-flags through env_flag_enabled (#459)"
```

---

## Task 6: Full Mac verification + review + DGX re-gate + PR

**Files:** none (verification + docs)

- [ ] **Step 1: Whole-crate Mac gate**

```bash
source "$HOME/.cargo/env"
cargo build -p kastellan-core
cargo clippy -p kastellan-core --all-targets -- -D warnings
cargo test -p kastellan-core workers:: broker::config:: registry_build:: worker_manifest:: -- --nocapture
```
Expected: build clean, clippy clean, all targeted tests PASS.

- [ ] **Step 2: `/review` → `/fixall` on the branch diff**; address any Critical/Important, re-verify.

- [ ] **Step 3: DGX re-gate** (cfg-linux `USE_MICROVM` re-points + registry_build VM test — Mac compiles them out):

```bash
ssh dgx 'cd ~/src/kastellan && git fetch origin --quiet && git checkout feat/459-broker-discovery-truthiness && git pull --ff-only'
ssh dgx 'setsid bash -lc "source ~/.cargo/env && cd ~/src/kastellan && cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 && cargo test --workspace 2>&1 && cargo clippy --workspace --all-targets -- -D warnings 2>&1; echo DONE_EXIT=\$?" > ~/dgx-459.log 2>&1 </dev/null &'
```
Poll `~/dgx-459.log` for `DONE_EXIT`. Expected: clippy clean; full workspace green (new baseline over 2564/0/47 — the added tests are the delta).

- [ ] **Step 4: Update HANDOVER.md + ROADMAP.md** (residual #1+#2 done, #459 open only for #3; new DGX baseline). Commit.

- [ ] **Step 5: Push + open PR** to `main`, linking #459; note residual #3 stays open.

---

## Self-Review

**Spec coverage:**
- Residual #1 broker discovery → Task 1 (`broker_bin_present`) + Task 2 (`assemble_registry` refuse). ✓
- Severity = hard refuse, unconditional → Task 2 Step 3 (`continue`, ERROR log, not force-routing-gated; `broker_worker_refused_even_when_not_force_routed`). ✓
- Residual #2 one primitive + two call styles → Task 3 (`flag_enabled`) + Task 4 (ctx sites) + Task 5 (closure sites via `env_flag_enabled`). ✓
- All enumerated sites (USE_MICROVM ×4, USE_BROKER, USE_SEARCH_BROKER, USE_EMBED_BROKER, ENABLE ×3-workers, USE_CONTAINER ×2) → Tasks 4/5 site lists. ✓
- Behaviour-change note (`=true` now enables; `"1"` still works; existing tests stay green) → Task 4/5 Step 4 expectations. ✓
- DGX re-gate for cfg-linux → Task 6 Step 3. ✓
- Residual #3 deferred → not in scope, noted in Task 6 Step 4/5. ✓

**Placeholder scan:** Task 4/5 tests say "mirror the existing sibling test" — this is a deliberate instruction to match each module's real fake-ctx harness (which varies per worker), not a placeholder for missing logic; the assertion and flag value are specified exactly. Representative complete code is given for web_search + browser_driver. Acceptable given the per-module harness variance.

**Type consistency:** `broker_bin_present(kind, ctx) -> bool` (Task 1) is the exact name/signature called in Task 2. `ResolveCtx::flag_enabled(&self, key) -> bool` (Task 3) is the exact method called in Task 4. `BrokerSpec::search(endpoint)` used in Task 2 matches the real constructor. `env_flag_enabled(Option<String>)` used in Task 5 matches the real primitive. ✓
