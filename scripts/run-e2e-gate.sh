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
# WHOLE log (a truncated gate log is not a gate), and then asserts two counts
# that a filtered-out run cannot satisfy:
#
#   1. `[E2E]` lines >= the profile's MIN_E2E. Emitted by
#      `RequireKnob::announce` on the SUCCESS path of a precondition, and only
#      under a truthy knob — so each one means "a demanded precondition was
#      actually met here".
#   2. libtest's reported `N passed` >= the profile's MIN_PASSED, summed across
#      the run's suites. This is the one that catches the name-filter typo.
#
# USAGE
#   bash scripts/run-e2e-gate.sh <profile> [extra cargo args...]
#   bash scripts/run-e2e-gate.sh --list
#
# Extra arguments are appended to the cargo invocation, before the `--`.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# shellcheck disable=SC1090,SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

# ---------------------------------------------------------------------------
# Preflight every external tool the script uses.
#
# macOS has no GNU `timeout`, and a classifying pipe swallows the
# "command not found" so the failure reads as the thing being measured. Check
# up front, by name, and say which one is missing.
# ---------------------------------------------------------------------------
for tool in cargo grep awk tee date; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "run-e2e-gate.sh: required tool not found on PATH: $tool" >&2
    exit 2
  }
done

# ---------------------------------------------------------------------------
# Profiles.
#   name | knobs (space-separated VAR=VAL) | MIN_E2E | MIN_PASSED | cargo args | harness args | os
#
# MIN_E2E and MIN_PASSED are LOWER BOUNDS, not exact counts, deliberately:
# an exact count is a second place the test census lives and it goes stale on
# the next added test, at which point somebody lowers it and the gate stops
# gating. A floor only ever needs raising, and only when a suite shrinks —
# which is itself worth noticing.
#
# ⚠️ A floor of 0 is not a gate. Every profile's MIN_PASSED must be >= 1.
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
# ---------------------------------------------------------------------------
PROFILES=(
  "guard-tier|KASTELLAN_PG_REQUIRE_E2E=1 KASTELLAN_SANDBOX_REQUIRE_E2E=1 KASTELLAN_GUARD_REQUIRE_E2E=1|4|1|-p kastellan-core --test guard_tier_e2e|--nocapture|any"
  "pg|KASTELLAN_PG_REQUIRE_E2E=1|2|1|-p kastellan-core --test injection_guard_e2e --test secret_vault_e2e --test conversation_continuity_e2e|--nocapture|any"
  "gliner|KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1 KASTELLAN_PG_REQUIRE_E2E=1|1|1|-p kastellan-core --test gliner_relex_e2e|--nocapture|any"
  "microvm|KASTELLAN_MICROVM_REQUIRE_E2E=1 KASTELLAN_PG_REQUIRE_E2E=1 KASTELLAN_SANDBOX_REQUIRE_E2E=1|1|1|-p kastellan-core @FIRECRACKER_SUITES|--nocapture --ignored|Linux"
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

# Every integration suite that gates on the shared micro-VM preflight, by
# reading the sources rather than by remembering them.
firecracker_suites() {
  local f names=""
  for f in core/tests/*.rs; do
    if grep -q 'kastellan_tests_common::microvm' "$f"; then
      names+="--test $(basename "$f" .rs) "
    fi
  done
  printf '%s' "$names"
}

list_profiles() {
  echo "profiles:"
  local spec name knobs
  for spec in "${PROFILES[@]}"; do
    IFS='|' read -r name knobs _ _ _ _ os <<<"$spec"
    printf '  %-12s [%-5s] %s\n' "$name" "$os" "$knobs"
  done
}

[ $# -ge 1 ] || { echo "usage: $0 <profile> [extra cargo args...]" >&2; list_profiles >&2; exit 2; }
[ "$1" = "--list" ] && { list_profiles; exit 0; }

WANT="$1"; shift

FOUND=""
for spec in "${PROFILES[@]}"; do
  IFS='|' read -r name knobs min_e2e min_passed cargo_args harness_args os <<<"$spec"
  if [ "$name" = "$WANT" ]; then FOUND=1; break; fi
done
[ -n "$FOUND" ] || { echo "run-e2e-gate.sh: unknown profile: $WANT" >&2; list_profiles >&2; exit 2; }

# Refuse a wrong-host run OUTRIGHT rather than let it fail a floor. Exit 2
# (usage), never 1 (gate failed): the difference is what stops somebody reading
# "0 tests passed" on a Mac as a broken gate and switching it off.
HOST_OS="$(uname -s)"
if [ "$os" != "any" ] && [ "$os" != "$HOST_OS" ]; then
  echo "run-e2e-gate.sh: profile '$WANT' requires $os, this host is $HOST_OS." >&2
  echo "  Its suites are #![cfg(target_os = \"$(echo "$os" | tr '[:upper:]' '[:lower:]')\")], so here they would" >&2
  echo "  compile to empty test binaries and report 0 passed at exit 0 — which is the" >&2
  echo "  false green this script exists to refuse. Run it on a $os host." >&2
  exit 2
fi

if [[ "$cargo_args" == *"@FIRECRACKER_SUITES"* ]]; then
  resolved="$(firecracker_suites)"
  [ -n "$resolved" ] || {
    echo "run-e2e-gate.sh: discovered ZERO micro-VM suites — the grep rule is stale," >&2
    echo "  or this is not the workspace root. Refusing to report a gate over nothing." >&2
    exit 2
  }
  cargo_args="${cargo_args/@FIRECRACKER_SUITES/$resolved}"
fi

# ---------------------------------------------------------------------------
# The log lives under $HOME, WHOLE.
#
# Not /tmp: it is scrubbed mid-run on both the DGX and the Mac, and a gate
# whose log vanished is a gate nobody can audit. Not piped through `tail`
# either — `cargo test --workspace | tail -400` once hid the failing suite AND
# made a ~3800-test run report "230 passed" at exit 101.
# ---------------------------------------------------------------------------
LOG_DIR="$HOME/.local/state/kastellan/gate-logs"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/e2e-gate-${WANT}-$(date +%Y%m%d-%H%M%S).log"

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
TEST_EXIT="${PIPESTATUS[0]}"

# ---------------------------------------------------------------------------
# The assertions.
#
# `grep -c` exits 1 on zero matches, which under `set -e` would abort before
# the diagnosis is printed — so every count is `|| true`-guarded and the
# verdict is computed from the numbers, not from grep's exit status. The zero
# case is precisely the one this script exists to REPORT.
# ---------------------------------------------------------------------------
E2E_COUNT="$(grep -c '^\[E2E\]' "$LOG" || true)"
SKIP_COUNT="$(grep -c '^\[SKIP\]' "$LOG" || true)"
WARN_COUNT="$(grep -c '^\[WARN\]' "$LOG" || true)"
PASSED_COUNT="$(awk '/^test result:/ { for (i = 1; i <= NF; i++) if ($i == "passed;") s += $(i-1) } END { print s + 0 }' "$LOG")"

echo ""
echo "==> evidence"
echo "    [E2E]  lines : $E2E_COUNT   (floor $min_e2e)"
echo "    tests passed : $PASSED_COUNT   (floor $min_passed)"
echo "    [SKIP] lines : $SKIP_COUNT"
echo "    [WARN] lines : $WARN_COUNT"
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
if [ "$E2E_COUNT" -lt "$min_e2e" ]; then
  echo "❌ POSITIVE CONTROL FAILED: $E2E_COUNT [E2E] lines, floor is $min_e2e."
  echo "   The knobs were set but that many demanded preconditions were never"
  echo "   reported met. Either the tier did not reach its fixtures, or a"
  echo "   precondition helper lost its RequireKnob::announce call."
  FAIL=1
fi
if [ "$SKIP_COUNT" -ne 0 ]; then
  echo "⚠️  $SKIP_COUNT [SKIP] line(s) under a demanded run. Each is a precondition"
  echo "   that bypassed every knob — the shape issue #622 was filed about."
  grep '^\[SKIP\]' "$LOG" | sort -u | sed 's/^/     /'
fi

if [ "$FAIL" -eq 0 ]; then
  echo "✅ gate passed as evidence: $PASSED_COUNT tests ran with $E2E_COUNT demanded preconditions met."
fi
echo "    full log: $LOG"
exit "$FAIL"
