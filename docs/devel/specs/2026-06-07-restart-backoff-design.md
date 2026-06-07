# Option K — cross-platform exponential restart backoff

**Date:** 2026-06-07
**ROADMAP item:** Option K (ROADMAP:61)
**Status:** approved design, pending implementation

## Problem

Every keep-alive `ServiceSpec` today restarts on a constant 5 s delay
(`Restart=on-failure RestartSec=5` on systemd; `KeepAlive=true` on launchd).
A daemon stuck in a crash loop therefore respawns every 5 s forever, hammering
the host (and the logs) with no ramp-down. systemd 252+ can ramp the restart
delay geometrically; launchd cannot.

## Goal

Give a `ServiceSpec` an optional, operator-tunable exponential restart backoff,
wire it through the systemd backend, and degrade honestly on launchd. Wire a
sensible curve into the two long-running daemon specs so the capability is
actually used.

## Non-goals

- No new backoff knob exposed via CLI or env (specs are constructed in code).
- No attempt to emulate exponential backoff on launchd (it has no equivalent —
  see the macOS section). `ThrottleInterval` is a constant *floor*, not a ramp,
  so mapping onto it would be semantically lossy; we deliberately don't.
- The initial delay stays the existing `RestartSec=5`; it is **not** made
  configurable (YAGNI — no service needs a different start today).

## Design

### New type — `RestartBackoff` (`supervisor/src/lib.rs`)

```rust
/// Operator-tunable exponential restart backoff for a keep-alive service.
///
/// Only meaningful when `ServiceSpec.keep_alive == true`. The ramp starts
/// from the existing initial delay (systemd `RestartSec=5`) and grows
/// geometrically to `max_delay_sec` over `steps` steps. Ignored-with-warning
/// on launchd (no equivalent knob — see `launchd_agents::install`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartBackoff {
    /// The maximum delay (seconds) the ramp climbs to. Maps to systemd
    /// `RestartMaxDelaySec=`.
    pub max_delay_sec: u32,
    /// Number of steps over which the delay grows from the initial
    /// `RestartSec` to `max_delay_sec`. Maps to systemd `RestartSteps=`.
    pub steps: u32,
}
```

### `ServiceSpec` field (`supervisor/src/lib.rs`)

```rust
/// Optional exponential restart backoff. `None` (the default) preserves
/// today's constant-5s behaviour byte-for-byte. Only honoured when
/// `keep_alive == true`. systemd ramps `RestartSec` → `max_delay_sec` over
/// `steps`; launchd ignores it with an install-time warning.
#[serde(default)]
pub restart_backoff: Option<RestartBackoff>,
```

Additive, `#[serde(default)]` — an old serialised spec without the field still
deserialises (same pattern as `after` / `part_of`).

### systemd backend (`supervisor/src/systemd_user/builder.rs`)

Inside the existing `if spec.keep_alive { … }` block, after the current
`RestartSec=5` line, when `restart_backoff` is `Some(b)` emit:

```ini
RestartSteps=<b.steps>
RestartMaxDelaySec=<b.max_delay_sec>
```

- Backoff lines appear **only** under `keep_alive` — a spec with
  `keep_alive == false` emits no restart directives at all, backoff or not.
- Doc-note: `RestartSteps` / `RestartMaxDelaySec` require systemd 252+. Older
  systemd logs an "unknown directive" warning at load but still starts the unit
  (non-fatal degrade), so emitting them unconditionally when requested is safe.

### macOS backend (`supervisor/src/launchd_agents.rs::install`)

launchd has no operator-controllable exponential backoff. When
`spec.restart_backoff.is_some()`, emit exactly one `tracing::warn!` at install
time, carrying the service name as a structured field:

> `service = <name>` — restart_backoff requested but launchd has no equivalent;
> falling back to KeepAlive default

The plist is written **unchanged** — `build_plist` is not modified. This mirrors
the existing "`after` / `part_of` ignored on launchd" precedent: the field is
honoured where the OS supports it and degrades with a visible warning where it
does not.

### Canonical specs (`supervisor/src/specs.rs`)

Both long-running daemon specs gain a real curve so the capability is exercised
rather than dead:

```rust
restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
```

A crash-looping daemon then ramps 5 s → ~5 min instead of hammering every 5 s.
The existing pinned tests still pass: they assert `Restart=on-failure` and
`RestartSec=5` are *present*, and adding two more lines does not break a
substring assertion.

## Testing (TDD — tests written first)

**systemd builder (`systemd_user/builder.rs`):**
- backoff `Some` → output contains `RestartSteps=8` and `RestartMaxDelaySec=300`,
  in that order, after `RestartSec=5`.
- backoff `None` → output contains neither directive (today's shape preserved).
- `keep_alive == false` with backoff `Some` → no `Restart*` lines at all
  (backoff is inert without keep-alive).

**launchd builder (`launchd_agents/builders.rs`):**
- `build_plist` output is byte-identical with vs. without `restart_backoff`
  (the plist never carries a backoff).

**canonical specs (`specs.rs`):**
- `core_service_spec` and `postgres_service_spec` carry
  `RestartBackoff { max_delay_sec: 300, steps: 8 }` (pins the chosen curve so a
  regression can't silently drop or change it).

**serde (`lib.rs`):**
- a spec JSON omitting `restart_backoff` deserialises to `None` (extends the
  existing `service_spec_ordering_fields_default_when_absent` test).
- round-trip serialise→deserialise of a spec carrying a backoff preserves it.

## File-size watch

`systemd_user/builder.rs` is at 478 LOC. The new inline tests may cross the
500-LOC cap (rule 4); if so, lift the builder's `#[cfg(test)] mod tests` block
to a sibling `systemd_user/builder/tests.rs` (same precedent as
`systemd_user/tests.rs`), keeping the production region byte-identical.

## Cross-platform / invariant notes

- Honours CLAUDE.md "no OS-only code without a counterpart of equivalent
  guarantee": the field is additive and degrades gracefully; the *guarantee* is
  unequal (launchd can't ramp), and that inequality is surfaced via a warning
  rather than hidden — matching the documented `after`/`part_of` precedent.
- No security-boundary surface touched: this is restart-timing config only.
