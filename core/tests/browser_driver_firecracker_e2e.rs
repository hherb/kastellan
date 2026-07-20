#![cfg(target_os = "linux")]
//! browser-driver × Firecracker micro-VM — slices 1 (the rootfs), 2 (the VM
//! entry) and 3 (a live render through a real egress sidecar).
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
//! * `vm_booted_browser_driver_launches_chromium` — the slice-2 live DGX tier
//!   (`#[ignore]`): boots `browser-driver.ext4` and proves Chromium starts
//!   inside the guest. Its stub proxy 503s, so the render deliberately fails and
//!   the received `CONNECT` line is the signal.
//!
//! * `vm_renders_real_page_through_real_sidecar` — **slice 3's acceptance tier**
//!   (`#[ignore]`, DGX + outbound HTTPS): the first real page ever rendered
//!   inside the VM, through a real egress-proxy sidecar driven by the production
//!   `SingleUseLifecycle::with_force_routing` manager. Asserts real returned
//!   text, an `allowed` sidecar decision, and wall-clock headroom.
//!
//! * `vm_render_of_heavy_page_stays_within_memory_budget` — slice 3's
//!   measurement tier (`#[ignore]`): renders a heavy page and samples the
//!   Firecracker VMM's peak RSS, turning `mem_mb: 2048` from a reasoned value
//!   into a measured one.
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

use kastellan_core::broker::BrokerConfigs;
use kastellan_core::egress::audit::EgressAuditRow;
use kastellan_core::scheduler::ToolEntry;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::worker_lifecycle::force_route::{DecisionSinkFactory, ForceRoutingConfig};
use kastellan_core::worker_lifecycle::{SingleUseLifecycle, WorkerLifecycleManager};
use kastellan_core::worker_manifest::{Resolution, ResolveCtx, WorkerManifest};
use kastellan_core::workers::browser_driver::BrowserDriverManifest;
use kastellan_sandbox::linux_firecracker::build_launch_plan;
use kastellan_sandbox::{SandboxBackendKind, SandboxBackends, SandboxPolicy};
use kastellan_tests_common::microvm::{
    firecracker_backend, firecracker_image_for, image_dir, skip_if_no_microvm,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, egress_proxy_bin_or_skip, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_origin_unreachable, skip_if_sandbox_unavailable, unique_suffix,
};

/// The rootfs filename produced by `build-browser-driver-rootfs.sh`.
const VM_ROOTFS: &str = "browser-driver.ext4";

/// The acceptance origin for the slice-3 live render (§3/§4.2 of the design
/// spec). It must be a **real public HTTPS host**: browser-driver's sidecar runs
/// in no-MITM transparent-tunnel mode, so Chromium does end-to-end TLS and has
/// to trust the origin's certificate on its own root store. A hermetic
/// self-signed loopback origin would need a CA in Chromium's NSS store — the
/// deferred MITM-of-browser work — which is why no real render through a real
/// sidecar had ever completed before this slice, in VM *or* host mode.
///
/// `example.org` specifically: a tiny page whose `Example Domain` heading has
/// been invariant for years, so the acceptance gate does not flake on content
/// drift.
const DEFAULT_ORIGIN_HOST: &str = "example.org";
const DEFAULT_ORIGIN_URL: &str = "https://example.org/";
/// Stable needle in `example.org`'s rendered text.
const DEFAULT_ORIGIN_NEEDLE: &str = "Example Domain";

