# Primary user↔kastellan communication channel — Matrix (self-hosted) + email fallback

**Date:** 2026-06-12
**Status:** Design approved (operator brainstorm 2026-06-12); implementation plan pending.
**ROADMAP:** Phase 2 — Channels (read-only) + Phase 3 — Channels outbound.
**Scope:** Decides *which* channel(s) kastellan and the user talk over, and the *hosting* and
*security* posture for them. The channel-bus plumbing, the pairing/auth layer, and the
per-channel worker implementations each get their own plan → implementation cycle; this spec
fixes the architecture they build against.

---

## Problem

kastellan needs a primary bidirectional channel between the user and the agent. The operator's
hard criteria: **easy to install, available on all platforms, no usage restrictions, no running
costs, fail-proof, and secure.** Those criteria are in tension — "fail-proof + no restrictions"
argues against reverse-engineered client libraries and centralized providers that can ban a
bot/number; "secure" argues for end-to-end encryption (the project already distrusts the
transport — threat-model adversary #1 prompt-injection, #5 peer-impersonation); "no running
cost + vendor-neutral self-host" argues for something we host ourselves. No single option wins
on every axis, so the design layers a secure primary with a robust cross-transport fallback.

### The reframe that drives the design

**Transport security and peer identity are separate layers and must both be solved:**

1. **Transport confidentiality + integrity** — only E2E encryption stops the *provider/MITM*
   from reading or *injecting* message content. The planned pairing layer does **not** cover
   this; it authenticates the peer, not the bytes.
2. **Peer authentication** — the already-planned pairing flow (TOTP/HOTP/WebAuthn, Phase 2)
   stops impersonation (adversary #5). Channel-native identity (Matrix device cross-signing)
   layers under it.
3. **Audit** — every inbound/outbound channel message is logged (already an invariant) and
   screened by `cassandra::injection_guard` exactly like untrusted worker output.

## Decision

- **Primary channel: Matrix, self-hosted, single-user, federation disabled**, via the
  `matrix-rust-sdk` (Apache-2.0). E2E-encrypted (Olm/Megolm via `vodozemac`), vendor-neutral,
  self-hostable at zero marginal cost, polished clients on every platform (Element), and a
  mature SDK that will not shatter on upstream protocol changes the way a reverse-engineered
  Signal client does. Device cross-signing gives strong channel-native anti-impersonation that
  complements the pairing layer.
- **Fallback channel: dedicated email (IMAP inbound / SMTP outbound)** — the most universal and
  genuinely fail-proof transport, on a *separate failure domain and separate provider*. Used for
  async, **low-trust notifications, never commands** (email is trivially spoofable — see
  "Why not …"). This is the redundancy answer; Matrix is **not** the right tool for
  primary/secondary homeserver failover (see "Homeserver redundancy").

### Why not the alternatives (recorded so we don't re-litigate)

| Option | Killed on |
|--------|-----------|
| **Signal** (`presage`, AGPL) | Best-in-class E2E, but `presage` is reverse-engineered → fragile to upstream changes (fails "fail-proof"); Signal number-ban risk; hostile to third-party clients. |
| **Telegram** (`grammers`) | Easiest to ship, but **no E2E for bots** (cloud chats are Telegram-readable → fails "secure"), centralized, can rate-limit/ban → fails "no restrictions" and the vendor-neutral ethos. |
| **Email as *primary*** | Most fail-proof/universal, but spoofable (fails adversary #5 without a heavy PGP+signing layer) and a huge phishing/attachment injection surface. Demoted to fallback. |

## Hosting tiers (operator-selectable, fail-down)

The homeserver is just another supervised unit (fits "single-host deployment, OS-native
supervisor, no k3s"). Three deployment tiers, **most-isolated first**:

| Tier | Where conduwuit runs | Isolation posture | When |
|------|----------------------|-------------------|------|
| **A (preferred)** | A **dedicated** small VPS (e.g. its own Hetzner CX-class instance, a few €/mo) | Own host: comms server is a separate compromise *and* failure domain from both the WireGuard/network-ingress box and the kastellan agent host. | Default recommendation. |
| **B** | The **existing Hetzner WireGuard VPS** | Co-hosted with network ingress → **shared blast radius with the tunnel into the home network**. Requires the hardening checklist below as the minimum bar. | If unwilling to pay for a second instance. |
| **C ("poor man's")** | The **kastellan host itself** | Co-hosted with the agent → a homeserver RCE is *adjacent to the agent it serves*. Same hardening checklist; additionally relies on the OS-user separation kastellan already assumes. | Fallback default when no separate box is available at all. |

The choice is operator config (which host the supervisor installs the unit on); the code is
identical across tiers. The spec records all three so the trade-off is durable.

### Security analysis of co-hosting (Tiers B and C)

The honest risk is **not** "Matrix endangers WireGuard directly" — WireGuard is a tiny
attack surface (UDP, in-kernel, silent to unauthenticated packets, no pre-auth parser) and is
unlikely to be the *entry* point. The real threat is **"Matrix is the larger public-facing
surface; if it is the entry point (RCE), the attacker is then on the same host as"**:

- **Tier B:** the WireGuard keys/config and the tunnel into the home network / DGX (the dev
  setup already drives the DGX over WireGuard SSH). This is the most sensitive box to add a
  public service to — hence Tier A is preferred.
- **Tier C:** the kastellan agent, its Postgres role, its scratch FS, and its secrets vault —
  i.e. exactly the assets the threat-model invariant is built to contain. Co-locating the comms
  server with the agent partially erodes that containment.

**Mitigation — apply kastellan's own philosophy to the homeserver** (defense-in-depth; reduces
but does not eliminate shared-host blast radius):

- Dedicated unprivileged `matrix` OS user; **never root**.
- systemd unit hardening: `NoNewPrivileges=yes`, `ProtectSystem=strict`, `ProtectHome=yes`,
  `PrivateTmp=yes`, a tight `SystemCallFilter=`, `ReadWritePaths=` limited to the data dir
  (mirrors the `linux_bwrap`/prelude posture we already enforce on workers). Or a container.
- WireGuard keys `0600 root:root` — unreadable by the `matrix` user (Tier B).
- Reverse proxy (Caddy) terminates TLS; conduwuit binds loopback only.
- Firewall: inbound 443 (client API) + WireGuard UDP only. **No federation port 8448.**

## Homeserver — conduwuit, single-user, federation OFF

- **Server:** **conduwuit** (maintained Conduit fork) — a single Rust binary, RocksDB backend,
  ~100–300 MB RAM, no Postgres/Redis dependency. Far lighter than Synapse and a clean ethos fit.
  License is AGPL/Apache-compatible (verify exact license at plan time per the dependency rule).
- **Single-user, closed registration** (token-only): only the user and the kastellan bot
  account exist.
- **Federation disabled** (`allow_federation = false`, no 8448): removes the entire
  "untrusted remote homeservers talk to mine" attack surface, where most homeserver CVEs live.
  This turns the homeserver into a near-private two-party appliance and is the single biggest
  surface reduction available.

### Homeserver redundancy — why email, not a second homeserver

Matrix has **no primary/secondary homeserver failover** for a single-user setup. Federation
replicates *rooms across the homeservers of their participants* — it is not a primary/replica HA
model for one homeserver, and a single box hosting both ends is a single point of failure
regardless. (Synapse worker/replication clusters exist but are the wrong weight class here.)
The correct redundancy is **cross-transport**: when the Matrix box is down, kastellan still
reaches the user via the **email fallback** — different server, protocol, and provider. That is
strictly more robust than two homeservers and is why the fallback is email, not Matrix #2.

## Architecture

### Channel-bus abstraction (build first)

The ROADMAP already calls for "Channel-bus fan-in into core conversation queue." Build that
abstraction **before** committing to Matrix specifics so the choice is reversible and fallbacks
are additive:

- A `Channel` trait (inbound stream of `IncomingMessage` + outbound `send`), one implementation
  per transport (`MatrixChannel`, `EmailChannel`), behind the bus.
- The bus fans inbound messages into the core conversation queue (the existing Postgres `tasks`
  / `LISTEN`+`NOTIFY` substrate — the same operator→daemon command channel `ask` and
  `memory l3 run` already ride; no new IPC mechanism).
- **The pairing/auth layer sits *above* the bus, channel-agnostic** — pairing authenticates a
  *peer principal*, independent of which transport carried the message.
- **Every inbound message is untrusted**: screened by `cassandra::injection_guard` (the same
  `extract_scannable_text` → `screen_with_profile` pipeline applied to worker output) before it
  influences a plan. A channel peer is exactly as untrusted as a fetched web page.

### Network containment for the channel workers

Inbound/outbound channel I/O is network egress and must obey the egress story already built:
the IMAP/SMTP/Matrix client runs under `Net::Allowlist` scoped to **only** the configured
server endpoint(s) (the homeserver host:port; the IMAP/SMTP host:port), force-routed through
the per-worker egress proxy (slices #1–#3 are live/landing). A compromised channel worker
reaches its one configured server and nothing else — the invariant holds at the network layer
for the comms path too. (The Matrix server, if Tier C co-hosted, is reached over loopback /
the configured address like the local-SearxNG literal-IP carve-out.)

### Library choices (verify licenses at plan time per the AGPL-compat rule)

- **Matrix:** `matrix-rust-sdk` (Apache-2.0) + `vodozemac` (Apache-2.0) for E2E.
- **Email:** an IMAP client crate for inbound + `lettre` (MIT/Apache) for SMTP outbound. (The
  ROADMAP already lists "IMAP inbound worker" and "SMTP outbound in mail worker.")
- **Pairing:** TOTP/HOTP + WebAuthn crates (already the Phase-2 DM-pairing-flow plan).

## Cross-platform

All of the above is pure-Rust client code over the existing `SandboxBackend`/egress seams — no
OS-specific channel code, satisfying the hard cross-platform constraint. The *homeserver* runs
on a Linux VPS (Tiers A/B) or the kastellan host (Tier C, Linux or macOS); it is operator
infrastructure, supervised but not part of the cross-platform worker matrix.

## Decomposition (slices, each its own plan → impl)

| # | Slice | Depends on | Phase |
|---|-------|-----------|-------|
| 1 | **Channel-bus abstraction** (`Channel` trait + fan-in to the task queue + inbound injection-guard screening) | — | 2 |
| 2 | **Matrix inbound** (`MatrixChannel` read path, E2E, single-user homeserver bring-up docs/scripts) | 1 | 2 |
| 3 | **Pairing/auth layer** (TOTP/HOTP + WebAuthn over the bus, revocable, audited) | 1 | 2 |
| 4 | **Matrix outbound** (agent → user replies) | 2 | 3 |
| 5 | **Email fallback** (IMAP inbound + SMTP outbound, low-trust notifications) | 1 | 2/3 |
| 6 | **Homeserver supervisor unit + hardening** (conduwuit unit, federation-off config, Tier A/B/C install path) | — | 2 |

## Explicitly deferred / out of scope

- Signal, Telegram, and other transports — recorded as rejected above; can be added later as
  additional `Channel` impls if a need arises, but not pursued now.
- Matrix bridges (to SMS/Signal/etc.) — out of scope; they reintroduce third-party trust.
- Homeserver HA/clustering — explicitly not pursued (cross-transport email is the redundancy).
- Rich media / attachment handling beyond text — later; attachments are an injection surface and
  get their own treatment when needed.

## Open points (for the plan, not blocking the design)

- Exact conduwuit license + version pin; confirm AGPL-compatibility per the dependency rule.
- conduwuit vs. an alternative lightweight homeserver if the license/maintenance check fails.
- Whether the kastellan bot is a separate Matrix account or a device of the user's account
  (separate account is cleaner for audit + revocation; confirm at plan time).
- E2E key/device verification UX for the bot account (cross-signing bootstrap, recovery key
  storage — store under the secrets vault, never plaintext).
- Email fallback's anti-spoof posture: even as low-trust, require SPF/DKIM/DMARC pass + a shared
  per-pairing token in-body before any message is even surfaced to the user (never actioned).
