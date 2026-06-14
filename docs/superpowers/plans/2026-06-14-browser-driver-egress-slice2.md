# browser-driver egress slice #2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `browser-driver` run in the default force-routed deployment with egress enforced at the netns boundary (private netns → egress-proxy sidecar → allowlist+SSRF at CONNECT), via an in-jail loopback-TCP↔UDS shim and **transparent tunneling** (no MITM of the browser); remove the dev-only escape hatch. Closes #280 and #263.

**Architecture:** A headless Chromium can't speak `CONNECT`-over-UDS, so the Python worker runs a tiny loopback-TCP↔UDS relay (a dumb byte-pipe) and launches Chromium with `--proxy-server=127.0.0.1:<port>`. The sidecar runs in a new **no-MITM mode** so the browser keeps end-to-end TLS to the origin (preserving Chromium-grade cert validation). The browser becomes a normal force-routable `Net::Allowlist` worker; the only browser-specific bit left is a one-line `disable_mitm` opt-out on its sidecar.

**Tech Stack:** Rust (core, sandbox, egress-proxy worker), Python 3.12 (browser-driver worker, asyncio + threading), Playwright/Chromium, bwrap (Linux), Seatbelt (macOS).

**Spec:** `docs/superpowers/specs/2026-06-14-browser-driver-egress-slice2-design.md`

**Toolchain:** `source "$HOME/.cargo/env"` before any cargo command. Python tests: `cd workers/browser-driver && .venv/bin/python -m pytest` (or `python -m pytest` if the venv is active). The DGX (native Linux) runs `ssh dgx '<cmd>'` for bwrap/PG acceptance; the dev Mac runs Seatbelt.

---

## Task ordering & dependencies

1. **Task 1** — egress-proxy no-MITM mode (introduces the `KASTELLAN_EGRESS_PROXY_DISABLE_MITM` env contract).
2. **Task 2** — core egress: thread `disable_mitm` to the sidecar (depends on Task 1's env name).
3. **Task 3** — force_route.rs: remove the exemption, add the one-line opt-out (depends on Task 2's `NetWorkerSpawn` field).
4. **Task 4** — tool_host.rs: drop the now-dead `ForceRouteUnconfined` (depends on Task 3).
5. **Task 5** — sandbox Seatbelt: loopback-TCP for the browser (independent; do any time).
6. **Task 6** — Python shim module (independent).
7. **Task 7** — Python worker wiring (depends on Task 6).
8. **Task 8** — manifest docs + escape-hatch cleanup (depends on Tasks 3–4 so removed symbols are gone).
9. **Task 9** — acceptance e2e (depends on all).
10. **Task 10** — docs + close issues (last).

---

## Task 1: egress-proxy no-MITM mode

**Files:**
- Modify: `workers/egress-proxy/src/proxy.rs` (`MitmCtx` struct ~101-109; `handle_conn` is_tls branch ~178-200)
- Modify: `workers/egress-proxy/src/main.rs` (env read + `MitmCtx` construction ~111-116)
- Test: `workers/egress-proxy/src/proxy.rs` `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing test**

Find the existing real-UDS `handle_conn` pass-through test in `proxy.rs`'s test module (it spawns an upstream `TcpListener`, builds a `MitmCtx`, connects a `UnixStream` pair, writes a `CONNECT`, then writes a first tunnel byte). Copy it to a new test that sends a **TLS-looking** first byte (`0x16`) with `disable_mitm: true`, and asserts the reported decision has `tls_intercepted == false` (transparent tunnel, not MITM). Add to the test module:

```rust
#[test]
fn disable_mitm_forces_transparent_tunnel_even_for_tls() {
    use crate::report::Decision;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    // Upstream the sidecar will tunnel to: echo one byte back.
    let upstream = std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream");
    let upstream_addr = upstream.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = upstream.accept() {
            let mut b = [0u8; 1];
            let _ = s.read(&mut b);
            let _ = s.write_all(&b); // echo
        }
    });

    // Allowlist the loopback upstream by host:port.
    let allow = HostAllowlist::from_endpoints(&[upstream_addr.to_string()]);
    let resolver = StdResolve;
    let (mut client, server) = UnixStream::pair().expect("uds pair");

    // Capture decisions.
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Decision>::new()));
    struct CapReporter(std::sync::Arc<std::sync::Mutex<Vec<Decision>>>);
    impl crate::report::Reporter for CapReporter {
        fn report(&mut self, d: Decision) { self.0.lock().unwrap().push(d); }
    }
    let mut reporter = CapReporter(captured.clone());

    let ca = ca::generate_ca().expect("ca");
    let mut cache = crate::leaf_cache::LeafCache::new();
    let upstream_tls = std::sync::Arc::new(
        crate::pins::build_upstream_client_config(None).expect("tls"),
    );
    let mut mitm = MitmCtx {
        ca: &ca,
        leaf_cache: &mut cache,
        upstream_tls,
        secret_hashes_path: None,
        disable_mitm: true, // <-- the new field
    };

    let handler = std::thread::spawn(move || {
        handle_conn(server, "browser-driver", &allow,
            &StdResolve, &mut reporter, &mut mitm);
    });

    // CONNECT to the loopback upstream, read 200, send a TLS ClientHello first byte.
    client.write_all(
        format!("CONNECT {upstream_addr} HTTP/1.1\r\nHost: {upstream_addr}\r\n\r\n").as_bytes(),
    ).unwrap();
    let mut head = [0u8; 39];
    client.read_exact(&mut head).unwrap(); // "HTTP/1.1 200 Connection Established\r\n\r\n"
    client.write_all(&[0x16]).unwrap(); // TLS record byte
    let mut echoed = [0u8; 1];
    client.read_exact(&mut echoed).unwrap();
    assert_eq!(echoed[0], 0x16, "transparent tunnel must relay the byte");
    drop(client);
    handler.join().unwrap();

    let decisions = captured.lock().unwrap();
    let last = decisions.last().expect("a decision");
    assert!(!last.tls_intercepted,
        "disable_mitm must transparently tunnel even a TLS ClientHello");
}
```

> NB: mirror the *exact* `Reporter` trait import path and `Decision` field set from the existing pass-through test — adapt the harness above to whatever helper the file already uses (it may already have a capturing reporter). The behavioral assertion (`!tls_intercepted` with a `0x16` first byte) is the point.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p kastellan-worker-egress-proxy disable_mitm_forces_transparent_tunnel -- --nocapture`
Expected: FAIL to **compile** — `MitmCtx` has no field `disable_mitm`.

- [ ] **Step 3: Add the field + gate the branch**

In `proxy.rs`, add to `MitmCtx`:

