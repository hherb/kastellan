# kastellan-worker-browser-driver

A read-only headless-browser render worker for kastellan. Exposes a single
JSON-RPC method, `browser.render(url)`, which navigates a URL in a headless
browser (Playwright/Chromium), lets client-side JS settle, and returns the
post-JS readable text + final HTML — "web-fetch for JS-heavy / SPA pages".

**Opt-in.** The daemon registers this worker only when
`KASTELLAN_BROWSER_DRIVER_ENABLE=1`. Staging (per the install script, Phase 2):
create a self-contained venv, `pip install -e .`, and `python -m playwright
install chromium`. The browser is force-routed and TLS-MITM'd by the egress
proxy in a later slice; slice #1 runs on the legacy direct-net allowlist path.

**Status:** slice #1 scaffold. The real Playwright render lands in the Phase-2
plan (its launch flags / seccomp profile were settled by the spike — see
`docs/superpowers/specs/2026-06-12-browser-driver-worker-design.md` §3.1).
Until then, starting the worker raises `NotImplementedError` rather than
pretending to render.

Design: `docs/superpowers/specs/2026-06-12-browser-driver-worker-design.md`.
Plan: `docs/superpowers/plans/2026-06-12-browser-driver-worker.md`.
