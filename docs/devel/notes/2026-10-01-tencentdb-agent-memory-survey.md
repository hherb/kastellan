# Cross-project study — **TencentDB Agent Memory** (TencentCloud)

**Date:** 2026-10-01
**Status:** Investigation / design input (no code change in this note)
**Question:** [`TencentCloud/tencentdb-agent-memory`](https://github.com/TencentCloud/tencentdb-agent-memory)
pitches itself as a layered "memory asset" system for coding agents
(L0 conversation → L1 atoms → L2 scenarios → L3 persona, plus skills, an
LLM-maintained wiki, a code graph, tool-output offloading, and a transparent
LLM-API proxy that injects memory into Claude Code / Codex / etc.). Could it
become a dependency, and which of its ideas should we take?

> Source: shallow clone, **one squashed commit** (no history), **MIT**, version 2.0.0,
> TypeScript / Node ≥ 22. Read by three reviewers (MemoryCore pipeline;
> offload/skills/wiki/proxy/panel; our own tree). I spot-checked the two
> load-bearing "defect" claims myself (§3.1, §3.2). Everything else below is
> reviewer-reported with file:line and **not re-run** — nothing was executed.
> Kastellan facts were re-checked in-tree on 2026-10-01 where marked ✔.

---

## 0. Verdict

**Not a dependency. Four things are worth taking, mostly as *failure-mode checklists*
rather than features; a fifth (the lexical lane) is a finding about our own tree.**

The licence is fine (MIT). The shape is not: Node/TypeScript, state in
SQLite / Tencent VDB / COS / Redis / MongoDB, its own LLM egress, its own auth and
team/ACL model, and a **proxy architecture** (it sits in the plaintext path of every
prompt and holds the upstream API key). Every one of our hard constraints
(memory core-only, LLM only via `llm-router`, Postgres-only, workers never write
memory) is violated by design — the same reason cognee was rejected (#784).

More important is the **trust model, which is the opposite of ours.** Tencent
treats conversation as trusted input to everything persistent. There is no
boundary between a transcript and a stored skill, wiki page, scene or persona
(§3.3). That is exactly adversary #6 in `docs/threat-model.md`. Read the repo as a
catalogue of what our gates exist to prevent — and note that where it is
*engineered* well (locking, versioning, reversible compaction) the engineering is
orthogonal to trust and borrowable.

**The headline number is not evidence.** README claims PersonaMem 48 % → 76 %.
The repo contains no PersonaMem harness, dataset, judge, baseline definition or
config; all `bench`/`eval` code is Mongo load testing and token estimation. It is a
number only.

---

## 1. What it is, in mechanism terms

| Layer | What it actually does |
| --- | --- |
| **L0** | User/assistant **text** only (tools dropped), sanitised, daily JSONL + store row. Strips its own `<relevant-memories>`/`<user-persona>`/… tags before capture so recall isn't re-captured. |
| **L1** | One LLM call per batch (10 newest + 5 background messages) does scene segmentation **and** extraction → `{content, type, priority 0-100 (-1 = global instruction), scene_name, source_message_ids, activity_start/end, timestamps[], version}`. Types: persona / episodic / instruction (+ four work types). |
| **L1 dedup** | **Entirely LLM-driven**, no similarity threshold. Top-5 candidates per new memory (FTS ∥ vector, RRF k = 60) → one batch call returns `store / skip / update / merge` + `target_ids` + `merged_content`. Any failure ⇒ store everything. |
| **L2** | Markdown "scene blocks" edited by an LLM agent with file tools. Prompt-only rules: default UPDATE, ≤ 1 new scene per batch, ≤ 1500 chars each, cap 15 scenes. "Heat" is an LLM-written counter. Directory backup + restore on failure. |
| **L3** | `persona.md` ≤ 2000 chars, regenerated incrementally from scenes newer than `last_persona_time`; trigger = explicit L2 request, cold start, or ≥ 50 new memories. Guards against drift/poisoning are prompt-only; no diff, no rollback. |
| **Recall** | Per turn: L1 hybrid search (max 5) **plus persona and the full scene index always injected**; detail is pulled by tools (`tdai_memory_search` L1, `tdai_conversation_search` L0, `read_file` for scenes), 3 searches/turn by prompt. |
| **Offload** | Context-window manager for **tool outputs** (off by default; needs an upstream runtime patch). See §2.1. |
| **Skills** | A "Skill Review Agent" LLM (≤ 16 tool iterations) turns a conversation slice into skills; immutable `(skill_id, version)` snapshots with `is_head` and optimistic-lock `expected_version`. |
| **Wiki** | Two-phase incremental LLM ingest, sha256 per source, `locked:true` pages never overwritten, FTS5 + wikilink BFS (no vectors). |
| **CodeGraph** | Thin bridge to the third-party npm `@colbymchenry/codegraph`. Nothing of their own. |
| **Proxy** | Base-URL swap (`ANTHROPIC_BASE_URL=http://127.0.0.1:8096/…`), injects persona + skill listing, captures sessions after each final answer. |
| **Hub/Panel** | Teams, roles, `private/team/restricted/agent/task` visibility + ACL. Enforcement lives in `MemoryCore/src/metadata`. |

---

## 2. What to borrow

### 2.1 Reversible, tiered compaction with pointer placeholders *(highest value; feeds `context_manager`)*

**Our gap.** `handoff.rs` offloads *one oversized result* (> 64 KiB) behind a
`handoff_ref`. Nothing compacts the *running conversation*: `context_manager` is
ROADMAP-only, #678 (truncation) and #78 (global token cap) are open.

**Their shape** (`offload/hooks/llm-input-l3.ts`, server twin
`offload_server/compact/compressor.ts`), run on **every** LLM call, thresholds as
fractions of the window:

- **< 0.5** nothing.
- **≥ 0.5 mild:** replace tool results, highest score first, with
  `[Offloaded Tool Result] summary + result_ref`; the original stays on disk. Skip a
  replacement whose summary is larger than the original.
- **≥ 0.85 aggressive:** delete oldest messages until under threshold; only the last
  user message is protected; tool-use/result pairing repaired.
- **≥ 0.95 emergency:** delete down to 0.6, truncating oversized messages.

**Take:**
1. **Replace-with-pointer, not delete.** It is the same protocol as our handoff
   (`{ref, byte_len, head}` + a fetch tool) applied to *history*. It satisfies
   openworker's "never rewrite persisted transcript" line already in the ROADMAP.
2. **Re-apply the decisions on every call so the prompt prefix stays byte-stable.**
   They say this is why the fast path exists. For a local vLLM/Ollama host with
   prefix caching that is a real latency win and a cheap property to pin with a test.
3. **Tiers by fraction of window**, composing with the openworker
   `min(0.8·window, 250 k)` cap already specified.

**Do not take:**
- **LLM-written summaries and LLM-assigned "replaceability scores".** Theirs are
  generated from the first 2000 chars of a result and self-scored. Ours should be
  mechanical (size, age, kind) — consistent with the Hermes finding already in the
  ROADMAP that an LLM-free anchor index beat summarisation (68.3 % vs 45.8 %).
- **The Mermaid "task state machine" re-injected as `role:"user"`** (`mmd-injector.ts`).
  An LLM-written artefact derived from tool output (incl. web content), re-entering as
  user-authority text, is a prompt-injection amplifier. If we want a re-orientation
  artefact it goes through `escape_untrusted_body` and carries no user authority.
- **Aggressive/emergency deletion of user and assistant text.** Only tool results are
  archived in their design; the rest is gone.

### 2.2 A failure-mode checklist for any distillation writer (#629 L4, reset snapshot)

The cognee note §4.6 already flagged watermarked, idempotent stages. This repo
supplies *concrete defects* to turn into tests. **Each is a test case for our L4
digest writer:**

| Their defect (verified ✔ where marked) | Our test |
| --- | --- |
| ✔ LLM failure returns `{success:false}`; the runner never reads it and **advances the L1 cursor anyway** (`l1-extractor.ts:216` vs `pipeline-factory.ts:604-660`). A transient outage permanently skips a batch. | A failed summariser call must **not** move the watermark. |
| Dedup update/merge **deletes targets before upserting** the merged row; a crash between loses both. | Replace is one transaction, or insert-new-then-supersede. |
| A failed L3 generation is treated as "no change" and **resets the trigger counter**. | A failed run leaves the trigger state exactly as found. |
| Equal-millisecond rows at a batch boundary can be missed (a TODO admits it). | Cursor is `(ts, id)`, never `ts` alone. |
| `deleteL1Expired` doesn't delete the FTS row. | Every derived index row is removed in the same transaction as its source. |

Good ideas to copy from the same code: **archive-before-enqueue** (fixes a "ghost
task" race), **over-fetch N+1 as the backlog signal** instead of a separate COUNT,
and a **downward-only timer** (`max(now+10 s, last+900 s)` with a hard max) so
bursts coalesce.

### 2.3 Richer L1 provenance: source ids and event time

**Our gap.** An L1 row's metadata is `{source, body_sha256, created_at, task_id}`
(`l1_promote.rs`) — no pointer back to *what was said*, no event time distinct from
write time.

**Their shape.** `source_message_ids`, `timestamps[]`, and for episodic items
`activity_start_time`/`activity_end_time`; the generation log records input/output
refs, **prompt hash and model** per layer.

**Take:** `source_audit_ids` (audit-log id range, as #629 already does for digests),
and a distinct `observed_at` vs `valid_from/valid_to`. This pairs with the proposed
`relation_observations` table (cognee §4.1) and gives supersession (§4.2) its
recency key. **Skip** the 0–100 LLM priority: unvalidated in their code
(only non-numeric → 50) and an injection target.

### 2.4 Dedup *action vocabulary*, routed to the operator

**Our gap.** L1 dedup is exact `body_sha256`. A paraphrase of an existing insight
is a new always-on row; there is no conflict/supersession handling.

**Their shape.** Candidate recall → `store | skip | update | merge`.

**Take the vocabulary and the candidate step, not the autonomy.** An agent that
merges or deletes its own memory is adversary #6. A `merge`/`update` verdict becomes
a **proposal in the operator review queue** (the quarantine-review surface), exactly
like cognee §4.3. Note their design weakness to avoid: no similarity threshold, so
OR-joined FTS candidates make the LLM call fire almost every time. Gate on a measured
threshold — which needs §2.5 first.

### 2.5 Evaluation before more memory features

Unchanged from cognee §4.5 and ROADMAP:77: **no recall eval exists**; every idea
here is a hypothesis until it does. Tencent's PersonaMem number reinforces the point
by being unverifiable. If an external corpus is wanted, PersonaMem could be a
candidate *for our harness* — I have **not** checked its licence, format, or whether
it fits a four-lane Postgres recall; do that before committing. Build the operator
golden set first.

---

## 3. Findings about our own tree (surfaced by this study)

### 3.1 The lexical lane ANDs every term ✔ — worth a measurement

`db/src/memories/search.rs:114` uses `plainto_tsquery('simple', $1)`, which **ANDs all
terms**, and the query is the **whole task instruction** (`ctx.instruction`,
`scheduler/agent.rs`). A long natural-language instruction will match almost nothing
lexically. Tencent OR-joins quoted tokens and BM25-ranks. This is a hypothesis, not a
measured defect (RRF means the lane degrades silently rather than fails) — but it is
the cheapest item here to confirm once the eval harness exists, and the fix
(`websearch_to_tsquery`, or an OR of the instruction's top terms) is small.

### 3.2 Recalled text can be restated as a new L1 insight — provenance is not checked

Their feedback-loop defence (strip your own injected tags before capture) has no
counterpart in our write path, because we capture nothing from dialogue. But the
planner **can** emit `l1_insight` on a terminal plan, in a prompt that contained
`<recalled>`, channel text and tool output. `validate_l1_body` ✔ checks newline,
control chars, reserved tag, length; `screen_agent_raised_body` ✔ runs the strict
injection catalogue; dedup is exact sha. **Nothing asks whether the insight merely
restates untrusted content** — a paraphrase of an email body would pass and become an
always-on, `<l1_insights>`-trusted row with no operator approval. Not shown to be
exploitable; **worth a threat-model line and a test** that tries it. (Agent-raised L1
having no operator gate is already documented; this is the laundering path into it.)

### 3.3 Their trust model, as a checklist of what our gates prevent

Reviewer-reported, file:line available on request:
- Injection filter `looksLikePromptInjection` exists but its call is **commented out**
  (`sanitize.ts:153`) ✔; recalled L1/L2 text is injected **unescaped**.
- Skills go live with no review queue; the prompt says "when in doubt, capture", and the
  listing header says the agent **MUST** load matching skills. A poisoned transcript
  becomes a persistent privileged instruction. Transcript delimiters
  (`<<end-of-transcript>>`) are not escaped. `protected:true` is prompt-only.
  `reviewer` / `candidate→approved` exist in the schema with **no enforcement code**.
- The L3 persona agent's file-tool workspace is the **whole data dir**, not just
  `persona.md`.
- Skill-bridge identity is a conversation-id header; ids travel in curl commands
  through the model and the upstream provider.
- No secret redaction before L0, offload refs or skill extraction; the bulk importer
  (`agents/asset-import.ts`) uploads raw sessions.
- The wiki's SSRF guard is a hostname **regex** (bypassed by DNS or redirect). We have
  `net-classify`.

Our `untrusted → user_approved → pinned` ladder, `escape_untrusted_body`, the
quarantine-by-default entity graph and `data_ceiling` are what is missing there.
**No change needed; it validates the design.**

---

## 4. Explicitly not borrowed

- **The proxy/injection architecture.** Kastellan *is* the agent daemon; there is no
  third-party client to front. Also: a proxy that holds the upstream key and sees all
  plaintext is a bigger target than anything we run.
- **Auto-generated persona (L3) injected into the system prompt.** Their trigger
  cadence (every N memories, incremental from a watermark) is reasonable; the
  *unguarded system-prompt channel* is the problem. Our stability-scored preference
  learning (ROADMAP:177, operator `profile pin` only) is the right shape.
- **Team/ACL/visibility model, quota/credit billing, ClickHouse analytics.** Single-user.
  (One transferable detail: `private` means *not even admins* — which is our default
  already.)
- **LLM-edited scene files with prompt-only caps.** Cap 15, ≤ 1500 chars, "heat" are
  all unenforced in code. If a project-context block is ever built it is
  operator-curated or digest-derived (#629), with code-enforced caps.
- **Skill extraction by a review agent over raw transcripts.** Ours stays
  planner-emitted, grounded after ≥ 1 dispatch, untrusted until approved.
- **Per-turn injection of the full scene index.** Fine for them; we already bound
  recalled text to 4 KiB and have no global cap (#78) — adding another always-on block
  makes #78 more urgent, not less.

### Smaller, noted for later

- **Wiki shapes worth remembering if document ingest ever lands:** sha256-per-source
  incremental ingest; `sources` frontmatter unioned so deleting a source
  **cascade-prunes** orphaned pages (the same retraction idea as `relation_observations`
  — derived artefacts die with their sources); `locked:true` ≈ an operator pin;
  wikilink BFS with per-hop score decay and a 200-node cap.
- **Skill versioning:** immutable `(skill_id, version)` + `is_head` + `expected_version`
  CAS. We have content-hash dedup but no version/mutation ledger (ROADMAP:173); this is
  a ready-made schema for it.
- **Recall hygiene they get wrong, we get right:** recall timeout via `Promise.race`
  that doesn't cancel the work; char budgets defaulting to **unlimited**; `scoreThreshold`
  ignored on the hybrid path. Ours: hard 4 KiB cap, degrade to empty.
- **Cold-start import** from existing notes/sessions: if ever built, an *operator CLI*
  that feeds the normal L1 gates and runs `leak-scan` first.

---

## 5. Suggested next steps (smallest first)

1. **Documentation only:** add §2.1 (reversible tiered compaction, prefix stability,
   mechanical scoring, no LLM-authored user-role artefacts) to the `context_manager`
   ROADMAP entry, and §2.2's table to #629 and the reset-snapshot-writer entry as
   acceptance tests.
2. **Two small issues:** the `plainto_tsquery` question (§3.1, blocked on the eval
   harness) and the restated-content L1 question (§3.2, test + threat-model line).
3. **Fold §2.3 into the `relation_observations` proposal** (ROADMAP:76) so L1 rows and
   edges get provenance in one design.
4. **Eval harness (ROADMAP:77) before §2.4.**