```rust
pub struct MitmCtx<'a> {
    pub ca: &'a crate::ca::CaMaterial,
    pub leaf_cache: &'a mut crate::leaf_cache::LeafCache,
    pub upstream_tls: std::sync::Arc<rustls::ClientConfig>,
    pub secret_hashes_path: Option<std::path::PathBuf>,
    /// When true, never MITM — always transparently tunnel, even a TLS
    /// ClientHello. Set for workers that do their own end-to-end TLS and cannot
    /// trust our per-instance CA (the browser; egress slice #2). The allowlist +
    /// SSRF check at CONNECT still apply; only inspection is skipped.
    pub disable_mitm: bool,
}
```

In `handle_conn`, change the peek so no-MITM mode forces transparent tunnel:

```rust
            // Peek the first tunnel byte (non-consuming) — unless MITM is
            // disabled for this worker, in which case we always tunnel.
            let is_tls = !mitm.disable_mitm
                && peek_first_byte(&client)
                    .map(crate::mitm::looks_like_tls)
                    .unwrap_or(false);
```

- [ ] **Step 4: Wire the env read in `main.rs`**

After the `worker` env read (near line 44), add:

```rust
    // No-MITM mode: a worker that does end-to-end TLS itself and cannot trust
    // our per-instance CA (the browser, egress slice #2) sets this so the proxy
    // transparently tunnels instead of intercepting. Allowlist + SSRF still apply.
    let disable_mitm = matches!(
        std::env::var("KASTELLAN_EGRESS_PROXY_DISABLE_MITM")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1")
    );
```

In the `MitmCtx` construction inside the accept loop (~111-116), add the field:

```rust
                let mut mitm = MitmCtx {
                    ca: ca.as_ref(),
                    leaf_cache: cache,
                    upstream_tls: std::sync::Arc::clone(upstream_tls),
                    secret_hashes_path: Some(secret_hashes_path.clone()),
                    disable_mitm,
                };
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p kastellan-worker-egress-proxy -- --nocapture`
Expected: PASS (new test + all existing egress-proxy unit tests). Any existing `MitmCtx { .. }` literals in other tests now also need `disable_mitm: false` — add it.

- [ ] **Step 6: Commit**

```bash
git add workers/egress-proxy/src/proxy.rs workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): no-MITM mode (KASTELLAN_EGRESS_PROXY_DISABLE_MITM)

Transparent-tunnel even a TLS ClientHello when set; allowlist+SSRF at
CONNECT unchanged. For the browser, which does end-to-end TLS and cannot
trust our per-instance CA (browser-driver egress slice #2).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: core egress — thread `disable_mitm` to the sidecar

**Files:**
- Modify: `core/src/egress/spawn.rs` (`proxy_policy` ~63, `spawn_sidecar` ~103, `ENV_*` consts ~11-15)
- Modify: `core/src/egress/net_worker.rs` (`NetWorkerSpawn` ~31-42, `spawn_net_worker` sidecar call ~147)
- Test: both files' `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing test (proxy_policy carries the env)**

In `spawn.rs` test module add:

```rust
    #[test]
    fn proxy_policy_sets_disable_mitm_env_when_requested() {
        let p = proxy_policy(
            Path::new("/bin/proxy"), &["example.com:443".into()],
            Path::new("/scratch"), "browser-driver", None, true,
        );
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_DISABLE_MITM], "1");
    }

    #[test]
    fn proxy_policy_omits_disable_mitm_env_when_false() {
        let p = proxy_policy(
            Path::new("/bin/proxy"), &["example.com:443".into()],
            Path::new("/scratch"), "web-fetch", None, false,
        );
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert!(!env.contains_key(ENV_DISABLE_MITM));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kastellan-core --lib egress::spawn`
Expected: FAIL to compile — `ENV_DISABLE_MITM` and the 6th `proxy_policy` arg don't exist.

- [ ] **Step 3: Add the const + param + env push**

In `spawn.rs`, add the const beside the others:

```rust
/// Env key that puts the sidecar into no-MITM (transparent-tunnel) mode for
/// workers that do their own end-to-end TLS (the browser). Must match the read
/// in `egress-proxy::main`.
const ENV_DISABLE_MITM: &str = "KASTELLAN_EGRESS_PROXY_DISABLE_MITM";
```

Change `proxy_policy`'s signature to add a trailing `disable_mitm: bool` and push the env (omit-when-false keeps the no-flag path byte-identical):

```rust
pub fn proxy_policy(
    binary: &Path,
    allowlist: &[String],
    scratch: &Path,
    worker: &str,
    cert_pins_json: Option<&str>,
    disable_mitm: bool,
) -> SandboxPolicy {
    // ... existing body up to the pins push ...
    if disable_mitm {
        env.push((ENV_DISABLE_MITM.to_string(), "1".to_string()));
    }
    // ... rest unchanged ...
}
```

Change `spawn_sidecar`'s signature to add a trailing `disable_mitm: bool` and forward it:

```rust
pub fn spawn_sidecar(
    backend: &dyn SandboxBackend,
    binary: &Path,
    allowlist: &[String],
    scratch: &Path,
    worker: &str,
    cert_pins_json: Option<&str>,
    disable_mitm: bool,
) -> anyhow::Result<SidecarHandle> {
    let policy = proxy_policy(binary, allowlist, scratch, worker, cert_pins_json, disable_mitm);
    // ... rest unchanged ...
}
```

Fix the two existing `proxy_policy(...)` calls in `spawn.rs` tests (`policy_uses_proxy_egress_and_net_client`, `proxy_policy_omits_pins_env_when_none`, `proxy_policy_includes_pins_env_when_set`) to pass a trailing `false`.

- [ ] **Step 4: Add the `NetWorkerSpawn` field + forward it**

In `net_worker.rs`, add to `NetWorkerSpawn`:

```rust
    /// Put this worker's sidecar into no-MITM (transparent-tunnel) mode. Set for
    /// the browser, which does end-to-end TLS and can't trust our CA (slice #2).
    pub disable_mitm: bool,
```

In `spawn_net_worker`, forward it to `spawn_sidecar`:

```rust
    let mut sidecar = spawn_sidecar(
        params.backend,
        params.proxy_bin,
        params.allowlist,
        scratch,
        params.worker_name,
        params.cert_pins_json,
        params.disable_mitm,
    )
    .map_err(|e| ToolHostError::Io(std::io::Error::other(format!("egress sidecar: {e}"))))?;
```

Add `disable_mitm: false` to every `NetWorkerSpawn { .. }` literal in `net_worker.rs` tests (there are 4: `spawn_net_worker_fails_closed_*`, `spawn_forced_net_worker_fails_closed_*`, `spawn_forced_net_worker_cleans_scratch_on_failure`, `net_worker_spawn_struct_carries_pins_field`).

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p kastellan-core --lib egress`
Expected: PASS. (The `force_route.rs` `NetWorkerSpawn` literal and the two e2e files still need the field — they're fixed in Task 3 / Task 9; the lib build alone may fail there. If so, temporarily add `disable_mitm: false` to `force_route.rs`'s `NetWorkerSpawn` literal so the crate compiles; Task 3 sets it properly.)

- [ ] **Step 6: Commit**

```bash
git add core/src/egress/spawn.rs core/src/egress/net_worker.rs
git commit -m "feat(egress): thread disable_mitm to the per-worker sidecar

