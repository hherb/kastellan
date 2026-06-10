#!/usr/bin/env bash
# Verification suite for the static site in site/.
# Checks: every expected page exists, is valid-enough HTML (tidy: no errors),
# every local href/src it references resolves to a real file, and every page
# carries the required meta/OG tags and shared nav.
set -u
SITE_DIR="$(cd "$(dirname "$0")/../../site" && pwd)"
PAGES="index.html roadmap.html security.html contributing.html"
fail=0

for page in $PAGES; do
  p="$SITE_DIR/$page"
  if [ ! -f "$p" ]; then echo "FAIL: missing page $page"; fail=1; continue; fi

  # 1. HTML validity: tidy reports no Errors (warnings tolerated)
  errs=$(tidy -qe "$p" 2>&1 | grep -c "Error:")
  if [ "$errs" -ne 0 ]; then echo "FAIL: $page has $errs tidy error(s)"; tidy -qe "$p" 2>&1 | grep "Error:"; fail=1; fi

  # 2. Required head tags
  for needle in '<meta name="description"' 'property="og:title"' 'property="og:image"' '<title>'; do
    if ! grep -q "$needle" "$p"; then echo "FAIL: $page missing $needle"; fail=1; fi
  done

  # 3. Shared nav links present on every page
  for link in 'href="roadmap.html"' 'href="security.html"' 'href="contributing.html"' 'github.com/hherb/kastellan'; do
    if ! grep -q "$link" "$p"; then echo "FAIL: $page missing nav link $link"; fail=1; fi
  done

  # 4. Every local (non-http, non-anchor) href/src resolves to a file
  for ref in $(grep -oE '(href|src)="[^"]+"' "$p" | sed -E 's/^(href|src)="//; s/"$//' \
               | grep -vE '^(https?:|mailto:|#)'); do
    clean="${ref%%#*}"; clean="${clean%%\?*}"
    if [ -n "$clean" ] && [ ! -e "$SITE_DIR/$clean" ]; then
      echo "FAIL: $page references missing local file: $clean"; fail=1
    fi
  done
done

# 5. Stylesheet exists and defines the locked design tokens
css="$SITE_DIR/style.css"
if [ ! -f "$css" ]; then echo "FAIL: missing style.css"; fail=1; else
  for token in '#4f46e5' '#fafbfd' '#0f172a'; do
    if ! grep -qi -- "$token" "$css"; then echo "FAIL: style.css missing palette token $token"; fail=1; fi
  done
fi

if [ "$fail" -eq 0 ]; then echo "OK: all site checks passed"; else exit 1; fi
