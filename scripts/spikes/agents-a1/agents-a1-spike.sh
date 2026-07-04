#!/usr/bin/env bash
# agents-a1-spike.sh — validate InternScience/Agents-A1 as kastellan's
# general-purpose reasoning model against a live OpenAI-compatible endpoint.
#
# See docs/devel/notes/2026-07-03-agents-a1-eval.md for the full assessment.
#
# This script does NOT launch the model server — it exercises an endpoint you
# have already started (vLLM/SGLang on the DGX, or Ollama on the Mac). It runs
# the license gate + three smoke probes and writes a fill-in results file.
#
# Usage:
#   # 1. start the server first, e.g. on the DGX (aarch64, ~35 GB FP8 download):
#   #    vllm serve InternScience/Agents-A1-FP8-dynamic --host 0.0.0.0 --port 8000 \
#   #      --served-model-name agents-a1 --max-model-len 262144 \
#   #      --enable-auto-tool-choice --tool-call-parser qwen3_coder
#   #
#   # 2. then, from a machine that can reach it (Mac over WireGuard):
#   ENDPOINT=http://10.0.0.3:8000/v1 MODEL=agents-a1 scripts/spikes/agents-a1/agents-a1-spike.sh
#
#   # Ollama Q4 alternative (Mac or DGX). The upstream community GGUF ships a
#   # broken stub template (completion-only, leaks control tokens, no tools);
#   # build the fixed tag first from the Modelfile beside this script:
#   #   ollama pull hf.co/InternScience/Agents-A1-Q4_K_M-GGUF
#   #   ollama create agents-a1:q4 -f scripts/spikes/agents-a1/agents-a1.Modelfile
#   ENDPOINT=http://127.0.0.1:11434/v1 MODEL=agents-a1:q4 scripts/spikes/agents-a1/agents-a1-spike.sh
#
# Env:
#   ENDPOINT  OpenAI-compatible base URL, incl. /v1   (default http://127.0.0.1:8000/v1)
#   MODEL     served model name                        (default agents-a1)
#   OUT       results markdown path                    (default alongside this script)
#
# Requires: curl, python3. Exits non-zero if any probe fails.
set -euo pipefail

ENDPOINT="${ENDPOINT:-http://127.0.0.1:8000/v1}"
MODEL="${MODEL:-agents-a1}"
# ${BASH_SOURCE[0]:-$0} keeps `set -u` happy when the script is piped in
# (e.g. `ssh host bash -s < agents-a1-spike.sh`), where BASH_SOURCE is unset;
# the cd is guarded so a non-file invocation falls back to the cwd.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || pwd)"
OUT="${OUT:-$HERE/agents-a1-spike-results.md}"

pass=0 fail=0 warns=0
say()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
ok()   { printf '\033[32m  PASS\033[0m %s\n' "$*"; pass=$((pass+1)); }
bad()  { printf '\033[31m  FAIL\033[0m %s\n' "$*"; fail=$((fail+1)); }
warn() { printf '\033[33m  WARN\033[0m %s\n' "$*"; warns=$((warns+1)); }

# --- extract a field from an OpenAI chat response via python3 (no jq dep) ---
jqpy() { python3 -c 'import sys,json;d=json.load(sys.stdin);print(eval(sys.argv[1]))' "$1"; }

chat() { # $1 = JSON body -> raw response on stdout
  curl -sS --max-time 300 "$ENDPOINT/chat/completions" \
    -H 'content-type: application/json' -d "$1"
}

# ---------------------------------------------------------------------------
# AGPL-compatible allowlist per CLAUDE.md: Apache/MIT/BSD/MPL/LGPL/(A)GPL.
# The "(affero |lesser )?" branch matters — AGPL is the project's own license
# class and must not be flagged incompatible.
COMPAT_RE='apache|mit|bsd|mpl|mozilla public|isc|(a|l)?gpl|gnu (affero |lesser )?general public'
say "Part 0 — license gate (HARD constraint: AGPL-compatible only)"
# The HF API `cardData.license` SPDX tag is authoritative and reliable — the
# raw `/LICENSE` blob 404s on repos that name the file differently (Agents-A1
# does), which previously produced a false FAIL. Query the API first, fall back
# to the raw blob only if the API yields nothing.
echo "  Querying https://huggingface.co/api/models/InternScience/Agents-A1 (cardData.license) ..."
LID="$(curl -sSL --max-time 30 https://huggingface.co/api/models/InternScience/Agents-A1 2>/dev/null \
  | python3 -c 'import sys,json
try:
  d=json.load(sys.stdin); print(d.get("cardData",{}).get("license") or d.get("license") or "")
except Exception: print("")' 2>/dev/null || true)"
if [ -n "$LID" ]; then
  echo "  API license id: $LID"
  if printf '%s' "$LID" | grep -qiE "$COMPAT_RE"; then
    ok "HF API reports AGPL-compatible license: '$LID'"
  else
    bad "HF API license '$LID' is NOT on the AGPL-compatible allow-list — STOP and verify before adopting"
  fi
