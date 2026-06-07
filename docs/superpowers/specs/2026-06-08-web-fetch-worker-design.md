# Design: `web-fetch` worker

**Date:** 2026-06-08
**ROADMAP item:** "`web-fetch` worker: HTTPS-only, host allowlist, body cap, redirect cap" (ROADMAP.md:145, Phase 3)
**Status:** approved, pre-implementation

## Problem

The agent loop is complete and proven end-to-end (`cli_ask_e2e`), but the only
agent-callable tool today is `shell-exec` (argv allowlist) — there is **no way
for the agent to reach the network**. The `workers/web-fetch` directory is an
empty `.gitkeep` placeholder. Without a web tool, the "submit a research task,
get a report" use case is impossible.

This worker is also the deliberate **first real consumer of network egress**, so
that the upcoming egress-proxy work has a genuine workload to enforce against
(rather than a throwaway test client). The proxy slice will later read this
worker's `Net::Allowlist` policy and enforce it at the trust boundary; this slice
establishes the worker, its self-enforced interim containment, and the policy
data the proxy will consume.

## Decision summary

Add a new tool worker `hhagent-worker-web-fetch` exposing a single JSON-RPC
method `web.fetch` that takes a URL and returns extracted readable text. It
mirrors the `shell-exec` worker pattern exactly: a small Rust binary using
`hhagent_worker_prelude::serve_stdio` (which calls `lock_down()` before serving),
plus a host-side `WorkerManifest` in `core/src/workers/web_fetch.rs` that declares
the `SandboxPolicy`.

The one policy difference from shell-exec that matters: `Profile::WorkerNetClient`
(permits `socket(2)`) + `Net::Allowlist`. Both already exist in the sandbox and
prelude crates — the net-permitting jail is built; this worker selects it.

### Interim network containment (pre-egress-proxy)

Until the egress proxy lands, `Net::Allowlist` is **not** enforced at the network
layer (Linux bwrap currently maps `Net::Allowlist` to `--share-net` = full net;
macOS Seatbelt to `(allow network*)`). So this slice contains the host allowlist
two ways from one source:

1. **Worker self-enforces** its domain allowlist (injected via env), re-checked on
   every redirect hop. This is real containment now, though worker-trust-dependent
   (a fully compromised worker could ignore its own check). It becomes
   defense-in-depth **layer 2** once the proxy enforces at the boundary.
2. **`Net::Allowlist` policy data is populated** (mapped to `host:443` entries) so
   the `SandboxPolicy` is already correct when the egress proxy slice reads it —
   no rework.

Both representations derive from the **same** `tool_allowlists` DB rows, so they
cannot drift.

## Approach

**HTTP client + runtime model: reqwest + a worker-owned tokio runtime.** Reuse the
already-vetted, already-in-tree `reqwest` 0.12 / `rustls-tls` stack (same TLS
posture as `llm-router`; no system OpenSSL). `serve_stdio` is blocking, so the
handler owns a current-thread tokio `Runtime` and `block_on`s each fetch. Rejected
alternative: a second HTTP stack (`ureq`) — a new dependency to license-vet that
duplicates the TLS config the project already standardized on, for a marginal
syscall-surface win that `Profile::WorkerNetClient` was explicitly built to cover.

New dependencies are therefore limited to the two **content** crates, needed
regardless of client choice (license-vetting both — must be AGPL-compatible — is a
plan step):

- HTML readability extraction — candidate `dom_smoothie` (MIT/Apache, pure Rust),
  **aliased in `Cargo.toml` to `readable_html`** via Cargo's package-rename
  (`readable_html = { package = "dom_smoothie", version = "…" }`) so the worker
  code refers to a self-documenting name regardless of which underlying crate the
  vetting stage settles on. (Note: a crate literally named `html2text`/`html2txt`
  exists but does *dumb* HTML→text conversion without boilerplate stripping — not
  what we want; the value here is readability/main-content extraction.)
- PDF text extraction — candidate `pdf-extract` (MIT, pure Rust).

### Known risk: DNS resolution under the jail

