# Matrix homeserver VPS bring-up (Tier A) — phase scripts

Reproducible, idempotent phase scripts that stand up the kastellan Matrix
homeserver (**Continuwuity**, federation OFF) on a dedicated public VPS — the
Tier A topology in [`docs/deploy/matrix-homeserver.md`](../../../docs/deploy/matrix-homeserver.md).
These are the exact scripts used for the live `matrix.kastellan.dev` deployment;
the end-to-end narrative + verification is in the runbook
[`docs/devel/runbooks/2026-06-19-matrix-homeserver-deploy.md`](../../../docs/devel/runbooks/2026-06-19-matrix-homeserver-deploy.md).

Run **as root, in order**, on the target box. Each is safe to re-run.

| Phase | Script | What it does |
|------|--------|--------------|
| 1 | `phase1-hardening.sh` | Swap, sysctl, ufw (SSH+80+443, deny-in), fail2ban, unattended-upgrades, SSH hardening |
| 2 | `phase2-homeserver.sh` | `matrix` user + data dir, download Continuwuity (pinned), federation-off loopback config, validate, hardened systemd unit, start |
| 3 | `phase3-caddy-tls.sh` | Caddy auto-TLS reverse proxy for `${SERVER_NAME}` → `127.0.0.1:6167` + client well-known |
| 4 | `phase4-accounts.sh` | Register operator (admin, via the **bootstrap token**) + `@kastellan` (config token), then close registration |

## Before you run

- Edit the `SERVER_NAME` (and `PORT`) variables at the top of each script for
  your host (they default to `matrix.kastellan.dev` / `6167`).
- An `A` record for `SERVER_NAME` must already resolve to the box (Phase 3's
  ACME needs it).
- Phase 2 and Phase 4 expect [`../check-conduwuit-config.sh`](../check-conduwuit-config.sh)
  to sit **next to them** at runtime (the deployment copies both into `~/`).

## The one non-obvious gotcha

A brand-new Continuwuity server requires the **first** account (the admin) to be
created with a **one-time, server-generated bootstrap token** printed to the log
at startup — the `registration_token` from your config does **not** work until
that first admin exists. `phase4-accounts.sh` reads the bootstrap token from the
journal automatically and uses it for the operator, then the config token for
`@kastellan`. Do not restart the service between Phase 2 and Phase 4 expecting
your config token to work for the first user.
