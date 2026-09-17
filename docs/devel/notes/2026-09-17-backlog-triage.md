# Backlog triage — 2026-09-17

A pass over all **150 open issues**. This note records the findings and the decisions so a later
session can act without redoing the analysis, and so the GitHub half stays reproducible.

**Status: the ROADMAP fix landed; the GitHub half has NOT been executed.** Every issue named below
is still open. The tool sandbox in the triage session blocked external writes (`gh`), so the actions
are staged as the script in §6 rather than applied.

---

## 1. The shape of the backlog

Closure is **healthy for PR-driven work and zero for roadmap-era work.** Every recently closed
issue (#661–#701) was closed by the PR that fixed it. Meanwhile:

| Filed | Open |
|---|---|
| 2026-05 | 14 |
| 2026-06 | 44 |
| 2026-07 | 11 |
| 2026-08 | 47 |
| 2026-09 | 34 |

**58 issues (39%) were filed 2026-06 or earlier and have not been touched since filing.**

The root cause is mechanical: **~7% of issues carry any label** (10 of 150 — three `bug`, five
`enhancement`). With no taxonomy the old cluster cannot be filtered for, so nobody sees it. Adding
a minimal label set is the cheapest thing that stops this regrowing.

## 2. The ROADMAP is the accurate source; the issues are the stale mirror

This is the reverse of what the epics assumed, and it is what makes the triage tractable — the
status markers in `docs/devel/ROADMAP.md` decide each case, so no closure rests on reading the
issue's own (stale) text.

### Close as shipped

- **#227** micro-VM backend for `python-exec` — ROADMAP Phase 4 `[x] Linux Firecracker … SLICE 1
  (2026-06-27, PR #364)` and Phase 1 `[x] macOS Apple container micro-VM backend — 2026-05-21`.
  Operator switch `KASTELLAN_<WORKER>_USE_MICROVM=1` is documented in CLAUDE.md and dispatched in
  `core/src/broker/spawn.rs`.
- **#215** channel-bus fan-in — ROADMAP Phase 2 `[x] Channel-bus abstraction (build first) —
  2026-06-12`; `core/src/channel/bus.rs` + `ingest.rs` are the chokepoint. #701/#709 built
  conversational continuity *on top* of it.
- **#216** DM pairing flow — ROADMAP Phase 2 `[x] DM pairing flow: — 2026-06-12`;
  `core/src/bin/kastellan-cli/pair.rs` ships issue / issue-token / list / revoke, with migration
  0018 REVOKEing minting from the runtime role. **The title's TOTP/HOTP + WebAuthn was dropped on
  purpose, not missed:** a single-use, SHA-256-stored, out-of-band code makes a rotating OTP
  redundant, and the forgeable-static-allowlist failure mode is what `issue-token` plus the
  DMARC+token gate addresses. WebAuthn, if wanted, needs its own issue and its own justification.

### Close as rejected — decided 2026-06-12, never mirrored

- **#214** Telegram inbound — ROADMAP strikes it through: *"rejected as primary 2026-06-12 (no bot
  E2E, centralized, ban risk)"*.
- **#219** Telegram/Signal outbound — ROADMAP Phase 3: *"(~~Telegram/Signal outbound~~ rejected as
  primary — see Phase 2 note.)"*

### Close as superseded

- **#213** IMAP inbound worker — the ROADMAP says so in words: *"Supersedes the original 'IMAP
  inbound worker' framing: localmail now exists and already ingests the mail over IMAP, so the
  channel polls its /v1 subscription instead of speaking IMAP itself."* Confirmed in the tree —
  `workers/email-in/src/client.rs` is a localmail REST client, and nothing in kastellan speaks IMAP.

### Close the six epics — #203 to #208

Their own bodies state the trade: *"Source of truth remains the ROADMAP; this mirror exists so the
work shows up on the GitHub Project board."* The mirror was never synced and now misleads:

1. **Every ROADMAP link points at `github.com/hherb/hhagent/…`** — filed 2026-06-08, project renamed
   two days later.
2. **The status text is wrong** — Phase 3 lists the egress proxy as pending (shipped 2026-06-10,
   `df51c5c`); Phase 4 says "All items pending" while python-exec and both micro-VM backends shipped.
3. **The children drifted independently** and none of it reached the epic.

A hand-synced mirror of a well-maintained file is debt with no offsetting benefit. Individual work
items still get their own issues as they become actionable — that part works.

### Stays open, correctly scoped

- **#232** audit log viewer — ROADMAP `[ ] Read-only audit log viewer (CLI complete; web optional)`
  matches the issue exactly. The CLI half shipped (`core/src/bin/kastellan-cli/audit_tail.rs`); the
  issue tracks the optional web half.
- **#220** SMTP outbound, **#211** `context_manager`, **#212** reset snapshot writer, **#223** MCP
  onboarding, **#228** tiered delegation, **#229** preference learning, **#230** policy gate,
  **#231** frontier escalation, **#233** soak test, **#209**/**#210** — all still `[ ]` on the
  ROADMAP and genuinely unstarted.

## 3. One real ROADMAP defect, fixed

The Phase 4 line for the `python-exec` micro-VM backend had stayed `[ ]` since the original seeding
while **duplicating two `[x]` entries** — its text still described the work at "discovery spike …
verdict COMMIT" stage, years of shipping later. Corrected in the same commit as this note.

This is the only tree change the triage produced.

## 4. The dominant theme: false-green gates

**Open:** #714 (Postgres e2e has no REQUIRE knob), #664 (gliner-relex REQUIRE_E2E exits 0 on a
filtered run), #622 (guard_tier_e2e is in no gate *and* self-skips to PASS), #691 (the #687
freshness gate is falsified by the repo's own clippy recipe), #655, and #237's absent macOS leg.

**Already closed in the same class:** #679, #667, #682, #684, #687.

Fixing these one at a time keeps regenerating them. One uniform contract — **every gate needs an
explicit REQUIRE knob *and* a positive control that fails when zero tests ran** — retires the class.

### #655 is the precondition for all of it

Verified still true on 2026-09-17:

```
$ gh api repos/hherb/kastellan/branches/main/protection   → 404 Branch not protected
$ gh api repos/hherb/kastellan/rulesets/16475311          → protect_main: deletion, non_fast_forward
```

**No required status checks, so every CI job is advisory** — a gate that cannot block a merge is a
notification. This makes the rest of the cluster moot until it is fixed, and it is one API call.

**Safe to enable:** `linux-check.yml` triggers on `pull_request:` with **no `paths` filter**, so all
three jobs report on every PR. The usual hazard — a required check that never reports, deadlocking a
docs-only PR — does not exist here. Use `strict_required_status_checks_policy: false`; requiring
branch-up-to-date would serialize merges for no safety gain at this size.

## 5. Smaller findings

- **#609 / #610 / #611** are titled `guard tier`, `guard audit`, `guard boot report`. Each has a
  detailed, specific body — they are unfindable by title, not empty. Retitles in §6.
- **#673 + #674** are two halves of one incident (the DGX localmail token expired 2026-08-30 and
  surfaced five days later as a wrong answer). #674 is why nothing noticed; #673 is why it was then
  misdiagnosed, `POLICY_DENIED` making "bad credential" and "kastellan refused on policy" identical.
  Fix together.
- **#696 + #591** are the same shape — one helper copied rather than shared (`sha256_hex` twice; the
  char-boundary truncation walk five times). #591's note that *"its correct test exists in only one
  place"* is why it matters: copies do not share the original's tests, so they do not share its fixes.
- **#237** CI gate is mostly done — clippy is live, hermetic per-crate tests are live, `cargo fmt
  --check` is deferred **deliberately** (CLAUDE.md: no rustfmt config yet). The real remainder is
  **the macOS leg: all six runners across all three workflows are `ubuntu-latest`**, against a hard
  "Linux + macOS first-class" constraint. Pair with #655 or it lands advisory.
- **The guard cluster** (#594–#612, ~18 issues from one 2026-08-22/23 burst) is 12% of the backlog on
  one subsystem. Decide whether the guard tier is current priority or batch-defer it behind a label.
- **#707** (Obscura) correctly remains open — ROADMAP still carries it as *"BLOCKED upstream on a
  stale V8"*, which #708 documented rather than resolved.

## 6. The staged actions

A reviewed, runnable script performing every GitHub action above — 12 closures with evidence-quoting
comments, 3 retitles, 4 cross-link/re-scope comments, and the #655 ruleset change with verification —
was produced alongside this note. It was **not run**.

Recommended order:

1. **#655** — one API call; makes every other gate binding.
2. The 11 closures — no code, removes ~7% of the backlog.
3. A minimal label set, so this does not silently regrow.
4. Then the false-green-gate cluster as one piece of work.