NetWorkerSpawn.disable_mitm -> proxy_policy/spawn_sidecar push
KASTELLAN_EGRESS_PROXY_DISABLE_MITM only when set (no-flag path byte-identical).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: force_route.rs — remove the exemption, add the one-line MITM opt-out

**Files:**
- Modify: `core/src/worker_lifecycle/force_route.rs` (whole file — see below)
- Test: same file's `#[cfg(test)] mod tests`

**Net effect:** `ForceRouteAction` collapses to `{ Sidecar, Direct }`; `force_route_action(force_routing_active, net_force_routable)` drops `worker_name`/`browser_dev_override`; `ForceRoutingConfig` drops `browser_insecure_direct_net`; the browser flows through the generic `Sidecar` arm with `disable_mitm: true`.

- [ ] **Step 1: Update the tests first (they encode the new contract)**

Replace the browser-exemption test cluster with the new expectations. Delete: `action_force_on_browser_without_override_refuses`, `action_force_on_browser_with_override_is_insecure_dev_exempt`, `action_browser_exemption_checked_before_generic_routable`, `browser_driver_force_routed_without_override_refuses_fail_closed`, `browser_driver_force_routed_with_override_takes_direct_path`, `enabled_with_browser_override_threads_the_flag`. Update `action_force_off_is_always_direct` and `action_force_on_*` to the 2-arg signature, and add the new browser-is-now-sidecar test:

```rust
    #[test]
    fn action_force_off_is_always_direct() {
        for routable in [true, false] {
            assert_eq!(force_route_action(false, routable), ForceRouteAction::Direct);
        }
    }

    #[test]
    fn action_force_on_routable_is_sidecar() {
        assert_eq!(force_route_action(true, true), ForceRouteAction::Sidecar);
    }

    #[test]
    fn action_force_on_not_routable_is_direct() {
        assert_eq!(force_route_action(true, false), ForceRouteAction::Direct);
    }

    /// The slice-#2 change: browser-driver is now a normal force-routable worker.
    /// Under force-routing it takes the Sidecar path (no refusal, no exemption).
    #[test]
    fn browser_driver_force_routed_takes_sidecar_path() {
        let policy = SandboxPolicy {
            net: Net::Allowlist(vec!["example.com:443".into()]),
            ..SandboxPolicy::default()
        };
        let scratch = tempfile::tempdir().expect("scratch root");
        let cfg = config_with(scratch.path().to_path_buf());
        let res = spawn_worker_maybe_forced(
            Some(&cfg), &FailBackend, &spec_for(&policy), BROWSER_DRIVER_TOOL);
        // Sidecar path maps the (failing) sidecar spawn to Io — proving it tried
        // to force-route the browser rather than refuse or run direct.
        assert!(matches!(res, Err(ToolHostError::Io(_))),
            "browser-driver under force-routing must force-route (Io fail-closed)");
    }
```

Update `config_with` / `config_with_browser_override` / `ForceRoutingConfig::new` call sites to drop the `browser_insecure_direct_net` arg (delete `config_with_browser_override` entirely). Update `disabled_resolves_to_none_even_with_a_binary`, `enabled_with_binary_resolves_to_some`, `enabled_without_binary_fails_closed` to the new `resolve_force_routing` arity (drop the trailing bool), and drop the `assert!(!cfg.browser_insecure_direct_net)` line.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kastellan-core --lib worker_lifecycle::force_route`
Expected: FAIL to compile (old variants/args still referenced by production code).

- [ ] **Step 3: Simplify the production code**

In `force_route.rs`:

(a) Delete the const `ENV_BROWSER_INSECURE_DIRECT_NET` (line ~37-40).

(b) `ForceRoutingConfig`: remove the `browser_insecure_direct_net` field and its doc; `new()` drops that param:

```rust
pub fn new(
    proxy_bin: PathBuf,
    scratch_root: PathBuf,
    make_sink: DecisionSinkFactory,
) -> Self {
    Self { proxy_bin, scratch_root, make_sink }
}
```

(c) Keep `BROWSER_DRIVER_TOOL` but repurpose its doc — it now marks the one worker whose sidecar runs in no-MITM mode:

```rust
/// The browser does end-to-end TLS itself and cannot trust our per-instance MITM
/// CA, so its sidecar must run in no-MITM (transparent-tunnel) mode. This is the
/// only worker-specific bit of force-routing left after egress slice #2 — the
/// browser is otherwise a normal force-routable `Net::Allowlist` worker.
pub(crate) const BROWSER_DRIVER_TOOL: &str = "browser-driver";
```

(d) `ForceRouteAction` collapses:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForceRouteAction {
    /// Route through a per-worker egress-proxy sidecar (egress enforced at the
    /// netns boundary).
    Sidecar,
    /// Spawn directly via `spawn_worker` — force-routing off, or a
    /// non-force-routable net (`Net::Deny`/`Net::ProxyEgress`).
    Direct,
}
```

(e) `force_route_action` drops the browser branches + args:

```rust
pub(crate) fn force_route_action(
    force_routing_active: bool,
    net_force_routable: bool,
) -> ForceRouteAction {
    if force_routing_active && net_force_routable {
        ForceRouteAction::Sidecar
    } else {
        ForceRouteAction::Direct
    }
}
```

(f) `spawn_worker_maybe_forced` — drop the override lookup + the two browser arms; set `disable_mitm` in the Sidecar arm:

```rust
pub(crate) fn spawn_worker_maybe_forced(
    force: Option<&ForceRoutingConfig>,
    backend: &dyn SandboxBackend,
    spec: &WorkerSpec<'_>,
    worker_name: &str,
) -> Result<SupervisedWorker, ToolHostError> {
    let action = force_route_action(
        force.is_some(),
        policy_net_is_force_routable(&spec.policy.net),
    );
    match action {
        ForceRouteAction::Direct => spawn_worker(backend, spec),
        ForceRouteAction::Sidecar => {
            let cfg = force.expect("Sidecar action implies force-routing is configured");
            let allowlist = match &spec.policy.net {
                Net::Allowlist(hosts) => hosts.clone(),
                _ => return spawn_worker(backend, spec),
            };
            let params = crate::egress::net_worker::NetWorkerSpawn {
                backend,
                proxy_bin: &cfg.proxy_bin,
                spec,
                allowlist: &allowlist,
                worker_name,
                secret_fingerprints: &[],
                cert_pins_json: None,
                // The browser does end-to-end TLS + can't trust our CA → its
                // sidecar transparently tunnels (slice #2).
                disable_mitm: worker_name == BROWSER_DRIVER_TOOL,
            };
            spawn_forced_net_worker(&params, &cfg.scratch_root, (cfg.make_sink)())
        }
    }
}
```

