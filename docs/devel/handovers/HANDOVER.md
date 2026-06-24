# kastellan — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention. Older sessions are compressed
> into "Earlier history" below; full per-session detail lives in the
> [`archive/`](archive/) snapshots.

**Last updated:** 2026-06-24 (**Close the Matrix inbound-loss window on worker respawn — [#321](https://github.com/hherb/kastellan/issues/321)
DONE on branch `feat/321-matrix-downtime-loss-window` (PR #347).** PR #320's self-healing `MatrixChannel::supervised` respawn
made the channel silently lossy for the worker's downtime: a message a user DM'd the bot while the worker was down arrived in the
respawned worker's catch-up sync and was dropped by the `live` gate (`workers/matrix/src/sdk_live.rs`), which suppresses the
*entire* initial sync to avoid replaying full room history on every start. **Key insight:** the "sync-token watermark" the issue
asked for *already exists* — matrix-sdk persists its sync token in the SQLite state store and `sync_once` resumes from it, so on a
restart the catch-up sync returns only events received *since* the last run (= exactly the downtime backlog); the bug was purely
that the `live` gate suppressed those too. **The fix (TDD, rule #1 pure-fn):** read the persisted token *before* the initial sync
and seed `live` from it — pure **`initial_live_state(prior_sync_token: Option<&str>) -> bool`** (= `is_some()`: prior token ⇒
restart ⇒ live from the start ⇒ surface the incremental backlog; no token ⇒ fresh login ⇒ keep suppressing full-history replay) +
fail-soft **`read_prior_sync_token(&Client) -> Option<String>`** (`client.state_store().get_kv_data(StateStoreDataKey::SyncToken)`
→ `.ok().flatten().and_then(into_sync_token)`; any read error ⇒ `None` ⇒ "fresh/suppress", which can never cause a stale-history
replay). `connect_client` seeds `live` from `initial_live_state(token.as_deref())` **before** `register_message_handler`; the
post-sync `live.store(true)` stays (no-op when already true). `MatrixChannel::supervised` doc comment updated (recovery, not
"lost"). **No new persistence, no protocol/schema/migration change.** `Client::sync_token()` is `pub(crate)` in matrix-sdk 0.18 so
the read goes through the public state-store key — no trait import needed (`get_kv_data` dispatches via the `&DynStateStore`
vtable). **Verification (macOS):** worker default **11/0**, `live-matrix` **21/0** (+3 new `initial_live_*` units, incl. the empty-token guard),
`cargo clippy -p kastellan-worker-matrix --all-targets --features live-matrix -- -D warnings` clean. New `#[ignore]`
`matrix_restart_recovers_downtime_message` e2e (`core/tests/matrix_live_e2e.rs`): init → `close()` bot → peer sends during
downtime → respawn same store → poll surfaces it. **VERIFIED LIVE on the DGX (2026-06-24):** both live e2e tests **2/0** against
a throwaway loopback matrix-conduit + encrypted room (`scripts/matrix/dev-e2e-bootstrap.sh`), reproducibly (~1.7s); the restart
test is a genuine regression gate — a **negative control** (`initial_live_state` forced to `false`) **FAILS** at the "never
received the downtime message" assertion after the full 45s deadline. **Test-robustness fix (`53808ab`):** the first-shutdown
check no longer asserts a *clean* exit — #321 covers downtime of any cause incl. a crash, the token persists incrementally during
sync, and the worker's sync task can race teardown into a transient crypto-store abort (`process::exit(1)`); the test now waits
for exit and logs the status without gating on it. Pure-Rust, `live-matrix`-gated. Spec/plan:
`docs/superpowers/{specs,plans}/2026-06-23-matrix-downtime-loss-window*`.)

_(Prior session — **Clearer injection-blocked signal to the planner — [#340](https://github.com/hherb/kastellan/issues/340)
DONE on branch `feat/340-injection-blocked-note` (PR #346).** Final follow-up of the #338 arc. When `tool_host::dispatch`
blocks a worker result on the output injection screen it substituted `{ injection_blocked, score, reason_codes }`; now that
successful step output reaches the planner (#338), that placeholder renders through `extract_scannable_text` — which emits only
**string leaf values** — so the planner saw just the reason-code string (e.g. `"ok: instruction_override"`), an unintelligible
gap that could tempt a re-run, *unlike* the `fetch_screen` placeholder which already carries a human-readable `note`. **The fix
(TDD, rule #1 pure-fn):** new pure **`core/src/tool_host/injection_placeholder.rs`** (72 LOC) — `WITHHELD_NOTE` const =
`"[tool output withheld: failed injection screen]"` + `injection_blocked_placeholder(score, &reason_codes) -> Value` (adds the
`note` string leaf; keeps `injection_blocked`/`score`/`reason_codes` for audit-shape parity with `fetch_screen`). `dispatch`'s
Block arm now calls it instead of the inline `json!`. `prompts/agent_planner.md` gained a planner bullet (a withheld step reports
`"ok: [tool output withheld: …]"`; don't re-run it). `docs/threat-model.md` gained a subsection documenting both the
planner-bound-output screening (the two chokepoints `tool_host` + `fetch_screen`) **and the known split-slice limitation** (an
injection payload split across a 64 KiB boundary or two `fetch_handoff` slices can each fall below the per-slice threshold and
evade single-slice screening — inherent to streaming bounded-memory screening; sandbox + egress proxy remain the real boundary).
**Verification (macOS):** new `injection_placeholder` units **3/0** (note present + signals "withheld"; structured fields kept;
no raw-output leak), `tool_host` lib **43/0**, `injection_guard_e2e` **6/0** against real PG18 + real Seatbelt jail (the
placeholder-shape test now also pins the `note` end-to-end), `cargo clippy -p kastellan-core --lib --tests -- -D warnings`
clean. Pure-Rust, no migration, no OS-gated code → DGX not required. **`tool_host.rs` 659→667** (still the leading over-cap
prod-split candidate — additive +8; real split tracked separately). The #338 planner-feedback arc (#337/#338/#343/#339/#340)
is now complete.)_

_(Prior session — **Global budget for `plans_so_far_summary` — [#339](https://github.com/hherb/kastellan/issues/339)
MERGED to `main` as `8fa67f9` (PR #345).** Hardening follow-up to #338, which raised the per-step summary term ~2000× (bare
`"ok"` → up to `STEP_OK_SUMMARY_MAX`=4 KiB); `plans_so_far_summary` re-renders every plan's every step every planner iteration,
so the *accumulated* total (`max_plans` × steps × 4 KiB, `max_plans` operator-overridable) was unbounded in the always-in-context
planner prompt. **Shipped (TDD, 2 tasks):** **(1)** lifted the rendering helpers (constants + `sink_screen_blocks` +
`render_step_outcome` + the per-plan mapping) out of the over-cap `inner_loop.rs` (575→481) into a new pure
**`core/src/scheduler/inner_loop/summary.rs`** (286 LOC); `TaskContext::plans_so_far_summary` is now a thin delegate to
`summary::render_plans_summary`, behavior byte-identical. **(2)** new `RenderedStep{text,elidable}` + pure
`apply_summary_budget(&mut [Vec<RenderedStep>], budget) -> usize` that elides the **oldest** successful-step output heads first
(replaced by `OK_ELIDED_MARKER`) until total step-text bytes ≤ `PLANS_SUMMARY_BUDGET`=**32 KiB**; `render_step_outcome` returns
`RenderedStep` (`elidable:true` only for real Ok heads — errors, decisions, the injection withheld-marker, and the **most-recent**
plan's heads are preserved, so no #338 loop regression). **Security:** screen-at-render-then-budget-elide is the safe order —
every head/detail passes `sink_screen_blocks` *before* the budget pass, which only ever *removes* already-screened text. Pure-Rust,
no migration → DGX not required. The budget is a compile-time constant; expose `PLANS_SUMMARY_BUDGET` via env only if a large
operator `max_plans` ever needs it (YAGNI today). Spec/plan: `docs/superpowers/{specs,plans}/2026-06-23-plans-summary-global-budget*`.)_

_(Prior session — **Feed successful tool output back to the planner — #338 DONE on branch
`feat/338-feed-tool-output-to-planner` (PR pending).** The success-half symmetric to PR #337's error-half, and the blocker
for every tool-using task: `render_step_outcome` (`core/src/scheduler/inner_loop.rs`) collapsed a successful
`StepOutcome::Ok(serde_json::Value)` to the bare scalar `"ok"`, discarding the worker's result — so the planner never saw a
step's *output* and re-issued the same successful step every iteration until `plan_iteration_cap_exceeded` (live DGX evidence:
5 identical `/usr/bin/ls /tmp` plans, the model's own prose "the output was not visible in the current context"). **Key
finding:** the injection-guard requirement #338 worried about was *already* met upstream — `tool_host::dispatch` screens every
worker result (blocked → tiny placeholder) over the first `SCAN_BYTE_CAP`=64 KiB, and `tool_dispatch::dispatch_step` stashes
any `Ok(v)` >`DEFAULT_RESULT_BYTE_CAP`=64 KiB to the handoff cache; since the two caps are equal, every `Ok(v)` reaching the
render is already screened + ≤64 KiB. **The fix (TDD):** the `Ok` arm now renders a bounded head via the existing
`injection_guard::extract_scannable_text` (new `STEP_OK_SUMMARY_MAX`=**4 KiB**, user-chosen) as `"ok: <head>"` (`…` on
truncation); render stays *screen-free* by design (the value is already screened). `prompts/agent_planner.md` updated:
`step_outcomes[j]` is now `"ok: <output head>"` and a new bullet tells the planner to answer from that output, not re-run the
step. **Security fix from the final review (the one real catch):** the `fetch_handoff` branch returned its slice *unscreened*,
and since `tool_host` only screened a stashed body's first 64 KiB, a fetch at `offset ≥ 64 KiB` could surface an unscreened
tail into the prompt (a regression *opened* by the render change). Closed with new
`core/src/scheduler/tool_dispatch/fetch_screen.rs::screen_fetched_data` (Strict/fail-closed) screening each served slice at the
dispatch chokepoint → blocked `data` replaced by a withheld-note placeholder; the invariant "everything reaching
`render_step_outcome` is screened" now holds via *both* chokepoints. **Verification — macOS:** inner_loop 31/0 (+4 render
tests), fetch_screen 3/0 (real Strict Block exercised, raw injection text proven gone), `cli_ask_e2e` 7/0 (PG18 override, incl.
the `ask_subprocess_fails_after_plan_iteration_cap` pin), `cargo clippy --workspace --all-targets -D warnings` CLEAN. Pure
Rust, no migration, no OS-gated code → DGX not required for the unit gate. **VERIFIED LIVE on the DGX (2026-06-23):** deployed
`main`@`181d70e` via `upgrade_from_git.sh` (no SDK bump → no relogin; channel bus up, `NRestarts=0`); `kastellan-cli ask "run
/usr/bin/ls /usr and tell me exactly how many entries you saw"` → **"I saw 9 entries in /usr"** (host `ls /usr | wc -l` = 9 ✓),
`plan_count=2`, `terminal_kind:ok`, `total_dispatch_calls=1` — the agent ran the step **once**, read its stdout, counted, and
answered without looping. A `/tmp` + `/` variant returned the jail's `Permission denied` stderr, which the planner likewise
**read and answered from** on plan 2 (proving the *output*, not just `"ok"`, is now fed back), no loop. **Follow-ups filed:**
[#339](https://github.com/hherb/kastellan/issues/339) global `plans_so_far_summary`
budget (per-step 4 KiB × `max_plans` × steps is unbounded; `max_plans` operator-overridable);
[#340](https://github.com/hherb/kastellan/issues/340) clearer injection-blocked signal from the `tool_host` placeholder to the
planner (renders as `"ok: <reason_code>"` today) + the split-across-slices screening limitation. Spec/plan:
`docs/superpowers/{specs,plans}/2026-06-22-feed-successful-tool-output-to-planner*`. **Follow-up hardening (branch
`feat/render-sink-injection-screen`, PR pending):** the "render stays screen-free, trust the two source chokepoints" decision
was **reversed** — `render_step_outcome` is now the **single mandatory sink screen**, re-screening the exact text it emits (the
`Ok` head AND the `Err` `detail`; the `code` is kept) with the step's **own per-tool profile** (`GuardProfile::for_tool`,
threaded from `steps[j].tool`), so the planner-screening invariant is *enforced at one point* not *relied upon* across sources.
Per-tool profile (not blind Strict) keeps the re-screen idempotent → no over-block of Relaxed doc-fetch workers (#142); Block →
`WITHHELD_MARKER`. Source screens (`tool_host`/`fetch_screen`) stay for non-planner consumers. 4 new units (35/0 inner_loop),
`cli_ask_e2e` 7/0, clippy clean. **Both halves are now MERGED to `main`** — #338 as `181d70e` (PR #341) and the render-sink
follow-up as `447767a` (PR #343); deployed + VERIFIED LIVE on the DGX 2026-06-23 ("9 entries in /usr" on plan 2, one dispatch,
no loop).)_

_(Prior session — **Agent tool-loop recovery — DONE, MERGED to `main` as `ff3e2f5`
(PR [#337](https://github.com/hherb/kastellan/pull/337)). Deployed + verified live on the DGX.** A live Matrix question
— *"What is the distance between Oslo and the capital of Poland?"* — failed with `plan_iteration_cap_exceeded (3>=3)` though
the model knew the answer. Systematic debugging on the live DGX (`tasks tail`, audit log) found **three** distinct problems.
**(1) Blind replanning (the reported bug):** `TaskContext::plans_so_far_summary` (`core/src/scheduler/inner_loop.rs`)
collapsed every failed step to the bare string `"err"`, discarding the `StepOutcome::Err { code, detail }` the dispatcher
produces — so the planner flailed through near-duplicate plans until the cap (and over-tooled a pure-knowledge question:
`shell-exec python3`, then an invented `google_search`). Fix: new pure `render_step_outcome` surfaces `err: <CODE>: <detail>`
(detail clamped to `STEP_ERR_DETAIL_MAX=200`) into the planner prompt; `agent_planner.md` gained guidance (answer directly
from in-context knowledge; only real tools exist; read the step error, don't repeat a denied step; `shell-exec` argv[0] MUST
be absolute — cleared env, no PATH in the jail); `DEFAULT_MAX_PLANS_FAST` 3→5 (`db/src/tasks.rs`). 2 new unit tests (TDD),
`cli_ask_e2e`/`observation_capture` cap pins updated. **(2) LLM transport timeout:** tool-using (multi-plan) tasks failed
with `router: HTTP transport error: error sending request for url (…:11434/v1/chat/completions)` ~30s after the prior step.
Root cause: the reqwest total `.timeout()` (`KASTELLAN_LLM_TIMEOUT_MS`, default **30s**) firing **mid-generation** — a real
agentic plan over `gemma4:26b-a4b-it-q8_0` with the ~13 KB `agent_planner.md` system prompt was **measured at ~86s**
standalone against a healthy Ollama. A reqwest timeout's `Display` is byte-identical to a send failure, which disguised it.
Ruled out (all reproduced fine): keep-alive connection reuse, model swapping (gemma 36GB + embeddinggemma 1.1GB both stay
resident), Ollama health (curl 200 in ~1.4s). Fix (`llm-router`): `DEFAULT_TIMEOUT_MS` 30_000→**180_000** (bounds generation,
not connect — dead backend still fails fast via the separate 5s `connect_timeout`); `RouterError::Transport` now appends
`[request timed out]`/`[connection failed]` via the pure tested `transport_kind_tag` so this can't be misdiagnosed again.
**(3) Empty allowlist (deployment):** the DGX `shell-exec` allowlist was empty → every step `POLICY_DENIED`. Added
`/usr/bin/{cat,ls,python3}` (operator DB state; entries MUST be absolute paths; daemon loads the allowlist ONCE at startup so
a restart is required to apply). **Verification — live DGX:** original distance question now completes on **plan 1**
(~1,050 km); a `shell-exec` step runs `/usr/bin/ls /tmp` with `terminal_kind:ok` in the jail; post-timeout-fix a tool task
formulated **all 5 plans** (every LLM call completed, none cut off at 30s). `cargo clippy --all-targets -D warnings` clean on
touched crates (core/db/llm-router). Deploy: relayed commits to the DGX (Mac→github push firewalled), brought DGX to
origin/main (matrix-sdk 0.18→0.18, **no relogin** — same device `xA31CsGn82`), `build-release.sh` + `install` + restart;
Matrix channel bus running. **⚠ KNOWN FOLLOW-UP ([#338](https://github.com/hherb/kastellan/issues/338)):** successful tool
**output** is still fed back as just `"ok"` (only the error half was fixed), so tool tasks loop re-running the same step until
the cap. Feeding worker stdout into the planner prompt is the prompt-injection surface — route it through
`core/src/cassandra/injection_guard.rs` and/or the handoff/fetch design (`core/src/handoff.rs`, spec
`2026-06-09-teach-planner-fetch-handoff`); deliberate design task, NOT a naive inline of raw stdout. Separate, model-side:
~86s/plan is gemma 26B on the DGX Spark with a 262144-token context — reducing the model's default `num_ctx` (via
`OLLAMA_CONTEXT_LENGTH`/Modelfile, NOT per-request — that forces a reload) is a possible perf follow-up.)_

_(Prior session — **Forward entity embed-on-insert — DONE on branch `feat/entity-forward-embed-on-insert`
(PR pending).** Closes the deferred *forward* half of the entity-embedding arc (PR #335 shipped backfill + lane; this is the
on-insert path, symmetric with the L1 #324-forward / #325-backfill split but for entities). New entities written by
`entity_extraction::batch_upsert` previously landed `embedding IS NULL` until a manual `entities reembed`; they are now embedded
the moment the upsert creates them, so a freshly-extracted entity is searchable via the entity-similarity recall lane with no
backfill run. **What shipped (TDD, 5 layers):** (1) **pure `select_new_entities(deduped, upsert_map) -> Vec<(id,kind,name)>`**
(`batch_upsert.rs`) — picks only rows the upsert just CREATED (`inserted == true`, the `xmax = 0` discriminator the upsert
already returns); conflict-hit existing rows are dropped (a still-NULL existing row stays the **backfill's** job — the #324/#325
division). 4 units. (2) **degrade-and-warn `embed_new_entities(pool, &dyn Embedder, &[(id,kind,name)])`** — embeds each via the
shared `entity_embedding_text` chokepoint (so on-insert == backfilled byte-for-byte) + the guarded `set_entity_embedding`; an
embed `None` (RouterEmbedder logged), a lost `IS NULL` race (`Ok(false)`, concurrent backfill won — no WARN), or a write `Err`
(WARN) skips that row and **never fails the upsert**. (3) wired into `upsert_entities_and_relations` (now takes `&dyn Embedder`)
**after the entity commit, before the relations phase** — committed new rows get embedded even if relations later error.
(4) `gliner_relex::upsert_entities_and_relations` delegate widened; **`GlinerRelexExtractor` now owns `Arc<dyn Embedder>`**
(`new(client, pool, embedder)`); `NoOpEntityExtractor` path unaffected (never upserts → never embeds). (5) `main.rs` builds the
one `RouterEmbedder` **before** the extractor and shares the Arc across L1 (scheduler) + entities. **Decisions:** embed only
NEW inserts (conflict-hits = backfill); no batch-embed seam (sequential loop, mirrors backfill — possible follow-up); no
migration / no ANN index (as #335). **Verification — macOS PG18:** new `entity_forward_embed_e2e` **3/0** (embed-on-insert +
lane surfaces the linked memory; conflict-hit NOT re-embedded [`call_count` pin]; declined embed leaves row NULL + upsert still
Ok) + regressions `entity_extraction_e2e` **16/0**, `entity_reembed_e2e` **4/0**, `memory_entity_link_e2e` **6/0**, batch_upsert
units **15/0** (+4); `cargo clippy --workspace --all-targets -D warnings` CLEAN. Pure-Rust, no migration, no OS-gated code → DGX
not required. **`batch_upsert.rs` is 514 LOC** (+14 over the 500 cap, within the documented ≤27-over deferral; tests already
external in `batch_upsert/tests.rs`). Spec/plan: `docs/superpowers/{specs,plans}/2026-06-21-entity-forward-embed-on-insert*`.)_

_(Prior session — **Entity-embedding backfill + entity-similarity recall lane — MERGED to `main` as
`4f4d61c` (PR [#335](https://github.com/hherb/kastellan/pull/335)).** `entities.embedding` (`vector(256)`, NULL for every row, no reader)
is now populated by a backfill CLI and consumed by a **4th recall lane**, mirroring the L1 arc (#324/#325).
**What shipped (8 tasks, TDD, subagent-driven):** (1) **`db::entity_embedding`** (new module) — `load_unembedded_entities`
(`(id,kind,name)` scan of NULL rows, **quarantine-blind**), `set_entity_embedding` (guarded race-safe `UPDATE … WHERE
embedding IS NULL`), `entity_similarity_search` (the lane: top-`ENTITY_SIMILARITY_FANOUT=64` entities by cosine `<=>`,
embedded + non-quarantined, → their linked memories ranked by `MIN(dist)`; `include_quarantined` seam mirrors
`graph_search`). Reuses the `check_embedding_dim`/`vector_literal` chokepoints. (2) **`core::memory::reembed`** (new) —
shared `ReembedReport` + `format_reembed_report`/`reembed_batch_failed` lifted out of `l1_reembed` (public paths unchanged).
(3) **`core::memory::entity_reembed`** — pure `entity_embedding_text(kind,name)="kind: name"` (single source of truth) +
`reembed_entities_null(pool,&dyn Embedder)` (degrade-and-warn per row; mirrors `reembed_l1_null`). (4) **recall lane** —
`RecallModes` gains `entity`; `ALL` + the new no-seeds default `SEMANTIC_LEXICAL_ENTITY` (used by `RecallParams::new`) enable
it, so the lane runs on the common cli_ask path; `ENTITY_ONLY` preset; quarantine-filtered (`false`) in production; RRF-fused.
(5) **CLI** `kastellan-cli entities reembed` (sibling module `entities_reembed.rs`; builds the real `RouterEmbedder`, prints
`scanned=/embedded=/skipped=`, non-zero exit on a wholly-failed batch). **Decisions:** backfill embeds ALL entities
(review-blind; the lane filters), no migration (column pre-exists from 0019), no ANN index (deferred), **no forward
embed-on-insert path** (deferred follow-up — new `batch_upsert` entities stay NULL until the next `entities reembed`).
**Verification — macOS targeted (PG18):** `cargo clippy --workspace --all-targets -D warnings` CLEAN; db lib **140/0**, core
memory lib **215/0**, recall units **22/22**; live e2e `entity_reembed_e2e` **4/0** (backfill→lane→linked-memory, idempotent,
degrade-and-warn, **quarantine-excluded-but-operator-visible**), `memory_recall_e2e` **2/0**, `memory_l1_reembed_e2e` **4/0**.
Pure-Rust, no OS-gated code → DGX not required. **⚠ Note:** the full serialized `cargo test --workspace` live run wedges on
the PRE-EXISTING, unrelated `memory_layers_e2e` (imports only `memory::layers`; 0-CPU pool deadlock under heavy multi-cluster
live-PG load — the documented sqlx-0.9 env issue, NOT this change; clippy compiles it fine). **Final review (opus):
merge-ready** after one stale-comment fix in the production recall caller (`pg_builder.rs`, fixed). Spec/plan:
`docs/superpowers/{specs,plans}/2026-06-21-entity-embedding-recall-lane*`.)_

_(Prior session — **matrix-sdk 0.18 deployed live to the DGX; Matrix channel restored after a jail CA-cert fix.
PR [#333](https://github.com/hherb/kastellan/pull/333) (CA fix + `upgrade_from_git.sh`).** Redeployed #329 to the DGX — the
live channel would NOT start. **Root cause (systematic debugging):** reproduced the daemon's exact bwrap jail and captured the
worker's otherwise-swallowed stderr → `build matrix client / No CA certificates were loaded from the system`. **matrix-sdk 0.18
validates the homeserver's TLS against the *system* trust store** (rustls native certs); 0.8 used bundled webpki roots, so it
never read them. The jail bound `resolv.conf`/`hosts`/`nsswitch` but **not** the CA bundle → the 0.18 worker exited ~40 ms into
`matrix.init`, before any login (looked like an auth failure — it wasn't). **Fix:** `build_matrix_policy` binds `/etc/ssl/certs`
+ `/etc/pki/tls/certs` + `/etc/ssl/cert.pem` into `fs_read` (`--ro-bind-try`, cross-distro); needed regardless of force-routing
since the worker does native E2E TLS through the transparent (`disable_mitm`) egress tunnel. +1 unit assertion; policy tests
green. **Second, separate problem — stale Vault password:** a fresh login 403'd. Ruled out the SDK via direct `curl` to
continuwuity — account exists (`displayname:kastellan`), identifier wire-format is the one that worked on 0.8, request
well-formed → `M_FORBIDDEN` = *credential* rejection, not a format issue. Reset the secret; the channel now runs via **session
restore** (device `xA31CsGn82`). **`secret put` gotcha:** the interactive (non-`--raw`) prompt stored a value login rejected,
while the exact 13 bytes via `printf|… secret put --raw` worked — **always use `--raw`** for exact bytes. **Store-wipe gotcha
confirmed + scripted:** a matrix-sdk *major* bump invalidates the on-disk crypto store; `install` does NOT wipe it → restore
fails → must `rm -rf ~/.local/state/kastellan/matrix/store` + re-login (`matrix probe`, keyring password). New
`scripts/upgrade_from_git.sh` encodes the whole flow (switch→pull→`build-release`→`install`→restart→verify; **keyring-only, no
password by default**; `--relogin` wipes + re-logs-in for SDK-major bumps; `-pwd` resets the stale Vault secret first via
`secret put --raw`). **Verified live:** `matrix worker logged in; starting channel bus` + `matrix channel bus running`, worker
stable. Addresses the deploy half of [#330](https://github.com/hherb/kastellan/issues/330). **Per-user install reminder:**
`kastellan-cli` lives at `~/.local/bin/kastellan-cli` (per-user, multi-tenant, never system-wide); scripts call it by absolute
path. A bare-`kastellan-cli` "No such file or directory at /usr/local/bin" is a stale bash command hash in an old shell —
`hash -r`, not a real problem.)_

_(Prior session — **matrix-sdk 0.8→0.18 + sqlx 0.8→0.9 — clears all 4 Dependabot alerts. Branch
`worktree-matrix-sdk-0.18-upgrade` (PR [#329](https://github.com/hherb/kastellan/pull/329)) — green, awaiting review/merge.**
**Why both at once:** the three `matrix-sdk-*` alerts (crypto sender-spoofing, base panics) and the `sqlx` cast-truncation
alert are entangled by a shared `libsqlite3-sys` native `links` conflict (`sqlx-sqlite` vs `matrix-sdk-sqlite → rusqlite`, only
one `links="sqlite3"` allowed per graph). No `matrix-sdk ≥0.11` shares a `libsqlite3-sys` major with `sqlx 0.8.x`, so neither
moves alone — they meet at `libsqlite3-sys 0.35` (matrix-sdk 0.18 + sqlx 0.9). **sqlx 0.9:** `query()`/`execute()` now take
`impl SqlSafeStr`; string literals are unchanged (the project uses runtime query strings, not `query!` macros — no `.sqlx`
cache, no DB at build), so of ~637 call sites only **4 dynamic-SQL sites** needed `AssertSqlSafe`/`raw_sql`
(`db/{pool,probe}.rs` + `db/tests/postgres_e2e.rs` — internal `SET ROLE`/`CREATE DATABASE`/`pg_notify`, all injection-safe).
**matrix-sdk 0.18:** `MatrixSession` moved to `authentication::matrix`, `UserIdentifier::UserIdOrLocalpart(x)` →
`UserIdentifier::Matrix(MatrixUserIdentifier::new(x))`, `#![recursion_limit = "256"]` for the deep crypto async `Send`-solver
overflow, dropped the removed `rustls-tls` feature (rustls implicit now). **Latent deadlock sqlx 0.9 made deterministic
(root-caused via systematic debugging + a 4-variant isolation test):** `Pool::close()` blocks until every connection is
returned, and a `PgListener` only releases its checked-out connection from *inside* `recv()`. `channel_bus_pg_e2e` called
`pool.close()` with the completed-task listener still in scope → **hung 16+ min** (passed on 0.8 by luck); fix = `drop(completed)`
before close. `ChannelBus::shutdown` aborted its pump tasks **without joining** → raced the daemon's `pool.close()` at shutdown;
now **abort-then-join** (matches the scheduler/audit-mirror signal-then-join pattern). Variants A–C (basic PgListener,
the SET-ROLE runtime pool, trigger-fired NOTIFY) all passed — variant D (`pool.close()` + live listener) reproduced it.
**Verified:** workspace **1994/0**, matrix worker `live-matrix` **18/0**, `cargo clippy --workspace --all-targets` (+ live-matrix)
clean. **⚠ DEPLOY GOTCHA — the matrix store is NOT auto-wiped by `install`:** a 0.8→0.18 SDK jump invalidates the on-disk
sqlite crypto store + `session.json` at `~/.local/state/kastellan/matrix/store`, and a plain reinstall preserves it (install
only copies binaries + regenerates env + restarts), so the new worker fails to restore → channel not started. Before redeploying:
`rm -rf ~/.local/state/kastellan/matrix/store`, then a fresh password login (password in the secret store) re-bootstraps a new
device + cross-signing — re-verify it once in `@horst`'s Element. (`uninstall --purge` also wipes it but nukes PG + secrets —
overkill.) Tested in dev; `--release` build + redeploy is the follow-up.
**Code-review follow-ups (PR #329):** `restore_or_login` now returns an **actionable** error naming the store-wipe remedy
(no more silent rediscovery of the deploy gotcha). Three non-blocking follow-ups lodged: [#330](https://github.com/hherb/kastellan/issues/330)
(auto-detect + recover from an incompatible crypto store after an SDK bump), [#331](https://github.com/hherb/kastellan/issues/331)
(CI doesn't compile `--features live-matrix`, so `sdk_live.rs` is uncovered — DGX-gated by design today), and
[#332](https://github.com/hherb/kastellan/issues/332) (focused variant-D PgListener/`pool.close()` deadlock isolation test).)_

_(Prior session — **L1 embedding backfill — `kastellan-cli memory l1 reembed` — [#325](https://github.com/hherb/kastellan/issues/325)
DONE. Branch `feat/325-l1-embedding-backfill` (PR [#327](https://github.com/hherb/kastellan/pull/327)).** Closes #323 item 2: PR #324 wired the *forward* embed path, but
pre-#324 rows and operator-added `memory l1 add` rows (which use `NoOpEmbedder` by design) still had `embedding IS NULL` and
were invisible to the semantic recall lane (`semantic_search` filters `WHERE embedding IS NOT NULL`). **What shipped (TDD, 3
layers):** (1) **`db::memories`** — two re-exported helpers reusing the existing `check_embedding_dim`/`vector_literal`
chokepoints: `load_unembedded_at_layer(executor, layer) -> Vec<(i64,String)>` (`search.rs`; `SELECT id, body WHERE layer=$1
AND embedding IS NULL ORDER BY id` — a stable, resumable scan) + `set_embedding(executor, id, &[f32]) -> bool` (`write.rs`;
guarded `UPDATE … SET embedding=$1::vector WHERE id=$2 AND embedding IS NULL` → **idempotent + race-safe**: a row embedded
concurrently by the forward path no-ops and returns `false`). (2) **`core::memory::l1_reembed`** (new module, 162 LOC) —
`reembed_l1_null(pool, &dyn Embedder) -> ReembedReport{scanned,embedded,skipped}`: scans NULL-embedding L1 rows, embeds each
via the injected `Embedder`, writes back; **degrade-and-warn per row** (a `None` / write-error / lost `IS NULL` race skips
that row, never fails the batch — mirrors `promote_l1`); only an initial-scan failure returns `Err`. Pure
`format_reembed_report` one-liner. (3) **CLI** — new `memory l1 reembed` action (`memory_l1.rs`) builds the **real**
`RouterEmbedder` from `RouterConfig::from_env()` (same config as the daemon's forward path, so backfilled vectors are
byte-identical to on-insert ones), prints `scanned=/embedded=/skipped=`; takes no args. **No separate `l1.reembed` audit
row** — each embed is already audited (`action='embed'` via `embed_query`), reembed changes no rows' existence, and
`cli_audit.rs` is far over-cap. **Decision: L1 only** (symmetric with #324). **Verification — macOS, live PG 18:** new
`memory_l1_reembed_e2e` 3/0 (backfill + `semantic_search`-finds-it; idempotent re-run embeds nothing; degrade-and-warn keeps
the row NULL), db unit +1 (`set_embedding` dim-reject, PG-free lazy pool), core unit +3 (`l1_reembed` signature pin + report
sum + `format_reembed_report`); `cargo clippy -p kastellan-db -p kastellan-core --all-targets -D warnings` clean; full
`cargo test --workspace` green except one **pre-existing flake** — `cli_ask_e2e::ask_subprocess_fails_after_plan_iteration_cap`
(an exact `audit_log` multiset assertion on `scheduler/task.finalize` that is timing-sensitive under heavy parallel suite
load; **passes deterministically when re-run in isolation**, and this change is purely additive to `db::memories` so cannot
affect the agent/scheduler/audit path — not yet filed as an issue). Pure-Rust, no migration, no OS-gated code → DGX not required.
**`db/src/memories/search.rs` is now 508 LOC (+8 over the 500 cap, within the documented ≤27-over deferral).**
**Review follow-ups (2026-06-21, same branch/PR #327):** new pure predicate `reembed_batch_failed(&report)` (`scanned>0 && embedded==0`,
re-exported) drives two things — `reembed_l1_null` now emits an **aggregate WARN** when a batch found rows but embedded none (the
per-row `None` path can't WARN generically), and the CLI now **exits non-zero** in that case (vs always-0 before) so a scripted
`reembed && next-step` doesn't proceed on a wholly-failed backfill; the idempotent no-op (`scanned==0`) still exits 0. +3 core
units for the predicate (empty-scan / any-embedded / all-skipped) and a 4th e2e scenario `reembed_mixed_batch_embeds_one_skips_the_other`
(`SequencedEmbedder` → exact `embedded=1, skipped=1` split). `memory_l1_reembed_e2e` now **4/0** on live PG 18; clippy clean.)_
_(Prior session — **Branch reconciliation + redeploy of newest `main` to the DGX. No code change — operational
session.** Local `main` had diverged (4 commits, `716b873`: an *earlier* iteration of the Matrix-channel work) from
`origin/main`, which had squash-merged the same work in **more refined** form via PR [#320](https://github.com/hherb/kastellan/pull/320)
(self-healing `supervised()`/`WorkerFactory` respawn, timeout-protected login, atomic `0o600` writes, `--matrix-*` install
flags). **Verified the divergent local work was fully superseded** — the two substantive local fixes (`DEFAULT_MAX_CONNECTIONS`
4 → 16 for the 4th long-lived `PgListener`; `ensure_cross_signing` UIA bootstrap; `ensure_v1_suffix`) are all present
verbatim in `main` — then **reset local `main` to `origin/main`** (backup branch taken + verified + deleted; nothing lost)
and fast-forwarded through #322/#324/#326. **Branch hygiene:** deleted 17 stale local + 34 stale merged-PR remote branches
(every one a MERGED PR or confirmed `main` ancestor); `origin` is now just `main` + the one open PR
[#264](https://github.com/hherb/kastellan/pull/264) (`update_worker_name_to_kastellan`). **Redeploy:** `scripts/build-release.sh`
(workspace release 37.75s + `live-matrix` worker 1m50s) + `./target/release/kastellan-cli install --matrix-homeserver-url
https://matrix.kastellan.dev --matrix-user @kastellan:matrix.kastellan.dev` deployed **`0ff5cee` (PR #326)** — the current
`main` tip — to the DGX. 10 binaries copied, both models already present, stop→start applied, all three services
(`kastellan.target`/`-core`/`-postgres`) **active**, Matrix worker re-logged-in + running jailed, `secret list` connects.
**Operator gotcha recorded:** `render_env_file` *regenerates* `~/.config/kastellan/kastellan.env` from CLI flags (no merge) —
the Matrix block (incl. `KASTELLAN_MATRIX_ENFORCE_SANDBOX=0`) is written **only** when `--matrix-homeserver-url`/`--matrix-user`
are passed, so every reinstall must re-pass them or the live channel is silently dropped. No tests run beyond the pre-deploy
`cargo test --workspace` (**1973/0**) on the synced tree.)_

_(Prior session — **Matrix `ProxyBridge` error surfacing — [#312](https://github.com/hherb/kastellan/issues/312)
CLOSED. MERGED to `main` as `0ff5cee` (PR [#326](https://github.com/hherb/kastellan/pull/326)).** The spike's deliberately-minimal error handling
(PR #311) must not stay silent now that the live Matrix channel (PR #320) carries real traffic through the bridge.
**Two silent paths closed in `workers/matrix/src/bridge.rs` (TDD):** (1) the accept loop **broke on any error** — a single
transient `accept()` failure (e.g. `ECONNABORTED`/`EINTR`/`EMFILE`) tore the bridge down for the worker's lifetime, after
which the SDK saw only opaque connection failures. It now **logs every error and continues** (never breaks — matches the
egress-proxy `incoming()` norm; breaking would leave the worker alive but the bridge silently dead, the exact regression),
backing off on non-trivial errors so a *persistent* condition logs at a readable cadence instead of
hot-looping. Strategy is a pure unit-tested classifier `classify_accept_error(&io::Error) -> AcceptRetry{Immediate,Backoff}`
(`ConnectionAborted`/`Interrupted` → immediate; resource-exhaustion/unknown → backoff); the backoff itself is a pure
unit-tested `backoff_delay(consecutive_backoffs)` — **capped exponential** (50ms base, doubling, clamped at 5s; counter resets
on a healthy accept), so a *wedged* listener logs at ~1 line/5s rather than ~20 lines/s forever (review follow-up). **No portable errno "fatal"
classification** — `ErrorKind` is the cross-platform seam, and a fatal accept is now loudly-diagnosable-via-logs rather than
a silent teardown (strictly better than the issue's "break on fatal" proposal). (2) `relay()` **dropped the connection with
no log** on UDS-connect failure — a dead/misconfigured sidecar surfaced only as an unexplained SDK timeout. `relay` now
returns `std::io::Result<()>` (surfacing both the UDS-connect error and any `copy_bidirectional` I/O error; a clean EOF stays
`Ok`, so no spurious logs on shutdown) and the spawn site logs on `Err` via the worker's `eprintln!("kastellan-worker-matrix:
…")` seam. **Verification — macOS hermetic:** matrix worker **11/0** default (+4: `transient_accept_errors_retry_immediately`,
`resource_and_unknown_accept_errors_back_off`, `backoff_delay_escalates_then_caps`, `relay_surfaces_uds_connect_failure`) /
**18/0** `live-matrix` (incl. the 2 `egress_spike` tests that drive the bridge through matrix-sdk); `cargo clippy
-p kastellan-worker-matrix --all-targets -D warnings` clean for **both** feature configs. Pure-Rust, no OS-gated code, no
`db`/cross-platform-gated change → DGX not required (the bridge is loopback-TCP↔UDS, identical on both OSes).
`bridge.rs` 110 → 287 LOC (under cap).)_

_(Prior session — **L1 embedding population — semantic recall lane now populated. MERGED to `main` as
`2ec853a` (PR [#324](https://github.com/hherb/kastellan/pull/324)).** Closes the forward write path of
[#323](https://github.com/hherb/kastellan/issues/323): no write path populated embeddings for any layer, so
`semantic_search` (`WHERE embedding IS NOT NULL`, layer-agnostic) returned 0 rows and recall ran lexical+graph only.
**What shipped (3 tasks, all TDD + reviewed):** (1) **`core/src/memory/embedder.rs`** — new `Embedder` async-trait seam
(mirrors the `EntityExtractor` seam): `embed_for_storage(text) -> Option<Vec<f32>>`; `RouterEmbedder` (delegates to the
existing `embed_query`, which already Matryoshka-truncates to `EMBEDDING_DIM` + writes the `action='embed'` audit row;
`Err → warn! + None`) + `NoOpEmbedder` (always `None`). `Option` not `Result` so the caller can't conflate
intentional-skip vs embed-failure (both store NULL). (2) **`promote_l1`** gains `embedder: &dyn Embedder`, called
**lazily — only after the dedup EXISTS-check passes**, so a duplicate body never triggers an embed; embed failure → row
stored with NULL embedding + WARN (degrade-and-warn, mirrors the entity-linker beside it; the insight write is never
blocked). (3) **Threaded `Arc<dyn Embedder>`** through `spawn_scheduler`→`lane_loop`→`drain_lane`→`write_l1_promoted_row`
(exactly like `entity_extractor`); `main.rs` builds the real `RouterEmbedder` for the agent-raised path. **Operator CLI
`l1 add` stays NoOp** (symmetric with its `NoOpEntityExtractor`; no Router in the CLI). **Decision: L1 only.** **Verification
— macOS, live PG 18:** `memory_l1_promote_e2e` **12/0** (+3: embed-on-insert + `semantic_search` finds it,
lazy-on-dedup-skip, degrade-and-warn); `embedding_recall_e2e` 4/0, `memory_recall_e2e` 2/0; `cargo clippy --workspace
--all-targets -D warnings` clean. Pure-Rust, no `db` change → DGX not required. **Deferred:** backfill / `kastellan-cli
memory l1 reembed` of existing NULL-embedding + operator rows (#323 item 2 — tracked in [#325](https://github.com/hherb/kastellan/issues/325)).
Spec/plan: `docs/superpowers/{specs,plans}/2026-06-20-l1-embedding-population*`.)_

_(Prior session — **Embedding dimension 1024 → 256 (Matryoshka). MERGED to `main` as `b06224f`
(PR [#322](https://github.com/hherb/kastellan/pull/322)).** Fixes the Matrix-session follow-up (b): the active embed model
**embeddinggemma** returns 768-d but the schema demanded 1024, so every embed failed the dim gate and recall ran with an
**empty semantic lane** (`recall failed; continuing with empty recall context`). Settled on **256**: embeddinggemma is a
Matryoshka/MRL model, so its 256-dim prefix (renormalized) is a valid, information-dense embedding — and 256 vs 1024 cuts
embedding storage ~4× and makes cosine ANN proportionally faster, with negligible MRL retrieval-quality loss. **What shipped:**
(1) **`db::memories::truncate_to_embedding_dim`** — pure fn (no I/O): rejects `< EMBEDDING_DIM` (can't upscale), else keeps the
leading 256 components + L2-renormalizes; 6 unit tests (768→256, unit-norm, direction-preservation, exact-length, too-short
reject, zero-vector no-div0). `EMBEDDING_DIM` 1024 → **256**. (2) **`embed_query`** (`core/src/memory/embed.rs`) now
Matryoshka-truncates the model output before the dim gate; the only surviving `EmbeddingDimMismatch` case is a model returning
*fewer* than 256 dims. (3) **Migration `0019_embedding_dim_256.sql`** — `ALTER` the three `vector(1024)` columns
(`memories`/`entities`/`deleted_memories`.embedding) to `vector(256)`, discarding stale embeddings first (NULL — a 1024-d vector
is not a valid 256 prefix, and they were never written in practice anyway; rows otherwise untouched, re-embedded on next write).
No ANN index to rebuild (0001 defers it). **Migrations 0001/0008 deliberately untouched** — sqlx checksums applied migrations,
so editing them would break validation on the live DGX DB; 0019 is additive. (4) Truncation chosen **client-side** over the
OpenAI `dimensions` request param so it doesn't depend on every backend honouring MRL truncation. Test fixtures updated
(`embedding_recall_e2e` mismatch test now uses `EMBEDDING_DIM-1`; payload-dim/cli_ask filler) + stale `1024` docs swept (manual,
llm-router doc-comments). **Verification — macOS, live PG 18:** db memories unit 16/0 (+6), core embed unit 3/0,
`embedding_recall_e2e` 4/0, `memory_recall_e2e` 2/0, `recall_assembly_e2e` 1/0, `cli_ask_e2e` 7/0, **db `postgres_e2e` 60/0
(full migration chain incl. 0019, real pgvector)**, `cargo clippy -p kastellan-db -p kastellan-core -p kastellan-llm-router
--all-targets -D warnings` clean. **DGX:** pure-Rust + plain-SQL migration, no OS-gated code; macOS PG 18 exercises the same
pgvector + migration SQL. The live DGX daemon applies 0019 on next deploy/restart — its embedding columns are all-NULL today
(768≠1024 meant nothing was ever stored), so the discard is a no-op there. **Follow-up ([#323](https://github.com/hherb/kastellan/issues/323)):**
no write path populates embeddings yet (l1_promote passes `None`), so the semantic recall lane is empty end-to-end until one
lands — when it does (`insert_memory_light` / l1_promote embedding population), route its model output through the same
`truncate_to_embedding_dim` chokepoint. (Ranking is unaffected by the renormalization since `semantic_search` orders by cosine
`<=>`, which is scale-invariant; review note from PR #322.))_

_(Prior session — **Matrix inbound channel — END-TO-END ROUNDTRIP LIVE on `matrix.kastellan.dev` under systemd.**
**MERGED to `main` as `9b5c310` (PR [#320](https://github.com/hherb/kastellan/pull/320)).** A real Matrix DM from `@horst` now runs through the agent and replies:
**inbound DM → invite auto-join → E2E decrypt → DB pairing → task → agent → LLM → reply** (verified: `17×23 → 391`, and a
free-text "Are you working now?" → coherent NL reply, both as `completed` tasks with `payload.kind="channel"`, on the
**systemd** `kastellan-core` daemon, `NRestarts=0`). **What shipped:** (1) `core/src/channel/matrix.rs` — `spawn_matrix_worker`
(sandboxed live-worker spawn via `build_matrix_policy` [`Net::Allowlist(homeserver:443)`, persistent E2E store as `fs_write`],
blocks on `matrix.init` so the returned `MatrixChannel` is logged-in; **password is `Option`** — the worker restores its
persisted `session.json`, so the daemon passes `None`), `SpawnedMatrixWorker`, `daemon_spawn_config_from_env` (gated on
`KASTELLAN_MATRIX_HOMESERVER_URL`; reads `_USER`/`_STORE`/`_WORKER_BIN`/`_ENFORCE_SANDBOX`), `host_from_url`. (2) `core/src/main.rs`
— replaced the Phase-D stub with real `ChannelBus::spawn` over `DbPeerAuthorizer` + `DbPairingService` + `PgChannelEvents` +
`PgCompletedTasks`, torn down before the scheduler on shutdown. (3) `workers/matrix/src/sdk_live.rs` — **auto-join invites**
(`register_autojoin_handler`, authorization stays fail-closed at the bus) + **cross-signing bootstrap**
(`ensure_cross_signing` via `bootstrap_cross_signing_if_needed`/UIA-password → bot self-signs its device; clears Element's
"device not verified by its owner" shield; server now returns `master_keys`+`self_signing_keys` for `@kastellan`, device
double-signed). (4) `kastellan-cli matrix probe` (`core/src/bin/kastellan-cli/matrix.rs`) — login/round-trip smoke +
`--send`/`--listen` diagnostic; keyring acquired **before** the tokio runtime (zbus `block_on` panics otherwise). (5)
**Installer:** `--matrix-homeserver-url`/`--matrix-user` flags write the `KASTELLAN_MATRIX_*` env block (survives reinstall,
which rewrites `kastellan.env`); **`ensure_v1_suffix` normalizes the LLM URL to `…/v1`** (the `:11434` default omitted it →
router hit `…/chat/completions` → HTTP 404; the agent's LLM calls were failing); `scripts/build-release.sh` builds the matrix
worker with `--features live-matrix` (a plain `--workspace` build is inert/refuses to run). (6) **`db` pool fix:**
`DEFAULT_MAX_CONNECTIONS` 4→16 — each long-lived `PgListener` (audit-mirror + 2 scheduler lanes + the new `tasks_completed`)
holds a pool slot; 4 listeners on a 4-slot pool starved every transactional query (claim_one/pairing/audit all timed out).
**Deployed:** `build-release.sh` + `kastellan-cli install --matrix-homeserver-url https://matrix.kastellan.dev --matrix-user
@kastellan:matrix.kastellan.dev` on the DGX; `@horst` paired via `kastellan-cli pair issue`.
**PR #320 review fixes (this session):** (i) the worker net allowlist was hardcoded to `:443` while `host_from_url` discarded
the URL port — now `host_port_from_url` scopes the allowlist to the homeserver's actual host:port (explicit port, or scheme
default https→443/http→80), so a self-hosted server on a non-443 port (e.g. `:8448`) is reachable; (ii) the supervised
respawn backoff loop had no shutdown escape (could spin forever against an unreachable homeserver after the bus was torn down)
— it now polls `inbound_tx.is_closed()` in 200ms slices and exits on channel shutdown; (iii) inbound messages that arrive
while the worker is down/respawning are silently dropped by the catch-up-sync `live` gate (needs a sync-token watermark) —
documented on `MatrixChannel::supervised` and tracked as [#321](https://github.com/hherb/kastellan/issues/321).
**Follow-ups (none blocking):** (a) **worker restart supervision** — self-healing respawn is now implemented
(`MatrixChannel::supervised`, capped backoff, replies retried across the bounce); residual is the inbound-loss window above
([#321](https://github.com/hherb/kastellan/issues/321)); (b) ~~**embedding dim mismatch**~~ — **FIXED** (this session, top
block: `EMBEDDING_DIM` 1024→256 + Matryoshka truncation); (c) **worker
hardening** — `KASTELLAN_MATRIX_ENFORCE_SANDBOX=0` for now (seccomp/Landlock off) + no egress force-routing coupling yet
(direct `--share-net`); (d) **in-daemon password materialize** — needs the keyring initialized outside the runtime (also the
latent `main.rs` bootstrap-secrets bug); (e) user-side device verification (TOFU) to clear the milder "you haven't verified
this user" state. Files also: `core/src/install/{plan,run}.rs`.)_

_(Prior session — **`kastellan-cli install` — MERGED (#316) + DGX post-merge verification + review fixes.**
The one-command per-user supervised installer (Postgres + daemon under `systemd --user` / launchd, from a freshly-built
tree) landed on `main` as `4fdafda` (PR [#316](https://github.com/hherb/kastellan/pull/316)). **What it does:** copies all
workspace binaries (atomic temp+rename, 0755) into a flat `~/.local/lib/kastellan/` prefix (so the daemon's
`current_exe()`-relative worker discovery just works) + assets into `~/.local/share/kastellan/`; shells out to idempotent
`kastellan-db-init --username $USER` (peer-auth role match); writes a tunable `~/.config/kastellan/kastellan.env` (mode
0600) carried by the new additive `ServiceSpec.environment_file` → systemd `EnvironmentFile=`; defaults to Ollama
`gemma4:26b-a4b-it-q8_0` + `embeddinggemma` (memory-fit-checked `ollama pull` when the endpoint is local Ollama, soft no-op
otherwise); enables linger (Linux); **restart (stop→start)** so reinstalls apply new artifacts; **verifies** (PG socket +
both services `active`, polled to 90s). `uninstall [--purge]` (typed confirm). `core/src/install/{plan,run}.rs` +
`core/src/bin/kastellan-cli/install.rs`. **Resolves HANDOVER open-question #6** (production install convention). **Review
fixes folded in this session (commit `608ce78` on the PR branch before merge):** (1) **launchd `EnvironmentFile=`
counterpart** — launchd has no such directive, so the LaunchAgents backend was silently dropping `environment_file` (a macOS
install would start the daemon with none of its tuned LLM/data config); `LaunchAgents::install` now reads the env file at
install time and folds its `KEY=value` pairs into the plist `EnvironmentVariables` (file overrides inline `env` on
collision, matching systemd's EnvironmentFile-after-Environment order). Pure `parse_env_file`/`merge_env` helpers live in
the sibling `supervisor/src/launchd_agents/builders.rs` (I/O-free; `launchd_agents.rs` now 526 LOC, +18, within the
≤27-over deferral). (2) **`uninstall --purge` idempotent** — `NotFound` per-dir treated as already-purged (no abort
mid-cleanup on a partial install). (3) **`--no-start` skips the `ollama pull`** — that mode only lays down artifacts.
Assets-source override deferred as [#317](https://github.com/hherb/kastellan/issues/317). **DGX post-merge verification —
PASSED (2026-06-20):** synced `main` to `4fdafda`, `cargo build --release --workspace` clean (exit 0, only the pre-existing
`sqlx-postgres` future-incompat warning); `kastellan-cli install` over the existing cluster **EXIT=0** (reinstall stop→start,
both models already present, 10 binaries, target up); `install --no-start` correctly **skips the model check** (units-only,
EXIT=0); final plain `install` EXIT=0 with both services `active`; **`kastellan-cli secret list` connects** (`(no secrets)`,
EXIT=0 → daemon authenticated to Postgres via peer auth); env file mode `0600` with all 7 keys, `EnvironmentFile=` present in
`kastellan-core.service`, `Linger=yes`. Supervisor 69/0 + core install (plan 10/0, e2e 2/0), clippy `-D warnings` clean.
Docs updated this session: this HANDOVER (header + open-question #6 + over-cap census), ROADMAP Phase-0 supervisor, README
quick-install section.)_

_(Prior session — **Matrix Phase D — live `LiveSdk` integration — DONE on branch
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
`KASTELLAN_MATRIX_HOMESERVER_URL`, `_USER`, `_STORE` (required), `_PASSWORD` (opt — only the *initial* login; restarts restore
`<store>/session.json`, so the spawn need not re-materialize the secret), `_DEVICE_NAME` (opt), `KASTELLAN_EGRESS_PROXY_UDS`
(opt). **Post-#313-review hardening (2026-06-19):** (1) a dead background sync loop now `process::exit(1)`s instead of silently
stalling (`poll` has no error channel — a dead loop looked alive while receiving nothing; exit lets the supervisor restart, and
skips the deadpool `Drop`); (2) `session.json` (access token + device keys) is now written `0600` via `write_private`; (3)
`_PASSWORD` made optional (above) — restored sessions don't need it. **Task 5 carry-forward:** the live e2e runs with
seccomp/Landlock `none`, so the matrix Landlock ruleset Task 5 wires **must grant RW on the persistent store dir** (the
background sync task keeps writing the SQLite state/crypto store after `lock_down`) or sync deadlocks — untested by the e2e.
**Verification — macOS hermetic:** matrix worker **14/0/0** (`live-matrix`, +5 `sdk_live` tests) / **7/0/0** (default);
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
names: `docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md#exact-sdk-builder-and-trigger-method-names`.)_

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
├── db                 kastellan-db: pure helpers (build_initdb_argv, build_postgresql_auto_conf, find_pg_bin_dir, pg_bin_dir_candidates_with_env_override) + conn::ConnectSpec + RUNTIME_ROLE/set_role_runtime_statement + probe::run (ensure DB → migrate as superuser → SET ROLE → audit, fail-closed) + graph::{Graph trait, PgGraph; recursive-CTE path() + walk_outbound/inbound_edges + walk_edges_around with DISTINCT ON diamond-dedupe} + audit::{insert, fetch_by_id, fetch_since, truncate_payload} + memories::{insert, insert_memory_at_layer, insert_memory_light (embedding-skipping light write path), semantic/lexical/graph search, link_memory_to_entities, set_skill_trust, load_layer_by_trust} + truncate_to_embedding_dim (Matryoshka 768→256 + L2-renorm; EMBEDDING_DIM=256) + entity_kinds + relation_kinds lookup caches + pool::{connect_runtime_pool, connect_admin_pool} + MIGRATOR (0001..0019; 0019 narrowed embedding cols to vector(256)) + memory_entities join table + deleted_memories audit table + secrets/ (AES-256-GCM at rest + OS keyring; prod-split into `crypto.rs` pure helpers [constants + validate_name/compute_aad/encrypt/decrypt] + `key_provider.rs` [KeyProvider trait + MapKeyProvider/OsKeyringProvider] + `error.rs` [SecretsError] + parent async DB I/O put/get/list/delete, all re-exported flat) + kastellan-db-init bin
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

**Just shipped (#340, branch `feat/340-injection-blocked-note`, PR #346):** the `tool_host` injection-blocked placeholder now
carries a human-readable `note` string leaf (`WITHHELD_NOTE`, pure `injection_placeholder.rs`) so the planner gets an
intelligible "content withheld" signal, not a bare reason-code; planner prompt + threat-model (incl. the split-slice screening
limitation) updated. See the "Last updated" header. **The whole #338 planner-feedback arc is now complete** (#337 error-half →
#338 success-half → #343 sink-screen → #339 global budget → #340 clear signal). The DGX is on `main`@`447767a`; #339 (`8fa67f9`)
and this branch are not yet deployed (no operator gate — neither change can loop or block; #339 bounds prompt size, #340 only
adds a string field).

**★ LEADING PICK — model-side perf (no code task): reduce the planner `num_ctx`.** ~86s/plan is gemma 26B on the DGX Spark with a
262144-token context; reducing the model's default `num_ctx` (`OLLAMA_CONTEXT_LENGTH` or a Modelfile — NOT per-request, which
forces a reload) is the cheapest live latency win now that the feedback arc is closed. Operator action on the DGX.

**Code picks (operator's choice — each ~one session):**
- **Matrix-worker seccomp/Landlock enforcement flip** (`KASTELLAN_MATRIX_ENFORCE_SANDBOX=0` today) — needs DGX syscall
  enumeration like the #281 arc (kill-mode + `journalctl -k`, see memory note); larger.
- **`tool_host.rs` prod-split** (now 667 LOC, the leading over-cap candidate) — lift `dispatch_with_sink`'s post-processing
  (scrub + injection screen + audit-emission arms) into a `tool_host/post_process.rs` sibling; tests already external under
  `tool_host/`.
- **[#298](https://github.com/hherb/kastellan/issues/298) full-daemon secret-scrub e2e** — needs a Vault-ref test seam in `main.rs`.
- **Test-infra debt:** the serialized `cargo test --workspace` live run wedges on `memory_layers_e2e` (0-CPU pool deadlock under
  heavy multi-cluster live-PG load — the documented sqlx-0.9 env issue); a focused isolation + `Pool::close()`/`PgListener`
  audit (cf. the #332 variant-D deadlock test).

**Prior entity-embedding work (still-valid follow-ups):** forward entity embed-on-insert shipped on
`feat/entity-forward-embed-on-insert` (the entity-embedding arc is complete: backfill #335 + forward). **Remaining entity-embedding follow-ups:** (1) an **ANN index** (ivfflat/hnsw) on
`entities.embedding` once entity cardinality warrants it (the lane does a sequential cosine scan today, matching the memories
semantic lane); (2) a **batch-embed seam** so the backfill + forward loops embed N entities per round-trip instead of one
`embed_for_storage` call each (sequential today; cheap to add behind the `Embedder` trait if embed latency becomes a recall-path
cost). **Open Matrix-hardening picks (residual follow-ups):**
(~~#321 inbound-loss window on respawn~~ — **DONE 2026-06-24**, sync-token-gated recovery; see header);
the matrix-worker **seccomp/Landlock enforcement** flip (`KASTELLAN_MATRIX_ENFORCE_SANDBOX=0` today; needs DGX
syscall enumeration like the #281 arc — larger). **Pre-existing test-infra debt surfaced this session:** the full serialized
`cargo test --workspace` live run wedges on `memory_layers_e2e` (0-CPU pool deadlock under heavy multi-cluster live-PG load —
the documented sqlx-0.9 env issue); worth a focused isolation + `Pool::close()`/`PgListener` audit (cf. the #332 variant-D
deadlock test).

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

**Production Matrix homeserver is now LIVE (2026-06-19): `matrix.kastellan.dev`** — Continuwuity (the maintained
conduwuit fork; conduwuit is archived), federation-off, loopback-bound behind Caddy auto-TLS, registration closed.
Accounts `@horst` (admin) + `@kastellan` (agent bot) exist. So Task 5 below now has a real homeserver to point at:
`KASTELLAN_MATRIX_HOMESERVER_URL=https://matrix.kastellan.dev`, `_USER=kastellan`, `_PASSWORD`=(store as a `db::secrets`
secret). Deploy details: runbook `docs/devel/runbooks/2026-06-19-matrix-homeserver-deploy.md`, scripts `scripts/matrix/vps/`,
doc `docs/deploy/matrix-homeserver.md`. **Gotcha for any redeploy:** a fresh Continuwuity server's first (admin) account
needs the one-time BOOTSTRAP token from the startup log, not the config `registration_token`.

**~~★ TOP PICK — channel-worker egress-coupled production spawn (plan Task 5) + daemon wiring.~~ — DONE, MERGED as
`9b5c310` (PR [#320](https://github.com/hherb/kastellan/pull/320)).** The live Matrix channel now runs end-to-end in the
systemd daemon (inbound DM → invite auto-join → E2E decrypt → DB pairing → task → agent → LLM → reply; see the prior-session
block up top). `core/src/channel/matrix.rs::spawn_matrix_worker` + `main.rs` `ChannelBus::spawn` over
`DbPeerAuthorizer`/`DbPairingService` shipped. **Residual follow-ups** (not blocking): ~~[#321](https://github.com/hherb/kastellan/issues/321)
inbound-loss window on respawn~~ — **DONE 2026-06-24** (sync-token-gated recovery; see header); ~~[#312](https://github.com/hherb/kastellan/issues/312) `ProxyBridge` error-surfacing~~ —
**DONE this session** (branch `fix/312-proxy-bridge-error-surfacing`; accept loop continues+logs+backs-off, `relay` returns
`Result` and the caller logs — see "Last updated" up top); matrix-worker hardening (`KASTELLAN_MATRIX_ENFORCE_SANDBOX=0`
today). **Historical note (the original pick, now satisfied):
Carry the
[#286](https://github.com/hherb/kastellan/issues/286) macOS-loopback caveat:** the `ProxyBridge` binds `127.0.0.1:0`
inside the worker (same pattern as browser-driver's `shim.py`); when this spawn grants the matrix worker a loopback-widening
Seatbelt profile on macOS, scope the grant to the bridge's bound port (or prefer a UDS-only transport / the `MacosContainer`
VM-netns backend). (~~Also [#312](https://github.com/hherb/kastellan/issues/312): make `ProxyBridge` surface accept/relay
errors instead of silently dropping~~ — **DONE this session**.) Plan: `docs/superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md` Tasks 5–6.

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
- **(b) Need a real prod split or a re-exported pure-helper seam** (a test-lift alone leaves the parent over cap): `core/src/cli_audit.rs` (958, the most over-cap production file), `db/graph.rs` (926, the design-gated Item 23b walk-impl split — deferred until a 2nd `WalkedEdge` consumer materialises), `core/src/scheduler/runner.rs` (777), `core/src/scheduler/audit.rs` (701, tests already lifted), `db/src/entities.rs` (653), `workers/prelude/src/seccomp_lock.rs` (650). (`core/src/scheduler/inner_loop.rs` is **DONE** — split via `inner_loop/invoke_expand.rs` [the `invoke_skill` expansion returning an `InvokeExpansion` enum] + `inner_loop/floor.rs` [`ClassificationFloorSource` + `apply_floor_raise`, re-exported] + `inner_loop/summary.rs` [#339: plan-summary rendering + the global budget]; back to **481 LOC** after #338/#343 grew it to 575. `db/secrets.rs` [848 → 252 + crypto/key_provider/error siblings], `systemd_user.rs`, `gliner_relex.rs` also done — see history.) Most over-cap production file remains `core/src/cli_audit.rs` (958).
  Also `supervisor/src/launchd_agents.rs` (526, +26) — Option K's install-time warn (+8) plus the installer's launchd `EnvironmentFile=` counterpart (#316 review fix: `install` reads `spec.environment_file` and folds it into the plist `EnvironmentVariables`; the pure `parse_env_file`/`merge_env` helpers live in the sibling `builders.rs` to keep the parent near cap). Tests already external, so a fix needs a real prod-split (disproportionate for a +26 file at the deferral threshold; deferred per this same ≤27-over policy — split the launchctl driver helpers if it grows). And `core/src/scheduler/tool_dispatch.rs` (507, +7) — pushed over by the handoff stash + `fetch_handoff` intercept; tests already external (`tool_dispatch/tests.rs`), so deferred per the same ≤27-over policy (a clean split would lift the `fetch_handoff` intercept + stash path into a `handoff_dispatch.rs` sibling if it grows).
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

- [#338](https://github.com/hherb/kastellan/issues/338) — **agent can't see successful tool output → tool tasks loop to the plan cap.** `plans_so_far_summary` renders `StepOutcome::Ok` as just `"ok"` (PR #337 fixed only the error half: `err: <CODE>: <detail>`). The agent re-runs the same step every iteration because it never sees the result. NOT a naive inline of stdout: feeding worker output into the planner prompt is the prompt-injection surface — route through `core/src/cassandra/injection_guard.rs` and/or the handoff/fetch design (`core/src/handoff.rs`, spec `2026-06-09-teach-planner-fetch-handoff`), bounded + classification-aware. This currently blocks ALL tool-using tasks end-to-end. Design-first.

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
6. ~~Worker binary discovery in production~~ / ~~production install convention~~ **RESOLVED 2026-06-20 (`kastellan-cli install`, PR #316 + DGX post-merge verification):** the installer copies all workspace binaries into a flat `~/.local/lib/kastellan/` prefix so the daemon's `current_exe()`-relative discovery (item 11, 2026-06-05) just works in a real deployment, brings up the supervised `kastellan.target`, and writes a tunable `~/.config/kastellan/kastellan.env`. Residual: FHS `libexec`/system-wide (multi-user) layout if/when packaging wants it (today's install is per-user, no root); optional `--assets-from` ([#317](https://github.com/hherb/kastellan/issues/317)).

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
