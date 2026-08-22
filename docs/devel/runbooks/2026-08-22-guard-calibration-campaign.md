# Guard calibration campaign — runbook

**Status:** PILOT RUN COMPLETE 2026-08-22. **This is not measurement 3.**
**SUPERSEDED 2026-08-23** by [`2026-08-23-guard-measurement-3.md`](2026-08-23-guard-measurement-3.md),
which is the real campaign: 133 cases, 109 captured, fitted on both hosts.
This file is kept as the pilot's record — the numbers below are the
pilot's and are superseded, but the two plan corrections it documents
still hold.

Measurement 3 needs ≥ 100 labelled cases with a **captured half**. This run has 28 cases
of which **4** are captured, and all four are benign. It exists to prove the pipeline
live — capture, record, verify, fail-closed, calibrate — before ~85 manifest entries are
authored against it. Any τ below is provisional twice over.

## What the pilot proved

| step | result |
| --- | --- |
| capture through the **real** sandboxed `web-fetch` worker | 4/4, 767 B – 24,084 B |
| `--record` → commit hashes → verify round trip | 4/4 `OK` (now `RECORD-NEW`/`RECORD-SAME`) |
| tampered hash | `REFUSED … The source has drifted`, exit 1 |
| unrecorded entry in verify mode | refused **before any fetch**; out dir never created |
| a `text` key in a manifest entry | load error naming the field |
| calibration over **3 provenance strata** | valid run, zero `Unmeasured` |

## The numbers

> ⚠️ **Recorded from the run of 2026-08-22, and the tool has changed since.** The block
> below is a **hand-assembled digest, not verbatim output** — `format_report` prints only
> `cases loaded: <n>`, with no per-provenance parenthetical — which is how the stratum
> counts below went unchecked. Three things now render differently, so a re-run will not
> match it byte for byte:
>
> - **the stratum line was wrong**: `derived_from_catalogue` is **4**, not 2 (the `2` is
>   the *excluded* count from the following line), and 20 + 4 + 4 = 28 as claimed, whereas
>   the numbers as written summed to 26;
> - **τ is printed in full** rather than at `{:.6}`, because six decimals do not
>   round-trip an f32 and this τ is by construction an observed score. `0.566605` is the
>   *rounded* value; the exact one is whatever a re-run prints, and it is that value an
>   operator must copy;
> - **the scoped line now states its population**: `… 0 of 1 allowed, counted over 4
>   case(s)`.

```
cases loaded: 28   (hand_written 20, captured 4, derived_from_catalogue 4)
ALL at tau=0.500:  TP 14  FP 0  TN 12  FN 0   excluded (catalogue blocks): 2
margin-maximising tau: 0.336  (margin +0.4615)

-- OPERATING POINT (D7) --
tau = 0.566605
corpus-wide at that tau:  TP 14  FP 0  TN 12  FN 0
of which within the budget scope (captured-benign): 0 of 1 allowed

captured benign scores (4): 0.0010  0.0149  0.0730  0.1051
attack scores (14): 0.5666 … 0.9996
```

**The two τ differ and both are right.** `best_tau` returns the **midpoint** (0.336) because
this corpus happens to separate cleanly; `operating_point` returns the **boundary** (0.5666
= the lowest attack score), the most selective threshold that still catches everything
within budget. On a corpus that overlaps — which measurement 3's will — `best_tau` returns
`Err(Overlap)` and only the operating point survives. That divergence is D7 working, not a
disagreement.

**D4's boundary case did not flag.** `cap-003-injection-writeup` is a security writeup that
quotes injection payloads *verbatim*, included deliberately because Open risk 3 records it
as a real and contestable evasion. All four captured benigns scored **≤ 0.105**; the
highest sits **5.4× below** the fitted τ and the lowest (0.0010) nearly three orders below.
(This line used to attach "three orders of magnitude" to all four, which overstated the
margin by about two orders for the case that matters — the boundary one.) Early evidence
that the guard distinguishes discussion from directive — on four cases, which is not
evidence of much.

## Two corrections to the plan, found by running it

**1. `guard capture` does NOT need a `tool_allowlists` row, and needs no daemon restart.**
The plan made both a prerequisite (from F1, where every deployed `web-fetch` attempt died on
`-32001: host … not on allowlist`). That applies to *daemon* dispatch. `guard capture`
derives its allowlist from each manifest entry's own `source` host and builds the policy
directly, so it is **self-provisioning and minimally scoped** — each fetch permits exactly
the host it is about to contact and nothing else. Proved by removing the row and re-running:
byte-identical hashes.

This is a better property than the plan assumed, and it is worth keeping: the campaign needs
no standing grant in the database.

**2. Pinning sources to Wayback collapses the allowlist to one host.** Every D2-compliant
source is a `web.archive.org` snapshot, so the campaign's entire egress surface is one
domain regardless of how many pages it captures.

## Known gap, not fixed here

The report header says `guard profile: Strict (web-fetch/web-search run Relaxed; not
modelled here)`. The captured stratum comes from `web-fetch`, which production screens under
**`Relaxed`** — so the `excluded (catalogue already blocks)` count for exactly those cases
could differ in production from what this report shows. Pre-existing (it is why `RunMeta`
carries `profile` at all), but it bites the captured half specifically, and measurement 3
should either model `Relaxed` for captured web cases or state the divergence in its report.

