//! DGX real-KVM live e2e for slice 5b-4b: the Matrix worker runs in a Firecracker
//! VM, force-routed through a host egress sidecar, with its E2E crypto/session store
//! on a persistent ext4 image at `/data`. Proves (1) VM-mode login + a real
//! send/recv round-trip against the live homeserver, and (2) the #321 downtime
//! recovery composed with a genuine fresh-VM respawn: kill the VMM
//! (`pkill -f kastellan-microvm-run`), `PersistentWorker` respawns a fresh VM +
//! sidecar, and the message sent while the bot was down is recovered from the sync
//! token persisted on `/data`.
//!
//! ALL tests are `#[ignore]` + skip-as-pass. This file is a **skeleton** committed by
//! slice 5b-4b (Tasks 1-6): the two `todo!()` bodies are filled in ON THE DGX during
//! live bring-up (Task 8), mirroring `core/tests/matrix_live_e2e.rs` almost verbatim
//! (copy its `required_env()`, the poll loop, and the `PeerId`/`OutgoingMessage` send
//! shape). Keep the bodies self-contained — do NOT depend on `matrix_live_e2e.rs`.
//!
//! Opt in on the DGX with:
//! ```text
//!   export KASTELLAN_MATRIX_FC_LIVE_E2E=1
//!   export PATH=$HOME/.local/bin:$PATH        # firecracker on PATH
//!   # build the release launcher + rootfs first (stale-launcher gotcha):
//!   cargo build --release -p kastellan-microvm-run -p kastellan-microvm-init \
//!               -p kastellan-worker-matrix --features live-matrix
//!   ./scripts/workers/microvm/build-matrix-rootfs.sh
//!   # required live env (same as matrix_live_e2e):
//!   #   KASTELLAN_MATRIX_HOMESERVER_URL / _USER / _PASSWORD / _PEER_USER /
//!   #   _PEER_PASSWORD / _ROOM
//!   cargo test -p kastellan-core --test matrix_firecracker_live_e2e -- --ignored --nocapture
//! ```
//!
//! On macOS the whole file is excluded (`#![cfg(target_os = "linux")]`) — it compiles
//! to an empty test binary, matching `python_exec_firecracker_e2e.rs`.
#![cfg(target_os = "linux")]

/// Opt-in gate. Set to `1` on the DGX to run the ignored VM-mode live tests.
const GATE: &str = "KASTELLAN_MATRIX_FC_LIVE_E2E";

/// Skip-guard (skeleton). The DGX bring-up (Task 8) extends this to also probe
/// `LinuxFirecracker` readiness on the matrix image and prepend the release launcher
/// to `$PATH` — see `python_exec_firecracker_e2e.rs::skip_if_no_microvm` for the
/// pattern to lift. Returns `false` (skip-as-pass) with an `eprintln!` when a
/// precondition is missing.
fn ready() -> bool {
    if std::env::var(GATE).ok().as_deref() != Some("1") {
        eprintln!("[SKIP] {GATE} != 1");
        return false;
    }
    // DGX (Task 8): add the FirecrackerVm probe on `matrix.ext4` + launcher-on-PATH
    // check here (skip-as-pass, not fail, when firecracker/KVM/rootfs are absent).
    true
}

#[test]
#[ignore = "live: DGX KVM + conduwuit + two bot accounts in a shared encrypted room"]
fn matrix_vm_send_recv_round_trip() {
    if !ready() {
        return;
    }
    // DGX (Task 8): mirror `matrix_live_e2e::matrix_send_recv_round_trip`, but the bot
    // goes through `spawn_matrix_worker` with a `MatrixSpawnConfig { use_microvm: true,
    // .. }` + the resolved `FirecrackerVm` worker backend + a real `MatrixEgress`
    // (host-bwrap sidecar). Build `matrix.ext4` first (see the module doc). Steps:
    //   1. spawn bot (VM) + peer, `matrix.init` both.
    //   2. peer `matrix.send { conversation: room, body }`.
    //   3. bot polls `matrix.poll { timeout_ms: 2000 }` up to 45s; assert body seen.
    todo!("fill on the DGX — see the module doc and matrix_live_e2e.rs for the shape");
}

#[test]
#[ignore = "live: DGX KVM + conduwuit + two bot accounts in a shared encrypted room"]
fn matrix_vm_restart_recovers_downtime_message() {
    if !ready() {
        return;
    }
    // DGX (Task 8): combine `matrix_live_e2e::matrix_restart_recovers_downtime_message`
    // (the #321 shape) with a genuine fresh-VM respawn. Steps:
    //   1. store lives on `matrix-state.ext4` in the microvm dir (mkfs'd on first spawn).
    //   2. spawn bot (VM) via `spawn_matrix_worker`; `matrix.init` persists session.json
    //      + the sync token onto the `/data` ext4 image.
    //   3. peer sends body = format!("kastellan-fc-live-restart-{}", process::id())
    //      while the bot VM is killed: `pkill -f kastellan-microvm-run` (use `-f`; the
    //      15-char `comm` truncation gotcha).
    //   4. `PersistentWorker` respawns a FRESH VM + sidecar against the same
    //      `matrix-state.ext4`; poll up to 45s; assert the downtime body surfaces.
    todo!("fill on the DGX — see the module doc and matrix_live_e2e.rs for the shape");
}
