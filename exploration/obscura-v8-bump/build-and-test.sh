#!/usr/bin/env bash
# NOTE: `pipefail` is load-bearing here, not decoration. Every cargo invocation
# below is piped into `tee`, and without it `$?` is *tee's* status — so a failed
# build reports success. That is not hypothetical: the 2026-09-14 re-test read
# "exit code 0" off a `cargo check` that had emitted 111 errors, purely because
# the status came from the tail of the pipe. Do not remove it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORK_DIR="${SCRIPT_DIR}/_obscura-workdir"
REPO_URL="https://github.com/h4ckf0r0day/obscura.git"
BRANCH="security/bump-deno-core-0.405"

echo "=== Obscura V8 Security Bump ==="
echo "Work directory: ${WORK_DIR}"
echo ""

# ── Step 1: Clone or update ──────────────────────────────────────────
if [ -d "${WORK_DIR}/.git" ]; then
    echo ">>> Updating existing clone..."
    cd "${WORK_DIR}"
    git fetch origin main
    git checkout main
    git reset --hard origin/main
else
    echo ">>> Cloning Obscura..."
    git clone "${REPO_URL}" "${WORK_DIR}"
    cd "${WORK_DIR}"
fi

# ── Step 2: Create branch ────────────────────────────────────────────
echo ""
echo ">>> Creating branch: ${BRANCH}"
git checkout -B "${BRANCH}"

# ── Step 3: Apply Cargo.toml bump ────────────────────────────────────
#
# ⚠️ This bump is KNOWN NOT TO COMPILE as of 2026-09-14 (111 errors). It is kept
# as the starting point for the port, not as a working fix. See README.md
# "Status of the bump" for the error breakdown before running this.
#
# `deno_error` must move in lockstep: deno_core requires >= 0.7 from 0.360
# onward, and Obscura pins 0.6. Bumping deno_core alone yields 30 additional
# trait-bound errors that have nothing to do with the real porting work.
echo ""
echo ">>> Bumping deno_core 0.350 → 0.405 in crates/obscura-js/Cargo.toml"
sed -i.bak 's/deno_core = "0\.350"/deno_core = "0.405"/g' crates/obscura-js/Cargo.toml
sed -i.bak 's/deno_error = "0\.6"/deno_error = "0.7"/g' crates/obscura-js/Cargo.toml
rm -f crates/obscura-js/Cargo.toml.bak

# Show the change
echo "--- Cargo.toml diff ---"
git diff crates/obscura-js/Cargo.toml
echo "---"

# ── Step 4: Build ────────────────────────────────────────────────────
echo ""
echo ">>> Building workspace (V8 compiles from source, ~5 min first time)..."
echo ""

if ! cargo build --workspace 2>&1 | tee "${SCRIPT_DIR}/build.log"; then
    echo ""
    echo "!!! BUILD FAILED !!!"
    echo ""
    echo "Check ${SCRIPT_DIR}/build.log for errors."
    echo ""
    echo "Common fixes:"
    echo "  - CreateSnapshotOptions gained new fields → add them in build.rs"
    echo "  - RuntimeOptions changed → update runtime.rs"
    echo "  - deno_error version mismatch → bump deno_error in Cargo.toml"
    echo ""
    echo "Stepping through intermediate versions does NOT obviously help:"
    echo "  the smallest possible step (0.360) fails before reaching Obscura's"
    echo "  own code — deno_core 0.360 pins temporal_rs 0.0.11, which does not"
    echo "  compile on rustc 1.94.1 and cannot be updated within that tree."
    echo "  See README.md 'Status of the bump'."
    exit 1
fi

echo ""
echo ">>> Build succeeded!"

# ── Step 5: Get actual V8 version ────────────────────────────────────
echo ""
echo ">>> Detecting V8 version..."
V8_VERSION=$(grep -rh "v8_version" target/debug/build/v8-*/out/ 2>/dev/null | head -1 || echo "unknown")
echo "V8 version info: ${V8_VERSION}"
echo ""
echo "To get the exact version string for server.rs, run:"
echo "  cargo run -- serve --port 9222 &"
echo "  curl -s http://127.0.0.1:9222/json/version | jq ."
echo "  kill %1"

# ── Step 6: Run tests ────────────────────────────────────────────────
echo ""
echo ">>> Running tests..."
if cargo test --workspace 2>&1 | tee "${SCRIPT_DIR}/test.log"; then
    echo ""
    echo ">>> All tests passed!"
else
    echo ""
    echo "!!! SOME TESTS FAILED — check ${SCRIPT_DIR}/test.log"
    exit 1
fi

# ── Step 7: Reminder to update version strings ───────────────────────
echo ""
echo "=== NEXT STEPS ==="
echo ""
echo "1. Update V8/Chrome version strings in crates/obscura-cdp/src/server.rs:"
echo "   - Search for '14.5.0.0' and 'Chrome/145'"
echo "   - Replace with actual V8 version from step 5"
echo ""
echo "2. Smoke test:"
echo "   cargo run -- serve --port 9222 &"
echo "   curl -s http://127.0.0.1:9222/json/version | jq ."
echo "   kill %1"
echo ""
echo "3. Commit and push:"
echo "   cd ${WORK_DIR}"
echo "   git add -A"
echo "   git commit -m 'security: bump deno_core 0.350 → 0.405 (V8 14.5 → 14.9)'"
echo "   git remote add fork https://github.com/YOUR_FORK/obscura.git"
echo "   git push -u fork ${BRANCH}"
echo "   gh pr create --repo h4ckf0r0day/obscura \\"
echo "     --title 'security: bump deno_core 0.350 → 0.405 (V8 14.5 → 14.9)' \\"
echo "     --body 'Covers CVE-2026-3910, CVE-2026-5281, CVE-2026-11645'"
echo ""
echo "=== Done ==="