Under bwrap `--unshare-all` the jail filesystem is minimal, so the worker's
`fs_read` must include `/etc/resolv.conf` (and `/etc/hosts`, `/etc/nsswitch.conf`)
or DNS fails inside the sandbox. The macOS Seatbelt profile likewise needs the
resolver permitted. **Plan-time verification:** confirm DNS actually resolves
inside the live bwrap jail and the Seatbelt equivalent; if glibc NSS proves
fiddly, fall back to reqwest's `hickory-dns` resolver feature (pure-Rust resolver;
still reads `/etc/resolv.conf`, but bypasses NSS).

## Components

- **`workers/web-fetch/`** — new crate `hhagent-worker-web-fetch`, bin
  `hhagent-worker-web-fetch`. Deps: `hhagent-protocol`, `hhagent-worker-prelude`,
  `reqwest` (workspace), `tokio` (workspace), `serde`/`serde_json`/`anyhow`,
  `url`, the HTML-readability crate (aliased `readable_html`), the PDF-text crate.
- **`core/src/workers/web_fetch.rs`** — `WebFetchManifest` + `web_fetch_entry(binary,
  allowlist) -> ToolEntry`, mirroring `core/src/workers/shell_exec.rs`. Selects
  `Profile::WorkerNetClient` + `Net::Allowlist`.
- **Registration** — add `WebFetchManifest` to the static `WORKER_MANIFESTS` list
  in `core/src/registry_build.rs`; add `"workers/web-fetch"` to the workspace
  `members` in the root `Cargo.toml`.

## JSON-RPC contract

**Method:** `web.fetch`

