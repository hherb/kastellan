# Embedding Broker Sidecar — Slice B (core wiring) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development
> or superpowers:executing-plans. Steps use `- [ ]` checkboxes.
> **DGX-gated:** the real bwrap-bind + spawn acceptance runs natively on the DGX
> (`ssh dgx '<cmd>'`); the Mac verifies build + clippy + the pure/hermetic units.

**Prerequisite (DONE):** Slice B1 — `SandboxPolicy.embed_broker_uds: Option<PathBuf>`
+ `linux_bwrap` `--bind` + `macos_seatbelt` `(allow network-outbound (remote
unix-socket …))` — is merged (branch `feat/embed-broker-slice-b`). This plan
consumes that field; do **not** re-add it.

**Goal:** Wire the trusted embedding broker end-to-end in core: spawn a
per-worker `embed-broker` sidecar, bind its `embed.sock` into the consuming
worker's jail via `embed_broker_uds`, inject `KASTELLAN_EMBED_BROKER_UDS` so the
worker's `choose_embedder` selects `BrokeredEmbedder`, and have the web-research
manifest **drop the embed host from `Net::Allowlist`** in broker mode — so the
embed backend leaves the worker's egress entirely.

**Slice A recap (already merged, `b077629`):** the `embed-broker` crate (serves
JSON-RPC `embed{model,input}` over a UDS, forwards OpenAI-compat to
`KASTELLAN_EMBED_BROKER_ENDPOINT`, fail-closed caps) and `BrokeredEmbedder` +
`choose_embedder` in web-research (broker UDS wins over endpoint) exist and are
hermetic. Slice B makes core actually *spawn* the broker and bind it in.

---

## Architecture decision (the genuinely-undecided part of Slice B)

The broker is spawned **at dispatch/spawn time, per-worker, 1:1** — mirroring the
egress sidecar (`spawn_forced_net_worker`), NOT resolved statically in the
manifest. The manifest's job is only to *declare* that a worker wants a broker
and carry the backend config; core's spawn chokepoint acts on it.

Three moving parts, mirroring force-routing's `ForceRoutingConfig` /
`spawn_worker_maybe_forced` / `EgressSidecar` triad:

1. **`ToolEntry.embed_broker: Option<EmbedBrokerSpec>`** — set by the manifest in
   broker mode. `EmbedBrokerSpec { endpoint: String, model: String }` names the
   backend the broker forwards to. When `Some`, the manifest **omits** the embed
   host from `Net::Allowlist` and does **not** inject
   `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` into the worker (the worker never
   reaches the backend directly).

2. **`EmbedBrokerConfig`** (daemon-level, analogous to `ForceRoutingConfig`) —
   the discovered `kastellan-worker-embed-broker` binary + a scratch root + the
   sandbox backend. Threaded into the spawn chokepoint. `None` ⇒ broker disabled
   (a worker with `embed_broker: Some` but no daemon config → **fail-closed**:
   refuse to spawn rather than silently fall back to direct egress, since the
   manifest already dropped the embed host so hybrid would silently die anyway;
   surface a `Misconfigured`/`ToolHostError`).

3. **`EmbedBrokerSidecar`** owned on `SupervisedWorker.embed_broker:
   Option<EmbedBrokerSidecar>` (sibling to `egress: Option<EgressSidecar>`) —
   holds the `SidecarHandle` + its scratch `PathBuf` for RAII teardown, 1:1 with
   the worker.

**Spawn ordering (critical — a worker may be BOTH force-routed AND broker-backed):**

```
spawn_worker_with_broker(entry, force, embed_cfg, …):
  1. if entry.embed_broker is Some and embed_cfg is Some:
       broker = spawn_embed_broker(embed_cfg, spec)   # mints embed-<pid>-<seq>/,
                                                       # spawns broker Net::Allowlist([endpoint host]),
                                                       # env KASTELLAN_EMBED_BROKER_{UDS,ENDPOINT},
                                                       # waits for embed.sock
       policy.embed_broker_uds = Some(broker.uds)
       policy.env += (KASTELLAN_EMBED_BROKER_UDS, broker.uds)   # so choose_embedder picks BrokeredEmbedder
     if embed_broker Some but embed_cfg None -> Err (fail-closed)
  2. worker = spawn_worker_maybe_forced(force, backend, &spec_with_broker, name)
       # force-routing may ALSO set proxy_uds + append the MITM CA via
       # rewrite_worker_policy — that path must PRESERVE embed_broker_uds + the env
  3. worker.embed_broker = Some(EmbedBrokerSidecar { handle, scratch })
```

