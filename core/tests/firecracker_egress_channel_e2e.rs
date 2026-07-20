#![cfg(target_os = "linux")]
//! Slice 4a e2e: proves the guest-initiated vsock egress reverse-channel on real
//! KVM. A force-routed VM (Net::Allowlist + proxy_uds) boots with the self-test
//! knob; the guest init dials the in-guest egress UDS, which relays over a second
//! vsock port to the launcher's reverse-relay and on to a host echo UnixListener
//! standing in for the egress proxy. We assert the host echo RECEIVES the guest's
//! PING — the novel guest→host direction, observed entirely host-side.
//!
//! DGX-only / #[ignore]: needs /dev/kvm + /dev/vhost-vsock + a built rootfs
//! (REBUILD via build-rootfs.sh so it carries the /run mountpoint) + the
//! kastellan-microvm-run RELEASE launcher (rebuild it; target/release is
//! preferred and a stale one silently shadows source changes). Run:
//!
//!     export PATH=$HOME/.local/bin:$PATH   # firecracker is off the ssh PATH
//!     cargo build --release -p kastellan-microvm-run
//!     cargo test -p kastellan-core --test firecracker_egress_channel_e2e -- --ignored --nocapture

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use kastellan_core::tool_host::{spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_sandbox::{Net};
use kastellan_tests_common::microvm::{firecracker_backend, image_dir, skip_if_no_microvm};

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "python-exec.ext4";

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "DGX-only: real KVM + vsock + rootfs with /run mountpoint"]
async fn egress_reverse_channel_delivers_guest_ping_to_host_proxy_uds() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }

    // Host echo "proxy": the proxy_uds target. On accept, read PING and reply PONG,
    // signalling receipt back to the test thread.
    let dir = std::env::temp_dir().join(format!("kastellan-s4a-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let echo_path = dir.join("egress.sock");
    let _ = std::fs::remove_file(&echo_path);
    let listener = UnixListener::bind(&echo_path).unwrap();
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        if let Ok((mut c, _)) = listener.accept() {
            let mut buf = [0u8; 5];
            if c.read_exact(&mut buf).is_ok() {
                let _ = tx.send(buf.to_vec());
                let _ = c.write_all(b"PONG\n");
            }
        }
    });

    // Force-routed entry: python-exec rootfs, but Net::Allowlist + proxy_uds +
    // the self-test knob. The worker process is irrelevant here — the init's
    // self-test originates the PING during boot.
    let mut entry = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        image_dir(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
    );
    entry.policy.net = Net::Allowlist(vec!["example.com:443".into()]);
    entry.policy.proxy_uds = Some(echo_path.clone());
    entry.policy.env.push(("KASTELLAN_MICROVM_EGRESS_SELFTEST".into(), "1".into()));

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let worker = spawn_worker(&*backend, &spec).expect("spawn force-routed worker in micro-VM");

    let got = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("host proxy UDS never received the guest PING (reverse channel broken)");
    assert_eq!(&got, b"PING\n", "guest-initiated egress reached the host proxy UDS");

    let _ = worker.close();
    let _ = std::fs::remove_dir_all(&dir);
}
