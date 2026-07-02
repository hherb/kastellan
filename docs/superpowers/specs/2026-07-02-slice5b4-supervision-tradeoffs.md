# Slice 5b-4 — supervision-structure trade-offs (working note for the brainstorm)

> Decision input for ONE design question: how does the Matrix channel adopt the
> shared `PersistentWorker` supervisor? This note will be folded into the 5b-4
> spec once the option is picked; it is not itself the spec.

## The gap to bridge

Matrix needs **server-push semantics** on top of a **pull-only worker protocol**:

- the driver must autonomously long-poll `matrix.poll` (the worker can't push);
- queued outbound `matrix.send` replies must be **retained across a worker
  respawn** (today's `pending: VecDeque` — no reply is ever dropped);
- the login **identity JSON must be surfaced at startup** (the daemon gates the
  whole `ChannelBus` on it as login proof).

`PersistentWorker` (5b-1, `core/src/worker_lifecycle/persistent.rs`, already
DGX-verified) is strictly **caller-driven request→reply**: `handle.call(method,
params)`, no autonomous loop, no inbound stream, factory returns no identity.
Matrix's bespoke `drive()`/`supervised_self_spawn`
(`core/src/channel/matrix.rs:312-475`) duplicates its respawn/backoff/alarm
state machine — that copy-not-share drift is exactly issue **#380**.

---

## Option A — reusable `PolledWorkerDriver` layered over `PersistentHandle`  ← recommended

`PersistentWorker` stays **exactly as verified** (owns spawn / respawn /
backoff / rate-alarm). A **new channel-generic driver** component in
`core/src/channel/` sits on top and owns everything push-shaped:

- the long-poll loop (`handle.call("matrix.poll", …)`);
- flushing queued outbound sends + retaining unacked ones across respawn;
- surfacing the login identity at startup.

Matrix's bespoke `drive()`/`supervised_self_spawn` is **deleted** (closes
#380). IMAP/Telegram (Phase 2) later instantiate the same driver with
different method names.

**Pros**

- Supervisor's verified state machine untouched — zero new states to re-test
  on the security-adjacent component.
- Streaming/buffer/retry policy lives **once**, in the channel layer, where
  every anticipated future consumer (channel workers) actually lives.
- Latency semantics identical to today's matrix driver.
- Implementation cost ≈ option C plus naming a trait.

**Cons**

- Two cooperating layers (driver thread over supervisor thread).
- Driver must absorb + retry `"persistent worker is restarting"` errors during
  a respawn window (small new retry loop).
- The generic trait is still designed from ONE live example (matrix) — just at
  a much cheaper-to-change layer than the supervisor.

---

## Option B — extend `PersistentWorker` with streaming + identity hooks

The shared supervisor itself gains: factory returning `(transport, identity)`,
an optional poll hook invoked when idle, a subscriber channel for inbound
events, and a retained outbound queue (if the no-dropped-reply guarantee is
kept).

**Pros**

- One abstraction, no second layer.
- Every future long-lived worker (channel or not) gets streaming.
- Matrix channel code shrinks the most.

**Cons**

- The driver loop must multiplex caller-jobs with autonomous polls
  (`recv_timeout` alternation) — **new states on the security-adjacent
  supervisor**: respawn-during-poll, shutdown-during-poll, subscriber-dropped,
  poll-error-vs-call-error. All need fresh test rigor; the 5b/5c reviews
  leaned on this component's simplicity.
- Non-streaming consumers (kv-demo, net-demo, python-exec-style workers) need
  a "polling: off" knob — API surface only matrix exercises today.
- Buffer-overflow + retry-across-respawn policy become **shared API designed
  from one example**.
- Inherits the same one-pipe latency coupling (one JSON-RPC request in flight
  at a time) — semantically it **cannot beat option A**; improving that needs
  protocol multiplexing, a much bigger lift. The extension relocates code into
  a hotter component without buying better behavior.

---

## Option C — matrix-specific thin driver now, generalize later

Same layering as A (driver over `PersistentHandle`), but the driver stays
matrix-shaped inside `channel/matrix.rs`. Extract the generic version when
IMAP/Telegram actually arrive.

**Pros**

- Least design work now; no speculative trait.
- The later generalization is a refactor backed by TWO real consumers' tests
  (classic rule-of-three).
- Still deletes the duplicated respawn state machine (closes #380).

**Cons**

- The Phase-2 channel workers will copy-adapt matrix's driver in the meantime —
  recreating exactly the copy-not-share drift #380 complained about, one layer
  up.
- A second migration touches the **live matrix channel again** when the generic
  version is extracted (two live-channel changes instead of one).

---

## Recommendation

**A.** It deletes the duplicated respawn machinery (#380's core complaint),
keeps the verified supervisor frozen, and puts the reuse where the anticipated
future workers (channels) actually live — at roughly option C's implementation
cost. B's only unique benefit (streaming for *non-channel* workers) has no
concrete consumer and can still be added later by promoting the driver.
