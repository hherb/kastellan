//! Unit tests for the `macos_container` module.
//!
//! `use super::*` resolves to the parent `macos_container` module per the
//! Rust 2018 sibling-directory module pattern (the same pattern used by
//! `injection_guard/tests.rs`, `tool_host/tests.rs`, and
//! `gliner_relex/tests.rs`), giving these tests access to the private
//! helpers (`clamp_memory_to_minimum`, `build_container_argv`,
//! `build_image_inspect_argv`, …), the `ClampedMemory` struct, and the
//! `CONTAINER_*` consts alongside the public `MacosContainer` API.
//! Integration tests that exercise the backend through the real Apple
//! `container` micro-VM live in `sandbox/tests/macos_container_smoke.rs`.

use super::*;
use crate::{Net, Profile};
use std::path::PathBuf;

// ---------- clamp_memory_to_minimum ----------

#[test]
fn clamp_raises_one_mib_to_floor_and_flags_clamping() {
    let out = clamp_memory_to_minimum(1);
    assert_eq!(
        out,
        ClampedMemory {
            mib: CONTAINER_MEM_MIN_MIB,
            clamped: true,
        }
    );
}

#[test]
fn clamp_raises_one_hundred_mib_to_floor_and_flags_clamping() {
    let out = clamp_memory_to_minimum(100);
    assert_eq!(
        out,
        ClampedMemory {
            mib: 200,
            clamped: true,
        }
    );
}

#[test]
fn clamp_passes_through_two_fifty_six_mib_without_clamping() {
    let out = clamp_memory_to_minimum(256);
    assert_eq!(
        out,
        ClampedMemory {
            mib: 256,
            clamped: false,
        }
    );
}

#[test]
fn clamp_passes_through_one_gib_without_clamping() {
    let out = clamp_memory_to_minimum(1024);
    assert_eq!(
        out,
        ClampedMemory {
            mib: 1024,
            clamped: false,
        }
    );
}

/// Direct call with `0` clamps to the floor and flags clamping. The
/// only in-tree callsite guards `mem_mb > 0` before calling, so this
/// path is unreachable from `build_container_argv` today — pinned to
/// match the docstring's documented behaviour for any future direct
/// caller (e.g. a different backend reusing the helper).
#[test]
fn clamp_zero_raises_to_floor_and_flags_clamping() {
    let out = clamp_memory_to_minimum(0);
    assert_eq!(
        out,
        ClampedMemory {
            mib: CONTAINER_MEM_MIN_MIB,
            clamped: true,
        }
    );
}

/// Exact-floor input is NOT clamped (the boundary is inclusive on the
/// "above" side). Pinned so a future "fix" to `<=` doesn't silently
/// log every container spawn at the floor.
#[test]
fn clamp_at_exact_floor_does_not_flag_clamping() {
    let out = clamp_memory_to_minimum(CONTAINER_MEM_MIN_MIB);
    assert_eq!(
        out,
        ClampedMemory {
            mib: CONTAINER_MEM_MIN_MIB,
            clamped: false,
        }
    );
}

// ---------- build_container_argv ----------

fn strict_policy() -> SandboxPolicy {
    SandboxPolicy::default()
}

fn netclient_policy() -> SandboxPolicy {
    SandboxPolicy {
        profile: Profile::WorkerNetClient,
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    }
}

#[test]
fn argv_starts_with_container_run() {
    let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/echo", &["hi"]);
    assert_eq!(argv[0], "container");
    assert_eq!(argv[1], "run");
}

/// Always-on flags must appear regardless of policy: `--rm` (auto-remove),
/// `-i` (stdin open for JSON-RPC), `--init` (signal-forwarding + zombie-reap),
/// `--progress none` (suppress noisy stderr progress lines).
#[test]
fn argv_always_carries_rm_and_interactive_and_init_and_progress_none() {
    let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(argv.contains(&"--rm".to_string()), "missing --rm; got: {argv:?}");
    assert!(argv.contains(&"-i".to_string()), "missing -i; got: {argv:?}");
    assert!(argv.contains(&"--init".to_string()), "missing --init; got: {argv:?}");
    // --progress none must appear as adjacent argv elements (not just both present somewhere).
    let progress_idx = argv
        .iter()
        .position(|s| s == "--progress")
        .expect("missing --progress");
    assert_eq!(
        argv[progress_idx + 1],
        "none",
        "--progress not followed by `none`; got: {argv:?}"
    );
}

