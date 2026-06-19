# Runbook — Matrix homeserver deploy (`matrix.kastellan.dev`, Continuwuity)

**Date:** 2026-06-19 · **Topology:** Tier A (dedicated public VPS) · **Outcome:** live + secured.

Stood up kastellan's production Matrix homeserver on a fresh VPS:
**Continuwuity** (the maintained conduwuit continuation — conduwuit itself is
archived), federation OFF, loopback-bound behind Caddy TLS. Driven over SSH from
the dev Mac; all privileged steps via reviewable phase scripts
([`scripts/matrix/vps/`](../../../scripts/matrix/vps/)). Reference:
[`docs/deploy/matrix-homeserver.md`](../../deploy/matrix-homeserver.md).

## Box

- Host `matrix.kastellan.dev` (203.57.115.150), SSH `hherb@…:2222` (key-only, root-login off).
- Ubuntu 26.04, x86_64 **AVX2**, 1 vCPU, 956 MB RAM, kernel 7.0 (io_uring OK), 20 GB disk.

## What was deployed (4 phases)

1. **OS hardening** — 2 GB swap (box had none); sysctl (swappiness + net hardening);
   ufw default-deny inbound, allow **2222/80/443** only (8448 never opened);
   fail2ban sshd jail on 2222 (banned a live brute-forcer within seconds);
   unattended-upgrades; additive SSH hardening drop-in.
2. **Continuwuity 0.5.9** — `conduwuit-haswell-linux-amd64-maxperf` (pinned,
   sha256 `7489e33c…be79460`) at `/usr/local/bin/conduwuit`; dedicated `matrix`
   system user + `/var/lib/conduwuit` (RocksDB store, 0700); federation-off
   loopback config at `/etc/kastellan/conduwuit.toml` (`port=6167`, token-gated,
   token generated on-box at `/root/kastellan-registration-token.txt`);
   validated by `check-conduwuit-config.sh`; hardened systemd unit
   `kastellan-matrix.service`. RAM footprint ~20 MB.
3. **Caddy v2.11.4** — auto Let's Encrypt TLS for `matrix.kastellan.dev` →
   `127.0.0.1:6167`, HSTS, serves `/.well-known/matrix/client`. No federation
   well-known, no 8448.
4. **Accounts + close** — `@horst` (admin) + `@kastellan` (agent bot), then
   `allow_registration = false` + restart; external register now 403.

## Gotchas hit (and the fixes baked into the scripts)

- **conduwuit is archived** → switched to Continuwuity. Config-compatible, but a
  few keys differ: it has a single `allow_federation` (no
  `allow_incoming/outgoing_federation`), and update-check is
  `allow_announcements_check` (not `allow_check_for_updates`). The committed
  template [`deploy/matrix/conduwuit.toml.template`](../../../deploy/matrix/conduwuit.toml.template)
  was corrected to match.
- **First-user bootstrap token.** A new Continuwuity server rejects the config
  `registration_token` until the first admin exists; that first account must use
  a **one-time bootstrap token printed to the log at startup**. This cost three
  Phase-4 iterations to diagnose (`M_FORBIDDEN: Invalid registration token`).
  `phase4-accounts.sh` now reads the bootstrap token from the journal for the
  operator, then uses the config token for `@kastellan`.
- **ufw + SSH on 2222.** The doc's `ufw allow OpenSSH` opens port 22, not 2222 —
  the scripts allow `2222/tcp` explicitly (allowed *before* `ufw enable`).
- **Hardened unit + io_uring/jemalloc.** `SystemCallFilter=@system-service`
  permits io_uring; jemalloc is fine under `MemoryDenyWriteExecute=yes`. No
  syscall friction — clean start, 0 restarts.

## Verification (all green)

External: HTTPS client API OK with valid Let's Encrypt cert; `POST /register` →
**403** (closed); `GET /login` offers `m.login.password` (the worker's path);
`.well-known/matrix/client` serves; port **8448 closed**. On-box: `kastellan-matrix`,
`caddy`, `fail2ban` all active, 0 restarts; fresh key-only SSH login confirmed
(no lockout).

## Next steps (wiring the kastellan worker — Phase D Task 5)

The homeserver is ready; nothing in the daemon points at it yet.

1. **Store `@kastellan`'s password as a kastellan secret** (`db::secrets`, not a
   plaintext env file).
2. **Worker env** (per the matrix worker contract): `KASTELLAN_MATRIX_HOMESERVER_URL=https://matrix.kastellan.dev`,
   `KASTELLAN_MATRIX_USER=kastellan`, `KASTELLAN_MATRIX_PASSWORD=<secret>` (initial
   login only; the worker then persists `<store>/session.json`),
   `KASTELLAN_MATRIX_STORE=<persistent dir>`.
3. **Pairing** — from `@horst` in Element, DM `@kastellan` and present a
   `kastellan-cli pair issue` code (DM pairing, slice #3).
4. The egress-coupled channel-worker spawn + `ChannelBus` wiring is the open
   **★ TOP PICK** in HANDOVER (Matrix Phase D Tasks 5–6).

> **Maintenance:** Continuwuity cuts security releases every week or two. To
> upgrade: download the new pinned binary over the old one + `systemctl restart
> kastellan-matrix`. The Let's Encrypt cert auto-renews via Caddy.
