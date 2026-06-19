# Deploying the kastellan Matrix homeserver (Continuwuity)

kastellan's primary user↔agent channel is **Matrix, self-hosted, single-user,
federation OFF** (decision:
[`docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`](../superpowers/specs/2026-06-12-primary-communication-channel-design.md)).
This page covers standing up the homeserver. The Matrix *client* runs as a
sandboxed kastellan worker (slice #2); this page is about the *server*.

> **conduwuit is archived — we run Continuwuity.** The original `conduwuit`
> project was archived/unmaintained in 2025. We deploy
> **[Continuwuity](https://continuwuity.org)**, the maintained community
> continuation (frequent security releases). It keeps the `conduwuit` binary
> name + `conduwuit.toml` config format, so the template + checker here are
> unchanged in spirit; only a few config keys differ (noted inline). A live
> production deployment exists at **`matrix.kastellan.dev`** — its end-to-end
> bring-up is captured in the runbook
> [`docs/devel/runbooks/2026-06-19-matrix-homeserver-deploy.md`](../devel/runbooks/2026-06-19-matrix-homeserver-deploy.md),
> with copy-paste phase scripts under [`scripts/matrix/vps/`](../../scripts/matrix/vps/).

> **Why kastellan does not supervise the homeserver itself.** kastellan's
> supervisor is *user-level* (`systemd --user` / launchd LaunchAgents), but the
> homeserver must run as a **dedicated unprivileged `matrix` user** — which a
> user-level manager cannot provide. So the homeserver is deployed independently
> (a root/system unit, or a separate host), not via a kastellan `ServiceSpec`.

## Files

- [`deploy/matrix/conduwuit.toml.template`](../../deploy/matrix/conduwuit.toml.template)
  — hardened, federation-off config template (the source of truth for the
  security invariants).
- [`deploy/matrix/kastellan-matrix.service.template`](../../deploy/matrix/kastellan-matrix.service.template)
  — hardened **system** systemd unit (dedicated user + sandboxing).
- [`scripts/matrix/setup-conduwuit.sh`](../../scripts/matrix/setup-conduwuit.sh)
  — dev / Tier-C local bring-up (renders + validates the config, runs conduwuit
  on loopback).
- [`scripts/matrix/check-conduwuit-config.sh`](../../scripts/matrix/check-conduwuit-config.sh)
  — validates a rendered config (federation off, loopback bind, registration not
  open); `--self-test` checks the committed template.

## Security invariants (non-negotiable)

The check script HARD-FAILS unless:

1. **`allow_federation = false`** — no untrusted remote homeservers ever reach
   this server. Removes the entire federation attack surface (most homeserver
   CVEs); makes it a private two-party appliance. Do not enable.
2. **`address = "127.0.0.1"`** — loopback bind. A TLS-terminating reverse proxy
   (Caddy/nginx) is the only thing facing the network.
3. **Registration is not open** — token-gated (`allow_registration = true` with a
   `registration_token`) only for the one-time creation of the operator + bot
   accounts, then `allow_registration = false`.

## Hosting tiers (fail-down; pick one)

| Tier | Where | Trade-off |
|------|-------|-----------|
| **A (preferred)** | a **dedicated** small VPS | Homeserver is a separate compromise + failure domain from both the WireGuard/ingress box and the kastellan agent host. Clean separation; a few €/mo. |
| **B** | the existing **WireGuard / network-ingress VPS** | Co-hosted with the tunnel into your home/DGX network — a homeserver RCE is adjacent to the WireGuard keys. Requires the hardened system unit below. |
| **C ("poor man's")** | the **kastellan host itself** | Co-hosted with the agent — adjacent to the agent's user/Postgres/secrets. Requires the hardened system unit; relies on the dedicated-`matrix`-user separation. The fail-down default when no separate box exists. |

The honest risk for B/C: conduwuit is the larger public-facing surface, so if it
is the entry point (RCE), the attacker is on the same host as (B) the WireGuard
tunnel or (C) the agent. The hardened unit contains an RCE to the `matrix` user +
its store — defense-in-depth that **reduces but does not eliminate** shared-host
blast radius. Tier A avoids it entirely. **Redundancy is cross-transport (the
email fallback), not a second homeserver** — Matrix has no single-user homeserver
failover.

## Production install (Tiers B/C — run as root)

1. **Create the dedicated user + data dir:**
   ```sh
   sudo useradd --system --home /var/lib/conduwuit --shell /usr/sbin/nologin matrix
   sudo mkdir -p /var/lib/conduwuit && sudo chown matrix:matrix /var/lib/conduwuit
   ```
2. **Install the Continuwuity binary** at a root-owned path. Releases are on
   [forgejo.ellis.link](https://forgejo.ellis.link/continuwuation/continuwuity/releases)
   (the binary is still named `conduwuit`). It is **dynamically linked** — it
   needs `libjemalloc2` + a kernel with `io_uring` on the host (`apt install
   libjemalloc2 liburing2`). On modern x86_64 (AVX2) use the
   `conduwuit-haswell-linux-amd64-maxperf` asset; otherwise `conduwuit-linux-amd64`.
3. **Render the config** from the template (substitute `{{SERVER_NAME}}`,
   `{{PORT}}`, `{{DB_PATH}}`, `{{REGISTRATION_TOKEN}}`) to e.g.
   `/etc/kastellan/conduwuit.toml`, then **validate**:
   ```sh
   scripts/matrix/check-conduwuit-config.sh /etc/kastellan/conduwuit.toml
   ```
4. **Install the hardened unit** (substitute `{{CONDUWUIT_BIN}}`,
   `{{CONFIG_PATH}}`, `{{STATE_DIR}}`):
   ```sh
   sudo cp deploy/matrix/kastellan-matrix.service.template \
           /etc/systemd/system/kastellan-matrix.service   # edit placeholders
   sudo systemctl daemon-reload && sudo systemctl enable --now kastellan-matrix
   ```
5. **Reverse proxy** (Caddy/nginx) terminates TLS on 443 and proxies to
   `127.0.0.1:<port>`. **Do not** open the federation port (8448).
6. **Create the two accounts** (operator + `@kastellan:<server>`), then set
   `allow_registration = false` and restart. **⚠️ First-user bootstrap gotcha:**
   on a brand-new Continuwuity server the **first** account (which becomes the
   admin) must be registered with a **one-time, server-generated bootstrap
   token** that Continuwuity prints to its log at startup
   (`journalctl -u kastellan-matrix | grep 'registration token'`) — the
   `registration_token` from your config **does not work until that first admin
   account exists**. Register the operator first with the bootstrap token, then
   `@kastellan` with the config token. (The phase scripts in
   [`scripts/matrix/vps/`](../../scripts/matrix/vps/) automate exactly this.)
7. **Firewall:** inbound 443 (client API) + 80 (ACME) + WireGuard UDP only.

## Tier A — dedicated public VPS (worked example: `matrix.kastellan.dev`)

The preferred topology: conduwuit on its own small box, **loopback-bound**, with a
TLS-terminating reverse proxy in front. The hardened unit + config are identical
to Tiers B/C; Tier A just adds the public-facing pieces (DNS, TLS, firewall) and
the clean separation of a dedicated host. Steps use `matrix.kastellan.dev` / port
`6167` as the running example.

**Prerequisites.** An `A` record `matrix.kastellan.dev → <vps-ip>` that resolves
(`dig +short matrix.kastellan.dev`), and — on a 1 GB box — a swap file as
insurance (`fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile
&& swapon /swapfile`, persisted in `/etc/fstab`).

1. **Firewall — client API + ACME only; never the federation port (8448):**
   ```sh
   sudo ufw allow OpenSSH && sudo ufw allow 80 && sudo ufw allow 443 && sudo ufw --force enable
   ```
2. **Continuwuity binary** (root-owned), matching the box's arch (`uname -m`).
   Needs `libjemalloc2` + `io_uring`:
   ```sh
   sudo apt install -y libjemalloc2 liburing2
   # x86_64 w/ AVX2 — see the releases page for the current version + aarch64 asset:
   sudo curl -fSL --proto '=https' -o /usr/local/bin/conduwuit \
     https://forgejo.ellis.link/continuwuation/continuwuity/releases/download/v0.5.9/conduwuit-haswell-linux-amd64-maxperf
   sudo chmod 755 /usr/local/bin/conduwuit && conduwuit --version
   ```
3. **Dedicated user + config + hardened unit** — exactly the Tiers B/C steps
   above, with `server_name = "matrix.kastellan.dev"`, `address = "127.0.0.1"`,
   `port = 6167`. Validate with `check-conduwuit-config.sh`, then
   `systemctl enable --now kastellan-matrix`.
4. **TLS reverse proxy (Caddy — automatic Let's Encrypt):** `sudo apt install -y caddy`,
   then `/etc/caddy/Caddyfile`:
   ```
   matrix.kastellan.dev {
       reverse_proxy 127.0.0.1:6167
   }
   ```
   `sudo systemctl restart caddy`, then verify:
   `curl https://matrix.kastellan.dev/_matrix/client/versions` (valid TLS, JSON).
5. **Accounts + close registration** — create the operator (first → admin) with
   the **bootstrap token** from the log, then `@kastellan` with the config token
   (see the first-user bootstrap gotcha above), then `allow_registration = false`
   and restart. The [`scripts/matrix/vps/phase4-accounts.sh`](../../scripts/matrix/vps/phase4-accounts.sh)
   helper drives this whole flow.
6. **Point the worker at it** — `KASTELLAN_MATRIX_HOMESERVER_URL=https://matrix.kastellan.dev`.
   The worker validates the real cert natively; its egress sidecar runs as a
   transparent tunnel (no MITM) for the matrix worker. See "Wiring kastellan".

## Dev / Tier-C quick start

```sh
export KASTELLAN_MATRIX_SERVER_NAME=localhost      # or your domain
scripts/matrix/setup-conduwuit.sh                  # renders+validates+runs on 127.0.0.1:6167
```
Then follow the printed steps to create the accounts + point kastellan at it.

## Live e2e loop (headless, throwaway)

To exercise the worker's `matrix-rust-sdk` integration end to end without standing
up a homeserver of your own,
[`scripts/matrix/dev-e2e-bootstrap.sh`](../../scripts/matrix/dev-e2e-bootstrap.sh)
brings up a **loopback, throwaway** homeserver in a container (matrix-conduit —
conduwuit's upstream; same client-server API + E2E relay), registers two accounts,
creates one **encrypted** room they both join, and writes the env block
[`core/tests/matrix_live_e2e.rs`](../../core/tests/matrix_live_e2e.rs) needs. This
is **dev-only** — not the production homeserver.

```sh
cargo build -p kastellan-worker-matrix --features live-matrix   # build the live worker first
scripts/matrix/dev-e2e-bootstrap.sh up                          # bring up + bootstrap
source ~/.matrix-e2e.env
cargo test -p kastellan-core --test matrix_live_e2e -- --ignored --nocapture
scripts/matrix/dev-e2e-bootstrap.sh down                        # stop + wipe
```
Needs `docker` (or `DOCKER=podman`) + `curl` + `jq`. Runs anywhere the live worker
builds (verified on the DGX, aarch64).

## Wiring kastellan (slice #2 Phase D)

Once the homeserver is up and the bot account + access token exist (stored as a
kastellan secret), the daemon is wired by setting `KASTELLAN_MATRIX_HOMESERVER` /
`KASTELLAN_MATRIX_USER` / `KASTELLAN_MATRIX_PEERS` and building with the
`live-matrix` feature — see
[`docs/superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md`](../superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md)
(Phase D) and the live runbook
[`docs/devel/runbooks/2026-06-12-matrix-live-and-email-dgx.md`](../devel/runbooks/2026-06-12-matrix-live-and-email-dgx.md).
