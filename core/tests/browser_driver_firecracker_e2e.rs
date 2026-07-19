#![cfg(target_os = "linux")]
//! browser-driver × Firecracker micro-VM — slice 1 (the rootfs).
//!
//! ## Tiers
//!
//! * `vm_policy_flows_through_plan_to_in_rootfs_guest_path` — hermetic; always
//!   runs on Linux (no KVM, no network, no rootfs image needed). It feeds a
//!   browser-driver VM policy through the REAL `build_launch_plan` and pins
//!   that the guest execs the **in-rootfs** worker path rather than a host
//!   `target/` path. That failure mode is nasty and has cost a debugging
//!   session before: PID1 `execv`s a path that does not exist inside the guest,
//!   panics, the VM boot-loops, and the dispatch simply hangs to wall-clock —
//!   presenting as a channel hang with no error naming the real cause. It also
//!   pins the cmdline budget, because env is hex-encoded and therefore costs
//!   two cmdline bytes per env byte.
//!
//! * `vm_booted_browser_driver_launches_chromium` — the live DGX tier
//!   (`#[ignore]`): boots `browser-driver.ext4` and proves Chromium starts
//!   inside the guest.
//!
//! Note `kastellan_sandbox::linux_firecracker` is `#[cfg(target_os = "linux")]`
//! (`sandbox/src/lib.rs:11-16`), so this whole file is compiled out on macOS.
//! The DGX `clippy -p kastellan-core --all-targets -D warnings` gate is the
//! authoritative check for it; Mac clippy cannot see this code at all.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_sandbox::linux_firecracker::{build_launch_plan, FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{
    Net, Profile, SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxPolicy,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable,
    unique_suffix,
};

fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

/// The worker path baked into the rootfs by
/// `scripts/workers/microvm/build-browser-driver-rootfs.sh` (as a symlink into
/// the staged venv). Slice 2's `MICROVM_WORKER_BIN` const must match this
/// byte for byte.
const IN_ROOTFS_WORKER: &str = "/usr/local/bin/kastellan-worker-browser-driver";

/// The rootfs filename produced by `build-browser-driver-rootfs.sh`.
const ROOTFS_FILE: &str = "browser-driver.ext4";

/// Decode the lowercase-hex cmdline tokens `microvm-init` consumes.
///
/// `plan.rs::hex_encode` is `pub(super)`, so a test cannot reach its inverse;
/// this is the minimal decoder needed to read one token back.
fn hex_decode(s: &str) -> Vec<u8> {
    // `% 2 == 0` rather than `usize::is_multiple_of`: that method is stable only
    // since 1.87 and this workspace's clippy MSRV is 1.78, so it trips
    // `clippy::incompatible_msrv` under `-D warnings`.
    assert!(s.len() % 2 == 0, "hex token has odd length: {s}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex byte"))
        .collect()
}

/// The VM policy slice 2's `browser_driver_firecracker_entry` will produce.
///
/// Built inline because that production entry does not exist yet — slice 1 is
/// the rootfs only. Mirrors the shape of `web_fetch_firecracker_entry`: empty
/// `fs_read` (a VM shares no host paths in), force-routed, VM backend.
fn browser_driver_vm_policy() -> SandboxPolicy {
    SandboxPolicy {
        // Empty: the per-instance CA is appended at spawn, and browser-driver
        // runs the sidecar in no-MITM transparent-tunnel mode anyway
        // (force_route::disable_mitm_for names this worker).
        fs_read: vec![],
        fs_write: vec![],
        // `Net::Allowlist` WITH `proxy_uds` == force-routed. Without `proxy_uds`
        // `build_launch_plan` rejects it fail-closed, because a VM carries no
        // virtio-net device (plan.rs:255-267).
        net: Net::Allowlist(vec!["example.org:443".to_string()]),
        cpu_ms: 30_000,
        // Chromium plus a RAM-backed /tmp tmpfs; see the design spec §6.
        mem_mb: 2048,
        profile: Profile::WorkerBrowserClient,
        tasks_max: Some(512),
        env: vec![
            (
                "KASTELLAN_BROWSER_DRIVER_ALLOWLIST".to_string(),
                r#"["example.org"]"#.to_string(),
            ),
            (
                "PLAYWRIGHT_BROWSERS_PATH".to_string(),
                "/usr/local/lib/kastellan-browser-driver/browsers".to_string(),
            ),
            ("TMPDIR".to_string(), "/tmp".to_string()),
            // Playwright's Node driver calls uv_os_homedir(); without HOME it
            // dies with "Connection closed while reading from the driver".
            ("HOME".to_string(), "/tmp".to_string()),
            // Host-side backend config: `resolve_image` reads these to find the
            // rootfs. `build_launch_plan` strips them before hex-encoding the
            // guest env (plan.rs:390), so they cost no cmdline budget.
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir()),
            (
                "KASTELLAN_MICROVM_ROOTFS".to_string(),
                ROOTFS_FILE.to_string(),
            ),
        ],
        proxy_uds: Some(PathBuf::from("/tmp/kastellan-egress.sock")),
        ..Default::default()
    }
}

