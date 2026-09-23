#!/usr/bin/env bash
# run-e2e-gate.sh — run a gated e2e tier as EVIDENCE, not as a formality.
#
# WHY THIS EXISTS
# ---------------
# A REQUIRE knob (`KASTELLAN_*_REQUIRE_E2E`) turns a tier's `[SKIP]`s into
# failures, which closes half the false-green class. It cannot close the other
# half, because **the knob only fires from inside a test body**:
#
#     KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1 \
#       cargo test -p kastellan-core --test gliner_relex_e2e happy_pat   # typo
#     → 0 passed; 0 failed; 4 filtered out
#     → exit 0
#
# A green run, the knob set, no model loaded, and not one `[SKIP]` line —
# because no test body ran. Inferring "it ran" from the absence of a `[SKIP]`
# is unsound (issue #664). The same shape hides a test renamed out of
# collection, and a `--exact` filter with a bare test name, which matches
# nothing for a lib test and reported 9 mutants "surviving" against zero tests.
#
# THE CONTRACT
# ------------
#   Every gate needs a REQUIRE knob AND a positive control that fails when
#   zero tests ran.
#
# This script is the positive control. It sets the profile's knobs, keeps the
# WHOLE log (a truncated gate log is not a gate), and then asserts counts that
# a filtered-out run cannot satisfy:
#
#   1. a PER-TIER `[E2E]` floor. Emitted by `RequireKnob::announce` on the
#      SUCCESS path of a precondition, and only under a truthy knob — so each
#      one means "a demanded precondition was actually met here".
#   2. libtest's reported `N passed` >= the profile's MIN_PASSED, summed across
#      the run's suites. This is the one that catches the name-filter typo.
#   3. an optional per-profile cap on `[SKIP]` lines, and a hard zero on
#      `[WARN]` lines.
#   4. every test binary the run started reached the neutralising panic hook
#      (a `[panic-hook]` line inside its cargo `Running` section), and an
#      optional per-profile cap on `[panic]` lines (#748).
#
# ⚠️ **The `[E2E]` floors are per TIER, not per run, and that is load-bearing.**
# A single total is satisfied by whichever knob happens to be chattiest: the
# `gliner` profile also sets the Postgres knob, so one `[E2E] Postgres-backed:`
# line would clear a total floor of 1 while proving nothing whatever about the
# 1.3 GB model tier — #651's fixture with #714's evidence pasted over it. The
# tier name is already in the line (`[E2E] <tier>: <detail>`), so counting per
# tier costs one grep and makes each floor say what it means.
#
# ⚠️ **A per-tier floor of 1 is enough, and a bigger number is worse.** Deleting
# any single `announce` call drops that tier's count to 0 and trips its floor.
# A floor near the observed total (guard-tier emits 44) would be a second place
# the test census lives — it rots on the next added test, somebody lowers it,
# and the gate stops gating. The old single floor of 4 against an observed 44
# could not detect a lost `announce` at all, which is precisely what its own
# failure text offered to diagnose.
#
# USAGE
#   bash scripts/run-e2e-gate.sh <profile> [extra cargo args...]
#   bash scripts/run-e2e-gate.sh --list
#
# Extra arguments are appended to the cargo invocation, before the `--`.
set -uo pipefail

# ⚠️ Not merely a shebang formality. Under zsh — the login shell on both of this
# repo's hosts — arrays are 1-indexed, so `${PIPESTATUS[0]}` is EMPTY and the
# "did the test run itself fail?" check silently disappears. `set -u` does not
# catch it: the array exists, index 0 is just empty. Refuse rather than judge.
if [ -z "${BASH_VERSION:-}" ]; then
  echo "run-e2e-gate.sh: must run under bash (got ${SHELL:-an unknown shell})." >&2
  echo "  In zsh \${PIPESTATUS[0]} is empty, so the cargo exit code would go unchecked." >&2
  echo "  Run: bash scripts/run-e2e-gate.sh <profile>" >&2
  exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" || {
  echo "run-e2e-gate.sh: cannot resolve the repository root" >&2
  exit 2
}
cd "$REPO_ROOT" || { echo "run-e2e-gate.sh: cannot cd to $REPO_ROOT" >&2; exit 2; }

