//! End-to-end tests for the Linux bwrap backend. These actually invoke `bwrap`,
//! so they only run on Linux and require `bwrap` on `$PATH`.

#![cfg(target_os = "linux")]

use std::io::Read;
use std::path::PathBuf;

use kastellan_sandbox::{linux_bwrap::LinuxBwrap, SandboxBackend, SandboxPolicy};

/// Skip the test if this host's kernel won't let us create an unprivileged
/// user namespace. Ubuntu 24.04+ requires an AppArmor profile for bwrap;
/// tests should report a clear hint rather than fail with an opaque error.
fn skip_if_no_userns() -> bool {
    match LinuxBwrap::probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] bwrap probe failed: {e}\n");
            true
        }
    }
}

fn strict_policy() -> SandboxPolicy {
    SandboxPolicy {
        cpu_ms: 5_000,
        ..SandboxPolicy::default()
    }
}

fn read_to_string(handle: &mut Option<impl Read>) -> String {
    let mut s = String::new();
    if let Some(h) = handle.as_mut() {
        let _ = h.read_to_string(&mut s);
    }
    s
}

#[test]
fn echo_runs_inside_sandbox() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/usr/bin/echo", &["hello-from-jail"])
        .expect("bwrap should spawn echo");
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "echo exited non-zero: {status:?}, stderr={}",
        read_to_string(&mut child.stderr)
    );
    let stdout = read_to_string(&mut child.stdout);
    assert_eq!(stdout.trim_end(), "hello-from-jail");
}

#[test]
fn host_etc_passwd_is_invisible_when_not_in_policy() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    // /etc is not bound, so /etc/passwd should not exist inside the jail.
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/usr/bin/cat", &["/etc/passwd"])
        .expect("bwrap should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "cat /etc/passwd should fail inside sandbox; stdout={} stderr={}",
        read_to_string(&mut child.stdout),
        read_to_string(&mut child.stderr)
    );
    let stderr = read_to_string(&mut child.stderr);
    assert!(
        stderr.to_lowercase().contains("no such file"),
        "expected 'No such file', got stderr: {stderr:?}"
    );
}

#[test]
fn host_home_is_invisible_when_not_in_policy() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    // The jail must not see the user's home dir under any circumstance unless
    // it was explicitly listed.
    let probe = "/home";
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/usr/bin/ls", &[probe])
        .expect("bwrap should spawn ls");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);
    // Either ls fails because /home doesn't exist, or it succeeds but lists
    // nothing. Both are acceptable; what's NOT acceptable is seeing real users.
    assert!(
        !stdout.contains("hherb"),
        "sandbox leaked the host's home directory! stdout={stdout:?}"
    );
    let _ = (status, stderr); // unused but kept for diagnostic context
}

#[test]
fn fs_read_path_is_visible_when_listed() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    let mut policy = strict_policy();
    policy.fs_read.push(PathBuf::from("/etc/hostname"));
    let mut child = backend
        .spawn_under_policy(&policy, "/usr/bin/cat", &["/etc/hostname"])
        .expect("bwrap should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "cat /etc/hostname should succeed when listed; stderr={}",
        read_to_string(&mut child.stderr)
    );
    let stdout = read_to_string(&mut child.stdout);
    assert!(!stdout.is_empty(), "expected non-empty hostname");
}

#[test]
fn net_is_unreachable_under_deny() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    // `getent hosts ...` performs a DNS lookup, which requires the host
    // network namespace. With Net::Deny we unshare net, so this MUST fail.
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/usr/bin/getent", &["hosts", "example.com"])
        .expect("bwrap should spawn getent");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "getent hosts should fail under Net::Deny — sandbox leaked the network namespace"
    );
}

/// Locate the `mem_burner` test fixture built by the sandbox crate's
/// `[[bin]]` stanza into `target/debug/mem_burner`. Mirrors the locator
/// pattern in `core/tests/shell_exec_e2e.rs::worker_binary` so future
/// readers find one consistent layout.
fn mem_burner_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("mem_burner")
}

#[test]
fn worker_with_low_mem_max_is_oom_killed() {
    // Negative test for the cgroup v2 enforcement layer added on top of
    // bwrap (see `linux_cgroup.rs`). Strategy:
    //   1. Build the `mem_burner` fixture into `target/debug/mem_burner`.
    //   2. Spawn it under a policy whose `mem_mb = 32` so
    //      `MemoryMax=32M` is set on the transient cgroup scope.
    //   3. Tell the fixture to allocate **256 MiB** — eight times the
    //      cap — and touch every page so the kernel actually accounts
    //      the memory. The cgroup OOM killer fires and the inner
    //      process is SIGKILL'd.
    //   4. Assert the parent (`Child` for `systemd-run`) reflects the
    //      kill: non-success exit. On glibc/Linux the propagated
    //      signal is SIGKILL (9); we accept any non-success exit so
    //      the test isn't over-specified to one libc / one systemd
    //      version.
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    let mem_burner = mem_burner_binary();
    if !mem_burner.exists() {
        eprintln!(
            "\n[SKIP] mem_burner fixture not built at {}; run `cargo build -p kastellan-sandbox`",
            mem_burner.display()
        );
        return;
    }

    let mut policy = strict_policy();
    policy.mem_mb = 32; // tight cap; mem_burner will try to use 256 MiB
    // Bind the fixture binary into the jail so /usr/bin/... isn't the
    // only thing visible. The fs_read entry resolves to a read-only
    // single-file bind mount inside bwrap.
    policy.fs_read.push(mem_burner.clone());

    let mut child = backend
        .spawn_under_policy(
            &policy,
            mem_burner.to_str().expect("path is utf-8"),
            &["--mb", "256"],
        )
        .expect("systemd-run + bwrap should spawn");
    let status = child.wait().expect("wait");

    assert!(
        !status.success(),
        "mem_burner should have been OOM-killed by the cgroup but exited cleanly. \
         stderr={} \
         (cgroup MemoryMax=32M was set; if the worker survived allocating 256 MiB \
         then either the cgroup wrapping is missing or the limit isn't being applied)",
        read_to_string(&mut child.stderr)
    );
}