**Params** (GET-only, HTTPS-only; the caller cannot widen the request surface —
same philosophy as shell-exec's argv allowlist):
```json
{ "url": "https://en.wikipedia.org/wiki/Foo" }
```

**Result:**
```json
{
  "final_url": "https://en.wikipedia.org/wiki/Foo",
  "status": 200,
  "content_type": "text/html",
  "title": "Foo — Wikipedia",
  "text": "…extracted readable text…",
  "truncated": false
}
```
- `final_url` — URL after redirects (so the LLM sees where it actually landed).
- `status` — final HTTP status code.
- `content_type` — final response content-type.
- `title` — best-effort (`<title>` for HTML; `null` for PDF/text/JSON).
- `text` — extracted readable text.
- `truncated` — `true` iff `text` was cut at the extracted-text cap.

## Request flow & safety caps

1. Parse `url`. **Reject non-`https` schemes** → `POLICY_DENIED`.
2. Check host against the injected allowlist (exact host, or leading-dot
   subdomain form). Non-match → `POLICY_DENIED`.
3. GET with:
   - redirects followed **manually**, up to a cap (default **5**),
     **re-checking the allowlist on every hop** (a 302 to a non-allowlisted host
     is refused — hence not using reqwest's automatic redirect following);
   - per-request **timeout** (default **20s**);
   - a **response body byte cap** (default **5 MiB**) enforced while streaming so
     a hostile server cannot OOM the jail.
4. Dispatch on `Content-Type`:
   - `text/html` → readability extract → `text` + `title`;
   - `application/pdf` → PDF text extract → `text` (`title` null);
   - `text/*`, `application/json` → decode as-is → `text` (`title` null);
   - anything else → `OPERATION_FAILED` naming the content-type.
5. Cap the **extracted text** length (default **100 KiB**, sets `truncated`) —
   keeps the planner's context budget sane until the large-result handoff cache
   (ROADMAP:129) lands.

All caps are compile-time constants in this slice (env-configurable later if a
real need appears — YAGNI now).

## Sandbox policy

`web_fetch_entry()` produces this `ToolEntry`, contrasted with shell-exec's
`Net::Deny` / `WorkerStrict`:

```rust
SandboxPolicy {
    fs_read: vec![
        binary.clone(),
        // DNS resolution under --unshare-all needs these readable in the jail:
        "/etc/resolv.conf".into(),
        "/etc/hosts".into(),
        "/etc/nsswitch.conf".into(),
    ],
    fs_write: vec![],
    net: Net::Allowlist(allowlist_host_ports),  // policy data set for the proxy slice
    cpu_ms: 10_000,
    mem_mb: 512,                                 // PDF/HTML parsing is heavier than argv exec
    profile: Profile::WorkerNetClient,           // permits socket()
    env: vec![("HHAGENT_WEB_FETCH_ALLOWLIST".to_string(), allow_json)],
    cpu_quota_pct: None,
    tasks_max: None,
}
// ToolEntry: wall_clock_ms: Some(30_000), lifecycle: SingleUse,
//            sandbox_backend: None, container_image: None
```

**One source, two representations.** The DB `tool_allowlists` rows for tool
`"web-fetch"` are the canonical domain list. The manifest:
- injects them as a JSON array env `HHAGENT_WEB_FETCH_ALLOWLIST` for the worker's
  own per-hop check, and
- maps them to `host:443` entries for `Net::Allowlist`,

both from the same rows — no drift. The manifest declares
`allowlist_tool() == Some("web-fetch")` so the daemon pre-fetches the rows (same
plumbing shell-exec uses). Binary discovery uses the standard `discover_binary`
precedence (`HHAGENT_WEB_FETCH_BIN` override authoritative, else exe-relative
sibling `hhagent-worker-web-fetch`).

## Error handling

Maps onto existing `hhagent-protocol` codes (the vocabulary shell-exec uses):

- `INVALID_PARAMS` — missing or malformed `url`.
- `POLICY_DENIED` — non-https scheme, or host not on the allowlist (initial URL or
  any redirect hop). Message names the rejected host, never a secret.
- `OPERATION_FAILED` — network error, timeout, redirect-cap exceeded, body-cap
  exceeded, unsupported content-type, or extraction failure. Human-readable,
  host/type-specific.
- `METHOD_NOT_FOUND` — any method other than `web.fetch`.

**No silent fallbacks:** a fetch that cannot be completed *or* safely extracted
returns an error, never an empty-but-success result.

## Testing (TDD — written before the implementation)

Following the repo split (pure unit tests + gated integration), keeping every file
under the 500-LOC cap.

1. **Worker unit tests** (no network):
   - allowlist matcher: exact host match; leading-dot subdomain match; rejects
     `evil-example.com` against an `example.com` entry; `.example.com` matches
     `a.example.com` and `example.com`;
   - non-https scheme rejected;
   - content-type dispatch (html / pdf / text / json / unsupported);
   - body-cap and extracted-text-cap truncation flag;
   - HTML readability and PDF extraction against small in-repo fixture bytes.
2. **Manifest unit tests** (mirror `core/src/workers/shell_exec.rs` tests):
   - `resolve` registers with the expected policy (`WorkerNetClient`,
     `Net::Allowlist` derived from the allowlist, the `/etc/*` `fs_read` entries,
     the env JSON); `Misconfigured` when no binary is found;
   - the dual-representation allowlist mapping (env JSON ↔ `Net::Allowlist`
     `host:443`) is byte-checked from one input.
3. **Core integration e2e** (`core/tests/web_fetch_e2e.rs`, gated, skip-as-pass
   where the sandbox can't run):
   - real core → sandbox → web-fetch round-trip against a **hand-rolled localhost
     HTTP test server** (same pattern as `llm-router`'s `local_backend_e2e` with a
     tokio `TcpListener`): serves an HTML page on an allowlisted host, asserts
     extracted text;
   - serves a redirect to a non-allowlisted host, asserts `POLICY_DENIED`.
   Localhost keeps it hermetic (no real internet) and exercises the live jail
   including the DNS path.

## Out of scope (deferred follow-ups)

- **Egress-proxy enforcement** of the allowlist at the trust boundary — the next
  slice; this worker is its consumer. `Net::Allowlist` data is populated here for
  that slice to read.
- **TLS pinning** and the **credential-leak scanner** — egress-proxy concerns.
- **Large-result handoff cache** (ROADMAP:129) — the extracted-text cap is the
  interim guard; the cache supersedes it when web-shaped results land.
- **`web-search`** (SearxNG) — separate worker; lets the agent discover URLs
  rather than being handed them. Needed for true "research," but independent of
  this slice.
- **Caller-supplied method/headers/body** (POST etc.) — GET-only by design now.
- **Env-configurable caps** — compile-time constants until a real need appears.
- **Wiring web-fetch into the agent's tool surface + the local-LLM demo** (the
  gemma model on Ollama drives the *agent loop*, not this worker) — a follow-up
  once the worker and its tests are green.