else
  # Fall back to the raw LICENSE blob (best-effort; may 404 by filename).
  echo "  API gave no license id; falling back to raw LICENSE blob ..."
  LIC="$(curl -sSL --max-time 30 https://huggingface.co/InternScience/Agents-A1/raw/main/LICENSE 2>/dev/null | head -3 || true)"
  echo "  ---"; echo "${LIC:-  (could not fetch — check the model card manually)}" | sed 's/^/  /'; echo "  ---"
  if [ -z "$LIC" ] || printf '%s' "$LIC" | grep -qiE 'entry not found'; then
    # A network/filename miss is not evidence of an incompatible license; warn
    # and defer to the manual model-card check rather than failing the spike.
    warn "could not read a license id — verify AGPL-compatibility manually (model card)"
  elif printf '%s' "$LIC" | grep -qiE "$COMPAT_RE"; then
    ok "raw LICENSE text looks AGPL-compatible (verify the model-card field)"
  else
    bad "raw LICENSE text did NOT match a known AGPL-compatible license — STOP and verify manually"
  fi
fi

# ---------------------------------------------------------------------------
say "Part 1 — reachability + model listing ($ENDPOINT, model=$MODEL)"
if curl -sS --max-time 20 "$ENDPOINT/models" >/tmp/a1_models.json 2>/dev/null; then
  ids="$(python3 -c 'import sys,json;print(", ".join(m["id"] for m in json.load(sys.stdin).get("data",[])))' </tmp/a1_models.json 2>/dev/null || echo '?')"
  ok "endpoint reachable; served models: ${ids:-none}"
else
  bad "endpoint $ENDPOINT/models unreachable — is the server up and routable?"
  echo "  aborting further probes."; exit 1
fi

# ---------------------------------------------------------------------------
say "Part 2 — basic chat round-trip"
# `|| true`: a curl timeout/connection error must land in the `bad` branch below
# (and still write the results file), not abort the whole spike via `set -e`.
# max_tokens is generous (512): Agents-A1 is a *thinking* model, so a tiny budget
# is spent reasoning before any final `content` is emitted. The extractor also
# falls back to the separated reasoning channel so a thinking model still counts
# as a live round-trip.
R="$(chat "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Reply with exactly one word: kastellan\"}],\"max_tokens\":512,\"temperature\":0}" || true)"
MSG="$(printf '%s' "$R" | python3 -c 'import sys,json
try:
  m=json.load(sys.stdin)["choices"][0]["message"]
  c=(m.get("content") or "").strip()
  if not c:
    r=(m.get("reasoning") or m.get("reasoning_content") or "").strip()
    c=("[reasoning-only] "+r) if r else ""
  print(c if c else "<empty>")
except Exception: print("<parse-error>")' 2>/dev/null || echo '<parse-error>')"
echo "  model said: ${MSG:0:120}"
case "$MSG" in
  '<parse-error>'|'<empty>'|'') bad "no usable chat content (raw: ${R:0:200})" ;;
  *) ok "chat completion returned content" ;;
esac

# ---------------------------------------------------------------------------
say "Part 3 — tool-calling (validates the qwen3_coder parser emits tool_calls)"
TR="$(chat "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"List the files in /etc. You must call the provided tool.\"}],\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"list_dir\",\"description\":\"List a directory\",\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"]}}}],\"tool_choice\":\"auto\",\"max_tokens\":256,\"temperature\":0}" || true)"
TNAME="$(printf '%s' "$TR" | python3 -c 'import sys,json
try:
  d=json.load(sys.stdin); tc=d["choices"][0]["message"].get("tool_calls") or []
  print(tc[0]["function"]["name"] if tc else "<none>")
except Exception as e: print("<parse-error>")' 2>/dev/null)"
echo "  tool_call name: $TNAME"
[ "$TNAME" = "list_dir" ] && ok "model emitted a well-formed tool_call" || bad "expected tool_call list_dir, got '$TNAME' (raw: ${TR:0:200})"

# ---------------------------------------------------------------------------
say "Results — $pass passed, $fail failed, $warns warned"
{
  echo "# Agents-A1 spike results"
  echo
  echo "- Endpoint: \`$ENDPOINT\`"
  echo "- Model: \`$MODEL\`"
  echo "- Automated probes: **$pass passed / $fail failed / $warns warned** (license gate, reachability, chat, tool-calling)"
  echo
  echo "## Manual checks still owed (fill in)"
  echo "- [ ] License file/model-card field confirmed AGPL-compatible: ____"
  echo "- [ ] Quant + hardware used (DGX FP8 / Mac Q4): ____"
  echo "- [ ] Throughput (tok/s), first-token latency, mem footprint: ____"
  echo "- [ ] Long-horizon: a >=10-step sandboxed-tool chain stayed coherent: ____"
  echo "- [ ] Instruction-following on CASSANDRA policy prompts (no jailbreak drift): ____"
  echo "- [ ] Recall-grounded answer quality vs current local model: ____"
  echo "- [ ] Decision: adopt / reject / defer — and why: ____"
} >"$OUT"
echo "  wrote $OUT"

[ "$fail" -eq 0 ] || exit 1