/// A deliberately heavy real page for the memory measurement: a large DOM,
/// unlike [`DEFAULT_ORIGIN_URL`], which is far too small to exercise the
/// `/tmp`-tmpfs-versus-guest-RAM question at all.
///
/// Only this host is allowlisted, so **cross-host subresources — notably the
/// article's images on `upload.wikimedia.org` — are egress-blocked** and the
/// render is DOM + same-origin CSS/JS only. That makes the measured peak a
/// *floor*: a page whose image assets were all in-allowlist would decode them
/// into exactly the shm-in-tmpfs memory this tier measures. Deliberate anyway —
/// this tier was split off to isolate content-drift risk, and widening the
/// allowlist would couple the reading to Wikipedia's image weight too.
const HEAVY_ORIGIN_HOST: &str = "en.wikipedia.org";
const HEAVY_ORIGIN_URL: &str = "https://en.wikipedia.org/wiki/Rust_(programming_language)";

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
    browser_driver_vm_entry_for(&[DEFAULT_ORIGIN_HOST.to_string()])
}

/// As [`browser_driver_vm_entry`], but with an explicit content allowlist.
///
/// Slice 3 renders against two different origins (a stable tiny page and a heavy
/// one), so the host list can no longer be a constant. The rows are handed to
/// the manifest exactly as `tool_allowlists` would supply them — bare hosts,
/// which `allowlist_to_net_entries` maps to `{host}:443` (#469: a bare host
/// reaching the proxy unmapped is an all-port grant).
fn browser_driver_vm_entry_for(hosts: &[String]) -> ToolEntry {
    let dir = image_dir();
    let get_env = move |k: &str| match k {
        "KASTELLAN_BROWSER_DRIVER_ENABLE" | "KASTELLAN_BROWSER_DRIVER_USE_MICROVM" => {
            Some("1".to_string())
        }
        "KASTELLAN_MICROVM_DIR" => Some(dir.clone()),
        _ => None,
    };
    let exists = |_p: &std::path::Path| false;
    let hosts = hosts.to_vec();
    let allowlist = move |_t: &str| hosts.clone();
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

/// Peak resident memory of the Firecracker VMM process, in MiB, sampled from the
/// host while a render is in flight.
///
/// **Why the VMM's host RSS is the right proxy for guest memory.** Firecracker
/// allocates guest RAM lazily, so its RSS grows to track the pages the guest has
/// actually touched — including the `/tmp` tmpfs pages that
/// `--disable-dev-shm-usage` redirects Chromium's shared memory into. That
/// redirect is exactly why `mem_mb` had to be raised to 2048 for the VM entry
/// (slice-1 spec §10.1, slice-2 manifest doc), and it is the quantity this
/// sampler measures.
///
/// VMMs already alive when sampling starts are **excluded**: another tier's VM
/// or a stale spike VM is not ours, and attributing its RSS to this render
/// would corrupt the reading in either direction. The VM this sampler brackets
/// boots strictly after the snapshot, so exclusion by pre-existing PID is safe.
///
/// Returns `None` when no *new* `firecracker` process was seen at all — the
/// caller treats that as a failure, not as "nothing to assert".
fn sample_peak_vmm_rss_mib(stop: &std::sync::atomic::AtomicBool) -> Option<u64> {
    use std::sync::atomic::Ordering;
    let preexisting: Vec<u32> =
        firecracker_vmm_rss_kb().into_iter().map(|(pid, _)| pid).collect();
    let mut peak: Option<u64> = None;
    while !stop.load(Ordering::Relaxed) {
        for (pid, kb) in firecracker_vmm_rss_kb() {
            if preexisting.contains(&pid) {
                continue;
            }
            let mib = kb / 1024;
            peak = Some(peak.map_or(mib, |p: u64| p.max(mib)));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    peak
}

/// Live `firecracker` VMM processes, as `(pid, current VmRSS in KiB)`.
fn firecracker_vmm_rss_kb() -> Vec<(u32, u64)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let pid_dir = entry.path();
        // `comm` is the 15-char-truncated process name; "firecracker" fits.
        let Ok(comm) = std::fs::read_to_string(pid_dir.join("comm")) else {
            continue;
        };
        if comm.trim() != "firecracker" {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(pid_dir.join("status")) else {
            continue;
        };
        for line in status.lines() {
            let Some(rest) = line.strip_prefix("VmRSS:") else {
                continue;
            };
            // "VmRSS:\t  123456 kB"
            if let Some(kb) = rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()) {
                out.push((pid, kb));
            }
        }
    }
    out
}

/// Boot the production browser-driver VM entry through the **real daemon
/// manager** and drive one `browser.render`.
///
/// Returns `(render_result, egress_decisions, elapsed)`.
///
/// **Why the manager rather than a hand-wired `NetWorkerSpawn`** (design spec
/// §4.1): `SingleUseLifecycle::with_force_routing(...).acquire(...)` is the real
/// daemon path. It resolves the *worker* backend from `entry.sandbox_backend`
/// (`FirecrackerVm`) and the *sidecar* backend from `resolve(None, None)` (host
/// bwrap), and — the load-bearing part here — it derives `disable_mitm` by
/// calling `force_route::disable_mitm_for(worker_name)` itself. A hand-wired
/// spawn would have this test *assert* `disable_mitm: true`, consulting no
/// production code. Under the manager, dropping `browser-driver` from
/// `disable_mitm_for` makes the sidecar terminate TLS and the render fails on an
/// untrusted certificate.
async fn render_in_vm_through_real_sidecar(
    pool: &sqlx::PgPool,
    proxy_bin: PathBuf,
    hosts: &[String],
    url: &str,
    timeout_ms: u64,
) -> (
    Result<serde_json::Value, kastellan_core::tool_host::ToolHostError>,
    Vec<String>,
    Duration,
) {
    let entry = browser_driver_vm_entry_for(hosts);

    let decisions: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_src = Arc::clone(&decisions);
    let make_sink: DecisionSinkFactory = Box::new(move || {
        let d = Arc::clone(&sink_src);
        Box::new(move |row: EgressAuditRow| {
            d.lock().unwrap().push(format!("{} {}", row.action, row.payload));
        })
    });
    // Scratch under /tmp: a SHARE_ANCHOR, so the confined VMM jail can bind the
    // sidecar's UDS and the guest→host vsock relay can reach it.
    let force = Arc::new(ForceRoutingConfig::new(
        proxy_bin,
        std::env::temp_dir(),
        make_sink,
        None, // no cert pins
    ));
    let sandboxes = Arc::new(SandboxBackends::default_for_current_os());
    let mgr = SingleUseLifecycle::with_force_routing(sandboxes, Some(force), BrokerConfigs::default());

    let started = std::time::Instant::now();
    let mut handle = mgr
        .acquire("browser-driver", &entry)
        .await
        .expect("acquire a force-routed browser-driver VM worker through the manager");
    let result = dispatch(
        pool,
        &Vault::new(),
        handle.worker_mut(),
        "browser-driver",
        "browser.render",
        serde_json::json!({ "url": url, "wait_until": "load", "timeout_ms": timeout_ms }),
    )
    .await;
    let elapsed = started.elapsed();

    // Decisions land on a detached ingest thread reading the proxy's stdout;
    // poll briefly so we do not race it.
    let mut snapshot = Vec::new();
    for _ in 0..60 {
        snapshot = decisions.lock().unwrap().clone();
        if !snapshot.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (result, snapshot, elapsed)
}

/// The post-JS readable text of a `browser.render` reply.
fn rendered_text(v: &serde_json::Value) -> String {
    v["text"].as_str().unwrap_or_default().to_string()
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
        &firecracker_image_for(VM_ROOTFS),
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
            .any(|(k, v)| k == "KASTELLAN_MICROVM_ROOTFS" && v == VM_ROOTFS),
        "the entry must boot {VM_ROOTFS}, the image build-browser-driver-rootfs.sh produces"
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
    if skip_if_no_microvm(VM_ROOTFS) {
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

/// Slice 3, tier (a) — **the first real page ever rendered inside the micro-VM**,
/// through a real `kastellan-worker-egress-proxy` sidecar.
///
/// Slices 1 and 2 stopped at the `CONNECT` line: their stub proxy answers 503 and
/// closes, so the render deliberately fails and the render *result* carries no
/// information. Everything after the CONNECT — end-to-end TLS to the origin, the
/// response body, Playwright's post-JS DOM extraction, the JSON-RPC reply
/// carrying real text — was untested in a VM until this test.
///
/// ## Why a real public origin, and not a hermetic one
///
/// browser-driver's sidecar runs in **no-MITM transparent-tunnel** mode
/// (`force_route::disable_mitm_for` names this worker), because the browser does
/// its own end-to-end TLS and cannot trust our per-instance MITM CA. So Chromium
/// must trust the ORIGIN's certificate on its own root store, and a hermetic
/// self-signed loopback origin would need a CA installed in Chromium's NSS store
/// inside the rootfs — the deferred MITM-of-browser work.
///
/// That constraint is why no real render through a real sidecar had ever
/// completed in this repo, in VM *or* host mode: the host-mode
/// `browser_driver_e2e::forced_render_of_loopback_page_through_sidecar` navigates
/// `https://` at a plain-HTTP loopback server precisely because the handshake
/// cannot succeed, and settles for the sidecar decision row as its signal. See
/// the design spec §3 for the full option table (a `--ignore-certificate-errors-*`
/// flag was rejected: it would weaken PRODUCTION launch args to make a test pass).
///
/// ## What is asserted
///
/// 1. The dispatch **succeeds** and the post-JS text contains `Example Domain` —
///    a completed render, not merely an attempt.
/// 2. A sidecar decision `allowed` for `example.org:443` — the render went
///    *through* the sidecar, not around it.
/// 3. Real wall-clock **headroom** under the entry's own `wall_clock_ms`. Merely
///    finishing is not enough: finishing at 89 s under a 90 s budget is a latent
///    failure, so the assertion is a fraction of budget, not a bare completion.
///
/// DGX-only. Needs outbound HTTPS. Run:
///
///     export PATH=$HOME/.local/bin:$PATH
///     cargo build --release -p kastellan-microvm-run
///     cargo build -p kastellan-worker-egress-proxy
///     cargo test -p kastellan-core --test browser_driver_firecracker_e2e -- \
///         --test-threads=1 --ignored --nocapture
///
/// `--test-threads=1` matters: each ignored tier boots its own VM, and the
/// memory tier attributes RSS to the one firecracker process that appears
/// after its sampler starts — tiers racing in parallel would defeat that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + browser-driver rootfs + egress proxy + outbound HTTPS"]
async fn vm_renders_real_page_through_real_sidecar() {
    if skip_if_no_microvm(VM_ROOTFS) || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    if skip_if_origin_unreachable(DEFAULT_ORIGIN_HOST) {
        return;
    }
    let Some(proxy_bin) = egress_proxy_bin_or_skip() else {
        return;
    };
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "bd-r",
        "bd-rl",
        &format!("kastellan-supervisor-test-pg-bdrender-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let hosts = vec![DEFAULT_ORIGIN_HOST.to_string()];
    let (result, decisions, elapsed) = render_in_vm_through_real_sidecar(
        &pool,
        proxy_bin,
        &hosts,
        DEFAULT_ORIGIN_URL,
        20_000,
    )
    .await;

    // Print the sidecar's own verdicts before asserting: on failure these say
    // whether the browser reached egress at all, and what the proxy decided.
    for line in &decisions {
        eprintln!("[egress-decision] {line}");
    }

    let value = result.expect(
        "browser.render must SUCCEED against a real HTTPS origin through the real sidecar — \
         this is slice 3's whole point. A failure here means the VM booted and Chromium \
         launched (slice 2 proves that separately) but the render did not complete: check \
         the egress decisions printed above for a block, and check that the origin's TLS \
         completed end-to-end (the sidecar must be in no-MITM transparent-tunnel mode)",
    );
    let text = rendered_text(&value);
    eprintln!(
        "[EVIDENCE] rendered {DEFAULT_ORIGIN_URL} in the micro-VM in {elapsed:?}; \
         status={} text={} bytes",
        value["status"],
        text.len()
    );
    assert_eq!(value["status"], 200, "render result: {value}");
    assert!(
        text.contains(DEFAULT_ORIGIN_NEEDLE),
        "expected {DEFAULT_ORIGIN_NEEDLE:?} in the rendered text, got {text:?}"
    );

    // The render must have gone THROUGH the sidecar. Match host AND port: a bare
    // host check would pass on any decision mentioning the host (#469's
    // all-port-grant lesson applies to assertions too).
    // NB the action is `egress.allowed`, not `allowed` — `decision_to_audit`
    // namespaces every verdict under `egress.` (its siblings are
    // `egress.blocked.credential_leak` / `egress.blocked.tls_pin`).
    let allowed_row = decisions
        .iter()
        .find(|d| {
            d.starts_with("egress.allowed")
                && d.contains(&format!("\"host\":\"{DEFAULT_ORIGIN_HOST}\""))
                && d.contains("\"port\":443")
        })
        .unwrap_or_else(|| {
            panic!(
                "no `egress.allowed {DEFAULT_ORIGIN_HOST}:443` decision: the page rendered \
                 but not provably through the sidecar. Decisions: {decisions:?}"
            )
        });

    // Transparent tunnel, not MITM: the sidecar must NOT have terminated TLS on
    // THIS connection — the check is scoped to the allowed row above, not to any
    // decision that happens to carry the flag. This is the production property
    // `force_route::disable_mitm_for` carries, observed here rather than
    // restated — Chromium validated example.org's real certificate itself,
    // which is what makes a real origin necessary at all. (NB `decision_to_audit`
    // defaults a MISSING `tls_intercepted` to false, so what this catches is the
    // sidecar reporting `true`, i.e. actively MITM-ing browser traffic.)
    assert!(
        allowed_row.contains("\"tls_intercepted\":false"),
        "the allowed decision for {DEFAULT_ORIGIN_HOST}:443 is not a transparent-tunnel \
         (non-MITM) one; a MITM'd connection would mean disable_mitm_for no longer names \
         this worker, and Chromium would reject our per-instance CA. Row: {allowed_row}"
    );

    // Wall-clock headroom against the entry's OWN budget, so this measures the
    // production value rather than a test-local one.
    let budget = browser_driver_vm_entry()
        .wall_clock_ms
        .expect("the VM entry sets a wall-clock budget");
    let used_pct = (elapsed.as_millis() as f64 / budget as f64) * 100.0;
    eprintln!("[EVIDENCE] wall clock: {elapsed:?} = {used_pct:.1}% of the {budget} ms budget");
    assert!(
        used_pct < 70.0,
        "render used {used_pct:.1}% of the {budget} ms wall-clock budget, leaving under \
         30% headroom: the budget in browser_driver_firecracker_entry is too tight for a \
         cold VM boot + Playwright driver + Chromium cold start + a real navigation"
    );

    pool.close().await;
}

/// Slice 3, tier (b) — **the memory measurement**: render a heavy real page and
/// check what it actually costs in guest RAM.
///
/// ## The question this answers
///
/// The guest has no `/dev/shm` (slice-1 spec §10.1), so Chromium runs with an
/// unconditional `--disable-dev-shm-usage`, which redirects its shared memory
/// into the guest `/tmp` tmpfs. That tmpfs is drawn from the **same** `mem_mb`
/// budget as everything else rather than from a separate device — so a heavy
/// page's shared-memory allocations compete with guest RAM, and if they tip over
/// the VM OOMs *with* `test_disable_dev_shm_usage_is_pinned` green throughout
/// (that test pins the flag, not the budget). Slice 2 set `mem_mb: 2048` by
/// reasoning; nothing had measured it.
///
/// ## How it is measured
///
/// Firecracker allocates guest RAM lazily, so the VMM process's host RSS tracks
/// the pages the guest has actually touched — tmpfs pages included. A sampler
/// thread walks `/proc/*/comm` for `firecracker` during the render and keeps the
/// peak (see [`sample_peak_vmm_rss_mib`]).
///
/// **A missing sample fails the test rather than skipping the assertion.** The
/// render succeeded, so a VM demonstrably ran; finding no firecracker process
/// means the sampler is broken, and quietly skipping would be exactly the
/// false-green pattern CLAUDE.md's "when tests pass but feel suspicious" rule
/// exists to prevent.
///
/// Split from the acceptance tier on purpose: this one depends on a page whose
/// weight can drift, and isolating that risk keeps the acceptance gate stable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + browser-driver rootfs + egress proxy + outbound HTTPS"]
async fn vm_render_of_heavy_page_stays_within_memory_budget() {
    if skip_if_no_microvm(VM_ROOTFS) || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    if skip_if_origin_unreachable(HEAVY_ORIGIN_HOST) {
        return;
    }
    let Some(proxy_bin) = egress_proxy_bin_or_skip() else {
        return;
    };
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "bd-m",
        "bd-ml",
        &format!("kastellan-supervisor-test-pg-bdmem-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler_stop = Arc::clone(&stop);
    let sampler = thread::spawn(move || sample_peak_vmm_rss_mib(&sampler_stop));

    let hosts = vec![HEAVY_ORIGIN_HOST.to_string()];
    let (result, decisions, elapsed) =
        render_in_vm_through_real_sidecar(&pool, proxy_bin, &hosts, HEAVY_ORIGIN_URL, 45_000).await;

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let peak = sampler.join().expect("sampler thread joins");

    for line in &decisions {
        eprintln!("[egress-decision] {line}");
    }
    let value = result.expect(
        "browser.render of the heavy page must succeed: a failure here is the OOM this \
         test exists to detect (or an egress block — check the decisions above)",
    );
    let text = rendered_text(&value);
    eprintln!(
        "[EVIDENCE] rendered {HEAVY_ORIGIN_URL} in {elapsed:?}; status={} text={} bytes",
        value["status"],
        text.len()
    );
    assert_eq!(value["status"], 200, "render result: {value}");
    assert!(
        text.len() > 5_000,
        "expected a substantial page (>5 KiB of text) so the memory measurement is \
         meaningful, got {} bytes — did the article move or get blocked?",
        text.len()
    );

    let peak = peak.expect(
        "no `firecracker` process was ever seen in /proc while the render was in flight, \
         yet the render succeeded — so a VM certainly ran and the SAMPLER is broken. \
         Failing loudly rather than skipping the memory assertion, which would be a \
         silent false green",
    );
    // Memory budget from the entry's OWN policy — the same "measure the
    // production value, not a test-local mirror" rule the acceptance tier
    // applies to wall_clock_ms. Firecracker enforces `policy.mem_mb` as the
    // guest RAM size, so this is the number the guest can actually OOM against.
    let budget_mb = browser_driver_vm_entry().policy.mem_mb;
    let used_pct = (peak as f64 / budget_mb as f64) * 100.0;
    eprintln!(
        "[EVIDENCE] peak Firecracker VMM RSS {peak} MiB = {used_pct:.1}% of the \
         {budget_mb} MiB guest budget (Chromium + the /tmp tmpfs holding its shm)"
    );
    assert!(
        used_pct < 85.0,
        "peak guest memory {peak} MiB is {used_pct:.1}% of the {budget_mb} MiB budget, \
         leaving under 15% headroom — raise mem_mb in browser_driver_firecracker_entry; \
         a heavier page would OOM the VM, and because --disable-dev-shm-usage redirects \
         Chromium's shm into the guest /tmp tmpfs that OOM competes with guest RAM"
    );

    pool.close().await;
}
