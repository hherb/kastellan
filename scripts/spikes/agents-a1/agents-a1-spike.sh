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
#   # Mac-local Ollama Q4 alternative:
#   ENDPOINT=http://127.0.0.1:11434/v1 MODEL=<ollama-tag> scripts/spikes/agents-a1/agents-a1-spike.sh
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
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${OUT:-$HERE/agents-a1-spike-results.md}"

pass=0 fail=0
say()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
ok()   { printf '\033[32m  PASS\033[0m %s\n' "$*"; pass=$((pass+1)); }
bad()  { printf '\033[31m  FAIL\033[0m %s\n' "$*"; fail=$((fail+1)); }

# --- extract a field from an OpenAI chat response via python3 (no jq dep) ---
jqpy() { python3 -c 'import sys,json;d=json.load(sys.stdin);print(eval(sys.argv[1]))' "$1"; }

chat() { # $1 = JSON body -> raw response on stdout
  curl -sS --max-time 300 "$ENDPOINT/chat/completions" \
    -H 'content-type: application/json' -d "$1"
}

# ---------------------------------------------------------------------------
say "Part 0 — license gate (HARD constraint: AGPL-compatible only)"
echo "  Fetching https://huggingface.co/InternScience/Agents-A1/raw/main/LICENSE ..."
LIC="$(curl -sSL --max-time 30 https://huggingface.co/InternScience/Agents-A1/raw/main/LICENSE 2>/dev/null | head -3 || true)"
echo "  ---"
echo "${LIC:-  (could not fetch — check the model card manually)}" | sed 's/^/  /'
echo "  ---"
if printf '%s' "$LIC" | grep -qiE 'apache license|mit license|bsd|mozilla public|gnu (lesser )?general public'; then
  ok "license text looks AGPL-compatible (verify the full file / model-card field)"
else
  bad "license NOT confirmed AGPL-compatible — STOP and verify manually before adopting"
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
R="$(chat "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Reply with exactly one word: kastellan\"}],\"max_tokens\":16,\"temperature\":0}")"
MSG="$(printf '%s' "$R" | jqpy 'd["choices"][0]["message"]["content"]' 2>/dev/null || echo '<parse-error>')"
echo "  model said: $MSG"
[ "$MSG" != '<parse-error>' ] && [ -n "$MSG" ] && ok "chat completion returned content" || bad "no usable chat content (raw: ${R:0:200})"

# ---------------------------------------------------------------------------
say "Part 3 — tool-calling (validates the qwen3_coder parser emits tool_calls)"
TR="$(chat "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"List the files in /etc. You must call the provided tool.\"}],\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"list_dir\",\"description\":\"List a directory\",\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"]}}}],\"tool_choice\":\"auto\",\"max_tokens\":256,\"temperature\":0}")"
TNAME="$(printf '%s' "$TR" | python3 -c 'import sys,json
try:
  d=json.load(sys.stdin); tc=d["choices"][0]["message"].get("tool_calls") or []
  print(tc[0]["function"]["name"] if tc else "<none>")
except Exception as e: print("<parse-error>")' 2>/dev/null)"
echo "  tool_call name: $TNAME"
[ "$TNAME" = "list_dir" ] && ok "model emitted a well-formed tool_call" || bad "expected tool_call list_dir, got '$TNAME' (raw: ${TR:0:200})"

# ---------------------------------------------------------------------------
say "Results — $pass passed, $fail failed"
{
  echo "# Agents-A1 spike results"
  echo
  echo "- Endpoint: \`$ENDPOINT\`"
  echo "- Model: \`$MODEL\`"
  echo "- Automated probes: **$pass passed / $fail failed** (license gate, reachability, chat, tool-calling)"
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