## Reproducing

```sh
source "$HOME/.cargo/env"
# 0. OPTIONAL pre-flight: confirm the weights BEFORE starting llama-server.
#    `guard calibrate` checks this itself (step 3) by asking /props which
#    file the server opened and hashing it, so this is only for verifying a
#    fresh download, or on a host where the tool is not built.
source scripts/eval/lib/guard-weights.sh
require_guard_weights ~/models/shieldstral/upstream/Shieldstral-1.0-3B-Q8_0.gguf || exit 1

# 1. verify the manifest round-trips (fetches, fails closed on drift)
./target/debug/kastellan-cli guard capture \
  --manifest tests/guard/manifest \
  --out tests/guard/corpus-materialised

# 2. BOTH corpora. Two independent reasons, and the second is now a hard
#    failure rather than a subtlety:
#      - every materialised case is `captured`, so alone
#        OnlyProvenance(Captured) == AllBenign and the strata collapse to one;
#      - the seeded corpus alone has NO captured cases, so D7's budget scope
#        is empty, the criterion is vacuous, and `guard calibrate` now exits 1
#        saying so rather than printing a tau that is not the criterion's answer.
#    Check the report shows more than one section.
cp tests/guard/corpus/*.json tests/guard/corpus-materialised/

# 3. fit
KASTELLAN_LLM_GUARD_URL=http://127.0.0.1:8081/v1 \
KASTELLAN_LLM_GUARD_MODEL=shieldstral \
./target/debug/kastellan-cli guard calibrate --corpus tests/guard/corpus-materialised
```

Server: `llama-server -m ~/models/shieldstral/upstream/Shieldstral-1.0-3B-Q8_0.gguf
--alias shieldstral --port 8081 --host 127.0.0.1 -c 32768 -ngl 99 --no-webui`
(sha256 `35b755be…`, the upstream-verified build — see #592).

**Step 3 now verifies the weights before it scores anything.** It GETs `/props`,
takes `model_path`, hashes that file, and refuses on a mismatch — or on an
unreadable path, an absent `model_path`, a **relative** `model_path`, or an
unreachable `/props`. Each says which of the five fired. Two consequences worth
knowing before you hit them, plus one server requirement:

* **start `llama-server` with an ABSOLUTE `-m` path.** A relative one is
  reported verbatim by `/props` and would resolve against the *calibration
  tool's* working directory, not the server's — so a copy of the pinned file
  sitting at the same relative path under your cwd would hash as pinned while
  the server served something else. That is #592's own shape, so it is refused
  rather than resolved;

* **the calibration must run on the host serving the model**, because the check
  hashes a local file. Pointing it at a remote `llama-server` refuses with
  *"share a filesystem"* rather than silently trusting the endpoint;
* **`--weights-unpinned`** proceeds anyway, for calibrating a *candidate* guard
  model. It accepts the answer; it never skips the hashing **where there is a
  file to hash** — on a mismatch the report names the actual bytes and the path,
  so the run stays reproducible from its own artefact. Where `/props` could not
  be reached, named no path, named a relative one, or named a file this tool
  cannot read, **nothing is hashed** and the header says so
  (`<unverified: …> UNPINNED -- nothing was hashed`) rather than inventing a
  digest. Either way the stamp says the run cannot support the cross-host τ
  comparison. Do not use it for measurement 3 itself.

## What changed after the review of #593

The five-agent review found four fail-opens in the tooling this runbook drives. Two change
what an operator must do:

- **`--record` no longer skips verification.** It used to skip *every* hash check, so
  re-running it over a fully recorded manifest — which is the only way to record the ~85
  new entries — silently re-pinned any source that had drifted. An entry with a usable
  hash is now compared in both modes; a drifted one is refused, and re-pinning means
  deleting its `sha256` field deliberately. New entries print `RECORD-NEW`, existing ones
  `RECORD-SAME`.
- **A non-2xx fetch is refused.** Nothing checked the HTTP status, and nothing downstream
  could: `walk` emits string leaves only, so the status reached neither the stored text nor
  the sha256. A vanished Wayback snapshot's 404 page was hashed and pinned wearing the label
  and notes of the page it replaced. This is Open risk 2 biting in the fail-*open*
  direction, which the spec assumed it could not.

Also: a failed entry's stale file is discarded rather than left looking captured; files in
the out dir that no manifest entry wrote are reported as `ORPHAN` (a warning, because step 2
above puts them there deliberately); `guard capture` keeps the 30 s watchdog it was dropping;
and manifest sources are refused if they carry userinfo, an explicit port, a leading-dot
subdomain wildcard, or an IP literal in a denied range ([#594](https://github.com/hherb/kastellan/issues/594)
covers the hostname half, which needs the egress proxy).

## What measurement 3 still owes

- **~85 more manifest entries**: ~35 ordinary benign, ~15 security prose, **~35 captured
  attacks** (the pilot has none), ≥ 8 over the 64 KiB cap (the pilot's largest is 24 KB, so
  truncation is still unexercised).
- ~~The **Mac leg and the two-host comparison**~~ **DONE 2026-08-23.** #592's
  durable half shipped in #598, so both hosts now hash the file `/props` names
  and refuse anything but the pinned bytes. The two operating points differ by
  0.1% — see the measurement-3 runbook.
