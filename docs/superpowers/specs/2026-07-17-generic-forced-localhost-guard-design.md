# Generic forced-localhost guard — design (#459, slice 1)

**Date:** 2026-07-17
**Issue:** [#459](https://github.com/hherb/kastellan/issues/459)
**Builds on:** #452/#457 (per-manifest endpoint guard, `core/src/workers/endpoint_guard.rs`),
the #457 retrospective review (which found this gap), and the corrected literal-IP
carve-out fact (an operator-allowlisted **literal IP** is dialable force-routed;
only `localhost`/`*.localhost` **names** are statically dead).

## Problem

#457 guards exactly two surfaces (web-search / web-research SearxNG endpoints).
The same statically-dead class — a force-routed worker whose `Net::Allowlist`
carries an RFC 6761 `localhost`/`*.localhost` **name** — is alive everywhere
else, because each guard was a per-manifest copy:

1. **web-fetch** — a `localhost`-name `tool_allowlists` row maps to a
   `host:443` net entry; the tool registers and every fetch dies at CONNECT.
2. **browser-driver** — DB allowlist rows pass verbatim into `Net::Allowlist`;
   same exposure once its egress is force-routed.
3. **matrix channel** — `build_matrix_policy` derives
   `Net::Allowlist([homeserver_host:port])`; a `localhost`-name homeserver
   spawns, every CONNECT is range-denied, and `PersistentWorker` respawns in a
   backoff loop forever with no actionable operator message.

Per-manifest copies don't scale — that is how the gap happened. The condition
is fully derivable from data every worker already produces at resolve time.

## Operator decisions (2026-07-17)

- **Scope:** generic post-resolve check + matrix bring-up seam only. The two
  #459 residuals (resolve-time broker-binary discovery; env-flag truthiness
  unification) are **deferred** to their own slices — #459 stays open.
- **Severity is data-driven, no per-worker hooks:** every allowlist host dead
  → refuse (the tool is statically dead); a proper subset dead → warn and
  register (one dead content host ≠ a dead tool). Empty allowlist → no check
  (that is the broker/zero-egress posture, deliberately exempt).
- **Matrix:** on refusal the channel does not start; the daemon runs on
  (fail-soft, matching the registry posture for misconfigured workers).

## Design

### 1. Pure screen in `endpoint_guard.rs`

```rust
pub(crate) enum NetScreen {
    Ok,
    Warn { dead: Vec<String> },   // subset dead: register + warn
    Refuse { detail: String },    // all dead: treat as Misconfigured
}

pub(crate) fn screen_net_allowlist(
    tool: &str,
    entries: &[String],
    force_routed: bool,
) -> NetScreen
```

Rules, in order:

| condition | result |
|---|---|
| `!force_routed` or `entries.is_empty()` | `Ok` |
| no entry host is a `localhost` name | `Ok` |
| **every** entry host is a `localhost` name | `Refuse` |
| some (but not all) entry hosts are | `Warn { dead }` |

A small pure `host_of_entry(entry: &str) -> &str` strips the `:port` suffix
before classification, tolerating IPv6 bracket literals (`[::1]:443` → `[::1]`,
which classifies `false` — literals are the carve-out, never flagged). Net
entries are always `host:port` today (`allowlist_to_net_entries` strips the
wildcard leading dot; matrix formats `{host}:{port}`), and a bare `host` entry
without a port must also classify correctly. Classification reuses the existing
`host_is_localhost_name` — **no DNS at resolve time**, same rationale as #457:
anything not statically knowable belongs to the connect-time proxy check.