# shellcheck disable=SC1090,SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

# ---------------------------------------------------------------------------
# Preflight every external tool the script uses.
#
# macOS has no GNU `timeout`, and a classifying pipe swallows the
# "command not found" so the failure reads as the thing being measured. Check
# up front, by name, and say which one is missing. The list is every external
# this file invokes — if you add one, add it here; "every" is a claim the
# preflight either honours or should not make.
# ---------------------------------------------------------------------------
for tool in cargo grep awk tee date uname basename mkdir env sort sed; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "run-e2e-gate.sh: required tool not found on PATH: $tool" >&2
    exit 2
  }
done

# ---------------------------------------------------------------------------
# Profiles.
#   name | knobs | E2E floors | MIN_PASSED | MAX_SKIP | cargo args | harness args | os | MAX_PANIC
#
# E2E floors are `tier=N[,tier=N...]`, where `tier` is the phrase the knob was
# constructed with and the string that appears in `[E2E] <tier>: <detail>`:
#   supervisor-backed  Postgres-backed  sandboxed  guard-tier  micro-VM  gliner-relex
#
# MIN_PASSED is a LOWER BOUND, deliberately: an exact count is a second place
# the test census lives and it goes stale the next time a test is added, at
# which point somebody lowers it and the gate stops gating. A floor survives
# every addition untouched; the only thing that disturbs it is a suite
# SHRINKING, and that is itself worth noticing.
#
# ⚠️ A floor of 0 is not a gate. Every profile's MIN_PASSED must be >= 1 and
# every profile must name at least one E2E floor — enforced at startup by
# `validate_profiles`, not merely asserted here. (A rule nothing enforces is
# the exact shape this whole script argues against.)
#
# MAX_SKIP is `any` or a number. `guard-tier` is 0: every precondition in its
# `bootstrap()` is knob-routed, so a `[SKIP]` there is by definition a bypass.
# `worker-report` is 0 for the same reason: its one precondition is
# `skip_if_sandbox_unavailable`, and the other four suites' parents read no knob.
# The rest stay `any` while #718's 92 hand-written `[SKIP]` sites exist.
#
# ⚠️ Harness args are per profile because the Firecracker suites are `#[ignore]`:
# WITHOUT `--ignored` the whole tier reports green having booted no VM, every
# suite exit 0, and `KASTELLAN_MICROVM_REQUIRE_E2E=1` does NOT catch it — the
# knob fires from inside a test body and an ignored body never runs. That is the
# same hole as #664 on a different tier, which is why the floors below are what
# actually gate it.
#
# ⚠️ `@FIRECRACKER_SUITES` is resolved by GREP at run time, not hand-listed. A
# hand-written roster goes stale the day a suite is added, and the gate then
# reports green over a tier it never selected — a guard sharing its census's
# blind spot.
#
# ⚠️ The `os` field exists because **a whole test file can be
# `#![cfg(target_os = "linux")]`**, in which case the Mac compiles NOTHING in it
# and `cargo test --test <name>` yields an empty binary: `0 passed`, exit 0.
# Every Firecracker suite is exactly that. Without this field the microvm
# profile on the Mac fails its own floor with "0 tests passed", which reads as a
# broken gate rather than as "wrong host" — and a gate that cries wolf is a gate
# somebody stops running. `any` means the profile is host-agnostic.
#
# MAX_PANIC is `any` or a number, like MAX_SKIP, and counts `[panic]` lines —
# the neutralising hook's rendering of a panic (#742). A panicking TEST already
# fails the run; what this catches is a panic inside a PASSING one (a caught
# panic, a panic on a non-test thread). No suite any profile selects has a
# `#[should_panic]`, so 0 is reachable. ⚠️ **Only write a number you MEASURED
# on a real run of that profile** — an unmeasured 0 is a gate that cries wolf.
#
# ⚠️ **The cap has no floor and cannot have one.** Zero `[panic]` lines from a
# binary whose panics went through the DEFAULT hook looks exactly like zero
# from one that never panicked. The per-binary `[panic-hook]` check below is
# what separates the two — the cap is only sound because that check exists.
# ⚠️ And only for panics AFTER the install: a panic that beats its binary's
# first knob read (libtest runs tests in parallel) is still the default hook's,
# in a section that nonetheless reads as hooked. Neither check sees it (#757).
#
# `worker-report` (#748) demands the suites that prove a dying worker's last
# words reach a failing test. Only the sandbox knob: one suite needs a sandbox,
# the rest are hermetic, and none needs Postgres or a guard backend — so unlike
# `guard-tier` it runs on both hosts.
# ---------------------------------------------------------------------------
PROFILES=(
  "guard-tier|KASTELLAN_PG_REQUIRE_E2E=1 KASTELLAN_SANDBOX_REQUIRE_E2E=1 KASTELLAN_GUARD_REQUIRE_E2E=1|supervisor-backed=1,sandboxed=1,Postgres-backed=1,guard-tier=1|1|0|-p kastellan-core --test guard_tier_e2e|--nocapture|any|0"
  "pg|KASTELLAN_PG_REQUIRE_E2E=1|supervisor-backed=1,Postgres-backed=1|1|any|-p kastellan-core --test injection_guard_e2e --test secret_vault_e2e --test conversation_continuity_e2e|--nocapture|any|0"
  "gliner|KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1 KASTELLAN_PG_REQUIRE_E2E=1|gliner-relex=1|1|any|-p kastellan-core --test gliner_relex_e2e|--nocapture|any|0"
  "microvm|KASTELLAN_MICROVM_REQUIRE_E2E=1 KASTELLAN_PG_REQUIRE_E2E=1 KASTELLAN_SANDBOX_REQUIRE_E2E=1|micro-VM=1|1|any|-p kastellan-core @FIRECRACKER_SUITES|--nocapture --ignored|Linux|0"
  "worker-report|KASTELLAN_SANDBOX_REQUIRE_E2E=1|sandboxed=1|1|0|-p kastellan-core -p kastellan-tests-common --test worker_early_exit_stderr_fallback_e2e --test persistent_worker_death_stderr_fallback_e2e --test panic_hook_gate_safety_e2e --test worker_report_broken_stderr_e2e --test panic_hook_broken_stderr_e2e|--nocapture|any|0"
)