/// `--init` must appear in every container run argv: it forwards
/// signals (so the lifecycle manager's outer-process kill reaches the
/// in-VM worker) and reaps zombies (Python's multiprocessing fork). The
/// flag is parallel to LinuxBwrap's unconditional `--as-pid-1` posture.
/// Pinned by issue #107.
#[test]
fn argv_carries_init_for_signal_forwarding_and_zombie_reaping() {
    let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(
        argv.contains(&"--init".to_string()),
        "missing --init; got: {argv:?}"
    );
}

/// `Net::Deny` must emit `--network none`. Explicit on both arms so a
/// future change to container's default doesn't silently re-open the
/// network on Deny policies.
#[test]
fn net_deny_emits_network_none() {
    let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    let net_idx = argv
        .iter()
        .position(|s| s == "--network")
        .expect("missing --network");
    assert_eq!(argv[net_idx + 1], "none");
}

#[test]
fn net_allowlist_emits_network_default() {
    let argv = build_container_argv(&netclient_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    let net_idx = argv
        .iter()
        .position(|s| s == "--network")
        .expect("missing --network");
    assert_eq!(argv[net_idx + 1], "default");
}

/// WorkerStrict adds `--read-only` (root FS RO), `--cap-drop ALL`,
/// `--user nobody`, and `--tmpfs /tmp` (so processes have a writable
/// scratch despite --read-only).
#[test]
fn strict_profile_adds_readonly_capdrop_user_nobody_and_tmpfs() {
    let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(argv.contains(&"--read-only".to_string()), "got: {argv:?}");
    let cap_idx = argv
        .iter()
        .position(|s| s == "--cap-drop")
        .expect("missing --cap-drop");
    assert_eq!(argv[cap_idx + 1], "ALL");
    let user_idx = argv
        .iter()
        .position(|s| s == "--user")
        .expect("missing --user");
    assert_eq!(argv[user_idx + 1], "nobody");
    let tmpfs_idx = argv
        .iter()
        .position(|s| s == "--tmpfs")
        .expect("missing --tmpfs");
    assert_eq!(argv[tmpfs_idx + 1], "/tmp");
}

/// WorkerNetClient is like Strict but WITHOUT `--read-only` (workers in
/// this profile may need to write outside /tmp). cap-drop, user nobody,
/// and /tmp tmpfs still apply.
#[test]
fn netclient_profile_drops_readonly_but_keeps_capdrop_user_and_tmpfs() {
    let argv = build_container_argv(&netclient_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(
        !argv.contains(&"--read-only".to_string()),
        "NetClient must not be --read-only; got: {argv:?}"
    );
    assert!(argv.contains(&"--cap-drop".to_string()));
    let user_idx = argv
        .iter()
        .position(|s| s == "--user")
        .expect("missing --user");
    assert_eq!(argv[user_idx + 1], "nobody");
    assert!(argv.contains(&"--tmpfs".to_string()));
}

/// `mem_mb == 0` means "unset"; the `-m` flag is dropped entirely (let
/// container's host default win). Pinned so a future regression doesn't
/// silently emit `-m 0M` (which container rejects).
#[test]
fn mem_mb_zero_drops_m_flag_entirely() {
    let mut p = strict_policy();
    p.mem_mb = 0;
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(
        !argv.iter().any(|s| s == "-m"),
        "mem_mb=0 must drop -m; got: {argv:?}"
    );
}

/// Non-zero `mem_mb` below the floor emits `-m 200M` (clamped). The
/// `tracing::warn!` is emitted by the build function; we can't observe
/// it from the test (no `tracing-test` dep) but the argv is the
/// load-bearing assertion.
#[test]
fn mem_mb_below_floor_emits_clamped_two_hundred_megabyte() {
    let mut p = strict_policy();
    p.mem_mb = 64;
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    let m_idx = argv.iter().position(|s| s == "-m").expect("missing -m");
    assert_eq!(argv[m_idx + 1], "200M");
}

#[test]
fn mem_mb_above_floor_passes_through() {
    let mut p = strict_policy();
    p.mem_mb = 1024;
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    let m_idx = argv.iter().position(|s| s == "-m").expect("missing -m");
    assert_eq!(argv[m_idx + 1], "1024M");
}

/// `cpu_quota_pct: None` does not emit `-c` (let container's
/// `--default-cpus` win). Pinned to prevent a future default-200% drift
/// that would diverge from `linux_cgroup`'s posture without an
/// explicit decision.
#[test]
fn cpu_quota_pct_none_drops_c_flag_entirely() {
    let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(
        !argv.iter().any(|s| s == "-c"),
        "cpu_quota_pct=None must drop -c; got: {argv:?}"
    );
}

#[test]
fn cpu_quota_pct_two_hundred_emits_two_fractional_vcpus() {
    let mut p = strict_policy();
    p.cpu_quota_pct = Some(200);
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    let c_idx = argv.iter().position(|s| s == "-c").expect("missing -c");
    assert_eq!(argv[c_idx + 1], "2");
}

#[test]
fn cpu_quota_pct_fractional_emits_decimal_vcpus() {
    let mut p = strict_policy();
    p.cpu_quota_pct = Some(150);
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    let c_idx = argv.iter().position(|s| s == "-c").expect("missing -c");
    assert_eq!(argv[c_idx + 1], "1.5");
}

/// `Some(0)` is treated as "unset" — drops `-c` entirely rather than
/// emitting `-c 0` (which `container` rejects with an opaque error).
/// Mirrors the `mem_mb == 0` drop-the-flag posture.
#[test]
fn cpu_quota_pct_zero_drops_c_flag_entirely() {
    let mut p = strict_policy();
    p.cpu_quota_pct = Some(0);
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(
        !argv.iter().any(|s| s == "-c"),
        "cpu_quota_pct=Some(0) must drop -c; got: {argv:?}"
    );
}

#[test]
fn tasks_max_none_drops_ulimit_flag_entirely() {
    let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(
        !argv.iter().any(|s| s == "--ulimit"),
        "tasks_max=None must drop --ulimit; got: {argv:?}"
    );
}

#[test]
fn tasks_max_emits_ulimit_nproc_with_soft_eq_hard() {
    let mut p = strict_policy();
    p.tasks_max = Some(64);
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    let u_idx = argv
        .iter()
        .position(|s| s == "--ulimit")
        .expect("missing --ulimit");
    assert_eq!(argv[u_idx + 1], "nproc=64:64");
}

#[test]
fn fs_read_emits_readonly_bind_mount_per_path() {
    let mut p = strict_policy();
    p.fs_read = vec![PathBuf::from("/etc/ssl"), PathBuf::from("/opt/data")];
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    let joined = argv.join(" ");
    assert!(
        joined.contains("--mount type=bind,source=/etc/ssl,target=/etc/ssl,readonly"),
        "got: {argv:?}"
    );
    assert!(
        joined.contains("--mount type=bind,source=/opt/data,target=/opt/data,readonly"),
        "got: {argv:?}"
    );
}

#[test]
fn fs_write_emits_writable_bind_mount_per_path() {
    let mut p = strict_policy();
    p.fs_write = vec![PathBuf::from("/var/lib/hhagent/scratch")];
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    let joined = argv.join(" ");
    assert!(
        joined.contains("--mount type=bind,source=/var/lib/hhagent/scratch,target=/var/lib/hhagent/scratch"),
        "got: {argv:?}"
    );
    // The fs_write path must NOT emit a separate `,readonly` mount.
    assert!(
        !joined.contains("type=bind,source=/var/lib/hhagent/scratch,target=/var/lib/hhagent/scratch,readonly"),
        "fs_write path was emitted as readonly; got: {argv:?}"
    );
}

#[test]
fn env_entries_emit_dash_e_kv() {
    let mut p = strict_policy();
    p.env = vec![
        ("FOO".into(), "bar".into()),
        ("HHAGENT_CPU_MS".into(), "5000".into()),
    ];
    let argv = build_container_argv(&p, DEFAULT_IMAGE, "/bin/true", &[]);
    // -e flags appear as adjacent pairs; locate by value.
    for needle in &["FOO=bar", "HHAGENT_CPU_MS=5000"] {
        let i = argv
            .iter()
            .position(|s| s == needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in {argv:?}"));
        assert_eq!(argv[i - 1], "-e");
    }
}

/// Image must appear in the argv exactly once, before `program` and any
/// `args`. Pinned to prevent a refactor from forgetting the image
/// (silently making `container run` use its default).
#[test]
fn image_appears_before_program_and_args() {
    let argv = build_container_argv(
        &strict_policy(),
        "alpine:3.20",
        "/bin/echo",
        &["hello", "world"],
    );
    let img_idx = argv
        .iter()
        .position(|s| s == "alpine:3.20")
        .expect("missing image");
    let prog_idx = argv
        .iter()
        .position(|s| s == "/bin/echo")
        .expect("missing program");
    let arg_idx = argv
        .iter()
        .position(|s| s == "hello")
        .expect("missing first arg");
    assert!(
        img_idx < prog_idx && prog_idx < arg_idx,
        "expected image < program < args order; got: {argv:?}"
    );
    assert_eq!(argv[prog_idx + 1], "hello");
    assert_eq!(argv[prog_idx + 2], "world");
    // Image must appear exactly once.
    let img_count = argv.iter().filter(|s| s.as_str() == "alpine:3.20").count();
    assert_eq!(img_count, 1, "image emitted more than once; got: {argv:?}");
}

#[test]
fn relative_policy_paths_are_rejected_by_spawn() {
    let backend = MacosContainer::new();
    let mut p = strict_policy();
    p.fs_read.push(PathBuf::from("relative/path"));
    let err = backend
        .spawn_under_policy(&p, "/bin/true", &[])
        .expect_err("must reject relative paths");
    let msg = format!("{err}");
    assert!(
        msg.contains("must be absolute"),
        "expected 'must be absolute' error, got: {msg}"
    );
}

/// `MacosContainer::with_image` overrides the default. Pinned so a
/// refactor that drops the constructor would trip immediately rather
/// than silently using the hard-coded `alpine:3.20`.
#[test]
fn with_image_overrides_default() {
    let backend = MacosContainer::with_image("ghcr.io/example/worker:dev");
    assert_eq!(backend.image(), "ghcr.io/example/worker:dev");
}

#[test]
fn default_constructor_uses_default_image() {
    let backend = MacosContainer::new();
    assert_eq!(backend.image(), DEFAULT_IMAGE);
}

// ---------- build_image_inspect_argv (issue #120) ----------

/// Pins the exact argv shape: `["container", "image", "inspect", <tag>]`.
/// Any change here is operator-visible (it changes the subprocess we
/// spawn), so a deliberate test update is the right friction.
#[test]
fn build_image_inspect_argv_shape() {
    let argv = build_image_inspect_argv("hhagent/gliner-relex:dev");
    assert_eq!(
        argv,
        vec!["container", "image", "inspect", "hhagent/gliner-relex:dev"]
    );
}

/// Tag is passed verbatim — no quoting, no escaping, no munging. The
/// shell-injection footgun is structurally impossible because we use
/// `Command::args(...)` (not a shell), but pinning the verbatim pass
/// keeps the contract obvious.
#[test]
fn build_image_inspect_argv_passes_tag_verbatim() {
    for tag in [
        "alpine:3.20",
        "ghcr.io/foo/bar:v1.2.3",
        "tag-with-dashes",
        "tag_with_underscores",
        "registry.example.com:5000/myimg:dev",
    ] {
        let argv = build_image_inspect_argv(tag);
        assert_eq!(argv[3], tag, "tag mangled for input {tag:?}");
        assert_eq!(argv.len(), 4, "argv length drifted for input {tag:?}");
    }
}

/// Empty tag still produces a 4-element argv (the subprocess will fail
/// loudly at the binary level, which is the right place for that
/// diagnostic — not here).
#[test]
fn build_image_inspect_argv_empty_tag_is_passthrough() {
    let argv = build_image_inspect_argv("");
    assert_eq!(argv, vec!["container", "image", "inspect", ""]);
}

// ---------- probe_image guards (post-review fixup) ----------

/// `probe_image("")` short-circuits before spawning with an operator-
/// actionable diagnostic. The pure argv builder still passes empty
/// strings through (`build_image_inspect_argv_empty_tag_is_passthrough`);
/// the spawn-path is where the guard belongs because that's where
/// the failure mode is operator-visible.
#[test]
fn probe_image_rejects_empty_tag_upfront() {
    let err =
        MacosContainer::probe_image("").expect_err("empty tag must error before spawn");
    let msg = format!("{err}");
    assert!(
        msg.contains("empty image_tag"),
        "expected empty-tag diagnostic, got: {msg}"
    );
    // Hint at the correct fix — caller should use None rather than
    // Some(""). The exact wording is allowed to drift; we just
    // pin the operator-actionable cue.
    assert!(
        msg.contains("None"),
        "expected None-fallback hint in diagnostic, got: {msg}"
    );
}
