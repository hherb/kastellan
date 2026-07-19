# browser-driver micro-VM — slice 3: a live render through a real egress sidecar

**Date:** 2026-07-19
**Arc:** browser-driver Firecracker micro-VM entry (slice 1 = rootfs, PR #470;
slice 2 = the VM entry, PR #472; **this is slice 3, the last**)
**Status:** implemented (PR #474)

**Execution note:** implemented inline (controller-implements) rather than via a
separate plan document — the slice is two tests in one existing file with no
production-code change, so a task-decomposed plan would have added ceremony
without adding safety. Prior slices in this arc each had a matching
`docs/superpowers/plans/…` file; this one deliberately does not.

---

## 1. What is missing after slice 2

Slices 1 and 2 boot `browser-driver.ext4`, launch a real Chromium inside it, and
prove the browser reaches egress — the live tier asserts a **stub** proxy
receives `CONNECT example.org:443`. The stub answers `503` and closes, so the
render deliberately fails. The CONNECT line, not the render result, is the
signal.

That leaves three things unproven, all of which this slice closes:

1. **No real page has ever rendered inside the VM.** Not in slice 1, not in
   slice 2 — and, as §3 explains, not in host mode either. Everything after the
   `CONNECT` (TLS to the origin, the response body, Playwright's post-JS DOM
   extraction, the JSON-RPC reply carrying real text) is untested in a VM.
2. **`mem_mb: 2048` is reasoned, not measured.** Slice 2 set it from the
   argument that guest RAM must cover Chromium *plus* the `/tmp` tmpfs that
   `--disable-dev-shm-usage` redirects shared memory into (slice-1 spec §10.1/§10.4).
   Nothing has ever measured what a render actually costs.
3. **`wall_clock_ms: 90_000` is reasoned, not measured.** Same origin: a cold VM
   boot, then Playwright's Node driver, then a Chromium cold start. Slice 2's
   live tier does use the entry's own budget, so a grossly-too-tight value would
   fail — but it never *renders*, so the budget is only exercised up to the first
   CONNECT, not to a completed page.

The dangerous shape of (2) is that it is **silent**. Guest `/tmp` is drawn from
the same 2048 MB as everything else rather than from a separate `/dev/shm`, so a
heavy page's shared-memory allocations compete with guest RAM. If that tips over,
the VM OOMs — with `test_disable_dev_shm_usage_is_pinned` green throughout,
because that test pins the *flag*, not the *budget*.

## 2. Goal

One real page, rendered inside the micro-VM, through a **real
`kastellan-worker-egress-proxy` sidecar**, returning real post-JS text — plus a
measurement of what it costs in guest memory and wall-clock time.

## 3. The origin problem, and why it forced the design

**Browser-driver's sidecar runs in no-MITM transparent-tunnel mode.**
`force_route::disable_mitm_for` names `browser-driver` (and `matrix`), so the
proxy tunnels bytes rather than terminating TLS. That is deliberate — it
preserves Chromium-grade origin certificate validation and keeps the sidecar's
blast radius small — but it has a consequence for testing:

> Chromium does end-to-end TLS with the origin, so **Chromium must trust the
> origin's certificate**. Our per-instance MITM CA is never in the picture.

Every other force-routed worker sidesteps this. web-fetch runs *with* MITM, so
its e2e injects the per-instance CA into the guest and a hermetic self-signed
loopback origin works fine. Browser-driver cannot do that.

This is exactly why **no real render through a real sidecar has ever completed,
in any mode**, and the existing tests say so in their own doc comments:

* `browser_driver_e2e::forced_render_of_loopback_page_through_sidecar` (host
  mode) navigates `https://127.0.0.1:<port>/` at a **plain-HTTP** loopback
  server. The TLS handshake cannot succeed. Its acceptance signal is the
  *sidecar decision row*, and its doc comment states plainly that a full 200 "is
  not hermetically achievable" and needs "the deferred MITM/NSS path".
* `web_fetch_firecracker_egress_e2e::real_web_fetch_through_sidecar` is an empty
  `#[ignore]`d scaffold that only `eprintln!`s a note (see §7 — lodged as debt).

### 3.1 Options considered

| # | Origin | Verdict |
|---|---|---|
| A | Hermetic self-signed loopback TLS origin | **Rejected.** Needs a CA in Chromium's NSS store inside the rootfs — that *is* the deferred MITM-of-browser work, far larger than this slice. |
| B | `--ignore-certificate-errors-spki-list` | **Rejected.** Would add a certificate-validation-weakening flag to `DEFAULT_LAUNCH_ARGS`, i.e. to **production**, to make a test pass. The arc already rejected `--ignore-certificate-errors-*` as the route to browser MITM. |
| C | Plain-HTTP loopback origin | **Rejected.** The egress proxy is a `CONNECT` proxy; Chromium sends an absolute-form `GET` for `http://`, which it rejects. This is why the host-mode forced test uses an `https://` URL against an HTTP server in the first place. |
| D | **A real public HTTPS origin** | **Chosen.** Chromium's own root store validates a real certificate, so end-to-end TLS completes and the render returns real text. |

Option D is the only one that completes a render without either weakening
production or building the NSS trust path. Its cost is an external dependency,
which §5.3 handles.

## 4. Design

### 4.1 Drive the production manager, not a hand-wired spawn

Slice 2's live tier calls `spawn_worker` directly against a stub UDS. This slice
uses **`SingleUseLifecycle::with_force_routing(...).acquire(...)`** — the real
daemon path — mirroring `web_research_vm_force_route_daemon_e2e` (#448).

That is strictly stronger, and the reason is specific: the manager resolves the
*worker* backend from `entry.sandbox_backend` (`FirecrackerVm`) and the
*sidecar* backend from `SandboxBackends::resolve(None, None)` (host bwrap), and
it derives `disable_mitm` by calling `force_route::disable_mitm_for(worker_name)`
itself. A hand-wired `NetWorkerSpawn` would have the test *assert*
`disable_mitm: true` — the production code would never be consulted. Under the
manager, if someone removed `browser-driver` from `disable_mitm_for`, the sidecar
would start terminating TLS and the render would fail on an untrusted certificate.
That is a real property being tested rather than restated.

### 4.2 Two tests, one job each

**(a) `vm_renders_real_page_through_real_sidecar`** — the acceptance gate.

* Origin `https://example.org/`, allowlist `example.org` (mapped to
  `example.org:443` by `allowlist_to_net_entries`, per the #469 all-port-grant fix).
* Asserts the dispatch **succeeds** and the returned text contains
  `Example Domain` — the first real page ever rendered in the VM.
* Asserts a captured sidecar decision `allowed` for `example.org:443`, so the
  render provably went *through* the sidecar rather than around it.
* Records elapsed wall-clock and asserts real **headroom** under the entry's own
  `wall_clock_ms`, not merely that it finished. Finishing at 89 s under a 90 s
  budget is a latent failure; the assertion is `< 70%` of budget.

`example.org` is chosen for stability: a tiny, famously invariant page whose
`Example Domain` heading has not changed in years. This test should not flake on
content drift.

**(b) `vm_render_of_heavy_page_stays_within_memory_budget`** — the measurement.

A light page under-exercises the exact concern, so a second test renders a
substantial real page (a Wikipedia article: large DOM, many subresources) and
measures peak guest memory.

*How the measurement works.* Firecracker allocates guest RAM lazily, so the
**VMM process's host RSS** tracks how much of the 2048 MB the guest has actually
touched — including the `/tmp` tmpfs pages that `--disable-dev-shm-usage`
redirects shared memory into, which is precisely the quantity §1(2) is about. A
sampler thread walks `/proc/*/comm` for the `firecracker` process during the
render and keeps the peak `VmRSS`.

*No silent skip.* If the sampler never finds a firecracker process, the test
**fails loudly** rather than skipping the assertion. The render succeeded, so a
VM demonstrably ran; finding none means the sampler is broken, and a
quietly-skipped assertion is the false-green pattern this project's "when tests
pass but feel suspicious" rule exists to prevent.

The two are split because they have different flake profiles: (a) is a stable
correctness gate, (b) depends on a page whose weight can drift. Isolating the
external-content risk in the measurement test keeps the acceptance gate solid.

### 4.3 What is deliberately NOT changed

* **No production code changes are expected.** This slice measures the slice-2
  budgets; it only changes them if a measurement says they are wrong (§6).
* **No `DEFAULT_LAUNCH_ARGS` change** — see option B above.
* **No NSS/CA work.** MITM-of-browser stays deferred.

## 5. Test-tier placement

### 5.1 Tier

Both are `#[ignore]`d, DGX-only: they need real KVM, vsock, the rootfs, the
egress-proxy binary, a live PG, and outbound HTTPS. Workspace counts move
`2590/0/48 → 2590/0/50` (two ignored tests added, none newly running).

### 5.2 Is an always-running check needed?

Slice 2's `/fixall` lesson was that an `#[ignore]`d live tier leaves budgets
unguarded, which is why the hermetic tier now pins `wall_clock_ms`/`cpu_ms`.
That pin already exists and still holds here.

The one production property this slice's design rests on —
`disable_mitm_for("browser-driver") == true` — is **already pinned** by
`force_route/tests.rs:205`, which runs on every platform. So no new hermetic test
is warranted; adding one would duplicate an existing pin.

### 5.3 Skip-as-pass discipline

The suite must stay green on a box without outbound HTTPS, but a silent skip
that looks like a pass is exactly what CLAUDE.md warns about. Both tests
pre-flight a TCP connect to the origin and, if it fails, print an explicit
`[SKIP]` line naming the reason — the established project pattern, visible under
`--nocapture`.

## 6. Acceptance

1. `vm_renders_real_page_through_real_sidecar` green on the DGX: real text
   returned, `allowed example.org:443` decision captured, elapsed time printed.
2. `vm_render_of_heavy_page_stays_within_memory_budget` green: peak VMM RSS
   printed as evidence and within budget.
3. Both measurements reported in HANDOVER, converting `mem_mb`/`wall_clock_ms`
   from *reasoned* to *measured*. **If a measurement contradicts a slice-2
   value, the manifest changes and the reasoning is recorded** — that is the
   point of the slice, not a failure of it.
4. Full DGX workspace `cargo test` + `clippy --workspace --all-targets -D warnings`
   clean; expected `2590/0/50`.
5. Mac `cargo check -p kastellan-core --all-targets` clean (this file is
   `cfg(target_os = "linux")`, so the DGX core-clippy gate is authoritative —
   the recurring `cfg-linux-e2e-deadcode-dgx-clippy` lesson).

## 7. Debt found while specifying

`web_fetch_firecracker_egress_e2e::real_web_fetch_through_sidecar` is a hollow
`#[ignore]`d scaffold: its body only `eprintln!`s "manual: see test doc". It
passes trivially while its name claims real-network origin validation through the
sidecar. It is counted among the workspace's ignored tests as though it were a
real deferred test.

Out of scope here (it is a web-fetch test, and web-fetch runs *with* MITM so it
has none of the origin problem above — option A works for it). **Lodge as a
GitHub issue** rather than leave it undocumented, per the project's
no-technical-debt rule.

## 8. Revisions

### 8.1 The measurements (2026-07-19, DGX, real KVM + live PG + real sidecar)

Both slice-2 budgets are now **measured and confirmed**, with wide margins.

| Tier | Origin | Text | Elapsed | Peak VMM RSS |
|---|---|---|---|---|
| acceptance | `example.org` | 125 B | **1.61 s** = 1.8% of the 90 s budget | not sampled |
| measurement | Wikipedia "Rust (programming language)" | 38 408 B | 3.50 s | **628 MiB** = 30.7% of the 2048 MiB budget |

Both sidecar decisions came back `egress.allowed … "tls_intercepted":false`,
confirming the transparent tunnel: Chromium validated each origin's real
certificate itself.

**The 628 MiB reading is a floor, not a ceiling.** Only `en.wikipedia.org` is
allowlisted, so cross-host subresources — notably the article's images on
`upload.wikimedia.org` — were egress-blocked and the render was DOM +
same-origin CSS/JS only. A page whose image assets were all in-allowlist would
decode them into exactly the shm-in-tmpfs memory this tier measures. Kept that
way deliberately (post-review, 2026-07-19): the tier exists to isolate
content-drift risk, and widening the allowlist would couple the reading to
Wikipedia's image weight too. The ~69% headroom conclusion stands with that
caveat attached.

**Neither budget is changed.** `mem_mb: 2048` carries ~69% headroom on a page far
heavier than anything slice 2 reasoned about, and `wall_clock_ms: 90_000` is
enormously generous — a *complete* cold-boot-to-rendered-page cycle is under two
seconds. Trimming the wall clock toward the observed figure was considered and
rejected: the budget must cover a cold host page cache, a slower origin, and a
much heavier page than either sampled here, and the cost of it being too generous
is only a slower failure, whereas too tight is a spurious dispatch abort.

### 8.2 Two corrections found by running it

1. **The egress action is `egress.allowed`, not `allowed`.** `decision_to_audit`
   namespaces every verdict under `egress.` (siblings
   `egress.blocked.credential_leak`, `egress.blocked.tls_pin`). The first live
   run rendered the page successfully and then failed on this assertion — a good
   failure: the render evidence was already printed, so the cause was immediate.
2. **A transparent-tunnel assertion was added** (`tls_intercepted == false`).
   The design leaned on `disable_mitm_for` naming this worker but never observed
   it; now a change that starts MITM-ing browser traffic fails here with a
   message naming the cause, instead of surfacing as an opaque certificate error.

### 8.3 Negative-case verification

Following the arc's standing discipline (slice 1's tautological pin, slice 2's
repointed const), the memory assertion was verified to be **live rather than
vacuous**: with the threshold tightened from 85% to 20%, the test **failed
loudly** with the intended message and the same reproducible 628 MiB reading,
then passed again on restore. The reading was identical across three separate
runs, so the sampler is measuring a real, stable quantity.

### 8.4 Debt lodged

§7's hollow `real_web_fetch_through_sidecar` scaffold is filed as
[#473](https://github.com/hherb/kastellan/issues/473).

### 8.5 Post-review hardening (/fixall, 2026-07-19)

Four review findings, all fixed in-PR and re-verified live on the DGX: all
three ignored tiers green post-fix, memory tier at **623 MiB = 30.4%** —
consistent with the prior 628 MiB readings, so the sampler change measures the
same stable quantity.

1. **Memory budget read from the entry, not a mirrored const.** The
   `VM_MEM_MB: u64 = 2048` mirror contradicted the acceptance tier's own
   "measure the production value" rule for `wall_clock_ms`; the memory tier now
   reads `browser_driver_vm_entry().policy.mem_mb`, so a future `mem_mb` change
   cannot leave the assertion measuring against a stale budget.
2. **Sampler excludes pre-existing `firecracker` PIDs.** Matching by comm alone
   could attribute another tier's (or a stale spike) VMM to this render; VMMs
   alive before the sampler starts are now snapshotted and skipped, and the
   documented run command gained `--test-threads=1` so tiers cannot race.
3. **Transparent-tunnel assertion scoped to the allowed row.** It previously
   matched `"tls_intercepted":false` on *any* decision; it now checks the same
   `egress.allowed <host>:443` row, with a comment noting `decision_to_audit`
   defaults a missing field to false — what the assertion catches is the sidecar
   reporting `true`, i.e. actively MITM-ing browser traffic.
4. **The measurement's floor caveat documented** (§8.1 above and at
   `HEAVY_ORIGIN_HOST`): cross-host image subresources are egress-blocked, so
   628 MiB understates a fully-loaded heavy page.
