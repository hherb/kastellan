# 16 — Channels: Matrix, email and the channel bus

A channel is how a person talks to Kastellan from outside the host: a
message comes in, becomes a task, and the task's answer goes back the same
way. **Matrix is the primary channel** (end-to-end encrypted, live in
production). **Email is the fallback**, and today it is **inbound only**. Both
sit behind one transport-agnostic **channel bus** in `core/src/channel/`, and
both transports run in **sandboxed workers** — the core never links
`matrix-rust-sdk` or speaks to a mail server itself.

> **Design sources.** `docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`
> (the bus), `…-channel-pairing-design.md`, `…-matrix-inbound-sandboxed-worker-design.md`,
> [the email fallback design](../../superpowers/specs/2026-07-28-email-fallback-channel-design.md),
> the ask-path slices (`2026-08-16…`, `2026-08-18…`, `2026-08-19…`),
> `2026-09-16-conversation-continuity-design.md` and
> `2026-10-08-825-reply-catch-up-design.md`, all under `docs/superpowers/specs/`.
> Operator setup of a homeserver: [`docs/deploy/matrix-homeserver.md`](../../deploy/matrix-homeserver.md).

---

## The security model in one paragraph

Three separable layers, each fail-closed. **Peer authentication**
(`auth.rs`): a message from a peer the operator has not paired never becomes
a task — it is dropped and audited. **Untrusted-input screening**
(`ingest.rs`): every accepted body goes through the CASSANDRA injection guard
(Strict profile) exactly like worker output — a paired peer is no more trusted
than a fetched web page. **Audit**: every received, rejected, enqueued and
replied message writes an `audit_log` row, and a row that cannot be written is
said out loud. All security decisions are **pure functions** (`auth`,
`ingest`, `route`, `ask_message`, `email::gate`); `bus.rs` is a thin async pump
over the `Channel` trait and two DB seams, so the whole loop is testable with
fakes.

---

## Module layout

```
core/src/channel/
  mod.rs              Channel trait, IncomingMessage/OutgoingMessage, actions::* (audit names)
  bus.rs              ChannelBus: per-channel pumps + the completed-task pump
  bus_inbound.rs      handle_inbound: authorize → pair → ask-answer → screen → enqueue
  bus_outbound.rs     tasks_completed NOTIFY / backlog sweep → route → queue
  bus_deliver.rs      claim the reply where it is sent, then Channel::send
  auth.rs             PeerAuthorizer (DbPeerAuthorizer: one query per message)
  pairing.rs          DbPairingService: single-use code → (channel, peer) binding
  ingest.rs           injection screen + channel task payload
  route.rs            completed task → reply body
  ask_message.rs      ask rendering, /approve /deny parsing (pure)
  outbox.rs           core-initiated outbound (asks) — registry the bus registers into
  catch_up.rs         late-reply note, sweep cadence (pure)
  polled_driver/      generic driver for a long-lived pull-only worker (poll, ack, replies)
  boot_supervisor/    supervised bring-up, restart on pump death, downtime escalation
  pump_liveness.rs    "did any pump stop?" bell
  respawn_alarm.rs    churn alarm for respawning workers/channels
  pg_events/          Postgres seams; lost_rows.rs = thinned [audit-lost] reporting
  matrix.rs, matrix/  MatrixChannel, spawn, config, sandbox policy, wire codecs
  email/              EmailChannel, gate.rs (DMARC + token), config, policy, wire
core/src/main/matrix_boot.rs, email_boot.rs   daemon bring-up of each channel
core/src/scheduler/conversation/              turn_record, the follow-up view and floor
workers/matrix/       kastellan-worker-matrix (matrix-rust-sdk behind JSON-RPC)
workers/matrix-wire/  serde wire types shared by worker and core — no logic
workers/email-in/     kastellan-worker-email-in (polls a localmail subscription)
```

---

## Inbound: one message, in order

`bus_inbound::handle_inbound` decides each message in a fixed order, and the
order is the security content:

1. **Malformed identity** (a NUL in the channel, peer or conversation id) →
   refused before authorization (`channel.rejected_malformed`). A NUL in the
   *body* is escaped and counted instead.
2. **Authorize** `(channel, peer)` against the `pairings` table. Not paired →
   `channel.rejected_unpaired`; lookup failed → `channel.rejected_unverifiable`
   (never mislabelled as unpaired); email evidence failed →
   `channel.rejected_unauthentic` with a reason label.
3. **Pairing carve-out** — only for an unpaired peer on a transport that
   authenticates its own senders (Matrix). The body is compared, by SHA-256,
   with live operator-issued codes; on a match the code is consumed and the
   peer bound in one transaction (`channel.paired`). The body is never
   interpreted, enqueued or echoed.
4. **Ask answer** — a paired peer's `/approve <token>` or `/deny <token>`
   resolves that peer's own open ask and is **never enqueued**. A body that
   carries a live token but does not parse is refused too, so a token never
   lands in `tasks.payload`.
5. **Screen and enqueue** — the injection guard blocks
   (`channel.injection_blocked`, body hash and reason codes only) or the
   message becomes a **fast-lane** task (`channel.received`).

