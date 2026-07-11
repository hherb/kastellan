//! Pure [`SandboxPolicy`] builders for the Matrix worker + the login-password
//! file helpers.
//!
//! Two policy shapes: the bwrap/Seatbelt [`build_matrix_policy`] (host paths
//! RO-shared into the jail) and the Firecracker VM [`build_matrix_vm_policy`]
//! (binary + CA baked into the rootfs, E2E store on a `persistent_store` image).
//! Both are pure + unit-tested; the spawn that consumes them lives in the parent.
//!
//! Split out of the parent `matrix.rs` (2026-07-07 prod-split, Item 9b); the
//! public `build_matrix_policy` / `build_matrix_vm_policy` paths are byte-identical
//! via the parent's `pub use` re-exports. `write_private` / `LOGIN_PASSWORD_FILE`
//! / `matrix_vm_password_path` / `MATRIX_MICROVM_WORKER_BIN` are `pub(crate)` for
//! the parent's spawn factory (and the tests via `super::`), not part of the
//! public surface.

use std::path::PathBuf;

use kastellan_sandbox::{Net, PersistentStore, Profile, SandboxPolicy};

/// Filename (inside the persistent store dir) for the one-time initial-login
/// password handed to the worker out-of-band (not via argv). The worker reads it
/// via `KASTELLAN_MATRIX_PASSWORD_FILE` and consumes (deletes) it after login.
pub(crate) const LOGIN_PASSWORD_FILE: &str = ".login-password";

/// The matrix worker binary's path INSIDE the VM rootfs (baked by
/// `build-matrix-rootfs.sh`). Used as the FC `program` so `microvm-init` execs it.
#[cfg(target_os = "linux")]
pub(crate) const MATRIX_MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-matrix";

/// Write `bytes` to `path`, truncating, with `0600` permissions (owner-only) —
/// the initial-login password is a secret at rest, like the worker's session.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

/// Build the [`SandboxPolicy`] for the long-lived Matrix worker. Pure +
/// unit-tested; the spawn that consumes it is Phase D.
///
/// - `Net::Allowlist([homeserver_host:443])` — the worker reaches only the
///   homeserver (via the egress proxy when `proxy_uds` is set).
/// - `Profile::WorkerMatrixClient` — outbound HTTPS via the proxy, plus the
///   matrix-rust-sdk SQLite-store seccomp additions (`matrix_client`).
/// - `fs_read`: the worker binary + the resolver config files (DNS in-jail) +
///   the system CA trust store (matrix-sdk 0.18 validates homeserver TLS against
///   it) + the egress CA when force-routed.
/// - `fs_write`: the **persistent** E2E store dir (NOT ephemeral scratch — the
///   SDK persists device keys + sync token there across restarts).
pub fn build_matrix_policy(
    binary: PathBuf,
    homeserver_host: &str,
    homeserver_port: u16,
    store_dir: PathBuf,
    proxy_uds: Option<PathBuf>,
    egress_ca: Option<PathBuf>,
) -> SandboxPolicy {
    let mut fs_read = vec![
        binary,
        PathBuf::from("/etc/resolv.conf"),
        PathBuf::from("/etc/hosts"),
        PathBuf::from("/etc/nsswitch.conf"),
    ];
    // matrix-sdk 0.18 validates the homeserver's TLS against the *system* trust
    // store (rustls + native certs), so the worker needs the CA bundle inside the
    // jail — without it `Client::builder().build()` fails at startup with "No CA
    // certificates were loaded from the system" and the channel never starts.
    // (matrix-sdk 0.8 used bundled webpki roots and never read these, which is why
    // this only surfaced after the 0.18 upgrade.) The worker does native
    // end-to-end TLS to the homeserver even through the egress tunnel (transparent
    // `disable_mitm`), so the system CA is needed regardless of force-routing.
    // Bind the well-known trust-store locations; `fs_read` is emitted as
    // `--ro-bind-try`, so paths absent on a given distro/OS are silently skipped.
    // `/usr/share/ca-certificates` is already covered by the `/usr` bind — these
    // are the `/etc` paths that are not.
    for ca in ["/etc/ssl/certs", "/etc/pki/tls/certs", "/etc/ssl/cert.pem"] {
        fs_read.push(PathBuf::from(ca));
    }
    if let Some(ca) = egress_ca {
        fs_read.push(ca);
    }
    SandboxPolicy {
        fs_read,
        fs_write: vec![store_dir],
        net: Net::Allowlist(vec![format!("{homeserver_host}:{homeserver_port}")]),
        cpu_ms: 0, // long-lived; no per-process CPU cap (bounded by cgroup/quota)
        mem_mb: 512,
        profile: Profile::WorkerMatrixClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env: Vec::new(), // spawn fills env (homeserver/user/secret refs) at Phase D
        proxy_uds,
        broker_uds: None,
        persistent_store: None,
    }
}

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
        broker_uds: None,
        persistent_store: Some(PersistentStore {
            host_backing: store_image,
            guest_mount: PathBuf::from("/data"),
            size_mib: 256,
        }),
    }
}

/// The in-guest / on-host path of the transient VM-bootstrap password file. It
/// sits under the `/tmp` share anchor (pid-scoped to avoid collisions) so the
/// Firecracker backend RO-shares it into the guest at the identical absolute path.
/// Bootstrap-only: written only when `cfg.password.is_some()` (see the Task-5
/// design note); steady-state daemon spawns are password-less.
///
/// Cross-platform (not `#[cfg(target_os = "linux")]`): only the Linux VM arm of
/// `spawn_matrix_worker` calls this in production, but keeping it unconditional
/// lets its unit test run on every dev platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn matrix_vm_password_path(pid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/kastellan-matrix-{pid}")).join(LOGIN_PASSWORD_FILE)
}