/// Image coordinates for the browser-driver micro-VM. The paths need not exist
/// for the hermetic tier — `build_launch_plan` is pure and does not touch the
/// filesystem.
fn browser_driver_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage {
        kernel_path: dir.join("vmlinux"),
        rootfs_path: dir.join(ROOTFS_FILE),
    }
}

fn locate_microvm_run() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core has a workspace parent")
        .join("target");
    for profile in ["release", "debug"] {
        let p = target.join(profile).join("kastellan-microvm-run");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Skip-as-pass unless real KVM + vsock + the browser-driver rootfs are present
/// AND the launcher is built. `locate_microvm_run` prefers `target/release`, so
/// a stale release binary silently runs OLD launcher code — rebuild it before
/// running this test (memory: firecracker-e2e-stale-release-launcher).
fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&browser_driver_image()) {
        eprintln!(
            "\n[SKIP] firecracker probe failed (need {ROOTFS_FILE} + KVM + vsock): {e}\n\
             \x20      build it with: bash scripts/workers/microvm/build-browser-driver-rootfs.sh\n"
        );
        return true;
    }
    match locate_microvm_run() {
        Some(bin) => {
            use std::sync::Once;
            static PATH_ONCE: Once = Once::new();
            PATH_ONCE.call_once(|| {
                let dir = bin.parent().unwrap().to_path_buf();
                let cur = std::env::var_os("PATH").unwrap_or_default();
                let mut paths = vec![dir];
                paths.extend(std::env::split_paths(&cur));
                let joined = std::env::join_paths(paths).expect("join PATH");
                std::env::set_var("PATH", joined);
            });
            false
        }
        None => {
            eprintln!("\n[SKIP] kastellan-microvm-run not built; run `cargo build --release -p kastellan-microvm-run`\n");
            true
        }
    }
}

fn firecracker_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(Some(SandboxBackendKind::FirecrackerVm), None)
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "browser-driver-firecracker-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