Pairing is operator-only: `kastellan-cli pair issue [--label …] [--ttl-mins …]`
prints a single-use code once (10-minute default lifetime, only its hash is
stored); `pair list [--all]`, `pair revoke <channel> <peer>`. Minting and
revoking use the admin pool — the runtime role can *consume* a code but cannot
mint one or revoke a pairing (migration `0018`).

---

## Outbound: replies and catch-up

When a task finishes, the `tasks_completed` NOTIFY wakes the bus's
completed-task pump, which routes the reply to its channel's queue. The
channel's own pump then **claims** the reply (`db::tasks::reply_claim`,
migration `0027`: one `UPDATE … WHERE reply_settled_at IS NULL`) and only then
sends it, writing `channel.replied` (with `via: notify|catch_up`). Exactly one
sender wins, even with two buses running.

Postgres never replays a missed NOTIFY, so a **catch-up sweep** walks the
unsettled backlog at bus start and every 5 minutes. A reply that goes out more
than 5 minutes after its task finished is delivered anyway, with a note saying
how late it is. A task swept as `crashed` at boot gets a reply saying it was
interrupted.

`channel.replied` means *claimed and handed to the transport*, not delivered.
A failed send — or a polled driver giving up on a reply its worker keeps
refusing — writes `channel.reply_undelivered` with a fixed reason label, never
the body.

---

## Matrix

- **The worker** (`workers/matrix`) wraps `matrix-rust-sdk` (login, E2E sync
  loop, buffered inbound) behind JSON-RPC: `matrix.init`, `matrix.poll`,
  `matrix.send`, `matrix.drain`. The real SDK is behind the **`live-matrix`
  feature**; a default build compiles the hermetic parts and refuses to run.
  `scripts/build-release.sh` builds the featured binary — see the release-build
  warning in `CLAUDE.md`.
- **Sandbox.** bwrap on Linux, Seatbelt on macOS, plus the worker's own
  `matrix_client` seccomp profile and Landlock, applied *after* login
  (`KASTELLAN_MATRIX_ENFORCE_SANDBOX`, default on). Egress is allowlisted to
  the homeserver's host:port and force-routed through the egress proxy.
  Opt-in Firecracker micro-VM on Linux with `KASTELLAN_MATRIX_USE_MICROVM=1`.
- **Room edge.** The worker declines invites and drops events from senders
  outside `KASTELLAN_MATRIX_PEERS` and from rooms that are not two-party
  (bot + one peer). This is defence in depth; **the core's pairing table is
  the gate**.
- **Session.** The crypto store persists in `KASTELLAN_MATRIX_STORE`
  (default under the state dir). The one-time interactive login is
  `kastellan-cli matrix probe`; the daemon then restores the saved session.
- **Driver.** `polled_driver` long-polls the worker under `PersistentWorker`
  supervision; replies are queued per conversation and survive a worker
  respawn.

## Email (inbound only)

`workers/email-in` polls a localmail subscription and hands every message, with
all its `Authentication-Results` headers verbatim, to the core. **The worker
makes no security decisions**; the gate is `channel/email/gate.rs`, so every
rejection is audited. Two independent checks: the **topmost**
`Authentication-Results` header must carry *our* MX's authserv-id
(`KASTELLAN_EMAIL_AUTHSERV_ID`) and `dmarc=pass`, and the body must carry the
per-pairing token on a `kastellan-token:` line. Email peers are paired
outright with `kastellan-cli pair issue-token --channel email --peer <addr>` —
an email sender can never try a pairing code.

⚠️ **Outbound email is not built yet**: `EmailChannel::send` always fails, so
every reply or ask to an email peer is a `channel.reply_undelivered` row.

---

## Operator asks (escalation)

When the review chain returns `Verdict::Escalate`, the scheduler raises an ask
(`asks` table, migration `0023`), parks the task in `awaiting_operator`, and —
for a channel task — delivers the question through `outbox.rs` into the same
conversation, with the two command lines to answer it. The operator replies
`/approve <token>` or `/deny <token>` in Matrix, or uses
`kastellan-cli inbox list|show|resolve` from the host. An approval binds to the
plan's digest; the task resumes through the `tasks_resumed` NOTIFY. Asks
expire (a 60 s sweep in the scheduler). A wrong, used, expired or foreign token
all get the same reply, so the answer is never an existence oracle.

## Conversational continuity

A channel message is a task with only its own sentence. So at finalize a
channel task stores a `tasks.turn_record` (migration `0026`): the tool calls
it made (with parameters, not results) and the most sensitive data class it
touched. The next task in the same channel, peer and conversation is shown up
to **3 earlier turns from the last 5 hours**, screened, as data, never as
instructions — and it **inherits their classification floor**
(`scheduler/conversation/floor.rs`).

---

## Supervised bring-up

