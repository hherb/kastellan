//! Real-spawn smoke for `PersistentWorker` + `ClientTransport` under the
//! default OS sandbox backend (Seatbelt on macOS, bwrap on Linux). Hermetic:
//! no VM, no network; uses the kv-demo worker binary as a minimal long-lived
//! RPC server. Skip-as-pass if the kv-demo binary is not built.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::worker_lifecycle::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
use kastellan_sandbox::{Net, PersistentStore, Profile, SandboxBackends, SandboxPolicy};

/// Locate the kv-demo binary under the workspace target dir.
fn kv_demo_binary() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // core/
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    let bin = target.join("debug").join("kastellan-worker-kv-demo");
    if bin.exists() { Some(bin) } else { None }
}

/// Create a temporary directory for kv state with a unique name and return its
/// **canonical** path. Canonicalization is required on macOS because
/// `std::env::temp_dir()` returns a `$TMPDIR`-based path that contains
/// symlinks (`/var` → `/private/var`). The Seatbelt backend resolves symlinks
/// in `fs_read`/`fs_write` via `canonicalize_policy_paths`, but it does NOT
/// yet canonicalize `persistent_store.guest_mount` — so we do it here to
/// ensure the generated Seatbelt rule matches what the kernel actually sees.
fn tempdir_path(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kastellan-kvdemo-{}-{}",
        label,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    // Canonicalize so the Seatbelt (subpath "...") rule matches the real path
    // the kernel observes (symlinks resolved, e.g. /var → /private/var on macOS).
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// Minimal base policy for the kv-demo worker: net denied, strict profile,
/// 5s CPU budget, 256 MiB memory. `fs_read` and `persistent_store` / `env`
/// are added by the factory closure.
fn base_policy() -> SandboxPolicy {
    SandboxPolicy {
        net: Net::Deny,
        profile: Profile::WorkerStrict,
        cpu_ms: 5_000,
        mem_mb: 256,
        ..SandboxPolicy::default()
    }
}

#[test]
fn persistent_worker_serves_real_worker_many_calls() {
    // Skip if kv-demo is not built.
    let bin = match kv_demo_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] kv-demo not built; run `cargo build -p kastellan-worker-kv-demo`");
            return;
        }
    };

    // Skip if the sandbox is unavailable (macOS: sandbox-exec missing, Linux: no
    // userns).
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

    let store_dir = tempdir_path("kvstate");
    let backends = SandboxBackends::default_for_current_os();
    let backend = backends.resolve(None, None);

    // bin parent dir: grants read access to the binary + co-located dylib
    // artifacts if any. On macOS, Seatbelt already allows /usr/lib +
    // /System/Library, so only the target/debug dir needs to be in fs_read.
    let bin_dir = bin.parent().unwrap().to_path_buf();

    let factory: PersistentFactory = {
        let bin = bin.clone();
        let store_dir = store_dir.clone();
        let bin_dir = bin_dir.clone();
        let backend = std::sync::Arc::clone(&backend);
        Box::new(move || {
            let mut policy = base_policy();
            // Grant read access to the binary directory so the loader can map it.
            policy.fs_read = vec![bin_dir.clone()];
            // Persistent writable store: host path == guest mount (both point at
            // the same temp dir on bwrap/Seatbelt directory-backed mode).
            policy.persistent_store = Some(PersistentStore {
                host_backing: store_dir.clone(),
                guest_mount: store_dir.clone(),
                size_mib: 0,
            });
            // Worker needs to know where to write its kv store.
            policy.env.push((
                "KASTELLAN_KV_STORE_DIR".to_string(),
                store_dir.to_string_lossy().into_owned(),
            ));
            let t = ClientTransport::spawn(
                &*backend,
                &policy,
                &bin.to_string_lossy(),
                &[],
            )?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        })
    };

    let h = PersistentWorker::spawn("kv", factory).expect("spawn kv persistent worker");

    // kv.put
    let put_result = h.call("kv.put", serde_json::json!({"key": "k", "value": "v1"}))
        .expect("kv.put");
    assert_eq!(put_result["ok"], true, "kv.put must return ok:true, got {put_result}");

    // kv.get
    let got = h.call("kv.get", serde_json::json!({"key": "k"}))
        .expect("kv.get");
    assert_eq!(got["value"], "v1", "kv.get must return the stored value, got {got}");

    // kv.stats: calls_served must be >= 3 (put + get + stats = 3 from the worker's POV)
    let stats = h.call("kv.stats", serde_json::json!({}))
        .expect("kv.stats");
    assert!(
        stats["calls_served"].as_u64().unwrap_or(0) >= 3,
        "kv.stats calls_served must be >= 3, got {stats}"
    );

    h.shutdown();

    // Clean up.
    let _ = std::fs::remove_dir_all(&store_dir);
}
