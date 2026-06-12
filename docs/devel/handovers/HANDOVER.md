# kastellan — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention. Older sessions are compressed
> into "Earlier history" below; full per-session detail lives in the
> [`archive/`](archive/) snapshots.

**Last updated:** 2026-06-13 (**Phase 4: Mac (Seatbelt) acceptance of `python_exec_e2e` GREEN 3/3** — required two real
fixes, the macOS xcrun-shim interpreter bug + the `unique_suffix` collision class; branch `claude/vigorous-feistel-b0c811`.
PR [#267](https://github.com/hherb/kastellan/pull/267) (python-exec slice #1) is **MERGED** to `main` at `313f6bb`.)
**Session-end verification (Mac, skip-as-pass posture):** `cargo test --workspace` **1679 / 0 / 8**;
`clippy --workspace --all-targets -D warnings` clean. The Mac `python_exec_e2e` acceptance run (live PG 18) is **3/3**:
jailed round-trip with a real framework python under Seatbelt + a now-meaningful socket-containment negative (previously
it passed *vacuously* — the xcrun shim exited 1 for any code); the scratch test self-skips on darwin by design.
**DGX (bwrap) acceptance of `python_exec_e2e` is still the open follow-up** — this session's SSH attempt to update the
DGX checkout was permission-denied by the harness; run it operator-side or re-authorize.

**This session (2026-06-13) — Phase 4 acceptance + two cross-cutting macOS fixes (branch `claude/vigorous-feistel-b0c811`).**
The handover's "acceptance first" step, Mac half. The first run failed twice, each a real bug:
1. **`tests-common::unique_suffix` collision class FIXED.** The `{pid}-{nanos}` scheme collides on macOS — `CLOCK_REALTIME`
   is ~µs-resolution, so two parallel test threads computed the **identical** PG data dir; one initdb hit `File exists`,
   the other had its directory ripped away mid-bootstrap by the first's panic-triggered `PathGuard` drop. Now
   `{pid}-{nanos}-{counter}` (process-global `AtomicU64`) — uniqueness is clock-independent; likely kills (or shrinks) the
   standing "parallel initdb churn" flake class (#130 territory, the `embedding_recall_e2e` note below). +1 pinned test
   (8 threads × 1000 suffixes, no duplicates — deterministically failed pre-fix).
2. **macOS interpreter cascade FIXED (`core/src/workers/python_exec.rs`).** `/usr/bin/python3` on every Mac is Apple's
   **xcrun shim** (SIP owns `/usr/bin`): in-jail it dies dlopen'ing `libxcrun.dylib` from the unreadable Xcode/CLT tree
   (exit 1 for ANY code — the socket negative "passed" vacuously), and even unjailed it **re-injects `SDKROOT`/`CPATH`/
   `LIBRARY_PATH`/… into the real python child**, breaking the worker's env-isolation contract (caught by
   `real_python.rs::child_env_is_cleared…` on the first full Mac workspace run since the merge). `PYTHON_CANDIDATES` is
   now per-OS (`pub`, the e2e probes the same list): Linux `/usr/bin` → `/usr/local/bin`; macOS Homebrew →
   `/usr/local/bin` → CLT, shim excluded by construction. And since every working macOS python canonicalizes into a
   **framework** layout (`…/Python*.framework/Versions/<v>/bin/<exe>`) whose `Python` dylib + `Resources/` are *siblings*
   of `bin/`+`lib/`, the old `<prefix>/lib` fs_read grant could not even load the binary — new `interpreter_extra_fs_read`
   grants the framework **version root** for that layout (POSIX prefixes keep `<prefix>/lib`). Worker-side
   `real_python.rs::find_python` mirrors the per-OS list (can't import core — duplicated with a keep-in-sync comment).
3. **Mac acceptance GREEN:** `KASTELLAN_PG_BIN_DIR=… cargo test -p kastellan-core --test python_exec_e2e` → **3/3**
   (jailed round-trip on Homebrew python 3.14 under Seatbelt, real socket-containment negative, darwin scratch self-skip).
   Manifest tests 7→10. **DGX (bwrap) acceptance remains open** — the SSH `git checkout/pull` on the DGX was
   permission-denied this session (the DGX checkout sits on a pre-#267 commit); run operator-side or re-authorize.

**PR #267 review pass (2026-06-12, prior session):** self-review found + fixed three things. (1) **macOS clippy break:**
the e2e scratch test's darwin early-return sat after `ready_or_skip()` → unused `env` binding → `-D warnings` fails on
the Mac acceptance run; gate hoisted above the binding. (2) **Streaming capped capture:** `run_code` no longer uses
`wait_with_output` (unbounded buffering — on macOS, where Seatbelt has no memory cap, a `print('x'*10**9)` payload would
balloon the worker's own RSS); two concurrent reader threads buffer ≤ `MAX_CAPTURE_BYTES` each then drain-discard to EOF
(O(cap) memory, no pipe-stall deadlock; runaway CPU stays bounded by the policy's cpu/wall caps). (3) **Interpreter
symlink canonicalization:** new `ResolveCtx::canonicalize` probe (`fs::canonicalize` in `build_tool_registry`; `None`
stub in test ctxs — 12 mechanical ctx-literal updates); the python-exec manifest canonicalizes the resolved interpreter
so an update-alternatives chain (`/usr/bin/python3 → /etc/alternatives/python3`, unreachable in-jail since
`/etc/alternatives` isn't bound) can't break the spawn — this container has exactly that layout, so the DGX-acceptance
watch item is closed by construction. Also merged `main` (slice #3b leak scanner) into the branch — Cargo/ROADMAP
auto-merged, HANDOVER resolved keeping both parallel session blocks. +5 tests (13 worker unit / 8 real-interpreter incl.
a dual-stream flood pin / 8 manifest).

**Prior session — Phase 4 entry: `python-exec` worker slice #1 (ROADMAP:202), PR [#267](https://github.com/hherb/kastellan/pull/267) MERGED.** The first executor for agent-authored
Python; everything later in Phase 4 (skill catalog, trust ceilings, delegation) invokes code through it. Spec'd
(`docs/superpowers/specs/2026-06-12-python-exec-worker-design.md`) then built:
- **Worker** `workers/python-exec` (Rust, mirrors shell-exec — the CPython process is a *child* so a wedged payload
  can't corrupt the JSON-RPC loop): `python.exec` `{code}` → `{exit_code, stdout, stderr, *_truncated}`; source piped
  over stdin to `<python> -I -S -B -` (`-I -S` IS the roadmap's "curated stdlib bind" — no site-/dist-packages; a
  determinism measure, the jail is the security boundary); child `env_clear` + `TMPDIR`/`HOME`/cwd → `/tmp`; 256 KiB
  code + per-stream capture caps; a Python exception is `exit_code` + traceback, **not** an RPC error (the planner
  iterates on its own code). Fail-closed startup on `KASTELLAN_PYTHON_EXEC_PYTHON`.
- **Containment (strictest of any worker):** `Net::Deny`; `Profile::WorkerStrict` — the seccomp filter survives
  `execve` into CPython, pinned empirically by the new `coreutils_smoke::python3_survives_strict` case (real BPF
  enforcement verified in this container: scratch write/read round-trip completes, `BASE_ALLOW` covers CPython);
  **`fs_write = []`** — scratch is the jail's per-spawn ephemeral `/tmp` tmpfs (#89), granted worker-side via an
  explicit `KASTELLAN_LANDLOCK_RW=["/tmp"]` in `policy.env` (`derive_lockdown_env` honours caller-supplied values; a
  `/tmp` entry in `fs_write` would instead bind the HOST `/tmp` over the tmpfs — do not "fix" that); cpu 10 s rlimit /
  mem 512 MiB cgroup / wall 30 s watchdog; `SingleUse`.
- **Manifest** `core/src/workers/python_exec.rs`: **opt-in `KASTELLAN_PYTHON_EXEC_ENABLE=1`**, else `Disabled` —
  shell-exec is deny-by-default via its empty argv allowlist, python-exec has no equivalent knob so the posture moves
  to registration. Interpreter override fails closed; candidate cascade is **per-OS** (since 2026-06-13: Linux
  `/usr/bin` → `/usr/local/bin`; macOS Homebrew → `/usr/local/bin` → CLT — `/usr/bin/python3` there is the jail-broken
  xcrun shim); `fs_read` = worker + interpreter + derived stdlib (`<prefix>/lib`, or the framework **version root** for
  macOS framework pythons). Registered in `WORKER_MANIFESTS`. `GuardProfile::Strict` (default) deliberately kept —
  output may launder fetched content.
- **Tests:** 10 worker unit + 7 real-interpreter integration (`workers/python-exec/tests/real_python.rs` — env
  isolation, no-site-packages, caps, >64 KiB-source feeder, all green vs this container's CPython 3.11) + 7 manifest
  unit + `core/tests/python_exec_e2e.rs` (production-policy jail round-trip / socket-containment negative / scratch
  round-trip; PG+sandbox gated, skip-as-pass here).
- **macOS notes (slice-1 gaps, both tighter-not-looser):** no writable scratch (Seatbelt deny-default + empty
  `fs_write`) and the standing `mem_mb` gap; Mac runtime validation pending, same posture browser-driver slice #1 had.

**PR #269 review pass (2026-06-13):** addressed the code-review findings in place. (1) `mitm/relay.rs::scan_relay`
rewritten as two independent per-direction `pump` futures driven by one `select!` — a direction's `write_all` no longer
head-of-line-stalls the peer direction's reads (the old single-`read`-loop awaited `write_all` inside the arm); preserves
scan-before-forward + kill-on-hit, adds a 256 KiB full-duplex no-stall test. (2) New `MAX_SECRET_LEN` (16 KiB) caps the
fingerprintable range, enforced in both `fingerprint_value` and `wire::decode_one` so a corrupt/oversized
`secret_hashes.json` `len` can't drive a large ring-buffer allocation. (3) `net_worker` now warns (not silently skips) if
the sidecar UDS has no parent dir. Findings #3 (`provision_audit_row` has no live caller) + #4 (double audit line per
blocked leak) are by-design for the mechanism-only slice — tracked on [#268](https://github.com/hherb/kastellan/issues/268)
to land with the dispatch-time live-append. +3 tests (1641 → 1644).

**Prior session (parallel machine) — egress proxy SLICE #3b: co-located credential-leak scanner (ROADMAP:142).** Brainstormed → spec'd →
planned → executed via subagent-driven TDD (13 tasks, 2-stage review per batch + an opus whole-branch review that confirmed
the **"never plaintext" invariant holds end-to-end**). The per-worker egress proxy already MITM-terminates worker TLS (slice
#3a), so it sees plaintext; #3b scans that plaintext for the verbatim bytes of secrets materialized for the calling worker,
killing + auditing the connection on a hit (hash + offset only, never plaintext). **Locked decisions:** hashes-only detection,
scratch-file lazy-re-read provisioning, best-effort streaming block with carry-over, mechanism+spawn-wire with dispatch-append
deferred. Shape:
- **New pure crate `kastellan-leak-scan`** (single source of truth so the algorithm can't drift between the two sides; deps
  serde/serde_json/sha2 only — avoids dragging web-common's reqwest/hyper tree into `core`): `SecretFingerprint`
  {len, fp64, sha256} + `fingerprint_value` (Rabin `fp64` + SHA-256), streaming `RollingMatcher` (per-length Rabin rolling
  pre-filter + SHA-256 confirm + `(maxLen+1)`-byte ring-buffer carry-over so a secret split across reads still matches;
  O(maxLen) memory ⇒ no body cap), `serialize_hashes`/`parse_hashes` (`secret_hashes.json`, hex-encoded, lenient).
- **Host:** `Vault::value_fingerprint` (read-lock, computes one-way hashes in place, **never returns/logs plaintext**);
  `core/src/egress/leak_provision.rs` (atomic temp+rename `write_secret_hashes` + `provision_audit_row` →
  `egress.secret_hash.provisioned {worker,name,value_sha256}` so a leak hash is name-correlatable); spawn-wiring —
  `spawn_net_worker`/`spawn_forced_net_worker` take `secret_fingerprints: &[SecretFingerprint]` and write the file into the
  sidecar scratch dir after sidecar-ready / before worker-spawn (best-effort: a write failure warns + disables scanning, never
  aborts the worker). **Today's callers pass `&[]`** (no egress worker carries secrets yet).
- **Proxy:** `MitmCtx.secret_hashes_path` + `load_patterns` (lazy per-connection read; missing/corrupt ⇒ empty ⇒ no scan);
  new `workers/egress-proxy/src/mitm/relay.rs` `scan_relay` replaces `copy_bidirectional` for the non-empty case — splits both
  halves, one `RollingMatcher` per direction, **scans each chunk before forwarding it** (the chunk completing a secret is never
  relayed), `tokio::select!` dual-pump terminating on dual-EOF; `intercept` now returns `Result<Option<LeakReport>, String>`.
  `report::Verdict::BlockedCredentialLeak` + `Decision.leak` (additive `skip_serializing_if`); host `egress/audit.rs` maps
  `egress.blocked.credential_leak` carrying hash+offset+direction, **never plaintext**.
- **Posture (deliberate):** leak scanning is **fail-OPEN** (defense-in-depth, NOT the containment boundary — the OS
  netns/Seatbelt barrier stays fail-closed). It catches verbatim-contiguous bytes only — encoding/cross-request splitting
  evade it (shared by any block mode; documented as the ceiling, not a perfect exfil barrier).
- **Audit note:** a detected leak emits TWO decisions — the pre-intercept `Allowed (tls_intercepted)` then
  `BlockedCredentialLeak` (the CONNECT *was* allowed; the leak was caught mid-tunnel). Coherent for the per-line audit
  consumer; correlate by worker/host/port.
- **Tests:** `kastellan-leak-scan` 15 units (incl. boundary-split + fp64-collision-rejected-by-SHA-256 pins); host units for
  vault/leak_provision/audit; proxy `scan_relay` in-memory duplex tests; `core/tests/egress_leak_scan_e2e.rs` cross-boundary
  contract (core writes ⇄ proxy parser reads, pins the `secret_hashes.json` literal). **Deferred:** dispatch-time live-append
  ([#268](https://github.com/hherb/kastellan/issues/268)) — lands with the first secret-bearing egress worker, and will bundle
  the now-8-arg spawn signatures (`#[allow(too_many_arguments)]`) into a params struct. `net_worker.rs` is 520 LOC (+20, a
  bucket-c test-lift candidate, within the ≤27-over precedent).
- Spec/plan: `docs/superpowers/{specs,plans}/2026-06-12-egress-proxy-slice3b-credential-leak-scanner-design.md` /
  `2026-06-12-egress-slice3b-credential-leak-scanner.md`.

**Prior session — `browser-driver` worker slice #1 (ROADMAP:147), MERGED PR [#262](https://github.com/hherb/kastellan/pull/262).**
Feasibility spike GREEN on both platforms (headless `chromium-headless-shell` renders inside the REAL jail; Seatbelt needs
`ipc-posix-shm*`+`iokit-*`+`mach-lookup/register`, DGX needs a new `Profile::BrowserClient` with 9 seccomp additions +
`io_uring` EPERM carve-out — pinned in design spec §3.1) + slice #1 scaffold (Playwright-Python `browser.render` worker, real
launch is **Phase 2**). **⚠ Phase 2 blocker [#263](https://github.com/hherb/kastellan/issues/263)** (force-routing collision)
must be resolved before un-stubbing the renderer. Detail: `docs/superpowers/{specs,plans}/2026-06-12-browser-driver-worker*`.

**Prior session — Matrix comms channel (Phase 2 inbound), MERGED PR [#265](https://github.com/hherb/kastellan/pull/265).**
Decision + bus + Matrix (hermetic) + pairing + outbound + homeserver infra; new `core/src/channel/*`, `workers/matrix*`,
`db/src/pairings.rs` + migration 0018. (Orthogonal to egress/secrets — did not touch slice #3b's files.)

**Prior session — egress proxy SLICE #3a (TLS-intercept MITM), MERGED PR [#259](https://github.com/hherb/kastellan/pull/259)
at `e2a7b2b`.** The per-worker proxy MITM-terminates each worker's TLS (in-proxy ephemeral per-instance CA via `rcgen`;
private key never leaves the sandbox, public `ca.pem` exported beside the UDS), re-originates a webpki-validated session to
the pinned origin, surfaces only an additive `tls_intercepted` audit flag. New egress-proxy modules
`ca.rs`/`leaf_cache.rs`/`mitm.rs`; worker trusts ONLY the per-instance CA when `KASTELLAN_EGRESS_PROXY_CA` set
(fail-closed). DGX-accepted under real bwrap. Full detail:
`docs/superpowers/specs/2026-06-11-egress-proxy-slice3-tls-intercept-design.md` + PR #259. **Slice #3b
(credential-leak scanner) shipped this session.**

**Prior session — egress proxy SLICE #2** (force-routing DGX-accepted + ON by default; PR
[#256](https://github.com/hherb/kastellan/pull/256) MERGED to `main` at `f0464d7`): every supervised `Net::Allowlist`
worker force-routes through its own egress-proxy sidecar (private netns, no direct route), fail-closed if the proxy
binary is missing. Pre-prune snapshot: [`archive/handover_20260611_pre-prune.md`](archive/handover_20260611_pre-prune.md).

**Prior session — `db/src/secrets.rs` prod split (refactor bucket item 9b-b, PR [#253](https://github.com/hherb/kastellan/pull/253) MERGED).** The most-over-cap clean prod-split
candidate (848 LOC) → a parent facade + three cohesive siblings, every file under the 500-LOC cap, public API
byte-identical via `pub use`:
- `db/src/secrets/error.rs` (77) — the shared `SecretsError` enum.
- `db/src/secrets/crypto.rs` (385) — size/migration constants, `SecretKey`/`Nonce` aliases, and the pure
  `validate_name`/`compute_aad`/`encrypt`/`decrypt` helpers + their 17 unit tests.
- `db/src/secrets/key_provider.rs` (197) — the `KeyProvider` trait + `MapKeyProvider` (tests) + `OsKeyringProvider`
  (production) + 2 unit tests.
- `db/src/secrets.rs` (252, was 848) — module docs + `pub use` re-exports + `SecretListing` + the async DB I/O
  (`put`/`get`/`list`/`delete`).
All `kastellan_db::secrets::*` paths preserved (verified against external callers `main.rs`,
`core/src/secrets/vault.rs`, `secret_vault_e2e`, `postgres_e2e`). No behaviour change — the same 130 db-lib tests
(now namespaced `secrets::crypto::tests` / `secrets::key_provider::tests`), workspace **1537 / 0 / 7** unchanged,
clippy `-D warnings` clean. ROADMAP unchanged by its own convention (file splits aren't tracked there — ROADMAP:12).

**Prior session — public website kastellan.dev (`site/`, Cloudflare Pages; PR [#252](https://github.com/hherb/kastellan/pull/252) MERGED).** Brainstormed (operator-approved
wireframes), spec'd, and built the public site: `site/{index,roadmap,security,contributing}.html` + shared
`style.css` (light "B1 Pure Clean" system, indigo accent, one dark band; AA-contrast audited) + retitled **SVG**
security diagrams (the PNG exports still said "hhagent" — the site now serves kastellan-branded SVGs from
`docs/*.svg` sources, −1.2 MB) + `scripts/site/check-site.sh` (page/meta/nav/local-link suite; hard-fails if tidy
is absent, loud-`[SKIP]`s Apple's pre-HTML5 tidy) + `site/README.md` (**operator action after merge:** Cloudflare
Pages → connect `hherb/kastellan`, preset None, no build command, output dir `site`, branch `main`, then attach
`kastellan.dev`). Content is curated by hand — checklist item 7 below keeps `site/roadmap.html` fresh. Spec/plan:
`docs/superpowers/{specs,plans}/2026-06-11-kastellan-dev-website*`. Follow-up: regenerate the root `assets/*.png`
architecture/request-flow exports (still "hhagent"-titled; only the site copies were fixed).

**Current state.** `main` is at `313f6bb` (python-exec slice #1, PR [#267](https://github.com/hherb/kastellan/pull/267)
MERGED; it carries egress slice #3b PR [#269](https://github.com/hherb/kastellan/pull/269), browser-driver slice #1 PR
[#262](https://github.com/hherb/kastellan/pull/262), Matrix comms PR [#265](https://github.com/hherb/kastellan/pull/265)).
**This session's work is on branch `claude/vigorous-feistel-b0c811`:** Mac acceptance of `python_exec_e2e` (3/3) + the
`unique_suffix` collision fix + the macOS interpreter-cascade/framework-fs_read fix — see the "This session" block above.
Dev box **macOS**; the DGX (aarch64) is driven natively over WireGuard SSH (`ssh dgx`) — **note: its checkout sits on a
pre-#267 commit; the DGX `python_exec_e2e` acceptance is the open follow-up.** **Session-end: Mac
`cargo test --workspace` = 1679 / 0 / 8; `clippy --workspace --all-targets -D warnings` clean.**
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
The current native-Linux test baseline is **1538 / 0 / 10**
(`feat/egress-slice3-tls-intercept`, 2026-06-12 — full `cargo test --workspace` with live PG 18 + worker binaries built
so the real-sandbox e2e suites run, not skip; clippy `-D warnings` clean. The older 1327 figure predated the
web-fetch/web-search/egress/handoff/secrets work).

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
kastellan (Rust workspace, 15 crates [+ `matrix`/`matrix-wire` from PR #265 not yet folded into this tree], AGPL-3.0)
├── core               kastellan-core: lib + 2 bins (`kastellan` daemon + `kastellan-cli` audit-tail viewer). Daemon blocks on SIGTERM/SIGINT via tokio::signal::unix; main.rs runs db::probe::run → connect_runtime_pool → spawn_mirror before wait_for_shutdown (fail-closed startup; mirror failures are logged but non-fatal). lib modules: tool_host (spawn_worker, dispatch chokepoint, lockdown-env derivation, wall-clock watchdog, sealed WorkerCommand, secret-ref substitution on input + injection-guard screen on output), secrets (Vault TTL'd RwLock<HashMap> + SecretRef opaque newtype + substitute_refs_in_params walker + value_fingerprint [one-way hash of a secret value for the egress #3b leak scanner — never exposes plaintext]), cassandra/injection_guard (22-entry substring catalogue as `Rule`s + per-tool `GuardProfile` Strict/Relaxed via `for_tool` + `screen`/`screen_with_profile` + extract_scannable_text; Relaxed caps the chat-template family at one sub-threshold contribution — #142), workspace (per-task scratch with RAII cleanup), audit_mirror (PgListener-driven JSONL writer with daily rotation + fsync per write), audit_tail (`tail -f`-style follower used by `kastellan-cli audit tail`), scheduler/ (audit.rs pure helpers + canonical SCHEDULER_AUDIT_ACTOR; runner.rs spec §7 lifecycle rows + l3_run routing; tool_dispatch.rs short-circuit rows; crash_recovery.rs sweep_and_audit; l3_run.rs daemon-side L3 skill execution), memory/ (mod.rs facade + recall.rs three-lane RRF-fused recall + embed.rs embed_query + l0_seed/l1_promote/l3_crystallise/l3_approval/l3_invoke/l3_surface), worker_lifecycle/ (Lifecycle enum + SingleUse/IdleTimeout/Composite managers; idle_timeout.rs acquire path + idle_timeout/release.rs release path; force_route.rs egress force-routing — `ForceRoutingConfig` + pure `policy_net_is_force_routable`/`resolve_force_routing`/`spawn_worker_maybe_forced` + env-glue `from_env`/`env_flag_enabled` [default scratch root `/tmp` on macOS for sun_path], the `KASTELLAN_EGRESS_FORCE_ROUTING` flip — **ON by default** in the supervised deployment via `core_service_spec`, fail-closed; both cold-spawn sites route Net::Allowlist workers through it), entity_extraction/ (batch_upsert.rs two-phase unnest + per-row attribution), worker_manifest (WorkerManifest trait + Resolution + ResolveCtx + discover_binary — the uniform self-description each worker registers behind), workers/ (shell_exec.rs ShellExecManifest + shell_exec_entry; web_fetch.rs WebFetchManifest + web_fetch_entry [Net::Allowlist + WorkerNetClient host-side manifest]; web_search.rs WebSearchManifest + web_search_entry [Net::Allowlist derived from the endpoint host:port; injects KASTELLAN_WEB_SEARCH_ENDPOINT + allowlist]; gliner_relex/ facade re-exporting wire.rs serde shapes + resolve.rs GlinerRelexEnv/resolve_env + entry.rs gliner_relex_entry/host+container builders + client.rs Client + manifest.rs GlinerRelexManifest; browser_driver.rs BrowserDriverManifest + browser_driver_entry + pure resolve_env [ENABLE-gated, WorkerNetClient + legacy direct-net Net::Allowlist, no proxy_uds; slice #1 scaffold — real Playwright render is Phase 2]; python_exec.rs PythonExecManifest + python_exec_entry + pure resolve_env [ENABLE-gated, Net::Deny + WorkerStrict, scratch = jail /tmp tmpfs via explicit KASTELLAN_LANDLOCK_RW]), registry_build (static WORKER_MANIFESTS [shell-exec, gliner-relex, python-exec, web-fetch, web-search, browser-driver] + pure assemble_registry [skips the reserved `handoff` name] + async build_tool_registry(pool, exe_dir)), handoff (in-memory per-task content-addressed HandoffCache: stash_if_oversized → placeholder, fetch → clamped slice, per-task byte budget + MAX_TRACKED_TASKS backstop, purge_task at terminal; wired into ToolHostStepDispatcher after dispatch returns + the `handoff`/`fetch` built-in intercept), egress/ (host-side egress-proxy integration — slice #2 COMPLETE: DGX-accepted, force-routing ON by default: spawn.rs `spawn_sidecar`/`SidecarHandle` [+`terminate(&mut)`]/`proxy_policy`; audit.rs pure `decision_to_audit` + runtime-free `ingest_decisions_into`; net_worker.rs pure `rewrite_worker_policy` + `spawn_net_worker` [sidecar-first fail-closed, 1:1 teardown via `SupervisedWorker.egress`] + `spawn_forced_net_worker` [scratch-owning wrapper, `EgressSidecar.scratch` RAII-cleaned] + `pg_decision_sink`; **slice #3b leak scanner:** `leak_provision.rs` [atomic `write_secret_hashes` + `provision_audit_row`], both spawn fns take `secret_fingerprints` [callers pass `&[]` today], `audit.rs` maps `egress.blocked.credential_leak` redacted [hash+offset+direction])
├── db                 kastellan-db: pure helpers (build_initdb_argv, build_postgresql_auto_conf, find_pg_bin_dir, pg_bin_dir_candidates_with_env_override) + conn::ConnectSpec + RUNTIME_ROLE/set_role_runtime_statement + probe::run (ensure DB → migrate as superuser → SET ROLE → audit, fail-closed) + graph::{Graph trait, PgGraph; recursive-CTE path() + walk_outbound/inbound_edges + walk_edges_around with DISTINCT ON diamond-dedupe} + audit::{insert, fetch_by_id, fetch_since, truncate_payload} + memories::{insert, insert_memory_at_layer, insert_memory_light (embedding-skipping light write path), semantic/lexical/graph search, link_memory_to_entities, set_skill_trust, load_layer_by_trust} + entity_kinds + relation_kinds lookup caches + pool::{connect_runtime_pool, connect_admin_pool} + MIGRATOR (0001..0017) + memory_entities join table + deleted_memories audit table + secrets/ (AES-256-GCM at rest + OS keyring; prod-split into `crypto.rs` pure helpers [constants + validate_name/compute_aad/encrypt/decrypt] + `key_provider.rs` [KeyProvider trait + MapKeyProvider/OsKeyringProvider] + `error.rs` [SecretsError] + parent async DB I/O put/get/list/delete, all re-exported flat) + kastellan-db-init bin
├── leak-scan          kastellan-leak-scan: pure shared credential-leak scanner (egress #3b single source of truth; deps serde/serde_json/sha2 only). fingerprint.rs (`SecretFingerprint{len,fp64,sha256}` + `fingerprint_value` [Rabin fp64 + SHA-256] + `MIN_SECRET_LEN`=8 + `RABIN_BASE`), matcher.rs (`RollingMatcher` — per-length Rabin rolling pre-filter + SHA-256 confirm + `(maxLen+1)`-byte ring-buffer carry-over; `feed`→first `LeakHit{sha256_hex,offset}`; O(maxLen) mem ⇒ no body cap), wire.rs (`serialize_hashes`/`parse_hashes` for `secret_hashes.json`, hex-encoded, lenient). Consumed by `core` (provision) + `egress-proxy` (detect)
├── llm-router         kastellan-llm-router: sole egress for LLM calls. Router::send + Router::embed over reqwest+rustls; Backend::{Local, Frontier} closed enum; PolicyGate trait (DefaultLocalPolicy always Local — Phase-5 seam). RouterConfig::from_env reads KASTELLAN_LLM_* env. Per-OS default URL: vLLM/SGLang on Linux (:8000), Ollama on macOS (:11434). Frontier dispatch returns PolicyDeniedFrontier until Phase 5
├── sandbox            kastellan-sandbox: SandboxPolicy (+ additive `proxy_uds: Option<PathBuf>` — slice #2 force-routing target) + `Net` enum {Deny | Allowlist(hosts) | ProxyEgress (the egress proxy's own policy — real netns, self-enforcing; #141 slice #1)}; `Net::Allowlist + proxy_uds` ⇒ bwrap private netns + UDS bind / Seatbelt deny-outbound-except-UDS (slice #2). + SandboxBackend trait + SandboxBackendKind (cfg-gated per-OS) + SandboxBackends resolver + LinuxBwrap (wrapped in systemd-run --scope cgroup) + MacosSeatbelt + MacosContainer (Apple `container` micro-VM, macOS-only, opt-in per-worker)
├── supervisor         kastellan-supervisor: SystemdUser (Linux; driver in systemd_user.rs + pure builders re-exported from systemd_user/builder.rs) + LaunchAgents (macOS) + specs::{core_service_spec, postgres_service_spec, kastellan_target_spec} + default_probe. ServiceSpec carries after/part_of ordering + optional restart_backoff (RestartBackoff{max_delay_sec,steps}: systemd → RestartSteps/RestartMaxDelaySec, launchd → warn-and-ignore); TargetSpec + Supervisor::{install,start,stop,uninstall}_target (default = generic bundle for launchd; SystemdUser overrides with a native kastellan.target unit). Names screened by validate_service_name before unit-file write
├── protocol           kastellan-protocol: JSON-RPC 2.0 over stdio (working)
├── tests-common       kastellan-tests-common: shared dev-dep crate (publish = false) — PgCluster + bring_up_pg_cluster(+_with_timeout), RAII guards, skip helpers, sandbox factory, binary discovery, macOS launchd serial lock (reentrant), deterministic SHA-256-seeded embedding seed. Consumed only from [dev-dependencies]; never linked into a runtime binary.
├── workers/prelude      kastellan-worker-prelude: Linux-only Landlock + seccomp lock_down (no-op on macOS) + cross-platform setrlimit(RLIMIT_CPU). Landlock now derives BOTH RW (from fs_write) and RO (from fs_read, env KASTELLAN_LANDLOCK_RO) rules so net workers can read /etc/resolv.conf in-jail
├── workers/shell-exec   kastellan-worker-shell-exec: uses prelude::serve_stdio
├── workers/web-common   kastellan-worker-web-common: shared lib for net-egress workers. allowlist.rs (HostAllowlist: host-only `from_env_json`/`is_allowed` + **port-scoped `from_endpoints`/`is_allowed_endpoint`/`is_port_scoped`** [host:port, IPv6-aware — #241]) + http.rs (HttpGet seam [+`transport_kind`] + RawResponse + ReqwestGet + **env-selected `make_get` factory**) + proxy_connect.rs (**ProxyConnectGet**: CONNECT-over-UDS HttpGet, hyper+tokio-rustls/ring, end-to-end TLS — used when `KASTELLAN_EGRESS_PROXY_UDS` set) + testing.rs (FakeGet, `testing` feature). Consumed by web-fetch + web-search + egress-proxy.
├── workers/web-fetch    kastellan-worker-web-fetch: first net-egress worker. HTTPS-only web.fetch JSON-RPC method. Consumes HostAllowlist + the HttpGet transport from web-common. extract.rs (HTML readability via dom_smoothie / PDF via pdf-extract / text+JSON, char-boundary text cap) + fetch.rs (the drive() redirect-follow loop — strict https-only per hop, 5-redirect cap) + handler.rs (web.fetch dispatch). Host-side manifest in core/src/workers/web_fetch.rs
├── workers/web-search   kastellan-worker-web-search: second net-egress worker. web.search JSON-RPC method (query → ranked {title,url,snippet,engine} hits from a SearxNG /search?format=json endpoint). Consumes HostAllowlist + transport from web-common. parse.rs (lenient SearxNG-JSON → Vec<Hit>) + search.rs (validate_endpoint [https everywhere, http loopback-only via is_loopback] + build_query_url + one-GET search() drive, count.clamp(1,20)) + handler.rs (dispatch + fail-closed from_env). Operator-configured KASTELLAN_WEB_SEARCH_ENDPOINT; LLM supplies only the query. Host-side manifest in core/src/workers/web_search.rs. Dev setup: scripts/web-search/setup-searxng.sh
├── workers/browser-driver kastellan-worker-browser-driver: Playwright-Python read-only render worker (ROADMAP:147; slice #1 scaffold — opt-in KASTELLAN_BROWSER_DRIVER_ENABLE=1). `browser.render` JSON-RPC stdio: navigate https URL headless → settle (wait_until) → post-JS readable text (readability-lxml) + final HTML, byte/char-capped. GLiNER-shaped: __main__.py (env/startup) + server.py (stdio dispatch + url/timeout/wait_until validation) + render.py (pure extract_render_result + Phase-2 Playwright drive behind a duck-typed renderer seam) + errors.py. Real browser launch = Phase 2 (spike pinned the flags/seccomp — design spec §3.1). Host manifest = core/src/workers/browser_driver.rs
├── workers/python-exec  kastellan-worker-python-exec: Phase-4 executor for agent-authored Python (opt-in KASTELLAN_PYTHON_EXEC_ENABLE=1). `python.exec` {code} → {exit_code, stdout, stderr, *_truncated}: source piped over stdin to `<python> -I -S -B -` (curated stdlib = no site-packages), child env cleared, 256 KiB code/capture caps; Python exceptions return as exit_code+traceback, not RPC errors. Strictest policy of any worker: Net::Deny + WorkerStrict seccomp (inherited by the CPython child; pinned by coreutils_smoke::python3_survives_strict) + fs_write=[] (scratch = jail's ephemeral /tmp tmpfs via explicit KASTELLAN_LANDLOCK_RW=["/tmp"]) + cpu 10 s / mem 512 MiB / wall 30 s, SingleUse. lib: exec.rs (python_args, truncate_lossy, run_code) + handler.rs. Host manifest = core/src/workers/python_exec.rs
└── workers/egress-proxy kastellan-worker-egress-proxy: per-worker egress boundary (ROADMAP:141/142; slice #1 allowlist+SSRF, slice #2 force-routing, slice #3a TLS-intercept). Sandboxed CONNECT proxy on a per-worker UDS; per CONNECT: HostAllowlist check (reuses web-common) → resolve DNS itself → ssrf.rs is_denied_range (reject private/loopback/link-local/ULA/CGNAT/multicast, IPv4-mapped+compatible unwrapped; literal-IP carve-out) → pin+dial → write 200 → peek first tunnel byte (recv MSG_PEEK; 0x16 → MITM, else transparent tunnel). **Slice #3a MITM:** in-proxy ephemeral per-instance CA (ca.rs, rcgen; private key never leaves the sandbox, public ca.pem exported beside the UDS), per-host CA-signed leaf cache (leaf_cache.rs), async terminate+re-originate (mitm.rs: looks_like_tls + intercept — tokio-rustls TlsAcceptor/TlsConnector + copy_bidirectional on a per-connection current-thread runtime; upstream validated against webpki). Decision carries tls_intercepted. **Slice #3b leak scanner:** `MitmCtx.secret_hashes_path` + `load_patterns` (lazy per-connection read of `secret_hashes.json`; missing/corrupt ⇒ no scan, fail-OPEN); `mitm/relay.rs` `scan_relay` replaces `copy_bidirectional` when patterns present — splits both halves, one `kastellan-leak-scan::RollingMatcher` per direction, **scans each chunk before forwarding**, kills on hit; `intercept` returns `Result<Option<LeakReport>,String>`; `report::Verdict::BlockedCredentialLeak` + `Decision.leak`. Modules: ssrf.rs, request_line.rs, report.rs, proxy.rs (decide + handle_conn connect→200→peek→branch + MitmCtx + run_mitm + load_patterns), ca.rs, leaf_cache.rs, mitm.rs (+ mitm/relay.rs), main.rs (install ring provider, generate CA + write ca.pem before lock_down, accept loop). Host side = core/src/egress
```

**Test baselines.** Native-Linux (DGX, PG 18 live, rustc 1.96.0, worker bins built): **1538 / 0 / 10**
on `feat/egress-slice3-tls-intercept` (2026-06-12 slice-#3a acceptance; the real-sandbox e2e suites actually run here,
unlike the older 1327 figure; predates the Matrix #265 + leak-scan #3b + python-exec #267 tests). macOS skip-as-pass
posture (no `KASTELLAN_PG_BIN_DIR`): **1679 / 0 / 8** (2026-06-13, after python-exec #267 + this session's
interpreter/`unique_suffix` fixes). 8–10 ignored =
explicit doctest/real-net markers;
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
- **2026-05-31 — `l3_crystallise.rs` test-lift (PR [#175](https://github.com/hherb/kastellan/pull/175) at `55b212e`):** inline `mod tests` → sibling; 676 → 467 LOC.
- **2026-05-31 — L3 skill crystallisation writer (PR [#173](https://github.com/hherb/kastellan/pull/173) at `6eb966e`):** first writer for `MemoryLayer::Skill` (L3) — agent emits `Plan.l3_skill` template → `drain_lane` validates → canonical-SHA-256 dedup → stores `layer=3 trust:"untrusted"`; `dispatch_count >= 1` grounding gate; `memory l3 {list,remove}`. Writer-only, non-executable. New `core/src/memory/l3_crystallise.rs`.
- **2026-05-31 — `inner_loop.rs` test-lift, closes [#81](https://github.com/hherb/kastellan/issues/81) (PR [#172](https://github.com/hherb/kastellan/pull/172) at `98a5be0`):** 655 → 438 LOC.
- **2026-05-30 — `replay.rs` test-lift (PR [#171](https://github.com/hherb/kastellan/pull/171) at `30aa52e`):** 804 → 422 LOC.
- **2026-05-30 — `tool_dispatch.rs` split (PR [#170](https://github.com/hherb/kastellan/pull/170) at `4e401cc`):** test-lift + re-exported `result_mapping.rs` seam; 828 → 442 LOC.
- **2026-05-30 — `db/memories.rs` split (PR [#169](https://github.com/hherb/kastellan/pull/169) at `e1be537`):** real prod split into re-exported `write.rs` + `search.rs`; 961 → 360 LOC.
- **2026-05-30 — `launchd_agents.rs` split (PR [#168](https://github.com/hherb/kastellan/pull/168) at `5bf010b`):** `builders.rs` + `tests.rs` siblings; 1093 → 485 LOC.
- **2026-05-30 — `scheduler/audit.rs` split (PR [#167](https://github.com/hherb/kastellan/pull/167) at `79fcc27`):** `extract_entities.rs` + `tests.rs` siblings; 1106 → 500 LOC.
- **2026-05-30 — #99 CLI `with_runtime` migration (PR [#166](https://github.com/hherb/kastellan/pull/166) at `75e9039`):** all six `kastellan-cli` dispatchers share one idiom; #99 CLOSED.
- **2026-05-30 — `macos_container.rs` test-lift (PR [#165](https://github.com/hherb/kastellan/pull/165) at `48c0396`):** 983 → 491 LOC.
- **2026-05-30 — #130 launchd bring-up serialization + #163 `sun_path` fix (PR [#164](https://github.com/hherb/kastellan/pull/164) at `091e53d`):** reentrant `serial_lock` around the macOS launchd window; bundled `injection_guard_e2e` label shorten + `check_socket_path_fits` guard. Both CLOSED.
- **2026-05-30 — #162 graph-lane seed-thread regression test (PR [#162](https://github.com/hherb/kastellan/pull/162) at `a83be4a`):** found item-12 wiring already shipped (Slice F, 2026-05-19); reconciled + pinned the seed thread; zero production change.
- **2026-05-30 — #153 clippy `-D warnings` gate (PR [#161](https://github.com/hherb/kastellan/pull/161) at `12b080c`):** cleared the whole workspace, flipped `linux-check` to `-D warnings`. CLOSED.
- **2026-05-29 — #5 `tool_host.rs` sibling-lift (PR [#160](https://github.com/hherb/kastellan/pull/160) at `fd7dd7a`):** watchdog + lockdown_env + seal tests → child modules; 911 → 519 LOC (trust-boundary residual).
- **2026-05-29 — #4b `injection_guard.rs` test-lift (PR [#159](https://github.com/hherb/kastellan/pull/159) at `1106145`):** 667 → 338 LOC.
- **2026-05-29 — #156 `walk()` sibling-continue (PR [#158](https://github.com/hherb/kastellan/pull/158) at `f3c380f`):** depth-skip now continues siblings. CLOSED.
- **2026-05-29 — #148/#149 secret-vault test gaps (PR [#157](https://github.com/hherb/kastellan/pull/157) at `53e68ed`):** `AuditSink` seam + `insert_fresh` extraction. Both CLOSED.
- **2026-05-29 — #143 `walk()` recursion-depth guard (PR [#155](https://github.com/hherb/kastellan/pull/155) at `6e82252`):** `MAX_WALK_DEPTH = 256`. CLOSED.
- **2026-05-29 — #144/#150 Linux build + clippy gate (PR [#152](https://github.com/hherb/kastellan/pull/152) at `560d845`):** `linux-check` CI green.
- **2026-05-29 — #147 redact secret plaintext in tool audit row (PR [#151](https://github.com/hherb/kastellan/pull/151) at `54e8885`).**
- **2026-05-29 — ★ Opaque secret references slice 1 (PR [#146](https://github.com/hherb/kastellan/pull/146) at `bc36e4c`):** `SecretRef` opaque newtype + `substitute_refs_in_params` walker + Vault. Closes openhuman Item 31.
- **2026-05-28 — ★ Worker-output prompt-injection guard slice 1 (PR [#141](https://github.com/hherb/kastellan/pull/141) at `62905ae`):** 22-entry substring catalogue + screen + `extract_scannable_text`. Closes openhuman Item 30.
- **2026-05-28 — `idle_timeout/release.rs` sibling-lift + #89 `/tmp` tmpfs pin** (PRs [#138](https://github.com/hherb/kastellan/pull/138)/[#139](https://github.com/hherb/kastellan/pull/139)/[#140](https://github.com/hherb/kastellan/pull/140)).
- **2026-05-27 — worker_lifecycle hardening (#84/#85/#86) + test-infra slices** (PRs #137/#135/#133/#132/#129; filed #130).
- **2026-05-26 — graph diamond-dedupe (#114/#115) + `KASTELLAN_PG_BIN_DIR` override + entity-upsert Layer B** (PRs #128/#126/#125).
- **2026-05-25 — Slice 2.5 follow-ups (#120/#121/#122) + `gliner_relex.rs` test-lift + GLiNER-Relex container** (PRs #124/#123/#118).
- **2026-05-23 — Item 23(a) test-lifts + Item 22 CLI splits (#111/#112) + `relations show`** (PRs #117/#116/#113).
- **2026-05-22 — kinds CLIs + `MacosContainer` Slice 2** (PRs #110/#109/#108; NB: the unconditional `Container` ref here is what broke the Linux build, #144).
- **2026-05-21 — macOS container backend Slice 1 + Apple `container` spike + GLiNER macOS device tree** (PRs #106/#105/#103/#100/#98).
- **2026-05-20 — quarantine review CLI + `kastellan-cli` split (#66) + entity-upsert Layer A** (PRs #96/#94/#93).
- **2026-05-19 — entity extraction v2: `memory_entities` auto-linker + GLiNER-Relex + migration 0016** (PRs #92/#91).
- **2026-05-18 — worker lifecycle managers + GLiNER worker + `inner_loop.rs` split (#81) + L1 promotion writer** (PRs #88/#87/#82).
- **2026-05-17 — recall-lane wiring into the production scheduler** (PR #79).
- **2026-05-16 — prompt-assembler L0+L1 + L0 seed loader + classification-floor inference** (PRs #74/#77/#70).
- **2026-05-15 — first CASSANDRA rules + replay harness + L1 storage migrations 0013/0014** (PRs #68/#67/#65/#61).
- **2026-05-14 — observation capture + constitutional refusal state (#23) + per-tool argv allowlist + CPU/rlimit** (PRs #60/#59/#54).
- **2026-05-13 — task-lifecycle audit rows + `WorkerCommand` seal (#16) + graph lane in recall** (PR #41).
- **2026-05-12 — `tests-common` crate (#15) + crash-recovery sweep + Option O embedding router** (PR #38).
- **2026-05-11 — scheduler online: `cli_ask_e2e` full-chain pin + CASSANDRA Phases 2–5.**
- **2026-05-10 — chokepoint + recall skeleton (Options M/N) + secrets-at-rest + audit NOTIFY/mirror + non-superuser role.**
- **2026-05-09 — cgroup v2 caps: `systemd-run --scope` MemoryMax/CPUQuota/TasksMax + C2.2 schema + Graph trait.**
- **2026-05-08 — Linux/macOS supervisors + per-task `Workspace` RAII + watchdog `kill(-1)` fix.**
- **2026-05-06/07 — Phase 0 sandbox core: Landlock+seccomp prelude + macOS Seatbelt + AGPL workspace + bwrap backend + shell-exec + first e2e.** Full detail in the 20260510 archive.

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

Phase 0 is complete; Phase 1 is on `main` and pinned by `cli_ask_e2e`. **The L3 invocation arc is COMPLETE on `main`** (PR #186, #179 CLOSED). **`web-fetch` (ROADMAP:145) / `web-search` (ROADMAP:146) workers + injection-guard per-tool profiles (#142) all MERGED.** **Egress proxy SLICE #1 (PR #240) + SLICE #2 (force-routing, PR #256 MERGED) are COMPLETE; SLICE #3a (TLS-intercept mechanism, PR #259 MERGED at `e2a7b2b`) is COMPLETE.** Next egress work is slice #3b (the co-located credential-leak scanner, on top of #3a's now-visible plaintext) and slice #4 (TLS pinning). The list below is an **operator-picks bucket** — sized roughly one session each, with file paths and the verification step.

**★ TOP PICK — Phase 4 continuation.** `python-exec` slice #1 is MERGED (PR #267); **Mac acceptance is GREEN
(2026-06-13)**; the Phase-4 sequence continues:
1. **DGX acceptance (small, do first):** update the DGX checkout to `main` ≥ `313f6bb` (it sits pre-#267), then
   `cargo test -p kastellan-core --test python_exec_e2e -- --nocapture` (real bwrap + Landlock + PG — the build
   container could only verify seccomp; Landlock reported `KernelTooOld` there; the Mac verified Seatbelt). This
   session's SSH attempt was permission-denied by the harness — run operator-side or grant the SSH state-change. Then
   flip `KASTELLAN_PYTHON_EXEC_ENABLE=1` wherever wanted — it is opt-in and unregistered by default.
2. **Skill catalog (named/persisted Python skills + optional human-approve gate, ROADMAP:203).** Build on the L3 arc's
   exact shape: the L3 templated-skill chain (crystallise → approve → pin → invoke, `memory l3 *`) is the working
   precedent; a Python skill is the same lifecycle where the payload is `python.exec` code instead of a tool-call
   template. Spec first: storage (L3 `memories` row vs dedicated table), the trust-enum mapping (ROADMAP:204 — per-level
   capability ceiling), and how `evaluate_approval`'s re-validation generalizes to code payloads (`secret://` scan
   carries over verbatim).
3. **python-exec slice-#2 candidates (on demand):** macOS writable scratch (shares browser-driver Phase 2's per-spawn
   scratch wiring); curated-wheels RO dir if skills demand packages; `stdin`/`argv` params.

**★ TOP PICK — egress proxy SLICE #4: TLS pinning for the frontier/LLM egress path (ROADMAP:142).** Slices #1–#3b are
COMPLETE (boundary + force-routing + MITM + credential-leak scanner). #4 pins the upstream certificate/SPKI for the
high-value frontier/LLM egress (the Phase-5 path) so a compromised public-CA trust store can't silently MITM kastellan's
own egress. The MITM re-origination leg already validates against webpki-roots (`egress-proxy::main` `upstream_tls`); #4
narrows that to a pinned key/cert for the specific frontier origin(s). **Spec it first.** Note slice #3b's **dispatch-time
live-append is the standing #3b follow-up** ([#268](https://github.com/hherb/kastellan/issues/268)) — it lands with the
first secret-bearing egress worker (which will also bundle the now-8-arg `spawn_net_worker`/`spawn_forced_net_worker`
signatures into a params struct, dropping the `#[allow(too_many_arguments)]`).
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

**★ TOP PICK — `browser-driver` Phase 2 (ROADMAP:147).** Slice #1 (spike + scaffold) shipped this session (PR #262 MERGED);
the spike **pinned the exact jail shape** (design spec §3.1). Phase 2 makes the worker actually render.

> **⚠ BLOCKER to resolve in Phase 2 — [#263](https://github.com/hherb/kastellan/issues/263) (force-routing collision).**
> The manifest declares `Net::Allowlist` with `proxy_uds: None` ("legacy direct-net", spec §2), but
> `policy_net_is_force_routable` ([`worker_lifecycle/force_route.rs:94`](../../../core/src/worker_lifecycle/force_route.rs))
> matches **all** `Net::Allowlist`, and `KASTELLAN_EGRESS_FORCE_ROUTING=1` is **ON by default** in the supervised
> deployment. So the moment Phase 2 drops the `NotImplementedError`, enabling the worker in the default deployment will
> **force-route it into a private netns + `CONNECT`-over-UDS — which a browser cannot speak** → silent network loss.
> Harmless in slice #1 (renderer is stubbed), real in Phase 2. **Pick one before un-stubbing the renderer:** (a) exempt
> `browser-driver` from force-routing while it's on the legacy path, or (b) gate the real renderer behind slice #2 so the
> browser only ever runs with `Net::Allowlist + proxy_uds` (the egress-proxy shim + in-browser per-instance-CA trust).
> Option (b) is cleaner (never on the host netns with an unenforced allowlist — see §6 defense-in-depth) but couples Phase 2
> and slice #2; (a) unblocks a render-only Phase 2 sooner. Acceptance: enabling it under force-routing must render or
> **fail closed with a clear error**, never silently lose the network.

1. **`render.py` real Playwright drive** — replace `__main__._build_renderer`'s `NotImplementedError` with a
   `PlaywrightRenderer` launching `chromium-headless-shell` with `args=["--no-sandbox","--disable-dev-shm-usage"]`,
   per-request `page.goto(url, wait_until=…, timeout=…)` → `page.content()` → `extract_render_result`; set
   `TMPDIR=<scratch>` so the user-data-dir lands in the writable scratch; abort off-allowlist subresources via request
   interception (self-enforce `KASTELLAN_BROWSER_DRIVER_ALLOWLIST`).
2. **`Profile::BrowserClient` seccomp** in `workers/prelude/src/seccomp_lock.rs` — `net_client` + the 9 additions
   (`fallocate ftruncate getresgid getresuid inotify_add_watch inotify_init1 memfd_create pidfd_open restart_syscall`)
   + an `io_uring_setup`/`io_uring_enter` **`Errno(EPERM)`** rule (not the default `KillProcess`). Wire the worker policy
   to select it.
3. **Seatbelt browser-profile extension** — gate the `ipc-posix-shm*`+`iokit-open/get-properties`+`mach-lookup/register`
   cluster to the browser-driver tool in `macos_seatbelt::build_profile` (try narrowing `mach-lookup` to `(global-name …)`).
4. **`fs_read`** the browser binary tree + fonts; **self-contained venv install script** `scripts/workers/browser-driver/install.sh`
   (system-`python3 -m venv` + `pip install -e .` + `playwright install chromium`); `mem_mb` already 1 GiB.
5. **`core/tests/browser_driver_e2e.rs`** — hermetic off-allowlist deny + `#[ignore] real_render_of_loopback_page`
   (cross-platform Seatbelt + bwrap; the spike's `scripts/spikes/browser-driver/` runners are the working reference).

Then **slice #2 (egress integration):** loopback-TCP↔UDS shim + in-browser per-instance-CA trust so the browser
force-routes through the egress proxy. **The other standing TOP PICK is egress slice #3b** (credential-leak scanner) above.

**Natural web-search follow-ups** (cheap, on demand): stand up a local SearxNG with `scripts/web-search/setup-searxng.sh`, set `KASTELLAN_WEB_SEARCH_ENDPOINT` + the `web-search` `tool_allowlists` row, and run the `#[ignore]` `core/tests/web_search_e2e.rs::real_search_against_searxng` to validate the real round-trip end to end. If/when a caller needs them: category/language/engine params or pagination on `web.search` (deferred per spec).

**Remaining handoff-cache follow-ups (ROADMAP:129)** — the cache (PR #199) and the planner-surfacing
(PR #200, this session) are both done; the mechanism is now live and known to the planner. Still open:
- **On-disk Workspace-backed store** — only once a per-task `Workspace` is actually wired into the live
  scheduler flow (it isn't today); the `HandoffCache` surface can take a disk impl behind it then.
- **Observe it in practice** — once a worker reliably returns >64 KiB (e.g. `web-fetch` on a large page),
  confirm the planner expands a stash via the `<handoff>` instruction in a real `cli_ask`-style run; if the
  prompt wording needs tuning, that's a cheap iteration on `render_handoff_block()`. (Optional / on demand.)

**Other Phase-3 natural picks:** egress slices #3/#4 and `browser-driver` are the two TOP PICKs above. Beyond those,
Phase-2 channels (IMAP/Telegram inbound) are the next phase boundary once Phase-3 egress is judged complete.

**Older follow-ups (ROADMAP:130, still open):** core-side caller wiring for `insert_memory_light` (lands with the first high-frequency writer — Phase 2 channels / Phase 3 browser); per-namespace caps + oldest-eviction on `memories.metadata` (no schema change); a graph-lane degradation test ([#196](https://github.com/hherb/kastellan/issues/196)).

**Refactor bucket — over-cap file splits (item 9b).** Re-census the exact split (`wc -l`) before picking — the numbers below drift each session:

- **(a) Clean test-lifts** (lifting the inline `mod tests` block alone lands the parent under cap): **none meaningfully remaining.** The substantial ones are done — `cassandra/types.rs`, `inner_loop_audit.rs`, `entity_extraction/gliner_relex.rs` (2026-06-07 batch); `macos_seatbelt.rs` (PR #192); `recall.rs`/`l0_seed.rs`/`capture.rs`/`inner_loop.rs`/`replay.rs` (Earlier history). A fresh census shows only files sitting **1–27 LOC over cap** still carry a liftable block (`core/src/main.rs` 527, `db/src/lib.rs` 525, `core/src/bin/kastellan-cli/memory_l3/run.rs` 519, `core/src/tool_host.rs` 519, `core/src/cassandra/constitutional.rs` 502, `core/src/memory/l1_promote.rs` 501) — a lift would save little; defer unless one grows.
- **(b) Need a real prod split or a re-exported pure-helper seam** (a test-lift alone leaves the parent over cap): `core/src/cli_audit.rs` (958, the most over-cap production file), `db/graph.rs` (926, the design-gated Item 23b walk-impl split — deferred until a 2nd `WalkedEdge` consumer materialises), `core/src/scheduler/runner.rs` (777), `core/src/scheduler/audit.rs` (701, tests already lifted), `db/src/entities.rs` (653), `workers/prelude/src/seccomp_lock.rs` (650), `core/src/scheduler/inner_loop.rs` (572, tests already lifted). (`db/secrets.rs` [848 → 252 + crypto/key_provider/error siblings], `systemd_user.rs`, `gliner_relex.rs` done — see history.) Next clean candidate after this session: `core/src/cli_audit.rs` (958, still the most over-cap production file).
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