The `Refuse`/`Warn` message text names the tool, the dead hosts, the
force-routing cause, and the generic remedy: *use a literal IP the operator
allowlists (the egress proxy's allowlisted-literal carve-out dials it) or a
routable hostname — and update the corresponding `tool_allowlists` row /
endpoint env var to match.*

### 2. Wiring in `assemble_registry` (`registry_build.rs`)

In the `Resolution::Register(entry)` arm, before inserting:

```text
force_routed = egress_will_force_route(entry_is_vm(&entry), ctx.get_env)
match entry.policy.net {
    Net::Allowlist(entries) => screen_net_allowlist(name, entries, force_routed),
    _ => Ok,   // Deny / ProxyEgress: nothing to screen
}
```

- `Refuse { detail }` → `tracing::error!` + skip, byte-identical treatment to
  today's `Misconfigured` arm: not registered, not advertised to the planner,
  no `LoadedToolRecord`, daemon still starts.
- `Warn { dead }` → one `tracing::warn!` naming the dead hosts; the tool
  registers normally.
- `entry_is_vm` is a tiny cfg-gated helper: on Linux,
  `entry.sandbox_backend == Some(SandboxBackendKind::FirecrackerVm)`; on other
  platforms `false` (the variant is Linux-cfg-gated and no non-Linux manifest
  produces it). VM workers are treated as always-force-routed — same posture
  as `egress_will_force_route`'s `is_microvm` arm (`plan.rs` fail-closed
  refuses a `Net::Allowlist` VM without a proxy, so no direct route ever
  exists in VM mode).

`assemble_registry` stays pure (env via `ctx.get_env`, no `std::env`), so the
whole path is unit-testable with fakes.

### 3. Matrix bring-up seam (`main/matrix_boot.rs`)

`spawn_matrix_channel` already receives the parsed homeserver config and
`force_routing: &Option<Arc<ForceRoutingConfig>>`, and already documents
fail-soft ("channel not started") for an unreachable homeserver. Add, after
config parse and before any spawn:

- `force_routed = force_routing.is_some() || cfg.use_microvm` (the parsed
  config's `use_microvm` field, from `KASTELLAN_MATRIX_USE_MICROVM=1`, Linux
  only — the VM arm is always effectively forced, same `plan.rs` guarantee as
  above).
- If `force_routed` and the homeserver host classifies as a `localhost` name
  (`host_is_localhost_name`), `tracing::error!` one actionable message (same
  family as the registry guard, plus the matrix-specific remedy naming
  `KASTELLAN_MATRIX_HOMESERVER`) and return without starting the channel. No
  worker spawn, no respawn loop, daemon unaffected.

The check itself is a pure helper beside the policy builders so it unit-tests
without a daemon.

### 4. De-duplication with the #457 guards

- The per-manifest **refusals** (web-search direct-mode endpoint, web-research
  endpoint) fire inside `resolve()` and return `Misconfigured` before any
  `Register` exists — the generic screen never sees those configs, so no
  double-report. They keep their precise per-worker remedies.
- web-research's `content_localhost_warnings` (added in the #457 retro pass)
  becomes semantically identical to the generic subset-warn → **removed in
  this slice** (its tests migrate to the generic screen's).
- web-research's **embed** warning stays: it explains the hybrid→lexical
  degradation consequence, which the generic message cannot know. If the embed
  host also appears in the union net allowlist, one duplicate warn is accepted
  — complementary messages, same fix.

### 5. Testing (TDD, red → green)

- **`screen_net_allowlist` unit tests:** empty list; force-routing off; no dead
  hosts; subset dead; all dead; port stripping incl. IPv6 brackets and a bare
  no-port entry; literal loopback IPs never flagged (carve-out pin);
  `*.localhost` and FQDN-trailing-dot forms.
- **`assemble_registry` tests (fake manifests):** all-dead allowlist +
  force-route env → skipped exactly like `Misconfigured` (not registered, no
  record, no doc); subset-dead → registered + record present; force-routing
  off → byte-identical to today (existing tests stay green as the regression
  pin).
- **Matrix guard unit tests** beside the pure helper; a bring-up-level test if
  the existing `matrix_boot` test seam supports it cheaply.
- **web-research:** drop the `content_localhost_warnings` tests with the
  function; assert the manifest still warns for the embed case.
- **Gates:** everything above is Mac-verifiable (Seatbelt). DGX targeted gate
  for the cfg(linux) `entry_is_vm` arm + the usual full-workspace
  `cargo test`/clippy vs the **2545/0/46** baseline at session end.

## Out of scope (deferred, #459 stays open)

- Resolve-time broker-binary discovery (registered-but-dead broker configs).
- Truthiness unification (`USE_*` flags accepting `1|true|yes|on`) — behavior
  change to shipped flags, needs its own slice + test updates.
- Any DNS-based classification (connect-time proxy owns it, by design).
