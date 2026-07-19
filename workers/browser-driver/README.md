# kastellan-worker-browser-driver

A read-only headless-browser render worker for kastellan. Exposes a single
JSON-RPC method, `browser.render(url)`, which navigates a URL in a headless
browser (Playwright/Chromium), lets client-side JS settle, and returns the
post-JS readable text + final HTML — "web-fetch for JS-heavy / SPA pages".

**Opt-in.** The daemon registers this worker only when
`KASTELLAN_BROWSER_DRIVER_ENABLE` is truthy (`1|true|yes|on`, trimmed and
case-insensitive — the unified dialect). Stage it with
`scripts/workers/browser-driver/install.sh`, which builds a self-contained venv
with the **system** `python3 -m venv` (deliberately not `uv`: a uv venv symlinks
to an external CPython whose libpython the jail blocks), installs this package
**non-editable** so it is copied into site-packages, and runs
`playwright install chromium`.

**Egress.** When force-routed, the worker starts an in-jail `ProxyShim`
(`shim.py`) and points Chromium at it with `--proxy-server` +
`--proxy-bypass-list=<-loopback>`. The sidecar runs in **no-MITM
transparent-tunnel** mode for this worker — `disable_mitm_for` in
`core/src/worker_lifecycle/force_route.rs` names `browser-driver` explicitly,
because the browser does end-to-end TLS itself and cannot trust our per-instance
MITM CA. In-Chromium NSS trust of that CA is deferred (see ROADMAP).

**Status: implemented.** `browser.render` is fully working — readability
extraction, allowlist enforcement, and the forced-egress path are all live, with
four `#[ignore]` acceptance tests in `core/tests/browser_driver_e2e.rs` (2/2
forced tests green on the DGX). A Firecracker micro-VM mode is in progress; see
`docs/superpowers/specs/2026-07-19-browser-driver-microvm-rootfs-design.md`.

Design: `docs/superpowers/specs/2026-06-12-browser-driver-worker-design.md`.
Plan: `docs/superpowers/plans/2026-06-12-browser-driver-worker.md`.
