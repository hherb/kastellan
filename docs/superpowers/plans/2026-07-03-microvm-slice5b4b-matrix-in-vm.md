# Firecracker micro-VM slice 5b-4b — Matrix-in-a-VM Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the live Matrix channel worker inside a Firecracker micro-VM on Linux — long-lived, respawn-supervised by the shared `PersistentWorker`, reaching the homeserver only through a per-worker transparent-tunnel egress sidecar, with its E2E crypto/session store on a `persistent_store` ext4 image that survives VM respawns.

**Architecture:** This is the VM-platform half of slice 5b-4 (the channel restructure, 5b-4a, already merged as `8f5ec97`). It composes four existing mechanisms with zero new IPC: the shared `PersistentWorker` (5b-1), `persistent_store` ext4 images (5b-2), `spawn_net_transport` transparent-tunnel net-in-VM (5c), and VMM confinement (5a). The novel surface is small: a matrix rootfs script (baked worker + OS CA bundle), a guest-loopback-up step in `microvm-init` (the in-guest `ProxyBridge` binds `127.0.0.1`), a VM-mode Matrix `SandboxPolicy` (empty `fs_read`, `persistent_store` at `/data`, microvm env), and an opt-in `KASTELLAN_MATRIX_USE_MICROVM=1` switch that resolves the **worker** backend to `FirecrackerVm` while the **sidecar** backend stays host bwrap (the 5c invariant — the egress proxy is the real-network boundary and needs a real route).

**Tech Stack:** Rust (workspace, edition 2021, rustc 1.96); `kastellan-worker-matrix` (`--features live-matrix`), `kastellan-microvm-init` (guest PID1, Linux-only libc), `kastellan-sandbox` (`LinuxFirecracker` + `PersistentStore`), `core` egress (`spawn_net_transport`), `matrix-rust-sdk 0.18` (validates homeserver TLS against the system trust store — the load-bearing reason the rootfs bakes a CA bundle). Firecracker/KVM/vsock. Bash for the rootfs builder.

## Global Constraints

