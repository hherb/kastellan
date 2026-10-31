"""Cold vs warm prefill, oMLX vs Ollama, same model family (Gemma 4 26B-A4B, 4-bit).

Dev-only measurement script (stdlib only), not part of the product. Run:
  python3 -I scripts/bench/prefill_bench.py prompts/agent_planner.md omlx gemma-4-26b-a4b-it-4bit 20,40,60
  python3 -I scripts/bench/prefill_bench.py prompts/agent_planner.md ollama gemma4:26b-a4b-it-qat 20,40,60
Results and method: docs/devel/notes/2026-10-31-prefill-cache-benchmark.md.

cold: the prompt differs at its first bytes (today's minute-stamped <now> block).
warm: a long identical prefix, a different short tail (a stable-first layout's next turn).
Prints one JSON line per measurement.
"""
import json, sys, time, urllib.request

PLANNER = open(sys.argv[1]).read()
SIZE_CHARS = {20: 30_000, 40: 68_000, 60: 104_000}  # filler chars for ~k total tokens (measured ~2 chars/token)


def filler(n_chars, salt):
    # Varied mail-like JSON so it tokenises like a real plans_so_far summary.
    out, i = [], 0
    while sum(map(len, out)) < n_chars:
        out.append(json.dumps({"message_id": 18000 + i, "from": f"billing{i % 37}@example{i % 11}.com.au",
                               "subject": f"Invoice I26{i:04d} for services {i % 12 + 1}/2026",
                               "date": f"2026-{i % 12 + 1:02d}-{i % 28 + 1:02d}T0{i % 9}:1{i % 6}:00Z",
                               "body_text": f"Dear customer, amount due ${(i * 37.31) % 2000:.2f} incl. GST ${(i * 3.39) % 180:.2f}. "
                                            f"Reference {salt}-{i * 7919 % 100000}. Please pay within 14 days."}))
        i += 1
    return "\n".join(out)


def omlx(model, system, user):
    body = {"model": model, "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
            "max_tokens": 64, "temperature": 0, "stream": True, "stream_options": {"include_usage": True}}
    req = urllib.request.Request("http://127.0.0.1:8000/v1/chat/completions", json.dumps(body).encode(),
                                 {"Content-Type": "application/json"})
    t0, ttft, usage, n = time.time(), None, None, 0
    with urllib.request.urlopen(req, timeout=1800) as r:
        for line in r:
            line = line.decode().strip()
            if not line.startswith("data:") or line == "data: [DONE]":
                continue
            ev = json.loads(line[5:])
            if ev.get("usage"):
                usage = ev["usage"]
            for c in ev.get("choices", []):
                d = c.get("delta", {})
                if d.get("content") or d.get("reasoning_content") or d.get("reasoning"):
                    n += 1
                    if ttft is None:
                        ttft = time.time() - t0
    total = time.time() - t0
    return {"ttft_s": round(ttft or total, 2), "total_s": round(total, 2), "chunks": n, "usage": usage}


def ollama(model, system, user):
    body = {"model": model, "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
            "stream": False, "think": False,
            "options": {"num_ctx": 65536, "num_predict": 64, "temperature": 0}}
    req = urllib.request.Request("http://127.0.0.1:11434/api/chat", json.dumps(body).encode(),
                                 {"Content-Type": "application/json"})
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=1800) as r:
        v = json.load(r)
    return {"total_s": round(time.time() - t0, 2), "prompt_tokens": v.get("prompt_eval_count"),
            "prefill_s": round(v.get("prompt_eval_duration", 0) / 1e9, 2),
            "decode_tok_s": round(v["eval_count"] / (v["eval_duration"] / 1e9), 1) if v.get("eval_duration") else None}


def run(backend, model, k):
    call = omlx if backend == "omlx" else ollama
    stamp = time.strftime("%H:%M:%S")
    big = filler(SIZE_CHARS[k], salt=f"{backend}{k}")
    # cold: unique first line, as the minute-stamped <now> block does today
    sys1 = f"<now>{stamp} {time.time_ns()}</now>\n" + PLANNER
    cold = call(model, sys1, big + "\nNext step: say OK.")
    print(json.dumps({"backend": backend, "k": k, "kind": "cold", **cold}), flush=True)
    # the identical request again: the best case any cache can give
    again = call(model, sys1, big + "\nNext step: say OK.")
    print(json.dumps({"backend": backend, "k": k, "kind": "identical-again", **again}), flush=True)
    # the clock changes at the front: today's layout, next minute
    moved = call(model, f"<now>{stamp} {time.time_ns()}</now>\n" + PLANNER, big + "\nNext step: say OK.")
    print(json.dumps({"backend": backend, "k": k, "kind": "front-changed", **moved}), flush=True)
    stable_sys = PLANNER
    a = call(model, stable_sys, big + "\n<now>a</now> Next step: say A.")
    b = call(model, stable_sys, big + "\n<now>b</now> Next step: say B.")
    print(json.dumps({"backend": backend, "k": k, "kind": "stable-prefix-first", **a}), flush=True)
    print(json.dumps({"backend": backend, "k": k, "kind": "stable-prefix-new-tail", **b}), flush=True)


if __name__ == "__main__":
    backend, model = sys.argv[2], sys.argv[3]
    for k in map(int, sys.argv[4].split(",")):
        run(backend, model, k)