# ⚠️ There is deliberately NO `sandbox` profile yet, and the reason is a
# measurement rather than an oversight. Running one against
# `-p kastellan-sandbox --all-targets` gives:
#
#     [E2E]  lines : 0   (floor 1)
#     tests passed : 147
#     [SKIP] lines : 10
#
# `kastellan-sandbox`'s own integration tests do not depend on
# `kastellan-tests-common`, so they cannot see `skip_if_sandbox_unavailable`
# and their ten `[SKIP]`s are hand-written — bypassing `skip_line` AND every
# knob. That is issue #718 (92 such sites in 47 files), and it means the
# sandbox tier genuinely CANNOT be gated yet. Shipping a profile that is red on
# every host would train everyone to ignore a red gate, which is the failure
# #682's design notes name outright. Add the profile in the PR that gives
# `kastellan-sandbox` a knob vocabulary.
#
# `KASTELLAN_SANDBOX_REQUIRE_E2E` itself IS live — the `guard-tier` profile
# sets it, and `skip_if_sandbox_unavailable` in `tests-common` honours it for
# every suite that goes through that helper.
#
# ⚠️ There is also no `container` profile, for the macOS Apple-`container`
# tier — the tier of #684, one of the four false greens this contract cites.
# Its three suites gate through `skip_if_no_container` / `skip_if_image_missing`,
# which have no knob and no `announce`, so a profile would be red on every Mac
# for the same reason the `sandbox` one would. Tracked separately; adding it is
# the acceptance test for whichever PR gives those two helpers a knob.

