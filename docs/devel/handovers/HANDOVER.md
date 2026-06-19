# kastellan — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention. Older sessions are compressed
> into "Earlier history" below; full per-session detail lives in the
> [`archive/`](archive/) snapshots.

**Last updated:** 2026-06-19 (**Matrix Phase D — live `LiveSdk` integration — DONE on branch
`feat/matrix-phase-d-live-sdk`** (the next slice after the spike #311, now merged to `main`). Implements the real
matrix-rust-sdk path behind the `live-matrix` feature; default build byte-identical (feature off → no SDK compiled).
**What shipped:** (1) `workers/matrix/src/sdk_live.rs` — `LiveSdk` impl of the `MatrixSdk` seam: owns a multi-thread tokio
`Runtime` + `block_on`s the SDK behind the sync `identity`/`poll`/`send`; **restore-or-password-login** persisting
`<store>/session.json` (stable device id across restarts → E2E intact); builds the client through `ProxyBridge` (`.proxy()`)
when `KASTELLAN_EGRESS_PROXY_UDS` is set; an `add_event_handler` decrypts room-text events (skips our own echoes) into a
bounded `VecDeque` (`push_bounded`, cap 256) that `poll` drains with a long-poll wait; one initial `sync_once` then a
continuous background `sync` task; pure `parse_config`/`drain` helpers unit-tested. (2) Worker `main.rs` restored to live
serving — `LiveSdk::connect` (network init: login + first sync, through the bridge) **then** `rlimit::apply_from_env` +
`prelude::lock_down` **then** the raw `kastellan_protocol::server::serve_stdio` (network-init-then-lockdown order; the
sync task keeps running under `net_client`); crate `#![allow(dead_code)]` narrowed to
`#![cfg_attr(not(feature = "live-matrix"), allow(dead_code))]` and the redundant `#[allow(dead_code)]` on `bridge.rs`
removed (LiveSdk consumes `ProxyBridge`). (3) Core `disable_mitm_for(worker_name)` pure predicate (browser-driver + the new
`MATRIX_TOOL = "matrix"`) in `worker_lifecycle/force_route.rs` replaces the inline `== BROWSER_DRIVER_TOOL`, so the matrix
worker's future egress-coupled spawn (plan Task 5) inherits the transparent-tunnel decision. (4)
`core/tests/matrix_live_e2e.rs` — `#[ignore]` two-worker (bot + peer) live send/recv round-trip: reuses the worker binary as
the test's second Matrix client (no `matrix-sdk` dev-dep in core), gated on `KASTELLAN_MATRIX_LIVE_E2E` + skip-as-pass.
**Worker env contract (worker-side; the channel-worker production spawn / Task 5 will set these):**
`KASTELLAN_MATRIX_HOMESERVER_URL`, `_USER`, `_PASSWORD`, `_STORE` (required), `_DEVICE_NAME` (opt), `KASTELLAN_EGRESS_PROXY_UDS`
(opt). **Verification — macOS hermetic:** matrix worker **13/0/0** (`live-matrix`, +4 `sdk_live` tests) / **7/0/0** (default);
`force_route` **25/0** (+1 `disable_mitm_only_for_transparent_tunnel_workers`); `matrix_live_e2e` compiles + skip-as-passes;
`cargo clippy --workspace --all-targets -D warnings` clean; `cargo clippy -p kastellan-worker-matrix --features live-matrix
--all-targets -D warnings` clean. **DGX live verification — DONE (2026-06-19):** `--features live-matrix` **builds on aarch64
Linux** (the cross-platform gate — matrix-sdk's first aarch64 compile), hermetic matrix tests **13/0/0** on the DGX, and the
**live encrypted send/recv round-trip passes** (`matrix_live_e2e` 1/0/0) against a real homeserver — a throwaway loopback
`matrix-conduit` container (conduwuit's upstream; standard CS-API + E2E relay, all the worker exercises), two registered
accounts in a shared **encrypted** room, driven headlessly via the conduit API. **A shutdown-abort defect was found + fixed
here:** matrix-sdk's SQLite stores use `deadpool`, whose connection `Drop` calls tokio `spawn_blocking` — which SIGABRTs
unless a runtime context is active; `LiveSdk` dropped the client on the non-runtime main thread, so every worker shutdown
aborted (the e2e *passed* but the worker processes aborted in cleanup). Fixed by holding `client: Option<Client>` + a `Drop`
that drops it inside `runtime.block_on` (re-verified: 0 panics/aborts). Default-feature Linux baseline (1839/0/15) carried
forward (matrix live path is `live-matrix`-cfg-gated; the `force_route` change is a platform-agnostic pure-fn refactor,
clippy+unit covered). **Remaining Phase D:**
[#312](https://github.com/hherb/kastellan/issues/312) `ProxyBridge` error-surfacing; the full channel-worker egress-coupled
production spawn (Task 5) + daemon `ChannelBus` wiring + `DbPeerAuthorizer`/`DbPairingService` swap. Spec for the SDK API
names: `docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md#exact-sdk-builder-and-trigger-method-names`.)

_(Prior session — **Matrix Phase D egress-transport spike — DONE, merged to `main` as `0a7df92` (PR
[#311](https://github.com/hherb/kastellan/pull/311)).** matrix-sdk 0.8.0 landed behind `live-matrix` feature; AGPL license pass (225 new crates, all PASS);
`ProxyBridge` (loopback-TCP↔UDS relay, `workers/matrix/src/bridge.rs`); hermetic spike test (`egress_spike.rs`) confirms
`matrix_sdk_routes_first_request_through_the_bridge` — CONNECT reaches the stub UDS via the bridge. Transport decision CONFIRMED:
transparent tunnel via `disable_mitm` (worker name) + `ProxyBridge`; no CA injection. SDK builder names (homeserver_url, sqlite_store,
proxy, build, whoami) recorded in the spec — consumed by this session's `LiveSdk`. Default build unaffected.
Spec: `docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md`.)

_(Prior session — **python-exec >64 KiB scratch-file param channel — DONE on branch
`feat/python-exec-scratch-file-params`, PR [#310](https://github.com/hherb/kastellan/pull/310), MERGED to `main` as `83bf95e`.** Runtime params >64 KiB were
previously refused outright (the 64 KiB cap exists because the worker hands params to the child CPython as an `execve` env
var); now they ride a file. The worker decides by serialized size: **≤64 KiB → inline env `KASTELLAN_PYTHON_PARAMS`
(byte-identical, unchanged); >64 KiB → write `<scratch>/params.json` (0600, in the worker's per-spawn writable scratch) +
set `KASTELLAN_PYTHON_PARAMS_FILE` to the in-jail path + default the inline env to `"{}"`; over the ceiling → fail-closed.**
The ceiling is operator-configurable via `KASTELLAN_PYTHON_PARAMS_FILE_MAX` (default 1 MiB, clamp `[64 KiB, 16 MiB]`),
enforced authoritatively **worker-side** (`workers/python-exec/src/exec/mod.rs`: pure `params_file_max` +
`decide_param_channel` + `params_env_pairs` + I/O `write_params_file`; `serialize_params` no longer caps). The **host** gate
keeps a fixed 16 MiB structural backstop (`l3py_invoke/pure.rs::HOST_PARAMS_HARD_MAX`; `validate_python_params` now takes
`max_bytes`) so the two pure host callers (`agent.rs`/`operator.rs`) stay env-free. The manifest
(`core/src/workers/python_exec.rs`) forwards the operator knob into the jail **only when set** (unset → byte-identical env;
`python_exec_entry` gained a 4th `Option<String>` arg). Transport chosen: worker-writes-to-scratch (params already arrive
over unbounded JSON-RPC stdio; no host RO-bind/new RAII guard). Secret substitution stays host-side in `dispatch` before the
worker, so the file holds the same materialized params the env var would — **the output secret-scrub is unaffected**;
python-exec is SingleUse so the scratch (and the file) is RAII-cleaned after the call. Agent idiom ("file-only-when-large",
documented on the `PARAMS_FILE_ENV` doc-comment): read `KASTELLAN_PYTHON_PARAMS_FILE` if set, else
`json.loads(os.environ.get("KASTELLAN_PYTHON_PARAMS", "{}"))`. **Verification — macOS (Seatbelt + PG 18) AND DGX native
aarch64 (real bwrap + live PG):** worker unit 45/0, core lib green (mac 979/0/1, DGX 968/0/1 — cfg-split), `cargo clippy
--workspace --all-targets -D warnings` clean on both, `python_exec_e2e` **5/5** (incl. live 100 KiB file-channel round-trip
through the real jail), `cli_memory_l3py_run_daemon_e2e` **5/5** (Scenario 5 reframed to prove daemon-path file-channel
delivery — over-ceiling REFUSAL is unreachable via the CLI argv channel, 128 KiB `MAX_ARG_STRLEN` on Linux, so it stays
worker/host unit-covered). Also FIXED a pre-existing Linux-latent test (`python_exec_child_env_is_clobber_proof` never
accounted for CPython PEP 538 `LC_CTYPE` coercion; fails identically on base, surfaced now that the daemon e2e runs on the
DGX). exec.rs split to `exec/mod.rs` (350) + `exec/tests.rs` (238) under the 500-LOC cap. Final whole-branch review (opus):
ready-to-merge, 0 Critical/0 Important. Spec/plan: `docs/superpowers/{specs,plans}/2026-06-18-python-exec-scratch-file-param-channel*`.)

_(Prior session — **browser-driver adopts per-spawn `ephemeral_scratch` — #283 FULLY CLOSED, PR
[#308](https://github.com/hherb/kastellan/pull/308) merged to `main` as `ae0127a`.** `browser_driver_entry` sets
`ephemeral_scratch: true` + `fs_write` empty on **both** OSes (was macOS `["/tmp"]`); each browser spawn gets a unique
per-spawn writable dir (macOS host-created `KASTELLAN_WORKER_SCRATCH` via `prepare_ephemeral_scratch`, Seatbelt-granted,
RAII-cleaned; Linux bwrap `/tmp` tmpfs — flag a no-op). Worker `_apply_worker_scratch` redirects `TMPDIR`/`HOME` to the
scratch when set, else the seeded `/tmp` stands (Linux byte-identical). Verified macOS `browser_driver_e2e --ignored` 4/4 +
**DGX 4/4** (real bwrap+Landlock+seccomp+PG). The shared `pyexec-` scratch prefix is the generic per-spawn mechanism this
session's param channel reuses.)

_(Prior session — **python-exec per-spawn writable scratch on macOS — DONE on branch
`feat/python-exec-macos-perspawn-scratch`, PR [#307](https://github.com/hherb/kastellan/pull/307), MERGED to `main` as `a746bc5`.** Closes the macOS-writable-scratch follow-up (Phase 4,
[#283](https://github.com/hherb/kastellan/issues/283) for python-exec). python-exec had a cross-platform parity gap:
on Linux it gets a per-spawn ephemeral `/tmp` tmpfs (bwrap `--tmpfs`, #89), but on macOS Seatbelt has no tmpfs and the
manifest's `fs_write=[]` left agent Python with **no writable scratch at all**. Fixed with a reusable mechanism, NOT a
python-exec-only hack: new additive `ToolEntry.ephemeral_scratch: bool` (python-exec sets it `true`, all 16 other literals
`false`) drives `core/src/tool_host/scratch.rs::prepare_ephemeral_scratch`, which on macOS host-creates
`<temp_dir>/pyexec-<pid>-<seq>`, grants it via `fs_write` (→ Seatbelt subpath rule), hands the path to the worker through
`KASTELLAN_WORKER_SCRATCH`, and RAII-cleans it (`EphemeralScratch` held in a new `SupervisedWorker.scratch`, attached via
`with_scratch` **post-spawn** at both cold-spawn sites [`manager.rs` SingleUse + `idle_timeout.rs` cold path] AND the e2e
harness — mirrors how egress attaches its sidecar, so `WorkerSpec`/`spawn_worker` stay untouched). The worker
(`workers/python-exec/src/exec.rs`) resolves `TMPDIR`/`HOME`/cwd from `KASTELLAN_WORKER_SCRATCH` (fallback `/tmp`).
**Linux byte-identical** (`prepare_ephemeral_scratch` returns `None` off macOS; env unset → `/tmp`). Seatbelt grants only
the spawn's own subpath, so invocations can't read each other's scratch — strictly stronger than browser-driver's shared
`/tmp`. Verification (Mac, PG 18 + real Seatbelt jail): `python_exec_e2e` 4/4 with
`scratch_tmp_write_round_trip_inside_jail` now **running+passing on macOS** (was a macOS `[SKIP]`; one fewer `[SKIP]`,
same pass count) + host-side `no leaked scratch dirs`; `tool_host` 40/0, `worker_lifecycle` 68/0, worker unit incl. 3 new
scratch tests; `cargo clippy --workspace --all-targets -D warnings` clean. **DGX not re-run** — change is macOS-`cfg`-gated
and the Linux path is byte-identical; the 1839/0/15 Linux baseline carries forward. Follow-ups: browser-driver adopting
the flag + dropping its `fs_write=["/tmp"]` (closes #283 fully); the >64 KiB scratch-file param channel (now unblocked).
Spec/plan: `docs/superpowers/{specs,plans}/2026-06-18-python-exec-macos-perspawn-scratch*`. **Post-review hardening (same PR):**
the host dir is now created with exclusive `std::fs::create_dir` (was `create_dir_all`) so a name collision with a
crash-leaked dir aborts the spawn fail-closed instead of reusing stale contents; `SupervisedWorker::close()` drops its
guards (watchdog→egress→scratch) explicitly to match the implicit `Drop` order; the `no leaked scratch dirs` check is
now an in-band assertion in the `python_exec_e2e` harness (was manual); and the `ephemeral_scratch` doc records that
per-spawn isolation holds for `SingleUse` workers only. Re-verified: `python_exec_e2e` 4/4 under the real jail,
scratch units 12/0, `clippy -D warnings` clean.)_

_(Prior session — **`cli_memory_l3py_run_daemon_e2e` test-lift** merged to `main` as `625e9d6` (PR
[#306](https://github.com/hherb/kastellan/pull/306)): hoisted shared daemon bring-up + inert mock LLM + CLI-output asserts
+ `cli_command` builder into `tests-common` (`daemon.rs` + `binaries.rs`), consumed by both daemon e2e files (l3py
838→499, l3 480→296); python-specific `find_python`/skill factories stay local (core-free). Earlier on `main`: **egress
slice-#4 operator cert-pin plumbing** (`4ecb94a`, PR #303; deferred e2e [#304](https://github.com/hherb/kastellan/issues/304));
**python-exec output secret-scrub** in-process e2e (PR #299) + scrub (`ddd2cf0`, PR #297); **[#268] egress #3b dispatch-time
secret-hash provisioning** (PR #296).)_

---

**Recently merged to `main` (condensed, newest first).** Full reasoning in the PRs / `docs/superpowers/specs` / archive snapshots:
- **Matrix Phase D egress-transport spike** (PR [#311](https://github.com/hherb/kastellan/pull/311), `0a7df92`): matrix-sdk 0.8.0 landed behind `live-matrix`; AGPL license pass (225 crates PASS); `ProxyBridge` loopback-TCP↔UDS relay; hermetic spike confirms `CONNECT homeserver:443` routes through the bridge. Transport locked = transparent tunnel + `disable_mitm`, no CA injection. The live `LiveSdk` integration built on top is this session (header up top).
- **python-exec >64 KiB scratch-file param channel** (PR [#310](https://github.com/hherb/kastellan/pull/310), `83bf95e`): runtime params >64 KiB now ride a file (`<scratch>/params.json`, 0600) instead of being refused; ≤64 KiB stays inline-env (byte-identical). Operator-configurable ceiling `KASTELLAN_PYTHON_PARAMS_FILE_MAX` (default 1 MiB); host gate keeps a fixed 16 MiB backstop. Verified macOS (Seatbelt+PG18) and DGX aarch64 (bwrap+PG): `python_exec_e2e` 5/5, `cli_memory_l3py_run_daemon_e2e` 5/5, clippy clean. See prior-session block up top.
- **python-exec per-spawn writable scratch on macOS** (PR [#307](https://github.com/hherb/kastellan/pull/307), `a746bc5`): the reusable per-spawn scratch mechanism this session's browser-driver work builds on — additive `ToolEntry.ephemeral_scratch: bool` → `core/src/tool_host/scratch.rs::prepare_ephemeral_scratch` (macOS host-creates `<temp_dir>/pyexec-<pid>-<seq>`, grants via `fs_write`, injects `KASTELLAN_WORKER_SCRATCH`, RAII-cleaned in `SupervisedWorker.scratch` via `with_scratch` post-spawn; Linux no-op). python-exec set the flag; Linux byte-identical. See the prior-session block up top.
- **python-exec output secret-scrub** (PR [#297](https://github.com/hherb/kastellan/pull/297), `ddd2cf0` + overlap-pin `d9570ee`): scans a python-exec result for the fingerprints of the secrets materialized into **this** dispatch and redacts them before the result is screened/audited/returned (python-exec runs agent-authored code + is `Net::Deny`, so its output is its only channel — the analog of egress #3b). New pure `kastellan_leak_scan::redact` (bounded-buffer, all-hits, marker `[redacted:<8hex>]`; shared `pow_base`/`sha256_hex` extracted into `fingerprint.rs`) + `core/src/tool_host/secret_scrub.rs` (`worker_redacts_output` python-exec-only gate, `fingerprints_for_dispatch` via `Vault::value_fingerprint` [no plaintext copy], `scrub_result_value` over every JSON string leaf, redacted `secret.output_scrubbed` audit row — hash/offset/len only), wired into `dispatch_with_sink`'s `Ok` arm **before** the injection screen using the pre-substitution `req_for_audit` snapshot. No-op (byte-identical) for every other worker. Accepted limits: secrets `<8` bytes unscannable (same as #3b); a vanishingly-narrow TTL-expiry race; a partial-suffix overlap edge (pinned). **In-process scrub e2e added this session** (see top block; full daemon e2e → [#298](https://github.com/hherb/kastellan/issues/298)).
- **[#268] egress #3b dispatch-time secret-hash provisioning** (PR [#296](https://github.com/hherb/kastellan/pull/296), `1da9882`): `tool_host::dispatch` writes each materialized secret's value-fingerprint into a force-routed net worker's egress-sidecar `secret_hashes.json` **before** `worker.call` (re-scans the pre-substitution `req_for_audit` via `collect_refs_in_params` + `Vault::value_fingerprint`; `egress::leak_provision::merge_secret_hashes` union accumulator + `tool_host/egress_provision` `compute_provision`/`emit_provision`). D1 fail-closed / D2 union across reused workers / D3 audit-newly-added (`ref_hash`-keyed). No-op for all current workers (`egress==None`; byte-identical `shell_exec_e2e`); activates with the first secret-bearing egress worker. PR #296 review pass unified `collect_refs_in_params` + substitution onto one `for_each_ref` traversal (parity-tested) + extracted pure `select_provisioned_rows`.
- **[#281] gliner-relex Landlock — #281 FULLY CLOSED** (PR [#295](https://github.com/hherb/kastellan/pull/295), `4b42848`): flipped Landlock **on** for the torch worker — `host_mode_entry` no longer emits `KASTELLAN_LANDLOCK_PROFILE=none`, so the lockdown-exec shim installs the ruleset alongside the `ml_client` seccomp filter (RO from `fs_read`, RW=`["/tmp"]` for torch's inductor cache, `fs_write` empty). No `fs_read` iteration needed (RO set = `DEFAULT_RO_EXEC_ROOTS ∪ fs_read` = what bwrap binds). DGX: 3 host-mode `gliner_relex_e2e` real-model suites green under Landlock + shim probe `FullyEnforced` (a world-readable out-of-RO file denied = real containment, not DAC); workspace 1839/0/15. Both pure-Python workers now have seccomp + Landlock.
- **[#281] browser-driver Landlock** (PR [#294](https://github.com/hherb/kastellan/pull/294), `545975e`): flipped Landlock **on** for browser-driver — `browser_driver_entry` no longer emits `KASTELLAN_LANDLOCK_PROFILE=none`, so the lockdown-exec shim installs the ruleset (RO from `fs_read` — venv, interpreter libs, `/etc` resolver files, the shim, per-instance CA when force-routed; RW = `/tmp` for Chromium's `--user-data-dir`, `fs_write` empty). No `fs_read` iteration needed (RO set = `DEFAULT_RO_EXEC_ROOTS ∪ fs_read` = what bwrap binds). Proxy UDS connect is not gated by Landlock `AccessFs` (path-based AF_UNIX connect is unmediated). DGX: all 4 `browser_driver_e2e --ignored` green + shim probe `FullyEnforced`; workspace 1839/0/15. The method gliner-relex Landlock (above) reused verbatim.
- **[#281] gliner-relex Linux seccomp via `ml_client` + the lockdown-exec shim** (PR [#293](https://github.com/hherb/kastellan/pull/293), HEAD `0b38f4f`): the heavy torch worker's host-mode spawn now routes through `kastellan-worker-lockdown-exec` so a real seccomp filter applies on Linux (was unfiltered — bwrap spawns the venv directly). New sandbox `Profile::WorkerMlClient` (strict off Linux) + prelude `ml_client` profile = `net_client` + `{mbind, get_mempolicy, mlock, munlock, mknodat}` (DGX-enumerated via the kill-mode/`journalctl -k` loop). Fail-closed shim discovery; seccomp-only (`LANDLOCK_PROFILE=none`). All 3 real-model e2e suites pass under the kill-mode filter on the DGX; workspace 1839/0/15. See top block.
- **[#281] pure-Python Linux seccomp via `kastellan-worker-lockdown-exec`** (PR [#292](https://github.com/hherb/kastellan/pull/292), `80de534`): browser-driver now spawns through a prelude exec-shim that applies `lock_down()` then `execve`s the venv script (inherits the `browser_client` seccomp filter under `NO_NEW_PRIVS`); `ToolEntry.lockdown_shim` + pure `build_program_and_args` + `KASTELLAN_LANDLOCK_PROFILE=none` (seccomp-only; Landlock deferred). Fail-closed on Linux. DGX `browser_driver_e2e` 4/4 + `lockdown_exec_smoke`; `capget`/`capset` added to `browser_client` (empirically required by Playwright-Node / Chromium-zygote). The shim + `build_program_and_args` infra the gliner-relex half (above) reuses.
- **#287 — macOS forced-egress "no decisions" was a STALE venv** (PR [#290](https://github.com/hherb/kastellan/pull/290), `5c228be`): not a code bug — a pre-slice-#2 browser-driver venv (no `shim.py`, no `--proxy-server`) let Chromium connect directly on macOS's shared loopback. Fix: `scripts/workers/browser-driver/install.sh` now `pip install --force-reinstall --no-deps` the local package + asserts `shim.py` is present (staleness tripwire). All 4 `browser_driver_e2e --ignored` pass on macOS after re-staging. macOS-only; no Rust changed.
- **`interpreter_deps` adopted in `python-exec` + `gliner-relex`** (PR [#289](https://github.com/hherb/kastellan/pull/289), `2d85ea1`): the #284 follow-up — the same out-of-prefix interpreter-dyld auto-bind now routed through one shared `core/src/workers/interpreter_deps.rs` (pure `resolve_interpreter_root` + `interpreter_lib_dirs_for_binary` helpers); `python-exec` (bare interpreter) + `gliner-relex` (uv venv host mode) both bind their interpreter's out-of-prefix lib dirs. Reads-only, fail-safe (missing `otool`/`ldd` ⇒ no extra binds), no-op where all deps are system libs. macOS core lib suite + clippy `-D warnings` green; path is a no-op on Linux (DGX `cargo test` not re-run pre-merge, negligible risk).
- **#284 interpreter-lib-dep auto-bind (a MISDIAGNOSIS fix)** (PR [#288](https://github.com/hherb/kastellan/pull/288), `a7338c3`): the "Chromium-148 Seatbelt SIGABRT" was a pyenv CPython linking a Homebrew `libintl` OUTSIDE its bound prefix → dyld `open()` blocked → SIGABRT before Chromium launches (empty stderr). New pure `core/src/workers/interpreter_deps.rs` (`out_of_prefix_lib_dirs` transitive dep-graph walk seeded with the binary+`libpython`, binds the canonical parent dir of every out-of-prefix non-system lib RO; `resolve_deps_via_tool` = `otool`/`ldd`, fail-safe). Wired into `browser-driver` + its e2e; `real_render_of_loopback_page` renders under Seatbelt with NO manual `EXTRA_FS_READ`. Unmasked [#287](https://github.com/hherb/kastellan/issues/287). Reads-only, DGX 1790/0 unchanged. (The cross-worker adoption into `python-exec` + `gliner-relex` is this session — top block.)
- **`browser-driver` egress slice #2 — egress-proxy-routed (transparent tunnel)** (PR [#285](https://github.com/hherb/kastellan/pull/285), `76c58d9`): the browser runs in a private netns reaching the net only via its per-worker egress sidecar in **no-MITM/transparent-tunnel** mode (browser keeps end-to-end TLS; in-jail `shim.py` `ProxyShim` loopback-TCP↔UDS bridge + Chromium `--proxy-server`). Removed the dev-only force-route exemption + `KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET` escape hatch. DGX acceptance 2/2 green; #263 + #280 closed. macOS forced-egress now tracked by [#287](https://github.com/hherb/kastellan/issues/287).
- **python-exec skill-catalog arc** (PRs [#275](https://github.com/hherb/kastellan/pull/275)/[#276](https://github.com/hherb/kastellan/pull/276)/[#278](https://github.com/hherb/kastellan/pull/278), `0cbddc5`/`e478309`/`02ccb57`): a "Python skill" = agent-authored verbatim Python promoted through the *same* L3 trust lifecycle as templated skills (SHA-256-bound, operator reads the source = the gate). crystallise/approve/pin (slice 1) + invoke/surface (slice 2) + runtime params (env-var channel). `core/src/memory/l3py_*`. Full detail in the PRs / archive.
- **`browser-driver` Phase 2 + slice #1** (PRs [#282](https://github.com/hherb/kastellan/pull/282) `9f2e955`, [#262](https://github.com/hherb/kastellan/pull/262)): headless Chromium renders under the real jail (`Profile::WorkerBrowserClient` seccomp/Seatbelt clusters, `render.py` `PlaywrightRenderer`, browsers-in-venv, `TasksMax=512`, `tool_host::spawn_worker` stderr-drain). macOS `/tmp` `fs_write` = [#283](https://github.com/hherb/kastellan/issues/283); pure-Python Linux seccomp = [#281](https://github.com/hherb/kastellan/issues/281).
- **`inner_loop.rs` prod-split** (PR [#279](https://github.com/hherb/kastellan/pull/279), `e16c80e`): `invoke_skill` expansion → `inner_loop/invoke_expand.rs` + floor → `inner_loop/floor.rs`; 630 → 481 LOC.
- **Phase 4 python-exec acceptance + macOS fixes** (PR [#270](https://github.com/hherb/kastellan/pull/270), `0de4249`): per-OS interpreter cascade (excludes the xcrun shim; framework version-root granted), `unique_suffix` → `{pid}-{nanos}-{counter}`; `python_exec_e2e` green both platforms. Closed [#273](https://github.com/hherb/kastellan/issues/273).
- **egress proxy — all 4 slices** (PRs [#240](https://github.com/hherb/kastellan/pull/240)/[#256](https://github.com/hherb/kastellan/pull/256)/[#259](https://github.com/hherb/kastellan/pull/259)/[#269](https://github.com/hherb/kastellan/pull/269)/[#272](https://github.com/hherb/kastellan/pull/272)): #1 allowlist+SSRF, #2 force-routing (ON by default, fail-closed), #3a TLS-intercept MITM (ephemeral per-instance CA), #3b credential-leak scanner (`kastellan-leak-scan`), #4 SPKI TLS-pinning. Feature-complete; callers pass `secret_fingerprints:&[]` + `cert_pins_json:None` today.
- **Matrix comms channel (Phase 2 inbound)** (PR [#265](https://github.com/hherb/kastellan/pull/265)): decision + bus + hermetic Matrix client + pairing + conduwuit homeserver infra; `core/src/channel/*`, `workers/matrix*`, migration 0018. Phase D (live SDK) DGX-pending.
- **`db/src/secrets.rs` prod-split** (PR [#253](https://github.com/hherb/kastellan/pull/253)) + **public website kastellan.dev** (PR [#252](https://github.com/hherb/kastellan/pull/252)): operator action — connect Cloudflare Pages (output `site`, branch `main`); regenerate root `assets/*.png` (still "hhagent"-titled).

**Current state.** `main` carries the full python-exec arc (skill-catalog slice 1 `0cbddc5`, slice 2 `e478309`, runtime params `02ccb57`) + the slice-#1 worker (PR #267) + all 4 egress slices + the above. Dev box is **macOS** (Seatbelt); the DGX Spark (aarch64) is driven natively over WireGuard SSH (`ssh dgx '<command>'`) for real-bwrap/PG Linux acceptance.

**Standing macOS test-infra gotcha (not a regression):** a *full-workspace* run under `KASTELLAN_PG_BIN_DIR` flakes ~4
tests in `core/tests/embedding_recall_e2e.rs` at PG bring-up (`tests-common/src/pg.rs`) — parallel `initdb`/launchd
churn (issue #130 territory); they pass single-threaded and in isolation. Use skip-as-pass for the whole workspace on
the Mac; run live-PG suites individually or on the DGX.

**Toolchain note (standing).** Dev box + CI are on rustc **1.96.0**
(`dtolnay/rust-toolchain@stable`). On the dev **Mac**, `core` cannot be
cross-`cargo test`/`check`'d for Linux (its `ring` C dep needs
`x86_64-linux-gnu-gcc`, the #144 cross-compile wall) — `core`'s Linux path is
CI-verified, and the `linux-check` CI is **compile + clippy only** (no
`cargo test`). On the **DGX Spark** (aarch64), `core` compiles/tests/clippies
**natively**, so a full native-Linux `cargo test --workspace` +
`cargo clippy --workspace --all-targets -D warnings` are both runnable there.
The current native-Linux test baseline is **1839 / 0 / 15**
(`feat/281-gliner-relex-landlock`, 2026-06-16 — full `cargo test --workspace` with live PG 18 + worker binaries built
[`cargo build --workspace`, so the `kastellan-worker-lockdown-exec` shim bin is fresh — see the #281 process lesson]; clippy
`-D warnings` clean. **Unchanged from the browser-driver Landlock baseline — gliner-relex Landlock renamed a test, didn't add one;
the 4 `browser_driver_e2e` render tests are `#[ignore]` and counted in the 15 ignored.** Was 1829 after the browser-driver #281 seccomp half).

---

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — high-level diagram, process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — invariant, scenarios in scope, defence-in-depth layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO list with commit hashes for shipped items
4. The design plan (outside the repo) — `~/.claude/plans/i-d-like-to-design-logical-starlight.md`
5. Memory notes (auto-loaded) — see `~/.claude/projects/-home-hherb-src-kastellan/memory/MEMORY.md`
6. Older handovers — `archive/handover_<timestamp>.md` (one snapshot per pruning event; full historical detail lives there). Most recent: [`archive/handover_20260605_pre-prune.md`](archive/handover_20260605_pre-prune.md).

## Working state (what's green right now)

```
kastellan (Rust workspace, 17 crates [core, db, leak-scan, llm-router, sandbox, supervisor, protocol, tests-common, prelude, shell-exec, web-common, web-fetch, web-search, python-exec, egress-proxy, matrix, matrix-wire]; browser-driver + gliner-relex are Python workers, not Cargo members; mail = .gitkeep stub. AGPL-3.0)
├── core               kastellan-core: lib + 2 bins (`kastellan` daemon + `kastellan-cli` audit-tail viewer). Daemon blocks on SIGTERM/SIGINT via tokio::signal::unix; main.rs runs db::probe::run → connect_runtime_pool → spawn_mirror before wait_for_shutdown (fail-closed startup; mirror failures are logged but non-fatal). lib modules: tool_host (spawn_worker, dispatch chokepoint, lockdown-env derivation, wall-clock watchdog, sealed WorkerCommand, secret-ref substitution on input + injection-guard screen on output + **`tool_host/secret_scrub.rs` — python-exec-only output secret-scrub**: `worker_redacts_output` gate, `fingerprints_for_dispatch` via `Vault::value_fingerprint`, `scrub_result_value` walks the result's JSON string leaves through `kastellan_leak_scan::redact`, `emit_scrub_audit` writes redacted `policy/secret.output_scrubbed`; called on the `Ok(v)` arm **before** the injection screen so the screen+audit+return all see redacted output; no-op for every other worker), secrets (Vault TTL'd RwLock<HashMap> + SecretRef opaque newtype + substitute_refs_in_params walker + value_fingerprint [one-way hash of a secret value for the egress #3b leak scanner — never exposes plaintext]), cassandra/injection_guard (22-entry substring catalogue as `Rule`s + per-tool `GuardProfile` Strict/Relaxed via `for_tool` + `screen`/`screen_with_profile` + extract_scannable_text; Relaxed caps the chat-template family at one sub-threshold contribution — #142), workspace (per-task scratch with RAII cleanup), audit_mirror (PgListener-driven JSONL writer with daily rotation + fsync per write), audit_tail (`tail -f`-style follower used by `kastellan-cli audit tail`), scheduler/ (audit.rs pure helpers + canonical SCHEDULER_AUDIT_ACTOR; runner.rs spec §7 lifecycle rows + l3_run routing; tool_dispatch.rs short-circuit rows; crash_recovery.rs sweep_and_audit; l3_run.rs daemon-side L3 skill execution + `kind=="python"` branch → invoke_python_skill, fail-closed), memory/ (mod.rs facade + recall.rs three-lane RRF-fused recall + embed.rs embed_query + l0_seed/l1_promote/l3_crystallise/l3_approval/l3_invoke/l3_surface [kind-aware] + l3py_crystallise/l3py_approval/l3py_invoke [facade + pure prepare_python_invocation w/ SHA-drift TOCTOU close + operator invoke_python_skill + agent expand_python_for_agent/load_pinned_python_skill_by_name]), worker_lifecycle/ (Lifecycle enum + SingleUse/IdleTimeout/Composite managers; idle_timeout.rs acquire path + idle_timeout/release.rs release path; force_route.rs egress force-routing — `ForceRoutingConfig` [+ `cert_pins: Option<CertPinMap>` + `pins_for(allowlist)`, slice-#4 operator pins] + pure `policy_net_is_force_routable`/`resolve_force_routing`/`spawn_worker_maybe_forced` [selects pins per worker into `cert_pins_json`] + `ForceRoutingError` + `from_env`/`env_flag_enabled`/`parse_cert_pins_env` [reads `KASTELLAN_EGRESS_CERT_PINS` fail-closed; default scratch root `/tmp` on macOS for sun_path], the `KASTELLAN_EGRESS_FORCE_ROUTING` flip — **ON by default** in the supervised deployment via `core_service_spec`, fail-closed; both cold-spawn sites route Net::Allowlist workers through it), entity_extraction/ (batch_upsert.rs two-phase unnest + per-row attribution), worker_manifest (WorkerManifest trait + Resolution + ResolveCtx + discover_binary — the uniform self-description each worker registers behind), workers/ (shell_exec.rs ShellExecManifest + shell_exec_entry; web_fetch.rs WebFetchManifest + web_fetch_entry [Net::Allowlist + WorkerNetClient host-side manifest]; web_search.rs WebSearchManifest + web_search_entry [Net::Allowlist derived from the endpoint host:port; injects KASTELLAN_WEB_SEARCH_ENDPOINT + allowlist]; gliner_relex/ facade re-exporting wire.rs serde shapes + resolve.rs GlinerRelexEnv/resolve_env + entry.rs gliner_relex_entry(env, lockdown_shim)/host+container builders [host-mode: `Profile::WorkerMlClient`, binds the lockdown-exec shim into fs_read; **Landlock + seccomp ACTIVE** on Linux when Some — `LANDLOCK_RW=["/tmp"]` for torch's inductor cache, RO from fs_read, #281 fully closed] + client.rs Client + manifest.rs GlinerRelexManifest [Linux: fail-closed `discover_binary` of `kastellan-worker-lockdown-exec`, Misconfigured if absent; macOS: None]; browser_driver.rs BrowserDriverManifest + browser_driver_entry + pure resolve_env [ENABLE-gated, WorkerNetClient + legacy direct-net Net::Allowlist, no proxy_uds; slice #1 scaffold — real Playwright render is Phase 2]; python_exec.rs PythonExecManifest + python_exec_entry + pure resolve_env [ENABLE-gated, Net::Deny + WorkerStrict, scratch = jail /tmp tmpfs via explicit KASTELLAN_LANDLOCK_RW]), registry_build (static WORKER_MANIFESTS [shell-exec, gliner-relex, python-exec, web-fetch, web-search, browser-driver] + pure assemble_registry [skips the reserved `handoff` name] + async build_tool_registry(pool, exe_dir)), handoff (in-memory per-task content-addressed HandoffCache: stash_if_oversized → placeholder, fetch → clamped slice, per-task byte budget + MAX_TRACKED_TASKS backstop, purge_task at terminal; wired into ToolHostStepDispatcher after dispatch returns + the `handoff`/`fetch` built-in intercept), egress/ (host-side egress-proxy integration — slice #2 COMPLETE: DGX-accepted, force-routing ON by default: spawn.rs `spawn_sidecar`/`SidecarHandle` [+`terminate(&mut)`]/`proxy_policy`; audit.rs pure `decision_to_audit` + runtime-free `ingest_decisions_into`; net_worker.rs pure `rewrite_worker_policy` + `spawn_net_worker` [sidecar-first fail-closed, 1:1 teardown via `SupervisedWorker.egress`] + `spawn_forced_net_worker` [scratch-owning wrapper, `EgressSidecar.scratch` RAII-cleaned] + `pg_decision_sink`; **slice #3b leak scanner:** `leak_provision.rs` [atomic `write_secret_hashes` + `provision_audit_row` + **`merge_secret_hashes` union accumulator (#268) + `provision_failed_audit_row`**], `EgressSidecar::provision_dispatch_secrets` (resolves scratch = UDS parent); **dispatch-time live-append (#268):** `tool_host/egress_provision.rs` [`compute_provision` (sync, scans the pre-substitution snapshot, fingerprints via `Vault::value_fingerprint`) + `emit_provision` (audit rows, fail-closed `Err`)] wired into `dispatch_with_sink` before `worker.call` — D1 fail-closed / D2 union / D3 audit-newly-added (`ref_hash`-keyed); `audit.rs` maps `egress.blocked.credential_leak` redacted [hash+offset+direction]; **slice #4 TLS pinning:** `proxy_policy`/`spawn_sidecar` take `cert_pins_json: Option<&str>` [push `KASTELLAN_EGRESS_PROXY_PINS` only when Some(non-blank) ⇒ no-pin path byte-identical], the two spawn fns now take a **`NetWorkerSpawn<'a>` params struct** [`backend, proxy_bin, spec, allowlist, worker_name, secret_fingerprints, cert_pins_json`] + explicit scratch/scratch_root + sink [dropped both `#[allow(too_many_arguments)]`], `audit.rs` maps `egress.blocked.tls_pin`; callers pass `secret_fingerprints: &[]` today; **slice-#4 operator pins NOW WIRED (2026-06-18):** `cert_pins.rs` [pure `CertPinMap` + `parse_cert_pins` (structural — shape + `sha256/` prefix; proxy stays authoritative strict validator) + `host_of_endpoint` + `select_pins_for_allowlist` (per-worker least-privilege subset)] feeds `force_route::spawn_worker_maybe_forced`'s `cert_pins_json` from `KASTELLAN_EGRESS_CERT_PINS`; `None`/unset ⇒ byte-identical no-pin path)
├── db                 kastellan-db: pure helpers (build_initdb_argv, build_postgresql_auto_conf, find_pg_bin_dir, pg_bin_dir_candidates_with_env_override) + conn::ConnectSpec + RUNTIME_ROLE/set_role_runtime_statement + probe::run (ensure DB → migrate as superuser → SET ROLE → audit, fail-closed) + graph::{Graph trait, PgGraph; recursive-CTE path() + walk_outbound/inbound_edges + walk_edges_around with DISTINCT ON diamond-dedupe} + audit::{insert, fetch_by_id, fetch_since, truncate_payload} + memories::{insert, insert_memory_at_layer, insert_memory_light (embedding-skipping light write path), semantic/lexical/graph search, link_memory_to_entities, set_skill_trust, load_layer_by_trust} + entity_kinds + relation_kinds lookup caches + pool::{connect_runtime_pool, connect_admin_pool} + MIGRATOR (0001..0017) + memory_entities join table + deleted_memories audit table + secrets/ (AES-256-GCM at rest + OS keyring; prod-split into `crypto.rs` pure helpers [constants + validate_name/compute_aad/encrypt/decrypt] + `key_provider.rs` [KeyProvider trait + MapKeyProvider/OsKeyringProvider] + `error.rs` [SecretsError] + parent async DB I/O put/get/list/delete, all re-exported flat) + kastellan-db-init bin
├── leak-scan          kastellan-leak-scan: pure shared credential-leak scanner (egress #3b single source of truth; deps serde/serde_json/sha2 only). fingerprint.rs (`SecretFingerprint{len,fp64,sha256}` + `fingerprint_value` [Rabin fp64 + SHA-256] + `MIN_SECRET_LEN`=8 + `RABIN_BASE` + shared `pub(crate)` `pow_base`/`sha256_hex`), matcher.rs (`RollingMatcher` — streaming, per-length Rabin rolling pre-filter + SHA-256 confirm + `(maxLen+1)`-byte ring-buffer carry-over; `feed`→first `LeakHit{sha256_hex,offset}`; O(maxLen) mem ⇒ no body cap; used by egress-proxy to BLOCK), **redact.rs (`redact(input,&[SecretFingerprint])`→`RedactOutcome{bytes,hits:Vec<RedactHit>}` — bounded-buffer all-hits replace-in-place sibling of the matcher; marker `[redacted:<8hex>]`, earliest-then-longest overlap resolution; used by core to SCRUB python-exec output)**, wire.rs (`serialize_hashes`/`parse_hashes` for `secret_hashes.json`, hex-encoded, lenient). Consumed by `core` (provision + scrub) + `egress-proxy` (detect)
├── llm-router         kastellan-llm-router: sole egress for LLM calls. Router::send + Router::embed over reqwest+rustls; Backend::{Local, Frontier} closed enum; PolicyGate trait (DefaultLocalPolicy always Local — Phase-5 seam). RouterConfig::from_env reads KASTELLAN_LLM_* env. Per-OS default URL: vLLM/SGLang on Linux (:8000), Ollama on macOS (:11434). Frontier dispatch returns PolicyDeniedFrontier until Phase 5
├── sandbox            kastellan-sandbox: SandboxPolicy (+ additive `proxy_uds: Option<PathBuf>` — slice #2 force-routing target) + `Net` enum {Deny | Allowlist(hosts) | ProxyEgress (the egress proxy's own policy — real netns, self-enforcing; #141 slice #1)}; `Net::Allowlist + proxy_uds` ⇒ bwrap private netns + UDS bind / Seatbelt deny-outbound-except-UDS (slice #2). + `Profile` {WorkerStrict | WorkerNetClient | WorkerBrowserClient | **WorkerMlClient** (gliner-relex torch tier — #281; renders byte-identical to WorkerStrict off Linux, only the Linux `ml_client` seccomp layer differs)} + SandboxBackend trait + SandboxBackendKind (cfg-gated per-OS) + SandboxBackends resolver + LinuxBwrap (wrapped in systemd-run --scope cgroup) + MacosSeatbelt + MacosContainer (Apple `container` micro-VM, macOS-only, opt-in per-worker)
├── supervisor         kastellan-supervisor: SystemdUser (Linux; driver in systemd_user.rs + pure builders re-exported from systemd_user/builder.rs) + LaunchAgents (macOS) + specs::{core_service_spec, postgres_service_spec, kastellan_target_spec} + default_probe. ServiceSpec carries after/part_of ordering + optional restart_backoff (RestartBackoff{max_delay_sec,steps}: systemd → RestartSteps/RestartMaxDelaySec, launchd → warn-and-ignore); TargetSpec + Supervisor::{install,start,stop,uninstall}_target (default = generic bundle for launchd; SystemdUser overrides with a native kastellan.target unit). Names screened by validate_service_name before unit-file write
├── protocol           kastellan-protocol: JSON-RPC 2.0 over stdio (working)
├── tests-common       kastellan-tests-common: shared dev-dep crate (publish = false) — PgCluster + bring_up_pg_cluster(+_with_timeout), RAII guards, skip helpers, sandbox factory, binary discovery (+ `cli_command` env-clear'd operator-CLI builder), **`daemon.rs` (MockLlm/spawn_inert_mock inert-503 LLM + parameterised bring_up_daemon + DaemonHandle/DaemonGuards + assert_cli_success/assert_cli_failure — shared by the cli_memory_l3*_run_daemon_e2e suites; deliberately core-free)**, macOS launchd serial lock (reentrant), deterministic SHA-256-seeded embedding seed. Consumed only from [dev-dependencies]; never linked into a runtime binary.
├── workers/prelude      kastellan-worker-prelude: Linux-only Landlock + seccomp lock_down (no-op on macOS) + cross-platform setrlimit(RLIMIT_CPU). Landlock derives BOTH RW (from fs_write) and RO (from fs_read, env KASTELLAN_LANDLOCK_RO) rules so net workers can read /etc/resolv.conf in-jail; **`KASTELLAN_LANDLOCK_PROFILE=none` skips the Landlock layer** (additive, `LandlockReport::Disabled`; supported opt-out but **no current worker sets it** — both browser-driver and gliner-relex now run Landlock-active, #281 fully closed). 2 bins: `kastellan-lockdown-probe` (test fixture; + `raw-getpid`/`raw-unshare` pre-lockdown subcommands) and **`kastellan-worker-lockdown-exec`** (#281 — production exec-shim: `rlimit::apply_from_env()` → `lock_down()` → `execve(target)`; the target inherits seccomp under `NO_NEW_PRIVS`; gives pure-Python venv workers worker-side Linux seccomp since bwrap spawns them directly, bypassing the Rust prelude — used by browser-driver AND gliner-relex). seccomp `Profile` {Strict | NetClient | BrowserClient | **MlClient**}: `browser_client` ADDITIONS include `capget`+`capset` (Playwright-Node + Chromium-zygote); **`ml_client` = `net_client` + `ML_CLIENT_ADDITIONS` {mbind, get_mempolicy, mlock, munlock, mknodat}** (torch/CUDA-probe/NUMA, DGX-enumerated via the kill-mode/`journalctl -k` loop; all DGX-confirmed load-bearing)
├── workers/shell-exec   kastellan-worker-shell-exec: uses prelude::serve_stdio
├── workers/web-common   kastellan-worker-web-common: shared lib for net-egress workers. allowlist.rs (HostAllowlist: host-only `from_env_json`/`is_allowed` + **port-scoped `from_endpoints`/`is_allowed_endpoint`/`is_port_scoped`** [host:port, IPv6-aware — #241]) + http.rs (HttpGet seam [+`transport_kind`] + RawResponse + ReqwestGet + **env-selected `make_get` factory**) + proxy_connect.rs (**ProxyConnectGet**: CONNECT-over-UDS HttpGet, hyper+tokio-rustls/ring, end-to-end TLS — used when `KASTELLAN_EGRESS_PROXY_UDS` set) + testing.rs (FakeGet, `testing` feature). Consumed by web-fetch + web-search + egress-proxy.
├── workers/web-fetch    kastellan-worker-web-fetch: first net-egress worker. HTTPS-only web.fetch JSON-RPC method. Consumes HostAllowlist + the HttpGet transport from web-common. extract.rs (HTML readability via dom_smoothie / PDF via pdf-extract / text+JSON, char-boundary text cap) + fetch.rs (the drive() redirect-follow loop — strict https-only per hop, 5-redirect cap) + handler.rs (web.fetch dispatch). Host-side manifest in core/src/workers/web_fetch.rs
├── workers/web-search   kastellan-worker-web-search: second net-egress worker. web.search JSON-RPC method (query → ranked {title,url,snippet,engine} hits from a SearxNG /search?format=json endpoint). Consumes HostAllowlist + transport from web-common. parse.rs (lenient SearxNG-JSON → Vec<Hit>) + search.rs (validate_endpoint [https everywhere, http loopback-only via is_loopback] + build_query_url + one-GET search() drive, count.clamp(1,20)) + handler.rs (dispatch + fail-closed from_env). Operator-configured KASTELLAN_WEB_SEARCH_ENDPOINT; LLM supplies only the query. Host-side manifest in core/src/workers/web_search.rs. Dev setup: scripts/web-search/setup-searxng.sh
├── workers/browser-driver kastellan-worker-browser-driver: Playwright-Python read-only render worker (ROADMAP:147; **egress slice #2 — egress-proxy-ROUTED in the default force-routed deployment**, opt-in KASTELLAN_BROWSER_DRIVER_ENABLE=1; #263/#280 resolved). Force-routing rewrites the manifest's `Net::Allowlist` (proxy_uds stays `None` in the manifest, SET at spawn by `rewrite_worker_policy` — like web-fetch) → private netns + per-worker egress sidecar in **no-MITM/transparent-tunnel** mode (`disable_mitm` keyed on the worker name; the browser does end-to-end TLS, can't trust our CA). In-jail **`shim.py` `ProxyShim`** (loopback-TCP↔UDS byte-pipe; Chromium `--proxy-server=127.0.0.1:<port>`) bridges Chromium's CONNECT to the sidecar UDS. macOS Seatbelt grants loopback-TCP for `WorkerBrowserClient`+proxy_uds; bwrap brings `lo` up in the netns. Runs direct-net only when force-routing is OFF (dev). MITM-of-browser (in-Chromium CA trust via NSS) deferred. NB on macOS: the non-forced render works under Seatbelt (#284 RESOLVED — out-of-prefix interpreter libs are now auto-bound, see `interpreter_deps.rs`); the **forced** egress-sidecar path on macOS is tracked by [#287](https://github.com/hherb/kastellan/issues/287) (Linux/bwrap forced is green).
    Modules: `browser.render` JSON-RPC stdio → headless Chromium (`--no-sandbox --disable-dev-shm-usage` + the slice-#2 `--proxy-server`/`--proxy-bypass-list` when force-routed) → post-JS readable text (readability-lxml) + final HTML, byte/char-capped. __main__.py (builds PlaywrightRenderer + starts/stops `ProxyShim` when `KASTELLAN_EGRESS_PROXY_UDS` set) + server.py (stdio dispatch + url/timeout/wait_until validation) + render.py (pure `extract_render_result` + `build_launch_args` + `PlaywrightRenderer` behind a `start()/stop()` seam + host_port_from_url/request_is_allowed) + **shim.py** (`ProxyShim` loopback-TCP↔UDS relay) + allowlist.py (per-nav/subresource interception, fail-closed) + errors.py. Host manifest = core/src/workers/browser_driver.rs (`Profile::WorkerBrowserClient`, Net::Allowlist, proxy_uds:None in-manifest [set at spawn by force-routing], browsers-in-venv via PLAYWRIGHT_BROWSERS_PATH, **per-spawn `ephemeral_scratch: true` + `fs_write` empty** [#283 CLOSED: macOS host-created `KASTELLAN_WORKER_SCRATCH` dir; Linux bwrap `/tmp` tmpfs — the worker's `_apply_worker_scratch` points TMPDIR/HOME at the scratch dir when the env is set, else the seeded `/tmp` stands], TasksMax=512, interpreter-root + KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ binds). Install: scripts/workers/browser-driver/install.sh (self-contained system-venv, non-editable, chromium into <venv>/browsers). **#281 FULLY CLOSED: on Linux the worker is spawned through the `kastellan-worker-lockdown-exec` shim (manifest sets `ToolEntry.lockdown_shim`, fail-closed if the shim is missing, binds it into `fs_read`) so BOTH the `browser_client` seccomp filter AND the Landlock ruleset apply (RO from `fs_read`, RW=`["/tmp"]` for Chromium's `--user-data-dir`); no longer sets `KASTELLAN_LANDLOCK_PROFILE=none`. macOS applies the profile via Seatbelt from the parent.**
├── workers/python-exec  kastellan-worker-python-exec: Phase-4 executor for agent-authored Python (opt-in KASTELLAN_PYTHON_EXEC_ENABLE=1). `python.exec` {code} → {exit_code, stdout, stderr, *_truncated}: source piped over stdin to `<python> -I -S -B -` (curated stdlib = no site-packages), child env cleared, 256 KiB code/capture caps; Python exceptions return as exit_code+traceback, not RPC errors. Strictest policy of any worker: Net::Deny + WorkerStrict seccomp (inherited by the CPython child; pinned by coreutils_smoke::python3_survives_strict) + fs_write=[] (scratch = jail's ephemeral /tmp tmpfs via explicit KASTELLAN_LANDLOCK_RW=["/tmp"]; macOS host-created per-spawn dir via ephemeral_scratch) + cpu 10 s / mem 512 MiB / wall 30 s, SingleUse. **Runtime params: ≤64 KiB ride the `KASTELLAN_PYTHON_PARAMS` env var; >64 KiB (up to the configurable `KASTELLAN_PYTHON_PARAMS_FILE_MAX`, default 1 MiB) are written to `<scratch>/params.json` (0600) and handed to the child via `KASTELLAN_PYTHON_PARAMS_FILE` (inline env defaulted to `"{}"`); over-ceiling fails closed.** lib: `exec/mod.rs` (python_args, truncate_lossy, run_code, serialize_params, + pure `params_file_max`/`decide_param_channel`/`params_env_pairs`/`ParamChannel` + `write_params_file`) + `exec/tests.rs` + handler.rs. Host manifest = core/src/workers/python_exec.rs (injects `KASTELLAN_PYTHON_PARAMS_FILE_MAX` into the jail only when the operator set it)
├── workers/matrix       kastellan-worker-matrix: Matrix inbound worker (**Phase D live `LiveSdk` DONE**). `MatrixSdk` seam (`sdk.rs`) + `MatrixHandler` for `matrix.init/poll/send` (handler.rs, fake-SDK unit tests). `matrix-sdk = 0.8.0` OPTIONAL dep behind `live-matrix = ["dep:matrix-sdk"]` (`e2e-encryption, sqlite, bundled-sqlite, rustls-tls`; default-features=false; default build unaffected). `ProxyBridge` (`bridge.rs`): loopback-TCP↔UDS relay (`bind(uds)→proxy_addr()`, accept loop, Drop-aborts; 2 unit tests). **`sdk_live.rs` (live-matrix): `LiveSdk` impl of `MatrixSdk`** — owns a multi-thread tokio `Runtime`, `block_on`s the SDK behind the sync methods; `LiveSdkConfig::from_env`/pure `parse_config`; `connect()` = create-store → build client (`.proxy()` via `ProxyBridge` when `KASTELLAN_EGRESS_PROXY_UDS` set) → **restore-or-password-login** persisting `<store>/session.json` → `add_event_handler` (room-text → bounded `VecDeque`, skips own echoes) → `sync_once` → spawn continuous `sync`; `poll` drains w/ long-poll wait, `send` resolves room + `text_plain`. Holds `client: Option<Client>` + a `Drop` that drops it inside `runtime.block_on` (matrix-sdk's deadpool SQLite `Drop` calls `spawn_blocking` → SIGABRTs off-runtime; DGX-found). `main.rs` (live-matrix): `LiveSdk::connect` → `rlimit` → `lock_down` → raw `serve_stdio` (network-init-then-lockdown); crate `#![cfg_attr(not(feature="live-matrix"), allow(dead_code))]`. `egress_spike.rs` (`#[cfg(all(test, feature="live-matrix"))]`): hermetic CONNECT-through-bridge proof. Tests: 7/0/0 (default), 13/0/0 (`live-matrix`: +4 `sdk_live` +2 spike). Live round-trip = `core/tests/matrix_live_e2e.rs` (`#[ignore]`, DGX/conduwuit).
├── workers/matrix-wire  kastellan-matrix-wire: shared serde wire types (`Event`/`PollResult`/`PollParams`/`SendParams`/`InitResult` + `push_bounded`). Consumed by `workers/matrix` + `core/src/channel/matrix.rs`.
└── workers/egress-proxy kastellan-worker-egress-proxy: per-worker egress boundary (ROADMAP:141/142; ALL 4 slices done — #1 allowlist+SSRF, #2 force-routing, #3a TLS-intercept, #3b leak scanner, #4 TLS pinning). Sandboxed CONNECT proxy on a per-worker UDS; per CONNECT: HostAllowlist check (reuses web-common) → resolve DNS itself → ssrf.rs is_denied_range (reject private/loopback/link-local/ULA/CGNAT/multicast, IPv4-mapped+compatible unwrapped; literal-IP carve-out) → pin+dial → write 200 → peek first tunnel byte (recv MSG_PEEK; 0x16 → MITM, else transparent tunnel). **Slice #3a MITM:** in-proxy ephemeral per-instance CA (ca.rs, rcgen; private key never leaves the sandbox, public ca.pem exported beside the UDS), per-host CA-signed leaf cache (leaf_cache.rs), async terminate+re-originate (mitm.rs: looks_like_tls + intercept — tokio-rustls TlsAcceptor/TlsConnector + copy_bidirectional on a per-connection current-thread runtime; upstream validated against webpki). Decision carries tls_intercepted. **Slice #3b leak scanner:** `MitmCtx.secret_hashes_path` + `load_patterns` (lazy per-connection read of `secret_hashes.json`; missing/corrupt ⇒ no scan, fail-OPEN); `mitm/relay.rs` `scan_relay` replaces `copy_bidirectional` when patterns present — splits both halves, one `kastellan-leak-scan::RollingMatcher` per direction, **scans each chunk before forwarding**, kills on hit; `intercept` returns `Result<Option<LeakReport>,String>`; `report::Verdict::BlockedCredentialLeak` + `Decision.leak`. **Slice #4 TLS pinning:** new `pins.rs` (`spki_sha256` [SHA-256 of DER SubjectPublicKeyInfo via x509-cert], `PinSet` [`KASTELLAN_EGRESS_PROXY_PINS` JSON `{host:["sha256/<b64>"]}` → lowercased host → 32-byte digests; **a host with an empty pin list ⇒ Err ⇒ startup aborts**], `chain_has_pin`, `PinningVerifier` [rustls `ServerCertVerifier`: webpki FIRST then SPKI-pin overlay for pinned hosts, else `RustlsError::General(PIN_MISMATCH_MARKER)`], `build_upstream_client_config` [None/blank/`{}` ⇒ plain webpki byte-identical; valid ⇒ `.dangerous()` custom verifier; malformed ⇒ Err ⇒ startup aborts]); `main.rs` reads the pins env once before lock_down; `proxy::classify_mitm_error` maps the marker → `Verdict::BlockedTlsPin`/`pin_mismatch`. **Fail-CLOSED** for a configured pin; additive over webpki (never weakens netns/allowlist/SSRF). Forward-looking: no pins provisioned today. Modules: pins.rs, ssrf.rs, request_line.rs, report.rs, proxy.rs (decide + handle_conn connect→200→peek→branch + MitmCtx + run_mitm + load_patterns + classify_mitm_error), ca.rs, leaf_cache.rs, mitm.rs (+ mitm/relay.rs), main.rs (install ring provider, generate CA + write ca.pem before lock_down, build pin-aware upstream config, accept loop). Host side = core/src/egress
```

**Test baselines.** Native-Linux (DGX, PG 18 live, rustc 1.96.0, worker bins built via `cargo build --workspace`): **1839 / 0 / 15**
on `feat/281-gliner-relex-seccomp` (2026-06-16 #281 gliner-relex acceptance; the real-sandbox e2e suites actually run here —
incl. the 3 gliner real-model suites loading `multi-v1.0` + running `extract` **under the kill-mode `ml_client` seccomp filter
applied via the lockdown-exec shim**; + the 4 `browser_driver_e2e` render tests under `browser_client`; + `lockdown_exec_smoke`).
macOS (2026-06-17, in-process scrub e2e): full workspace `cargo test --workspace` **1879 / 0 / 13** (1878 prior + 1 new
`python_exec_e2e::materialized_secret_param_is_scrubbed_from_output`) + clippy `--workspace --all-targets -D warnings`
clean; the new scrub e2e + `python_exec_e2e` suite ran live (PG 18 + real Seatbelt jail). (Prior scrub-session macOS
baseline was **1878 / 0 / 13** = 1877 at #297 merge + 1 overlap-pin.) **DGX native-Linux not re-run** — a test-only
addition + a test-harness refactor touching no sandbox/seccomp/Landlock; the 1839/0/15 Linux baseline is carried forward
as the standing gate.
8–15 ignored = explicit doctest/real-net markers;
`[SKIP]` lines on `--nocapture` are GLiNER-Relex real-model tests gated on
`KASTELLAN_GLINER_RELEX_ENABLE=1`. (Full per-session test-count history is in the
archive snapshots; the suite table below lists what each integration suite verifies.)

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit (linux) | 16 | bwrap argv builder shape (6) + cgroup `systemd-run` argv builder shape (10) |
| `sandbox` unit (macos) | 14 | sandbox-exec profile builder + path canonicalization + on-host probe + TinyScheme-injection rejection + strict-profile mach-lookup guard (issue #1) |
| `sandbox` integration (`linux_smoke`) | 7 | **real** bwrap+cgroup: jailed echo, fs invisibility, net deny, relative-path reject, OOM-kill under MemoryMax, `/tmp` per-spawn ephemeral tmpfs (#89) |
| `sandbox` integration (`macos_smoke`) | 10 | **real** sandbox-exec: jailed echo, fs invisibility, fs_read readable, net deny, fresh session leader (#2), no appleevents bootstrap (#1) |
| `sandbox` integration (`macos_container_smoke`) | 7+ | **real** Apple `container`: argv shape, alpine smoke under `--init`, bind-mount-readonly, strict profile, probe skip |
| `core` unit | 60+ | lockdown-env, watchdog, workspace RAII, audit parsers, dispatch-result mapping, ToolRegistry, injection_guard catalogue, secrets Vault + SecretRef, L3 crystallise/approval/invoke/surface units (see archive for full breakdown) |
| `core` integration (`shell_exec_e2e`) | 4 | **cross-platform real** core → sandbox → shell-exec round-trip; every call routes through `tool_host::dispatch` |
| `core` integration (`python_exec_e2e`) | 4 | **real** core → sandbox → python-exec round-trip under the production policy: print round-trip, socket-attempt contained by the jail, **per-spawn scratch write (now cross-platform — Linux tmpfs `/tmp` + macOS host-created per-spawn dir, #283)**, **materialized-secret param scrubbed to `[redacted:]` + one `secret.output_scrubbed` row** (the in-process scrub e2e — full daemon e2e → #298) |
| `web-common` unit | 8 | shared `HostAllowlist` matcher (exact/wildcard/case/lookalike/empty/malformed-json/trim/lone-dot) |
| `web-fetch` unit | 21 | extract (HTML/PDF/text/JSON/char-boundary cap/unsupported), fetch redirect-drive (cap, non-allowlisted/non-HTTPS refusal, no-Location), handler (happy path, policy-denied arms, method-not-found, invalid-params). (Allowlist matcher tests moved to `web-common`.) |
| `core` integration (`web_fetch_e2e`) | 1 (+1 ignored) | **real** sandbox deny-path: host outside allowlist is denied (hermetic); `real_fetch_extracts_readable_text` `#[ignore]` (real network, validates DNS+TLS in-jail) |
| `web-search` unit | 24 | parse (SearxNG-JSON happy/url-less-skip/defaults/empty/missing-key/malformed), search (parsed hits, count truncate+clamp, empty-query, non-200, redirect, loopback truth table incl. `[::1]`, scheme rule https/http-loopback/http-remote-denied, host-not-allowlisted, request-URL build), handler (method-not-found, missing/empty query, happy path, operation-failed) |
| `core` integration (`web_search_e2e`) | 1 (+1 ignored) | **real** sandbox fail-closed deny-path: endpoint host off allowlist → worker refuses at startup (hermetic); `real_search_against_searxng` `#[ignore]` (live SearxNG, DNS/TLS/loopback in-jail) |
| `core` unit (`web_search` manifest) | 3 | resolve registers `WorkerNetClient` + endpoint-derived `Net::Allowlist` (loopback `:8888` + https `:443`); `Misconfigured` when no binary |
| `egress-proxy` unit | 37 | ssrf (denied ranges v4/v6 + mapped + compatible) 7, request_line 7, report (JSON line + `tls_intercepted`) 4, proxy (`decide` + real-UDS `handle_conn` pass-through round-trip + `tls_intercepted=false` + 403) ~9, **slice #3a:** `ca` (CA PEM round-trip + leaf SAN + uniqueness) 3, `leaf_cache` (Arc reuse + distinct + bounded) 3, `mitm` (`looks_like_tls` 2 + **hermetic two-leg TLS round-trip** with only-CA worker trust 1) 3 |
| `core` integration (`egress_proxy_e2e`) | 2 (+1 ignored) | **real** sandboxed sidecar via `spawn_sidecar` + test CONNECT client: allowed literal-loopback round-trip + off-allowlist 403 + `decision_to_audit` mapping; PG-gated `audit_log` persistence (skip-as-pass); `#[ignore]` real-net round-trip |
| `core` integration (`egress_force_routing_e2e`) | 3 (+1 ignored) | **real** live force-routing via `spawn_forced_net_worker`: allow round-trip + 403 + `on_decision` ingest + 1:1 teardown + **slice #3a `ca.pem` export asserted under the real sandbox**; Linux-only no-direct-route; PG-gated `pg_decision_sink`→`audit_log`. `#[ignore]` `real_mitm_fetch_through_sidecar` (live HTTPS origin through the MITM, only-CA worker trust — 200 on the Mac; fails on the DGX for a pre-existing DNS/env reason). Skip-as-pass without sandbox/proxy-bin/PG; runs on macOS (Seatbelt) + DGX (bwrap) |
| `core` unit (`egress::audit`/`egress::spawn`) | 5 | `decision_to_audit` verdict→action + garbage-None + **`tls_intercepted` carry/default** (4); `proxy_policy` Net::ProxyEgress+WorkerNetClient+env-keys (1). Plus `rewrite_worker_policy` injects CA `fs_read`+env (in `net_worker` tests) |
| `core` unit (`handoff`) | 19 | HandoffRef parse, put/get_slice round-trip + offset/len/eof, per-task budget eviction, global MAX_TRACKED_TASKS backstop, purge isolation, placeholder fields, stash passthrough/over-cap/exact-cap, fetch utf8/clamp/not-found/invalid/cross-task |
| `core` integration (`handoff_dispatch_e2e`) | 3 | **hermetic** (lazy pool, fake lifecycle) dispatcher-level `fetch_handoff` intercept: stashed slice returned, unknown-ref → HANDOFF_NOT_FOUND, missing param → INVALID_PARAMS |
| `core` unit (`registry_build`) | 6 | assemble_registry Register/Disabled/Misconfigured + the reserved-`handoff`-name skip |
| `core` integration (`memory_recall_e2e`) | 1 | **real** Phase-1 entry: all three lanes + 1-hop entity expansion + fused RRF + empty-seed degrade |
| `core` integration (`cli_ask_e2e`) | 2 | **real** full prod chain (CLI → PG → scheduler → LLM → CASSANDRA → dispatch → finalize) against a queued mock LLM |
| `core` integration (`injection_guard_e2e`) | 6 | **PG-required**: placeholder shape, one policy row, privacy invariant, SHA shape, benign passthrough, error-path bypass |
| `core` integration (`injection_guard_fixtures`) | 4 | per-tool profiles (#142): benign chat-template docs Allow under Relaxed + Block under Strict; corroborated attacks Block under both; full `extract_scannable_text`→`screen_with_profile` pipeline on a web-fetch-shaped value |
| `core` integration (`secret_vault_e2e`) | 9 | **PG-required**: materialize/redeem rows, fail-closed redemption, opaque-ref-not-plaintext (#147), no plaintext in policy rows |
| `core` integration (`cli_memory_l3_run_daemon_e2e`) | 2 | **PG + real daemon**: `--execute` succeeds against the daemon registry with `env_clear()` + NO `KASTELLAN_SHELL_EXEC_BIN` (the #179 regression pin) + no-daemon cancels & errors |
| `core` integration (`cli_memory_l3_e2e` / `_run_e2e`) | 10 / 5 | **PG-required**: L3 list/remove/approve/revoke/pin + operator `run` (dry-run / execute / refuse paths) |
| `db` unit | 71+ | initdb/auto_conf/bin-dir builders, ConnectSpec, graph pins, probe SQL pin, RUNTIME_ROLE pins, audit truncate, secrets AES-GCM, memory pins, kinds validation |
| `db` integration (`postgres_e2e`) | 8+ | probe idempotency, PgGraph, runtime-role REVOKE, audit NOTIFY, secrets, memory_entities cascade, deleted_memories journalling, walk-edges dedupe |
| `llm-router` unit + integration | 41 + 8 | error truncate, decode, config from_env, embedding wire shapes, compose_url, pick_backend; hand-rolled TCP mock chat+embed chokepoints |
| `prelude` unit + smoke | 21 | env/profile parse, BPF builds, syscall presence; landlock_smoke (4); seccomp_smoke (6) |
| `supervisor` unit + integration | 44–52 + 2–4 | build_unit_file/build_plist, validate_service_name, driver round-trips, specs; systemctl/launchctl bootstrap (macOS serialised via reentrant Mutex) |
| `core` integration (scheduler_*_e2e) | 8+ | inner_loop, lanes, crash_recovery, agent_prompts — cross-platform skip-as-pass without PG |

**Build & test:**
```sh
source "$HOME/.cargo/env"
cargo build --workspace          # produces ./target/debug/kastellan + workers (macOS; see #144 for Linux)
cargo test --workspace           # all green on macOS (skip-as-pass) / DGX (live PG)
./target/debug/kastellan           # runs the core daemon, emits one JSON log line
```

**Required one-time host setup (Ubuntu 24.04+ only):** the AppArmor profile that lets `bwrap` create unprivileged user namespaces is already installed on the user's DGX Spark. Other Linux hosts may need `sudo scripts/linux/install-bwrap-apparmor-profile.sh`. macOS uses `sandbox-exec` (no setup needed).

---

## Earlier history (summary)

One bullet per session, newest first. Full reasoning lives in the archive snapshots:
the L3 arc + 2026-05-29 → 2026-06-04 sessions in
[`archive/handover_20260605_pre-prune.md`](archive/handover_20260605_pre-prune.md);
sessions 2026-05-10 → 2026-05-29 in
[`archive/handover_20260529_pre-prune.md`](archive/handover_20260529_pre-prune.md);
sessions 2026-05-06 → 2026-05-09 in
[`archive/handover_20260510_pre-prune.md`](archive/handover_20260510_pre-prune.md).

- **2026-06-15 — IBM Granite Guardian 4.1 evaluation (docs-only, branch `claude/exciting-wilson-c1f637`):**
  investigated `ibm-granite/granite-guardian-4.1-8b` as a model-based safety/judge tier and the user
  **locally smoke-tested it** (Mac, 8-bit quant: performance "not bad", reasoning "quite solid for that size")
  ⇒ **viable**. Apache-2.0 (license clean), hybrid Mamba-2 (low memory), runs through the existing
  `kastellan-llm-router` local pointer (Ollama :11434 / vLLM :8000) — no new egress, no vendor/NVIDIA dep.
  **Advisory / defense-in-depth ONLY, never a gate** (~0.79 F1, misses ~1 in 5; sandbox + egress proxy stay
  the real containment). Added a Phase 5 ROADMAP item ("Model-based CASSANDRA guard tier") with three hook
  points; **first slice = `GuardianReviewStage` implementing `ReviewStage`**, slotted into `ChainReviewStage`
  after `DeterministicPolicy`, `yes`→`Verdict::Advisory` (not `Block`), no-think `Router::send`, fail-open.
  Hooks 2/3 = function-call-hallucination pre-flight at `ToolHostStepDispatcher` + groundedness on
  `memory::recall`. Caveats: English-only; `<think>` traces not logged verbatim; ~doubles inference load.
  No code; ROADMAP-only. Memory note: `granite-guardian-evaluation.md`.
- **2026-06-12 — comms SLICE #6: conduwuit homeserver infra (branch `claude/zen-bell-6bn2ze`):** the homeserver
  deliverable, shaped as operator infra (NOT a kastellan `ServiceSpec` — the user-level supervisor can't run conduwuit
  as a dedicated `matrix` user, so it's a root/system unit or a separate host). `deploy/matrix/conduwuit.toml.template`
  (federation OFF, loopback bind, token-gated registration); `deploy/matrix/kastellan-matrix.service.template` (hardened
  SYSTEM unit — dedicated user, `NoNewPrivileges`/`ProtectSystem=strict`/`SystemCallFilter=@system-service`/`ReadWritePaths`
  data-dir-only); `scripts/matrix/setup-conduwuit.sh` (dev/Tier-C: render→validate→run on loopback, container or binary);
  `scripts/matrix/check-conduwuit-config.sh` (verifier — federation-off + loopback + registration-not-open; `--self-test`
  renders the template + asserts accept-safe / reject-open-registration, **green here**); `docs/deploy/matrix-homeserver.md`
  (Tier A/B/C + co-hosting blast-radius analysis + root install steps + reverse-proxy/firewall). ROADMAP homeserver item ticked.
- **2026-06-12 — comms SLICE #4 (outbound reply mapping; code, branch `claude/zen-bell-6bn2ze`):** fixed
  `channel::route::reply_body` to surface the agent's **real** completion result. A completed task's
  `tasks.result` is `Outcome::result_payload()` = the agent's `plan.result` (default
  `{"kind":"text","body":"..."}`), **not** a `{"kind":"completed"}` wrapper — the slice-#1 stub assumed the
  latter, so a real Matrix reply would have said "Task finished (text)." instead of the answer. Now: any
  non-`error`/`blocked`/`refused` result is a completion → surface `body` (non-empty), then a `message`
  alias, then compact JSON; `error`/`blocked`/`refused` map to safe user sentences. +3 route tests (29
  channel lib tests total); clippy clean. Live delivery still rides slice #2 Phase D. (Isolated fix to
  existing slice-1 code — git-history-documented per ROADMAP convention; ROADMAP "Matrix outbound" noted.)
- **2026-06-12 — comms SLICE #3: DM pairing (in-channel single-use code + DB-backed authorizer; code, branch `claude/zen-bell-6bn2ze`):**
  operator decisions = **in-channel code handshake** (with a bounded carve-out) + **defer WebAuthn** (no consumer surface).
  Shipped: migration **0018** (`pairings` + `pairing_codes` + least-privilege grants — runtime can authorize/bind/consume
  but NOT revoke or mint codes); `db::pairings` (is_paired/insert_pairing/revoke_pairing/list_pairings/insert_code/
  any_active_code + **atomic single-use** `claim_code`); `auth.rs` refactor — `PeerAuthorizer` now **async + (channel,peer)**;
  `StaticPairings` async; **`DbPeerAuthorizer`** (fail-closed on DB error); `ingest.rs` refactor (authz moved to the bus;
  pure `screen_and_classify` → Enqueue|InjectionBlocked; `sha256_hex` shared); `bus.rs` — **`PairingService` seam** + the
  **carve-out** in `handle_inbound` (the ONLY place unpaired input is touched, **compare-only** — SHA-256 vs an active code,
  never enqueued/echoed; returns a pairing-ack `OutgoingMessage` on success); `ChannelBus::spawn` takes
  `Option<PairingService>`; **`DbPairingService`** (`any_active_code` gate → atomic claim+bind in one tx); **CLI**
  `kastellan-cli pair {issue,list,revoke}` (mint single-use code, hash-only storage, print plaintext once, audit
  `pairing.code_issued`/`pairing.revoked`). Tests: 26 channel lib (auth/ingest/bus carve-out incl. valid-code-pairs +
  wrong-code-dropped) + 4 CLI + 3 channel e2e green here; `db::pairings` PG e2e (single-use claim, expired-code, revoke)
  skip-as-pass as root (live DGX/Mac); full workspace clippy `-D warnings` clean. **Deferred:** WebAuthn; daemon wiring
  (swap `StaticPairings`→`DbPeerAuthorizer` + pass `DbPairingService` into `ChannelBus::spawn`) — rides slice #2 Phase D;
  per-peer classification-floor policy. Spec/plan: `docs/superpowers/{specs,plans}/2026-06-12-channel-pairing*`.
- **2026-06-12 — comms SLICE #2 Phases A–C+E: Matrix inbound via a sandboxed worker (code, branch `claude/zen-bell-6bn2ze`):**
  decided architecture = **sandboxed worker** (matrix-rust-sdk in `kastellan-worker-matrix`, not in-core) + **spec+plan
  first** (hold the live SDK code). Shipped the hermetic, verify-anywhere portion: `workers/matrix-wire`
  (shared serde wire types `Event`/`PollResult`/`PollParams`/`SendParams`/`InitResult` + `push_bounded`);
  `workers/matrix` (the `MatrixSdk` seam + `MatrixHandler` for `matrix.init/poll/send`, fake-SDK unit tests; `main`
  gated on the `live-matrix` feature — default build compiles the hermetic parts, refuses to run without the real SDK);
  `core/src/channel/matrix.rs` (the `WorkerClient` seam + `MatrixChannel` — a blocking **driver thread** bridges the
  **synchronous** `kastellan-protocol::Client` to the async `Channel` trait via mpsc, keeping the protocol pure
  request/response with no server-initiated notifications; `ProtocolWorkerClient`; `spawn_worker_client` reusing
  `derive_lockdown_env` so the channel worker is locked down like a tool worker but holds a raw `Client` since poll/send
  are transport plumbing, NOT audited dispatches — correctly bypassing the #16 dispatch seal; `build_matrix_policy` pure;
  `MatrixConfig::from_env`/`parse_peers_csv`); a **config-gated `main.rs` hook** (byte-identical when
  `KASTELLAN_MATRIX_HOMESERVER` unset); and `core/tests/matrix_channel_e2e.rs` (full `MatrixChannel`→`ChannelBus` loop
  against a real `fake_matrix_worker` example process — paired round-trip + unpaired-dropped negative — **no
  matrix-rust-sdk / homeserver / sandbox / PG**). Tests: 6 wire + 5 handler + 7 core-channel-matrix (driver/policy/config)
  + 2 matrix e2e, all green here; full workspace builds; clippy `-D warnings` clean (default features).
  **Phase D (DGX-pending):** real `matrix-rust-sdk` `LiveSdk` impl + egress force-routing coupling + persistent encrypted
  E2E store + restart supervision + dev conduwuit script + `#[ignore]` live e2e; **top risk = the
  matrix-rust-sdk-through-MITM-egress-proxy spike** (custom-CA + CONNECT-over-UDS; fallback = MITM-bypass pin for the
  trusted homeserver). Deferred slices: #3 pairing (replaces `StaticPairings`), #4 outbound richness, #5 email, #6
  homeserver supervisor unit. Spec/plan: `docs/superpowers/{specs,plans}/2026-06-12-matrix-inbound-sandboxed-worker*`.
- **2026-06-12 — comms SLICE #1: channel-bus abstraction (code, branch `claude/zen-bell-6bn2ze`):** built
  `core/src/channel/` — dyn-safe `Channel` trait (`IncomingMessage`/`OutgoingMessage`) + the pure
  security core: fail-closed `PeerAuthorizer`/`StaticPairings` (`auth.rs`, empty ⇒ deny all),
  `classify_inbound` (authorize-FIRST → `injection_guard` screen under `GuardProfile::Strict` →
  `tasks` payload, `ingest.rs`), `reply_for_completed_task` (finalized task → user reply,
  `route.rs`) — plus the `ChannelBus` runtime (`bus.rs`) over four seams (`Channel`/
  `PeerAuthorizer`/`ChannelEvents`/`CompletedTasks`; real `PgChannelEvents` enqueue+audit +
  `PgCompletedTasks` over the `tasks_completed` NOTIFY — the Postgres `tasks` queue IS the
  fan-in/fan-out, no new IPC). Channel tasks carry the same `instruction`+`classification_floor*`
  an `ask` task does, so the **scheduler/runner is untouched**; unpaired peers + injection are
  dropped + audited (`channel.rejected_unpaired`/`channel.injection_blocked`, hash only, never the
  body). 18 unit tests + hermetic `FakeChannel` full-loop e2e green on this box; PG-gated
  `channel_bus_pg_e2e` skip-as-passes here (root container, no supervisor — runs live on DGX/Mac);
  clippy `-D warnings` clean. **Deferred to slice #2:** real `MatrixChannel` (E2E `matrix-rust-sdk`)
  + its sandboxed worker + `main.rs` wiring (daemon stays byte-identical this slice); slice #3
  pairing (TOTP/WebAuthn) replaces `StaticPairings` with a DB-backed authorizer; slice #6 conduwuit
  homeserver unit. Plan: `docs/superpowers/plans/2026-06-12-channel-bus-abstraction.md`.
- **2026-06-12 — primary communication channel DESIGN (docs-only, branch `claude/zen-bell-6bn2ze`):** operator brainstorm locked the user↔kastellan channel: **Matrix, self-hosted, single-user, federation OFF** (E2E via `matrix-rust-sdk`, vendor-neutral, zero marginal cost, all platforms) as primary; **email as the cross-transport low-trust fallback** (separate failure domain — Matrix has no single-user homeserver failover). Signal (`presage` fragility/ban-risk) + Telegram (no bot E2E, centralized) rejected as primary. Homeserver = supervised **conduwuit**, hosting tiers fail-down (A dedicated VPS preferred → B existing WireGuard VPS → C "poor man's" on the kastellan host); co-hosting blast-radius analysed (WireGuard/ingress + agent adjacency) with a systemd-hardening minimum bar. Channel-bus abstraction built first; inbound screened by `injection_guard`; pairing (TOTP/WebAuthn) sits above the bus; channel workers `Net::Allowlist`-scoped + egress-proxy-routed. Spec `docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`; ROADMAP Phase 2/3 + threat-model updated. No code.
- **2026-06-11 — egress proxy SLICE #2 Task 4.4 live auto-flip (ROADMAP:141, PR [#250](https://github.com/hherb/kastellan/pull/250) MERGED):** wired the merged force-routing mechanism into both cold-spawn sites behind the opt-in `KASTELLAN_EGRESS_FORCE_ROUTING` (default OFF ⇒ byte-identical legacy). New `core/src/worker_lifecycle/force_route.rs` (pure `policy_net_is_force_routable`/`resolve_force_routing`/`spawn_worker_maybe_forced` + env-glue `from_env`, fail-closed); `egress::net_worker::spawn_forced_net_worker` owns a per-worker scratch (RAII-cleaned via `EgressSidecar.scratch`); `main.rs` aborts startup if enabled-but-no-proxy-binary. +16 Mac tests (incl. a `/fixall` review-hardening pass: UDS path-length guard, proxy-bin discovery DI, leak-not-remove on the unreachable no-bundle arm). **DGX acceptance + flip-on completed 2026-06-11 (slice #2 COMPLETE — see this session's top block);** stale-scratch crash-sweep [#251](https://github.com/hherb/kastellan/issues/251) deferred.
- **2026-06-11 — egress proxy SLICE #2 force-routing MECHANISM (ROADMAP:141, PR #249 MERGED):** `web-common::ProxyConnectGet` (CONNECT-over-UDS, hyper+tokio-rustls/ring, end-to-end TLS) behind env-selected `make_get`; OS force-routing — bwrap `Net::Allowlist+proxy_uds` → private netns + UDS bind, Seatbelt deny-outbound-except-UDS (gating probe **confirms AF_INET denied** on the dev Mac) + additive `SandboxPolicy.proxy_uds`; allowlist port-scoping (closes [#241](https://github.com/hherb/kastellan/issues/241)); host-side `core::egress::spawn_net_worker` (sidecar-first fail-closed, 1:1 teardown). DGX kernel-barrier probe `sandbox/tests/linux_force_routing.rs` written (run on DGX).
- **2026-06-10 — egress proxy SLICE #2 DESIGN (spec + plan, PR #246 MERGED):** locked the transport (two `HttpGet` impls), Linux private-netns + UDS force-routing, macOS Seatbelt-deny-except-UDS with `MacosContainer` fallback, #241 fold-in, and the fail-closed host-side hookup; no code.
- **2026-06-10 — crates.io 0.1.0 published (PR [#245](https://github.com/hherb/kastellan/pull/245) MERGED, tag `v0.1.0` = `6f6f741`):** all 12 publishable crates live (`kastellan-tests-common` stays `publish=false`). Publish in dep order; *version updates* (not new-crate) have the higher rate limit, so future releases won't crawl.
- **2026-06-10 — rename hhagent → kastellan (PR #244 MERGED):** mechanical workspace rename (crates `kastellan-*`, paths `kastellan_*`, env `KASTELLAN_*`, file/dir renames; 389 files, 1491 tests green). One-time host fallout: PG db/role `kastellan`, keychain service `kastellan`, state dirs `~/.kastellan` + `~/.local/{share,state}/kastellan`, `/etc/kastellan/env`, systemd unit `kastellan-core`. `~/src/hhagent` kept as a compat symlink (registered worktrees).
- **2026-06-10 — egress proxy SLICE #1 boundary host-allowlist + SSRF/IP defense (ROADMAP:141, PR [#240](https://github.com/hherb/kastellan/pull/240) MERGED):** new `workers/egress-proxy` (sandboxed per-worker CONNECT proxy on a UDS — reuses `HostAllowlist`, self-resolves DNS, rejects private/loopback/link-local/ULA/CGNAT/multicast IPs, pins+dials, tunnels). `Net::ProxyEgress` variant; host side `core/src/egress`. Mechanism only — did not route real workers (that's slice #2). Filed #241/#242/#243.
- **2026-06-09 — planner `fetch_handoff` surfacing (ROADMAP:129, PR #200 MERGED):** `assemble_system_prompt` now emits an always-present, drift-proofed `<handoff>` block (`render_handoff_block()` interpolates the source-of-truth tool/method constants + byte caps) teaching the planner the placeholder shape + `fetch` protocol — the handoff cache is no longer inert.
- **2026-06-09 — injection-guard per-tool profiles (#142, PR [#239](https://github.com/hherb/kastellan/pull/239) MERGED):** `GuardProfile{Strict|Relaxed}` + `for_tool` (only web-fetch/web-search relax) + `screen_with_profile`; Relaxed caps the chat-template family at one 0.40 sub-threshold contribution so legit model-card fetches Allow but corroborated attacks Block. (Detailed in this session's header "Prior session".)
- **2026-06-09 — `web-search` worker + shared `web-common` crate (ROADMAP:146, PR [#238](https://github.com/hherb/kastellan/pull/238) MERGED):** second net worker (`web.search` → SearxNG JSON hits; operator-set `KASTELLAN_WEB_SEARCH_ENDPOINT`, http loopback-only). Extracted `workers/web-common` (`HostAllowlist` + `HttpGet`/`ReqwestGet`) as the single source of truth; web-fetch re-pointed byte-preserved.
- **2026-06-08 — large-tool-result handoff cache (ROADMAP:129, PR #199 MERGED):** in-memory per-task content-addressed `HandoffCache` (`core/src/handoff.rs`); `ToolHostStepDispatcher::dispatch_step` stashes oversized `Ok` results (>64 KiB, `task_id>0`) as a `{handoff_ref,…}` placeholder + audit row; reserved `handoff`/`fetch` built-in returns clamped slices (256 KiB). Per-task byte budget + `MAX_TRACKED_TASKS` backstop; purged at task terminal. Injection-blocked outputs never stashed.
- **2026-06-08 — `web-fetch` worker (ROADMAP:145, PR [#197](https://github.com/hherb/kastellan/pull/197) MERGED):** first net-egress worker (`web.fetch`, HTTPS-only, host-allowlisted self-enforced per redirect hop, `dom_smoothie`/`pdf-extract` extraction, 5 MiB/5-redirect caps). Host manifest `Net::Allowlist`+`WorkerNetClient`. Cross-cutting Landlock-RO fix (`KASTELLAN_LANDLOCK_RO` from `fs_read`) so DNS works in-jail. Full detail in `archive/`.
- **2026-06-07 — `insert_memory_light` two-tier write path (ROADMAP:130, PR [#195](https://github.com/hherb/kastellan/pull/195) MERGED at `4918b60`):** `db::memories::insert_memory_light(executor, body, metadata, layer)` — thin delegate to `insert_memory_at_layer` with `embedding = None`, no new SQL/migration, inherits the L0 `PolicyViolation` guard. Degradation contract: lexical + `metadata @>` work; semantic skips (`WHERE embedding IS NOT NULL`); graph never surfaces it. 2 PG e2e + 1 PG-free L0-guard unit test. Deferred: caller wiring; per-namespace caps; graph-lane degradation test ([#196](https://github.com/hherb/kastellan/issues/196)).
- **2026-06-07 — Option K: cross-platform exponential restart backoff (ROADMAP:61, PR [#194](https://github.com/hherb/kastellan/pull/194) MERGED):** `ServiceSpec.restart_backoff: Option<RestartBackoff{max_delay_sec,steps}>` (additive, `#[serde(default)]`, `None`=old constant-`RestartSec=5`). systemd emits `RestartSteps`/`RestartMaxDelaySec` (252+; older warns-but-loads); macOS launchd warns-and-ignores (no equivalent knob). core+postgres specs wired 5s→300s/8-step. Builder test modules lifted to siblings to stay under cap. Residual: `launchd_agents.rs` 508 LOC (+8, deferred per ≤27-over policy).
- **2026-06-07 — three clean test-lifts batch (item 9b-a, PR [#193](https://github.com/hherb/kastellan/pull/193) MERGED):** scripted byte-identity lifts of inline `mod tests` blocks — `cassandra/types.rs` 897→336, `scheduler/inner_loop_audit.rs` 655→304, `entity_extraction/gliner_relex.rs` 570→386. Residual: `cassandra/types/tests.rs` 568 (over-cap test file, bucket-c).
- **2026-06-07 — `macos_seatbelt.rs` test-lift (item 9b-a, PR [#192](https://github.com/hherb/kastellan/pull/192) MERGED):** inline `#[cfg(test)] mod tests` → sibling `macos_seatbelt/tests.rs`; parent 604 → 332 LOC, production byte-identical, 16 unit tests pass from the new location.
- **2026-06-06 — `systemd_user.rs` production split (item 9b-b, PR [#191](https://github.com/hherb/kastellan/pull/191) MERGED):** the most over-cap file (1069 LOC after the `kastellan.target` slice) → 427-LOC `systemctl --user` driver parent + `systemd_user/builder.rs` (478, pure builders+tests, re-exported via `pub use`) + `systemd_user/tests.rs` (216, driver tests); mirrors the `launchd_agents.rs` precedent. Behaviour-preserving (workspace 1327/0/4).
- **2026-06-06 — `gliner_relex.rs` production split (item 9b, PR [#189](https://github.com/hherb/kastellan/pull/189) MERGED):** 921-LOC monolith → 51-LOC re-export facade + five cohesive siblings (`wire`/`resolve`/`entry`/`client`/`manifest`, all under cap); public API byte-identical via `pub use`. Reconciled same session: `recall.rs` test-lift (PR [#188](https://github.com/hherb/kastellan/pull/188), 622→406). Residual: `workers/gliner_relex/tests.rs` 851 (bucket-c).
- **2026-06-05 — worker manifest plumbing (item 11, PR [#187](https://github.com/hherb/kastellan/pull/187) MERGED at `2e3d0c5`):** `trait WorkerManifest` + `Resolution` enum + `ResolveCtx` + pure `discover_binary` — each worker self-describes; `registry_build.rs` reduced to `assemble_registry(manifests, ctx)`. Plain workers resolve as a sibling of the `kastellan` binary (`current_exe()`-relative; `KASTELLAN_*_BIN` override wins, fail-closed if set-but-invalid; gliner exempt). Every produced `ToolEntry` byte-identical; containment shape stays compiled-in. Workspace 1311/0/4.
- **2026-06-05 — #179 Opt-3 daemon reroute of `memory l3 run` (PR [#186](https://github.com/hherb/kastellan/pull/186) at `67bc474`, #179 CLOSED):** `run` now enqueues an `l3_run` task the daemon executes against its single live `ToolRegistry` (the Postgres `tasks` queue + `LISTEN/NOTIFY` IS the operator→daemon command channel — `ask`'s second user, zero new IPC). New `scheduler/l3_run.rs`; `drain_lane` routing; CLI rewrite waits on `tasks_completed` with busy-vs-absent daemon detection (`tasks::any_live_worker`, pending-only cancel). Deleted the interim `diagnose_registry_divergence` (PR #180). TOCTOU re-validation now strictly stronger (live registry); all 7 security invariants PASS. Workspace 1297/0/4.
- **2026-06-04 — `capture.rs` test-lift + `secret_vault_e2e` `sun_path` fix (PR [#185](https://github.com/hherb/kastellan/pull/185) at `ef01ae3`):** clean over-cap test-lift → `observation/capture/tests.rs`; parent 715 → 373 LOC, production L1–371 byte-identical. Bundled: dropped the redundant doubled `{suffix}` from `secret_vault_e2e` data/log labels (108-byte `sun_path` overflow under the harness `TMPDIR`; #104 systemic sweep stays open). First DGX native-Linux verification in a while; toolchain bumped 1.95→1.96 to match CI; workspace 1290/0/4.
- **2026-06-04 — `l0_seed.rs` test-lift (PR [#183](https://github.com/hherb/kastellan/pull/183) at `305b927`):** clean over-cap test-lift → `l0_seed/tests.rs`; parent 730 → 462 LOC, behaviour-preserving (production L1–459 byte-identical; 19 unit tests pass from new location).
- **2026-06-04 — L3 over-cap file splits, the #181 follow-up (PR [#182](https://github.com/hherb/kastellan/pull/182) at `f695a46`):** production-split `l3_invoke.rs` (569 → 38-line facade + `pure`/`operator`/`agent` siblings) and `memory_l3.rs` (692 → 52-line dispatcher + per-subcommand siblings + `shared.rs` approve/pin DRY); all L3 files under the 500-LOC cap, behaviour-preserving (workspace 1319/0/3 unchanged; live PG L3 suites green).
- **2026-06-03 — #179 interim diagnostic, Approach C (PR [#180](https://github.com/hherb/kastellan/pull/180) at `fdfd0a8`):** pure `diagnose_registry_divergence` classifier + actionable CLI `hint:` for the `Refused` arm (since DELETED by this session's Opt-3 reroute). #179 re-scoped to the Opt-3 structural fix.
- **2026-06-03 — L3 operator-triggered invocation, "the DOOR" (PR [#178](https://github.com/hherb/kastellan/pull/178) at `d862e6e`):** `kastellan-cli memory l3 run <id>` executes an approved skill — substitute `{{params}}` → live `ToolRegistry` re-validation → sandboxed dispatch → audit; dry-run by default. Filed #179 (the registry-parity question this session resolved).
- **2026-06-04 — L3 autonomous door, agent-path (PR [#181](https://github.com/hherb/kastellan/pull/181) at `6e10a81`):** `Plan.invoke_skill` directive the inner loop expands (pinned-only; reuses `prepare_invocation` live re-validation; CASSANDRA review on the agent path) + the `pin` command (real `Pinned`-vs-`UserApproved`). Completes the L3 arc bar #179's IPC reroute.
- **2026-06-01 — L3 recall surfacing, the `<skills>` block (PR [#177](https://github.com/hherb/kastellan/pull/177) at `4b978d8`):** new `core/src/memory/l3_surface.rs` surfaces only `UserApproved`/`Pinned` skills to the planner (L0 → L1 → skills → recalled → base); `skill_count` threaded + audited. Surfacing-only, no invocation. Carries SQL trust push-down `load_layer_by_trust` (`a53b4bc`).
- **2026-05-31 — L3 skill trust enum + approval gate (PR [#176](https://github.com/hherb/kastellan/pull/176) at `bbcc7b3`):** `SkillTrust{Untrusted|UserApproved|Pinned}` (fail-safe parse); pure `evaluate_approval` (re-validate + `secret://` scan + tool-existence vs the `registry.loaded` snapshot, fail-closed); `set_skill_trust` db helper; `memory l3 {approve,revoke}` + audit rows. Trust flips → `user_approved` ONLY on `Approve`. No execution.
- **2026-05-31 — L3 skill crystallisation writer (PR [#173](https://github.com/hherb/kastellan/pull/173) at `6eb966e`):** first writer for `MemoryLayer::Skill` (L3) — agent `Plan.l3_skill` → validate → canonical-SHA-256 dedup → `layer=3 trust:"untrusted"`; `dispatch_count >= 1` grounding gate; `memory l3 {list,remove}`. New `core/src/memory/l3_crystallise.rs`.
- **2026-05-30/31 — refactor + CI batch** (PRs #161–#175): file-splits/test-lifts (`db/memories`, `tool_dispatch`, `launchd_agents`, `scheduler/audit`, `macos_container`, `replay`, `inner_loop`, `l3_crystallise`) under the 500-LOC cap; #99 CLI `with_runtime`; #153 clippy `-D warnings` gate; #130/#163 launchd serialization. Detail in git / archive.
- **2026-05-29 — security slices + refactor batch** (PRs #146–#160): ★ opaque secret references (`SecretRef` + Vault, #146) + worker-output prompt-injection guard (#141) + `walk()` depth-guard/sibling-continue + Linux build/clippy gate (#144/#150) + several test-lifts. Full detail in [`archive/handover_20260605_pre-prune.md`](archive/handover_20260605_pre-prune.md).
- **2026-05-06 → 2026-05-28 — Phase 0 + Phase 1 build-out** (PRs #38–#140): sandbox core (Landlock+seccomp prelude, Seatbelt, bwrap, shell-exec, cgroup caps), Linux/macOS supervisors, scheduler online + CASSANDRA, recall lanes + L0/L1 memory, entity extraction v2 + GLiNER-Relex, worker-lifecycle managers, macOS Apple-`container` backend, observation capture. Full detail in the [`archive/`](archive/) `20260510` / `20260529` snapshots.

---

## Key design decisions locked in

- **Vendor-neutral, AGPL-compatible deps only.** AGPL project; all third-party deps must be AGPL-compatible (Apache-2.0, MIT, BSD, MPL, LGPL, (A)GPL all fine).
- **Cross-platform first-class.** Linux (DGX Spark primary) + macOS (Apple Silicon and Intel). No Linux-only code without a macOS counterpart of equivalent guarantee.
- **Rust core, Python workers.** Rust for core (no eval/dynamic surface); Python only inside sandboxed tool workers. shell-exec is Rust because it's a thin execve wrapper — Python's first appearance will be `python-exec` in Phase 4 (or possibly `web-fetch` earlier).
- **Hybrid LLM with policy routing.** Local-first via OpenAI-compatible HTTP (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS). Frontier (Claude/OpenAI) only via the Phase-5 policy gate, through the egress proxy.
- **Single-host deployment via OS-native user-level supervisor.** `systemd --user` (Linux) / `launchd` LaunchAgents (macOS). No k3s.
- **Fixed core tools, sandbox-bound agent-authored Python.** Critical workers are human-curated and shipped with the binary. Agent-authored code only runs inside `python-exec`'s strict sandbox; named/persisted skills get an optional human-approve gate (the L3 skill arc).
- **JSON-RPC 2.0 over stdio.** MCP-stdio compatible. Lets us swap in a richer MCP client later without changing the trust boundary.
- **Operator→daemon command channel = the Postgres `tasks` queue + `LISTEN/NOTIFY`** (not a new IPC socket). `ask` and `memory l3 run` both ride it; daemon-side execution against the single live `ToolRegistry` is the canonical pattern (#179 Opt-3).

---

## Next TODO (pick one)

Phase 0 is complete; Phase 1 is on `main` and pinned by `cli_ask_e2e`. **The L3 invocation arc is COMPLETE on `main`** (PR #186, #179 CLOSED). **`web-fetch` (ROADMAP:145) / `web-search` (ROADMAP:146) workers + injection-guard per-tool profiles (#142) all MERGED.** **Egress proxy is now ALL 4 SLICES COMPLETE** (#1 boundary/SSRF PR #240, #2 force-routing PR #256, #3a MITM PR #259, #3b leak scanner PR #269, #4 TLS pinning this branch). The list below is an **operator-picks bucket** — sized roughly one session each, with file paths and the verification step.

**#281 is FULLY CLOSED** — both pure-Python venv workers now have worker-side seccomp **+ Landlock** on Linux via the lockdown-exec shim: browser-driver (`browser_client` seccomp PR #292 + Landlock PR #294, both on `main`) and gliner-relex (`ml_client` seccomp PR #293 + Landlock this branch). **`browser-driver` is also egress-proxy-routed (slice #2, PR #285), renders under Seatbelt on macOS (#284), macOS forced path green (#287).** Leading remaining picks: **MITM-of-browser** (in-Chromium CA trust via NSS — deferred slice #2 follow-up, once leak-scanning #3b is wired); the **egress follow-ups** below; **python-exec Phase-4 continuation** (top pick below); or Phase-2 channels (IMAP/Telegram inbound) as the next phase boundary.

**Egress follow-ups now that the proxy is feature-complete (each small, on demand):** ~~(1) slice #4 operator pin config~~ — **DONE 2026-06-18** (branch `feat/egress-operator-cert-pins`, PR [#303](https://github.com/hherb/kastellan/pull/303)): force-routed tool workers now enforce operator-configured cert pins (`KASTELLAN_EGRESS_CERT_PINS`, fail-closed, per-worker least-privilege selection by allowlist host; `core/src/egress/cert_pins.rs` + `force_route.rs`). **What's left for the frontier path is Phase-5, NOT pin config:** frontier LLM egress doesn't exist yet (`Router::send` denies all frontier calls + runs in-core via reqwest, not a sidecar), so "route frontier egress through a **pinned** sidecar" needs the whole Phase-5 escalation path (Router-behind-a-sidecar + a real PolicyGate + frontier API key from `db::secrets`) first; the pin plumbing is then ready to serve it (the operator just adds the frontier host to `KASTELLAN_EGRESS_CERT_PINS`). **Tracked in [#304](https://github.com/hherb/kastellan/issues/304):** a real-sandbox cert-pin enforcement e2e (a force-routed worker dials a pin-mismatching host → blocked with `tls_pin`/`pin_mismatch`; needs a controllable TLS origin; no frontier consumer yet to justify it). ~~(2) slice #3b dispatch-time live-append ([#268])~~ — **DONE 2026-06-17** (this session, branch `feat/268-egress-dispatch-time-provisioning`): `tool_host::dispatch` now provisions each materialized secret's fingerprint into the force-routed worker's sidecar `secret_hashes.json` before egress (fail-closed, union, `ref_hash`-keyed audit). Activates with the first secret-bearing egress worker. (The spawn-time `secret_fingerprints` field stays `&[]`; the live path is the dispatch hook.)

**Matrix Phase D live `LiveSdk` is DONE + DGX-verified this session** (see the header up top) — `sdk_live.rs` + worker
`main.rs` live serving + core `disable_mitm_for` + the `#[ignore]` `matrix_live_e2e.rs`; hermetically green on macOS,
**and the live encrypted round-trip passes on the DGX** (aarch64 build + 13/0 hermetic + 1/0 live e2e, 0 shutdown aborts
after the deadpool `Drop` fix). The Matrix follow-ups below are the natural continuation. (DGX live-loop recipe, if you
need to re-run it: `scripts/matrix/dev-e2e-bootstrap.sh up` — a throwaway loopback `matrix-conduit` container + curl bootstrap
of two accounts + an encrypted room; `source ~/.matrix-e2e.env` then the `#[ignore]` e2e; `… down` to tear down. Runs on the
DGX via `ssh dgx 'bash -s up' < scripts/matrix/dev-e2e-bootstrap.sh`. Documented in `docs/deploy/matrix-homeserver.md`.)

**★ TOP PICK — channel-worker egress-coupled production spawn (plan Task 5) + daemon wiring.** This is what makes the live
Matrix channel actually run in the daemon. Today `core/src/channel/matrix.rs` has the driver + pure `build_matrix_policy`
but **no production spawn of the matrix worker with a real egress sidecar** (the `disable_mitm_for("matrix")` decision is
wired + ready but nothing routes the channel worker through `spawn_worker_maybe_forced` yet). Build: the long-lived
channel-worker spawn (sandbox + per-worker egress sidecar in transparent-tunnel mode + persistent store + restart
supervision), then `core/src/channel/matrix.rs::from_env` + the `main.rs` `ChannelBus` wiring (plan Tasks 5–6), and swap
`StaticPairings`→`DbPeerAuthorizer` + pass `DbPairingService` (slice #3 deferrals). **Carry the
[#286](https://github.com/hherb/kastellan/issues/286) macOS-loopback caveat:** the `ProxyBridge` binds `127.0.0.1:0`
inside the worker (same pattern as browser-driver's `shim.py`); when this spawn grants the matrix worker a loopback-widening
Seatbelt profile on macOS, scope the grant to the bridge's bound port (or prefer a UDS-only transport / the `MacosContainer`
VM-netns backend). Also [#312](https://github.com/hherb/kastellan/issues/312): make `ProxyBridge` surface accept/relay
errors instead of silently dropping (must not ship under live traffic). Plan: `docs/superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md` Tasks 5–6.

**Phase 4 continuation (`python-exec` arc, now on `main`).** `python-exec` slice #1 shipped
(PR [#267](https://github.com/hherb/kastellan/pull/267)); **acceptance is GREEN on BOTH platforms** (2026-06-13, PR
[#270](https://github.com/hherb/kastellan/pull/270): Mac Seatbelt 3/3 + DGX bwrap 3/3, no skips). The Phase-4 sequence
continues:
1. **Operator flip (no code):** set `KASTELLAN_PYTHON_EXEC_ENABLE=1` wherever the worker is wanted — it is opt-in and
   unregistered by default. Whether the supervised deployment (`core_service_spec`) should carry it by default is an
   operator decision; the deliberate slice-#1 posture is OFF.
2. **Skill catalog arc is functionally complete + MERGED:** crystallise/approve/pin (slice 1 `0cbddc5`) + invoke/surface
   (slice 2 `e478309`) + runtime params (env-var channel, 64 KiB, free-form, secret-aware; `02ccb57`). The priority (b)
   refactor — splitting `core/src/scheduler/inner_loop.rs` (630 → 481 LOC) — is DONE (`inner_loop/invoke_expand.rs` +
   `inner_loop/floor.rs`). **(a) battle-test the params free-form passthrough — DONE 2026-06-17** (this session, branch
   `feat/python-exec-output-secret-scrub`): the risk found + closed was the secret-in-param → python-exec output → audit/CLI
   leak; output is now scrubbed of this-dispatch's materialized-secret fingerprints (`leak_scan::redact` + `tool_host/secret_scrub.rs`),
   python-exec-only, no-op elsewhere. See "Last updated" up top. **(c) real-secret scrub e2e — DONE in-process 2026-06-17**
   (this session, branch `feat/python-exec-scrub-inprocess-e2e`): `python_exec_e2e::materialized_secret_param_is_scrubbed_from_output`
   proves the scrub end-to-end through the real worker + real jail + real Vault + real `dispatch`; the full **daemon** e2e
   (CLI→scheduler→l3py routing, which never touches the scrub) is deferred to [#298](https://github.com/hherb/kastellan/issues/298)
   (needs a security-sensitive Vault-ref test seam in `main.rs`). **(b) `cli_memory_l3py_run_daemon_e2e` test-lift —
   DONE 2026-06-18** (PR [#306](https://github.com/hherb/kastellan/pull/306)): shared daemon bring-up + inert mock LLM + CLI-output
   asserts + `cli_command` builder hoisted into `tests-common` (`daemon.rs` + `binaries.rs`), consumed by **both** daemon
   e2e files (l3py 838 → 499, l3 480 → 296); python-specific `find_python` + skill factories stay local (`tests-common`
   is deliberately core-free). See "Last updated" up top.
3. **python-exec worker slice-#2 candidates (on demand):** ~~macOS writable scratch~~ — **DONE 2026-06-18** (branch
   `feat/python-exec-macos-perspawn-scratch`, PR [#307](https://github.com/hherb/kastellan/pull/307)): a reusable per-spawn scratch mechanism (`ToolEntry.ephemeral_scratch`
   → `tool_host/scratch.rs::prepare_ephemeral_scratch` → host dir + Seatbelt `fs_write` grant + `KASTELLAN_WORKER_SCRATCH` +
   RAII `SupervisedWorker.scratch`); macOS now has a per-spawn isolated writable scratch, Linux byte-identical. See "Last
   updated". ~~the **scratch-file param channel** for >64 KiB payloads~~ — **DONE 2026-06-19** (branch
   `feat/python-exec-scratch-file-params`, this session; see "Last updated" up top — worker writes `<scratch>/params.json`
   for >64 KiB params, configurable `KASTELLAN_PYTHON_PARAMS_FILE_MAX`, verified macOS + DGX). Remaining: curated-wheels RO
   dir if skills demand packages. ~~browser-driver adopting `ephemeral_scratch`~~ — **DONE
   2026-06-18, #283 FULLY CLOSED** (branch `feat/browser-driver-perspawn-scratch`; see "Last updated" up top): `browser_driver_entry`
   now sets `ephemeral_scratch: true` + `fs_write` empty on both OSes, the worker's `_apply_worker_scratch` redirects
   TMPDIR/HOME to the per-spawn dir, e2e 4/4 on macOS Seatbelt. **Other Phase-4 picks:** micro-VM backend (ROADMAP), tiered delegation policy (ROADMAP).

**Egress deferrals carried forward:** [#242](https://github.com/hherb/kastellan/issues/242) tunnel idle/resolve timeouts;
[#251](https://github.com/hherb/kastellan/issues/251) stale-scratch crash-sweep (needs cross-platform pid-liveness);
transparent gzip/brotli if an origin refuses `Accept-Encoding: identity`; the `pg_decision_sink` back-pressure decoupling
(bounded channel + async writer) before high-rate production load. **Slice #3a review follow-ups (PR #259, addressed
2026-06-12):** `peek_first_byte` now **retries on `EINTR`** rather than downgrading a TLS flow to pass-through (the
silent-interception-escape hole is closed — matters for 3b's scanner); `mitm::intercept`'s upstream re-dial is now
bounded by `ORIGIN_CONNECT_TIMEOUT` (10s, mirrors `proxy::CONNECT_TIMEOUT`); the 200-write-fail path now still emits an
`allowed_but_200_write_failed` audit decision (restores slice #1's always-log-an-allowed-Dial invariant); the
`LeafCache` is hoisted to proxy lifetime (was per-connection); redundant `webpki-roots` dev-dep dropped. **Slice #3a
minor deferrals still open:** the MITM path re-dials the origin inside `intercept` (one extra connect; the sync pre-200
connect only proves reachability — a later opt can thread the converted tokio stream through); the `copy_bidirectional`
relay + the blocking `peek_first_byte` still lack **read** idle-deadlines (folded into
[#242](https://github.com/hherb/kastellan/issues/242)); literal-IP **HTTPS** origins now require an IP-SAN cert under
MITM upstream validation (behaviour-change decision — needs a tracking issue; see PR #259 review).

**`browser-driver` Phase 2 + egress slice #2 are DONE; #263 + #280 CLOSED.** It renders under the real jail (Phase 2, PR
#282) and is egress-proxy-routed in the default force-routed deployment (slice #2, this session — transparent tunnel +
in-jail loopback shim; see the top block). Remaining browser-driver picks:
- **★ MITM-of-browser (deferred slice-#2 follow-up):** in-Chromium trust of the per-instance proxy CA via a proper **NSS
  trust-store import** (not the `--ignore-certificate-errors-*` error-suppression flag), so the sidecar can content/leak-scan
  browser egress. Do this only once leak-scanning (#3b) is actually wired — it trades away Chromium-grade origin validation +
  enlarges the sidecar blast radius, so it needs a concrete inspection benefit to justify.
- ~~**[#287] — macOS forced (egress-sidecar) render emits no decisions**~~ — **RESOLVED 2026-06-15** (this session): it was a
  stale browser-driver venv, not a code bug. All 4 `browser_driver_e2e --ignored` tests (incl. both forced ones) now pass on
  macOS once the venv is re-staged from current source; `install.sh` now `--force-reinstall`s to prevent recurrence.
- ~~**[#281](https://github.com/hherb/kastellan/issues/281) — pure-Python Linux seccomp + Landlock**~~ — **FULLY CLOSED.**
  Both workers run worker-side seccomp + Landlock on Linux via the lockdown-exec shim: browser-driver (`browser_client`
  seccomp PR #292 + Landlock PR #294, on `main`) and gliner-relex (`ml_client` seccomp PR #293 + Landlock this branch).
  Neither worker sets `KASTELLAN_LANDLOCK_PROFILE=none` any longer.
- **Phase-2 hardening (on demand):** narrow the Seatbelt `mach-lookup`/`sysctl-write`/`system-socket` grants to specific
  services; ~~a true per-spawn scratch (vs the shared `/tmp`) on macOS (#283)~~ **DONE 2026-06-18 (#283 closed)**; screenshot output; warm-keep lifecycle.

Operator note: `scripts/workers/browser-driver/install.sh` stages the venv + Chromium; `KASTELLAN_BROWSER_DRIVER_ENABLE=1`
to register; on a host whose interpreter pulls libs outside its prefix (e.g. a pyenv CPython linking `/opt/homebrew`), set
`KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ='["/opt/homebrew"]'`. (Egress slice #3b dispatch-time provisioning [#268] is now DONE — see "Recently completed" above.)

**Natural web-search follow-ups** (cheap, on demand): stand up a local SearxNG with `scripts/web-search/setup-searxng.sh`, set `KASTELLAN_WEB_SEARCH_ENDPOINT` + the `web-search` `tool_allowlists` row, and run the `#[ignore]` `core/tests/web_search_e2e.rs::real_search_against_searxng` to validate the real round-trip end to end. If/when a caller needs them: category/language/engine params or pagination on `web.search` (deferred per spec).

**Remaining handoff-cache follow-ups (ROADMAP:129)** — the cache (PR #199) and the planner-surfacing
(PR #200, this session) are both done; the mechanism is now live and known to the planner. Still open:
- **On-disk Workspace-backed store** — only once a per-task `Workspace` is actually wired into the live
  scheduler flow (it isn't today); the `HandoffCache` surface can take a disk impl behind it then.
- **Observe it in practice** — once a worker reliably returns >64 KiB (e.g. `web-fetch` on a large page),
  confirm the planner expands a stash via the `<handoff>` instruction in a real `cli_ask`-style run; if the
  prompt wording needs tuning, that's a cheap iteration on `render_handoff_block()`. (Optional / on demand.)

**Other Phase-3 natural picks:** the egress proxy is feature-complete (all 4 slices), so `browser-driver` Phase 2 is the
leading Phase-3 pick above. Beyond that, Phase-2 channels (IMAP/Telegram inbound) are the next phase boundary.

**Older follow-ups (ROADMAP:130, still open):** core-side caller wiring for `insert_memory_light` (lands with the first high-frequency writer — Phase 2 channels / Phase 3 browser); per-namespace caps + oldest-eviction on `memories.metadata` (no schema change); a graph-lane degradation test ([#196](https://github.com/hherb/kastellan/issues/196)).

**Refactor bucket — over-cap file splits (item 9b).** Re-census the exact split (`wc -l`) before picking — the numbers below drift each session:

- **(a) Clean test-lifts** (lifting the inline `mod tests` block alone lands the parent under cap): **none meaningfully remaining.** The substantial ones are done — `cassandra/types.rs`, `inner_loop_audit.rs`, `entity_extraction/gliner_relex.rs` (2026-06-07 batch); `macos_seatbelt.rs` (PR #192); `recall.rs`/`l0_seed.rs`/`capture.rs`/`inner_loop.rs`/`replay.rs` (Earlier history). A fresh census shows only files sitting **1–27 LOC over cap** still carry a liftable block (`core/src/main.rs` 527, `db/src/lib.rs` 525, `core/src/bin/kastellan-cli/memory_l3/run.rs` 519, `core/src/cassandra/constitutional.rs` 502, `core/src/memory/l1_promote.rs` 501) — a lift would save little; defer unless one grows. **`core/src/tool_host.rs` is now 627** (584 on `main` before #268; +~25 #268 dispatch hook, +16 the secret-scrub wiring — bulk kept out in `tool_host/egress_provision.rs` + `tool_host/secret_scrub.rs`). A real prod-split of `tool_host.rs` (its tests already live under `tool_host/`) is the leading over-cap candidate now — needs a seam (e.g. lift `dispatch_with_sink`'s `match call_result` post-processing — scrub + injection screen + audit-emission arms — into a `tool_host/post_process.rs` sibling).
- **(b) Need a real prod split or a re-exported pure-helper seam** (a test-lift alone leaves the parent over cap): `core/src/cli_audit.rs` (958, the most over-cap production file), `db/graph.rs` (926, the design-gated Item 23b walk-impl split — deferred until a 2nd `WalkedEdge` consumer materialises), `core/src/scheduler/runner.rs` (777), `core/src/scheduler/audit.rs` (701, tests already lifted), `db/src/entities.rs` (653), `workers/prelude/src/seccomp_lock.rs` (650). (`core/src/scheduler/inner_loop.rs` is **DONE** — split 630 → 481 this session via `inner_loop/invoke_expand.rs` [the `invoke_skill` expansion returning an `InvokeExpansion` enum] + `inner_loop/floor.rs` [`ClassificationFloorSource` + `apply_floor_raise`, re-exported]. `db/secrets.rs` [848 → 252 + crypto/key_provider/error siblings], `systemd_user.rs`, `gliner_relex.rs` also done — see history.) Most over-cap production file remains `core/src/cli_audit.rs` (958).
  Also `supervisor/src/launchd_agents.rs` (508, +8) — pushed over by Option K's install-time warn; tests already external, so a fix needs a real prod-split (disproportionate for 8 lines; deferred per this same policy). And `core/src/scheduler/tool_dispatch.rs` (507, +7) — pushed over by the handoff stash + `fetch_handoff` intercept; tests already external (`tool_dispatch/tests.rs`), so deferred per the same ≤27-over policy (a clean split would lift the `fetch_handoff` intercept + stash path into a `handoff_dispatch.rs` sibling if it grows).
- **(c) Over-cap *test* files** (lower priority — not production code, but rule 4 still applies): `core/src/workers/gliner_relex/tests.rs` (851), `core/src/cassandra/types/tests.rs` (568).

**Engineering pickups (need a spec/design first):**

- The egress proxy (ROADMAP:141) and `browser-driver` (ROADMAP:147) above both need a spec/design first.

**Test-infra / smaller picks:**

- **[#134](https://github.com/hherb/kastellan/issues/134)** — revise the `bring_up_pg_cluster` doc example or ship a real `_with_timeout` caller.
- **[#104](https://github.com/hherb/kastellan/issues/104)** — systemic de-doubling of the `pid+nanos` tempdir suffix across all e2e callers (the `secret_vault_e2e` instance was fixed last session; this tracks the broader sweep).
- **`KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1` CI knob** — turn the container e2e's skip-as-pass into a hard fail for any runner with PG + container + image + weights staged.

**Operator actions (no code):** recapture observation fixtures against the current daemon (`cargo test -p kastellan-core --test observation_capture -- --ignored --nocapture`); real-model relation-extraction validation (`KASTELLAN_GLINER_RELEX_ENABLE=1 cargo test … entity_extraction_e2e`).

---

## Design notes for parked work

### Option P — entity↔memory linkage + graph lane (Phase 1 cont.)

The `memory_entities` join table (P1) shipped; the graph lane is wired into `recall` and the **production caller wiring is DONE** (2026-05-19 Slice F, PR #91): `RouterAgent::formulate_plan` populates `seed_entity_ids` from `entity_extractor.extract(&ctx.instruction)` each iteration; `main.rs` wires the real `GlinerRelexExtractor`. For a query carrying `seed_entity_ids`, the lane traverses outbound 1-hop then `SELECT memory_id FROM memory_entities WHERE entity_id = ANY($1)` ranked by neighbour count. **Remaining parked work is the quarantine review gate, not the wiring:** freshly-extracted entities default `quarantine=TRUE` and `graph_search` filters `quarantine=FALSE`, so seed entities surface no memories until an operator un-quarantines them ([#40](https://github.com/hherb/kastellan/issues/40) tracks the graph-default policy question). Secondary deferral: `entities.embedding` is NULL for all entities; a populated column would seed an entity-similarity lane (the `vector(1024)` column already exists).

---

## Open follow-up issues (filed but not picked)

Only currently-open issues are listed; closed-issue detail lives in the archive snapshots and git history.

- ~~[#287](https://github.com/hherb/kastellan/issues/287)~~ — **RESOLVED 2026-06-15** (PR `fix/287-browser-driver-stale-venv`): the macOS forced egress-sidecar "no decisions" was a **stale browser-driver venv** (a pre-slice-#2 install with no shim / no `--proxy-server`), not a code bug — fixed `install.sh` to `--force-reinstall` the local package so re-runs always stage current source. All 4 `browser_driver_e2e --ignored` tests pass on macOS.
- [#298](https://github.com/hherb/kastellan/issues/298) — full-DAEMON python-exec output secret-scrub e2e: the in-process scrub e2e is done (`python_exec_e2e::materialized_secret_param_is_scrubbed_from_output`); driving the whole CLI→scheduler→l3py→dispatch chain needs a security-sensitive Vault-ref test seam in `main.rs` (the `secret://` ref is minted randomly + never logged, so the separate CLI process can't pass a working ref). Design-first.
- [#286](https://github.com/hherb/kastellan/issues/286) — browser-driver Seatbelt `localhost:*` loopback widening is host-shared on macOS (no netns), so a compromised browser worker could reach host-local services bypassing the egress sidecar. Latent (Chromium is proxy-routed; the macOS forced egress path itself doesn't complete yet — #287). Fix: scope the rule to the shim's bound port, a UDS-only transport, or the `MacosContainer` VM-netns backend.
- [#3](https://github.com/hherb/kastellan/issues/3) — drop `SYS_SENDFILE`/`SYS_FADVISE64` shim once libc exposes them on aarch64.
- [#4](https://github.com/hherb/kastellan/issues/4) — bump Last-commit + test-count fields whenever a Recently-completed entry is added (process hygiene).
- [#8](https://github.com/hherb/kastellan/issues/8) — collapse `default_probe`/`default_supervisor` cfg-ladder duplication once a third entry point or backend OS appears.
- [#13](https://github.com/hherb/kastellan/issues/13) — write a migration numbering / rename hygiene checklist (sqlx fingerprints version+slug; a rename on a shipped migration silently breaks startup).
- [#14](https://github.com/hherb/kastellan/issues/14) — replace the brittle `wait_for_log_match("database probe succeeded")` in `supervisor_e2e.rs` with a real readiness signal.
- [#20](https://github.com/hherb/kastellan/issues/20) — `agent_prompts` PK on sha256 means renamed prompt files lose their original name *(0011 changed the PK to `(sha256, name)`; tracks any residual)*.
- [#21](https://github.com/hherb/kastellan/issues/21) — scheduler per-iteration cancellation poll could be a `watch::Receiver` instead of a DB round-trip.
- [#24](https://github.com/hherb/kastellan/issues/24) — deployment: `KASTELLAN_PROMPTS_DIR` has a cwd-relative fallback; production unit files must set it explicitly.
- [#37](https://github.com/hherb/kastellan/issues/37) — scheduler crash-recovery sweep+audit is unoptimised for high crash counts.
- [#39](https://github.com/hherb/kastellan/issues/39) — tests-common optional hardening (PgCluster.sup access, internal self-tests).
- [#40](https://github.com/hherb/kastellan/issues/40) — design: should `RecallParams::new()` default to graph-off until an entity-extraction step lands? *(partially addressed by `with_seeds`.)*
- [#42](https://github.com/hherb/kastellan/issues/42) — `deleted_memories` AFTER DELETE trigger uses `SECURITY INVOKER`; deferred until a second DELETE-capable role is proposed.
- [#47](https://github.com/hherb/kastellan/issues/47) — observation/capture: distinguish 'no verdict row' from a real Approve verdict *(SCHEMA_VERSION 2 made `verdict_today` Optional; tracks residual.)*
- [#50](https://github.com/hherb/kastellan/issues/50) — unify finalize-payload provenance signal across crashed/producer-cancelled/runtime emitters.
- [#55](https://github.com/hherb/kastellan/issues/55) — macOS Apple `container` micro-VM backend *(spike + Slices 1/2/2.5 shipped; tracks the broader rollout.)*
- [#62](https://github.com/hherb/kastellan/issues/62) — audit-payload truncation can silently nuke `agent/plan.formulate` fields.
- [#63](https://github.com/hherb/kastellan/issues/63) — e2e gap: classification_floor plumbing from `tasks.payload` to the `agent/plan.formulate` audit row.
- [#73](https://github.com/hherb/kastellan/issues/73) — scheduler/runner e2e integration test + TaskContext-construction reminder for producer-side floor-source validation.
- [#76](https://github.com/hherb/kastellan/issues/76) — prompt-assembly: verify PromptAssembly error retry semantics in scheduler.
- [#78](https://github.com/hherb/kastellan/issues/78) — prompt-assembly: global token cap with priority drop for the assembled system prompt.
- [#104](https://github.com/hherb/kastellan/issues/104) — audit the pid+nanos tempdir pattern across the workspace (follow-up to #101; `secret_vault_e2e` instance fixed 2026-06-04).
- [#107](https://github.com/hherb/kastellan/issues/107) — `MacosContainer` PID-1 signal-handling posture *(closed in code by always-on `--init`; verify end-to-end before long-lived workers migrate).*
- [#127](https://github.com/hherb/kastellan/issues/127) — env-var save/restore RAII helper for the `pg_bin_dir_candidates_with_env_override` tests.
- [#134](https://github.com/hherb/kastellan/issues/134) — tests-common: revise `bring_up_pg_cluster` doc example or ship a real `_with_timeout` caller.

---

## Open questions parked for later

(From the design plan, restated here so they're surfaced when relevant.)

1. Embedding model on-device — bge-m3 vs nomic-embed-text vs ColBERT (Phase 1)
2. ~~Channel approval — passcode pairing vs static contact allowlist (Phase 2)~~ **Resolved 2026-05-06:** pairing flow with WebAuthn-or-OTP fallback, modeled on ZeroClaw's `security/{pairing,webauthn,otp}.rs`.
3. ~~Egress proxy as separate worker vs in-process in `tool_host`~~ **Resolved 2026-05-06:** separate worker, with the credential-leak scanner co-located.
4. Skill review workflow for *named* agent-authored Python (Phase 4) — see Phase 4 line items: trust enum + per-level capability ceiling. *(The L3 skill arc — crystallise → approve → pin → invoke — is the first concrete implementation of this for templated tool-call skills.)*
5. Worker keep-alive vs spawn-per-call (idle-timeout lifecycle shipped for GLiNER-Relex; revisit for other workers when latency matters).
6. ~~Worker binary discovery in production~~ **Advanced 2026-06-05 (item 11):** plain compiled workers default to a sibling of the `kastellan` binary (`current_exe()`-relative; `KASTELLAN_*_BIN` override wins; gliner exempt — keeps venv/weights env resolution). Residual: FHS `libexec` layout if/when packaging wants it.

## Inspirations / things to read before each milestone

Two adjacent OpenClaw-derived projects ship code we can read (Apache-2.0/MIT, AGPL-compatible) before each new milestone — convergent prior art saves design time:

- **ZeroClaw** ([`zeroclaw-labs/zeroclaw`](https://github.com/zeroclaw-labs/zeroclaw), 100% Rust): read [`crates/zeroclaw-runtime/src/security/`](https://github.com/zeroclaw-labs/zeroclaw/tree/main/crates/zeroclaw-runtime/src/security) — has working `bubblewrap.rs`, `landlock.rs`, `seatbelt.rs`, `firejail.rs`, `pairing.rs`, `webauthn.rs`, `leak_detector.rs`, `workspace_boundary.rs`. Architectural drawback vs us: tools run as in-process Rust traits, OS sandbox wraps the runtime — weaker boundary than our process-per-worker. Don't copy the in-process tool model.
- **IronClaw** ([`nearai/ironclaw`](https://github.com/nearai/ironclaw)): read its dispatcher chokepoint pattern (`ToolDispatcher::dispatch()` is the single audit/safety-validation funnel for *every* action, regardless of caller). Drawbacks: WASM-as-boundary is software-only containment; Postgres+libSQL dual backend is overkill at our stage.

The *defining* architectural difference: kastellan enforces **one OS process + one bwrap/Seatbelt jail per worker**. Both reference projects retreated from that. Don't.

---

## How to update this document at session end

**Header first, prose last.** The header is what the next session reads first
and treats as authoritative; stale header fields silently mislead future
sessions even when the prose is correct. Follow the steps in this order:

1. **Bump header fields at the top — before writing any prose:**
   - `Last updated:` → today's date.
   - **Current state / Last commit** → the hash of the most recent shipped commit. Confirm with `git log --oneline -1`.
   - `Session-end verification:` → re-run `cargo test --workspace` and copy the **passed / failed / ignored / `[SKIP]`** counts into this line.
   - **Every test-count number embedded elsewhere in the doc that changed this session** — a fresh agent grep-finds them and will trust whatever is there.
2. **Move "Next TODO" → "Recently completed (this session)"** if the picked option shipped, with enough detail (file paths, why-not-X, gotchas, test-count delta) that the next session can start cold.
3. **Write a fresh "Next TODO (pick one)"** with options sized for one session each — include file paths, gotchas, and the verification step.
4. **Refresh "Working state"** — anything new under stubs, anything that became real.
5. **Tick the matching items off in [`../ROADMAP.md`](../ROADMAP.md)** with the commit hash.
6. **Commit both files together** with a `docs(handover): ...` message.
7. **If a milestone shipped:** does `site/roadmap.html` (timeline + "Last
   updated" stamp, and the landing-page status numbers) need a one-line
   update? See `site/README.md`.

### Pruning convention

The handover should stay focused on **what the next session needs to act on**: the current state, the last 2–3 sessions in detail, and the next TODO. Older session entries get compressed into the "Earlier history" summary or dropped entirely once they're no longer load-bearing.

When HANDOVER.md grows past the point where the next session can absorb it cold (rough rule of thumb: more than a couple of screens of "Recently completed"), prune it:

1. **Snapshot first.** Copy the current HANDOVER.md to `archive/handover_<YYYYMMDD>[_<slug>].md` (e.g. `handover_20260605_pre-prune.md`). The archive is the audit trail — never edited after the fact, never deleted.
2. **Keep verbatim:** the header, "Read these first," "Working state" (current truth), the most recent 1–2 sessions of "Recently completed," "Key design decisions," "Next TODO," "Open follow-up issues," "Open questions," "Inspirations," and this section.
3. **Compress everything else** into a single "Earlier history" section: one bullet per session, naming the slice + the headline change + a pointer to the archive snapshot for full reasoning.
4. **Cross-link** from the compressed bullets to the archive snapshot so anyone who needs the full reasoning can find it.
5. **Commit the prune separately** with `docs(handover): prune older sessions, archive pre-prune snapshot` so the diff is reviewable.

The archive directory is the historical record; HANDOVER.md is the working brief.