(g) `resolve_force_routing` drops the trailing bool param and the `ForceRoutingConfig::new` call loses its 4th arg:

```rust
pub fn resolve_force_routing(
    enabled: bool,
    proxy_bin: Option<PathBuf>,
    scratch_root: PathBuf,
    make_sink: DecisionSinkFactory,
) -> Result<Option<ForceRoutingConfig>, ProxyBinaryNotFound> {
    if !enabled {
        return Ok(None);
    }
    let proxy_bin = proxy_bin.ok_or(ProxyBinaryNotFound)?;
    Ok(Some(ForceRoutingConfig::new(proxy_bin, scratch_root, make_sink)))
}
```

(h) `from_env`: drop the `browser_insecure_direct_net` read + arg:

```rust
    let make_sink: DecisionSinkFactory = Box::new(move || {
        Box::new(pg_decision_sink(pool.clone(), handle.clone()))
    });
    Ok(resolve_force_routing(true, proxy_bin, scratch_root, make_sink)?.map(Arc::new))
```

Remove `use` of any now-unused import. Update `config_with` in tests to the 3-arg `new`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p kastellan-core --lib worker_lifecycle::force_route`
Expected: PASS. The crate may still not fully build because `tool_host::ToolHostError::ForceRouteUnconfined` is now unused — that's removed in Task 4. If `cargo build -p kastellan-core` errors only on an *unused-variant* warning (not an error), proceed; otherwise do Task 4 before re-running the whole crate.

- [ ] **Step 5: Commit**

```bash
git add core/src/worker_lifecycle/force_route.rs
git commit -m "feat(force-route): remove browser-driver exemption; route it like any net worker