Each channel is brought up by a `ChannelSupervisor` (`boot_supervisor/`):
spawn the worker (and its egress sidecar), log in, open the LISTEN connection,
start the bus — retried with capped backoff, **unbounded**, because a
homeserver may be down for an hour. A statically-dead configuration (a
`localhost` homeserver under force-routing, a partial email config) is
`Fatal` and stops instead of spinning. Once up, every bus pump holds a guard
on a "liveness bell"; if any pump ends, the channel is restarted
(`channel.died`), and a respawn-rate alarm flags churn. `channel.started` /
`channel.boot_failed` record bring-up. A broken email config never takes Matrix
or the scheduler down with it.

## Audit rows and lost rows

Every action name is a const in `channel::actions` — use them, never a string
literal. Rows carry ids, labels and hashes, **never message bodies, tokens or
transport error text**. When a row cannot be written (Postgres refused,
wedged, read-only), the writer says so on the `[audit-lost]` marker in the
daemon log. A flood is thinned per writer, channel, action and outcome: the
first 16 of a burst get a line each, then only every 2^k-th, and each line
carries the burst count; the remaining counts are said at shutdown, before the
channels stop. Lost *inbound* messages (a bus stopped with messages buffered,
a worker that still held unpolled events) are counted on
`channel.inbound_dropped`, and an event the driver could not decode on
`channel.inbound_undecodable`.

---

## Knobs

| Variable | Purpose |
|----------|---------|
| `KASTELLAN_MATRIX_HOMESERVER_URL` | **Gates the Matrix channel**: unset ⇒ no Matrix, byte-identical daemon |
| `KASTELLAN_MATRIX_USER` | Bot account (`@bot:server`), required with the URL |
| `KASTELLAN_MATRIX_PEERS` | Comma-separated room-edge allowlist enforced in the worker |
| `KASTELLAN_MATRIX_STORE` | Persistent crypto/session store |
| `KASTELLAN_MATRIX_ENFORCE_SANDBOX` | Worker seccomp + Landlock; default on, `0` is a debug opt-out |
| `KASTELLAN_MATRIX_USE_MICROVM` | Linux opt-in Firecracker mode |
| `KASTELLAN_MATRIX_WORKER_BIN` | Override worker discovery |
| `KASTELLAN_EMAIL_ENDPOINT` | **Gates the email channel** (localmail base URL) |
| `KASTELLAN_EMAIL_ADDRESS`, `_SUBSCRIPTION`, `_TOKEN_FILE`, `_AUTHSERV_ID` | Required with the endpoint; a partial set is fatal for email only |

`kastellan-cli install --matrix-homeserver-url <u> --matrix-user <@u:server>`
writes the Matrix block; the rest belong in the operator overlay
(`docs/deploy/operator-env.md`).

---

## Invariants and traps

- **No security check in a worker.** A check in `email-in` or the Matrix
  worker would let a rejected message vanish without an audit row. Put it in
  the core's pure gate.
- **Never `Debug`-print secrets.** `PeerEvidence` and `OutgoingMessage` have
  hand-written `Debug` impls that redact the token and the body; keep it that
  way for any new secret-bearing type.
- **The ask-answer arm runs whether or not asks are wired.** A command-shaped
  body must stay out of the task queue on every channel.
- **Claim where you send.** A reply is claimed by the channel's own pump just
  before `send`; claiming earlier loses replies when a queue closes.
- **Audit action strings are a committed operator interface.** Don't rename
  one; fix its doc instead.

---

## Testing

```sh
cargo test -p kastellan-core channel::                          # pure decisions + bus with fakes
cargo test -p kastellan-core --test channel_bus_e2e             # hermetic full loop
cargo test -p kastellan-core --test matrix_channel_e2e          # real driver stack, fake worker
cargo test -p kastellan-core --test email_channel_e2e --test email_mitm_e2e
cargo test -p kastellan-core --test channel_bus_pg_e2e --test reply_catch_up_pg_e2e \
  --test conversation_continuity_e2e --test scheduler_ask_path_e2e --test cli_inbox_e2e
cargo test -p kastellan-db --test pairings_e2e --test reply_claim_e2e
bash scripts/run-e2e-gate.sh pg                                 # PG tier as evidence
```

The PG-backed suites skip-as-pass without a cluster. `matrix_live_e2e` and
`matrix_firecracker_live_e2e` are `#[ignore]`d and need a real homeserver
(`KASTELLAN_MATRIX_LIVE_E2E=1` / `KASTELLAN_MATRIX_FC_LIVE_E2E=1`; see each
test's header and
[`docs/deploy/matrix-homeserver.md`](../../deploy/matrix-homeserver.md)).

---

## Where to start when changing it

- **A new inbound rule** → `bus_inbound.rs` (order matters) and a pure helper
  beside `auth`/`ingest`, with a negative test and its audit row.
- **A new channel** → implement `Channel`, reuse `polled_driver` with your own
  `PolledWorkerSpec` (decide `ack_method` deliberately — it also says whether
  the upstream redelivers), a sandboxed worker, a `*_boot.rs` under
  `ChannelSupervisor`, and an `actions` entry for each new row.
- **Reply wording or routing** → `route.rs`, `catch_up.rs`.
- **The ask message** → `ask_message.rs` (pure) and `scheduler/asks/`.