- **AGPL-compatible deps only** (Apache-2.0/MIT/BSD/MPL/LGPL/(A)GPL). This plan adds **no new third-party crate** — it reuses the existing `libc`, `matrix-sdk`, `kastellan-sandbox`, and egress stack.
- **Cross-platform first-class.** VM-specific code is `#[cfg(target_os = "linux")]`; every reusable abstraction (config parsing, the VM policy builder) compiles and unit-tests on both OSes. macOS keeps the 5b-4a Seatbelt (+ sidecar) path byte-identical. Non-VM Linux (flag unset) keeps the 5b-4a bwrap path byte-identical.
- **Rust core, Python only inside workers.** The matrix worker is Rust; no Python enters this slice.
- **Every worker sandboxed before it runs.** No unsandboxed spawn. In-VM, force-routing is mandatory (`Net::Allowlist` without `proxy_uds` is already rejected fail-closed by `build_launch_plan` — no virtio-net device exists). The sidecar is spawned sidecar-first fail-closed.
- **Additive & byte-identical off the new path.** The flag-unset (bwrap/Seatbelt) path is untouched. `persistent_store: None` / non-VM callers stay byte-identical.
- **Fail-closed backend switch.** `KASTELLAN_MATRIX_USE_MICROVM=1` set but `LinuxFirecracker` not ready ⇒ refuse to spawn the channel (no silent bwrap fallback), matching the microvm convention.
- **Files under 500 LOC where feasible.** `core/src/channel/matrix.rs` is at 519 LOC today; keep additions minimal and lift the new pure VM policy builder into the same file only if it stays under ~560, else a `channel/matrix/vm_policy.rs` sibling (the tests already live in `channel/matrix/tests.rs`).
- **TDD; all tests pass before commit.** RED→GREEN→commit each task.
- **Build env:** `source "$HOME/.cargo/env"` before every cargo command. Dev/CI rustc is **1.96**.
- **Linux-cfg verification on the Mac:** `core` cannot cross-`cargo test`/`clippy` for Linux (its `ring` C-dep — the #144 wall). Cross-platform additions (config parse, VM policy builder) are Mac-testable directly. The `#[cfg(target_os = "linux")]` daemon backend switch (`main.rs`), the `microvm-init` loopback step, and the rootfs script are **DGX-only** to run; verify `kastellan-microvm-init` on the Mac with cross-clippy (`cargo clippy -p kastellan-microvm-init --target aarch64-unknown-linux-gnu --all-targets`) where the linker allows (pure-Rust + libc). The full `cargo test --workspace` + the VM e2e run on the DGX.
- **FC e2e gotchas (DGX):** rebuild the **release** launcher (`cargo build --release -p kastellan-microvm-run`) AND `matrix.ext4` (`build-matrix-rootfs.sh` — the init is baked in) before the e2e; `export PATH=$HOME/.local/bin:$PATH` (firecracker is off the non-interactive ssh PATH → e2e silently SKIP-as-passes otherwise). Drive the DGX as exactly `ssh dgx '<cmd>'`.
- **Commit trailer** on every commit: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
- **Branch:** all work on `feat/microvm-slice5b4b-matrix-in-vm` (create off `main`@`8f5ec97`).

## File Structure

- **Create** `scripts/workers/microvm/build-matrix-rootfs.sh` — the matrix VM rootfs: `kastellan-worker-matrix --release --features live-matrix` + `ldd` closure + `kastellan-microvm-init` + **the OS CA trust store baked in** + `/run` + `/data` (persistent-store mountpoint) + share anchors; `ROOTFS_MIB=512`; emits `matrix.ext4` beside the shared `vmlinux`.
- **Modify** `workers/microvm-init/src/main.rs` — add pure `pack_ifname` + `bring_loopback_up()` (SIOCSIFFLAGS IFF_UP on `lo`), call it unconditionally from `main` after `mount_pseudo_fs()`.
- **Modify** `core/src/channel/matrix.rs` — add `use_microvm: bool` to `MatrixSpawnConfig`; read `KASTELLAN_MATRIX_USE_MICROVM` + optional `KASTELLAN_MATRIX_PASSWORD` in `parse_daemon_spawn_config`; add pure `build_matrix_vm_policy`; branch `spawn_matrix_worker` on `use_microvm` (VM program path, VM policy, transient per-spawn password file under a `/tmp` RO-share).
- **Modify** `core/src/channel/matrix/tests.rs` — unit tests for the new config field, VM policy builder, and password-path derivation.
- **Modify** `core/src/main.rs` — daemon wiring: when `spawn_cfg.use_microvm` (Linux), resolve the worker backend to `sandboxes.firecracker`; the `MatrixEgress.sidecar_backend` stays `sandboxes.bwrap`.
- **Create** `core/tests/matrix_firecracker_live_e2e.rs` — DGX real-KVM `#[ignore]`: VM-mode round-trip + `pkill -f kastellan-microvm-run` respawn + #321 downtime recovery across a fresh VM + persistent store.
- **Modify** `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — tick 5b-4b, record the DGX gate.

---

### Task 1: `build-matrix-rootfs.sh` — the matrix VM rootfs (baked worker + OS CA bundle)

**Files:**
- Create: `scripts/workers/microvm/build-matrix-rootfs.sh`
- Test: `bash -n` syntax check + `grep` assertions (shell script; verified end-to-end by the Task 7 DGX e2e, per the net-demo/kv-demo precedent which have no unit tests)

**Interfaces:**
- Consumes: `cargo build --release -p kastellan-worker-matrix --features live-matrix -p kastellan-microvm-init`; the shared `$OUT_DIR/vmlinux`.
- Produces: `$OUT_DIR/matrix.ext4` (default `$OUT_DIR=/var/lib/kastellan/microvm`) with the worker at `/usr/local/bin/kastellan-worker-matrix`, init at `/sbin/init`, the OS CA store under `/etc/ssl` + `/usr/share/ca-certificates`, and the `/data` + `/run` + share-anchor mountpoints.

- [ ] **Step 1: Write the rootfs builder**

Create `scripts/workers/microvm/build-matrix-rootfs.sh` (mirrors `build-kv-demo-rootfs.sh`/`build-net-demo-rootfs.sh` with the three 5b-4b.1 deltas: `--features live-matrix`, baked CA bundle, `ROOTFS_MIB=512`, `/data` anchor):

```bash
#!/usr/bin/env bash
# Build the matrix micro-VM rootfs (ext4) beside the shared vmlinux. The matrix
# worker is a LONG-LIVED Net::Allowlist worker that does its OWN end-to-end TLS to
# the homeserver through the egress proxy (transparent tunnel). Unlike net-demo,
# matrix-sdk 0.18 validates that TLS against the SYSTEM trust store, so this rootfs
# BAKES the OS CA bundle (/etc, /usr are not share anchors — it cannot ride
# fs_read). /run is the egress-relay mountpoint (slice 4a); /data is the persistent
# crypto/session store mountpoint (slice 5b-2).
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash: ./scripts/workers/microvm/build-matrix-rootfs.sh" >&2; exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *) echo "Unsupported arch '${HOST_ARCH}'." >&2; exit 1 ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
# matrix-sdk + tokio + rustls/ring + bundled-sqlite closure — net-demo's 128 is far
# too small. 512 MiB leaves headroom for the worker's read-only rootfs.
ROOTFS_MIB=512

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write micro-VM dir: $OUT_DIR — run sudo ./scripts/linux/install-firecracker-vsock.sh or set KASTELLAN_MICROVM_DIR." >&2
    exit 1
fi
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

source "$HOME/.cargo/env"
# bundled-sqlite ⇒ no host libsqlite3 needed; rustls-tls ⇒ no host OpenSSL needed.
cargo build --release -p kastellan-worker-matrix --features live-matrix -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-matrix "$WORK/usr/local/bin/kastellan-worker-matrix"

copy_lib_closure() {
    for obj in "$@"; do
        ldd "$obj" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        install -D -m0755 "$lib" "$WORK$lib"
    done
}
copy_lib_closure target/release/kastellan-microvm-init target/release/kastellan-worker-matrix

# Bake the OS CA trust store: matrix-sdk 0.18 reads the SYSTEM store to validate the
# homeserver leaf. /etc and /usr are not share anchors, so unlike every other rootfs
# this one ships certificates. Copy whatever this build host provides (Debian/Ubuntu
# layout first, RH layout as a fallback); at least one must exist.
CA_FOUND=0
for ca in /etc/ssl/certs /etc/ssl/cert.pem /usr/share/ca-certificates /etc/pki/tls/certs; do
    if [ -e "$ca" ]; then
        install -d "$WORK$(dirname "$ca")"
        cp -a "$ca" "$WORK$ca"
        CA_FOUND=1
    fi
done
if [ "$CA_FOUND" -eq 0 ]; then
    echo "No OS CA trust store found on this build host — matrix TLS validation would fail in-guest." >&2
    exit 1
fi

# Pseudo-fs + slice-3 share anchors + /run (egress relay, slice 4a) + /data
# (persistent crypto store mountpoint, slice 5b-2).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" "$WORK/run" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"

mkfs.ext4 -q -F -O ^has_journal -L matrix -d "$WORK" "$OUT_DIR/matrix.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/matrix.ext4 (+ shared $OUT_DIR/vmlinux)"
```

- [ ] **Step 2: Make it executable + syntax-check**

Run:
```bash
chmod +x scripts/workers/microvm/build-matrix-rootfs.sh
bash -n scripts/workers/microvm/build-matrix-rootfs.sh && echo "SYNTAX_OK"
```
Expected: `SYNTAX_OK` (no output from `bash -n` on success).

- [ ] **Step 3: Assert the three deltas are present (self-check)**

Run:
```bash
grep -q 'ROOTFS_MIB=512' scripts/workers/microvm/build-matrix-rootfs.sh \
  && grep -q -- '--features live-matrix' scripts/workers/microvm/build-matrix-rootfs.sh \
  && grep -q 'matrix.ext4' scripts/workers/microvm/build-matrix-rootfs.sh \
  && grep -q '/etc/ssl/certs' scripts/workers/microvm/build-matrix-rootfs.sh \
  && grep -q '"\$WORK/data"' scripts/workers/microvm/build-matrix-rootfs.sh \
  && echo "DELTAS_OK"
```
Expected: `DELTAS_OK`.

- [ ] **Step 4: Commit**

```bash
git add scripts/workers/microvm/build-matrix-rootfs.sh
git commit -m "feat(microvm): matrix VM rootfs builder (baked worker + OS CA bundle, 512 MiB)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `microvm-init` — bring guest loopback UP

**Files:**
- Modify: `workers/microvm-init/src/main.rs` (add `pack_ifname` + `bring_loopback_up`; call in `main`)
- Test: inline `#[cfg(test)] mod tests` in the same file (the `pack_ifname` unit tests)

**Interfaces:**
- Consumes: the existing `libc` dependency (already used throughout this file for `mount`/`socket`/`ioctl`-adjacent calls).
- Produces: `bring_loopback_up()` (called unconditionally from `main`) so the in-guest `ProxyBridge` can bind/dial `127.0.0.1`. Pure helper `pack_ifname(&str) -> [libc::c_char; 16]` is unit-testable without a socket.

- [ ] **Step 1: Write the failing test for `pack_ifname`**

In `workers/microvm-init/src/main.rs`, inside the existing `#[cfg(test)] mod tests` block (near the other `parse_*` tests), add:

```rust
    #[cfg(target_os = "linux")]
    #[test]
    fn pack_ifname_lo_is_nul_padded() {
        let n = super::pack_ifname("lo");
        assert_eq!(n[0], b'l' as libc::c_char);
        assert_eq!(n[1], b'o' as libc::c_char);
        assert_eq!(n[2], 0);
        assert_eq!(n[15], 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pack_ifname_truncates_to_15_and_nul_terminates() {
        // 20-char name → 15 bytes kept, index 15 stays NUL.
        let n = super::pack_ifname("0123456789abcdefGHIJ");
        assert_eq!(n[14], b'e' as libc::c_char); // 15th kept char (index 14)
        assert_eq!(n[15], 0);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-microvm-init pack_ifname`
Expected: FAIL — `cannot find function 'pack_ifname' in module 'super'` (compile error).

*(On the Mac these `#[cfg(target_os = "linux")]` tests are compiled out — they run on the DGX. On the Mac, verify the crate still builds: `cargo build -p kastellan-microvm-init` compiles the non-Linux stub `main`.)*

- [ ] **Step 3: Write `pack_ifname` + `bring_loopback_up`**

In `workers/microvm-init/src/main.rs`, add near `mount_pseudo_fs` (both `#[cfg(target_os = "linux")]`):

```rust
/// Pack an interface name into a 16-byte `ifr_name` buffer: NUL-padded, truncated
/// to 15 chars + a trailing NUL. Pure — unit-testable without a socket.
#[cfg(target_os = "linux")]
fn pack_ifname(name: &str) -> [libc::c_char; 16] {
    let mut buf = [0 as libc::c_char; 16];
    for (i, b) in name.bytes().take(15).enumerate() {
        buf[i] = b as libc::c_char;
    }
    buf
}

/// Bring the guest loopback interface (`lo`) UP. A minimal Firecracker guest boots
/// with `lo` DOWN; the matrix worker's in-guest `ProxyBridge` binds and dials
/// `127.0.0.1:<port>`, which fails on a down loopback. Called UNCONDITIONALLY from
/// `main` — it is harmless for workers that never touch loopback (removing a
/// per-worker conditional). Fail-loud to the kernel console but never aborts PID1:
/// read the current flags (SIOCGIFFLAGS), OR in IFF_UP, write back (SIOCSIFFLAGS).
#[cfg(target_os = "linux")]
fn bring_loopback_up() {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            eprintln!(
                "microvm-init: loopback socket() failed (errno {})",
                *libc::__errno_location()
            );
            return;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        ifr.ifr_name = pack_ifname("lo");
        if libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut ifr) != 0 {
            eprintln!(
                "microvm-init: SIOCGIFFLAGS(lo) failed (errno {})",
                *libc::__errno_location()
            );
            libc::close(fd);
            return;
        }
        // ifr_ifru is a union; ifru_flags is the active member after SIOCGIFFLAGS.
        ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(fd, libc::SIOCSIFFLAGS, &mut ifr) != 0 {
            eprintln!(
                "microvm-init: SIOCSIFFLAGS(lo) IFF_UP failed (errno {})",
                *libc::__errno_location()
            );
        } else {
            eprintln!("LOOPBACK_UP");
        }
        libc::close(fd);
    }
}
```

> **Note for the implementer:** the `libc` crate names the `ifreq` fields `ifr_name` (`[c_char; 16]`) and `ifr_ifru` (a union whose `ifru_flags` member is `c_short`) on `*-unknown-linux-gnu`. If a version mismatch surfaces a different union accessor, adjust the single `ifr.ifr_ifru.ifru_flags` line only — the `pack_ifname` unit test and the `main` call site do not change. Confirm on the DGX build.

- [ ] **Step 4: Call it from `main` (after `mount_pseudo_fs`)**

In the `#[cfg(target_os = "linux")] fn main()`, insert the call immediately after `mount_pseudo_fs();`:

```rust
fn main() {
    mount_pseudo_fs();
    // Guest `lo` boots DOWN; the matrix worker's ProxyBridge binds 127.0.0.1.
    // Unconditional + harmless for loopback-free workers (slice 5b-4b).
    bring_loopback_up();
    let cmdline_for_mounts = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    apply_host_mounts(&parse_mount_manifest(&cmdline_for_mounts));
    // ... (rest unchanged) ...
```

- [ ] **Step 5: Verify (Mac cross-clippy + DGX test)**

Mac (cfg-only lint where the linker allows):
```bash
source "$HOME/.cargo/env"
cargo build -p kastellan-microvm-init            # non-Linux stub compiles
cargo clippy -p kastellan-microvm-init --target aarch64-unknown-linux-gnu --all-targets
```
DGX (native): `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-microvm-init pack_ifname'`
Expected: `pack_ifname_*` tests PASS on the DGX.

- [ ] **Step 6: Commit**

```bash
git add workers/microvm-init/src/main.rs
git commit -m "feat(microvm): bring guest loopback up in init (matrix ProxyBridge binds 127.0.0.1)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `MatrixSpawnConfig.use_microvm` + config parsing (`KASTELLAN_MATRIX_USE_MICROVM`, `KASTELLAN_MATRIX_PASSWORD`)

**Files:**
- Modify: `core/src/channel/matrix.rs` (`MatrixSpawnConfig` struct + `parse_daemon_spawn_config`)
- Test: `core/src/channel/matrix/tests.rs`

**Interfaces:**
- Consumes: the existing `parse_daemon_spawn_config(get: impl Fn(&str)->Option<String>, exe_dir, default_store)` injectable-getter signature.
- Produces: `MatrixSpawnConfig.use_microvm: bool` (default `false`) read from `KASTELLAN_MATRIX_USE_MICROVM` (`"1"` ⇒ true); `MatrixSpawnConfig.password: Option<String>` now populated from `KASTELLAN_MATRIX_PASSWORD` (was hardcoded `None`) so a VM bootstrap run can supply a first-login password. Consumed by `spawn_matrix_worker` (Task 5) and `main.rs` (Task 6).

- [ ] **Step 1: Write the failing tests**

In `core/src/channel/matrix/tests.rs`, add (adjust the helper name if the existing tests build the getter differently — grep the file for how `parse_daemon_spawn_config` is currently exercised and reuse that pattern):

```rust
    // Minimal env map → getter closure, mirroring the existing parse tests.
    fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + '_ {
        move |k: &str| pairs.iter().find(|(kk, _)| *kk == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn parse_daemon_config_defaults_use_microvm_false_and_password_none() {
        let env = [
            ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://matrix.kastellan.dev"),
            ("KASTELLAN_MATRIX_USER", "@kastellan:kastellan.dev"),
            ("KASTELLAN_MATRIX_STORE", "/state/matrix/store"),
            ("KASTELLAN_MATRIX_WORKER_BIN", "/bin/kastellan-worker-matrix"),
        ];
        let cfg = super::parse_daemon_spawn_config(getter(&env), None, None).unwrap();
        assert!(!cfg.use_microvm);
        assert_eq!(cfg.password, None);
    }

    #[test]
    fn parse_daemon_config_reads_use_microvm_and_password() {
        let env = [
            ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://matrix.kastellan.dev"),
            ("KASTELLAN_MATRIX_USER", "@kastellan:kastellan.dev"),
            ("KASTELLAN_MATRIX_STORE", "/state/matrix/store"),
            ("KASTELLAN_MATRIX_WORKER_BIN", "/bin/kastellan-worker-matrix"),
            ("KASTELLAN_MATRIX_USE_MICROVM", "1"),
            ("KASTELLAN_MATRIX_PASSWORD", "s3cret"),
        ];
        let cfg = super::parse_daemon_spawn_config(getter(&env), None, None).unwrap();
        assert!(cfg.use_microvm);
        assert_eq!(cfg.password.as_deref(), Some("s3cret"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib channel::matrix::tests::parse_daemon_config`
Expected: FAIL — `no field 'use_microvm' on type MatrixSpawnConfig` (compile error).

- [ ] **Step 3: Add the field + parse it**

In `core/src/channel/matrix.rs`, add the field to `MatrixSpawnConfig` (after `enforce_sandbox`):

```rust
    /// When `true` (Linux only, `KASTELLAN_MATRIX_USE_MICROVM=1`), the worker runs
    /// in a Firecracker VM: the caller resolves the `FirecrackerVm` backend and
    /// `spawn_matrix_worker` builds the VM policy (persistent_store at /data + baked
    /// rootfs). Ignored on macOS. Default `false` ⇒ the 5b-4a bwrap/Seatbelt path.
    pub use_microvm: bool,
```

In `parse_daemon_spawn_config`, replace the hardcoded `password: None` and add `use_microvm`. The current tail builds the struct; change it to:

```rust
    let use_microvm = get("KASTELLAN_MATRIX_USE_MICROVM")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let password = get("KASTELLAN_MATRIX_PASSWORD").filter(|v| !v.is_empty());
    Some(MatrixSpawnConfig {
        worker_bin,
        homeserver_url,
        user,
        store_dir,
        password,
        device_name: Some("kastellan-daemon".to_string()),
        enforce_sandbox,
        use_microvm,
    })
```

> Also update **every other `MatrixSpawnConfig { .. }` literal** (the existing tests in `channel/matrix/tests.rs` and any live-e2e helper) to add `use_microvm: false` — grep `MatrixSpawnConfig {` across the repo and fix each. The compiler lists them.

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib channel::matrix::tests::parse_daemon_config`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/matrix.rs core/src/channel/matrix/tests.rs
git commit -m "feat(matrix): MatrixSpawnConfig.use_microvm + read KASTELLAN_MATRIX_USE_MICROVM/PASSWORD

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: `build_matrix_vm_policy` — the VM-mode Matrix `SandboxPolicy` (pure)

**Files:**
- Modify: `core/src/channel/matrix.rs`
- Test: `core/src/channel/matrix/tests.rs`

**Interfaces:**
- Consumes: `kastellan_sandbox::{Net, Profile, SandboxPolicy, PersistentStore}` (add `PersistentStore` to the existing `use kastellan_sandbox::{...}` line).
- Produces: `pub fn build_matrix_vm_policy(homeserver_host: &str, homeserver_port: u16, image_dir: String, store_image: PathBuf) -> SandboxPolicy`. Empty `fs_read`/`fs_write` (worker binary + OS CA are baked into the rootfs; `fs_write` becomes ephemeral scratch under FC anyway), `Net::Allowlist([host:port])`, `Profile::WorkerMatrixClient`, `mem_mb: 512`, `cpu_ms: 0`, env `KASTELLAN_MICROVM_DIR` + `KASTELLAN_MICROVM_ROOTFS=matrix.ext4`, and `persistent_store = Some(PersistentStore { host_backing: store_image, guest_mount: /data, size_mib: 256 })`. Consumed by `spawn_matrix_worker` (Task 5).

- [ ] **Step 1: Write the failing test**

In `core/src/channel/matrix/tests.rs`:

```rust
    #[test]
    fn vm_policy_has_persistent_store_at_data_and_microvm_env() {
        use kastellan_sandbox::{Net, Profile};
        let store_image = std::path::PathBuf::from("/var/lib/kastellan/microvm/matrix-state.ext4");
        let p = super::build_matrix_vm_policy(
            "matrix.kastellan.dev",
            443,
            "/var/lib/kastellan/microvm".to_string(),
            store_image.clone(),
        );
        // Baked-in binary + CA ⇒ nothing to RO-share.
        assert!(p.fs_read.is_empty());
        assert!(p.fs_write.is_empty());
        assert_eq!(p.mem_mb, 512);
        assert_eq!(p.cpu_ms, 0);
        assert!(matches!(p.profile, Profile::WorkerMatrixClient));
        assert_eq!(p.net, Net::Allowlist(vec!["matrix.kastellan.dev:443".to_string()]));
        assert!(p.proxy_uds.is_none()); // force-routing sets this at spawn
        // Persistent store rides an ext4 image mounted at /data (survives respawn).
        let ps = p.persistent_store.expect("persistent_store set in VM mode");
        assert_eq!(ps.host_backing, store_image);
        assert_eq!(ps.guest_mount, std::path::PathBuf::from("/data"));
        assert_eq!(ps.size_mib, 256);
        // Backend boots the matrix rootfs from the shared image dir.
        assert!(p.env.iter().any(|(k, v)| k == "KASTELLAN_MICROVM_DIR"
            && v == "/var/lib/kastellan/microvm"));
        assert!(p.env.iter().any(|(k, v)| k == "KASTELLAN_MICROVM_ROOTFS"
            && v == "matrix.ext4"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib channel::matrix::tests::vm_policy`
Expected: FAIL — `cannot find function 'build_matrix_vm_policy'`.

- [ ] **Step 3: Implement `build_matrix_vm_policy`**

Add `PersistentStore` to the sandbox import at the top of `core/src/channel/matrix.rs`:
```rust
use kastellan_sandbox::{Net, PersistentStore, Profile, SandboxBackend, SandboxPolicy};
```
Then add the builder (place it right after `build_matrix_policy`):

```rust
/// VM-mode (5b-4b) Matrix policy. Unlike the bwrap `build_matrix_policy`, the
/// worker binary AND the OS CA trust store are BAKED INTO the rootfs
/// (`build-matrix-rootfs.sh`), so `fs_read` is empty — there are no host paths to
/// RO-share, and the sidecar resolves DNS so no resolver files are needed in-guest.
/// The E2E crypto/session store rides a `persistent_store` ext4 image mounted at
/// `/data`: it survives VM respawns (the FC backend wipes `fs_write` per spawn),
/// which is what preserves the device identity, `session.json`, and the #321
/// sync-token downtime recovery. Force-routing sets `proxy_uds` at spawn.
pub fn build_matrix_vm_policy(
    homeserver_host: &str,
    homeserver_port: u16,
    image_dir: String,
    store_image: PathBuf,
) -> SandboxPolicy {
    SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Allowlist(vec![format!("{homeserver_host}:{homeserver_port}")]),
        cpu_ms: 0, // long-lived; bounded by the KVM mem cap + cgroup
        mem_mb: 512,
        profile: Profile::WorkerMatrixClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env: vec![
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
            ("KASTELLAN_MICROVM_ROOTFS".to_string(), "matrix.ext4".to_string()),
        ],
        proxy_uds: None,
        persistent_store: Some(PersistentStore {
            host_backing: store_image,
            guest_mount: PathBuf::from("/data"),
            size_mib: 256,
        }),
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib channel::matrix::tests::vm_policy`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/matrix.rs core/src/channel/matrix/tests.rs
git commit -m "feat(matrix): build_matrix_vm_policy — persistent_store at /data + baked rootfs

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: `spawn_matrix_worker` — VM branch (program path, VM policy, transient password RO-share)

**Files:**
- Modify: `core/src/channel/matrix.rs` (`spawn_matrix_worker` + a small pure `matrix_vm_password_path` helper)
- Test: `core/src/channel/matrix/tests.rs`

**Interfaces:**
- Consumes: `cfg.use_microvm`, `cfg.password` (Task 3); `build_matrix_vm_policy` (Task 4); the existing `MatrixEgress`, `spawn_net_transport`/`NetTransportSpawn`, `PolledWorkerDriver::spawn`, `MATRIX_POLLED_SPEC`.
- Produces: the same `SpawnedMatrixWorker`. In VM mode: `program` = the in-guest baked path `/usr/local/bin/kastellan-worker-matrix`; policy = `build_matrix_vm_policy`; store env `KASTELLAN_MATRIX_STORE=/data`; and — only when `password.is_some()` — a transient 0600 password file under a `/tmp` share anchor, delivered to the guest as an RO-share and (re)written by the factory on every (re)spawn. Consumed by `main.rs` (Task 6).

**Design note — password delivery (deviation from spec §5b-4b.3, with rationale).** The spec says the host deletes the password file "right after the init handshake returns (RAII)." That is fragile against the respawn factory: a deleted `fs_read` path would break the *next* VM spawn's RO-share. Instead, the factory (which already runs on every (re)spawn) **re-writes** the 0600 password file to a fixed pid-scoped `/tmp` path before each spawn, and lists that stable path in the policy's `fs_read`. The worker's existing read-then-delete still fires (the delete fails harmlessly on the RO mount). Steady state stays password-less by the operator **unsetting `KASTELLAN_MATRIX_PASSWORD`** after the first successful login persists `session.json` to `/data` (⇒ `cfg.password = None` ⇒ no file written, no `fs_read` entry). This preserves the spec's intent (transient plaintext, bootstrap-only) while being respawn-safe. The initial-login source is `KASTELLAN_MATRIX_PASSWORD` on a one-time daemon run; the direct CLI probe stays the non-VM diagnostic path.

- [ ] **Step 1: Write the failing test for the password path helper**

In `core/src/channel/matrix/tests.rs`:

```rust
    #[test]
    fn vm_password_path_is_pid_scoped_under_tmp_anchor() {
        let p = super::matrix_vm_password_path(4242);
        assert_eq!(
            p,
            std::path::PathBuf::from("/tmp/kastellan-matrix-4242/.login-password")
        );
        // Must sit under the /tmp share anchor so the FC backend RO-shares it.
        assert!(p.starts_with("/tmp/"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib channel::matrix::tests::vm_password_path`
Expected: FAIL — `cannot find function 'matrix_vm_password_path'`.

- [ ] **Step 3: Add the helper + branch `spawn_matrix_worker`**

Add the pure helper near `build_matrix_vm_policy`:

```rust
/// The in-guest / on-host path of the transient VM-bootstrap password file. It
/// sits under the `/tmp` share anchor (pid-scoped to avoid collisions) so the
/// Firecracker backend RO-shares it into the guest at the identical absolute path.
/// Bootstrap-only: written only when `cfg.password.is_some()` (see the Task-5
/// design note); steady-state daemon spawns are password-less.
#[cfg(target_os = "linux")]
fn matrix_vm_password_path(pid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/kastellan-matrix-{pid}")).join(LOGIN_PASSWORD_FILE)
}
```

Now branch `spawn_matrix_worker`. The current body: (1) `create_dir_all(store_dir)`, (2) write password into `store_dir/.login-password`, (3) `build_matrix_policy(...)` + env pushes incl. `KASTELLAN_MATRIX_STORE=store_dir` + `KASTELLAN_MATRIX_PASSWORD_FILE`, (4) `program = cfg.worker_bin`, (5) the `PersistentFactory`, (6) supervisor + driver. Replace steps (1)–(4) with a `use_microvm` branch that computes `(policy, program, pw_delivery)`, where `pw_delivery` is an `Option<PathBuf>` the factory (re)writes each spawn.

Insert **before** the `let factory: PersistentFactory = ...` closure (replacing the existing steps 1–3 of the current fn):

```rust
    let (host, port) = host_port_from_url(&cfg.homeserver_url)?;

    // VM mode (Linux, opt-in) vs the 5b-4a bwrap/Seatbelt path.
    #[cfg(target_os = "linux")]
    let use_microvm = cfg.use_microvm;
    #[cfg(not(target_os = "linux"))]
    let use_microvm = false;

    // `pw_write` — Some((host_path, secret)) means the factory writes a transient
    // 0600 password file before each (re)spawn (VM bootstrap only). Non-VM mode
    // writes the file once into the bwrap-bound store_dir (existing behaviour).
    let mut pw_write: Option<(PathBuf, String)> = None;

    let (mut policy, program) = if use_microvm {
        #[cfg(target_os = "linux")]
        {
            // Rootfs image dir + the persistent-store ext4 backing file live in the
            // stable microvm dir (mkfs-once, outside any run dir — 5b-2).
            let image_dir = std::env::var("KASTELLAN_MICROVM_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
            let store_image = PathBuf::from(&image_dir).join("matrix-state.ext4");
            let mut policy = build_matrix_vm_policy(&host, port, image_dir, store_image);
            // The worker writes its crypto store to the /data mount, not store_dir.
            policy.env.push(("KASTELLAN_MATRIX_STORE".into(), "/data".into()));
            if let Some(pw) = &cfg.password {
                let pw_path = matrix_vm_password_path(std::process::id());
                policy.fs_read.push(pw_path.clone()); // RO-shared into the guest
                policy
                    .env
                    .push(("KASTELLAN_MATRIX_PASSWORD_FILE".into(), pw_path.display().to_string()));
                pw_write = Some((pw_path, pw.clone()));
            }
            (policy, MATRIX_MICROVM_WORKER_BIN.to_string())
        }
        #[cfg(not(target_os = "linux"))]
        {
            unreachable!("use_microvm is forced false off Linux")
        }
    } else {
        // 5b-4a path — unchanged.
        std::fs::create_dir_all(&cfg.store_dir)
            .map_err(|e| anyhow::anyhow!("create matrix store dir {:?}: {e}", cfg.store_dir))?;
        if let Some(password) = &cfg.password {
            let pw_path = cfg.store_dir.join(LOGIN_PASSWORD_FILE);
            write_private(&pw_path, password.as_bytes())
                .map_err(|e| anyhow::anyhow!("write matrix password file {pw_path:?}: {e}"))?;
        }
        let mut policy =
            build_matrix_policy(cfg.worker_bin.clone(), &host, port, cfg.store_dir.clone(), None, None);
        if cfg.password.is_some() {
            let pw_path = cfg.store_dir.join(LOGIN_PASSWORD_FILE);
            policy
                .env
                .push(("KASTELLAN_MATRIX_PASSWORD_FILE".into(), pw_path.display().to_string()));
        }
        policy
            .env
            .push(("KASTELLAN_MATRIX_STORE".into(), cfg.store_dir.display().to_string()));
        let program = cfg
            .worker_bin
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("worker bin path not UTF-8: {:?}", cfg.worker_bin))?
            .to_string();
        (policy, program)
    };

    // Env common to both modes.
    policy
        .env
        .push(("KASTELLAN_MATRIX_HOMESERVER_URL".into(), cfg.homeserver_url.clone()));
    policy.env.push(("KASTELLAN_MATRIX_USER".into(), cfg.user.clone()));
    if let Some(dev) = &cfg.device_name {
        policy.env.push(("KASTELLAN_MATRIX_DEVICE_NAME".into(), dev.clone()));
    }
    if !cfg.enforce_sandbox {
        policy.env.push(("KASTELLAN_SECCOMP_PROFILE".into(), "none".into()));
        policy.env.push(("KASTELLAN_LANDLOCK_PROFILE".into(), "none".into()));
    }
```

Add the module-level const near the other matrix consts (`POLL_MS`, `LOGIN_PASSWORD_FILE`):

```rust
/// The matrix worker binary's path INSIDE the VM rootfs (baked by
/// `build-matrix-rootfs.sh`). Used as the FC `program` so `microvm-init` execs it.
#[cfg(target_os = "linux")]
const MATRIX_MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-matrix";
```

Finally, in the `PersistentFactory` closure, re-write the transient password file at the **start** of each spawn (both the `Some(eg)` and `None` arms, before the transport is built). Add this at the top of the `move || { ... }` body, before the `match &egress`:

```rust
        // VM bootstrap: (re)write the transient 0600 password file each spawn so
        // the RO-shared fs_read path always exists at spawn time (respawn-safe).
        if let Some((pw_path, secret)) = &pw_write {
            if let Some(parent) = pw_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("create matrix pw dir {parent:?}: {e}"))?;
            }
            write_private(pw_path, secret.as_bytes())
                .map_err(|e| anyhow::anyhow!("write matrix pw file {pw_path:?}: {e}"))?;
        }
```

> The factory closure must `move` `pw_write` — it is already `move ||`. `pw_write` is `Option<(PathBuf, String)>` (owned), so it captures cleanly alongside the existing `policy`, `program`, `allowlist`, `egress`.

- [ ] **Step 4: Run the matrix lib tests (Mac) to verify pass + no regressions**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib channel::matrix`
Expected: PASS (the new `vm_password_path` test + all existing matrix unit tests). On the Mac the `use_microvm` branch is compiled out (`use_microvm` is forced `false`), so the bwrap arm is what compiles — confirm it is byte-equivalent to today by re-reading the diff.

- [ ] **Step 5: Verify the hermetic channel e2e still passes (Mac)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test matrix_channel_e2e`
Expected: PASS (2/2) — the restructure preserves `Channel` semantics on the non-VM path. (If the fixture is unbuilt it skip-as-passes; build it with `cargo build -p kastellan-core --example fake_matrix_worker` first.)

- [ ] **Step 6: Cross-clippy the Linux branch (Mac, best-effort) + commit**

```bash
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --all-targets   # native Mac lint of the shared code
git add core/src/channel/matrix.rs core/src/channel/matrix/tests.rs
git commit -m "feat(matrix): spawn_matrix_worker VM branch (in-guest program, VM policy, transient pw RO-share)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```
> The `#[cfg(target_os = "linux")]` VM arm is compiled+linted natively on the DGX in Task 7's full build. The `ring` C-dep blocks cross-`clippy` of `core` for Linux on the Mac (#144).

---

### Task 6: Daemon wiring — resolve the worker backend to `FirecrackerVm` in VM mode

**Files:**
- Modify: `core/src/main.rs` (the matrix channel block, ~lines 387–401)

**Interfaces:**
- Consumes: `spawn_cfg.use_microvm` (Task 3); `sandboxes.firecracker` + `sandboxes.bwrap` (`SandboxBackends`, Linux fields already present).
- Produces: the worker `backend` passed to `spawn_matrix_worker` is `sandboxes.firecracker` when `use_microvm`, else `sandboxes.bwrap`; the `MatrixEgress.sidecar_backend` stays `sandboxes.bwrap` (the 5c invariant — the sidecar is the real-network boundary and must run on the host). macOS is unchanged.

- [ ] **Step 1: Rewrite the backend-resolution block**

In `core/src/main.rs`, replace the current backend/egress lines (the `#[cfg(target_os = "linux")] let backend = ... bwrap;` / `#[cfg(target_os = "macos")] ... seatbelt;` pair and the `egress = force_routing.as_ref().map(...)` closure) with:

```rust
        // Worker backend: Firecracker VM when the operator opted in
        // (KASTELLAN_MATRIX_USE_MICROVM=1, Linux); else the host jail. The SIDECAR
        // backend always stays the host bwrap/Seatbelt (5c invariant — the egress
        // proxy needs a real network route; a VM here would boot a proxy with none).
        #[cfg(target_os = "linux")]
        let sidecar_backend: Arc<dyn kastellan_sandbox::SandboxBackend> =
            Arc::clone(&sandboxes.bwrap);
        #[cfg(target_os = "linux")]
        let backend: Arc<dyn kastellan_sandbox::SandboxBackend> = if spawn_cfg.use_microvm {
            Arc::clone(&sandboxes.firecracker)
        } else {
            Arc::clone(&sandboxes.bwrap)
        };
        #[cfg(target_os = "macos")]
        let sidecar_backend: Arc<dyn kastellan_sandbox::SandboxBackend> =
            Arc::clone(&sandboxes.seatbelt);
        #[cfg(target_os = "macos")]
        let backend: Arc<dyn kastellan_sandbox::SandboxBackend> = Arc::clone(&sandboxes.seatbelt);

        let egress = force_routing.as_ref().map(|fr| {
            kastellan_core::channel::matrix::MatrixEgress {
                sidecar_backend: Arc::clone(&sidecar_backend),
                routing: Arc::clone(fr),
            }
        });
```

The `spawn_blocking(move || spawn_matrix_worker(backend, ChannelId(...), &spawn_cfg, egress))` call below is unchanged — it already receives `backend` (now VM-or-bwrap) and `egress` (sidecar always bwrap). Note `spawn_cfg` is moved into the closure; read `spawn_cfg.use_microvm` **before** the move (the `let backend = if spawn_cfg.use_microvm` line above runs first, so this is already correct — confirm ordering when editing).

- [ ] **Step 2: Verify it compiles (Mac builds the macOS arm; DGX builds the Linux arm)**

Mac: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --bin kastellan`
Expected: builds clean (macOS arm — `use_microvm` path compiled out via the forced-false shadow in `spawn_matrix_worker`, but `main.rs` still references `spawn_cfg.use_microvm`, which exists cross-platform ⇒ compiles).

- [ ] **Step 3: Commit**

```bash
git add core/src/main.rs
git commit -m "feat(matrix): daemon resolves worker backend to FirecrackerVm in VM mode (sidecar stays bwrap)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: DGX real-KVM live e2e — VM round-trip + respawn + #321 downtime recovery

**Files:**
- Create: `core/tests/matrix_firecracker_live_e2e.rs`
- Test: the file itself (`#[ignore]`, DGX-gated, skip-as-pass elsewhere)

**Interfaces:**
- Consumes: the whole stack — `spawn_matrix_worker` with `use_microvm: true`, the `FirecrackerVm` backend, `matrix.ext4`, the sidecar, a live homeserver (`matrix.kastellan.dev`) + two bot accounts in a shared encrypted room.
- Produces: a gated acceptance test proving VM-mode matrix works and recovers a downtime message across a fresh-VM respawn + persistent store.

**Model:** combine `core/tests/matrix_live_e2e.rs::matrix_restart_recovers_downtime_message` (the #321 shape) with the `python_exec_firecracker_e2e.rs` skip-guard (`skip_if_no_microvm` — probes firecracker + puts `kastellan-microvm-run` on `$PATH`). The respawn is a real `pkill -f kastellan-microvm-run` (15-char `comm` truncation gotcha — use `-f`), driven through the production `spawn_matrix_worker` + `PersistentWorker` (not a bare `Command`), so the respawn boots a fresh VM + fresh sidecar.

- [ ] **Step 1: Write the gated e2e**

Create `core/tests/matrix_firecracker_live_e2e.rs`:

```rust
//! DGX real-KVM live e2e for slice 5b-4b: the Matrix worker runs in a Firecracker
//! VM, force-routed through a host egress sidecar, with its E2E store on a
//! persistent ext4 image at /data. Proves (1) VM-mode login + a real send/recv
//! round-trip against the live homeserver, and (2) the #321 downtime recovery
//! composed with a genuine fresh-VM respawn: kill the VMM (`pkill -f
//! kastellan-microvm-run`), PersistentWorker respawns a fresh VM + sidecar, the
//! message sent while down is recovered from the persisted sync token on /data.
//!
//! ALL tests `#[ignore]` + skip-as-pass. Opt in on the DGX with:
//!   export KASTELLAN_MATRIX_FC_LIVE_E2E=1
//!   export PATH=$HOME/.local/bin:$PATH        # firecracker on PATH
//!   # build the release launcher + rootfs first (stale-launcher gotcha):
//!   cargo build --release -p kastellan-microvm-run
//!   ./scripts/workers/microvm/build-matrix-rootfs.sh
//!   # required live env (same as matrix_live_e2e):
//!   KASTELLAN_MATRIX_HOMESERVER_URL / _USER / _PASSWORD / _PEER_USER /
//!   _PEER_PASSWORD / _ROOM
//!   cargo test -p kastellan-core --test matrix_firecracker_live_e2e -- --ignored --nocapture
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kastellan_core::channel::matrix::{spawn_matrix_worker, MatrixEgress, MatrixSpawnConfig};
use kastellan_core::channel::{Channel, ChannelId, OutgoingMessage, PeerId};
use kastellan_sandbox::{SandboxBackendKind, SandboxBackends};

const GATE: &str = "KASTELLAN_MATRIX_FC_LIVE_E2E";

// Skip-guard: gate env + firecracker readiness + launcher on PATH. Returns false
// (skip-as-pass) with an eprintln when any precondition is missing.
fn ready() -> bool {
    if std::env::var(GATE).ok().as_deref() != Some("1") {
        eprintln!("[SKIP] {GATE} != 1");
        return false;
    }
    // Reuse the microvm probe skip logic pattern from python_exec_firecracker_e2e:
    // FirecrackerVm probe on the matrix image + prepend target/release to PATH.
    // (Copy skip_if_no_microvm here or lift it into tests-common — see Step 3.)
    true
}

// ... (bot/peer account env reader — copy `required_env()` from matrix_live_e2e.rs) ...
// ... (a `spawn_vm_matrix(store_image_dir, acct)` that builds MatrixSpawnConfig
//      with use_microvm: true + resolves the FirecrackerVm backend + a real
//      MatrixEgress sidecar, driving through spawn_matrix_worker) ...

#[test]
#[ignore = "live: DGX KVM + conduwuit + two bot accounts in a shared encrypted room"]
fn matrix_vm_send_recv_round_trip() {
    if !ready() { return; }
    // 1. Build matrix.ext4 is a precondition (documented). Resolve FC backend.
    // 2. spawn bot (use_microvm) + peer, matrix.init both.
    // 3. peer matrix.send { conversation: room, body }.
    // 4. bot polls matrix.poll { timeout_ms: 2000 } up to 45s; assert body seen.
    // (Fill in mirroring matrix_live_e2e::matrix_send_recv_round_trip, but the bot
    //  goes through spawn_matrix_worker with use_microvm + MatrixEgress.)
    todo!("fill in per the module doc — see matrix_live_e2e.rs for the shape");
}

#[test]
#[ignore = "live: DGX KVM + conduwuit + two bot accounts in a shared encrypted room"]
fn matrix_vm_restart_recovers_downtime_message() {
    if !ready() { return; }
    // 1. store_image_dir = a stable dir (matrix-state.ext4 mkfs'd on first spawn).
    // 2. spawn bot (use_microvm) via spawn_matrix_worker; matrix.init persists
    //    session.json + sync token onto the /data ext4 image.
    // 3. peer sends body = format!("kastellan-fc-live-restart-{}", process::id())
    //    while the bot VM is killed (`pkill -f kastellan-microvm-run`).
    // 4. PersistentWorker respawns a FRESH VM + sidecar against the same
    //    matrix-state.ext4; poll up to 45s; assert the downtime body surfaces.
    todo!("fill in per the module doc — combine matrix_live_e2e restart shape + a real pkill -f");
}
```

> This test file is written to compile and skip-as-pass everywhere; the two `todo!()` bodies are filled in **on the DGX** during the live bring-up (the assertions and account plumbing mirror `matrix_live_e2e.rs` almost verbatim — copy `required_env`, the poll loop, and the `PeerId`/`OutgoingMessage` send shape). Keep the bodies self-contained; do not add a dependency on `matrix_live_e2e.rs`.

- [ ] **Step 2: Confirm it compiles + skip-as-passes on the Mac**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test matrix_firecracker_live_e2e`
Expected: the file is `#![cfg(target_os = "linux")]` ⇒ on macOS it compiles to an empty test binary (0 tests, PASS). *(If you prefer a running skip on macOS, drop the `#![cfg]` and gate each test on `ready()` — but the Linux-only cfg matches `python_exec_firecracker_e2e.rs`.)*

- [ ] **Step 3: Build the rootfs + release launcher on the DGX, fill in the bodies, run the gate**

On the DGX (native), one time:
```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  cargo build --release -p kastellan-microvm-run -p kastellan-microvm-init -p kastellan-worker-matrix --features live-matrix && \
  ./scripts/workers/microvm/build-matrix-rootfs.sh'
```
Then fill in the two `todo!()` bodies (mirroring `matrix_live_e2e.rs`), and run with the live env + gate set (see the module doc). Expected: `matrix_vm_send_recv_round_trip` + `matrix_vm_restart_recovers_downtime_message` both PASS. Capture the `LOOPBACK_UP` + `egress.allowed matrix.kastellan.dev:443` evidence from `--nocapture`.

- [ ] **Step 4: Commit**

```bash
git add core/tests/matrix_firecracker_live_e2e.rs
git commit -m "test(matrix): DGX VM-mode live e2e — round-trip + fresh-VM respawn + #321 recovery

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Full DGX gate + docs (HANDOVER + ROADMAP)

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full DGX workspace gate**

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  cargo build --workspace && \
  cargo test --workspace 2>&1 | tail -5 && \
  cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3'
```
Expected: `cargo test --workspace` passes (record the passed/failed/ignored counts — baseline before this slice is **2270 / 0 / 34**; expect +N for the new unit tests and +2 ignored for the new live e2e), clippy clean. Also run the two live e2e (Task 7 Step 3) and confirm both PASS.

- [ ] **Step 2: Update HANDOVER.md** — move 5b-4b from "Next TODO" to the current-session block (header `Last updated`, `Current state`, `Session-end verification` with the DGX counts), refresh "Working state" (new rootfs script, matrix VM policy, `use_microvm`, guest loopback-up), and write a fresh "Next TODO" (leading candidates: generalize `PolledWorkerDriver` for IMAP/Telegram; `tool_host.rs` prod-split; `persistent_store` resize #381).

- [ ] **Step 3: Tick ROADMAP.md** — mark `SLICE 5b-4b` `[x]` with the merge commit + the DGX gate counts.

- [ ] **Step 4: Commit the docs + open the PR**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): slice 5b-4b matrix-in-a-VM done + DGX gate

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git push -u origin feat/microvm-slice5b4b-matrix-in-vm
gh pr create --base main --title "feat(microvm): slice 5b-4b — Matrix runs in a Firecracker VM (persistent crypto store)" \
  --body "Closes #380 (5b-4b half). Composes 5b-1 PersistentWorker + 5b-2 persistent_store + 5c spawn_net_transport + 5a VMM confinement. Opt-in KASTELLAN_MATRIX_USE_MICROVM=1 (Linux). DGX real-KVM gate: <counts>; VM-mode live matrix round-trip + fresh-VM respawn + #321 recovery green.

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

---

## Self-Review

**1. Spec coverage.**
- §5b-4b.1 rootfs (baked worker + OS CA + `ROOTFS_MIB=512` + `/data`/`/run`) → **Task 1**. ✔
- §5b-4b.2 guest loopback up (`SIOCSIFFLAGS IFF_UP`, unconditional) → **Task 2**. ✔
- §5b-4b.3 crypto store `fs_write`→`persistent_store` at `/data` (256 MiB, mkfs-once) → **Task 4** (policy) + **Task 5** (`KASTELLAN_MATRIX_STORE=/data` + wiring). Password-file delivery via `/tmp` RO-share → **Task 5** (with a documented, respawn-safe deviation from the literal "delete after init"). ✔
- §5b-4b.4 backend switch (`KASTELLAN_MATRIX_USE_MICROVM=1`, worker=FC/sidecar=bwrap, fail-closed) → **Task 3** (flag) + **Task 6** (backend resolution). **Fail-closed note:** production spawn does not currently call `LinuxFirecracker::probe`; the FC backend's own spawn-time fail-closed (missing `firecracker`/`kastellan-microvm-run` under confinement ⇒ `Err`, no bwrap fallback) satisfies "refuse to spawn." If an explicit pre-spawn probe gate is wanted, add it in Task 6 mirroring the e2e `skip_if_no_microvm` — flagged, not silently omitted. ✔
- §5b-4b.5 DGX e2e (round-trip + `pkill -f` respawn + downtime recovery) + full gate → **Task 7** + **Task 8**. ✔

**2. Placeholder scan.** The only intentional deferrals are the two `todo!()` e2e bodies in Task 7, explicitly filled in on the DGX during live bring-up (they mirror `matrix_live_e2e.rs` verbatim and cannot be authored blind — a fake homeserver isn't in scope). Every code task (1–6) carries complete, exact code. No "add error handling"/"similar to Task N" placeholders.

**3. Type consistency.** `MatrixSpawnConfig.use_microvm` (Task 3) is read in Task 5 (`spawn_matrix_worker`) and Task 6 (`main.rs`). `build_matrix_vm_policy` (Task 4) is called in Task 5. `matrix_vm_password_path` (Task 5) is tested + used in the factory. `MATRIX_MICROVM_WORKER_BIN` (Task 5) is the FC `program`. `sandboxes.firecracker`/`sandboxes.bwrap` (Task 6) are confirmed `SandboxBackends` fields. `PersistentStore { host_backing, guest_mount, size_mib }` matches `sandbox/src/lib.rs:88-96`. `Net::Allowlist`/`Profile::WorkerMatrixClient` match the sandbox enums. All consistent.

**Open decision surfaced to the operator (Task 5 design note):** the VM-bootstrap password path deviates from the spec's literal "host deletes after init (RAII)" in favor of a respawn-safe per-spawn rewrite + operator-unset-to-go-password-less. If the operator prefers the literal delete-after-init (accepting the respawn-fragility, e.g. by making the very first VM login a non-respawning one-shot), Task 5's factory step and `pw_write` handling change accordingly — everything else holds.
