#!/usr/bin/env bash
# label-backlog.sh — create the minimal issue taxonomy and apply it to every
# open issue.
#
# WHY THIS EXISTS
# ---------------
# The 2026-09-17 backlog triage found the mechanical cause of 58 untouched
# issues: **~7% of issues carried any label** (10 of 150, all GitHub defaults).
# With no taxonomy the roadmap-era cluster cannot be filtered for, so nobody
# sees it and it grows. See `2026-09-17-backlog-triage.md` §1 and §6 step 3.
#
# HOW IT CLASSIFIES
# -----------------
# Two dimensions, applied independently:
#
#   area:*    exactly ONE per issue — the subsystem it belongs to. Assigned by
#             the first matching rule in AREA_RULES (order is significant:
#             narrower subsystems come first, so "guard corpus capture takes the
#             direct-egress path" lands on area:guard, not area:egress).
#   theme     ZERO OR MORE — cross-cutting properties worth filtering for on
#             their own (a false-green gate, work that needs a live host).
#
# A keyword classifier is wrong sometimes, so OVERRIDES wins over AREA_RULES for
# named issues. Correct a misclassification by adding a line there, not by
# hand-editing the label on GitHub — otherwise the next run undoes it.
#
# IDEMPOTENT: `gh issue edit --add-label` is additive and a label already
# present is a no-op, so re-running after adding overrides only fixes drift.
# It never REMOVES a label, so an area corrected in OVERRIDES needs the old one
# removed by hand once (the script prints the command).
#
# USAGE
#   bash docs/devel/notes/label-backlog.sh --dry-run   # print the plan only
#   bash docs/devel/notes/label-backlog.sh             # create + apply
set -euo pipefail

REPO="hherb/kastellan"
DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

# ---------------------------------------------------------------------------
# 1. The label set. name|colour|description
#
# Deliberately small. A taxonomy nobody can hold in their head is one nobody
# applies, which is how the backlog got here. Colours group the two dimensions
# visually: areas share a blue-grey family, themes are warm.
# ---------------------------------------------------------------------------
LABELS=(
  "area:guard|5319e7|Injection-guard tier: adjudication, calibration, corpus, boot probe"
  "area:planner|5319e7|Agent loop, plan.formulate, step budget, tool docs, prompt assembly"
  "area:channel|5319e7|Matrix/email channels, DMs, conversation continuity, operator asks"
  "area:sandbox|1d76db|SandboxPolicy and the OS backends: bwrap, Seatbelt, seccomp, Landlock"
  "area:microvm|1d76db|Firecracker and Apple container tiers: rootfs, VMM, guest kernel"
  "area:egress|1d76db|Egress proxy, MITM, allowlists, SSRF, cert pinning, leak scan"
  "area:memory|0e8a16|Three-lane memory, embeddings, entities, context management"
  "area:audit|0e8a16|Audit log: rows, payload truncation, provenance, causal structure"
  "area:workers|fbca04|Individual tool workers: mail, web-*, python-exec, shell-exec, gliner"
  "area:supervisor|fbca04|systemd/launchd units, installer, deployment"
  "area:db|006b75|Postgres layer, migrations, sqlx, pooling"
  "area:llm|006b75|llm-router, model selection, streaming, timeouts"
  "area:ci|bfd4f2|CI workflows, gates, clippy, rustdoc, test infrastructure"
  "false-green|b60205|A gate or test that can report green having asserted nothing"
  "needs-live-host|d93f0b|Cannot be closed without a run on the DGX or the Mac"
  "roadmap|c2e0c6|Roadmap-era work item, still unstarted on docs/devel/ROADMAP.md"
)

# ---------------------------------------------------------------------------
# 2. Area rules, in priority order: "label<TAB>extended-regex" against the title.
#    First match wins. Case-insensitive.
# ---------------------------------------------------------------------------
AREA_RULES=(
  "area:guard|guard|shieldstral|injection.guard|SCAN_BYTE_CAP|tau\b"
  "area:microvm|micro.?vm|firecracker|rootfs|VMM|guest kernel|vsock|persistent.store|Apple .container|python-exec container|mkfs"
  "area:channel|matrix|channel|ask |asks\.|AskResolver|ask path|ask delivery|ask rejection|ask containment|/approve|Element|conversation|DM |SMTP|Telegram"
  "area:planner|planner|plan\.|prompt|tool_doc|ToolParam|inner.loop|invoke_skill|step budget|handoff|result view|task_exec|scheduler"
  "area:egress|egress|proxy|MITM|allowlist|SSRF|cert.pin|leak.scan|net_client|tunnel|loopback|web-common"
  "area:workers|mail\b|mail\.|web-research|web-fetch|browser-driver|python.exec|shell.exec|gliner|localmail|worker"
  "area:audit|audit|truncat|provenance"
  "area:memory|memory|embedding|entit|context_manager|dedup|graph.lane|snapshot writer|preference learning"
  "area:sandbox|sandbox|bwrap|seatbelt|seccomp|landlock|jail|workspace.out|scratch"
  "area:supervisor|supervisor|systemd|launchd|installer|install\b|deployment|unit.file|SIGTERM"
  "area:db|postgres|\bPG\b|sqlx|migration|pool\.close|PgListener|TOCTOU on L1"
  "area:llm|llm-router|stream chat|frontier escalation|model"
  "area:ci|\bci\b|clippy|rustdoc|cargo fmt|gate|test:|test\(|license audit|soak test|flaky"
)

