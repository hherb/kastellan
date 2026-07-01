# Obscura V8 Security Bump: deno_core 0.350 -> 0.405

## Why

Obscura pins `deno_core = "0.350"` -> V8 engine 14.5, which is vulnerable to
three actively-exploited zero-days:

| CVE | CVSS | Description | Patched in |
|-----|------|-------------|------------|
| CVE-2026-3910 | High | V8 zero-day, in-the-wild | Chrome 147 |
| CVE-2026-5281 | High | V8 zero-day | Chrome 148 |
| CVE-2026-11645 | 8.8 | OOB read/write in V8, in-the-wild | Chrome 149 |

Bumping to `deno_core 0.405` (June 25 2026) pulls V8 14.9 via Deno's
`rusty_v8` bindings, covering all three CVEs.

## Quick start

```bash
cd exploration/obscura-v8-bump
./build-and-test.sh
```

The script clones Obscura, applies the patch, builds, and runs tests.
V8 compiles from source on first build (~5 min, ~5 GB disk).

## What the patch changes

1. **`crates/obscura-js/Cargo.toml`** — `deno_core = "0.350"` -> `"0.405"`
   (both `[dependencies]` and `[build-dependencies]`)
2. **`crates/obscura-cdp/src/server.rs`** — V8/Chrome version strings
   updated to match the new engine

## Breakage risk assessment

| File | API surface | Risk |
|------|-------------|------|
| `build.rs` | `CreateSnapshotOptions` | Moderate — struct may have new fields |
| `ops.rs` | `#[op2]` macros | Low — stable across versions |
| `runtime.rs` | `JsRuntime::new`, `RuntimeOptions` | Moderate |
| `v8_lock.rs` | Pure tokio Mutex | None |

If the direct jump breaks, try stepping: 0.350 -> 0.370 -> 0.390 -> 0.405.

## Relevance to Kastellan

Evaluating Obscura as a lightweight alternative to Chromium for the
`web-scrape` worker. Obscura is a V8-powered DOM scraper (no layout engine),
useful for JS-rendered SPA content extraction without the 300 MB Chromium
overhead. Not a replacement for the `browser-driver` worker (which needs
real rendering). See the project assessment in the PR description.

## Files

- `bump-deno-core.patch` — the Cargo.toml diff
- `version-strings.patch` — the CDP version string updates
- `build-and-test.sh` — clone, patch, build, test automation
