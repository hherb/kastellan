# kastellan — Development Roadmap

Sequenced feature list. Items are added whenever a feature decision is made
and ticked off as they ship. Order reflects expected build sequence — earlier
items unlock later ones.

> **How to update.** When we agree on a new feature, append it to the most
> appropriate phase (or a new phase) at the position that respects
> dependencies. When a feature ships, change `[ ]` → `[x]` and condense its
> entry to a **terse one line** with the commit/PR hash — enough to document
> the build sequence, no more. Pending `[ ]` items keep their full design
> context. **Pure refactors, test-module lifts, file splits, clippy/CI gates,
> flake fixes, and isolated bug fixes are NOT recorded here** unless they're
> load-bearing for remaining work — git history and the handover archives
> (`handovers/archive/`) are the durable record for those.

---

## Phase 0 — foundations (COMPLETE, archived)

Workspace skeleton, the Linux bwrap backend (+ the AppArmor `unprivileged_userns`
workaround), the macOS Seatbelt port, `kastellan-protocol` and the first sandboxed
worker, Postgres bring-up, the audit log and the LLM-router stub — all shipped
between `140eec5` and 2026-05-10, with no open items left.

**The "Phase 0 cont. — Service supervisor" section below is deliberately NOT
archived**, despite the Phase-0 heading: it is where the supervisor / installer /
operator-env track still accumulates work (#458, #504, #531, #552 all landed there
in 2026-08). The heading is historical, the content is live.

Per-item detail and commit hashes: [`archive/roadmap_phase0.md`](archive/roadmap_phase0.md).

## Phase 0 cont. — Service supervisor

