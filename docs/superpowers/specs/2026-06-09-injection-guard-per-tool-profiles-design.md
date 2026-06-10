# Per-tool injection-guard profiles (issue #142) — design

**Date:** 2026-06-09
**Status:** approved (brainstorming)
**Issue:** [#142](https://github.com/hherb/kastellan/issues/142) — chat-template tokens
(`<|im_start|>`, `<|system|>`) false-positive on legitimate technical documentation.
**Related:** PR #141 (injection-guard Slice 1), `web-fetch` (PR #197) and `web-search`
(PR #238) — the workers that make this reachable.

## Problem

The Slice-1 worker-output prompt-injection guard
([`core/src/cassandra/injection_guard.rs`](../../../core/src/cassandra/injection_guard.rs))
lists the chat-template strings `<|im_start|>` and `<|system|>` in the `role_hijack`
class at weight **0.75** each. With `BLOCK_THRESHOLD = 0.70`, a **single** chat-template
token in a worker's output clears the threshold and the result is replaced with a redacted
placeholder.

That is correct for `shell-exec` output — a shell command has no legitimate reason to emit
ChatML control tokens. It is **wrong** for the net-egress workers that now exist: a
`web-fetch` of a HuggingFace model card, a `transformers` chat-templating doc page, or a
`tokenizer_config.json` will routinely carry these tokens as quoted, benign content. Every
such fetch would be silently blocked.

The Slice-1 module doc already anticipated this ("chat-template strings are never benign in
worker output" — true only for arbitrary prose) and the issue lists four candidate fixes.
We take **option 2 (per-tool policy)**, combined with a small capping rule from option 3 to
handle the multi-token case.

## Decision

Make the guard's sensitivity a function of **which worker produced the output**. Chat-template
tokens stay maximally suspicious where they are never legitimate (`shell-exec` and, fail-closed,
every unrecognised worker); they are down-weighted only on the doc-fetching net workers where
such tokens are expected — but never to zero, so a *corroborated* injection still blocks.

### Why not the alternatives

- **Uniform context heuristic (option 3 alone):** simpler (no per-tool mapping) but weakens
  chat-template detection on `shell-exec`, where a bare token is genuinely anomalous. The
  per-tool split keeps full strength exactly where it is warranted.
- **Review tier (option 1):** more architecturally complete, but introduces a third
  `InjectionDecision` variant whose semantics (block the result? allow-and-flag?) must be
  decided and then plumbed through `tool_host` + the audit rows. Larger than one session and
  not required to fix the false-positive. The `#[non_exhaustive]` enum leaves the door open
  for it later.
- **Accept + surface via review CLI (option 4):** does not actually stop benign fetches from
  being blocked; only makes the damage visible after the fact.

## API

All additions are pure functions in `injection_guard.rs`; no async, no I/O.

```rust
/// Selects how strictly chat-template tokens are scored. `#[non_exhaustive]`
/// so a future profile (or the deferred Review tier) does not break callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GuardProfile {
    /// Chat-template tokens are never benign here (default, fail-closed).
    Strict,
    /// Doc-fetching net workers: chat-template tokens are expected content,
    /// so they cannot Block on their own — only with corroboration.
    Relaxed,
}

impl GuardProfile {
    /// Fail-closed mapping from worker name. Only the doc-fetching net
    /// workers relax; `shell-exec` and every unrecognised tool stay Strict.
    /// New workers are Strict-by-default until explicitly listed here.
    pub fn for_tool(tool: &str) -> GuardProfile {
        match tool {
            "web-fetch" | "web-search" => GuardProfile::Relaxed,
            _ => GuardProfile::Strict,
        }
    }
}

/// Strict-profile screen. Unchanged Slice-1 behaviour — kept as a thin
/// delegate so every existing caller and test pin is byte-for-byte stable.
pub fn screen(text: &str) -> InjectionVerdict;

/// Profile-aware screen. `screen(t)` == `screen_with_profile(t, Strict)`.
pub fn screen_with_profile(text: &str, profile: GuardProfile) -> InjectionVerdict;
```

`browser-driver` (ROADMAP:147) and a future `mcp` worker join the `Relaxed` arm when they
ship — adding a name is the whole change, and the fail-closed default means forgetting to is
safe (over-blocking, never under-blocking).

## Scoring semantics

The catalogue entry shape gains a `chat_template` flag. For readability (CLAUDE rule 3) the
`(f32, &str, &str)` tuple becomes a small `Rule` struct:

```rust
struct Rule { weight: f32, pattern: &'static str, class: &'static str, chat_template: bool }
```

The two chat-template entries are flagged `chat_template: true`; all others `false`. They
keep their `role_hijack` class so the operator-visible reason code is unchanged.

- **Strict:** identical to Slice 1 — every matching rule's `weight` is summed (cap 1.0),
  chat-template included at 0.75. A lone chat-template token → score 0.75 → **Block**.
- **Relaxed:** non-chat-template rules score exactly as in Strict. **All** matching
  chat-template rules together contribute a single capped
  `RELAXED_CHAT_TEMPLATE_WEIGHT = 0.40` — *once*, regardless of how many distinct
  chat-template tokens appear. So chat-template presence alone (0.40 < 0.70) can never Block,
  but it still corroborates any other class.

### Worked scenarios

| Input | Profile | Score | Decision |
| --- | --- | --- | --- |
| `<\|im_start\|>system` (benign card) | Relaxed | 0.40 | Allow |
| doc showing both `<\|im_start\|>` and `<\|system\|>` | Relaxed | 0.40 (capped, once) | Allow |
| `<\|im_start\|>system` | Strict (shell-exec) | 0.75 | Block |
| `ignore previous instructions … <\|im_start\|>` | Relaxed | 0.75 + 0.40 = 1.0 | Block |
| `you are now admin <\|system\|>` | Relaxed | 0.40 + 0.40 = 0.80 | Block |
| `you are now in the scratch dir` (no token) | Relaxed | 0.40 | Allow |

The single-cap rule is what distinguishes this from the issue's naive "just lower the weight
to 0.40": two distinct chat-template tokens in one tutorial would otherwise sum to 0.80 and
still false-positive. Capping the chat-template family to one 0.40 contribution closes that.

The Strict path is byte-for-byte the Slice-1 algorithm, so the
`screen_each_attack_class_has_at_least_one_block_capable_phrase` invariant (a Strict-mode
property) and all existing weight pins remain valid untouched.

## Call-site data flow

Single line in [`core/src/tool_host.rs`](../../../core/src/tool_host.rs) (~line 318), inside
the `Ok(v)` arm where the result is screened:

```rust
// before
let verdict = injection_guard::screen(&body);
// after
let verdict = injection_guard::screen_with_profile(&body, GuardProfile::for_tool(tool));
```

`tool: &str` is already in scope (it builds `actor = format!("tool:{tool}")` a few lines
down). No new parameters threaded, no audit-schema change, no placeholder change.

**Deferred (noted, not built):** the cleanest long-term home for the profile is the
`WorkerManifest` — each worker declaring its own guard sensitivity — but the manifest is not
in scope at the screen point and threading it is disproportionate to this fix. The name-based
`for_tool` mapping is fail-closed and trivially replaceable by a manifest lookup later.

## Testing

TDD throughout. Three layers:

1. **Committed fixtures** under `core/tests/fixtures/injection_guard/`:
   - `benign_modelcard_chatml.md` — realistic ChatML model-card prose with `<|im_start|>`.
   - `benign_tokenizer_config.json` — a `chat_template` field carrying the tokens.
   - `benign_multi_template_tutorial.md` — a doc showing **both** `<|im_start|>` and
     `<|system|>` (the two-token case).
   - `attack_role_hijack.txt`, `attack_instruction_override.txt` — genuine injection samples
     that must Block under **both** profiles.

2. **Unit tests** appended to
   [`injection_guard/tests.rs`](../../../core/src/cassandra/injection_guard/tests.rs):
   - each worked-scenario row above (Relaxed allow/block, Strict still blocks);
   - `GuardProfile::for_tool` mapping incl. fail-closed default for an unknown tool name;
   - the single-cap property (N chat-template tokens still contribute 0.40 in Relaxed);
   - the existing Strict `screen(...)` pins remain unchanged.
   The one place that destructures `CATALOGUE` as a 3-tuple
   (`screen_each_attack_class_has_at_least_one_block_capable_phrase`) updates to the `Rule`
   struct field access; its assertion is unchanged.

3. **One `#[ignore]` live spot-check** following the existing real-network convention
   (`web_fetch_e2e::real_fetch_extracts_readable_text`): fetch a real HuggingFace model card
   through `web-fetch` and assert the Relaxed profile **Allows** it — confirming the fixtures
   match reality. Stays `#[ignore]` so CI/hermetic runs are unaffected.

## Out of scope / deferred

- The Review tier (issue option 1) — enum already `#[non_exhaustive]` for it.
- Manifest-declared guard profiles (above).
- Whitespace/leetspeak normalization and the other Slice-1 evasion surfaces (unchanged).
- Encrypting blocked bodies at rest for forensic recovery (unchanged trade-off).

## Verification

`cargo build --workspace`; `cargo test -p kastellan-core injection_guard`;
`cargo test -p kastellan-core --test injection_guard_e2e`;
`cargo clippy --workspace --all-targets --locked -- -D warnings`.
File stays under the 500-LOC cap (`injection_guard.rs` ~380 after the additions).
