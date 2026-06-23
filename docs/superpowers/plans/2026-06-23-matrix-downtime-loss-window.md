# Matrix downtime-loss window (#321) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the Matrix worker from silently dropping messages a user sends while it is down/respawning, by surfacing the incremental catch-up sync on restart instead of suppressing it.

**Architecture:** matrix-sdk already persists a sync token in its SQLite state store; on a restart `sync_once` resumes from it, so the catch-up sync returns only events received since the last run (the downtime backlog). The `live` gate in `sdk_live.rs` currently suppresses those along with genuine full-history replay. We read the persisted token *before* the initial sync and seed the `live` flag from a pure decision: token present (restart) → live from the start; no token (fresh login) → suppress backlog then go live. No new persistence.

**Tech Stack:** Rust, matrix-sdk 0.18 (`live-matrix` feature), tokio, `std::sync::atomic`.

## Global Constraints

- AGPL-3.0 project; AGPL-compatible deps only. No new dependency is added by this plan.
- Cross-platform Linux + macOS; this change is OS-agnostic (`sdk_live.rs` is `live-matrix`-gated, identical on both).
- Rust core; no in-process Python.
- Pure functions in reusable modules preferred (rule #1); TDD (rule #2); junior-readable inline docs mandatory (rule #3); files ≤500 LOC where feasible (rule #4).
- All tests pass before committing (rule #6).
- The module `workers/matrix/src/sdk_live.rs` compiles only under `--features live-matrix`; all `cargo` commands below pass `-p kastellan-worker-matrix --features live-matrix`.
- Fail-soft rule for this change: any inability to read the token maps to `None` ("fresh / suppress"), which can never cause a history replay.

---

### Task 1: Pure `initial_live_state` decision + unit tests

**Files:**
- Modify: `workers/matrix/src/sdk_live.rs` (add the pure fn near `drain`, and tests in the existing `mod tests`)

**Interfaces:**
- Consumes: nothing (pure, std-only).
- Produces: `fn initial_live_state(prior_sync_token: Option<&str>) -> bool` — `true` when a prior sync token exists (restart: surface the incremental catch-up backlog), `false` otherwise (fresh login: suppress the full-history replay). Used by Task 2's wiring in `connect_client`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `workers/matrix/src/sdk_live.rs`:

```rust
    #[test]
    fn initial_live_true_when_prior_token_present() {
        // A persisted sync token means this is a restart: the catch-up sync is
        // incremental (only events since the last run), so we must surface them.
        assert!(initial_live_state(Some("s12_34_56")));
    }

    #[test]
    fn initial_live_false_when_no_prior_token() {
        // No token means a fresh login: the catch-up sync replays room history,
        // which must stay suppressed.
        assert!(!initial_live_state(None));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-matrix --features live-matrix initial_live`
Expected: FAIL to compile — `cannot find function initial_live_state`.

- [ ] **Step 3: Write the minimal implementation**

Add this function just above `fn drain(` in `workers/matrix/src/sdk_live.rs`:

```rust
/// Decide whether inbound delivery should be live from the very start of the
/// initial sync, given the sync token (if any) the SDK persisted on a previous
/// run.
///
/// matrix-sdk stores its sync token in the SQLite state store and resumes from
/// it on restart, so when a prior token exists the catch-up sync returns only
/// events received *since* that token — genuinely-unprocessed messages,
/// including any a user sent while the worker was down. Those must be surfaced,
/// so we start live (`true`).
///
/// With no prior token this is a fresh login, whose catch-up sync replays recent
/// room history; that must stay suppressed, so we start not-live (`false`) and
/// flip live only after the initial sync drains (see `connect_client`).
fn initial_live_state(prior_sync_token: Option<&str>) -> bool {
    prior_sync_token.is_some()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-matrix --features live-matrix initial_live`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add workers/matrix/src/sdk_live.rs
git commit -m "feat(matrix): pure initial_live_state decision for restart backlog (#321)"
```

---

### Task 2: Read the persisted sync token and wire it into `connect_client`

**Files:**
- Modify: `workers/matrix/src/sdk_live.rs` (imports; new `read_prior_sync_token`; the `live` seed in `connect_client`)

**Interfaces:**
- Consumes: `initial_live_state(Option<&str>) -> bool` from Task 1.
- Produces: `async fn read_prior_sync_token(client: &Client) -> Option<String>` — reads the SDK's persisted sync token from the state store, fail-soft to `None`. Used only inside `connect_client`.

- [ ] **Step 1: Add the store-data imports**

In the `use matrix_sdk::...` block near the top of `workers/matrix/src/sdk_live.rs`, add:

```rust
use matrix_sdk::store::{StateStoreDataKey, StateStoreExt as _};
```

Note: `StateStoreExt` is the re-exported extension trait; `get_kv_data` is on the
base `StateStore` trait, callable on `&DynStateStore` returned by
`client.state_store()`. If the compiler reports `get_kv_data` unresolved without
a different trait in scope, replace `StateStoreExt as _` with `StateStore as _`
(`matrix_sdk::store::StateStore`) — both are re-exported from `matrix_sdk::store`.

- [ ] **Step 2: Add the fail-soft reader**

Add this function just below `initial_live_state` in `workers/matrix/src/sdk_live.rs`:

```rust
/// Read the sync token matrix-sdk persisted on a previous run, if any.
///
/// `Client::sync_token()` is `pub(crate)` in matrix-sdk 0.18, so we read the
/// same value the SDK stores via the public state-store key. **Fail-soft:** a
/// store-read error (or an absent value) yields `None`, which routes
/// [`initial_live_state`] to "fresh / suppress". A read failure can therefore
/// never cause a stale-history replay — at worst it re-drops a downtime window,
/// which is exactly the pre-#321 behavior.
async fn read_prior_sync_token(client: &Client) -> Option<String> {
    client
        .state_store()
        .get_kv_data(StateStoreDataKey::SyncToken)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.into_sync_token())
}
```

- [ ] **Step 3: Seed `live` from the token in `connect_client`**

In `connect_client`, replace the `live` initialization (currently the block at
lines ~313–319 that ends with `let live = Arc::new(AtomicBool::new(false));`):

```rust
    // Gate inbound delivery on `live`: false during the initial catch-up sync so
    // its backlog (room history, and any messages received while the worker was
    // down/restarting) is consumed *silently* — only events from the continuous
    // sync afterwards reach the buffer. Without this, every (re)start replays the
    // whole room history as fresh inbound events.
    let live = Arc::new(AtomicBool::new(false));
```

with:

```rust
    // Gate inbound delivery on `live`. On a *fresh login* the catch-up sync
    // replays recent room history, which must be suppressed (false until the
    // initial sync drains). On a *restart*, the SDK resumes from its persisted
    // sync token, so the catch-up sync returns only events received since the
    // last run — including any a user sent while the worker was down. We seed
    // `live` from whether that prior token exists, so the restart backlog is
    // surfaced instead of dropped (#321). The token is read before the handler is
    // registered so the decision covers the entire initial sync.
    let prior_sync_token = read_prior_sync_token(&client).await;
    let live = Arc::new(AtomicBool::new(initial_live_state(prior_sync_token.as_deref())));
```

Leave the post-`sync_once` `live.store(true, Ordering::SeqCst);` line unchanged —
it is a no-op when already `true` and still flips the fresh-login case live after
its backlog drains.

- [ ] **Step 4: Build under the live feature to verify it compiles**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-matrix --features live-matrix`
Expected: builds clean. (If `get_kv_data` is unresolved, apply the trait-import
fallback noted in Step 1 and rebuild.)

- [ ] **Step 5: Run the worker's full test suite (both feature configs)**

Run:
```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-matrix
cargo test -p kastellan-worker-matrix --features live-matrix
cargo clippy -p kastellan-worker-matrix --all-targets --features live-matrix -- -D warnings
```
Expected: default suite green; `live-matrix` suite green (includes the 2 new
`initial_live_*` tests); clippy clean.

- [ ] **Step 6: Commit**

```bash
git add workers/matrix/src/sdk_live.rs
git commit -m "feat(matrix): surface restart catch-up backlog via persisted sync token (#321)"
```

---

### Task 3: Update the stale `supervised` doc comment

**Files:**
- Modify: `core/src/channel/matrix.rs:176-180`

**Interfaces:**
- Consumes: nothing.
- Produces: nothing (documentation only).

- [ ] **Step 1: Replace the limitation note**

In `core/src/channel/matrix.rs`, replace the doc lines currently reading:

```rust
    /// respawn (no dropped replies). Note that *inbound* messages that arrive
    /// during the downtime are NOT recovered: the respawned worker's catch-up sync
    /// is consumed silently (it only surfaces events from the continuous sync
    /// afterwards), so a message sent to the bot while it was down is lost. Closing
    /// that window needs a sync-token watermark — tracked as issue #321.
```

with:

```rust
    /// respawn (no dropped replies). Inbound messages a user sends during the
    /// downtime are recovered on restart (#321): the respawned worker resumes
    /// from the SDK's persisted sync token, so its catch-up sync surfaces the
    /// messages received while it was down rather than dropping them. Only a
    /// *fresh login* (no prior token) still suppresses its catch-up backlog, to
    /// avoid replaying the whole room history.
```

- [ ] **Step 2: Verify core still builds**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core`
Expected: builds clean (doc-only change).

- [ ] **Step 3: Commit**

```bash
git add core/src/channel/matrix.rs
git commit -m "docs(matrix): supervised note reflects #321 sync-token recovery"
```

---

### Task 4: Live restart proof + handover (DGX-gated; document if harness can't extend cleanly)

**Files:**
- Modify (if feasible): `core/tests/matrix_live_e2e.rs` (add an `#[ignore]` restart scenario)
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

**Interfaces:**
- Consumes: the behavior from Tasks 1–3.
- Produces: nothing consumed by code; a verification artifact + handover update.

- [ ] **Step 1: Assess the live e2e harness**

Read `core/tests/matrix_live_e2e.rs`. Determine whether a restart scenario
(start bot worker → confirm baseline poll → stop bot worker → peer sends a
uniquely-tagged message → restart bot worker against the SAME store dir → assert
the bot surfaces the tagged message via `matrix.poll`) can be added without
restructuring the existing single round-trip. The bot must reuse its persistent
`KASTELLAN_MATRIX_STORE` across the stop/start so the sync token survives.

- [ ] **Step 2a: If feasible — add the `#[ignore]` restart test**

Add a second `#[ignore]` test mirroring the existing one's setup but with the
stop/send/restart sequence above. Keep it skip-as-pass without the
`KASTELLAN_MATRIX_LIVE_E2E=1` opt-in and homeserver env, matching the existing
test. Run it on the DGX per the file's header recipe and confirm PASS.

- [ ] **Step 2b: If NOT cleanly feasible — document the manual DGX check**

If extending the harness would require restructuring it, do not force it. Record
in the HANDOVER the manual verification performed on the DGX instead: with the
live channel running, stop the matrix worker, DM the bot from `@horst`, restart
the worker, and confirm the bot replies to the message sent during the downtime
(and that a fresh-login store-wipe still does NOT replay history). State which
path (2a or 2b) was taken.

- [ ] **Step 3: Update HANDOVER.md and ROADMAP.md**

Move #321 from "Next TODO" to "Recently completed (this session)" with: the
sync-token-gated fix, the pure `initial_live_state` seam, the fail-soft reader,
the doc-comment update, the test-count delta (+2 `live-matrix` units), and which
verification path was taken. Remove #321 from the open-follow-ups / Matrix
residuals lists. Re-state the next TODO options. Keep both docs concise.

- [ ] **Step 4: Commit**

```bash
git add core/tests/matrix_live_e2e.rs docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "test(matrix)+docs: live restart proof + handover for #321"
```

(If Task 2a was not taken, drop `core/tests/matrix_live_e2e.rs` from the `git add`.)

---

## Self-Review

**Spec coverage:**
- Pure `initial_live_state` → Task 1. ✓
- Fail-soft `read_prior_sync_token` via the public store key → Task 2. ✓
- Wiring in `connect_client` (read before handler registration; post-sync store stays) → Task 2. ✓
- `supervised` doc-comment update → Task 3. ✓
- Live restart proof (extend e2e if feasible, else documented manual DGX check) → Task 4. ✓
- Rejected alternatives / API path are background, no task needed. ✓

**Placeholder scan:** No TBD/TODO; every code step shows the exact code; commands have expected output. The only conditional is Task 4's 2a/2b branch, which is explicit and decidable at execution time. ✓

**Type consistency:** `initial_live_state(Option<&str>) -> bool` and `read_prior_sync_token(&Client) -> Option<String>` are used consistently across Tasks 1–2 (`prior_sync_token.as_deref()` bridges `Option<String>` → `Option<&str>`). ✓