- [x] Linux `systemd --user` unit generator + `systemctl --user` driver (`supervisor/src/systemd_user.rs`) — 2026-05-10
- [x] macOS LaunchAgent plist generator + `launchctl bootstrap` driver (`supervisor/src/launchd_agents.rs`) — [#353](https://github.com/hherb/kastellan/issues/353) — 2026-05-08
- [x] Core daemon `ServiceSpec` (`specs::core_service_spec`) + cross-OS `default_probe()` + e2e against the real binary — 2026-05-09
- [x] `kastellan.target` that brings up Postgres + core — 2026-06-06
- [x] Auto-restart with backoff on worker crash (Option K). — 2026-06-07
- [x] **One-command operator install** — [#568](https://github.com/hherb/kastellan/pull/568), [#316](https://github.com/hherb/kastellan/pull/316), [#458](https://github.com/hherb/kastellan/issues/458) `2519e4eb` — 2026-08-15

## Phase 1 — Memory & Loop

- [x] **Dispatcher chokepoint invariant** — 2026-05-10
- [x] **`memory::recall` — semantic + lexical lanes** — 2026-05-10
- [x] **Graph lane in `memory::recall`** — `76fe940` — 2026-05-13
- [x] **Embedding router** — 2026-05-12
- [x] **Scheduler (CASSANDRA Phase 1)** — `93da413` — 2026-05-11
- [x] **`ToolHostStepDispatcher`** — 2026-05-11
- [x] **Scheduler audit-row coverage** — 2026-05-12
- [x] **`cli_ask_e2e` full-chain pin** — 2026-05-11
- [x] **First real `ConstitutionalGuard` rule** — `67d29a0` — 2026-05-15
- [x] **First real `DeterministicPolicy` rule** — 2026-05-15
- [x] **Observation / rule-iteration harness** — 2026-05-13
- [x] **Constitutional refusal state** — `f1fea54` — 2026-05-14
- [x] **Automatic classification-floor inference** — `4ddfe3b` — 2026-05-16
- [x] **Memory layers — L1 always-in-context index** — `eb8e4bd` — 2026-05-15
- [x] **L0 seed data loader** — 2026-05-16
- [x] **Prompt assembler (L0 + L1 + base)** — 2026-05-16
- [x] **Recall-lane wiring** — `7553404` — 2026-05-17
- [x] **L1 promotion writer** — `eb6b8a8` — 2026-05-18
- [x] **Worker lifecycle policy** — `b7dba3a` — 2026-05-18
  - [ ] **Slice 3 (operator surface + SIGTERM grace)** — `kastellan-cli supervisor status` for warm workers + cap state; formal SIGTERM-grace-then-SIGKILL teardown via `grace_period_seconds`; proactive SIGCHLD crash detection. Low priority until a worker needs one of these.
- [x] **GLiNER-Relex worker** — [#650](https://github.com/hherb/kastellan/issues/650), [#613](https://github.com/hherb/kastellan/issues/613), [#659](https://github.com/hherb/kastellan/issues/659) — 2026-05-18
  - [ ] **operator-CLI macOS validation** (operator action): install Postgres locally (`brew install postgresql@17 && brew services start postgresql@17`) and rerun `KASTELLAN_GLINER_RELEX_ENABLE=1 cargo test -p kastellan-core --test gliner_relex_e2e -- --nocapture` to exercise the full PG-backed lifecycle path on darwin. Python `_resolve_device` is already cross-validated; this is the lifecycle-manager validation. Half-hour once PG is installed.
- [x] **Entity extraction v2** — `f12b460` — 2026-05-19
- [x] **Memory-write-time entity auto-linker** — `d58ecc9` — 2026-05-19
- [x] **Operator quarantine-review CLI** — `028e541` — 2026-05-20
- [x] **Relation-label vocabulary** — `5bcd060` — 2026-05-21
- [x] **Vocabulary management CLIs** — 2026-05-22
- [x] **`relations show <entity-id>`** — `9a46e18` — 2026-05-23
- [x] **macOS Apple `container` micro-VM backend** — 2026-05-21
- [ ] **`context_manager`**: token-budget + task-completion + wall-clock reset triggers. **Design borrowed from openworker's `coworker/compaction.py`** (auto-compaction spec, 2026-07-28), whose five load-bearing choices are all ones a naive implementation gets wrong: (1) the trigger is `min(threshold_pct × context_window, cap_tokens)` (0.8 / 250 k) — the **cap** is the non-obvious half, and exists so a 1 M-context model still compacts early, because quality and latency degrade well before the nominal limit; (2) the newest slice kept verbatim is a **token fraction of the trigger (0.25), not a turn count** — one huge tool loop must not starve the working set; (3) user messages are preserved **mechanically** (clipped, capped to the newest N), never left to the summarizer to remember, *and* the cap is load-bearing — without it the preserved block re-grows across repeated compactions until it reclaims the window it just freed; (4) tool results are the first casualty ("a file read 40 turns ago is better re-read") — Kastellan is strictly better placed here, since an oversized result is already a `{handoff_ref, summary_head}` placeholder (`core/src/handoff.rs`), so compaction drops the head and **keeps the ref**, leaving the body re-fetchable instead of destroyed; (5) **the persisted transcript is never modified — only the outbound view is.** That last one is not a preference for us: `audit_log` is the record of what the agent was told, and a compactor that rewrote history would break the property the whole audit surface exists to provide. The summarizer call itself runs tools-off with a modest ceiling (~3 k). **Two Kastellan-specific constraints their design has no analogue for:** the summary is LLM output re-entering the prompt at *higher* trust, so it must pass `escape_untrusted_body` at the same site the `<recalled>` / `<l1_insights>` bodies do (adversary #6 in the threat model), and the summarizer call is a `llm-router` dispatch like any other — it needs its own audit row and must respect the task's `data_ceiling`, or compaction becomes an unaudited egress of the whole conversation.
- [ ] **Tiered session digests — fill `MemoryLayer::L4`** ([#629](https://github.com/hherb/kastellan/issues/629)). **Design borrowed from Headlong's `design/tiered_memory.md`** (cross-project study, 2026-08-27; write-up in [`docs/devel/notes/2026-08-27-headlong-borrowings.md`](notes/2026-08-27-headlong-borrowings.md) §4). L4 is declared in `db/src/memories.rs`, accepted by the DB, and written by nothing; `prompt_assembly::assemble` has no episodic block at all, so cross-task continuity today is whatever L2 row someone happened to write. A logarithmic rollup pyramid over `audit_log` fixes it in bounded space: fanout `F=10`, tier *k* covers `F^k` rows, `⌈log_F N⌉` tiers, higher tiers rolled up **from the tier below** (not re-summarised from raw — that is what makes it exponentially cheap), **built forward-only from a start marker** so enabling it costs nothing up front. Three properties to keep: the raw log is the source of truth and the tiers are *an index, not testimony* (each rollup cites `audit_log.id` ranges so the agent can drill); the system must still work with every rollup deleted; and the new `<history>` block is model-authored, so it escapes via `escape_untrusted_body` like `<recalled>`, **not** verbatim like `<l0_meta_rules>`. Pairs with `context_manager` above (same paging problem: window = fast memory, log = backing store). Depends on [#628](https://github.com/hherb/kastellan/issues/628) for cheap citation. **Not** adopting Headlong's `unified_progressive_resolution_memory.md` (an HNSW-style redesign of the whole memory model, and unimplemented on their side too) — L4 digests are purely additive.
- [ ] **Reset snapshot writer** (compact context → memory before reset) — writes through the L1 promotion path (embed-lazily-after-dedup, degrade-and-warn), never a raw `insert_memory`, so a snapshot is recallable and inherits the L0 `PolicyViolation` guard.
- [x] **Worker-output prompt-injection guard (slice 1)** — `62905ae` — 2026-05-28
- [x] **Opaque secret references (slice 1)** — `bc36e4c` — 2026-05-29
- [x] **L3 skill arc** — `6eb966e` — 2026-05-31
- [x] **Developer onboarding manual** — `99bbfab` — 2026-05-25
- [x] **Large-tool-result handoff cache** — [#198](https://github.com/hherb/kastellan/issues/198), [#200](https://github.com/hherb/kastellan/pull/200) — 2026-06-08
- [x] **Registry-driven `<tools>` planner block** — [#437](https://github.com/hherb/kastellan/pull/437), [#533](https://github.com/hherb/kastellan/issues/533), [#543](https://github.com/hherb/kastellan/issues/543) `bc9c9a67` — 2026-07-11
- [x] **Memory two-tier write path: `put_doc()` vs `put_doc_light()`** — [#195](https://github.com/hherb/kastellan/pull/195), [#196](https://github.com/hherb/kastellan/issues/196) `39a036a` — 2026-06-07
- [x] **L1 embedding population — semantic recall lane populated** — [#323](https://github.com/hherb/kastellan/issues/323), [#324](https://github.com/hherb/kastellan/pull/324) `2ec853a` — 2026-06-20
- [x] **L1 embedding backfill — `kastellan-cli memory l1 reembed`** — [#323](https://github.com/hherb/kastellan/issues/323), [#325](https://github.com/hherb/kastellan/issues/325) — 2026-06-21
- [x] **Entity-embedding backfill + entity-similarity recall lane** — [#335](https://github.com/hherb/kastellan/pull/335) `4f4d61c` — 2026-06-21
- [x] **Agent tool-loop recovery (PR [#337](https://github.com/hherb/kastellan/pull/337))** — [#337](https://github.com/hherb/kastellan/pull/337), [#338](https://github.com/hherb/kastellan/issues/338) — 2026-06-22
- [x] **Forward entity embed-on-insert** — 2026-06-21
- [x] **Feed successful tool output back to the planner ([#338](https://github.com/hherb/kastellan/issues/338))** — [#338](https://github.com/hherb/kastellan/issues/338), [#339](https://github.com/hherb/kastellan/issues/339), [#340](https://github.com/hherb/kastellan/issues/340) — 2026-06-22
- [x] **Global budget for `plans_so_far_summary` ([#339](https://github.com/hherb/kastellan/issues/339))** — [#339](https://github.com/hherb/kastellan/issues/339) — 2026-06-23
- [x] **Clearer injection-blocked signal to the planner ([#340](https://github.com/hherb/kastellan/issues/340))** — [#340](https://github.com/hherb/kastellan/issues/340) — 2026-06-23
- [x] **Planner reliability on a local reasoning model — the agent could not answer ANY question about the user's email** — [#505](https://github.com/hherb/kastellan/pull/505), [#509](https://github.com/hherb/kastellan/pull/509), [#508](https://github.com/hherb/kastellan/issues/508) `6b083553` — 2026-08-02

## Phase 2 — Channels (read-only)

- [ ] **Conversational continuity for channel tasks** ([#701](https://github.com/hherb/kastellan/issues/701)). Every channel message becomes a task carrying only its own sentence; the conversation id routes the reply and nothing else, and recall contributes nothing, so a follow-up ("the last 3 flight bookings") has no referent. Measured live 2026-09-14 (tasks 187/188) and the cause of #677's original 185/186 pair. **Architectural, brainstorm first:** which turns and what time window; the bot's own earlier answers are tool-derived and re-enter as fenced untrusted data, never instructions; whether a follow-up sees the prior task's step outcomes (bounded, through the #702 view) or only its final answer; a follow-up inherits the most sensitive classification its earlier turns touched; prompt budget. Adjacent substrate: `context_manager` (Phase 1) and L4 session digests ([#629](https://github.com/hherb/kastellan/issues/629)). Acceptance: the same two DMs pass.

> **Primary channel decided 2026-06-12 (operator brainstorm):** **Matrix, self-hosted,
> single-user, federation OFF** (E2E via `matrix-rust-sdk` + `vodozemac`, vendor-neutral, zero
> marginal cost, all platforms via Element). **Email is the cross-transport fallback** (separate
> failure domain), used for low-trust async notifications, never commands *(superseded 2026-07-28 for inbound: the shipped slice-1 channel does ingest commands, gated by DMARC + per-pairing token — see the email entry below)*. Signal (`presage`
> fragility + ban risk) and Telegram (no bot E2E, centralized) rejected as primary. Homeserver
> runs as a supervised **conduwuit** unit; hosting is operator-selectable, fail-down: Tier A
> dedicated VPS (preferred) → Tier B existing WireGuard VPS (co-host = shared blast radius with
> network ingress) → Tier C the kastellan host itself ("poor man's" default). Matrix has **no
> single-user homeserver failover** — redundancy is the cross-transport email fallback, not a
> second homeserver. Full design + co-hosting security analysis + slice decomposition:
> `docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`.

- [x] **Channel-bus abstraction (build first)** — 2026-06-12
- [x] **Matrix inbound** — [#321](https://github.com/hherb/kastellan/issues/321), [#348](https://github.com/hherb/kastellan/issues/348), [#350](https://github.com/hherb/kastellan/pull/350) — 2026-06-20
- [x] **Homeserver supervisor unit + hardening** — 2026-06-12
- [~] **Email inbound channel (fallback)** — **SLICE 1 MERGED 2026-07-31 (`bf8e850b`, PR #496); slices 2–3 open.** **DESIGNED 2026-07-28**, spec `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`, slice-1 plan `docs/superpowers/plans/2026-07-28-email-fallback-channel-slice1.md`. **Supersedes the original "IMAP inbound worker" framing:** localmail now exists and already ingests the mail over IMAP, so the channel polls its `/v1` subscription instead of speaking IMAP itself — no IMAP crate to sandbox, no mail credentials in a kastellan jail, and it reuses the force-routed egress path (private netns + 1:1 sidecar) that #491/#492 built out. **The extra-CA half of that path did NOT apply here until PR [#503](https://github.com/hherb/kastellan/pull/503)** — the channel's sidecar was a *transparent tunnel* with no MITM leg, so `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` governed only the separate `kastellan-worker-mail` TOOL. On that branch the channel intercepts unconditionally and the env var applies to it too — but note the map is **global and host-keyed**, so one entry reaches every worker resolving to that address. The 2026-06-12 security posture is kept in full (DMARC pass + per-pairing in-body token), with the gate as **pure fns in core** so every rejection still lands in `audit_log`. Two single-origin workers (inbound poller, SMTP sender) is **forced by #492**: one worker holding both self-signed localmail and a public SMTP host is `MixedAllowlist`, which fails the spawn. **Pairing is operator-only** (`pair issue-token`) — the right posture for a spoofable transport. **Correction (review, spec D8):** the original reasoning here — "the token lives on the pairing row, so an unpaired sender can never present a valid one, hence the carve-out is unreachable over email" — was **WRONG and is refuted by the code**. An unpaired sender resolves to `Ok(None)` → `Rejected`, which *does* reach the carve-out, and `try_pair` would then mint a NULL-token row, permanently disabling DMARC+token for that address. Operator-only pairing is therefore enforced by an **explicit guard**, not by construction: `bus::handle_inbound` consults the carve-out only when `msg.evidence.is_none()` (i.e. only for a transport that authenticates its own peers, e.g. Matrix), and `DbPeerAuthorizer` refuses a token-less pairing row whenever the transport *did* supply evidence. Both guards are load-bearing — do not delete them as redundant. **SLICE 1 MERGED to `main` as `bf8e850b` (PR [#496](https://github.com/hherb/kastellan/pull/496)), 2026-07-31** (code-complete 2026-07-29 on branch `feat/email-fallback-channel-inbound`; merged content code-identical to the reviewed `658924ae`): sandboxed `email-in` worker, pure DMARC+token gate + evidence enforcement at the bus authorization chokepoint, core-side `EmailChannel` over the channel-generic `PolledWorkerDriver`, hermetic e2e (`core/tests/email_channel_e2e.rs`, 8 cases incl. the header-order-bypass and skipped-id-cursor-wedge fixes), and daemon wiring (`core/src/main/email_boot.rs`, mirrors `matrix_boot.rs`): a real `EmailEgress` (force-routed 1:1 sidecar when `KASTELLAN_EGRESS_FORCE_ROUTING` is on; the same legacy `--share-net` path Matrix takes when it's off — same rule, same invariant, no divergence) and a real `AckOnlyAudit` closure writing `channel.skipped_ack_only` rows for every silently-discarded message id. A partial config or worker-spawn failure **disables the email channel with a loud `error!` and leaves the daemon running** (`spawn_email_channel -> Option<ChannelBus>`, no `Result`, so no `?` can reinstate an abort) — the fallback channel must never be able to take Matrix, the scheduler, or the graceful-shutdown path down with it; design §6 says the daemon refuses to start *the email channel*, not the daemon. Corrected from an earlier abort-on-`?` posture by the final whole-branch review; see `email_boot.rs`'s module docs. Operator env block + traps documented in `core/src/install/plan.rs::render_email_help`. **KNOWN GAP found in review — CLOSED by PR [#503](https://github.com/hherb/kastellan/pull/503) (2026-08-01; DGX gate GREEN at 2938/0/53, clippy clean):** the force-routed sidecar was a transparent tunnel with no MITM leg, so a self-signed localmail was unreachable by this channel, force-routed or not. #503 gives `spawn_net_transport` a caller-chosen `Mitm` posture, has the channel intercept unconditionally, and selects the anchor once at boot. **Two limits survive:** the anchor map is GLOBAL and host-keyed (one entry reaches every worker on that address — the DGX shares `10.0.0.3` between localmail `:8443` and SearxNG `:8888`), and interception is the *precondition* for the #3b leak scanner, **not** coverage — nothing on this path is scanned ([#501](https://github.com/hherb/kastellan/issues/501)). **Post-PR review round (2026-07-30) — 9 findings, all fixed, DGX-reverified.** Two more High-severity defects in the same families as the pre-PR ones: (a) the gate was **still bypassable** via a legal RFC 8601 `method-version` — `method = Keyword [ [CFWS] "/" [CFWS] method-version ]` (§2.2) means `dmarc/1=fail` is a legal *real* verdict, which the parser read as method `dmarc/1` and therefore did not count, so the more-than-one-dmarc rule never fired and a forged unversioned `dmarc=pass` decided (confirmed a live ADMIT against the real code on the DGX before fixing); (b) a **401/403 on `message_detail` destroyed the message**, because `is_permanent` called it permanent ⇒ acked ⇒ monotonic cursor past it, though both are retryable after an operator action. Three Medium: `email.poll` was a 250ms poll loop, not a long poll (~4 req/s ⇒ ~345k requests, TLS handshakes and `egress.allowed` audit rows per day on an *idle* inbox — `POLL_INTERVAL` now 5s, ratio test-pinned); `channel.replied` asserted deliveries that never happened, so a new **`channel.reply_undelivered`** action is written when the transport refuses (in slice 1 `EmailChannel::send` fails unconditionally, so every email reply is routed-but-undelivered until slice 2); and the per-family-bus `senders` miss logged a misleading `warn!("…dropping")` on every delivered reply (now `debug!`; bus unification filed as [#497](https://github.com/hherb/kastellan/issues/497)). Four Low, incl. a removed comment now substituting a SPACE — deleting one *welded* tokens, so `mx(x)example.net` collapsed to `mxexample.net` and matched that configured authserv-id. **Also newly documented, and not fixable at this layer:** a non-escaping MX can be made to swallow its own verdict, leaving a forged pass as the only methodspec — which is precisely why the per-pairing token, not the DMARC verdict, is what makes the gate hold. See HANDOVER for the full list. **Slice 1 is DONE and on `main` — nothing outstanding at this tier.** The authoritative DGX-native gate at `658924ae` was GREEN: **DGX aarch64 2926/0/53**, `cargo clippy --workspace --all-targets -- -D warnings` clean, only 4 `[SKIP]` (the one env-gated GLiNER tier); Mac 2808/0/23. The localmail dependency (Task 1) **merged 2026-07-30** as [hherb/localmail#223](https://github.com/hherb/localmail/pull/223) `0b6c5e05`. **Deployment is a separate gate:** the DGX's running `localmail-serve` predates that merge commit by three days, so it still serves the pre-cursor API and must be restarted before a live slice-1 deployment; the known MITM/self-signed gap above also applies to a live tier. Slices: **2** SMTP outbound + full round trip (`lettre`); **3** DGX deploy + live tier — the MITM + extra-CA half of slice 3 landed early in PR [#503](https://github.com/hherb/kastellan/pull/503), so slice 3 is now the `localmail-serve` restart, the deploy, and the live run.
- [x] DM pairing flow: — 2026-06-12
- [x] **Channel bring-up is supervised, not one-shot** — [#514](https://github.com/hherb/kastellan/issues/514), [#516](https://github.com/hherb/kastellan/pull/516), [#502](https://github.com/hherb/kastellan/issues/502) `a21f8276` — 2026-08-04
- [ ] ~~Telegram inbound adapter (`grammers`, Rust)~~ — **rejected as primary 2026-06-12** (no bot E2E, centralized, ban risk). Could return later as an additional `Channel` impl if a need arises.

## Phase 3 — Channels outbound + browser + web

- [x] Egress proxy (per-worker host allowlist, TLS pinning, audit logging) — [#241](https://github.com/hherb/kastellan/issues/241), [#251](https://github.com/hherb/kastellan/issues/251), [#406](https://github.com/hherb/kastellan/pull/406) `df51c5c` — 2026-06-10
- [x] **Credential-leak scanner co-located in the egress proxy** — [#268](https://github.com/hherb/kastellan/issues/268) — 2026-06-12
- [~] **Matrix outbound** (agent → user replies over the E2E `MatrixChannel`) — primary outbound path (decision 2026-06-12; slice #4). **Reply mapping shipped** (2026-06-12): `route::reply_body` surfaces the agent's real completion `plan.result` (`{"kind":"text","body":...}` → the body; `message` alias; compact fallback) and maps `error`/`blocked`/`refused` to safe sentences — fixing the slice-#1 stub that mis-handled the real shape. Live delivery rides slice #2 Phase D. (~~Telegram/Signal outbound~~ rejected as primary — see Phase 2 note.)
- [ ] SMTP outbound in mail worker (`lettre`) — fallback-channel outbound; low-trust notifications, never the primary command path (slice #5) — this is **email-channel slice 2** (slice 1, the gated inbound half, merged `bf8e850b`); until it lands `EmailChannel::send` fails unconditionally and email replies audit as `channel.reply_undelivered`
- [x] **`kastellan-worker-mail` — read-only personal mail-archive tool** — [#483](https://github.com/hherb/kastellan/pull/483), [#487](https://github.com/hherb/kastellan/pull/487), [#490](https://github.com/hherb/kastellan/pull/490) `ce144513` — 2026-07-22
- [x] `web-fetch` worker: — 2026-06-08
- [x] `web-search` worker (SearxNG default) — 2026-06-09
- [x] **`web-research` composite worker** — [#421](https://github.com/hherb/kastellan/pull/421), [#422](https://github.com/hherb/kastellan/pull/422), [#426](https://github.com/hherb/kastellan/issues/426) `b067b22` — 2026-07-08

- [x] **embedding-broker sidecar** — [#427](https://github.com/hherb/kastellan/issues/427), [#430](https://github.com/hherb/kastellan/pull/430), [#431](https://github.com/hherb/kastellan/issues/431) `b077629` — 2026-07-09
- [x] **search-broker sidecar (web-search × force-routing × loopback SearxNG)** — [#440](https://github.com/hherb/kastellan/pull/440) `3d92fa6` — 2026-07-12
- [x] **browser-driver Firecracker micro-VM entry (4th and last single-use net worker)** — [#470](https://github.com/hherb/kastellan/pull/470), [#472](https://github.com/hherb/kastellan/pull/472), [#474](https://github.com/hherb/kastellan/pull/474) `8dbff298` — 2026-07-19
- [x] **web-search Firecracker micro-VM entry (3rd Mechanism-2 consumer)** — [#451](https://github.com/hherb/kastellan/pull/451) `02985295` — 2026-07-15
- [x] **forced-endpoint resolve-time guard (#452 + #429) + the literal-IP carve-out correction** — [#457](https://github.com/hherb/kastellan/pull/457), [#460](https://github.com/hherb/kastellan/pull/460), [#459](https://github.com/hherb/kastellan/issues/459) `77e016f6` — 2026-07-16
- [x] **generic forced-localhost guard (#459 slice 1)** — [#462](https://github.com/hherb/kastellan/pull/462), [#467](https://github.com/hherb/kastellan/pull/467), [#469](https://github.com/hherb/kastellan/pull/469) `6584b87a` — 2026-07-17
- [x] **web-research × search-broker (single-broker XOR)** — [#465](https://github.com/hherb/kastellan/pull/465), [#441](https://github.com/hherb/kastellan/pull/441), [#444](https://github.com/hherb/kastellan/pull/444) `0e5ee0a4` — 2026-07-17
- [x] **injection-guard per-tool profiles ([#142](https://github.com/hherb/kastellan/issues/142))** — [#142](https://github.com/hherb/kastellan/issues/142) — 2026-06-09
- [x] `browser-driver` worker (Playwright headless, dedicated profile, scratch FS) — [#280](https://github.com/hherb/kastellan/issues/280), [#281](https://github.com/hherb/kastellan/issues/281), [#263](https://github.com/hherb/kastellan/issues/263) `2d85ea1` — 2026-06-14
- [ ] **MCP onboarding: discover → boot-spawn → validate → persist** — when kastellan grows third-party MCP-server support (any of the registries openhuman taps: Smithery, `modelcontextprotocol/registry`), naive "spawn it with the operator's intended policy" is a foothold attack: a malicious MCP server gets its production sandbox on first run. Adopt openhuman's pattern (`docs/MCP_SETUP_AGENT.md` — "boot-spawn for this one server... spawns the candidate subprocess in a scratch workspace"): every newly-discovered MCP server is first booted under a **maximally restrictive** `SandboxPolicy` (`Net::Deny`, `fs_read=[]`, `fs_write=[scratch]`, `Profile::Strict`, `cpu_ms=5000`, `mem_mb=128`), driven through `initialize` + `tools/list` over our existing `kastellan-protocol` stdio JSON-RPC, recording the declared tool surface to `db::mcp_servers` (new migration) only on success. Only then does the operator promote the server to its intended runtime policy via a separate explicit step that lands one `actor='cli' action='mcp.promoted'` audit row carrying the SHA-256 of the policy that was approved. Production runs refuse to spawn an MCP server whose policy hash has changed since promotion (mirror of the `tool_allowlists` SHA-256 drift detection from PR #51). Cross-platform "free" via `SandboxBackend` — same flow on bwrap, Seatbelt, and the new `MacosContainer` backend (Issue #55).
- [ ] **Operator ask channel ("the Inbox") — and the discharge of the `Verdict::Escalate` TODO** ([#564](https://github.com/hherb/kastellan/issues/564)) — the channel bus has been deployed since slice 5b-4b, yet `core/src/scheduler/inner_loop.rs`'s `Verdict::Escalate` arm still degrades to `Block` behind a `TODO(channel-bus)`. The reason is structural, not neglect: `channel/bus.rs` is strictly *inbound task → outbound reply on completion* (`route::reply_for_completed_task`, driven off the completed-task `NOTIFY`). Nothing in the bus lets **core initiate** an outbound question and suspend a task until a correlated answer arrives — so the one verdict CASSANDRA has for "a human must decide this" is unrepresentable at runtime and silently becomes a refusal. Every human-in-the-loop feature below is blocked on the same missing primitive. **Pattern: openworker's `coworker/inbox.py` + `inbox_routing.py`** — a durable, cross-surface human-attention queue whose item is `approval | question | notification`, whose state machine is `pending → resolved` **exactly once, idempotent, first-responder-wins**, and whose id rides in the delivered message so a reply from any transport resolves the same record. Their framing is worth keeping verbatim: the store is the record, messaging channels are *transports of the same item*.
  - **Kastellan shape.** New `asks` table behind `db::asks` (`{id, task_id, kind, body, options, nonce, state, created_at, deadline_at, resolved_at, resolved_by, resolution}`), with resolve as a **guarded `UPDATE … WHERE state='pending'` returning rows-affected** — the same race-safe idiom `set_embedding` / `set_entity_embedding` already use, which buys first-responder-wins with no lock and no second surface to keep consistent. The task suspends into a new `tasks.state='awaiting_operator'` that the lane runner does not claim, **with the lease released** — otherwise a suspended task holds a lane slot and the crash-recovery sweep reaps it as a zombie. Resolution re-enqueues through the existing `LISTEN/NOTIFY` path: the Postgres `tasks` queue *is* the operator→daemon command channel (#179's finding), so this adds no IPC.
  - [x] **Slice 1a SHIPPED — the durable record, DB layer only** — [#570](https://github.com/hherb/kastellan/pull/570), [#571](https://github.com/hherb/kastellan/issues/571) `06c65ae7` — 2026-08-17
  - [x] **Slice 1b SHIPPED — the ask path; `Verdict::Escalate` no longer degrades to `Block`** — [#571](https://github.com/hherb/kastellan/issues/571), [#575](https://github.com/hherb/kastellan/issues/575), [#576](https://github.com/hherb/kastellan/issues/576) — 2026-08-18
  - [x] **Slice 2 SHIPPED — MERGED as [#579](https://github.com/hherb/kastellan/pull/579) (`bb937df7`), 2026-08-20; gated + deployed the same day — the ask reaches the operator's Matrix room and is answered there.** — [#579](https://github.com/hherb/kastellan/pull/579), [#580](https://github.com/hherb/kastellan/issues/580), [#581](https://github.com/hherb/kastellan/issues/581) `bb937df7` — 2026-08-20
  - **The correlation token must be unforgeable, which their design does not need and we do.** openworker embeds a plain item id (`[ow:<hex>]`) and is safe only because the transport is a single-user desktop app. On a Matrix room any peer who can send can guess an id and resolve someone else's approval. So: a random per-ask **nonce**, matched only against `pending` asks whose owning task belongs to a peer `channel::auth` already authorized — id and authority kept separate.
  - **The answer is data, never instructions.** The inbound body is untrusted like any other channel message (it goes through `screen_and_classify`), so resolution is a **closed set** — `approve|deny` for an approval, an index into `options` for a question. Free text is stored for the audit row and shown to the operator; it is never interpolated into a plan. This is the difference between an ask channel and a prompt-injection funnel aimed straight at the reviewer's own decision.
  - **A deadline is mandatory here and absent there.** A desktop app can leave an item pending forever because a human will eventually look at the window; a headless daemon cannot. Every ask carries `deadline_at`; on expiry the task fails closed with an `ask_timeout` outcome and an audit row, rather than holding state indefinitely.
  - **Companion axis — autonomy ceiling per task.** Copy openworker's separation exactly (`coworker/unattended.py`): *unattended mode changes **where the human is reached**, not what the agent may do* — the ceiling is the permission mode (`Mode::{DISCUSS, PLAN, INTERACTIVE, AUTO, CUSTOM}`), an independent axis. Kastellan is daemon-first, i.e. **always** unattended, which makes the distinction load-bearing rather than cosmetic: today "no human is watching" and "the agent may act" are the same implicit state. A `tasks.autonomy` column (defaulted from the lane, overridable per task) gives CASSANDRA a ceiling to check against instead of inferring one.
  - **Dead-letter half** (their `coworker/unrouted.py`): asks that expire unanswered, and background replies with nowhere to go, land in a queryable store rather than a log line. Kastellan already audits `channel.reply_undelivered` — this turns that from a grep into an operator surface.
  - **Follow-ons that become cheap once the primitive exists, and should NOT precede it:** an `ask_user`-shaped planner tool for genuinely ambiguous tasks (their `tools/ask.py`, itself modelled on Claude Code's `AskUserQuestion` — question + quick-reply options + free-text escape, and grouping up to 4 questions into **one** call so a clarification costs one round trip rather than four), and `propose_plan`-style plan approval (their `tools/plan.py`, where approval flips the live permission mode out of read-only *in the same session*, keeping the exploration context). Both are the same Inbox item with a different `kind`.

## Phase 4 — python-exec & agent-authored skills

- [ ] `python-exec` worker: scratch FS only, no net, hard CPU/mem/wallclock; curated stdlib bind
  - [x] **python-exec shipped (host bwrap/Seatbelt) — 2026-06-12 to 06-26** — [#356](https://github.com/hherb/kastellan/issues/356) — 2026-06-12
  - [x] **Linux Firecracker micro-VM backend (`SandboxBackendKind::FirecrackerVm`) — SLICE 1 (2026-06-27, PR #364).** — [#370](https://github.com/hherb/kastellan/issues/370), [#373](https://github.com/hherb/kastellan/pull/373), [#375](https://github.com/hherb/kastellan/pull/375) `555f611` — 2026-06-27
  - [ ] **Follow-ups:** curated-wheels RO dir if/when the skill catalog demands packages; planner-prompt surfacing
    (parity note: the net workers have none either).
- [ ] Skill catalog (named/persisted Python skills) with optional human-approve gate
  - [x] **Slice 1 — crystallise + approval + operator CLI — 2026-06-13** — [#275](https://github.com/hherb/kastellan/pull/275) — 2026-06-13
  - [x] **Slice 2 — invocation + surfacing — 2026-06-13** — [#276](https://github.com/hherb/kastellan/pull/276) — 2026-06-13
  - [x] **Runtime params — 2026-06-14** — [#278](https://github.com/hherb/kastellan/pull/278) — 2026-06-14
  - [x] **Output secret-scrub (params battle-test follow-up) — 2026-06-17** — 2026-06-17
- [ ] **Skill trust enum** — `Untrusted | UserApproved | Pinned`, each level mapping to an explicit capability ceiling (which workers it may invoke, which net allowlists, which fs paths). Authorship and approval recorded in `audit_log`; promotion requires re-approval. (Pattern: IronClaw skill trust model — user-placed vs registry-installed. The L3 templated-skill arc above is the first concrete implementation of this shape.)
- [ ] **Operator-authored skills (`SKILL.md` folders) with progressive disclosure** — the *top-down* complement to the L3 arc, which is entirely bottom-up: an L3 skill is crystallised from a trajectory the agent already executed, so the agent can only ever re-use what it has already discovered. There is no way for the operator to hand it a procedure up front ("when triaging a referral letter, always …"). **Pattern: openworker's `coworker/skills/{base,store}.py`**, which implements the Anthropic `SKILL.md` format — a folder with YAML frontmatter (`name`, `description`, optional `allowed-tools`) plus a markdown body and optional resources. Three mechanics worth taking:
  - **Progressive disclosure**, the whole point: only `{name, description}` enters the prompt at assembly time; the body is fetched on demand through a reserved built-in. Kastellan already has both halves of the machinery — the `<skills>` block surfaces `UserApproved`/`Pinned` entries as reference-only, and `tool_dispatch`'s `handoff`/`fetch` interception is the precedent for a reserved built-in that answers **before registry lookup with no worker spawn**. A `skill/load` intercept is the same shape.
  - **Folder-is-truth scoping, with the disable state deliberately kept OUT of the folder** — global skills in the state dir, project skills under the workspace, so project skills are git-shareable for free; but a per-operator mute lives in personal state, because one user's disable must not be committed for everyone else. Small decision, easy to get wrong once and then be stuck with.
  - **Staged upload (parse → preview → confirm)** before anything lands in a scope dir — the operator reviews exactly what will be saved. Mirrors `memory l3 approve`'s posture.
  - **Trust treatment is the same, and non-negotiable:** the body is operator-authored text entering the planner prompt, so it takes the `escape_untrusted_body` + reserved-tag guard the python-skill `description` already gets (#533's lesson — anything rendered into `<tools>`/`<skills>` that did not come from a compiled-in literal is untrusted). Frontmatter `allowed-tools` is a **request, not a grant**: resolved against the live `ToolRegistry` at load, intersected with the existing allowlist, never additive — the same rule as `SkillTrust`, and the same rule openworker states for its own persona packages (a package declares what it wants; only the operator decides what it gets).
  - **Two findings from Hermes Agent's shipped version of this, both cheap and both traps** (survey [`docs/devel/notes/2026-09-06-hermes-agent-survey.md`](notes/2026-09-06-hermes-agent-survey.md) §3.5). **(i) A description budget must be a write-time validation error, never an assembly-time cut.** Their `SKILL.md` description limit is 60 characters *because* "the system-prompt skill index truncates the description to 60 chars and loads it every session, so anything past char 60 is silently cut and never routes" — a silently-truncated routing key, which is our kind of bug and the reason their own linter calls this "the most-violated rule and it is NOT cosmetic". Whatever budget the `<skills>` block picks, over-budget is a rejected write. **(ii) Progressive disclosure without a manifest makes the capability invisible.** Their live benchmarking found models "substituting visible core tools (running `gh` in the terminal instead of searching for the deferred GitHub tool) or declaring a capability nonexistent" until a name+description listing was embedded alongside the loader. The `{name, description}` half of the disclosure is not an optimisation of the index — it *is* the index, and dropping it to save tokens removes the routing signal, not the cost.
- [ ] **Skill lifecycle — usage telemetry, staleness, and a content-addressed mutation ledger.** The L3 templated-skill arc and the Python skill catalogue give us *creation* under a trust ladder that Hermes has no equivalent of. What we have nothing of is what happens after: a catalogue only grows, `memories` rows at `layer=3` accumulate, and there is no signal for "this skill has not been recalled in ninety days" or "these two are near-duplicates". **Pattern: Hermes Agent's `agent/curator.py` + `tools/skill_ledger.py`** (survey [`docs/devel/notes/2026-09-06-hermes-agent-survey.md`](notes/2026-09-06-hermes-agent-survey.md) §3.4): a deterministic LLM-free `active -> stale (30 d) -> archived (90 d)` pass, an *opt-in* LLM consolidation pass held off by default because it costs 50-100 model calls a run and makes broad structural changes, a usage sidecar (`use_count` / `view_count` / `patch_count` + timestamps), a snapshot before every mutating run, and an **append-only ledger of every mutation — by curator, by agent, and by the operator CLI alike — carrying per-file `{path, sha256}` before/after manifests whose contents are stored content-addressed and deduplicated by hash**, which buys single-entry rollback and can resurrect a hard-deleted skill. That ledger is the same content-addressed-spill shape as #678's slice (c) and should share its store rather than inventing a second one; note their explicit rule that the ledger "is telemetry, never a gate — if writing an entry fails, the mutation still goes through", which is the correct polarity for a record that is not a boundary. **Three of their design rules are better than the feature they serve.** (1) **Provenance is declared, never inferred** — only skills the *background* writer created are lifecycle-managed; skills created at the operator's request are the operator's, and adoption is an explicit act. They refuse to infer authorship from telemetry and say why: "a skill with thousands of patches proves the agent *maintains* it, not that the agent *wrote* it… An automatic 'looks agent-made, adopt it' heuristic would eventually archive something you hand-wrote." (2) **Never-recalled is not disposable** — `use_count == 0` gets a grace floor, since "zero uses is absence of evidence", and anything referenced by a schedule is exempt *including while that schedule is paused*. (3) **Worst case is archival, never deletion.** ⚠️ **Sequenced after the entries in Phase 5, deliberately:** Nous ship this feature on judgment — they have six eval suites and **not one of them measures whether a skill created from experience makes the next task go better** — so if we build the lifecycle we build its eval with it, on the battery from that entry.
  - **The background writer is the prerequisite, and it brings one operational lesson we need before anything else.** Their post-turn review is a forked agent that replays the conversation asking "should any skill or memory be saved?", inheriting the parent's runtime so it reuses the same prompt prefix, under a **dispatch-side tool whitelist** (memory + skill-management + read-only reads, with an opt-in named-tools list documented to "prefer tools that stage a proposal for human review"). On a cloud provider that fork is free; **on a single-GPU host it is not**, and they learned it the expensive way: "the same fork occupies the GPU your next prompt needs — for minutes on a large model — and sending a new prompt cancels it, **discarding the learning**." So on a managed local runtime the review is *deferred by default*: queued at turn end, run after a quiet settle window, coalesced per session, re-queued rather than dropped when preempted, and run anyway past a max age. **The DGX is exactly that host** — one llama-server, with the guard tier already competing for it — so any background learning work we add inherits this constraint on day one rather than discovering it.
- [ ] Optional micro-VM backend for `python-exec` (Firecracker on Linux, Apple `container` on macOS — discovery spike completed 2026-05-21, verdict COMMIT; see [`docs/superpowers/specs/2026-05-21-macos-container-spike-notes.md`](../superpowers/specs/2026-05-21-macos-container-spike-notes.md))
- [ ] **Tiered delegation policy with hard no-recursion ceiling** — when the scheduler grows subagent delegation (today everything is one inner loop), borrow openhuman's `docs/DELEGATION_POLICY.md` four-tier shape: Tier 1 reply-directly (no tools), Tier 2 direct tool, Tier 3 inline subagent (≤5 turns, no new thread), Tier 4 dedicated worker thread (>5 turns). **The structural constraint that matters: workers do not spawn workers.** Encode it in `tool_host` as a compile-time check (`SubagentContext: Sealed` newtype that can only be constructed from the root scheduler) so the spawn tree is provably finite and the audit log has bounded fan-out per task. Maps cleanly onto the existing `Lifecycle::{SingleUse, IdleTimeout}` shape: tier-3 inline subagents are `SingleUse`, tier-4 dedicated threads piggyback on `IdleTimeoutLifecycle`. Pre-req for any meaningful agent-authored-skills work; defines the budget per skill invocation.
- [ ] **Stability-scored preference learning** — when the agent starts inferring user preferences (style, vetoes, tooling, timezone, identity facts), a naive "remember whatever the latest message said" path is vulnerable: a single injected message in any channel permanently flips a long-standing preference. Adopt openhuman's `docs/AGENT_SELF_LEARNING.md` scoring shape: `stability(class, key, value) = base × cue × user_state`, evidence weighted by source (explicit user statement 1.0, structural data 0.9, behavioural heuristic 0.7, recurrence 0.6), only "Active" at stability ≥1.5 (requires corroboration). Storage: new `user_profile_facets` table behind `db::profile`, runtime-role can INSERT bounded candidate rows but only the explicit "operator pin" CLI surface (`kastellan-cli profile pin <class> <key>=<value>`) can promote a facet to Active or override automatic scoring. Keeps `memory access is core-only` invariant intact — workers never write profile state. Pre-req for any prompt-assembly surface that injects a `UserProfileSection` (current `assemble_system_prompt` ships an `L0`/`L1` block but no profile facets yet).

## Phase 5 — Frontier escalation, hardening, audit UI

- [ ] **Declared tool risk class + an operator-local override store** — the substrate the policy gate below needs, and the thing that makes third-party MCP tools (Phase 3, "MCP onboarding") governable at all. **Pattern: openworker's `coworker/risk.py` + `coworker/overrides.py`.** Their `RiskClass{READ, WRITE_LOCAL, EXEC, EXTERNAL}` replaced hardcoded `WRITE_TOOLS`/`SHELL_TOOL` *name sets* the permission engine carried inline: risk became "a declared property a single `classify` reads". Kastellan's equivalent knowledge is currently implicit and scattered — `shell-exec` is special-cased as the deny-by-default entry, `tool_allowlists.kind` distinguishes `argv0` from `domain`, and `cassandra::deterministic` reasons about data classification but has no notion of a tool's intrinsic *side-effect* category. Declaring a `RiskClass` on `WorkerManifest` (alongside the `AllowlistDecl { tool, kind }` that #545 just collapsed into one declaration, and for the same reason: two independently-defaulting sources of one fact is a state the type system should reject) gives the reviewer a first-class property instead of name matching, and gives the ask channel above its trigger condition — `is_consequential` = anything but a pure read.
  - **The override store, and its one inviolable rule.** Operator-local glob rules over tool names, most-specific-wins (literal-character count; an exact pattern beats any glob), resolved *before* the declared base. Their docstring states the rule that matters and states it as inviolable: **the store is user-local and is NEVER written by a persona or package.** A package declares what it wants; only the operator decides how far to trust it. For Kastellan that is exactly the MCP posture: a server's advertised tool surface is *data* recorded at boot-spawn time, while the risk class it runs under is **operator state**, set by an explicit `kastellan-cli policy risk set '<glob>' <class>` with an audit row, never derived from the server's own manifest. Every third-party MCP tool defaults to `External` (their conservative default, and correct), and relaxing a read-only one is a deliberate, logged act.
  - **Do not let this blur into containment.** Risk class drives **consent** — does this call need an operator ask. `SandboxPolicy` drives **containment** — what the call can reach if it is malicious. Two axes. openworker has only the first, which is precisely why a compromised tool there reaches the whole machine; borrowing their consent ergonomics must not import their habit of treating an approval prompt as a security boundary.
- [ ] **Target-bound standing grants** — the best idea in openworker's codebase, and the thing that makes an ask channel economically viable rather than an alarm everyone learns to dismiss. Their `permissions.py` standing rules are `"tool → target"` entries with three hard constraints: eligible for **external risk only** (never exec, never local write — "shell asks forever"); the tool must **declare which argument names its target** (`target_arg_for`, a static table — never a substring scan of the argument blob); and the call must actually name one. The grant lives **on the automation record**, so revocation is per-automation and deleting the automation takes its grants with it. The insight is that auto-approval is made safe by **binding it to one exact value** rather than by widening a tool's permission: `mail.send → horst@…` pre-approved for one recurring task is emphatically not `mail.send` allowed.
  - **Kastellan has no automation record yet** — `db::tasks` rows are one-shot, and no recurring-task entity exists — so v1 scopes a grant to a single `task_id` for that task's lifetime, stored with the task and deleted with it. Same ownership property, one entity down; when recurring tasks land the grant moves to the recurring definition unchanged.
  - **Enforcement rides the existing chokepoint** and is strictly additive: a grant may only ever turn an *ask* into an *allow*. It can never override a `Verdict::Block` or a `ConstitutionalBlock`, never apply to an `Exec`-risk tool, and never widen `tool_allowlists` — which stays the deterministic boundary it is today. Grant minting is itself an Inbox resolution ("allow every time, for this task, to this recipient"), so it lands one audit row naming the exact rule, and the rendered rule is what the operator saw, not a summary of it.
  - **Why it is a prerequisite and not a nicety:** without it, a recurring task raises the same approval every run, the operator approves reflexively, and the gate has negative value — it trains the habit it exists to prevent while still costing a round trip.
- [ ] **Retire truncation as the answer to "bigger than the budget" — map-reduce for the one class where that is right** ([#678](https://github.com/hherb/kastellan/issues/678)). *(2026-09-14: [#702](https://github.com/hherb/kastellan/pull/702) made the planner's per-step view a pruned, labelled value, so slice (e)'s anchor index can now harvest from the value rather than from key-stripped text; (c), (d) and (e) themselves remain.)* Truncation is currently doing **three structurally different jobs**, and sorting them is most of the work; only one becomes map-reduce. **(1) A control that stops seeing its evidence — map-reduce.** `SCAN_BYTE_CAP = 64 KiB` means an injection payload at offset 65 537 is invisible to the tier whose entire job is finding it, and the document is still delivered (a documented deferral in the 2026-09-02 audit). The reduce is trivially safe — `p = max(p_i)`, block if any chunk blocks — i.e. **strictly more sensitive than today and never less**, so nothing currently blocked becomes clear. It also removes the premise of two open issues rather than re-tuning around them: [#604](https://github.com/hherb/kastellan/issues/604) (the cap bounds BYTES not tokens, so a 64 KiB document can be 44k tokens and the adjudication fails HTTP 400 — chunking to a *token* budget makes that unreachable) and [#612](https://github.com/hherb/kastellan/issues/612) (D9 extrapolates the timeout linearly from a ~1 KiB sample, wrong by 4.4x on Metal, so a worst-case document times out and **fails open** — a bounded chunk needs no extrapolation at all; check against #612's measurement before claiming it). **(2) A record that must be faithful — spill, NOT map-reduce.** `truncate_payload`/`PAYLOAD_MAX_BYTES = 4096` drops `req` and `result` wholesale. **An audit row is testimony; a summarised audit row is a model's account of what happened, which is exactly what an audit log must not contain.** The fix is content-addressed spill — and the sha256 is **already computed and stored**, so the key exists and only the store is missing — plus a bounded, explicitly-marked inline head. Confirmed live on the DGX 2026-09-05: two DMs four minutes apart answered the same question with different booking references and it was impossible to establish which was grounded, while the *small* `shell.exec` row kept everything and is what made [#677](https://github.com/hherb/kastellan/issues/677) diagnosable — **the selection is backwards, truncation drops exactly the rows carrying the evidence a long answer was built from** ([#617](https://github.com/hherb/kastellan/issues/617), [#621](https://github.com/hherb/kastellan/issues/621)). **(3) A resource guard — must STAY and be labelled.** `MAX_RECORD_BYTES`, `MAX_CARRY_BYTES`, `PER_TASK_BYTE_BUDGET`, `MAX_FETCH_BYTES` are containment against a compromised worker driving the core to OOM, not context management; removing them in the name of "no more truncation" would be a containment regression. **Where the map-reduce lives: `core/src/handoff.rs` is already half of it** — an oversized result is stashed WHOLE (64 MiB per-task budget) behind a `{handoff_ref, summary_head}` placeholder, so the body is not lost, only unusable; what is missing is the reduce (`handoff.query(ref, question)` → chunk → per-chunk extraction against the task's actual question → answer + cited offsets). Same backing store, no new storage. Pairs with **`context_manager`** (Phase 1), which pages the *conversation window* where this pages *one oversized artefact* — same backing store, different axis, and that item's point (4) already anticipates it. **Constraints that must not be lost:** every chunk is attacker-controlled text and the map step is a model call over it, so chunk outputs reach the reduce as escaped **data** (the L1-insight rule), never instructions; the polarity **inverts to fail-closed** — today a document past the cap is silently unscreened, and in the new design an errored or timed-out chunk must make the adjudication a refusal, not a `clear`; cost is bounded per task, and hitting that bound must be a refusal, not a truncation, or the design has reinvented its own subject; and the reduce is lossy by construction so it must never feed the audit log. **Slicing:** (a) inventory + classify every cap with a test that fails on a new unclassified one (cheap, and the thing that stops this recurring — see also [#591](https://github.com/hherb/kastellan/issues/591), the char-boundary walk hand-written in five places with its correct test in one); (b) guard-tier map-reduce; (c) audit spill-to-store; (d) handoff `query`; **(e) the anchor index, which is what makes (d) reachable** — see the Hermes survey [`docs/devel/notes/2026-09-06-hermes-agent-survey.md`](notes/2026-09-06-hermes-agent-survey.md) §3.1. `handoff` already pages by byte *offset* (`get_slice`/`fetch`) and an offset is unguessable, so the recovery path exists and is unusable; Nous solved the same problem with an **LLM-free regex harvest of exact identifiers** rendered beside the summary — seven categories, per-category caps, most-frequent-then-most-recent, a byte budget — under a heading telling the model to use them verbatim *and as recovery query terms*. Their committed four-transcript scorecard puts lean+recovery at **68.3% recall on 49K retained tokens against 45.8% on 162K** for their previous fat-tail default, and attributes the needle-fact half specifically to the anchor index (one transcript 23.3 -> 60.0 closed-book, 46.7 -> 80.0 with recovery). **The same loss is ours one axis over:** `core/src/scheduler/inner_loop/summary.rs` bounds each Ok view at `STEP_OK_SUMMARY_MAX` = 16 KiB and elides oldest-first past `PLANS_SUMMARY_BUDGET` = 96 KiB (4 and 32 before #702), leaving `{"status":"ok","elided":"summary budget"}` — paraphrase-free, and still exactly how a needle fact leaves the prompt. Pure function, unit-testable like `build_argv`; the index is harvested from **untrusted** text so it takes `escape_untrusted_body` + the reserved-tag guard like everything else rendered into the planner prompt (#533). Likely also the mechanism [#560](https://github.com/hherb/kastellan/issues/560) needs — before #702 a bare `"20973"` reached the planner among subjects and dates with nothing marking it as *the id*; #702 now labels it in the per-step view, so #560 needs re-measuring, and the index is what keeps the label once the step is elided [[tool-output-reaches-planner-key-stripped]] — and **do not** close #560 by rewriting the parameter description a third time.
- [ ] **Per-dispatch loop guardrail — the layer [#677](https://github.com/hherb/kastellan/issues/677) proves we do not have.** We already bound the *outer* loop (`MAX_STEPS_PER_PLAN` = 64, `ctx.max_plans`, and the forced-synthesis turn, which Nous do not have and which is the better idea). Nothing bounds the *inner* one: nothing in `tool_host::dispatch` notices that this exact call, with these exact arguments, already ran this task and returned this exact result — which #677 was first read as measuring (the issue called three of six plan iterations near-duplicate searches; the audit rows later showed iteration 2 was a schema rejection and the dominant cause was the key-stripped result view #702 replaced, so the duplicate-dispatch case is plausible rather than demonstrated — see #699). **Pattern: Hermes' `agent/tool_guardrails.py`** (survey [`docs/devel/notes/2026-09-06-hermes-agent-survey.md`](notes/2026-09-06-hermes-agent-survey.md) §3.2), a 558-line **side-effect-free controller** — it observes and returns decisions; the caller decides whether a decision becomes guidance, a synthetic result, or a halt. Identity is `(tool_name, sha256(canonical sorted-key JSON args))`, non-reversible, and their `to_metadata()` deliberately returns it *without the raw argument values* — which is the audit shape we want too, since it lets a row say "third identical dispatch" without carrying the arguments a second time. Three detectors: same-tool-same-args failing (`exact_failure`), same-tool-different-args failing (`same_tool_failure`, warn-only for tools whose red output is normal work — a run of distinct failing commands is diagnosis, not a loop), and same-tool-same-args returning a **byte-identical result** (`idempotent_no_progress`, which is #677's shape). Two refinements worth taking whole: **a successful mutation resets the counters** ("pure loops never mutate between attempts, so the replay detector keeps its teeth" — an edit-then-re-run is not a loop, a re-run with nothing changed is), and the **identical-result stub**, which from the *second* byte-identical repeat replaces the result *in context* with a reference to the first while the tool still executes — a context saving, not a guard, and the natural companion to the elided outcome. **Kastellan-specific requirements that are not in theirs:** the verdict is **advisory to the planner and never a silent drop** — a suppressed dispatch renders in `plans_so_far_summary` as an explicit marker, because a step that vanishes is how a planner learns to distrust its own history; and it is **not a security control and must be documented as not one**, the same way D10 says the guard tier is advisory — nothing downstream may relax on it. Lands as a pure `TaskGuardrail` accumulated in `TaskContext` beside `plans`. Their flat `IterationBudget` (parent 500, subagent 50) is the *delegation* half and belongs with the tiered delegation policy above, not here.
- [ ] **A planner A/B battery with programmatic graders — the instrument that would have caught [#560](https://github.com/hherb/kastellan/issues/560) before production.** Every planner-facing change we have shipped (`disable_thinking`, the lenient plan-parser error, forced synthesis, the #536 prompt rewrite) was validated by reasoning plus a couple of live runs. #560 is the standing proof that this is not enough: #536 applied the prescribed fix, deployed, and **both later runs still fabricated the `message_id`**, and we learned it from production. **Pattern: Hermes Agent's `evals/core_tool_deferral/`** (survey [`docs/devel/notes/2026-09-06-hermes-agent-survey.md`](notes/2026-09-06-hermes-agent-survey.md) §3.3) — two plain git worktrees pinned to the two SHAs under test ("never `pip install -e`"), a task battery where each task carries fixtures, a **programmatic grader with partial credit** and scripted user replies, one isolated subprocess per (arm, model, task, rep) with a hermetic env, resume-safe orchestration, and — the discipline that matters and that the openworker note flagged for the reviewer eval — **`exit 3 = infra/config error, never scored**: machinery failure separated from judgment failure. Their committed verdict is the reporting standard too: 288 runs across three model tiers, contested cells re-run to n=6, the one real regression named and quantified with its safety consequence checked ("in 0 of 288 runs was the WRONG file deleted — the failure mode is degraded UX, never destructive action"), a distractor control proving no overhead on tasks that need none, and an audit of the single outlier run. Ours is cheaper to stand up because the graders are mechanical: *did it call `mail.get_attachment_text`? did it pass `message_id` as `i64` and not a string? did it reach an answer without a duplicate search?* Runs against a real local model on the DGX (mock-LLM e2e cannot see any of this), seeded on day one with #677 and #560, and publishes a dated committed report like the guard-tier ship gates already do. **This is what turns the two entries above from plausible into demonstrated, and it is why they are sequenced before the skill-lifecycle work below** — Nous ship their context economics on measurement and their headline learning loop on judgment, and the difference shows.
- [ ] **Layered oversight corpus + a committed ship gate for the review lattice** — the only item that tells us whether the escalation lattice actually *works*, and today nothing measures it: `tests/guard/corpus` (133 cases) is single-key and answers only the output-screening question, while `cassandra::review`'s stages have no corpus at all. **Pattern: openworker's `tests/corpora/` + `scripts/eval_reviewer.py` + `reports/`** (survey [`docs/devel/notes/2026-09-02-openworker-resurvey.md`](notes/2026-09-02-openworker-resurvey.md) §3.1). Three things to take. **(a) Split the questions.** They carry three datasets because the originals "mix together" three different questions: *should the deterministic gate decide this* (120 rows, keyed `allow_without_reviewer` / `reviewer_eligible` / `human_only` / `hard_deny` — our `Approve`/`Advisory`/`Escalate`/`Block` under other names), *what should the reviewer decide* (121), and *what should happen across several actions* (60: provenance, combined effect, transformed injection). **(b) Two answer keys per row.** `expected_current` = the implementation as reviewed on a stated date; `expected_secure` = the recommended policy; when they differ the row **must** carry `known_gap: true` + a `failure_point`. That is what stops a test "accidentally blessing an identified vulnerability just because it matches today's behavior", and it yields two reports off one corpus — a regression report (production still matches documented behaviour) and a remediation report (distance from the target policy). **(c) Separate machinery failure from judgment in the harness.** Their `Verdict.error` marks an `unsure` that came from a provider 5xx or a timeout rather than the model judging; the live engine treats both identically (ask the human) but the eval must not, because a gate that passes on error-unsures is "caution by outage, not judgment" — a real 2026-08-17 finding on their side. Parse defects deliberately stay `error=False`: the model *did* answer and its answer failed the contract, which is a model property the eval should see. Ship gates get published as dated committed reports, as ours already are for the guard.
- [ ] **Non-diagnostic denial toward the agent, for every CASSANDRA verdict** — `WITHHELD_NOTE` is already the right instinct for the injection guard; generalise it. openworker's `AGENT_DENY_MESSAGE` tells the agent only "blocked by the safety reviewer, do not retry or vary it, call `ask_user` if it is genuinely required", and routes the real reason to the human and the audit row, because at that moment "the agent may be acting on injected instructions, and a specific reason turns the reviewer into an oracle — retry, read the reason, adjust, retry". Today `Verdict::Block`'s and `Verdict::Advisory`'s reasons both feed back toward the loop. Small and self-contained: the audit row and the operator ask keep the full concern; the loop-facing string becomes a fixed sentence. Pairs with the docs item below.
- [ ] **`kastellan_policy` role — the DB-grant form of a self-protection floor** — migration 0009 grants `kastellan_runtime` INSERT/DELETE on `tool_allowlists` (deliberate: the operator CLI writes under that role through the `core::cli_audit` chokepoint), which means the daemon's own application role can widen its own argv allowlist. Not exploitable today — the model has no SQL tool and memory access is core-only — but it is exactly the escalation openworker's `protected_paths()` exists to block ("approve one ordinary-looking command, it quietly appends to the rule file, every future session is more permissive… this cannot be a property of a sandbox or of any one mode; it is a floor"). Ours can be enforced far better than theirs, by GRANT rather than by string-matching inside the process that could be compromised: a distinct `kastellan_policy` role owning the policy tables (`tool_allowlists`, `agent_prompts`, and whatever the risk-override store above becomes), `SELECT`-only for `kastellan_runtime`, entered by the CLI path via `SET ROLE`. Same shape as 0002's superuser/runtime split, one level in. **Companion rule for the standing-grant design above:** an action whose effect lands *after* the approving conversation ends — a saved skill, a recurring task's instructions or schedule — is a floor class no standing grant may ever cover, and narrowing such a change is floored alongside broadening it, because telling the two apart means judging intent, which is what a floor exists to avoid.
- [ ] **Document why the guard tier fails OPEN and the plan reviewer should fail CLOSED** — one paragraph in the CASSANDRA docs, to stop a future reviewer "fixing" the asymmetry. The guard adjudicates *fetched content*, so failing closed hands a denial-of-service oracle to anyone who can serve the agent a page (wiring-spec D8). A plan reviewer adjudicates *the agent's own proposed action*, where every failure mode — malformed JSON, unknown verdict, empty reply, timeout, provider error — should collapse to "a human decides", with no parse path that results in execution (openworker `reviewer.py` §8.5). Both postures are correct; only naming which input each stage judges makes that legible.
- [ ] Policy gate: per-tool, per-task, per-data-classification routing decision
- [ ] Frontier escalation through egress proxy (Anthropic / OpenAI)
- [~] **Model-based CASSANDRA guard tier** — slice 1 [#585](https://github.com/hherb/kastellan/pull/585) `f90631da`, measurement 3 [#606](https://github.com/hherb/kastellan/pull/606), wiring [#607](https://github.com/hherb/kastellan/pull/607), live bring-up and the audit-cap fix all merged 2026-08-21 to 2026-08-23; first production run 2026-08-23. **Advisory, not a gate (D10).** What still binds lives in HANDOVER § The guard tier; open work is issues [#612](https://github.com/hherb/kastellan/issues/612), [#604](https://github.com/hherb/kastellan/issues/604), [#622](https://github.com/hherb/kastellan/issues/622), [#639](https://github.com/hherb/kastellan/issues/639) and the #599–#611 cluster. Full history: [`archive/roadmap_20260914_pre-prune.md`](archive/roadmap_20260914_pre-prune.md).

  **[#632](https://github.com/hherb/kastellan/issues/632) FIXED and MERGED `466ca7ff`
  ([#640](https://github.com/hherb/kastellan/pull/640), 2026-09-01)** -- `tok_per_s` -> `fastest_tok_per_s` in BOTH
  `BootRates` and `TimeoutBasis::Probed`, moved together because renaming one alone leaves
  `fastest_tok_per_s: Some(*tok_per_s)` in `from_basis`, which reads like a bug and invites a
  later session to "restore" the old name. **The REPORTING vocabulary is deliberately frozen**:
  the durable `policy / guard_tier.boot` key stays `"tok_per_s"` (live rows carry it and the
  operator query `slowest_tok_per_s < tok_per_s / 2` is written against it) and so do `main.rs`'s
  two tracing fields -- a `warn!` line naming this number differently from the audit row it
  accompanies would read as a second measurement. That decision is now a comment at both reporting
  sites, and the key/field divergence is visible on the `"tok_per_s": rates.fastest_tok_per_s`
  line itself. The issue's own site count was low: 62 raw occurrences across 12 files, of which
  only ~40 are Rust identifiers -- the rest are wire keys, the operator query, tracing field names,
  a pseudocode symbol in `derive_guard_timeout`'s doc, and a local variable holding ONE sample's
  rate (correctly left alone, since `fastest` is the f32 that reaches the basis). A blind
  `sed` would have renamed the wire key and broken every stored row's query; the existing
  `CONFIGURED_KEYS` array is what makes that a test failure rather than a silent one.
  **Gate: DGX 3910 / 0 / 55, 176 suites, `TEST_EXIT=0`, 8 `[SKIP]` all gliner-relex** -- byte
  identical to `8d92c02b`'s baseline, which is what a correct pure rename looks like: it adds and
  removes no tests.

  **[#634](https://github.com/hherb/kastellan/issues/634) FIXED and MERGED in the same squash
  `466ca7ff` (2026-09-01)** --
  the three hand-rolled `bring_up_daemon` copies (`cli_ask_e2e`, `observation_capture`,
  `guard_boot_row_e2e`, ~70 identical lines each) now use `tests_common::daemon`. The parameters
  became a `DaemonSpec` builder rather than a seventh, eighth and ninth positional argument --
  three of the existing six were already adjacent `&str`s, the same transposition hazard #632 is
  about one crate over. **Two divergences the issue's own table missed, both found by reading the
  copies rather than the issue:** `observation_capture` uses a **15 s** readiness budget (the issue
  documented only guard_boot_row's 20 against the shared 10, so the real spread is three values,
  not two); and it passes `KASTELLAN_LLM_LOCAL_URL` **verbatim** -- that variable is
  operator-supplied and documented as already carrying `/v1`, so the shared helper's unconditional
  append would have dialled `/v1/v1`. That is the one migration hazard that fails **silently**:
  `LlmEndpoint::{Base, Verbatim}` makes the two shapes distinct types at every call site.
  ⚠️ **The first cut of that fix was itself a regression, caught by the PR review.** Having the
  types, `mail_daemon_e2e` deleted its `strip_suffix("/v1")` and passed `Verbatim` -- but that
  `strip_suffix` plus the helper's append had *normalised*, accepting both `http://h:11434` and
  `http://h:11434/v1`. `Verbatim` accepts only the second, and the bare form is the one the
  installer calls canonical (`OLLAMA_LLM_URL`), so the migration silently narrowed an operator
  variable inside an `#[ignore]`d test. Fixed by a third constructor,
  `LlmEndpoint::from_operator_url`, which classifies rather than assumes; a `Base` that already
  carries `/v1` now asserts rather than doubling it. **Making a distinction representable is not
  the same as making the wrong side of it unreachable.**
  **17 new `tests-common` unit tests** (11 in the migration, +6 from the PR-review wave)
  **and all 15 mutants killed**, each by the test written for it --
  including the `/v1/v1` mutant, the extra_env ORDERING transposition (a deletion mutant is weaker
  and killed two tests instead of the one), a `data_dir`/`user` swap, and removal of the 200-char
  name cap. They matter out of proportion to their size: `linux-check.yml` runs
  `cargo test -p kastellan-tests-common` on **every PR** and is the only target there that
  reaches this code, while the six daemon
  e2es these values configure run on no PR at all. The `extra_env`-later-wins guarantee was a
  comment at a call site with nothing testing it; it is now a property. ⚠️ **The PR review corrected
  how it is guaranteed.** The first version asserted the *model* (`rfind` over `spec.env`) while
  merely *documenting* the render, and only half of that was checkable: systemd documents last-wins
  for a repeated assignment, but launchd gets a plist dict with a **duplicate key**, whose
  resolution the format does not define and which nothing in `kastellan-supervisor` tests -- so a
  containment control (`force_routing(false)`) rested on a belief about `CFPropertyList`. Fixed by
  **removing the dependency rather than testing it**: `service_spec` collapses duplicates last-wins
  before returning. General case filed as
  [#644](https://github.com/hherb/kastellan/issues/644).
  Also folded in: the character-for-character `guard_tier_boot_payload` duplicate, and #635's
  stderr-on-failure fix that `cli_ask_e2e`'s private copy had never received -- and note the
  attribution, which the first draft had inverted: #635 fixed the shared helper **and**
  `guard_boot_row_e2e`'s copy, writing one fix twice; the copies that never got it were
  `cli_ask_e2e`'s and `observation_capture`'s. **Three files shrank
  below or toward the cap**: `guard_boot_row_e2e` 687 -> 537, `cli_ask_e2e` 858 -> 741,
  `observation_capture` 664 -> 604.
  **Filed from the review, and all three now FIXED and MERGED `121f22a2`
  ([#645](https://github.com/hherb/kastellan/pull/645), 2026-09-02):**
  [#642](https://github.com/hherb/kastellan/issues/642) -- one un-`cfg`'d `validate_service_name` +
  `MAX_NAME_LEN` at the supervisor crate root, replacing a character-identical copy in each backend
  that **neither host ever ran the other of**, plus the `tests-common` hand-copy that checked the
  half which essentially cannot fire and skipped the charset half that can. Two tests the copies
  never had: the cap in **both** directions, and the cap pinned to a **literal**.
  [#641](https://github.com/hherb/kastellan/issues/641) -- `DaemonSpec::new(label, data_dir, llm)`,
  no two parameters sharing a type; `suffix`/`user` were the same expression at all six call sites,
  so deleting beat newtyping, and no setters were added speculatively. The name is now validated at
  construction against the supervisor's own predicate. ⚠️ Two consequences recorded rather than
  discovered later: `new` reads the environment (eagerly, once, so `service_spec` stays pure), and
  the unit's suffix no longer matches its sibling PG cluster's -- which #548's sweep may one day
  want back. [#643](https://github.com/hherb/kastellan/issues/643) -- one `ReportedRates` mapping
  shared by the `info!`, the `warn!` and the durable row; a transposed pair reports a **contended**
  boot as a quiet one, silencing exactly what #624 was filed to make visible. Chose the shared
  struct over the subscriber test because it leaves **no second site to diverge**. Plus a
  movement-only `LlmEndpoint` split (`spec.rs` 538 -> 438).
  **Gated on the DGX at 3940 / 0 / 55, 176 suites, `TEST_EXIT=0`**, reconciling exactly as 3928 + 12
  by per-suite diff; cold clippy exit 0 over 345 `Checking`+`Compiling` lines, 27 crates, zero
  warnings; **ten mutants, ten killed**. **Both hosts green, nothing outstanding**: the Mac covers
  the `cfg(target_os = "macos")` `launchd_agents` half the DGX compiles out --
  `kastellan-supervisor --lib` 115 / 0 with all 8 `service_name::tests` observed running there too
  (the point of #642: one rule set, both hosts execute it) and 38 launchd / 0 systemd confirming the
  platform split, plus `clippy -p kastellan-supervisor --all-targets -D warnings` exit 0.
  **Still open from that review:** [#644](https://github.com/hherb/kastellan/issues/644) (the launchd
  duplicate-plist-key question for every *other* `ServiceSpec` producer).
  **The FIRST four-agent review of that fix ([#614](https://github.com/hherb/kastellan/pull/614)) found it kept
  half the defect:** an unaffordable preserved key was dropped *silently*, giving a row
  byte-identical to one whose dispatch never ran a tier — the same absence-vs-loss ambiguity one
  function down. Keys are now admitted individually against the budget less a reserved marker
  allowance, anything refused is named under `DROPPED_PRESERVED_KEY`, and a `const` block makes a
  future member that shadows `_truncated`/`sha256`/`len` a **compile error**. The same review found
  the new live probe instrument passed having measured nothing whenever
  `KASTELLAN_LLM_GUARD_TIMEOUT_MS` was pinned — precisely what #612 tells a Metal operator to do —
  and that `derive_guard_timeout`'s doc conflated the size sweep's tok/s with the boot probe's, so
  its own arithmetic did not close.
  **[#612](https://github.com/hherb/kastellan/issues/612) filed, not fixed:** D9's probe extrapolates
  linearly from ~1 KiB; the DGX is flat (1.09x) but the Mac is **4.37x** optimistic (1 137 tok/s at
  1 KiB, **260 at 64 KiB**), so a worst-case document takes 171 s against a derived 91 s and **fails
  OPEN** without firing the ceiling-clamp warning. Metal hosts should pin
  `KASTELLAN_LLM_GUARD_TIMEOUT_MS` until it is settled. This corrects the earlier expectation that the
  Mac would clamp to the 120 s ceiling: it does not, and that is the defect.
  **WIRING SLICE MERGED 2026-08-23** (`8736f559`, PR [#607](https://github.com/hherb/kastellan/pull/607), closes
  [#586](https://github.com/hherb/kastellan/issues/586)) — the tier reaches
  `post_process::finalize` as a threaded `Option<Arc<GuardTier>>`, catalogue first with a
  short-circuit proved by a request count, escalate-up only. Spec amended with **M2** and
  **D8/D9/D10**. **D8:** the attacker-reachable HTTP 400 of
  [#604](https://github.com/hherb/kastellan/issues/604) still fails **open** at runtime
  (fail-closed would let anyone serving the agent a web page deny it every document by
  padding one) but the tier now refuses to **boot** below
  `SCAN_BYTE_CAP + 512 = 66 048` tokens of per-request context — 1 tok/byte is a *bound*,
  not a guess, because Shieldstral's tokeniser is byte-level BPE. **D9:** the timeout is
  derived from a boot **throughput probe** and clamped to `[15 s, 120 s]`; D2's constant
  was wrong by 40x on the Mac, and too short a guard timeout does not error, it fails
  open. **M2** measured the probe first: a **cache-buster prefix** defeats the prefix cache
  (deliberately *not* called a nonce — it is not secret and authenticates nothing, and CodeQL's
  `rust/hard-coded-cryptographic-value` rule reads the parameter NAME)
  (`cached_tokens: 0`, two cold samples within 3%, inside M1's band), the contaminated
  repeat reads **21 094 tok/s** against a true ~5 000 unless `cached_tokens` is subtracted,
  and 1024 dense bytes tokenise at **1.26 bytes/token**. **D10:** the tier ships as
  **advisory defence-in-depth, not a gate** — 65% recall, weakest against narrative
  indirect injection — and nothing downstream may relax on it. D5's per-dispatch `p`, on
  **cleared** documents too, makes production the score source for a corpus that is not
  catalogue-selected. **DGX gate `69834357` (branch tip): 3823 / 0 / 54**, `TEST_EXIT=0`, 175 suites,
  reconciling exactly against `main` 3759 **+64**; 8 `[SKIP]` all gliner-relex, *not* the
  bwrap-userns skip. Mac: `guard_tier_e2e` 13/0 with zero `[SKIP]` under real PG; clippy
  `-D warnings` exit 0 over **218** `Checking` lines from a cold target dir. **13 mutants,
  12 killed, 1 equivalent** — `is_timeout` had no coverage at all and its always-false mutant
  left the whole workspace green while handing the slowest hosts the shortest guard timeout;
  killed by a pure `probe_error_outcome` plus a layer-2 case against a mock that accepts and
  never answers.
  **A five-agent PR review then produced ELEVEN more fixes, and they clustered where m13 did — the
  boot-time IO glue, one call frame further out than the mutation set reached.** The worst:
  **the derived timeout was never proven to reach the HTTP client** (`from_config(cfg,
  timeout.timeout)` → `probe_budget` left the whole workspace green, because `tier.timeout()` reads
  the *struct*, not the client's budget — #586's entire payload, untested); **`is_timeout` was wrong
  in both directions** (a *connect* timeout also sets `is_timeout()`, so a 5 s connect stall derived
  the 120 s ceiling; and a budget expiry while reading a non-2xx error body became `HttpStatus`, not
  `Transport`, taking the **floor** — a fail-open, fixed in `llm-router` at all three swallow sites);
  and **the model was consulted on results with no text at all**, where the verdict on an empty
  `<Document>` is undefined and a `p >= tau` would withhold a result containing nothing to inject
  (now the named door `Unadjudicated::NoScannableText`). Also: `GuardReport.p` could be `Some(NaN)`;
  the `Saturated` basis reported a fabricated 12.8 tok/s and a post-clamp `derived_ms`; a failed boot
  probe discarded its error text and logged at `info!`; `truncated`/`body_byte_len` now ride the
  Allow half; and `KASTELLAN_REQUIRE_GUARD=1` closes the gap that the hazard D6 argues from — an
  `install` that drops **all three** keys lands on the one non-fatal arm. All eleven mutation-proven.
  Post-review gate green on **both** hosts at `31a05e00`: **DGX 3834 / 0 / 54** (`TEST_EXIT=0`, 175
  suites, 8 `[SKIP]` all gliner-relex — *not* the bwrap-userns skip; clippy 0 over **231** cold
  `Checking` lines), **Mac 3712 / 0 / 24** (clippy 0 over 218). Reconciles exactly — 3823 **+11** for
  the eleven new tests, **+75** against `main`. `guard_tier_e2e` **17 / 0 on both**. Deferred:
  [#608](https://github.com/hherb/kastellan/issues/608)–[#611](https://github.com/hherb/kastellan/issues/611).
  **MEASUREMENT 3 MERGED 2026-08-23** (`d51c9b20`, PR [#606](https://github.com/hherb/kastellan/pull/606)) — plan Task 5
  complete. **133 cases, 109 captured** through the real `web.fetch` path (D5 floor: ≥100 with a
  captured half), 24 truncated at `SCAN_BYTE_CAP` on both labels, **zero `Unmeasured`**, weights
  hashed against the pin at use on both hosts so D6's comparison is *enforced*.
  **τ = 0.79552656**, the lower of the two hosts' operating points, verified FP-0 on both; the
  hosts agree to **0.1%** with identical confusion counts. `best_tau` returns **NONE — the
  classes overlap at every threshold**, which is D7 earning its place.
  **Three findings travel with the number and matter more than it does.**
  (A) **τ is set by SECURITY PROSE with 1.0-1.2 points of headroom, and that stratum is
  BIMODAL** — the eleven highest-scoring benign cases are all D4's expensive stratum (Wikipedia on XSS at
  0.7843 against τ 0.7963; the twelfth benign is 0.1052), but the stratum's 19 cases span
  0.0009–0.7843. OWASP's LLM Top 10 scores 0.0009 where OWASP's LLM01 scores 0.5446, and
  Wikipedia's prompt-injection article 0.0274 where its XSS article scores 0.7843. The guard
  reacts to **payloads quoted verbatim**, not to subject matter — so τ is pinned by roughly
  four documents, and documentation that explains without reproducing scores like ordinary
  content (`llmguard-injection-doc`: 0.0012).
  (B) **The misses concentrate in NARRATIVE indirect injection** — 19 of 55 attacks missed;
  bare imperative payloads 6/6 caught at median 0.9955, but the greshake scenarios (the same
  intent inside a plausible document) score a **median 0.0797, 5/8 missed**, with the canonical
  `albert_einstein.md` case at **0.0082**. That is the delivery shape the tier exists for.
  (C) **Truncation can cost the whole signal** — a 1.8 MB payload truncated to 64 KiB scores
  0.0102 against a family median of 0.9937.
  **Recommendation recorded: ship as advisory defence-in-depth at that τ; 65% recall is not a
  gate, and nothing else should relax on it.** Runbook + both reports:
  [`2026-08-23-guard-measurement-3.md`](runbooks/2026-08-23-guard-measurement-3.md).
  **Five issues filed** — [#601](https://github.com/hherb/kastellan/issues/601) (capture screens
  `Relaxed`, calibrate excludes on `Strict`; quantified **inert** for this run),
  [#602](https://github.com/hherb/kastellan/issues/602) (a rate-limited **200 with an empty
  body** is hashed and pinned as the page — #596 closed this for 404s only),
  [#603](https://github.com/hherb/kastellan/issues/603) (the pin covers the **final URL**, so a
  Wayback redirect reads as drift), [#604](https://github.com/hherb/kastellan/issues/604)
  (**`SCAN_BYTE_CAP` bounds bytes, not tokens** — 64 KiB tokenised to 44,437 and the
  adjudication died on HTTP 400), [#605](https://github.com/hherb/kastellan/issues/605) (the
  `PROVISIONAL` banner is unconditional). **The wiring slice inherits two obligations:** what an
  **errored** guard call does (HTTP 400 and timeout are both attacker-reachable), and that its
  derived **15 s** timeout is **22× short** — the same document takes ~5.5 min on the Mac,
  because M1's 6.5 bytes/token was benign prose and adversarial text runs at 1.47.
  **Its own review round found one real defect, one script that did not do what its header
  said, and eight factual errors in the campaign's prose — all fixed on the branch.**
  `render_per_case` re-implemented the adjudication states inline and drifted from `decide`
  on the **non-finite door**: `Some(NaN)` printed as a score in a run the matrix had already
  called INVALID, sorted (by `total_cmp`) to the bottom among the guard's most confident
  detections — in the one section whose job is answering *which* case. `render_distribution`
  had the identical gap under a comment asserting it could not. Fixed by extracting **one
  classifier the sort key, the verdict column and the distribution list all read from**;
  seven mutants, all killed. `paced-capture.sh` could not detect the **empty-200** its own
  header names as the hazard (the byte count was printed and discarded), and `timeout` — not
  in macOS base userland — failed invisibly because the classifying grep ate its own error;
  both preflighted or checked now, plus `WRITE-FAILED`, a kept log, and an out-dir/manifest
  reconciliation. `--per-case` had **no test of any kind** — the emission block was deletable
  with the suite green. Two caveats the artefacts did not state about themselves are now in
  the runbook: the Mac report is **not recomputable from its own printed scores** (4 dp
  against an 8-sig-fig τ), and **τ is fitted and evaluated on the same 133 cases**, so `FP 0`
  is guaranteed by the criterion that chose it and is not a false-positive *rate*.

  **M1 (slice 1's Open risk 1) DISCHARGED 2026-08-22** — six DGX runs of `live_shieldstral_size_sweep`: at `SCAN_BYTE_CAP`
  (64 KiB = 10,062 prompt tokens) the tier costs a **p50 3,215 ms / 3,558 ms**, ~85× measurement 1's 30–43 ms, which was
  taken on ~26-token strings. Cost is **entirely prompt processing and linear** (decode is 1 token at 0.00 ms; prompt eval
  4,039–6,660 tok/s), so any host's cost follows from its throughput — a CPU-only host would need ~50 s. Specs:
  [`2026-08-22-guard-measurement-3-corpus-design.md`](../superpowers/specs/2026-08-22-guard-measurement-3-corpus-design.md)
  (**do this FIRST** — ≥120 cases, captured half, third-party text never committed: a manifest + fetch script sha256-verifies
  into a gitignored dir, dissolving both the licensing and the privacy problem, with **no loader change** needed) and
  [`2026-08-22-shieldstral-guard-wiring-design.md`](../superpowers/specs/2026-08-22-shieldstral-guard-wiring-design.md)
  (τ **required, no default**, so slice 1's D9 becomes a property of the code; a **derived** 15 s timeout closing
  [#586](https://github.com/hherb/kastellan/issues/586); `p` recorded on every adjudicated dispatch so production becomes
  measurement 3's own score source). **Measurement 3 Tasks 1–4 MERGED 2026-08-22**
  (`b58edc77`, [#593](https://github.com/hherb/kastellan/pull/593); DGX gate **3668 / 0 / 54**, +42 reconciling exactly): D7's
  `operating_point` + `BudgetScope` — needed because `best_tau` is separability-only and
  would return `Err(Overlap)` on any corpus with real captured content; the operating point
  rendered **once, corpus-wide**; a `manifest` module carrying metadata and never text; and
  `kastellan-cli guard capture`, which drives the **real** chokepoint and refuses a result
  that came back as the injection placeholder (storing it would record a benign-looking
  document in place of the page, which then gets scored). **Task 5 PILOTED LIVE 2026-08-22** and superseded by the real campaign; its two plan
  corrections still hold — `guard capture` needs **no** `tool_allowlists` row and no daemon
  restart (it derives its allowlist per entry), and Wayback pinning collapses the campaign's
  egress surface to one domain.
  **[#592](https://github.com/hherb/kastellan/issues/592) blocked the two-host τ
  comparison:** the hosts ran different Q8_0 builds (HF LFS oid `35b755be…` vs the DGX's `5cee57a9…` at identical byte
  length) — pinning a quantisation LABEL is not pinning the bytes.
  **FIXED — the pin is now checked at use, so Task 5 Step 7 is unblocked** (2026-08-22, `abb3d3a7`, [#598](https://github.com/hherb/kastellan/pull/598); closes [#592](https://github.com/hherb/kastellan/issues/592)).
  kastellan never opens the GGUF, and llama.cpp's `/v1/models` reports an **empty** `digest` while the fields it
  *does* report (`ftype`, `size`, `n_params`) are exactly the shape facts two Q8_0 builds share — so the endpoint
  cannot prove which bytes it loaded. What it can do is **name the file**: `guard calibrate` now GETs `/props`,
  takes `model_path`, hashes that file itself, and **refuses before scoring anything** on a mismatch, an
  unreadable path, an absent `model_path`, a **relative** `model_path`, or an unreachable `/props`.
  `--weights-unpinned` keeps a *candidate* model calibratable, and stamps the report — the marking rides on
  the number rather than on the operator remembering. `RunMeta` gained a `weights` field beside
  `policy_digest`, for the same reason: τ is only meaningful against known inputs, and the weights are an
  input. Sum duplicated into `scripts/eval/lib/guard-weights.sh` for operator pre-flight and CI-enforced by
  `rust_and_bash_guard_pins_agree`, copying the guest-kernel precedent. Live-verified on the DGX against the
  real 3.6 GB files: upstream passes, the #592 original is refused naming it a *different quantiser run of
  the right model*.
  **Limits documented rather than sold around:** it trusts the server's self-report of `model_path`, and it is
  TOCTOU — the same posture `guest_kernel_pin` carries. **The projector is a second instance, still open —
  [#597](https://github.com/hherb/kastellan/issues/597).**

  **A five-agent review of the branch found three defects the branch's own tests could not reach, and the
  most important one is the shape this project keeps paying for.** (1) **Nothing pinned the ACCEPT path.**
  Every weights fixture in the tree is deliberately not the pinned file, and none can be — the real one is
  3.6 GB — so an implementation that refused *unconditionally* passed the entire suite. The consequence
  would have been #592 inverted: `guard calibrate` refusing on a correctly-provisioned host, the operator
  reaching for `--weights-unpinned` to get unstuck, and every measurement-3 report stamped untrustworthy.
  Fixed by splitting the IO probe from a pure `apply_opt_out`, whose accepting arm is now unit-tested under
  both settings of the flag — **mutation-proved:** force it to refuse and exactly that test fails.
  (2) **A relative `model_path` was resolved against the CLI's cwd, not the server's** — so a copy of the
  pinned file at the same relative path under the tool's working directory would hash as pinned while the
  server served other bytes. A fail-open in #592's own shape, reached through the fix for #592; now
  `WeightsPinError::RelativePath`, refused rather than resolved, per the repo's `fs_read`-must-be-absolute
  rule. (3) **The opt-out fabricated a measurement**, rendering `<unverified: …> (0 bytes)` — a byte count,
  in the field position a real streamed count occupies, for a file that was never opened. `WeightsProvenance`
  gained the third state it always had (`Unverified{kind}`), `FileDigest`'s fields went private so the
  shortcut no longer compiles, and `Pinned` now carries the digest it *measured* instead of reciting
  `PINNED_SHA256` back. Also: the bash pre-flight's `sha256sum | cut` masked hasher failures (a pipeline's
  status is its last command's), so its `|| return 1` was dead code and a permissions error was reported as
  "a different file altogether"; the bash half is now **executed** by `tests-common` via a ported
  `bash_with_pin`, not grepped — **mutation-proved**, flip the mismatch branch's `return 1` to `return 0`
  and the reject test fails. Two items were filed rather than folded in:
  [#599](https://github.com/hherb/kastellan/issues/599) (an unpinned run still exits 0, so nothing
  machine-readable separates it from a verified one) and
  [#600](https://github.com/hherb/kastellan/issues/600) (`run-shieldstral-llamacpp.sh`, the one script that
  *launches* a Shieldstral server, still never hashes `$MODEL`). **Re-gated on the DGX at `f46c67cf`: 3749 / 0 / 54
  across 174 suites, `TEST_EXIT=0`, `CLIPPY_EXIT=0` zero warnings**, reconciling exactly as the pin's original +42
  plus the review's +21.

  **The review ran AFTER the merge and found four fail-opens** — [#596](https://github.com/hherb/kastellan/pull/596), merged 2026-08-22 as `2ab6612c`.
  Each could produce a corpus or a threshold that *looked verified and was not*, and none was
  reachable by any existing test: (1) **`--record` disabled every hash check**, including on
  already-recorded entries, so the campaign's next step — ~85 new entries, recorded by running
  `--record` over the whole directory — would have silently re-pinned any source that had
  drifted; (2) **an empty budget scope made D7's criterion vacuous**, and the shipped corpus has
  zero captured cases, so that was the *default* `guard calibrate` run, printing `0 of 1 allowed`
  over a population that does not exist; (3) **τ printed at `{:.6}`** reparses strictly greater
  48% of the time (200k samples), so an operator copying it deploys a threshold at which the
  boundary case the report counted as a true positive stops flagging; (4) **the HTTP status was
  never checked**, so a vanished snapshot's 404 page was hashed and pinned wearing the label of
  the page it replaced — Open risk 2 failing *open*, which the spec assumed impossible. The exit
  code also could not see the operating point at all, so `SingleClass`/`Overlap`/the new
  `EmptyBudgetScope` exited 0. **+18 tests**, including the `guard_capture_cli_e2e` exit-status
  file the sibling command already had. Deferred: [#594](https://github.com/hherb/kastellan/issues/594)
  (capture has no egress proxy, so a *hostname* resolving into a denied range is unchecked — the
  IP-literal half is closed), [#595](https://github.com/hherb/kastellan/issues/595) (manifest
  content bounds are loader conventions, not type invariants).

  Spec + plan `docs/superpowers/{specs,plans}/2026-08-21-shieldstral-guard-slice-1*`. Slice 1 lands the guard endpoint
  seam (`RouterConfig::{guard_url,guard_model}` + the pure `for_guard`), the adjudicator
  (`cassandra::guard_model` — a digest-pinned prompt artefact, a pure three-valued `decide`, and a thin async shell
  holding its own `Router`), and an offline calibration harness (`core::guard_calibration` + `kastellan-cli guard
  calibrate` + 24 seeded corpus cases). **Deliberately no production wiring**: `tool_host`, `scheduler`,
  `channel/ingest.rs`, `recall_assembly` and `injection_guard.rs` are byte-identical to `main`, verified as a merge
  gate, so the merge cannot regress the daemon. **DGX gate 3599 / 0 / 54** at the branch's final code tip `53618ad4`,
  whose tree the merge commit reproduces exactly (the two later branch commits were docs-only) — so that is also
  `main`'s baseline. **Two findings overturned the study and must not be re-derived from
  it.** (F1) Its proposed `0.45–0.70` adjudication band is very nearly empty: catalogue weights are only
  `{0.40, 0.50, 0.75}` and `screen` sums them, so any two rules firing already totals ≥ 0.80 — the reachable set is
  `{0, 0.40, 0.50, 0.75, 0.80, 0.90, 1.0}` and the band holds *exactly one* value, 0.50, reachable by two of the
  twenty-two patterns alone. The tier is re-aimed at everything **below** `BLOCK_THRESHOLD`, i.e. the catalogue
  *miss* at 0.0, which is where leetspeak / non-English / novel phrasing actually live; a test pins the weight
  structure so a future reweighting cannot silently invalidate that reasoning. (F2) `kastellan-cli observation
  replay` cannot be measurement 3's vehicle — it walks `CaptureJson.plans` through `ChainReviewStage`, a *plan*-level
  tool, while the guard adjudicates *document* text; the seven fixtures contain no screened documents. A separate
  vehicle was built rather than overloading one subcommand with two schemas. **Measured, not assumed:** all 12
  evasion cases in the seeded corpus score exactly 0.0 under the shipping `screen()`, so the catalogue's blindness is
  established by test. **⚠️ The seeded corpus is a PROOF OF CONCEPT and does NOT discharge measurement 3** — 24 cases,
  none captured from real worker output; any τ from it is provisional and must never become a default (said in D9, in
  the report footer, in `DEFAULT_TAU`'s doc, and in `tests/guard/corpus/README.md`). **The wiring slice still owes, in
  order:** the `#[ignore]` `live_shieldstral_size_sweep` latency number at 1/8/64 KiB (measurement 1's p50 30–43 ms
  was on ~26-token strings; the spec makes this a precondition), measurement 3's ≥100-case corpus with a captured
  half, then the wiring at `post_process::finalize` — the only one of `screen`'s five call sites that is async *and*
  holds an `AuditSink`. Wiring shape: `catalogue >= BLOCK_THRESHOLD -> Block, model not consulted`; below it,
  `Flagged -> Block | Clear -> Allow | Unmeasured -> Allow, audited`; escalate-up only. There is deliberately **no**
  `escalates() -> bool` helper — a caller consuming a bool structurally cannot audit the `Unmeasured` distinction it
  is required to audit. Reviewed by one whole-branch pass (0 Critical, 6 Important, 10 Minor) and a scoped re-review
  verdicting every finding addressed, then a **second round (2026-08-21: 1 Critical, 5 Important, 7 Minor, all fixed
  on-branch)**. The Critical generalises past this slice: `guard_model_e2e`'s mock read the request only far enough
  to find `Content-Length` and then **discarded it** — it had copied the listener from
  `llm-router/tests/local_backend_e2e.rs` but dropped the `oneshot` that returns the served body, which is the half
  with the value. That left the adjudicator's whole request construction unpinned, and two mutations that silently
  kill the tier in production kept all eight tests green: deleting `.with_logprobs(..)` (no distribution comes back
  ⇒ every call `Unmeasured`) and swapping the tuned policy prompt for a naive one (measured moving an indirect
  injection 0.9998 → **0.0038**). *A mock that does not return what it was sent tests only your own canned
  response.* Also fixed: none of the new `core` tests ran in CI (two hermetic steps added — the corpus test catches
  a catalogue change arriving in **someone else's** PR, which an operator run cannot); `guard calibrate` had no
  tests at all, including the unmeasured→exit-1 line; a run that adjudicated **nothing** exited 0 (`invalidity()` is now the single
  definition, naming which cause fired, with `is_valid()` delegating to it — a shape that came from
  mutation-testing the fix, which showed the CLI e2e staying green under a revert of `is_valid` alone); `SYSTEM_PROMPT` had no drift guard
  and its apparent test was tautological; a non-finite score read as `Clear` (`decide` now routes it to
  `Unmeasured`, and `confusion_at` delegates to `decide` rather than re-writing `p >= tau`); and **the feasibility
  study itself was never corrected** — it now carries a SUPERSEDED banner, since it is the document a later session
  actually opens. The guard inheriting the planner's 180 s timeout (vs a 30–43 ms target) is recorded as open risk 6
  and filed as [#586](https://github.com/hherb/kastellan/issues/586) rather than fixed here, because choosing a
  bound before the size sweep would be inventing a number.

- [x] **Model-based CASSANDRA guard tier — the study and its five measurements. RE-OPENED 2026-08-13: Mistral Shieldstral 1.0 3B is now the recommended pick over Granite Guardian** — [#592](https://github.com/hherb/kastellan/issues/592), [#597](https://github.com/hherb/kastellan/issues/597), [#569](https://github.com/hherb/kastellan/pull/569) `07b6451e` — 2026-08-13
- [ ] **(baseline) Model-based CASSANDRA guard tier — IBM Granite Guardian 4.1 (defense-in-depth, advisory only)** —
  a local safety/judge model (`ibm-granite/granite-guardian-4.1-8b`, **Apache-2.0** ⇒ license-audit clean;
  hybrid Mamba-2/Transformer ⇒ low memory; runs on the **existing `kastellan-llm-router` local backend
  pointer** — `:8000` on both hosts now (vLLM on Linux, oMLX on macOS; the `:11434` macOS mapping this
  entry originally assumed is superseded) — no new egress, no vendor lock-in, no NVIDIA dep).
  Three hook points, all **advisory** — Guardian is a probabilistic classifier (~0.79 F1 OOD-safety; misses
  ~1 in 5), so it **never** becomes a gate: the OS sandbox (bwrap/Seatbelt) + the egress proxy's deterministic
  allowlist/SSRF/pinning remain the only things that actually contain a compromise. It lowers attempt volume
  and feeds the audit log; it does not change the threat model.
  1. **`GuardianReviewStage` implementing `ReviewStage`** (`core/src/cassandra`) — slots into the existing
     `ChainReviewStage(vec![ConstitutionalGuard, DeterministicPolicy, …])` after the deterministic rules.
     `async fn review(&self, plan, ctx)` formats the agent's `Plan` (esp. tool-call steps) into Guardian's
     `<guardian>` criteria block, calls `Router::send` in **no-think mode** (low latency, direct `<score>`),
     maps `yes` → `Verdict::Advisory`/`Escalate` (NOT `Block` — advisory tier) carrying the jailbreak/harm
     reason tag. Fail-open on router error (defense-in-depth, not the boundary).
  2. **Function-calling-hallucination check before step dispatch** (`ToolHostStepDispatcher`) — Guardian's
     agentic detector validates that a tool-call step's args are consistent with the worker's manifest schema
     + the user intent *before* the worker is spawned. This is the strongest novel fit — semantic validation
     the argv allowlist structurally can't express. Advisory pre-flight; the allowlist + sandbox still gate.
  3. **RAG groundedness on `memory::recall`** — Guardian's context-relevance/groundedness/answer-relevance
     scores spot-check that a memory-backed answer is grounded in what was actually recalled (BYOC criteria
     can also encode kastellan-specific policy — "attempts secret exfiltration?", "targets a non-allowlisted
     host?" — as auditable yes/no rows). **Limitation: English-only.** `<think>` traces are NOT logged verbatim
     (IBM warns they may contain unsafe / unfaithful content). Cost: an 8B judge ~doubles inference load, so
     no-think inline + reserve think-mode for offline spot-checks. (Investigated 2026-06-15; sources in the
     handover.) Pairs naturally with the policy gate + frontier escalation above (screen what crosses the
     local↔frontier boundary).
- [ ] **`audit_log` causal columns — `task_id` + `caused_by`** ([#628](https://github.com/hherb/kastellan/issues/628)), the substrate the viewer below wants. `audit_log` is `(id, ts, actor, action, payload)`; every causal fact lives inside `payload` by convention (`task_id` in 58 places across 19 files, enforced nowhere), there is no grouping key for one plan iteration, and no row links to the row that caused it — so the log answers questions by **correlation** rather than **equality**. That is the #616 defect one level up ("could not be counted — only inferred, by correlating `router_error` rows against `body_byte_len` and `ms`"), and #619's `LIKE 'operator%'` is the same shape again. **Design borrowed from Headlong's `design/trajectory_spec.md`** (*"writers stamp exact links; readers must not guess"*; links stamped at the **transport** — for us `db::audit::insert`, which every write site already goes through). Ships with the reader rule for pre-migration rows: tolerate absent fields, render ungrouped, **never** reconstruct membership heuristically. Gives the viewer a tree instead of payload-sniffing.
- [ ] Read-only audit log viewer (CLI complete; web optional)
- [ ] 7-day adversarial soak test (prompt-injected channel content; no escapes in audit log)

---

## Cross-cutting / continuous

- [x] **Full-project security audit + remediation (pre-release)** — [#392](https://github.com/hherb/kastellan/pull/392), [#386](https://github.com/hherb/kastellan/issues/386), [#387](https://github.com/hherb/kastellan/issues/387) `2d2aa70` — 2026-07-02
- [x] **Second full-project security audit + remediation (pre-release, 2026-09-02)** — [#660](https://github.com/hherb/kastellan/pull/660), [#661](https://github.com/hherb/kastellan/issues/661), [#662](https://github.com/hherb/kastellan/issues/662) `62d98a00` — 2026-09-02
- [x] **Micro-VM diagnostics: the path can now say why it failed** — [#675](https://github.com/hherb/kastellan/pull/675), [#666](https://github.com/hherb/kastellan/issues/666), [#671](https://github.com/hherb/kastellan/issues/671) `f831b3d1` — 2026-09-05
- [x] **A stale micro-VM rootfs image can no longer gate anything** — [#667](https://github.com/hherb/kastellan/issues/667), [#680](https://github.com/hherb/kastellan/pull/680), [#679](https://github.com/hherb/kastellan/issues/679) `fb560ab7` — 2026-09-07
- [x] **Every micro-VM precondition answers to the REQUIRE knob** — [#679](https://github.com/hherb/kastellan/issues/679), [#683](https://github.com/hherb/kastellan/pull/683), [#682](https://github.com/hherb/kastellan/issues/682) `ec9a2e94` — 2026-09-07
- [x] **`target/release/` gets one producer, so the #667 gate stops crying wolf** — [#682](https://github.com/hherb/kastellan/issues/682), [#685](https://github.com/hherb/kastellan/pull/685), [#686](https://github.com/hherb/kastellan/issues/686) `0939e80c` — 2026-09-09
- [x] **Guest-kernel integrity pin** — [#471](https://github.com/hherb/kastellan/issues/471), [#478](https://github.com/hherb/kastellan/pull/478), [#479](https://github.com/hherb/kastellan/issues/479) — 2026-07-20
- [x] **Guest-kernel integrity at VM boot** — [#479](https://github.com/hherb/kastellan/issues/479) — 2026-07-21
- [x] **Provisioning-binary checksum pins** — [#386](https://github.com/hherb/kastellan/issues/386), [#388](https://github.com/hherb/kastellan/issues/388), [#389](https://github.com/hherb/kastellan/issues/389) — 2026-07-21
- [x] **Linux bind-path symlink canonicalization** — [#387](https://github.com/hherb/kastellan/issues/387), [#482](https://github.com/hherb/kastellan/pull/482) — 2026-07-22
- [x] **Audit-#7 family tail — worker-discovery / lockdown-env / keyring / scrub** — [#388](https://github.com/hherb/kastellan/issues/388), [#389](https://github.com/hherb/kastellan/issues/389), [#486](https://github.com/hherb/kastellan/pull/486) — 2026-07-23
- [x] **The macOS Apple-`container` micro-VM tier gets the REQUIRE knob and a freshness gate** — [#684](https://github.com/hherb/kastellan/issues/684), [#687](https://github.com/hherb/kastellan/issues/687), [#688](https://github.com/hherb/kastellan/pull/688) `09a4f924` — 2026-09-10
- [x] **Every micro-VM preflight subprocess answers to a budget, and the last two #688 deferrals close** — [#690](https://github.com/hherb/kastellan/issues/690), [#689](https://github.com/hherb/kastellan/issues/689), [#686](https://github.com/hherb/kastellan/issues/686) `c5bf5e5f` — 2026-09-10
- [x] **An oversized dispatch still records what ran** — [#617](https://github.com/hherb/kastellan/issues/617), [#694](https://github.com/hherb/kastellan/pull/694), [#591](https://github.com/hherb/kastellan/issues/591) — 2026-09-11
- [x] **The planner reads a tool result as labelled JSON** — [#702](https://github.com/hherb/kastellan/pull/702), part of [#677](https://github.com/hherb/kastellan/issues/677) — 2026-09-14. `inner_loop/result_view` replaces the injection guard's key-stripping flattening as the planner's view; identifier keys only, space-free ids atomic up to 1 KiB. Live: the working question still works; #677's follow-up still fails on [#701](https://github.com/hherb/kastellan/issues/701). Second review round: mail headers returned as `{name, values}` (sender-chosen names were reaching the planner as keys), sink-only blocks audited with `tier: "sink"`; deferred [#703](https://github.com/hherb/kastellan/issues/703) (guard model never sees keys), [#704](https://github.com/hherb/kastellan/issues/704), [#705](https://github.com/hherb/kastellan/issues/705).
- [ ] Threat-model doc kept in sync with shipped backends
- [ ] Architecture doc kept in sync with shipped components
- [ ] License audit on every new dependency (AGPL-compatible only)
- [ ] CI: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace` — both Linux and macOS. (Clippy `-D warnings` gate live on `linux-check`, #153; `cargo fmt` still TODO.)
- [x] **Public website `kastellan.dev`** — 2026-06-11
- [x] **Early test + deletion-audit infra (2026-05-12)** — 2026-05-12