#[test]
fn vm_policy_flows_through_plan_to_in_rootfs_guest_path() {
    let plan = build_launch_plan(
        &browser_driver_vm_policy(),
        &browser_driver_image(),
        IN_ROOTFS_WORKER,
        &[],
    )
    .expect("a force-routed browser-driver VM policy must produce a launch plan");

    let token = plan
        .boot_args
        .split_whitespace()
        .find_map(|t| t.strip_prefix("kastellan.worker="))
        .expect("boot args must carry a kastellan.worker= token");
    let decoded = String::from_utf8(hex_decode(token)).expect("worker token is utf8");

    // 1a. The plan carries the program through faithfully (hex round-trips and
    //     the token is the one we asked for).
    //
    //     NOTE this assertion ALONE is tautological — it compares the decoded
    //     token against the very constant fed into `build_launch_plan`, so it
    //     stays green even if that constant points at a host build path. It is
    //     kept only as an encoding check; 1b below is what carries the real
    //     property. (An earlier revision of this test had 1a only, and passed
    //     when the constant was deliberately repointed at `target/debug` —
    //     found by exercising the negative case.)
    assert_eq!(decoded, IN_ROOTFS_WORKER, "hex token must round-trip");

    // 1b. THE REAL PIN: the path handed to the guest must be an in-rootfs path,
    //     asserted by SHAPE rather than by equality with itself. A host
    //     `target/{debug,release}` path ENOENTs inside the guest, panics PID1
    //     and boot-loops, which surfaces only as a dispatch hang to wall-clock
    //     with nothing naming the real cause (memory:
    //     vm-worker-in-rootfs-binary-path). Slice 2's `MICROVM_WORKER_BIN` must
    //     satisfy the same shape.
    assert!(
        decoded.starts_with('/'),
        "guest worker path must be absolute, got {decoded:?}"
    );
    assert!(
        !decoded.contains("/target/debug/") && !decoded.contains("/target/release/"),
        "guest worker path {decoded:?} is a HOST build-output path: it does not \
         exist inside the guest, so PID1 will ENOENT, panic and boot-loop, and \
         the dispatch will hang to wall-clock looking like a channel hang"
    );
    assert_eq!(
        decoded, "/usr/local/bin/kastellan-worker-browser-driver",
        "guest worker path must be the path baked by \
         scripts/workers/microvm/build-browser-driver-rootfs.sh"
    );

    // 2. Force-routed ⇒ the VM carries no virtio-net device at all. This is
    //    strictly stronger than the bwrap private-netns path browser-driver
    //    uses in host mode.
    assert!(
        !plan.net_enabled,
        "a force-routed VM worker must boot with no NIC"
    );

    // 3. Cmdline budget. Env is hex-encoded (two cmdline bytes per env byte),
    //    so the env set is the real constraint on this entry.
    //    `build_launch_plan` already fails closed above MAX_CMDLINE_BYTES
    //    (1920, plan.rs:137) — reaching this line proves we are under the hard
    //    cap. Assert real HEADROOM too, so that a production-sized allowlist
    //    (longer than this fixture's single host) cannot silently tip a future
    //    slice over the cap.
    let used = plan.boot_args.len();
    assert!(
        used < 1536,
        "cmdline is {used} bytes, leaving under 384 bytes of headroom below the \
         1920-byte cap; a production-sized allowlist would not fit"
    );
}