force_route_action collapses to {Sidecar,Direct}; the browser now takes the
generic Sidecar path with disable_mitm=true. Drops DirectInsecureDevExempt /
RefuseProductionUnconfined, the browser_insecure_direct_net config, and the
KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET escape hatch. (#263/#280)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: tool_host.rs — drop the dead `ForceRouteUnconfined`

**Files:**
- Modify: `core/src/tool_host.rs` (the `ForceRouteUnconfined` variant ~62 + its doc + any `POLICY_DENIED` mapping)
- Test: existing tool_host tests (no new test; this is a deletion)

- [ ] **Step 1: Find every reference**

Run: `grep -rn "ForceRouteUnconfined" core/src core/tests`
Expected: only the variant declaration in `tool_host.rs` and possibly a match arm mapping it to an error code (`POLICY_DENIED`). (The force_route.rs use was removed in Task 3.)

- [ ] **Step 2: Remove the variant + its mappings**

Delete the `ForceRouteUnconfined { worker: String }` variant, its doc comment, and any `ForceRouteUnconfined { .. } => ...` match arm (e.g. in a `to_error_code`/`code()` impl). If a match becomes non-exhaustive elsewhere, the compiler will point at it.

- [ ] **Step 3: Build to verify**

Run: `cargo build -p kastellan-core`
Expected: builds clean (no unused-variant warning, no non-exhaustive-match error).

- [ ] **Step 4: Run the crate unit tests**

Run: `cargo test -p kastellan-core --lib`
Expected: PASS (skip-as-pass on the Mac for PG suites).

- [ ] **Step 5: Commit**

```bash
git add core/src/tool_host.rs
git commit -m "refactor(tool_host): drop now-unreachable ForceRouteUnconfined error

The browser-driver production lockout is gone (slice #2 routes it through the
sidecar), so this variant has no producer.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: sandbox Seatbelt — loopback TCP for the browser under proxy_uds

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs` (the `(Net::Allowlist(_), Some(uds))` arm ~389-404)
- Test: `sandbox/src/macos_seatbelt.rs` (or its `tests.rs` sibling — match where the existing profile tests live)

**Why:** the `proxy_uds` arm emits `(deny network-outbound)` + allow-only-the-UDS, which blocks the loopback TCP the shim needs. Allow loopback TCP **only** for `Profile::WorkerBrowserClient` + `proxy_uds`. bwrap needs no change (it brings `lo` up in the private netns).

- [ ] **Step 1: Write the failing tests**

Add to the macOS profile-builder tests (mirror the existing `build_profile`-asserting test style):

```rust
    #[test]
    fn browser_proxy_uds_allows_loopback_tcp() {
        let policy = SandboxPolicy {
            net: crate::Net::Allowlist(vec!["example.com:443".into()]),
            proxy_uds: Some(std::path::PathBuf::from("/tmp/egress.sock")),
            profile: crate::Profile::WorkerBrowserClient,
            ..SandboxPolicy::default()
        };
        let p = build_profile(&policy);
        assert!(p.contains("(deny network-outbound)"), "still deny-by-default");
        assert!(p.contains("unix-socket (path-literal"), "UDS still allowed");
        assert!(p.contains(r#"(allow network-bind (local ip "localhost:*"))"#));
        assert!(p.contains(r#"(allow network-inbound (local ip "localhost:*"))"#));
        assert!(p.contains(r#"(allow network-outbound (remote ip "localhost:*"))"#));
    }

    #[test]
    fn non_browser_proxy_uds_has_no_loopback_tcp() {
        let policy = SandboxPolicy {
            net: crate::Net::Allowlist(vec!["example.com:443".into()]),
            proxy_uds: Some(std::path::PathBuf::from("/tmp/egress.sock")),
            profile: crate::Profile::WorkerNetClient,
            ..SandboxPolicy::default()
        };
        let p = build_profile(&policy);
        assert!(!p.contains(r#"network-bind (local ip "localhost"#),
            "non-browser UDS workers must not be widened with loopback TCP");
        assert!(!p.contains(r#"network-outbound (remote ip "localhost"#));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kastellan-sandbox browser_proxy_uds_allows_loopback_tcp`
Expected: FAIL (rules not emitted).

- [ ] **Step 3: Emit the loopback rules in the proxy_uds arm**

In the `(Net::Allowlist(_), Some(uds))` match arm (after the UDS allow `push_str`), add:

```rust
            // The browser reaches its in-jail loopback-TCP↔UDS shim over
            // 127.0.0.1 (egress slice #2): allow loopback TCP bind/accept (shim)
            // + connect (Chromium). Scoped to the browser profile so the other
            // UDS workers (in-process CONNECT-over-UDS, no loopback) stay strict.
            if matches!(policy.profile, crate::Net::Allowlist(_) /*placeholder*/) {}
            if matches!(policy.profile, crate::Profile::WorkerBrowserClient) {
                out.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
                out.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
                out.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
            }
```

> Delete the bogus placeholder line; it's only there to flag that the gate is on `policy.profile`, not net. The real gate is the `WorkerBrowserClient` `if`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p kastellan-sandbox`
Expected: PASS (new + all existing macOS profile tests).

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "feat(sandbox/seatbelt): allow loopback TCP for browser under proxy_uds

The browser-driver's in-jail loopback-TCP<->UDS shim (egress slice #2) needs
127.0.0.1 bind/accept/connect, which the deny-outbound-except-UDS rule blocks.
Scoped to Profile::WorkerBrowserClient so other UDS workers stay strict. bwrap
unchanged (it brings lo up in the private netns).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Python loopback-TCP↔UDS shim

**Files:**
- Create: `workers/browser-driver/src/kastellan_worker_browser_driver/shim.py`
- Test: `workers/browser-driver/tests/test_shim.py`

**Design:** a `ProxyShim` class that runs an asyncio relay on a **background thread** (the worker itself is synchronous), exposing **sync** `start() -> int` (bound loopback port) and `stop()`. Each accepted TCP connection opens the UDS and splices bytes both ways — a dumb pipe (Chromium's `CONNECT host:port` is exactly the sidecar's UDS protocol).

- [ ] **Step 1: Write the failing test**

```python
# workers/browser-driver/tests/test_shim.py
"""Tests for the loopback-TCP<->UDS relay shim (egress slice #2).

A fake UDS server stands in for the egress sidecar: it accepts a connection and
echoes everything it receives. The test connects to the shim's loopback TCP port
with a blocking socket and asserts bytes round-trip through the UDS.
"""
import socket
import tempfile
import threading
import os

from kastellan_worker_browser_driver.shim import ProxyShim


def _fake_uds_echo_server(uds_path: str, ready: threading.Event) -> threading.Thread:
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(uds_path)
    srv.listen(8)
    ready.set()

    def serve():
        while True:
            try:
                conn, _ = srv.accept()
            except OSError:
                return
            threading.Thread(target=_echo, args=(conn,), daemon=True).start()

    def _echo(conn):
        with conn:
            while True:
                data = conn.recv(4096)
                if not data:
                    return
                conn.sendall(data)

    t = threading.Thread(target=serve, daemon=True)
    t.start()
    return t


def test_shim_relays_bytes_through_uds():
    tmp = tempfile.mkdtemp()
    uds_path = os.path.join(tmp, "egress.sock")
    ready = threading.Event()
    _fake_uds_echo_server(uds_path, ready)
    assert ready.wait(timeout=5)

    shim = ProxyShim(uds_path)
    port = shim.start()
    try:
        assert isinstance(port, int) and port > 0
        c = socket.create_connection(("127.0.0.1", port), timeout=5)
        c.sendall(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
        got = c.recv(4096)
        assert got == b"CONNECT example.com:443 HTTP/1.1\r\n\r\n"
        c.close()
    finally:
        shim.stop()


def test_shim_handles_concurrent_connections():
    tmp = tempfile.mkdtemp()
    uds_path = os.path.join(tmp, "egress.sock")
    ready = threading.Event()
    _fake_uds_echo_server(uds_path, ready)
    assert ready.wait(timeout=5)

    shim = ProxyShim(uds_path)
    port = shim.start()
    try:
        conns = [socket.create_connection(("127.0.0.1", port), timeout=5) for _ in range(5)]
        for i, c in enumerate(conns):
            msg = f"hello-{i}".encode()
            c.sendall(msg)
            assert c.recv(4096) == msg
        for c in conns:
            c.close()
    finally:
        shim.stop()
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd workers/browser-driver && .venv/bin/python -m pytest tests/test_shim.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named '...shim'`.

- [ ] **Step 3: Implement `shim.py`**

```python
# workers/browser-driver/src/kastellan_worker_browser_driver/shim.py
"""A loopback-TCP <-> UDS relay so a headless Chromium can reach the egress
sidecar (egress slice #2).

Chromium speaks HTTP-proxy `CONNECT host:port` over a TCP socket; the egress
sidecar speaks the *same* CONNECT protocol over its Unix-domain socket. So this
shim is a dumb byte-pipe: accept a TCP connection on 127.0.0.1, open the UDS,
and splice bytes both ways. No HTTP parsing.

The browser-driver worker is synchronous (sync Playwright), so the relay runs on
its own background thread with a private asyncio event loop. The public API is
sync: `start()` returns the bound loopback port; `stop()` shuts it down.
"""
import asyncio
import threading
from typing import Optional


class ProxyShim:
    def __init__(self, uds_path: str):
        self._uds_path = uds_path
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._thread: Optional[threading.Thread] = None
        self._server: Optional[asyncio.AbstractServer] = None
        self._port: Optional[int] = None

    def start(self) -> int:
        """Start the relay on a background thread; return the bound TCP port."""
        ready = threading.Event()
        err: list[BaseException] = []

        def run() -> None:
            loop = asyncio.new_event_loop()
            self._loop = loop
            asyncio.set_event_loop(loop)
            try:
                self._server = loop.run_until_complete(
                    asyncio.start_server(self._handle, host="127.0.0.1", port=0)
                )
                self._port = self._server.sockets[0].getsockname()[1]
            except BaseException as e:  # noqa: BLE001 - surface to start()
                err.append(e)
                ready.set()
                return
            ready.set()
            loop.run_forever()
            # Drain on shutdown.
            loop.run_until_complete(self._server.wait_closed())
            loop.close()

        self._thread = threading.Thread(target=run, name="egress-shim", daemon=True)
        self._thread.start()
        if not ready.wait(timeout=10):
            raise RuntimeError("egress shim failed to start within 10s")
        if err:
            raise err[0]
        assert self._port is not None
        return self._port

    def stop(self) -> None:
        """Stop the relay and join its thread (best-effort, idempotent)."""
        loop = self._loop
        server = self._server
        if loop is None:
            return

        def _shutdown() -> None:
            if server is not None:
                server.close()
            loop.stop()

        loop.call_soon_threadsafe(_shutdown)
        if self._thread is not None:
            self._thread.join(timeout=5)

    async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        """One TCP client: open the UDS, splice both directions until either EOF."""
        try:
            uds_reader, uds_writer = await asyncio.open_unix_connection(self._uds_path)
        except OSError:
            writer.close()
            return
        try:
            await asyncio.gather(
                self._pipe(reader, uds_writer),
                self._pipe(uds_reader, writer),
            )
        finally:
            for w in (writer, uds_writer):
                try:
                    w.close()
                except Exception:  # noqa: BLE001
                    pass

    @staticmethod
    async def _pipe(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            while True:
                chunk = await reader.read(65536)
                if not chunk:
                    break
                writer.write(chunk)
                await writer.drain()
        except (ConnectionError, OSError):
            pass
        finally:
            try:
                writer.write_eof()
            except (OSError, RuntimeError):
                pass
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd workers/browser-driver && .venv/bin/python -m pytest tests/test_shim.py -v`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add workers/browser-driver/src/kastellan_worker_browser_driver/shim.py workers/browser-driver/tests/test_shim.py
git commit -m "feat(browser-driver): loopback-TCP<->UDS relay shim (egress slice #2)

ProxyShim: a dumb byte-pipe so headless Chromium can reach the egress sidecar
(Chromium's CONNECT over TCP == the sidecar's CONNECT over UDS). Runs on a
background asyncio thread; sync start()/stop() for the sync worker.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Python worker wiring — launch args + shim lifecycle

**Files:**
- Modify: `workers/browser-driver/src/kastellan_worker_browser_driver/render.py` (add `build_launch_args`; thread into `PlaywrightRenderer`)
- Modify: `workers/browser-driver/src/kastellan_worker_browser_driver/__main__.py` (start shim if UDS set; pass proxy args)
- Test: `workers/browser-driver/tests/test_launch_args.py`

- [ ] **Step 1: Write the failing test**

```python
# workers/browser-driver/tests/test_launch_args.py
"""build_launch_args wires Chromium's --proxy-server only when force-routed."""
from kastellan_worker_browser_driver.render import build_launch_args, DEFAULT_LAUNCH_ARGS


def test_no_proxy_when_port_none():
    args = build_launch_args(None)
    assert args == DEFAULT_LAUNCH_ARGS
    assert not any(a.startswith("--proxy-server") for a in args)


def test_proxy_server_and_bypass_when_port_given():
    args = build_launch_args(54321)
    assert "--proxy-server=127.0.0.1:54321" in args
    # Force loopback destinations through the proxy too (remove implicit bypass).
    assert "--proxy-bypass-list=<-loopback>" in args
    # Base flags preserved.
    for base in DEFAULT_LAUNCH_ARGS:
        assert base in args
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd workers/browser-driver && .venv/bin/python -m pytest tests/test_launch_args.py -v`
Expected: FAIL — `ImportError: cannot import name 'build_launch_args'`.

- [ ] **Step 3: Add `build_launch_args` to `render.py`**

After `DEFAULT_LAUNCH_ARGS` (line ~29), add:

```python
def build_launch_args(proxy_port: Optional[int]) -> list[str]:
    """Chromium launch args. When force-routed (a shim port is given), route all
    traffic through the in-jail proxy at 127.0.0.1:<port> and remove Chromium's
    implicit loopback bypass so even loopback destinations go through the proxy
    (and are allowlist-checked by the sidecar). Without a port: the dev direct
    path, byte-identical to before."""
    args = list(DEFAULT_LAUNCH_ARGS)
    if proxy_port is not None:
        args.append(f"--proxy-server=127.0.0.1:{proxy_port}")
        args.append("--proxy-bypass-list=<-loopback>")
    return args
```

(The `PlaywrightRenderer` already accepts `launch_args`; no change needed there — `__main__` passes the built list.)

- [ ] **Step 4: Run the launch-args test to verify it passes**

Run: `cd workers/browser-driver && .venv/bin/python -m pytest tests/test_launch_args.py -v`
Expected: PASS.

- [ ] **Step 5: Wire the shim + args into `__main__.py`**

Rewrite `__main__.py` to start the shim when force-routed and pass the args:

```python
"""Entry point for `kastellan-worker-browser-driver`.

Reads the operator allowlist from `KASTELLAN_BROWSER_DRIVER_ALLOWLIST`. When the
host force-routes egress (`KASTELLAN_EGRESS_PROXY_UDS` set), starts an in-jail
loopback-TCP<->UDS shim and points Chromium at it via --proxy-server; the
sidecar enforces the allowlist + SSRF at the netns boundary. Without that env
(dev / force-routing off) it runs direct, as before. The renderer also
self-enforces the allowlist per navigation/subresource (defense in depth).
"""
import os
import sys

from .allowlist import HostAllowlist
from .render import PlaywrightRenderer, build_launch_args
from .server import Server
from .shim import ProxyShim

ALLOWLIST_ENV = "KASTELLAN_BROWSER_DRIVER_ALLOWLIST"
PROXY_UDS_ENV = "KASTELLAN_EGRESS_PROXY_UDS"


def _maybe_start_shim() -> tuple[ProxyShim | None, int | None]:
    """Start the egress shim iff force-routed; return (shim, port) or (None, None)."""
    uds = os.environ.get(PROXY_UDS_ENV, "").strip()
    if not uds:
        return (None, None)
    shim = ProxyShim(uds)
    port = shim.start()
    return (shim, port)


def main() -> None:
    allowlist = HostAllowlist.from_env_json(os.environ.get(ALLOWLIST_ENV, ""))
    shim, port = _maybe_start_shim()
    try:
        renderer = PlaywrightRenderer(
            allowlist=allowlist,
            launch_args=build_launch_args(port),
        )
        Server(renderer=renderer).run(sys.stdin, sys.stdout)
    finally:
        if shim is not None:
            shim.stop()


if __name__ == "__main__":
    main()
```

- [ ] **Step 6: Run the full Python suite to verify nothing regressed**

Run: `cd workers/browser-driver && .venv/bin/python -m pytest -v`
Expected: PASS (all existing tests + the new shim + launch-args tests). The existing `test_render_drive.py` builds `PlaywrightRenderer` directly with a fake factory — unaffected.

- [ ] **Step 7: Commit**

```bash
git add workers/browser-driver/src/kastellan_worker_browser_driver/render.py \
        workers/browser-driver/src/kastellan_worker_browser_driver/__main__.py \
        workers/browser-driver/tests/test_launch_args.py
git commit -m "feat(browser-driver): launch Chromium through the egress shim when force-routed

build_launch_args adds --proxy-server=127.0.0.1:<shim-port> + removes the
implicit loopback bypass; __main__ starts/stops the shim when
KASTELLAN_EGRESS_PROXY_UDS is set, runs direct otherwise.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: manifest docs + escape-hatch cleanup

**Files:**
- Modify: `core/src/workers/browser_driver.rs` (doc comments only; `proxy_uds` stays `None`)
- Modify: `scripts/workers/browser-driver/install.sh` (drop the `INSECURE_DIRECT_NET` help text)
- Verify: no remaining `KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET` outside docs

- [ ] **Step 1: Confirm what's left**

Run: `grep -rn "KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET\|INSECURE_DIRECT_NET\|dev-only legacy direct-net\|force-routing is slice #2" core/src scripts workers`
Expected: matches in `core/src/workers/browser_driver.rs` (doc comments at the module header ~7, `browser_driver_entry` doc ~161-164, and the `proxy_uds: None` comment ~250) and `scripts/workers/browser-driver/install.sh`. (The Rust *code* refs were removed in Tasks 3–4.)

- [ ] **Step 2: Update `browser_driver.rs` doc comments**

Edit the module header (lines ~6-10), the `browser_driver_entry` doc (lines ~161-164), and the `proxy_uds: None` inline comment (~250) to describe the slice-#2 posture. Replace the `proxy_uds: None` comment with:

```rust
        proxy_uds: None, // force-routing sets this at spawn (rewrite_worker_policy); same as web-fetch
```

Update the module header line about "legacy direct-net `Net::Allowlist` path (no `proxy_uds`); egress-proxy force-routing is slice #2" to state that the worker is now force-routed in the default deployment (private netns → sidecar, transparent tunnel), running direct only when force-routing is off (dev). Keep the existing manifest test `entry_has_browser_client_policy_and_operator_allowlist` (it asserts `proxy_uds.is_none()` on the *manifest* entry — still true; the rewrite happens at spawn). No functional code change.

- [ ] **Step 3: Update `install.sh`**

Remove the help-text line documenting `KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET` (around line 64). If it documents enabling the worker under force-routing, replace with a note that the worker is now egress-proxy-routed by default (no escape hatch needed).

- [ ] **Step 4: Verify the cleanup**

Run: `grep -rn "INSECURE_DIRECT_NET" core/src scripts workers && echo "FOUND (bad)" || echo "clean"`
Expected: `clean` (only docs/HANDOVER/ROADMAP may still reference it historically — those are updated in Task 10).

Run: `cargo test -p kastellan-core --lib workers::browser_driver`
Expected: PASS (doc-only change; tests unaffected).

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/browser_driver.rs scripts/workers/browser-driver/install.sh
git commit -m "docs(browser-driver): describe slice-#2 force-routed posture; drop escape-hatch help

Manifest proxy_uds stays None (force-routing rewrites it at spawn). Remove the
KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET help text.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: acceptance e2e — force-routed render + off-allowlist fail-closed at the sidecar

**Files:**
- Modify: `core/tests/browser_driver_e2e.rs` (add a force-routed render helper + 2 `#[ignore]` tests)
- Reference harness: `core/tests/egress_force_routing_e2e.rs` (`proxy_binary_or_skip` ~46, `short_scratch_root` ~68, `minted_uds` ~54, `NetWorkerSpawn`/`spawn_forced_net_worker` usage ~151-163)

**Goal:** prove the browser renders an allowlisted loopback page **through the sidecar** under force-routing, and that an off-allowlist navigation fails closed at the sidecar (not only via in-process interception).

- [ ] **Step 1: Add a forced-render helper + the two tests**

Add imports at the top of `browser_driver_e2e.rs`:

```rust
use std::path::{Path, PathBuf};
use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
```

Add helpers mirrored from `egress_force_routing_e2e.rs` (copy `proxy_binary_or_skip`, `short_scratch_root` verbatim — they are small and self-contained), then:

```rust
/// Render `url` through the real jail **force-routed** through an egress-proxy
/// sidecar (the production posture). Mirrors `render_in_jail` but spawns via
/// `spawn_forced_net_worker` with `disable_mitm: true` (the browser tunnels TLS
/// end-to-end). Returns the dispatch result.
async fn render_in_jail_forced(
    pool: &sqlx::PgPool,
    env: &TestEnv,
    proxy_bin: &Path,
    scratch_root: &Path,
    allowlist: &[String],
    url: &str,
) -> Result<serde_json::Value, kastellan_core::tool_host::ToolHostError> {
    let entry = browser_driver_entry(&env.browser, allowlist);
    let backend = backend();
    let program = env.browser.script_path.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let params = NetWorkerSpawn {
        backend: &*backend,
        proxy_bin,
        spec: &spec,
        allowlist,
        worker_name: "browser-driver",
        secret_fingerprints: &[],
        cert_pins_json: None,
        disable_mitm: true, // browser does end-to-end TLS; sidecar transparently tunnels
    };
    let mut sworker = spawn_forced_net_worker(&params, scratch_root, |_row| {})
        .expect("force-route browser-driver under sidecar");
    let result = dispatch(
        pool,
        &Vault::new(),
        &mut sworker,
        "browser-driver",
        "browser.render",
        serde_json::json!({ "url": url, "wait_until": "load", "timeout_ms": 10000 }),
    )
    .await;
    let _ = sworker.close();
    result
}

/// Acceptance (#280/#263): the browser renders an allowlisted loopback page
/// **through the egress sidecar** under force-routing — egress enforced at the
/// netns boundary, not in-process. Needs a staged Chromium + the egress-proxy
/// binary → `#[ignore]`. Cross-platform (Seatbelt + bwrap).
#[test]
#[ignore = "requires staged Chromium + egress-proxy binary"]
fn forced_render_of_loopback_page_through_sidecar() {
    let env = match ready_or_skip() { Some(e) => e, None => return };
    let Some(proxy) = proxy_binary_or_skip() else { return };
    let scratch_root = short_scratch_root(&format!("bd-fr-{}", unique_suffix()));
    let authority = spawn_loopback_page();
    let url = format!("http://{authority}/");
    let allowlist = vec![authority.clone()];
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let r = render_in_jail_forced(&pool, &env, &proxy, &scratch_root, &allowlist, &url)
            .await
            .expect("forced browser.render round trip");
        assert_eq!(r["status"], 200, "render result: {r}");
        let text = r["text"].as_str().unwrap_or("");
        assert!(text.contains("js-ran"), "post-JS marker missing: {r}");
        pool.close().await;
    });
    let _ = std::fs::remove_dir_all(&scratch_root);
}

