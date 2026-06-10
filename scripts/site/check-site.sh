#!/usr/bin/env bash
# Verification suite for the static site in site/.
# Checks: every expected page exists, is valid-enough HTML (tidy: no errors),
# every local href/src it references resolves to a real file, and every page
# carries the required meta/OG tags and shared nav.
set -u
SITE_DIR="$(cd "$(dirname "$0")/../../site" && pwd)"
[ -n "$SITE_DIR" ] && [ -d "$SITE_DIR" ] || { echo "FAIL: site/ directory not found"; exit 1; }
PAGES="index.html roadmap.html security.html contributing.html"
fail=0

# tidy: hard-fail if absent; loud-skip if it's the pre-HTML5 Apple 2006 build
# (macOS /usr/bin/tidy rejects <nav>/<main> etc.). brew install tidy-html5 for
# full validation; CI should install a modern tidy so this never skips there.
#
# Apple 2006 build prints:
#   HTML Tidy for Mac OS X released on 31 October 2006 - Apple Inc. build 13462
# Modern HTML Tidy 5.x prints:
#   HTML Tidy for Apple macOS version 5.8.0   (or Linux version 5.x.y etc.)
#
# Regex 'HTML Tidy.*[ .]5\.' matches a space or literal dot before "5."
# — matches "version 5.8.0" but not the 2006 date-based string (no "5." present).
TIDY_OK=1
if ! command -v tidy >/dev/null 2>&1; then
  echo "FAIL: tidy not installed (brew install tidy-html5 / apt install tidy)"; exit 1
elif ! tidy --version 2>/dev/null | grep -qE 'HTML Tidy.*[ .]5\.'; then
  echo "[SKIP] tidy is pre-HTML5 (Apple 2006 build) — HTML validity check skipped; brew install tidy-html5 for full validation"
  TIDY_OK=0
fi

for page in $PAGES; do
  p="$SITE_DIR/$page"
  if [ ! -f "$p" ]; then echo "FAIL: missing page $page"; fail=1; continue; fi

  # 1. HTML validity: tidy reports no Errors (warnings tolerated)
  if [ "$TIDY_OK" -eq 1 ]; then
    tidy_out=$(tidy -qe "$p" 2>&1)
    errs=$(printf '%s\n' "$tidy_out" | grep -c "Error:")
    if [ "$errs" -ne 0 ]; then echo "FAIL: $page has $errs tidy error(s)"; printf '%s\n' "$tidy_out" | grep "Error:"; fail=1; fi
  fi

  # 2. Required head tags
  for needle in '<meta name="description"' 'property="og:title"' 'property="og:image"' '<title>'; do
    if ! grep -q "$needle" "$p"; then echo "FAIL: $page missing $needle"; fail=1; fi
  done

  # 3. Shared nav links present on every page
  for link in 'href="roadmap.html"' 'href="security.html"' 'href="contributing.html"' 'github.com/hherb/kastellan'; do
    if ! grep -q "$link" "$p"; then echo "FAIL: $page missing nav link $link"; fail=1; fi
  done

  # 4. Every local href/src resolves to a file.
  # Filter skips absolute URLs (https?:, mailto:, tel:, data:, protocol-relative //)
  # and fragment-only refs (#). Note: word-splitting here is intentional and safe —
  # hrefs in this controlled static site contain no spaces.
  for ref in $(grep -oE '(href|src)="[^"]+"' "$p" | sed -E 's/^(href|src)="//; s/"$//' \
               | grep -vE '^(https?:|mailto:|tel:|data:|//|#)'); do
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