**`rewrite_worker_policy` interaction:** it clones `spec.policy` and mutates
`proxy_uds` / `fs_read` (CA). Confirm it preserves `embed_broker_uds` and the
injected env (a struct clone does; add a regression test pinning it). The broker
socket bind is orthogonal to the netns/proxy decision (Slice B1 guarantees this).

**Broker's own egress (v1 = loopback):** the broker runs on the host netns with
`Net::Allowlist([backend host:port])` — NOT force-routed (a loopback backend is
the target; force-routing the broker itself, for a remote backend, is a later
slice, noted in the spec's parked questions). Seccomp: `Profile::WorkerNetClient`
must permit AF_UNIX `accept` + AF_INET `connect` — **DGX-verify** with the
kill-mode journalctl syscall enumeration (memory note
`dgx-seccomp-syscall-enumeration`), exactly as the egress-proxy was verified.

---

## Global constraints

- AGPL-3.0; AGPL-compatible deps only. No new non-compatible deps.
- Cross-platform Linux + macOS. The broker spawn is OS-agnostic (UDS on both);
  the bwrap/Seatbelt bind is Slice B1 (done). No new Linux-only code beyond what
  the backend resolver already gates.
- Files under ~500 LOC; pure functions in reusable modules (rule 1).
- TDD: failing test first (rule 2 + 6). Inline docs for a junior (rule 3).
- `source "$HOME/.cargo/env"` first; run cargo in the FOREGROUND (no background
  waits — subagents stall otherwise, memory note `subagent-foreground-cargo-tests`).
- `git add <specific files>` only, never `-A` (memory note
  `subagent-commit-add-specific-files`). Commit trailer:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
- **Verification gate (Mac):** `cargo build --workspace` exit 0; `cargo clippy
  --workspace --all-targets -- -D warnings` clean; targeted unit tests green.
  **DGX gate (owed, discharge before merge):** native `cargo test --workspace`
  (real bwrap + KVM + live PG) + workspace clippy; the new spawn/bind e2e green;
  seccomp AF_UNIX-accept+AF_INET-connect confirmed for the broker.

---

### Task 1 — `EmbedBrokerSpec` + `ToolEntry.embed_broker` field (pure, Mac)

**Files:** `core/src/scheduler/*` (wherever `ToolEntry` is defined),
`core/src/worker_manifest.rs` (if `Resolution` needs it).

- [ ] Define `pub struct EmbedBrokerSpec { pub endpoint: String, pub model: String }`
      (Clone, Debug) in a small module (e.g. `core/src/egress/embed_broker.rs` or
      a new `core/src/embed_broker/mod.rs`).
- [ ] Add `pub embed_broker: Option<EmbedBrokerSpec>` to `ToolEntry`, defaulting
      `None` at every existing construction site (grep the `ToolEntry { … }`
      literals — several workers). Byte-identical behaviour when `None`.
- [ ] TEST: a `ToolEntry` round-trips the field; `None` is the default.

### Task 2 — web-research manifest broker mode (pure, Mac)

**Files:** `core/src/workers/web_research.rs`.

- [ ] New env gate `KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1`. When set AND an
      embed endpoint is configured, `resolve` returns a `ToolEntry` with:
      `embed_broker: Some(EmbedBrokerSpec { endpoint, model })`, the embed host
      **removed** from `net_entries` (assert it is absent from `Net::Allowlist`),
      and the `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` env **not** injected.
- [ ] Precedence: broker mode is independent of `USE_MICROVM` — a VM worker can
      also use a broker (the broker runs host-side; the VM reaches it via the
      slice-4a vsock UDS bound as `embed_broker_uds`). Decide + test the VM×broker
      combination or explicitly defer it with a `log`/doc note.
- [ ] TESTS (mirror the existing `resolve_*` tests): broker mode drops the embed
      host + omits the endpoint env + sets `embed_broker`; unset ⇒ byte-identical
      to today (the existing 3 host tests stay green).

### Task 3 — `spawn_embed_broker` (mostly pure + hermetic, Mac; live bind = DGX)

**Files:** `core/src/embed_broker/spawn.rs` (mirror `egress/net_worker.rs`).

- [ ] `EmbedBrokerConfig { broker_bin: PathBuf, scratch_root: PathBuf }` +
      `from_env`/`resolve` (discover `kastellan-worker-embed-broker`, fail-closed
      if a broker-wanting worker has no binary).
- [ ] `spawn_embed_broker(cfg, spec, backend) -> Result<(EmbedBrokerSidecar,
      PathBuf /*uds*/), ToolHostError>`: mint `embed-<pid>-<seq>/` under
      `scratch_root` (reuse the `make_worker_scratch_dir` sun_path-cap pattern;
      share a prefix const with the #251 scratch sweep so husks are reclaimed),
      spawn the broker sandboxed with `Net::Allowlist([host_of(endpoint)])` +
      `Profile::WorkerNetClient` + env `KASTELLAN_EMBED_BROKER_UDS=<scratch>/embed.sock`
      + `KASTELLAN_EMBED_BROKER_ENDPOINT=<endpoint>`, wait for `embed.sock`
      (poll-with-deadline, like the egress proxy's `egress.sock` readiness).
- [ ] Fail-closed teardown: if the broker never binds, remove the scratch dir and
      `Err` (no half-spawned worker). `EmbedBrokerSidecar`'s `Drop` kills the
      broker + removes scratch.
- [ ] TESTS (hermetic): pure `host_of_endpoint` + `Net::Allowlist` shape; scratch
      naming/sun_path cap; a fake-binary "never binds" path → `Err` + scratch
      cleaned. **DGX:** a real broker binds `embed.sock` and a jailed worker
      connects (rides into the Task-5 e2e).

### Task 4 — chokepoint wiring + `SupervisedWorker.embed_broker` (Mac build/clippy; DGX live)

**Files:** `core/src/worker_lifecycle/{manager,idle_timeout,force_route}.rs`,
the `SupervisedWorker` struct, `core/src/main.rs` (build the `EmbedBrokerConfig`).

- [ ] Add `embed_broker: Option<EmbedBrokerSidecar>` to `SupervisedWorker` (RAII).
- [ ] Insert the broker spawn **before** `spawn_worker_maybe_forced` in the single
      cold-spawn chokepoint both lifecycle managers call; thread `EmbedBrokerConfig`
      alongside `ForceRoutingConfig`.
- [ ] Regression test: a force-routed + broker-backed spec keeps `embed_broker_uds`
      + the injected env after `rewrite_worker_policy` (pure-policy assertion, Mac).
- [ ] `main.rs` builds `EmbedBrokerConfig` from env at startup (like force-routing).

### Task 5 — DGX end-to-end (Slice C overlaps here) + #431 minors

- [ ] `core/tests/embed_broker_egress_e2e.rs` (DGX-gated `#[ignore]`): real broker
      + jailed web-research worker; assert the worker reaches the broker over the
      bound UDS and (with a live Ollama) `ranking == "hybrid"` with **zero embed
      egress** (no embed host in the sidecar decisions). Rootfs/scripts if VM mode.
- [ ] Fold the [#431] Slice-A review minors that touch this wiring: map broker
      `INVALID_PARAMS`/`METHOD_NOT_FOUND` → non-`Transport` `EmbedError`; a
      per-connection read timeout on the broker serve loop; `CryptoProvider`
      install in the broker `main` if the backend is `https://`; dedup the
      reorder/count-check across `forward_embed`/`HttpEmbedder`/`BrokeredEmbedder`.

---

## Open decisions to lock before coding

1. **VM × broker** — does a `USE_MICROVM` web-research worker also get a broker
   (broker host-side, reached via the vsock UDS bound as `embed_broker_uds`), or
   is broker-mode host-only in v1? Recommend: host-only in v1, `log` a warning if
   both flags are set; VM×broker is a Slice-C follow-up (the vsock-UDS bind is the
   only extra plumbing).
2. **Long-lived shared broker vs per-worker 1:1** — v1 per-worker to match the
   egress-sidecar lifecycle (spec parked question); revisit only if spawn overhead
   shows up.
3. **Auto vs explicit broker opt-in** — v1 explicit env gate
   (`KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER`); an auto rule ("loopback embed
   endpoint + force-routing/VM ⇒ use a broker") is a later ergonomic win.

Spec: `docs/superpowers/specs/2026-07-09-embed-broker-sidecar-design.md`.