# Every LINUX integration suite that gates on the shared micro-VM preflight, by
# reading the sources rather than by remembering them.
#
# ⚠️ The cfg filter is not cosmetic. Three suites that use this module —
# `python_exec_container_e2e`, `lifecycle_container_routing_e2e`,
# `python_exec_warm_idle_e2e` — are `#![cfg(target_os = "macos")]`, so on the
# Linux host this profile demands they compile to EMPTY test binaries: 0 passed,
# exit 0. That is the very shape the `os` field above exists to refuse, and
# selecting them here would have smuggled it back in through the discovery.
firecracker_suites() {
  local f names=""
  for f in core/tests/*.rs; do
    grep -q 'kastellan_tests_common::microvm' "$f" || continue
    grep -q '^#!\[cfg(target_os = "macos")\]' "$f" && continue
    names+="--test $(basename "$f" .rs) "
  done
  printf '%s' "$names"
}

# How many `--test <name>` targets a cargo argument string names.
#
# ⚠️ Counts TOKENS, not lines. It was `grep -c -- '--test'`, which counts LINES,
# and `firecracker_suites` emits every target on ONE line — so it read 1 on
# every host, under the floor of 12, and the `microvm` profile refused every run
# from #720 until #748 found it. Never reached on the Mac (the `os` check
# refuses first). `gate_script_tests` now runs this against the real discovery.
count_test_targets() {
  local n=0 tok
  # Unquoted on purpose: word-splitting IS the tokeniser. Target names are
  # file stems, so they contain no whitespace or glob characters.
  for tok in $1; do
    [ "$tok" = "--test" ] && n=$((n + 1))
  done
  printf '%s' "$n"
}

# The floor below which a shrinking discovery is a bug rather than a deletion.
#
# `firecracker_suites` refusing ZERO is not enough: a grep rule that matched 1
# of 15 would select one suite, clear MIN_PASSED=1, and report ✅ over a tier it
# 93% did not run — a guard sharing its census's blind spot, one step milder.
# Raise this deliberately when suites are added; a drop means the rule broke.
MIN_FIRECRACKER_SUITES=12

# ---------------------------------------------------------------------------
# Profile hygiene, enforced rather than asserted in a comment.
#
# `IFS='|' read -r a b c <<<"x|y"` succeeds with c EMPTY, and `set -u` does not
# help because the variable IS set. So a profile line that loses a field to a
# stray `|` silently removes a floor — and an empty operand in `[ "$x" -lt N ]`
# is a test that ERRORS (status 2) and is therefore read as "floor met".
# A malformed profile must be a usage error, not a quiet ✅.
# ---------------------------------------------------------------------------
validate_profiles() {
  local spec name knobs e2e_floors min_passed max_skip cargo_args harness_args os max_panic extra
  local bad=0 pair tier floor
  for spec in "${PROFILES[@]}"; do
    IFS='|' read -r name knobs e2e_floors min_passed max_skip cargo_args harness_args os max_panic extra \
      <<<"$spec"
    if [ -z "$name" ] || [ -n "$extra" ]; then
      echo "run-e2e-gate.sh: profile '${name:-<unnamed>}' does not have exactly 9 fields" >&2
      bad=1
      continue
    fi
    for field in knobs e2e_floors min_passed max_skip cargo_args harness_args os max_panic; do
      if [ -z "${!field}" ]; then
        echo "run-e2e-gate.sh: profile '$name' has an empty '$field' field" >&2
        bad=1
      fi
    done
    case "$min_passed" in
      ''|*[!0-9]*) echo "run-e2e-gate.sh: profile '$name' MIN_PASSED is not a number" >&2; bad=1 ;;
      *) [ "$min_passed" -ge 1 ] || {
           echo "run-e2e-gate.sh: profile '$name' MIN_PASSED must be >= 1 (a floor of 0 is not a gate)" >&2
           bad=1
         } ;;
    esac
    case "$max_skip" in
      any) ;;
      '') echo "run-e2e-gate.sh: profile '$name' MAX_SKIP is empty (write 'any' for no cap)" >&2; bad=1 ;;
      *[!0-9]*) echo "run-e2e-gate.sh: profile '$name' MAX_SKIP must be a number or 'any'" >&2; bad=1 ;;
    esac
    case "$max_panic" in
      any) ;;
      '') ;;  # already reported as an empty field above
      *[!0-9]*) echo "run-e2e-gate.sh: profile '$name' MAX_PANIC must be a number or 'any'" >&2; bad=1 ;;
    esac
    [ -n "$e2e_floors" ] || { echo "run-e2e-gate.sh: profile '$name' names no E2E floor" >&2; bad=1; }
    for pair in ${e2e_floors//,/ }; do
      tier="${pair%%=*}"
      floor="${pair#*=}"
      if [ -z "$tier" ] || [ "$tier" = "$pair" ]; then
        echo "run-e2e-gate.sh: profile '$name' E2E floor '$pair' is not tier=N" >&2
        bad=1
        continue
      fi
      case "$floor" in
        ''|*[!0-9]*) echo "run-e2e-gate.sh: profile '$name' floor for '$tier' is not a number" >&2; bad=1 ;;
        *) [ "$floor" -ge 1 ] || {
             echo "run-e2e-gate.sh: profile '$name' floor for '$tier' must be >= 1" >&2
             bad=1
           } ;;
      esac
    done
    case "$os" in
      any|Linux|Darwin) ;;
      *) echo "run-e2e-gate.sh: profile '$name' has an unknown os '$os'" >&2; bad=1 ;;
    esac
  done
  [ "$bad" -eq 0 ] || { echo "run-e2e-gate.sh: refusing to run with a malformed profile table." >&2; exit 2; }
}
validate_profiles

list_profiles() {
  echo "profiles:"
  local spec name knobs os
  for spec in "${PROFILES[@]}"; do
    # The trailing `_` is load-bearing: `read` puts the REST of the line in its
    # last variable, so without it `os` would read as `any|<MAX_PANIC>`.
    IFS='|' read -r name knobs _ _ _ _ _ os _ <<<"$spec"
    printf '  %-14s [%-6s] %s\n' "$name" "$os" "$knobs"
  done
}

[ $# -ge 1 ] || { echo "usage: $0 <profile> [extra cargo args...]" >&2; list_profiles >&2; exit 2; }
[ "$1" = "--list" ] && { list_profiles; exit 0; }

WANT="$1"; shift

FOUND=""
for spec in "${PROFILES[@]}"; do
  IFS='|' read -r name knobs e2e_floors min_passed max_skip cargo_args harness_args os max_panic \
    <<<"$spec"
  if [ "$name" = "$WANT" ]; then FOUND=1; break; fi
done
[ -n "$FOUND" ] || { echo "run-e2e-gate.sh: unknown profile: $WANT" >&2; list_profiles >&2; exit 2; }

# Refuse a wrong-host run OUTRIGHT rather than let it fail a floor. Exit 2
# (usage), never 1 (gate failed): the difference is what stops somebody reading
# "0 tests passed" on a Mac as a broken gate and switching it off.
HOST_OS="$(uname -s)"
if [ "$os" != "any" ] && [ "$os" != "$HOST_OS" ]; then
  # Mapped, not lowercased: `Darwin` lowercases to `darwin`, which is not a
  # `target_os` value, so the hint would name a cfg that can never match.
  case "$os" in
    Linux) target_os="linux" ;;
    Darwin) target_os="macos" ;;
    *) target_os="$os" ;;
  esac
  echo "run-e2e-gate.sh: profile '$WANT' requires $os, this host is $HOST_OS." >&2
  echo "  Its suites are #![cfg(target_os = \"$target_os\")], so here they would" >&2
  echo "  compile to empty test binaries and report 0 passed at exit 0 — which is the" >&2
  echo "  false green this script exists to refuse. Run it on a $os host." >&2
  exit 2
fi

if [[ "$cargo_args" == *"@FIRECRACKER_SUITES"* ]]; then
  resolved="$(firecracker_suites)"
  discovered="$(count_test_targets "$resolved")"
  if [ "$discovered" -lt "$MIN_FIRECRACKER_SUITES" ]; then
    echo "run-e2e-gate.sh: discovered $discovered micro-VM suites, floor is $MIN_FIRECRACKER_SUITES." >&2
    echo "  Either the grep rule is stale, this is not the workspace root, or suites" >&2
    echo "  were deleted. Refusing to report a gate over a tier it mostly did not select." >&2
    exit 2
  fi
  cargo_args="${cargo_args/@FIRECRACKER_SUITES/$resolved}"
fi

# ---------------------------------------------------------------------------
# The log lives under $HOME, WHOLE.
#
# Not /tmp: it is scrubbed mid-run on both the DGX and the Mac, and a gate
# whose log vanished is a gate nobody can audit. Not piped through `tail`
# either — `cargo test --workspace | tail -400` once hid the failing suite AND
# made a ~3800-test run report "230 passed" at exit 101.
#
# ⚠️ Both steps are CHECKED, and that is not defensive habit. Every verdict
# below is an arithmetic `[ "$x" -lt N ]`; when the log cannot be read, the
# counts come back as the EMPTY STRING (grep exits 2 printing nothing, and the
# `|| true` that correctly preserves the zero-match case preserves this too),
# and `[ "" -lt 4 ]` exits 2 — which an `if` reads as FALSE. Every assertion
# then vanishes and the script prints ✅ at exit 0, having measured nothing.
# `tee` does not fail the pipeline either: it reports its own error, still
# relays stdout, and cargo exits 0 — so nothing downstream would notice.
# ---------------------------------------------------------------------------
LOG_DIR="$HOME/.local/state/kastellan/gate-logs"
mkdir -p "$LOG_DIR" || { echo "run-e2e-gate.sh: cannot create $LOG_DIR" >&2; exit 2; }
LOG="$LOG_DIR/e2e-gate-${WANT}-$(date +%Y%m%d-%H%M%S).log"
: > "$LOG" || { echo "run-e2e-gate.sh: cannot write $LOG" >&2; exit 2; }

echo "==> profile : $WANT"
echo "==> knobs   : $knobs"
echo "==> cargo   : cargo test $cargo_args $* -- $harness_args"
echo "==> log     : $LOG"
echo ""

# `env` rather than `export`, so the knobs apply to this run and nothing else —
# a knob left exported into a later plain `cargo test` would turn every honest
# skip on the host into a failure and look like a regression.
# shellcheck disable=SC2086
env $knobs cargo test $cargo_args "$@" -- $harness_args 2>&1 | tee "$LOG"
# ⚠️ Copy the WHOLE array in one command, on the line straight after the
# pipeline. An assignment is itself a command, so `TEST_EXIT="${PIPESTATUS[0]}"`
# resets PIPESTATUS to that assignment's own single status, and a following
# `${PIPESTATUS[1]}` is then unset. Under `set -u` that killed the script with
# "unbound variable" after every run: exit 1 on every profile, whatever the tests
# did (#719). `tests-common/src/gate_script_tests/run.rs` runs this script and
# fails if it happens again.
PIPE_EXITS=("${PIPESTATUS[@]}")
TEST_EXIT="${PIPE_EXITS[0]:-}"
TEE_EXIT="${PIPE_EXITS[1]:-}"

if [ "${TEE_EXIT:-1}" -ne 0 ]; then
  echo "run-e2e-gate.sh: tee failed writing $LOG — the run happened but was not recorded," >&2
  echo "  so there is nothing to assert over. Refusing to report a verdict." >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# The assertions.
#
# `grep -c` exits 1 on zero matches while printing `0`, so every count is
# `|| true`-guarded: the zero case is precisely the one this script exists to
# REPORT, not to abort on. `${x:-0}` then covers grep's OTHER non-zero exit —
# an unreadable file, which prints nothing at all.
#
# ⚠️ That default makes an unreadable log read as zero `[SKIP]`/`[WARN]`/
# `[panic]` lines, which every CAP accepts; it is the floors that go red on it
# (0 passed). And before any verdict, the UNHOOKED awk below reads the same
# file with its exit status CHECKED, so an unreadable log is refused outright
# rather than judged — which is also why the not-a-count loop further down can
# never see an empty count.
# ---------------------------------------------------------------------------
count_e2e_for_tier() {
  local tier="$1" n
  n="$(grep -c "^\[E2E\] ${tier}: " "$LOG" || true)"
  printf '%s' "${n:-0}"
}

E2E_TOTAL="$(grep -c '^\[E2E\]' "$LOG" || true)"; E2E_TOTAL="${E2E_TOTAL:-0}"
SKIP_COUNT="$(grep -c '^\[SKIP\]' "$LOG" || true)"; SKIP_COUNT="${SKIP_COUNT:-0}"
WARN_COUNT="$(grep -c '^\[WARN\]' "$LOG" || true)"; WARN_COUNT="${WARN_COUNT:-0}"
PASSED_COUNT="$(awk '/^test result:/ { for (i = 1; i <= NF; i++) if ($i == "passed;") s += $(i-1) } END { print s + 0 }' "$LOG" || true)"
PASSED_COUNT="${PASSED_COUNT:-0}"
PANIC_COUNT="$(grep -c '^\[panic\]' "$LOG" || true)"; PANIC_COUNT="${PANIC_COUNT:-0}"
BINARY_COUNT="$(grep -cE '^[[:space:]]+Running ' "$LOG" || true)"; BINARY_COUNT="${BINARY_COUNT:-0}"

# Every test binary must have reached the neutralising panic hook (#748).
#
# Cargo prints `     Running tests/<suite>.rs (…)` before it starts each test
# binary, and the binary's own output follows on the same stream, so the log
# splits cleanly into one section per binary. A binary that read a REQUIRE knob
# (or called `panic_hook::install_once` itself) prints exactly one
# `[panic-hook]` line in its section. A section without one is a binary that
# got neither the REQUIRE semantics nor the hook — its panics went through the
# DEFAULT hook, unneutralised and invisible to the `[panic]` cap.
#
# Measured at run time rather than read off the source, because suites reach
# the knob through a dozen indirect helpers and a list of their names would be
# a census that rots.
#
# ⚠️ The awk exit is CHECKED: a failed awk prints nothing, which reads as "every
# binary was hooked" — the empty-operand false green in another costume.
UNHOOKED="$(awk '
  /^[[:space:]]+Running / { if (cur != "" && !hooked) print cur; cur = $0; hooked = 0; next }
  /^\[panic-hook\]/ { hooked = 1 }
  END { if (cur != "" && !hooked) print cur }
' "$LOG")"
AWK_EXIT=$?
if [ "$AWK_EXIT" -ne 0 ]; then
  echo "run-e2e-gate.sh: awk failed ($AWK_EXIT) splitting $LOG by binary — refusing a verdict." >&2
  exit 3
fi

# Belt and braces: an operand that is somehow still not a number must refuse a
# verdict rather than silently satisfy every floor.
for var in TEST_EXIT E2E_TOTAL SKIP_COUNT WARN_COUNT PASSED_COUNT PANIC_COUNT BINARY_COUNT; do
  case "${!var}" in
    ''|*[!0-9]*)
      echo "run-e2e-gate.sh: $var is \"${!var}\", which is not a count — refusing a verdict." >&2
      exit 3
      ;;
  esac
done

echo ""
echo "==> evidence"
echo "    tests passed : $PASSED_COUNT   (floor $min_passed)"
echo "    [E2E]  lines : $E2E_TOTAL   (per-tier floors: $e2e_floors)"
echo "    [SKIP] lines : $SKIP_COUNT   (max $max_skip)"
echo "    [WARN] lines : $WARN_COUNT   (max 0)"
echo "    [panic] lines: $PANIC_COUNT   (max $max_panic)"
echo "    test binaries: $BINARY_COUNT"
echo "    cargo exit   : $TEST_EXIT"

FAIL=0
if [ "$TEST_EXIT" -ne 0 ]; then
  echo "❌ the test run itself failed (exit $TEST_EXIT) — read $LOG"
  FAIL=1
fi
if [ "$PASSED_COUNT" -lt "$min_passed" ]; then
  echo "❌ POSITIVE CONTROL FAILED: $PASSED_COUNT tests passed, floor is $min_passed."
  echo "   A run that selected zero tests exits 0. Check the --test names and any"
  echo "   name filter for a typo, and that the tests were not renamed out of"
  echo "   collection. This is issue #664's shape."
  FAIL=1
fi
for pair in ${e2e_floors//,/ }; do
  tier="${pair%%=*}"
  floor="${pair#*=}"
  got="$(count_e2e_for_tier "$tier")"
  echo "    [E2E] $tier : $got   (floor $floor)"
  if [ "$got" -lt "$floor" ]; then
    echo "❌ POSITIVE CONTROL FAILED: $got '[E2E] $tier' lines, floor is $floor."
    echo "   The knobs were set but that tier never reported a demanded precondition"
    echo "   met. Either it did not reach its fixtures, or a precondition helper lost"
    echo "   its RequireKnob::announce call."
    FAIL=1
  fi
done
if [ "$max_skip" != "any" ] && [ "$SKIP_COUNT" -gt "$max_skip" ]; then
  echo "❌ $SKIP_COUNT [SKIP] line(s) under a demanded run, max is $max_skip."
  echo "   Each is a precondition that bypassed every knob — the shape issue #622"
  echo "   was filed about."
  grep '^\[SKIP\]' "$LOG" | sort -u | sed 's/^/     /'
  FAIL=1
elif [ "$SKIP_COUNT" -ne 0 ]; then
  echo "⚠️  $SKIP_COUNT [SKIP] line(s) under a demanded run. Each is a precondition"
  echo "   that bypassed every knob (issue #718's 92 hand-written sites). Not fatal"
  echo "   for this profile yet — see MAX_SKIP in the profile table."
  grep '^\[SKIP\]' "$LOG" | sort -u | sed 's/^/     /'
fi
if [ "$WARN_COUNT" -ne 0 ]; then
  echo "❌ $WARN_COUNT [WARN] line(s): a knob was set but NOT honoured, so this run"
  echo "   was not the demanded one. An out-of-dialect value (=y, =2, =enabled)"
  echo "   reverts to skip; the gate must not pass on a disarmed knob."
  grep '^\[WARN\]' "$LOG" | sort -u | sed 's/^/     /'
  FAIL=1
fi

# Tests passed but not one `Running` header: there is nothing to attribute a
# hook announcement to, so "no binary lacked the hook" would be vacuously true.
if [ "$PASSED_COUNT" -gt 0 ] && [ "$BINARY_COUNT" -eq 0 ]; then
  echo "❌ $PASSED_COUNT tests passed but the log has no \`Running\` lines, so the"
  echo "   per-binary panic-hook check has nothing to check. Cargo's output format"
  echo "   changed, or the log is not a cargo log. Refusing rather than passing vacuously."
  FAIL=1
fi
if [ -n "$UNHOOKED" ]; then
  echo "❌ test binaries that never reached the panic hook (no [panic-hook] line in"
  echo "   their section). Each got neither its REQUIRE knob nor the neutralising"
  echo "   hook: read a knob through tests_common, or call panic_hook::install_once()"
  echo "   first thing in every test of a hermetic suite."
  printf '%s\n' "$UNHOOKED" | sed 's/^[[:space:]]*/     /'
  FAIL=1
fi
if [ "$max_panic" != "any" ] && [ "$PANIC_COUNT" -gt "$max_panic" ]; then
  echo "❌ $PANIC_COUNT [panic] line(s), max is $max_panic. A panic inside a test that"
  echo "   still passed — caught, or on a non-test thread. Read each one."
  grep '^\[panic\]' "$LOG" | sort -u | sed 's/^/     /'
  FAIL=1
elif [ "$PANIC_COUNT" -ne 0 ]; then
  echo "⚠️  $PANIC_COUNT [panic] line(s) (max $max_panic):"
  grep '^\[panic\]' "$LOG" | sort -u | sed 's/^/     /'
fi

if [ "$FAIL" -eq 0 ]; then
  echo "✅ gate passed as evidence: $PASSED_COUNT tests ran with $E2E_TOTAL demanded preconditions met."
fi
echo "    full log: $LOG"
exit "$FAIL"
