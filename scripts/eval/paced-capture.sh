#!/usr/bin/env bash
#
# Materialise the guard calibration corpus ONE ENTRY AT A TIME, pausing
# between entries.
#
# WHY THIS EXISTS, and why a bare `kastellan-cli guard capture` over the
# whole manifest is not enough:
#
#   `guard capture` fires its fetches back to back. web.archive.org
#   throttles that. A throttled response is NOT a clean failure -- it
#   arrives as a transport error (reported as FETCH-FAILED), or, worse,
#   as an HTTP 200 with an empty or truncated body, which under --record
#   would be hashed and pinned AS THE CASE (issue #602).
#
#   Measured on the measurement-3 campaign, 2026-08-23:
#     104 entries, back to back  -> 20 FETCH-FAILED, 1 spurious drift
#     the same 21, one at a time -> 0 failures
#
#   So the pause is not politeness, it is what makes the run's failures
#   mean what they say. A FETCH-FAILED from a paced run is worth
#   investigating; one from an unpaced run is usually just the throttle.
#
# Usage:
#   scripts/eval/paced-capture.sh <manifest-dir> <out-dir> [--record] [pause-seconds]
#
# Without --record every entry must print OK: the source still yields the
# bytes whose hash the manifest pins. With --record, new entries print
# RECORD-NEW and already-pinned ones RECORD-SAME -- and are still
# verified, because --record is not a way to skip the check.
#
# Exits non-zero if any entry did not succeed.

set -uo pipefail

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <manifest-dir> <out-dir> [--record] [pause-seconds]" >&2
  exit 2
fi

MANIFEST="$1"
OUT="$2"
shift 2

RECORD=""
PAUSE=8
for arg in "$@"; do
  case "$arg" in
    --record) RECORD="--record" ;;
    ''|*[!0-9]*) echo "$0: unrecognised argument '$arg'" >&2; exit 2 ;;
    *) PAUSE="$arg" ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CLI="$REPO_ROOT/target/debug/kastellan-cli"
if [ ! -x "$CLI" ]; then
  echo "$0: $CLI not built -- run 'cargo build --workspace' first" >&2
  exit 1
fi
if [ ! -d "$MANIFEST" ]; then
  echo "$0: manifest directory '$MANIFEST' does not exist" >&2
  exit 1
fi

mkdir -p "$OUT"

total=0
bad=0
# `guard capture` takes a DIRECTORY, so each entry is handed to it in a
# scratch directory of its own. That is also what keeps one entry's
# failure from aborting the rest.
#
# The authoritative signal is the TOOL's outcome line, not the exit
# status of the pipeline: piping through `grep` would report whether a
# line matched, which a refusal also satisfies. So the line is captured
# and classified.
for f in "$MANIFEST"/*.json; do
  [ -e "$f" ] || { echo "$0: no *.json in '$MANIFEST'" >&2; exit 1; }
  total=$((total + 1))
  tmp=$(mktemp -d)
  cp "$f" "$tmp/"
  line=$(timeout 180 "$CLI" guard capture --manifest "$tmp" --out "$OUT" $RECORD 2>&1 \
         | grep -E "^(OK|RECORD-NEW|RECORD-SAME|REFUSED|FETCH-FAILED)" | head -1)
  rm -rf "$tmp"

  if [ -z "$line" ]; then
    # No recognisable outcome at all: a timeout, a crash, or an output
    # shape change. Counted as bad -- silence must never read as success.
    echo "NO-OUTCOME $(basename "$f" .json)"
    bad=$((bad + 1))
  else
    echo "$line"
    case "$line" in
      OK*|RECORD-NEW*|RECORD-SAME*) ;;
      *) bad=$((bad + 1)) ;;
    esac
  fi
  sleep "$PAUSE"
done

echo
if [ "$bad" -gt 0 ]; then
  echo "$bad of $total entries did NOT succeed (out dir: $OUT)" >&2
  exit 1
fi
echo "all $total entries succeeded into $OUT"
