# browser-driver worker — design (slice #1: read-only render, spike-gated)

**Status:** approved 2026-06-12 (brainstorm). **ROADMAP:** 147.
**Scope:** the first JS-capable net worker. A read-only `browser.render(url)`
that drives a headless browser (Playwright, Python) inside the existing OS
sandbox and returns the post-JS readable text + final HTML — "web-fetch for
JS-heavy / SPA pages."

This slice is **spike-gated**: a throwaway feasibility spike proves a headless
browser survives the real jail (Seatbelt + bwrap) and pins the required
seccomp/Landlock/flag set *before* any production worker is written. The spike
outcome feeds the worker design; specifics it must finalize are marked
**[spike-gated]** below.

---

## 1. Why

web-fetch / web-search retrieve *static* bytes. A growing share of the web
renders its content with client-side JS (SPAs, infinite-scroll, paywalled
shells, dashboards). The agent needs a worker that runs the page's JS, lets it
settle, and returns what a human would actually see. Playwright headless is the
standard tool; the open risk is whether a full browser engine can run under
kastellan's process-per-worker OS jail at all — hence spike-first.

## 2. Locked decisions (from the brainstorm)

| Axis | Decision | Rationale |
| ---- | -------- | --------- |
| Capability | Read-only `browser.render(url)`; no interaction | Smallest valuable surface; single-use worker; mirrors web-fetch |
| Risk posture | **Spike first** | Browser-in-jail is the load-bearing unknown; de-risk before building (cf. GLiNER MPS spike, Apple-container spike) |
| Output | `text` (post-JS readability) + final `html` | No binary payloads; flows through injection-guard + handoff + char caps the other net workers use |
| Driver stack | **Playwright (Python worker)** | Cross-engine, robust auto-wait/network-idle; reuses the proven GLiNER Python-worker scaffold; matches the handover's stated intent |
| Egress (slice #1) | **Legacy direct-net `Net::Allowlist`** (host netns, no `proxy_uds`); self-enforced host allowlist per navigation + subresource | Browser can't `CONNECT` over the proxy UDS, and slice #3a MITM needs in-browser CA trust — both are genuinely new pieces. Defer to **slice #2 (egress integration)** so the spike stays focused on the jail. |
| Extraction | **Python-side readability** (`readability-lxml`, Apache-2.0 — AGPL-OK) on the post-JS DOM | Avoids hoisting web-fetch's `dom_smoothie` extractor out of its crate into `web-common` (a larger, separate refactor — deferred) |
| Injection guard | browser-driver joins `GuardProfile::Relaxed` | Fetched page content legitimately contains chat-template-like tokens; the guard code already anticipates this worker |

## 3. The spike (this session's deliverable — the gate)

**Goal:** prove a headless browser renders a page inside the **real** OS jail on
**both** Mac (Seatbelt) and the DGX (bwrap + Landlock + seccomp prelude), and
record the exact jail adjustments required.

**Must answer:**

1. Does `chromium-headless-shell` (Playwright's lean headless build) launch +
   render under **Seatbelt**? under **bwrap + the prelude**? With which launch
   flags — `--no-sandbox` (disable Chromium's own user-namespace sandbox and
   rely on *our* jail), and whether `--single-process` and
   `--disable-dev-shm-usage` are also needed.
2. Which **seccomp** syscalls the prelude profile must allow to avoid `SIGSYS`
   — a browser spawns helper processes (`clone`/`clone3` with namespace flags,
   `futex`, `prctl`, `memfd_create`, …). Capture the precise delta against the
   existing `WorkerNetClient` profile.
3. Which **`fs_read`** (and any `fs_write`/tmpfs) paths must be bound: the
   browser binary tree, system fonts, the worker venv, `/dev/shm` (or rely on
   `--disable-dev-shm-usage`), a writable scratch profile dir.
4. Render **hermetically** — a `file://` page or a loopback HTTP server —
   isolating "browser runs in the jail" from any egress/DNS concern.
5. Rough **RAM/CPU** footprint → informs the cgroup `MemoryMax` for the worker.

**Engine order:** Chromium first. If its nested sandbox cannot be tamed under
bwrap, fall back to Firefox (Playwright) and record which engine won and why.

**Form:** throwaway `scripts/spikes/browser-driver/` (a launch script + a tiny
Python render probe) + a **findings note** folded back into this spec (a new
"§3.1 Spike findings" section). Not production code; archived/removed after the
worker lands, like prior spikes.

**Success criteria:** one page's readable text extracted from inside the real
jail on **Mac AND DGX**, with the working `--flags` + seccomp-allow delta +
`fs_read` set written down. A red spike (browser cannot be contained with
acceptable flags) re-opens the driver-stack decision (Rust-CDP, or the
container backend) before any worker code.

## 3.1 Spike findings — GREEN (executed 2026-06-12, Mac Seatbelt + DGX bwrap)

**Verdict: GREEN on both platforms.** Headless **Chromium** (Playwright's
`chromium-headless-shell`, build v1223 / Chrome 148) rendered the hermetic
`file://` fixture (JS executed — `js-ran` paragraph present) from inside the
real jail on macOS (Seatbelt) **and** the DGX (aarch64, bwrap). No engine
fallback to Firefox was needed. Proceed to Phase 1. Throwaway harness:
`scripts/spikes/browser-driver/{probe.py,fixture.html,run.sh,seatbelt-run.sh,seatbelt-bisect.sh,dgx-bwrap-run.sh}`.

**Engine + launch flags (both platforms):** `chromium.launch(headless=True,
args=["--no-sandbox", "--disable-dev-shm-usage"])`. `--no-sandbox` disables
Chromium's *own* user-namespace sandbox (our OS jail is the boundary);
`--disable-dev-shm-usage` makes Chromium use the profile dir instead of
`/dev/shm` (so the jail needs **no** writable `/dev/shm`). `--single-process`
was **not** required on either platform.

**Writable scratch (both):** Chromium needs **one** writable dir for its
`--user-data-dir`; Playwright places it under `$TMPDIR`. Map to
`policy.fs_write = [scratch]` + set `TMPDIR=<scratch>`. On bwrap the `--tmpfs
/tmp` already provides this if `TMPDIR=/tmp`.

**macOS Seatbelt — minimal additions over `macos_seatbelt::build_profile`:**
the base strict profile (deny-default, no `mach-lookup`) renders only after
adding **all three** of these clusters (bisected — dropping any one → child
`SIGSEGV`):
- `(allow ipc-posix-shm*)` — shared-memory IPC between browser processes.
- `(allow iokit-open)` + `(allow iokit-get-properties)` — GPU/graphics probing
  (even under SwiftShader software rendering).
- `(allow mach-lookup)` + `(allow mach-register)` — Mach IPC bootstrap.

  This is a **real threat-model widening**: the base profile deliberately denies
  `mach-lookup` (issue #1). browser-driver must re-grant it. **Phase-2 hardening:**
  try narrowing `mach-lookup` to specific `(global-name …)` services rather than
  the unrestricted form; record the actual service set Chromium needs. `fs_read`
  additions: the venv, the Playwright browser cache
  (`~/Library/Caches/ms-playwright`), system + user `Fonts`. **Packaging gotcha:**
  a uv-created venv symlinks `python` to an *external* uv-managed CPython, whose
  `libpython` lives outside `venv_dir` — the jail blocked it. The production
  worker venv must be **self-contained** (system-`python3 -m venv`, which copies/
  symlinks within a stable interpreter under `/usr`) **or** mount the interpreter
  root. `resolve_env` should prefer a self-contained venv so only `venv_dir`
  needs binding.

**DGX bwrap — renders under `build_argv`'s invariants** (`--unshare-all
--share-net --die-with-parent --new-session --as-pid-1 --clearenv`, `--proc
/proc --dev /dev --tmpfs /tmp`, `--ro-bind /usr` + the `/bin`/`/lib` symlinks)
plus `--ro-bind` of the venv + `~/.cache/ms-playwright` + `/etc`. The aarch64
headless-shell exists and works.

**DGX seccomp — additions over the `net_client` allow-list** (enumerated by
`strace -f -c` of the full bwrapped process tree, then diffed; the prelude's
default is `KillProcess`, so every syscall Chromium issues must be listed). The
genuine browser additions are:
`fallocate`, `ftruncate`, `getresgid`, `getresuid`, `inotify_add_watch`,
`inotify_init1`, `memfd_create`, `pidfd_open`, `restart_syscall`.
(`capget`/`capset`/`pivot_root`/`umount2` also appeared but are **bwrap's own
container setup**, executed before the worker self-applies the filter — *not*
added.) **Security decision — `io_uring_setup`/`io_uring_enter`:** Chromium
probes io_uring, but it is a well-known sandbox-escape primitive. Do **not**
`Allow` it; add it as an explicit **`Errno(EPERM)`** rule (not the default
`KillProcess`) so Chromium falls back gracefully instead of being killed. This
needs a new `Profile::BrowserClient` (or a `net_client`+browser superset) in
`workers/prelude/src/seccomp_lock.rs` with an `Errno`-action carve-out — a small
Phase-2 change. **Landlock:** RW = the scratch/user-data-dir (from `fs_write`);
RO = venv + browser cache + fonts (from `fs_read` via `KASTELLAN_LANDLOCK_RO`),
identical to the web-fetch/web-search RO derivation.

**Resource footprint:** headless-shell resident is ~150–300 MB for a single
page; the plan's `mem_mb = 1024` slice-1 cap is comfortably safe (tune later).

**Net new Phase-2 work surfaced by the spike** (folded into §9 / the Phase-2
plan): a `Profile::BrowserClient` seccomp profile with the 9 additions + an
`io_uring` `Errno(EPERM)` carve-out; a macOS Seatbelt browser-profile extension
(the shm/iokit/mach clusters) gated to the browser-driver tool; a self-contained
venv install script; the `TMPDIR`→scratch wiring.

## 4. Worker package (`workers/browser-driver/`) — [shape spike-gated]

uv package `kastellan_worker_browser_driver`, `[project.scripts]
kastellan-worker-browser-driver`, mirroring the GLiNER layout:

- `__main__.py` — read env, validate preconditions (browser binary present),
  hand off to `Server.run(stdin, stdout)`. Startup errors write one JSON line
  to stderr + non-zero exit *before* the stdio loop (maps to the slice-2 crash
  classifier's "dead").
- `server.py` — JSON-RPC 2.0 stdio loop (byte-for-byte the GLiNER pattern):
  tolerant frame parse → `PARSE_ERROR`; dispatch only `browser.render`;
  per-field validation sharing one `INVALID_INPUT` code; model/browser failure
  caught as a request-local `RENDER_FAILED`, worker stays alive.
- `render.py` — the Playwright drive (launch context → `page.goto(url,
  wait_until=…)` → settle → `page.content()` + readability → caps). Duck-typed
  behind `server.py` so tests inject a fake browser (cf. GLiNER's `model.py`).
- `errors.py` — code constants + `error_response`/`success_response` helpers.

### Wire contract — `browser.render`

**params**

| field | type | rule |
| ----- | ---- | ---- |
| `url` | string | required, **https only** (loopback `http://` allowed only for tests, mirroring web-search's `validate_endpoint`) |
| `timeout_ms` | int | optional, default 15000, clamped to `[1000, 30000]` |
| `wait_until` | string | optional, one of `load`/`domcontentloaded`/`networkidle`, default `networkidle` |

**result**

| field | type | note |
| ----- | ---- | ---- |
| `final_url` | string | after JS-driven redirects |
| `status` | int | main-document HTTP status (best-effort from the navigation response) |
| `title` | string | `document.title` |
| `text` | string | post-JS readability, char-boundary-capped |
| `html` | string | final serialized DOM, byte-capped |

**Caps** (reuse web-fetch values where they exist): total HTML byte cap, text
char cap with char-boundary truncation, hard render timeout. Exact constants
mirror `workers/web-fetch` / `web-common`.

## 5. Host manifest (`core/src/workers/browser_driver.rs`) — [spike-gated]

Mirror the GLiNER manifest split if it grows past the cap; start single-file.

- `resolve_env(env_lookup, is_dir, exists) -> Result<BrowserDriverEnv, ResolveSkipReason>`
  — pure, fakeable. Skip reasons: `Disabled` (`KASTELLAN_BROWSER_DRIVER_ENABLE`
  != `"1"` after trim), `VenvDirUnresolvable`, `ScriptShimMissing`,
  `BrowserBinaryMissing` (the Playwright browser tree absent).
- `BrowserDriverManifest` implementing `WorkerManifest` → emits a `ToolEntry`
  with `Profile::WorkerNetClient` and, **slice #1**, `Net::Allowlist(hosts)`
  with **no `proxy_uds`** (legacy host-netns path — see §2 egress decision).
- Allowlist source: unlike web-search (fixed endpoint), the render URL is
  per-dispatch, so the allowlist can't be host-derived at registration. Slice
  #1 uses an **operator-configured** `KASTELLAN_BROWSER_DRIVER_ALLOWLIST`
  (host:port JSON, reusing `web-common::HostAllowlist::from_endpoints`); the
  worker self-enforces it per navigation **and per subresource** via Playwright
  request interception (defense in depth — the jail's netns is the hard
  boundary, the in-worker check is the early reject).
- Optional macOS container backend via `KASTELLAN_BROWSER_DRIVER_USE_CONTAINER=1`
  + `_IMAGE` override, exactly as GLiNER — **[spike-gated]** on whether the
  browser even needs it (it may be the answer if bwrap-nesting fails).
- `KASTELLAN_BROWSER_DRIVER_ENABLE=1` opt-in; daemon logs the skip reason at
  startup otherwise (production default = disabled until the operator stages the
  venv + browser).

## 6. Subresource / allowlist semantics (slice #1)

A browser pulls subresources (CSS/JS/fonts/images/XHR) from many hosts. Slice #1:

- The worker intercepts every request; off-allowlist subresources are
  **aborted** (page may render partially) and counted; the main navigation host
  must be on the allowlist or the render fails closed.
- This is documented as a known slice-#1 limitation: pages that hard-depend on
  cross-origin CDNs render partially unless the operator widens the allowlist.
- Broadening (e.g. an "allow same-site + declared CDN list" policy) is a
  follow-up, and lands naturally with **slice #2** when egress-proxy enforcement
  (which already does host:port allowlisting at the boundary) takes over.

## 7. Cross-cutting integration

- **Injection guard:** flip `GuardProfile::for_tool("browser-driver")` Strict →
  Relaxed (beside web-fetch/web-search) and update the existing assertion in
  `core/src/cassandra/injection_guard/tests.rs:426`. Rendered output then rides
  the dispatch chokepoint like any net worker's.
- **Handoff cache:** oversized HTML (>64 KiB `Ok` result, `task_id>0`)
  auto-stashes — no new code, already automatic at the dispatcher.
- **Worker manifest registry:** add `BrowserDriverManifest` to the static
  `WORKER_MANIFESTS` list in `registry_build.rs`.
- **Lifecycle:** single-use (spawn-per-call) for slice #1; the browser is
  cold-launched each render. (Warm-keep via the idle-timeout lifecycle is a
  latency follow-up, not slice #1.)

## 8. Testing (TDD)

**Hermetic / pure (run everywhere):**
- Python: `server.py` dispatch against a fake browser — happy path, unknown
  method, missing/empty `url`, non-https url rejection, `timeout_ms` clamp,
  `wait_until` validation, `RENDER_FAILED` surfacing (mirror GLiNER
  `test_server.py`); `render.py` readability + caps against canned HTML.
- Rust: `resolve_env` skip-reason branches (fakeable env/fs); manifest emits
  `WorkerNetClient` + `Net::Allowlist` + the operator allowlist;
  `Misconfigured` when no binary; allowlist reuse from `web-common`.

**Real-sandbox e2e (`core/tests/browser_driver_e2e.rs`):**
- Skip-as-pass without the browser binary / sandbox / venv.
- Hermetic deny-path: a host off the allowlist refuses at startup (mirrors
  `web_search_e2e`).
- `#[ignore] real_render_of_loopback_page` — spawn a loopback HTTP server
  serving a JS-rendered page, render it through the real jail, assert the
  post-JS text appears. Cross-platform (Seatbelt + bwrap).

**Manual:** the spike, on Mac + DGX.

## 9. Out of scope (explicit, for slice ≥2)

- Egress-proxy force-routing for the browser (UDS↔loopback-TCP shim +
  in-browser per-instance-CA trust) — **slice #2**.
- Screenshot/PNG output — fast slice #2/#3 once render is proven.
- Scripted interaction / stateful sessions (click/type/fill).
- Warm-keep lifecycle; broadened subresource allowlist policy.
- Hoisting web-fetch's `dom_smoothie` extractor into `web-common`.

## 10. AGPL / dependency check

- **Playwright** (Python): Apache-2.0 — OK. Bundled browser binaries are
  separate downloads (Chromium: BSD-style; Firefox: MPL-2.0) staged by an
  operator install script, not vendored.
- **readability-lxml**: Apache-2.0 — OK. (`lxml`: BSD — OK.)
- No CDDL/BUSL/SSPL/Elastic deps.

## 11. Open questions the spike resolves

1. Engine: Chromium vs Firefox under bwrap nesting.
2. The exact `--flags` + seccomp-allow delta + `fs_read` set.
3. Whether `/dev/shm` must be a real tmpfs or `--disable-dev-shm-usage`
   suffices.
4. Whether the macOS container backend is required (vs Seatbelt direct).
5. Browser-binary install/discovery convention (Playwright's `PLAYWRIGHT_BROWSERS_PATH`
   vs a staged tree) — feeds `resolve_env` + an `install.sh`.