/// Fail-closed at the sidecar: under force-routing, a navigation host not on the
/// allowlist is rejected at the egress boundary (CONNECT 403), so the render
/// fails — proving the OS boundary, not just in-process interception, blocks it.
#[test]
#[ignore = "requires staged Chromium + egress-proxy binary"]
fn forced_off_allowlist_fails_closed_at_sidecar() {
    let env = match ready_or_skip() { Some(e) => e, None => return };
    let Some(proxy) = proxy_binary_or_skip() else { return };
    let scratch_root = short_scratch_root(&format!("bd-fr-deny-{}", unique_suffix()));
    let authority = spawn_loopback_page();
    let url = format!("http://{authority}/");
    let allowlist = vec!["someother.test:443".to_string()]; // navigation host NOT allowed
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let r = render_in_jail_forced(&pool, &env, &proxy, &scratch_root, &allowlist, &url).await;
        assert!(r.is_err(), "off-allowlist nav must fail closed at the sidecar, got: {r:?}");
        pool.close().await;
    });
    let _ = std::fs::remove_dir_all(&scratch_root);
}
```

- [ ] **Step 2: Compile the e2e (it's `#[ignore]`, so it won't run without staging)**

Run: `cargo test -p kastellan-core --test browser_driver_e2e -- --list`
Expected: the four tests are listed (2 old + 2 new); compiles clean.