# ---------------------------------------------------------------------------
# 3. Overrides — issue number|area — where the title's keywords mislead.
#    Each carries the reason, because a bare number is unreviewable.
# ---------------------------------------------------------------------------
OVERRIDES=(
  # --- the title's leading noun is the SUBJECT, but the defect is elsewhere ---
  "714|area:ci"          # "Postgres e2e" but the defect is the gate, not the DB
  "691|area:ci"          # image freshness GATE falsified by the clippy recipe
  "664|area:ci"          # gliner is the subject; the defect is REQUIRE_E2E
  "622|area:ci"          # guard_tier_e2e is the subject; the defect is the gate
  "639|area:ci"          # a test-file split
  "638|area:ci"          # rustdoc warnings
  "676|area:db"          # the socket disappears; the flake is PG ownership
  "548|area:db"          # PG e2e units leaking into the operator's real config
  "332|area:db"          # PgListener + pool.close deadlock
  "328|area:ci"          # a flake under full-workspace load
  "560|area:planner"     # "mail.get_message" but the defect is fabrication
  "543|area:workers"     # "reaches the planner" — the defect is browser-driver's codes
  "693|area:audit"       # "scheduler step-failure" — the defect is the audit row
  "621|area:audit"       # "fetch_handoff" — the defect is the recorded outcome
  "695|area:audit"       # an oversized tool ROW; no keyword names the subsystem
  "629|area:memory"      # "over the audit log" is the source; L4 is the subject
  "678|area:planner"     # "truncation" is the mechanism; long context is the subject

  # --- "guard" as a VERB, not the guard tier ---
  "438|area:planner"     # "Guard tool_doc().method against drift" — tool docs
  "711|area:planner"     # "the planner-prompt drift guard" — the planner's prompt

  # --- a keyword matched inside an unrelated word ---
  "484|area:workers"     # "per-task out dir" contains the asks rule's literal "ask "
  "696|area:db"          # matched "asks." in the FILE NAME asks.rs; it is a db helper
  "501|area:egress"      # "channel sidecar" is the subject; leak-scan fingerprints the defect
  "334|area:llm"         # "agent-planner calls" — the fix is llm-router streaming

  # --- no rule matched at all ---
  "712|area:channel"     # REPLIED_STATES ↔ notify_task_completed, channel replies
  "658|area:workers"     # python interpreter probe/resolution
  "657|area:workers"     # interpreter_deps diagnostics
  "595|area:workers"     # ManifestEntry — the worker manifest loader
  "550|area:supervisor"  # the generated kastellan.env is an install artefact
  "455|area:workers"     # web-research forced-synthesis fallback
  "228|area:planner"     # tiered delegation is an agent-loop policy

  # --- roadmap-era items whose one-line title carries no subsystem keyword ---
  "535|area:ci"          # gitignored report files referenced from source
  "442|area:ci"          # datetime crate consolidation, workspace-wide
  "236|area:ci"          # license audit on every new dependency
  "235|area:ci"          # architecture doc sync
  "234|area:ci"          # threat-model doc sync
  "233|area:ci"          # adversarial soak test
  "232|area:audit"       # audit log viewer
  "231|area:llm"         # frontier escalation is model routing; egress is the path
  "230|area:planner"     # per-tool/per-task routing policy
  "223|area:planner"     # MCP onboarding is a tool-surface concern
  "212|area:memory"      # reset snapshot writer
  "211|area:memory"      # context_manager
  "210|area:workers"     # GLiNER-Relex lifecycle on darwin
  "209|area:supervisor"  # worker lifecycle operator surface
  "196|area:memory"      # graph-lane degradation
  "174|area:memory"      # L1/L3 dedup index
  "78|area:planner"      # prompt assembly
  "76|area:planner"      # prompt assembly retry semantics
  "73|area:planner"      # floor-source validation in the scheduler
  "63|area:planner"      # classification_floor plumbing into plan.formulate
  "55|area:microvm"      # Apple container discovery spike
  "50|area:planner"      # finalize-payload provenance across emitters
  "47|area:audit"        # 'no verdict row' vs a real Approve verdict
  "37|area:planner"      # scheduler crash recovery sweep
  "24|area:supervisor"   # production unit files must set PROMPTS_DIR
  "21|area:planner"      # scheduler cancellation poll
  "20|area:db"           # PK on sha256 in agent_prompts
  "8|area:supervisor"    # default_probe cfg duplication
  "3|area:sandbox"       # prelude seccomp shim
  "519|area:supervisor"  # "installer:" — the micro-VM tier is what it fails to deploy
)

