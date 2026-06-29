//! Cross-platform (Seatbelt on macOS, bwrap on Linux) e2e for the persistent-VM
//! lifecycle ABSTRACTION without a VM: a kv-demo worker under `PersistentWorker`
//! with a persistent host-dir store. Proves many-calls-one-boot, then
//! worker-death via `kv.crash`, PersistentWorker respawn, and store survival.
//!
//! Skip-as-pass if the kv-demo binary is not built or the default OS sandbox is
//! unavailable (e.g. macOS without sandbox-exec, Linux without userns).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;
use std::time::Duration;

use kastellan_core::worker_lifecycle::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
use kastellan_sandbox::{Net, PersistentStore, Profile, SandboxBackends, SandboxPolicy};

// ── helpers ──────────────────────────────────────────────────────────────────

/// Locate the kv-demo binary under the workspace target dir.
fn kv_demo_binary() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // core/
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    let bin = target.join("debug").join("kastellan-worker-kv-demo");
    if bin.exists() { Some(bin) } else { None }
}

/// Create a canonicalized temporary directory for kv state with a unique name.
/// Canonicalization is required on macOS because `std::env::temp_dir()` returns
/// a `$TMPDIR`-based path that contains symlinks (`/var` → `/private/var`).
/// The Seatbelt backend resolves symlinks in `fs_read`/`fs_write` but does NOT
/// canonicalize `persistent_store.guest_mount`, so we do it here to ensure the
/// generated Seatbelt rule matches what the kernel actually sees.
fn unique_tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kastellan-kv-persist-{}-{}",
        label,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// Minimal base policy for the kv-demo worker: net denied, strict profile,
/// 5s CPU budget, 256 MiB memory. `fs_read`, `persistent_store`, and `env`
/// are filled in by the factory closure.
fn base_deny_policy() -> SandboxPolicy {
    SandboxPolicy {
        net: Net::Deny,
        profile: Profile::WorkerStrict,
        cpu_ms: 5_000,
        mem_mb: 256,
        ..SandboxPolicy::default()
    }
}

// ── test ─────────────────────────────────────────────────────────────────────

#[test]
fn kv_demo_survives_respawn_under_default_backend() {
    // Skip if kv-demo binary is not built.
    let bin = match kv_demo_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] kv-demo not built; run `cargo build -p kastellan-worker-kv-demo`");
            return;
        }
    };

    // Skip if the default OS sandbox is unavailable.
    #[cfg(target_os = "linux")]
    {
        use kastellan_sandbox::linux_bwrap::LinuxBwrap;
        if let Err(e) = LinuxBwrap::probe() {
            eprintln!("[SKIP] bwrap probe failed: {e}");
            return;
        }
    }
    #[cfg(target_os = "macos")]
    {
        use kastellan_sandbox::macos_seatbelt::MacosSeatbelt;
        if let Err(e) = MacosSeatbelt::probe() {
            eprintln!("[SKIP] sandbox-exec probe failed: {e}");
            return;
        }
    }

    // Persistent store: a canonicalized temp dir. host_backing == guest_mount
    // (directory-backed mode on bwrap/Seatbelt — no VM disk image needed).
    let store = unique_tmp_dir("respawn");
    let backends = SandboxBackends::default_for_current_os();
    let backend = backends.resolve(None, None);
    let bin_dir = bin.parent().unwrap().to_path_buf();

    // Factory: builds a fresh ClientTransport each time PersistentWorker spawns
    // (initial boot + every respawn after worker death).
    let factory: PersistentFactory = {
        let bin = bin.clone();
        let store = store.clone();
        let bin_dir = bin_dir.clone();
        let backend = std::sync::Arc::clone(&backend);
        Box::new(move || {
            let mut policy = base_deny_policy();
            // Grant read access to the binary directory (loader needs it).
            policy.fs_read = vec![bin_dir.clone()];
            // Persistent writable store: host path == guest mount.
            policy.persistent_store = Some(PersistentStore {
                host_backing: store.clone(),
                guest_mount: store.clone(),
                size_mib: 0,
            });
            // Tell the worker where to write its kv store.
            policy.env.push((
                "KASTELLAN_KV_STORE_DIR".to_string(),
                store.to_string_lossy().into_owned(),
            ));
            let t = ClientTransport::spawn(&*backend, &policy, &bin.to_string_lossy(), &[])?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        })
    };

    let h = PersistentWorker::spawn("kv-persist", factory).expect("spawn kv persistent worker");

    // ── Phase 1: many calls on one boot ──────────────────────────────────────

    let put = h
        .call("kv.put", serde_json::json!({"key": "k", "value": "before-crash"}))
        .expect("kv.put");
    assert_eq!(put["ok"], true, "kv.put must return ok:true, got {put}");

    // Record pre-crash pid so we can confirm the respawned worker is a new process.
    let mut pre_crash_pid: Option<u64> = None;
    for i in 0..5 {
        let stats = h.call("kv.stats", serde_json::json!({}))
            .unwrap_or_else(|e| panic!("kv.stats call {i} failed: {e}"));
        if i == 4 {
            pre_crash_pid = stats["pid"].as_u64();
        }
    }

    // ── Phase 2: deterministic worker death via kv.crash ─────────────────────

    // kv.crash calls std::process::exit(1) without sending a reply → the caller
    // receives an I/O error, which PersistentWorker treats as a worker death and
    // respawns.  We expect (and tolerate) an Err here.
    let crash_result = h.call("kv.crash", serde_json::json!({}));
    eprintln!("[INFO] kv.crash result: {:?}", crash_result);
    // crash_result is nearly always Err (worker exited before replying).
    // The Ok branch is theoretically possible if the channel drains before the
    // client observes EOF, but in practice never happens with exit(1).

    // ── Phase 3: bounded retry until respawned worker answers ─────────────────

    // PersistentWorker respawns with backoff.  Poll with short sleeps up to ~2s.
    let mut got = Err(anyhow::anyhow!("not yet"));
    for attempt in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        match h.call("kv.get", serde_json::json!({"key": "k"})) {
            Ok(v) => { got = Ok(v); break; }
            Err(e) => {
                eprintln!("[INFO] attempt {attempt}: kv.get err: {e}");
                // May be "persistent worker is restarting" — keep retrying.
            }
        }
    }
    let got = got.expect("kv.get should succeed after respawn within ~2s");
    assert_eq!(
        got["value"], "before-crash",
        "persistent store must survive worker-death respawn, got {got}"
    );

    // ── Phase 4: optional — confirm the respawned process is a fresh PID ──────

    let post_stats = h.call("kv.stats", serde_json::json!({})).expect("post-respawn kv.stats");
    let post_pid = post_stats["pid"].as_u64();
    if let (Some(pre), Some(post)) = (pre_crash_pid, post_pid) {
        assert_ne!(pre, post, "respawned worker must be a new OS process (pid changed)");
        eprintln!("[INFO] pre-crash pid={pre}, post-respawn pid={post} — confirmed new process");
    }

    // ── Teardown ──────────────────────────────────────────────────────────────
    h.shutdown();
    let _ = std::fs::remove_dir_all(&store);
}
