#![cfg(target_os = "linux")]
//! browser-driver × Firecracker micro-VM — slices 1 (the rootfs) and 2 (the
//! VM entry).
//!
//! Both tiers now drive the PRODUCTION entry via `BrowserDriverManifest::resolve`
//! under `ENABLE=1` + `USE_MICROVM=1` (see [`browser_driver_vm_entry`]). Slice 1
//! had to hand-roll an equivalent policy inline because that entry did not exist
//! yet; slice 2 replaced it, which is what binds the private
//! `MICROVM_WORKER_BIN` const to the path baked into the rootfs.
//!
//! ## Tiers
//!
//! * `vm_policy_flows_through_plan_to_in_rootfs_guest_path` — hermetic; always
//!   runs on Linux (no KVM, no network, no rootfs image needed). It feeds the
//!   resolved VM policy through the REAL `build_launch_plan` and pins that the
//!   guest execs the **in-rootfs** worker path rather than a host `target/`
//!   path. That failure mode is nasty and has cost a debugging session before:
//!   PID1 `execv`s a path that does not exist inside the guest, panics, the VM
//!   boot-loops, and the dispatch simply hangs to wall-clock — presenting as a
//!   channel hang with no error naming the real cause. It also pins the cmdline
//!   budget, because env is hex-encoded and therefore costs two cmdline bytes
//!   per env byte.
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

use kastellan_core::scheduler::ToolEntry;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::worker_manifest::{Resolution, ResolveCtx, WorkerManifest};
use kastellan_core::workers::browser_driver::BrowserDriverManifest;
use kastellan_sandbox::linux_firecracker::{build_launch_plan, FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxPolicy};
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

/// The rootfs filename produced by `build-browser-driver-rootfs.sh`.
const ROOTFS_FILE: &str = "browser-driver.ext4";

/// The production micro-VM entry under test, resolved exactly as the daemon
/// resolves it: through `BrowserDriverManifest::resolve` with `ENABLE=1` and
/// `USE_MICROVM=1`.
///
/// **Slice 2 made this the real thing, and that is the point.** Slice 1
/// hand-rolled an inline policy here because the production entry did not exist
/// yet, which left the guest worker path as three unlinked copies (the
/// Dockerfile symlink, a test const, and a literal inside an assertion). Spec
/// §10.4 flagged the consequence: a slice-2 author who typed a different
/// `MICROVM_WORKER_BIN` would get a green pin and a boot loop. Going through the
/// manifest closes that gap — the guest path, the rootfs filename, the memory
/// budget and the whole env set now come from the code that runs in production,
/// so any divergence fails these tests instead of hiding until a hang.
///
/// `exists` returns false throughout on purpose: it proves the VM branch needs
/// **no** host venv, interpreter or lockdown-exec shim on disk. Host mode would
/// return `Misconfigured` under the same probes.
fn browser_driver_vm_entry() -> ToolEntry {
    let dir = image_dir();
    let get_env = move |k: &str| match k {
        "KASTELLAN_BROWSER_DRIVER_ENABLE" | "KASTELLAN_BROWSER_DRIVER_USE_MICROVM" => {
            Some("1".to_string())
        }
        "KASTELLAN_MICROVM_DIR" => Some(dir.clone()),
        _ => None,
    };
    let exists = |_p: &std::path::Path| false;
    let allowlist = |_t: &str| vec!["example.org".to_string()];
    let ctx = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &|_p| true,
        exe_dir: None,
        canonicalize: &|_p| None,
        allowlist: &allowlist,
    };
    match BrowserDriverManifest.resolve(&ctx) {
        Resolution::Register(entry) => entry,
        Resolution::Disabled { detail } => {
            panic!("VM branch must register, got Disabled: {detail}")
        }
        Resolution::Misconfigured { detail } => panic!(
            "VM branch must register without any host venv/shim on disk, got \
             Misconfigured: {detail}"
        ),
    }
}

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

