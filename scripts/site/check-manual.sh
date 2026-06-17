#!/usr/bin/env bash
# Verification suite for the generated manual site (docs.kastellan.dev).
# Builds into a temp dir via uv, then checks: every chapter rendered, every
# local href/src resolves, the Pages glue files exist, the palette tokens are
# present, and tidy reports no HTML errors. Mirrors scripts/site/check-site.sh.
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT
fail=0

# Build (uv resolves the PEP 723 deps).
if ! uv run "$ROOT/scripts/site/build_manual.py" --out "$OUT" >/dev/null; then
  echo "FAIL: build_manual.py did not complete"; exit 1
fi

# 1. Every manifest chapter produced an .html file.
for stem in index 01-what-is-kastellan 02-dev-env-linux 03-dev-env-macos \
            04-repo-tour 05-build-test-run 06-architecture 07-sandboxing \
            08-hard-constraints 09-rust-patterns 10-first-contribution \
            11-cassandra-pipeline 12-memory-and-recall 13-llm-router; do
  [ -f "$OUT/$stem.html" ] || { echo "FAIL: missing $stem.html"; fail=1; }
done

# 2. Pages glue + stylesheets + assets exist.
for f in style.css manual.css pygments.css CNAME .nojekyll assets/favicon.png; do
  [ -e "$OUT/$f" ] || { echo "FAIL: missing $f"; fail=1; }
done
[ "$(cat "$OUT/CNAME" 2>/dev/null)" = "docs.kastellan.dev" ] \
  || { echo "FAIL: CNAME is not docs.kastellan.dev"; fail=1; }

# 3. Palette tokens present in the copied stylesheet.
for token in '#4f46e5' '#fafbfd' '#0f172a'; do
  grep -qi -- "$token" "$OUT/style.css" \
    || { echo "FAIL: style.css missing palette token $token"; fail=1; }
done

# 4. No raw .md link targets leaked, and every local href/src resolves.
for p in "$OUT"/*.html; do
  if grep -qE '(href|src)="[^"]+\.md(#[^"]*)?"' "$p"; then
    echo "FAIL: $(basename "$p") contains an unrewritten .md link"; fail=1
  fi
  for ref in $(grep -oE '(href|src)="[^"]+"' "$p" | sed -E 's/^(href|src)="//; s/"$//' \
               | grep -vE '^(https?:|mailto:|tel:|data:|//|#)'); do
    clean="${ref%%#*}"; clean="${clean%%\?*}"
    if [ -n "$clean" ] && [ ! -e "$OUT/$clean" ]; then
      echo "FAIL: $(basename "$p") references missing local file: $clean"; fail=1
    fi
  done
done

# 5. HTML validity (same loud-skip pattern as check-site.sh).
if ! command -v tidy >/dev/null 2>&1; then
  echo "FAIL: tidy not installed (brew install tidy-html5 / apt install tidy)"; fail=1
elif ! tidy --version 2>/dev/null | grep -qE 'HTML Tidy.*[ .]5\.'; then
  echo "[SKIP] tidy is pre-HTML5 (Apple 2006 build) — HTML validity check skipped"
else
  for p in "$OUT"/*.html; do
    errs=$(tidy -qe "$p" 2>&1 | grep -c "Error:")
    [ "$errs" -eq 0 ] || { echo "FAIL: $(basename "$p") has $errs tidy error(s)"; fail=1; }
  done
fi

if [ "$fail" -eq 0 ]; then echo "OK: all manual checks passed"; else exit 1; fi