#[test]
fn tmp_is_per_spawn_ephemeral_tmpfs_under_worker_strict() {
    // Regression pin for issue #89: bwrap's `--tmpfs /tmp` (see
    // `linux_bwrap::build_argv`) is load-bearing for every Python worker
    // that uses /tmp — HF cache, Python `tempfile`, NumPy mmap, and the
    // `TORCHINDUCTOR_CACHE_DIR=/tmp/torchinductor` set by the GLiNER-Relex
    // worker in `core/src/workers/gliner_relex.rs`. The comment there says
    // "/tmp is tmpfs inside the sandbox so this is ephemeral per-spawn";
    // until this test, that invariant was only asserted by comments + the
    // bwrap argv shape, with no positive runtime pin.
    //
    // If a future refactor switches `--tmpfs /tmp` to `--bind /tmp /tmp`
    // (e.g. to expose host caches for performance), two concurrent
    // workers would race on `/tmp/torchinductor` — the symptom would be
    // intermittent cache corruption, hard to attribute and harder to
    // bisect. This test catches that drift.
    //
    // Three properties asserted in one spawn pair:
    //   1. A marker written in spawn A is **not** visible in spawn B
    //      (ephemerality / per-spawn isolation).
    //   2. Spawn B can also write under /tmp (writability — guards
    //      against an over-correction that mounts /tmp read-only).
    //   3. Spawn B's own freshly-written marker IS in its own /tmp
    //      listing (sanity check that the listing is real and not
    //      silently empty — without this, a future bug where /tmp
    //      rejected reads could masquerade as "ephemerality holds").
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();

    // Unique marker so concurrent test runs don't collide on a fake
    // "leak" hit if /tmp ever did become shared. PID + nanos is overkill
    // for the local case but cheap insurance against parallel CI lanes
    // sharing a host /tmp namespace if the invariant ever broke.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let spawn_a_marker = format!("probe-spawn-a-{}-{nanos}", std::process::id());
    let spawn_b_marker = format!("probe-spawn-b-{}-{nanos}", std::process::id());

    // Spawn A: write the spawn-A marker, exit. `/bin/sh` resolves
    // through the `--symlink usr/bin /bin` mapping the backend
    // already injects (see `linux_bwrap::build_argv`), so this
    // works whether or not the host has `/usr/bin/sh` directly.
    let mut child_a = backend
        .spawn_under_policy(
            &strict_policy(),
            "/bin/sh",
            &["-c", &format!("touch /tmp/{spawn_a_marker}")],
        )
        .expect("bwrap should spawn /bin/sh for spawn A");
    let status_a = child_a.wait().expect("wait for spawn A");
    assert!(
        status_a.success(),
        "spawn A (touch /tmp/{spawn_a_marker}) failed: {status_a:?}, stderr={}",
        read_to_string(&mut child_a.stderr)
    );

    // Spawn B: write a different marker AND list /tmp.
    // - exit 0  => writability of /tmp under WorkerStrict (no EROFS).
    // - stdout MUST NOT contain spawn_a_marker  => ephemerality.
    // - stdout MUST contain spawn_b_marker      => listing is real.
    let cmd_b = format!("touch /tmp/{spawn_b_marker} && ls /tmp");
    let mut child_b = backend
        .spawn_under_policy(&strict_policy(), "/bin/sh", &["-c", &cmd_b])
        .expect("bwrap should spawn /bin/sh for spawn B");
    let status_b = child_b.wait().expect("wait for spawn B");
    let stdout_b = read_to_string(&mut child_b.stdout);
    let stderr_b = read_to_string(&mut child_b.stderr);
    assert!(
        status_b.success(),
        "spawn B ({cmd_b}) failed: {status_b:?}, stdout={stdout_b:?} stderr={stderr_b:?} \
         — /tmp may have been mounted read-only (EROFS)?",
    );
    assert!(
        !stdout_b.contains(&spawn_a_marker),
        "/tmp leaked across spawns! spawn A wrote {spawn_a_marker:?} and spawn B saw it. \
         ls /tmp from spawn B = {stdout_b:?}",
    );
    assert!(
        stdout_b.contains(&spawn_b_marker),
        "spawn B's own marker missing from its own /tmp listing — listing may be empty or /tmp \
         may be read-only. spawn_b_marker={spawn_b_marker:?}, ls /tmp = {stdout_b:?}",
    );
}

#[test]
fn relative_policy_paths_are_rejected() {
    let backend = LinuxBwrap::new();
    let mut policy = strict_policy();
    policy.fs_read.push(PathBuf::from("relative/path"));
    let res = backend.spawn_under_policy(&policy, "/usr/bin/true", &[]);
    assert!(matches!(
        res,
        Err(kastellan_sandbox::SandboxError::Backend(_))
    ));
}
