# Deploying the kastellan Matrix homeserver (conduwuit)

kastellan's primary user↔agent channel is **Matrix, self-hosted, single-user,
federation OFF** (decision:
[`docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`](../superpowers/specs/2026-06-12-primary-communication-channel-design.md)).
This page covers standing up the homeserver. The Matrix *client* runs as a
sandboxed kastellan worker (slice #2); this page is about the *server*.

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
2. **Install the conduwuit binary** (per upstream) at a root-owned path.
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
6. **Create the two accounts** (operator + `@kastellan:<server>`) using the
   registration token (Element → Register against your URL, or the conduwuit
   register API), then set `allow_registration = false` and restart.
7. **Firewall:** inbound 443 (client API) + WireGuard UDP only.

## Dev / Tier-C quick start

```sh
export KASTELLAN_MATRIX_SERVER_NAME=localhost      # or your domain
scripts/matrix/setup-conduwuit.sh                  # renders+validates+runs on 127.0.0.1:6167
```
Then follow the printed steps to create the accounts + point kastellan at it.

## Wiring kastellan (slice #2 Phase D)

Once the homeserver is up and the bot account + access token exist (stored as a
kastellan secret), the daemon is wired by setting `KASTELLAN_MATRIX_HOMESERVER` /
`KASTELLAN_MATRIX_USER` / `KASTELLAN_MATRIX_PEERS` and building with the
`live-matrix` feature — see
[`docs/superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md`](../superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md)
(Phase D) and the live runbook
[`docs/devel/runbooks/2026-06-12-matrix-live-and-email-dgx.md`](../devel/runbooks/2026-06-12-matrix-live-and-email-dgx.md).