/// Live tier: boot `browser-driver.ext4` and prove Chromium launches inside it.
///
/// ## Why "the stub proxy received CONNECT" is the acceptance signal
///
/// A host `UnixListener` stands in for the egress proxy at the worker's
/// `proxy_uds`. A force-routed browser-driver VM boots, one `browser.render` is
/// dispatched, and we assert the stub **receives the worker's
/// `CONNECT example.org:443` line**.
///
/// That single line proves the whole chain at once, and each link is load-bearing:
///
/// * the VM booted and PID1 `execv`'d the in-rootfs Python entrypoint;
/// * the worker came up and served JSON-RPC over the vsock stdio bridge
///   (otherwise `dispatch` never returns a reply);
/// * `_maybe_start_shim` started the in-jail `ProxyShim` (it only starts when
///   `KASTELLAN_EGRESS_PROXY_UDS` is non-blank);
/// * **Chromium actually launched** — a browser that failed to start emits no
///   CONNECT at all, so this is a positive proof of launch, not an inference;
/// * Chromium honoured `--proxy-server` + `--proxy-bypass-list=<-loopback>`;
/// * the guest→host vsock egress relay (port 1025) carried the bytes.
///
/// This is deliberately stronger than discriminating on the render error's
/// message text. A browser-launch failure and a navigation failure both surface
/// as `RENDER_FAILED` (-32003, `errors.py`), so the JSON-RPC code cannot tell
/// them apart and the message would have to be pattern-matched — brittle across
/// Playwright versions. A received CONNECT line is a byte sequence, not a
/// string match on an error.
///
/// The render itself is EXPECTED to fail: the stub answers 503 and closes, so
/// Chromium reports a proxy/navigation error. That is fine — the render result
/// is not the signal. Completing a real render through a real sidecar is
/// slice 3.
///
/// Unlike the web-fetch VM e2e there is **no CA** here: browser-driver runs the
/// sidecar in no-MITM transparent-tunnel mode (`force_route::disable_mitm_for`
/// names this worker), because the browser does end-to-end TLS itself and
/// cannot trust our per-instance MITM CA.
///
/// DGX-only. Run:
///
///     export PATH=$HOME/.local/bin:$PATH
///     cargo build --release -p kastellan-microvm-run
///     bash scripts/workers/microvm/build-browser-driver-rootfs.sh
///     cargo test -p kastellan-core --test browser_driver_firecracker_e2e -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + browser-driver rootfs"]
async fn vm_booted_browser_driver_launches_chromium() {
    if skip_if_no_microvm() {
        return;
    }
    // Skip-as-pass without PG/supervisor/sandbox (dispatch needs a pool for audit).
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "bd-d",
        "bd-l",
        &format!("kastellan-supervisor-test-pg-browserdriver-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // Host scratch under /tmp (a share anchor) holding the stub proxy UDS.
    let dir = std::env::temp_dir().join(format!("kastellan-bd-vm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds_path = dir.join("egress.sock");
    let _ = std::fs::remove_file(&uds_path);

    // Stub "proxy": accept connections until the NAVIGATION CONNECT arrives,
    // answering every request 503 so Chromium fails fast instead of hanging.
    // A single accept would be fragile: a future Chromium may open a
    // speculative or background connection before the navigation CONNECT, and
    // that connection would consume the only slot, failing the test on browser
    // drift rather than on a real regression. Non-target request lines are
    // printed under --nocapture rather than silently swallowed, so if the
    // recv_timeout below ever fires, what DID arrive is visible.
    let listener = UnixListener::bind(&uds_path).unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let Ok(clone) = stream.try_clone() else {
                continue;
            };
            let mut line = String::new();
            let _ = BufReader::new(clone).read_line(&mut line);
            let mut w = stream;
            let _ = w.write_all(b"HTTP/1.1 503 stub\r\n\r\n");
            if line.starts_with("CONNECT example.org:443") {
                let _ = tx.send(line);
                return;
            }
            eprintln!("[stub-proxy] ignoring non-target request line: {line:?}");
        }
    });

    // Force-route the policy exactly as rewrite_worker_policy does in
    // production — minus the CA, which this worker deliberately does not get.
    let mut policy = browser_driver_vm_policy();
    policy.proxy_uds = Some(uds_path.clone());
    policy.env.push((
        "KASTELLAN_EGRESS_PROXY_UDS".into(),
        uds_path.to_string_lossy().into_owned(),
    ));

    let backend = firecracker_backend();
    let program = IN_ROOTFS_WORKER.to_string();
    let spec = WorkerSpec {
        policy: &policy,
        program: &program,
        args: &[],
        // Generous: VM boot + Playwright's Node driver + a Chromium cold start.
        wall_clock_ms: Some(120_000),
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn browser-driver in micro-VM");

    // Drive one render on a background task; we only need it to make Chromium
    // attempt egress. The assertion is the stub receiving CONNECT.
    let render = tokio::spawn(async move {
        let _ = dispatch(
            &pool,
            &Vault::new(),
            &mut worker,
            "browser-driver",
            "browser.render",
            serde_json::json!({ "url": "https://example.org/", "timeout_ms": 20000 }),
        )
        .await;
        (worker, pool)
    });

    let started = std::time::Instant::now();
    let got = rx.recv_timeout(Duration::from_secs(90)).expect(
        "stub proxy never received the navigation CONNECT from the in-VM browser (any \
         non-target request lines it DID receive are printed above): the VM failed to \
         boot, the worker failed to serve over vsock, the ProxyShim did not start, or \
         CHROMIUM FAILED TO LAUNCH (an incomplete dlopen/lib closure in the rootfs)",
    );
    // Print the evidence, not just a green line. A live VM tier that passes
    // suspiciously fast is exactly the case the project's "when tests pass but
    // feel suspicious" rule warns about, so make the received bytes and the
    // elapsed time visible under --nocapture.
    eprintln!(
        "[EVIDENCE] stub proxy received {got:?} after {:?} (VM boot + Python worker + \
         Playwright driver + Chromium launch + navigation)",
        started.elapsed()
    );
    // Belt-and-braces with the accept loop's filter: this can only fail if a
    // future edit weakens that filter, in which case it fails loudly here.
    assert!(
        got.starts_with("CONNECT example.org:443"),
        "expected CONNECT example.org:443 from the in-VM Chromium, got {got:?}"
    );

    let (worker, pool) = render.await.expect("render task joins");
    let _ = worker.close();
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}
