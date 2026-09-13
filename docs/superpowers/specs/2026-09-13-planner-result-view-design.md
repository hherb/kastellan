# Design — the planner's view of a tool result

**Issue:** [#677](https://github.com/hherb/kastellan/issues/677). Slice of
[#678](https://github.com/hherb/kastellan/issues/678); subsumes the mechanism behind
[#560](https://github.com/hherb/kastellan/issues/560). Date: 2026-09-13.

## The defect, as measured

`core/src/scheduler/inner_loop/summary.rs::render_step_outcome` builds the planner's view of a
successful step by calling `cassandra::injection_guard::extract_scannable_text(value, cap)`. That
function exists to flatten a JSON value into scannable text for the **injection guard**, and for
that job it is correct. As the planner's *only* view of a tool result it destroys the three things
the planner needs:

| What | Why it is lost |
| --- | --- |
| Every object key | `walk` iterates `for (_k, v) in map.iter()` — keys are discarded |
| Every number and boolean | `// Numbers, bools, null contribute nothing scannable.` → `_ => false` |
| All structure | Leaves are concatenated newline-separated, alphabetical by key within an object |

### The live evidence

Task 186 on the DGX (audit rows 3765–3801, 2026-09-05) asked where the last three flight bookings
went, saying the details were in a PDF attachment. It never called `mail.get_attachment_text`, the
tool task 185 had used successfully four minutes earlier.

A real `mail.search` hit, fetched from the live localmail on 2026-09-13 (correspondent and booking reference replaced; the shape and types are verbatim):

```json
{ "message_id": "3327", "account": {"id": "1", "name": null}, "folder": null,
  "subject": "Fwd: Flight Itinerary (Booking ref# ABC123)",
  "from": {"address": "traveller@example.com", "name": "A Traveller"}, "to": [],
  "date": "2018-11-11T13:58:11+00:00", "snippet_html": "…",
  "has_attachments": false, "score": 0.0618, "matched_arms": ["message"] }
```

Walked alphabetically with keys, numbers and booleans dropped, the planner saw:

```
1
2018-11-11T13:58:11+00:00
traveller@example.com
A Traveller
message
3327
…the snippet…
Fwd: Flight Itinerary (Booking ref# ABC123)
```

`has_attachments` is a **boolean** and vanished entirely, so the planner could not tell which hit
carried the PDF. `message_id` survived only as the bare line `3327`, indistinguishable from the
account id's bare line `1` — exactly #560's signature, now confirmed against the real API rather
than inferred. `score` vanished. The literal word `message`, from `matched_arms`, reads as content.

**Not the only cause, and the other two are out of scope here.** localmail silently ignores the
`has_attachment` filter (ten hits requested with it came back six `false`), and `mail.search`
rejects a filter-only search because `query` is a required `String` — which is what actually
failed task 186's second iteration with `-32602: missing field \`query\``, not the "near-duplicate
search" #677 describes. Both get their own issue. This design fixes the view.

## What ships

A pure, structure-preserving, budget-bounded **prune** of the result value, replacing the
flattening in `render_step_outcome` only. `extract_scannable_text` itself is untouched: the
injection guard, `handoff::summary_head` and `guard_capture` keep the exact function they have.

### 1. The pure core — `core/src/scheduler/inner_loop/result_view.rs`

A new module, because `summary.rs` is already 560 lines and over the 500-LOC cap.

```rust
pub struct PruneLimits { pub leaf: usize, pub items: usize, pub keys: usize }
pub fn prune(value: &Value, limits: PruneLimits) -> Value
/// The pruned value that fits `total`, plus its serialised byte length.
pub fn render(value: &Value, total: usize) -> (Value, usize)
/// Every object key and string leaf of a view, newline-separated: what the sink screen checks.
pub fn screen_text(view: &Value) -> String
```

- **Strings** longer than `limits.leaf` are cut on a char boundary and marked with a trailing `…`,
  **only when cutting actually shortens them**. The guard matters: cutting a 6-byte string to a
  4-byte cap and appending a 3-byte ellipsis would *grow* it. `apply_summary_budget` already uses
  this idiom for `OK_ELIDED_MARKER`; this reuses the reasoning.
- **Numbers, booleans and nulls** pass through unchanged. They are tiny, and they are the payload
  the labels exist to carry — dropping them is the defect.
- **Arrays** keep the first `limits.items` elements and append **one marker element**, the string
  `…<N> more items omitted`. A marker element rather than a wrapper object, so an array stays an
  array: the planner must emit parameters matching the tool's real schema, and a view that silently
  reshapes a list into `{items, omitted}` teaches it a shape the tool will reject.
- **Objects** keep the first `limits.keys` keys in `serde_json::Map` order — alphabetical, hence
  deterministic — and add `"_omitted_keys": <N>` when any were dropped. Without an object cap, an
  object with ten thousand short keys cannot be shrunk by a leaf cap at all, and the search below
  would bottom out and discard the whole value.
- **Arrays and objects get separate caps** (20 and 64). Tool results are *records* whose fields
  are all potentially meaningful and rarely number more than a few dozen, while *lists* are where
  breadth explodes; one shared knob would strip `message_id` from every hit before it shortened the
  list.
- **Depth** is capped at `injection_guard::MAX_WALK_DEPTH`, the existing `pub const`, **imported
  rather than re-spelled** so the two cannot drift. Same reason as there: a pathologically deep
  value must not overflow the dispatcher thread's stack (#143).

### 2. Meeting the total budget — a measured search, not byte accounting

Exact byte accounting during a single walk is where this would go subtly wrong. Instead `render`
tries candidates in an order chosen so the planner loses the least useful thing first, and
**measures every candidate before returning it**:

1. **Containers only.** Default array and object caps, strings whole. A small result is shown
   exactly.
2. **The highest string cap that fits,** by binary search between `LEAF_FLOOR` (64 B) and the
   longest string in the value. Every string is cut to the same "water level", so a single large
   document keeps as much text as the budget allows, while a list keeps all its hits with evenly
   trimmed snippets.
3. **Narrower containers,** strings at `LEAF_FLOOR`: halve the array cap to 1, then the object cap
   to 1.
4. **A fallback** `{"_view_unavailable": "result too large to summarise within budget"}`.

Because every returned candidate was measured, the postcondition `serialised.len() <= total` does
**not** rest on a monotonicity argument, and is a property test. Monotonicity only affects the
*quality* of the answer the binary search finds. It holds for the leaf cap given the never-grow
guard. It does not strictly hold for the container caps, because the omitted-items marker vanishes
when its count reaches zero, which is why step 3 is a measured linear walk rather than a search.
The guarantee holds for any `total >= MIN_VIEW_TOTAL` (128 B, above the fallback's 67); a
module-level `const` assertion in `summary.rs` pins the production budgets above it, in the style
#694 established and deliberately outside `#[cfg(test)]`, which release builds strip.

> **Corrected during planning (2026-09-13).** The approved draft capped every string at a fixed
> 512 B from the first round. `mail.get_attachment_text` returns `{"sha256", "text"}` with the
> whole extracted document in one string, and results up to `handoff::DEFAULT_RESULT_BYTE_CAP`
> (64 KiB) reach the render whole — so that draft would have cut attachment text to 512 B where
> the planner receives 4 KiB today, regressing task 185, the success path. The water level in step
> 2 gives a single document up to the whole per-step budget instead. The same review split the
> shared `children` knob into `items` and `keys`, for the reason given in section 1.

### 3. The outcome shape the planner sees

`step_outcomes[j]` changes from a string to an object. Today it is `"ok: <head>"` /
`"err: <CODE>: <detail>"`, so a pruned JSON document embedded in it would be double-escaped —
every `"` becoming `\"` — which is both noise for the model to read and a token cost on exactly
the payloads this change spends more budget on.

```json
{"status": "ok",  "output":   <pruned value>}
{"status": "ok",  "withheld": "failed injection screen"}
{"status": "ok",  "elided":   "summary budget"}
{"status": "err", "code": "POLICY_DENIED", "detail": "<bounded detail>"}
```

`status` stays `ok` / `err`, matching the real `StepOutcome`, because "the worker ran successfully
but its output was suppressed" is a distinction the current prompt teaches and that a
`status: "withheld"` would erase. Which of the three `ok` shapes applies is said by **which key is
present**. An `err` whose `detail` trips the screen keeps one shape, with the marker in `detail`.

`prompts/agent_planner.md` documents the old string shape in five places (lines 33–34, 213,
243–244, 249, 256) and is updated in the same commit. Prompts are read from `prompts/*.md` at
daemon startup, hashed into `agent_prompts`, and copied to the deployed assets dir by
`install/run.rs`, which `scripts/upgrade_from_git.sh` invokes — so the edit reaches the DGX.

### 4. Budgets

Raised, on the measured ground that DGX plan latency is generation-bound rather than
context-bound: dropping `num_ctx` from 262144 to 65536 bought about 10 %.

| Knob | Before | After | Why |
| --- | --- | --- | --- |
| per-step total | `STEP_OK_SUMMARY_MAX` 4 KiB | 16 KiB | The 27 KB task-186 result prunes to ~7–8 KB, so the whole result becomes visible and labelled |
| accumulated | `PLANS_SUMMARY_BUDGET` 32 KiB | 96 KiB | 6 × the per-step ceiling, so a full fast-lane task (`DEFAULT_MAX_PLANS_FAST` = 5, plus the forced-synthesis turn) cannot be pushed into elision by the ceiling alone |
| per-leaf string | none | adaptive, floor `LEAF_FLOOR` 64 B | The water level of section 2: one document keeps up to the whole step budget, a list trims snippets evenly |
| per-array items | none | 20 | Bounds a large listing without hiding a default `limit: 10` |
| per-object keys | none | 64 | Bounds a pathological map without touching a real record |

A long-lane task (`DEFAULT_MAX_PLANS_LONG` = 12) can still exceed 96 KiB and elide, which is
intended — that is the axis the deferred anchor index addresses, and it is not task 186's axis.

96 KiB is roughly 24k tokens against a 65536-token window, leaving the system prompt's tools,
skills and recalled memory ample room.

`ok_summary_cap`'s `web.search_batch` scaling is kept, now selecting the `total` passed to `render`.

### 5. The screening invariant

Today the screened text and the prompt text are the same string, and that must survive:
`render_step_outcome` screens `screen_text(view)` — every object key and every string leaf of the
pruned view, newline-separated — and that view is exactly what enters the prompt. Punctuation,
numbers, booleans and null are left out of the screened text for the reason
`extract_scannable_text` leaves them out: so the catalogue cannot fire on JSON shape itself. Keys
are **included**, because they now reach the planner and a worker authors them. Three
consequences, each worth a test:

- The sink screen now sees **more** kinds of text than before — keys as well as leaves — so it is
  not weaker on what reaches the prompt.
- Whatever the water level cuts away is cut *before* the prompt, so text the screen did not see
  cannot reach the planner.
- Raising the total to 16 KiB does put more untrusted text in front of the planner. Deliberate, and
  every byte of it is screened. The authoritative screen remains at the source
  (`tool_host` / `tool_dispatch::fetch_screen`) over `SCAN_BYTE_CAP`.

No `escape_untrusted_body` is needed: the context is a **user message** serialised by serde, so
there is no tagged block for a harvested string to close. That is why `step_outcomes` is not
escaped today, and the reasoning is unchanged.

## Testing

TDD, each test watched failing first.

1. `prune` preserves keys; preserves a boolean; preserves a number — the three losses above, one
   test each.
2. `prune` cuts an over-cap leaf and marks it; **never grows** a short one.
3. `prune` caps array length and appends a marker naming the dropped count; caps object keys and
   adds `_omitted_keys`; bails at `MAX_WALK_DEPTH`; walks back off a straddling multi-byte char.
4. `render` keeps **one large document** to nearly the whole budget (the regression the planning
   review caught), keeps **every hit** of a long listing with its id and flag, and narrows
   containers only when floor-length strings still do not fit.
5. **Property:** serialised length never exceeds `total`, over a range including pathological
   inputs — one enormous leaf, a wide array of wide objects, an object with ten thousand short keys
   (the cliff the object `keys` cap exists to close), and deep nesting.
6. The fallback is reached when nothing fits, and the fallback itself fits `total`.
7. Determinism; `screen_text` carries keys and string leaves but no punctuation or scalars. equal input, equal output.
8. **The regression test, with the real hit shape above:** the rendered view carries
   `message_id`, `3327` and `has_attachments`; and the *old* flattening does **not**, as a failing
   control proving the test discriminates rather than passing vacuously.
9. The four outcome shapes render as specified; a blocked output renders withheld and never the
   content.
10. The budget pass still elides oldest-first, now measuring serialised bytes; an injection phrase
    in an object **key** is withheld; the planner prompt documents every shape the renderer emits.

## Acceptance

A hermetic gate is necessary but explicitly **not** sufficient here: #536 rewrote a parameter
description, deployed, and both later runs still fabricated. Acceptance is a **live re-run** — the
DGX redeployed from this branch, task 186's question asked again over Matrix, and the planner
reaching `mail.get_attachment_text`. The new audit rows carry #694's `req_summary`, so what was
asked is recoverable this time. One live run is a sample of one, and the hermetic tests are what
stop the property regressing.

## Deliberately not in this change

- **The anchor index** (survey §3.1, #678 slice (e)). Its value is surviving *elision* on long
  tasks; task 186 never hit `PLANS_SUMMARY_BUDGET`. Once the view is a pruned value rather than
  flattened text, the harvest can run on the value — which is the correction this investigation
  forces on §3.1, whose proposed `anchor_index(text: &str)` would harvest from text the booleans
  have already left.
- **The tool-call guardrail** (survey §3.2, a duplicate-dispatch detector).
- **The planner seeing its own prior `(tool, method, parameters)`** — `render_plans_summary` emits
  only `{decision, step_outcomes}`, so it cannot tell that iteration 4 dropped the attachment
  filter iteration 3 used. Filed separately; a small change, but a different defect.
- **`plan.decision` reaching the prompt unscreened**, contradicting `sink_screen_blocks`'s
  documented "single, mandatory sink screen". Filed separately.