- [ ] **Step 3: Run the acceptance e2e on macOS (Seatbelt)**

Prereq: `scripts/workers/browser-driver/install.sh` has staged the venv + Chromium; the egress-proxy binary is built (`cargo build -p kastellan-worker-egress-proxy`); a local PG is available (set the session `KASTELLAN_PG_BIN_DIR` per the Postgres.app memory note).

Run:
```bash
KASTELLAN_BROWSER_DRIVER_ENABLE=1 \
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
cargo test -p kastellan-core --test browser_driver_e2e -- --ignored --nocapture \
  forced_render_of_loopback_page_through_sidecar forced_off_allowlist_fails_closed_at_sidecar
```
Expected: both PASS (render returns `js-ran`; off-allowlist errors). A `[SKIP]`/early-return means a prereq is missing — stage it and re-run; do **not** treat a skip as a pass for the acceptance gate.

- [ ] **Step 4: Run the acceptance e2e on the DGX (bwrap) — the real Linux gate**

This validates that bwrap brings `lo` up in the private netns (the loopback feasibility assumption).

Run:
```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  KASTELLAN_BROWSER_DRIVER_ENABLE=1 \
  cargo test -p kastellan-core --test browser_driver_e2e -- --ignored --nocapture \
    forced_render_of_loopback_page_through_sidecar forced_off_allowlist_fails_closed_at_sidecar'
```
Expected: both PASS. If the forced render fails with a connection/loopback error, the loopback-in-netns assumption is wrong — see "Risk" below.