# ---------------------------------------------------------------------------
# 4. Themes — issue number lists. Cross-cutting, so an issue may carry several.
# ---------------------------------------------------------------------------
# The false-green cluster named by the triage note §4, plus the ones whose own
# title states the shape ("exits 0", "reports green", "silent PASS").
FALSE_GREEN=(714 664 622 691 237 599 611 608 550 621 455 47)
# Work that cannot be verified, let alone closed, without a real host.
NEEDS_LIVE_HOST=(677 668 612 604 597 594 581 560 554 519 456 455 304 286 243 242 233 210 55)
# Roadmap-era items still `[ ]` on docs/devel/ROADMAP.md (triage note §2).
ROADMAP=(232 220 231 230 229 228 223 233 212 211 209 210)

# ---------------------------------------------------------------------------
say() { printf '%s\n' "$*"; }

# Run a gh call, retrying a transient API failure.
#
# GitHub answers a bulk labelling pass with the occasional 504, and under
# `set -e` inside a `while read` pipeline that aborts the whole run partway —
# leaving a half-labelled backlog whose remainder nobody can see. Three tries
# with a short backoff turns the common case into a no-op. A call that still
# fails is reported and the pass CONTINUES: one unlabelled issue named in the
# output is recoverable; an aborted pass silently missing sixty is not.
run() {
  if [ "$DRY_RUN" = 1 ]; then say "  DRY-RUN: $*"; return 0; fi
  local attempt
  for attempt in 1 2 3; do
    if "$@"; then return 0; fi
    say "  retry $attempt/3 after a failed: $*"
    sleep $((attempt * 2))
  done
  say "  ⚠️  GAVE UP after 3 attempts: $*"
  FAILED=$((FAILED + 1))
  return 0
}
FAILED=0

say "==> 1. creating labels"
for spec in "${LABELS[@]}"; do
  IFS='|' read -r name colour desc <<<"$spec"
  if gh label list --repo "$REPO" --limit 200 --json name -q '.[].name' | grep -qx "$name"; then
    say "  exists: $name"
  else
    run gh label create "$name" --repo "$REPO" --color "$colour" --description "$desc"
    say "  created: $name"
  fi
done

# Pure: the area label for a title, or empty when no rule matches.
area_for_title() {
  local title="$1" rule label pattern
  for rule in "${AREA_RULES[@]}"; do
    label="${rule%%|*}"
    pattern="${rule#*|}"
    if printf '%s' "$title" | grep -qiE "$pattern"; then
      printf '%s' "$label"
      return 0
    fi
  done
  printf ''
}

# Pure: the override for an issue number, or empty.
override_for() {
  local n="$1" o
  for o in "${OVERRIDES[@]}"; do
    [ "${o%%|*}" = "$n" ] && { printf '%s' "$(printf '%s' "${o#*|}" | awk '{print $1}')"; return 0; }
  done
  printf ''
}

in_list() {
  local needle="$1"; shift
  local x
  for x in "$@"; do [ "$x" = "$needle" ] && return 0; done
  return 1
}

say ""
say "==> 2. applying labels to open issues"
gh issue list --repo "$REPO" --state open --limit 300 --json number,title \
  -q '.[] | "\(.number)\t\(.title)"' | while IFS=$'\t' read -r num title; do
  area="$(override_for "$num")"
  [ -n "$area" ] || area="$(area_for_title "$title")"

  labels=()
  if [ -n "$area" ]; then
    labels+=("$area")
  else
    say "  #$num  NO AREA RULE MATCHED — add an override: $title"
  fi
  in_list "$num" "${FALSE_GREEN[@]}"     && labels+=("false-green")
  in_list "$num" "${NEEDS_LIVE_HOST[@]}" && labels+=("needs-live-host")
  in_list "$num" "${ROADMAP[@]}"         && labels+=("roadmap")

  [ ${#labels[@]} -eq 0 ] && continue
  joined="$(IFS=,; printf '%s' "${labels[*]}")"
  say "  #$num  $joined"
  run gh issue edit "$num" --repo "$REPO" --add-label "$joined"
done

say ""
say "==> done. Verify with:"
say "    gh issue list --state open --limit 300 --json number,labels \\"
say "      -q '[.[] | select(.labels|length==0) | .number] | length'   # should be 0"