/// Force-route the production entry's policy the way `rewrite_worker_policy`
/// does at spawn: set `proxy_uds`.
///
/// The manifest deliberately leaves `proxy_uds` `None` (force-routing owns it),
/// but `build_launch_plan` **rejects** a `Net::Allowlist` VM policy without one,
/// fail-closed, because a VM carries no virtio-net device (plan.rs:255-267). So
/// every VM tier here has to apply that one spawn-time mutation itself.
///
/// Unlike web-fetch there is no CA to add: browser-driver runs its sidecar in
/// no-MITM transparent-tunnel mode (`force_route::disable_mitm_for` names this
/// worker), because the browser does end-to-end TLS itself.
fn force_routed_policy(entry: &ToolEntry, uds: PathBuf) -> SandboxPolicy {
    let mut policy = entry.policy.clone();
    policy.proxy_uds = Some(uds);
    policy
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
    // The PRODUCTION entry (slice 2), not a hand-rolled fixture: the guest path
    // asserted below is `browser_driver::MICROVM_WORKER_BIN` itself.
    let entry = browser_driver_vm_entry();
    let program = entry.binary.display().to_string();
    let plan = build_launch_plan(
        &force_routed_policy(&entry, PathBuf::from("/tmp/kastellan-egress.sock")),
        &browser_driver_image(),
        &program,
        &[],
    )
    .expect("a force-routed browser-driver VM policy must produce a launch plan");

    // The manifest must select the VM backend — otherwise everything below
    // would be asserting about a host-mode entry that never boots a VM.
    assert!(
        matches!(
            entry.sandbox_backend,
            Some(SandboxBackendKind::FirecrackerVm)
        ),
        "USE_MICROVM=1 must resolve to the Firecracker backend"
    );
    // The rootfs the entry names must be the one this file's image coordinates
    // (and the build script) refer to.
    assert!(
        entry
            .policy
            .env
            .iter()
            .any(|(k, v)| k == "KASTELLAN_MICROVM_ROOTFS" && v == ROOTFS_FILE),
        "the entry must boot {ROOTFS_FILE}, the image build-browser-driver-rootfs.sh produces"
    );

    let token = plan
        .boot_args
        .split_whitespace()
        .find_map(|t| t.strip_prefix("kastellan.worker="))
        .expect("boot args must carry a kastellan.worker= token");
    let decoded = String::from_utf8(hex_decode(token)).expect("worker token is utf8");

    // 1a. The plan carries the program through faithfully (hex round-trips).
    //     Purely an encoding check — it compares the decoded token against what
    //     was fed in, so it says nothing about whether that value is correct.
    //     1b and 1c carry the real properties.
    assert_eq!(decoded, program, "hex token must round-trip");

    // 1b. The path handed to the guest must be an in-rootfs path, asserted by
    //     SHAPE. A host `target/{debug,release}` path ENOENTs inside the guest,
    //     panics PID1 and boot-loops, which surfaces only as a dispatch hang to
    //     wall-clock with nothing naming the real cause (memory:
    //     vm-worker-in-rootfs-binary-path).
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

    // 1c. THE REAL PIN, and the reason this test drives the production manifest:
    //     `decoded` is `browser_driver::MICROVM_WORKER_BIN` carried through
    //     `resolve()` and `build_launch_plan`, so this equality binds that
    //     private const to the symlink baked by the build script. Slice 1 could
    //     only assert this against its own test constant (spec §10.4: "an
    //     instruction, not a mechanism"); now a typo in the production const
    //     fails here instead of boot-looping on the DGX.
    //
    //     If this ever fails, change whichever side is wrong — but the two must
    //     agree: `Dockerfile.browser-driver`'s `ln -sf … /usr/local/bin/…` and
    //     `MICROVM_WORKER_BIN` in `core/src/workers/browser_driver.rs`.
    assert_eq!(
        decoded, "/usr/local/bin/kastellan-worker-browser-driver",
        "the production MICROVM_WORKER_BIN must equal the path baked by \
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

    // Boot the PRODUCTION entry (slice 2) and force-route it exactly as
    // rewrite_worker_policy does at spawn — minus the CA, which this worker
    // deliberately does not get (no-MITM transparent tunnel).
    let entry = browser_driver_vm_entry();
    let mut policy = force_routed_policy(&entry, uds_path.clone());
    policy.env.push((
        "KASTELLAN_EGRESS_PROXY_UDS".into(),
        uds_path.to_string_lossy().into_owned(),
    ));

    let backend = firecracker_backend();
    let program = entry.binary.display().to_string();
    let spec = WorkerSpec {
        policy: &policy,
        program: &program,
        args: &[],
        // The entry's own budget (90 s: VM boot + Playwright's Node driver + a
        // Chromium cold start), so this tier exercises the production value
        // rather than a more generous test-only one that could mask a
        // too-tight manifest setting.
        wall_clock_ms: entry.wall_clock_ms,
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