- [ ] **Step 5: Commit**

```bash
git add core/tests/browser_driver_e2e.rs
git commit -m "test(browser-driver): force-routed render + off-allowlist fail-closed at sidecar (#280)

Acceptance e2e: the browser renders an allowlisted loopback page through the
egress sidecar (egress at the netns boundary), and an off-allowlist navigation
fails closed at the sidecar. Green on macOS Seatbelt + DGX bwrap.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

**Risk / fallback (loopback in netns):** if Step 4 shows Chromium can't reach `127.0.0.1:<shim>` in the bwrap private netns, bwrap is not bringing `lo` up. Fix options, in order: (a) confirm bwrap version brings up loopback (`bwrap --unshare-net ip link` should show `lo` UP); (b) if not, add a Linux-side step that brings `lo` up inside the netns — `LinuxBwrap` would need a tiny init that runs `ip link set lo up`, or use bwrap's loopback handling. Surface this to the user before implementing a sandbox change — it's a design delta.

---

## Task 10: docs + close issues

**Files:**
- Modify: `docs/devel/ROADMAP.md` (tick the browser-driver egress slice #2 / #263 item)
- Modify: `docs/devel/handovers/HANDOVER.md` (new "This session" block; prune; update test baselines)
- GitHub: close #280 and #263 (via the merge PR body, or `gh issue close`)

- [ ] **Step 1: Update ROADMAP.md** — mark browser-driver as egress-proxy-routed (slice #2 done, #263/#280 closed); note the transparent-tunnel decision and that MITM-of-browser is a deferred follow-up (NSS import).

- [ ] **Step 2: Update HANDOVER.md** — add a concise "This session" block (transparent-tunnel egress routing: shim + no-MITM sidecar + force-route exemption removed + Seatbelt loopback + e2e green both platforms). Update the worker tree note for `browser-driver` (now force-routed, `proxy_uds` set at spawn, `shim.py` added). Prune the prior session blocks per the handover convention (keep under 500 lines where feasible). Record the deferred MITM-of-browser follow-up.

- [ ] **Step 3: Full workspace verification before the PR**

macOS:
```bash
source "$HOME/.cargo/env"
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # skip-as-pass posture (no KASTELLAN_PG_BIN_DIR)
cd workers/browser-driver && .venv/bin/python -m pytest && cd -
```
DGX (native Linux gate):
```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings'
```
Expected: all green; clippy clean both platforms.

- [ ] **Step 4: Commit docs**

```bash
git add docs/devel/ROADMAP.md docs/devel/handovers/HANDOVER.md
git commit -m "docs: browser-driver egress slice #2 done — close #263/#280

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Open the PR**

```bash
git push -u origin feat/browser-driver-egress-slice2
gh pr create --base main --title "browser-driver egress slice #2: egress-proxy routable (transparent tunnel)" \
  --body "$(cat <<'EOF'
Makes browser-driver run in the default force-routed deployment with egress
enforced at the netns boundary. Transparent tunnel (no MITM of the browser):
in-jail loopback-TCP<->UDS shim + a no-MITM sidecar mode. Removes the dev-only
escape hatch + production lockout.

Closes #280. Closes #263.

Spec: docs/superpowers/specs/2026-06-14-browser-driver-egress-slice2-design.md

## Verification
- macOS Seatbelt + DGX bwrap: forced render through the sidecar + off-allowlist
  fail-closed at the sidecar (browser_driver_e2e --ignored), both green.
- Workspace cargo test + clippy -D warnings clean (Mac + DGX); browser-driver pytest green.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-review notes (for the implementer)

- **Spec coverage:** §5.1→Task 1, §5.2→Task 2, §5.3→Task 3, §5.4→Task 4, §5.5→Task 5, §5.6 shim→Task 6 + wiring→Task 7, §5.7→Task 8, §5.8 cleanup→Task 8, §7 acceptance→Task 9, docs→Task 10. All covered.
- **Type consistency:** the new symbols — `MitmCtx.disable_mitm` (Task 1), `proxy_policy(.., disable_mitm)` / `spawn_sidecar(.., disable_mitm)` / `NetWorkerSpawn.disable_mitm` (Task 2), `force_route_action(active, routable)` + `disable_mitm: worker_name == BROWSER_DRIVER_TOOL` (Task 3), `build_launch_args(Optional[int])` + `ProxyShim.start()/stop()` (Tasks 6-7) — are used consistently across tasks.
- **Known mechanical fan-out:** adding the `disable_mitm` param/field forces edits to every existing `proxy_policy`/`spawn_sidecar`/`NetWorkerSpawn`/`MitmCtx`/`ForceRoutingConfig::new`/`resolve_force_routing` call site (tests in `spawn.rs`, `net_worker.rs`, `force_route.rs`, `egress_force_routing_e2e.rs`, `egress_proxy_e2e.rs`). The compiler enumerates them; fix each by adding the trailing `false`/`disable_mitm: false` (or `true` for the browser e2e).
- **Open risk:** the bwrap loopback-in-netns assumption (Task 9 Step 4 / Risk note) is the one thing not provable until the DGX run. Escalate before adding any sandbox change.
