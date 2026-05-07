# Phase 0b — macOS Seatbelt Sandbox Backend (Design)

**Status:** approved 2026-05-07
**Branch target:** `main`
**Companion docs:** [`docs/threat-model.md`](../../threat-model.md), [`docs/architecture.md`](../../architecture.md), [`docs/devel/ROADMAP.md`](../../devel/ROADMAP.md), [`docs/devel/handovers/HANDOVER.md`](../../devel/handovers/HANDOVER.md)

## Goal

Implement the macOS counterpart of the Linux `bwrap` sandbox backend so the same `SandboxPolicy` runs tool workers under `sandbox-exec` (Seatbelt) on macOS with containment that matches the Linux side as closely as the platforms allow. End state: `cargo test --workspace` is green on both Linux and macOS, with the same `SandboxBackend` contract observable from both.

## Non-goals (this session)

- `setrlimit`-based CPU/memory/wallclock enforcement. Linux today does not enforce these either; both platforms will pick this up via the future supervisor work.
- A `die-with-parent` equivalent on macOS. Tracked as an open item; mitigated for now by `Command::process_group(0)` and the supervisor lifecycle.
- The future micro-VM backend (Apple `container` CLI on Tahoe+). Separate backend, separate phase.
- Updates to `architecture.md` (already covers `sandbox-exec`), `ROADMAP.md`, or `HANDOVER.md` — those are session-end housekeeping per the handover convention.

## Architectural alignment

- Same `SandboxPolicy` and `SandboxBackend` trait drive both platforms — no new fields, no new variants.
- One process per worker, one Seatbelt jail per worker — invariant 1 in `architecture.md`.
- The host network allowlist is enforced by the future egress proxy, not by Seatbelt; `Net::Allowlist` lifts the network-deny in the profile, the proxy enforces the per-host list. Same split as bwrap's `--share-net`.
- Worker-side defence-in-depth: `hhagent-worker-prelude::lock_down()` already returns `LockdownReport::SkippedNonLinux` on macOS. We do not add a worker-side macOS layer in this session — Seatbelt is the parent-side filter, and the threat model already documents that macOS is the weaker of the two platforms.

## File layout

```
sandbox/src/macos_seatbelt.rs        NEW   sibling of linux_bwrap.rs; one file
sandbox/src/lib.rs                   EDIT  cfg-gated mod, default_backend() arm
sandbox/Cargo.toml                   EDIT  register fixtures/net_probe as a dev bin
sandbox/tests/macos_smoke.rs         NEW   #![cfg(target_os = "macos")]
sandbox/tests/fixtures/net_probe.rs  NEW   tiny TcpStream::connect probe binary
core/tests/shell_exec_e2e.rs         EDIT  drop cfg(linux); per-OS probe helper
docs/threat-model.md                 EDIT  SPI paragraph + macOS smoke row
```

No new third-party dependencies. Inline `-p` profile delivery; no `tempfile` needed. Profile sizes are well under macOS `ARG_MAX` (~1 MiB).

## SandboxPolicy → `.sb` profile mapping

`build_profile(policy: &SandboxPolicy) -> String` is a pure function (no I/O, no system calls), unit-testable in isolation. The emitted profile uses the TinyScheme dialect that `sandbox-exec` accepts:

```scheme
(version 1)
(deny default)

;; --- always-on: dyld + libsystem need these to start any process ---
(allow process-fork)
(allow process-exec*)
(allow file-read* (subpath "/usr/lib"))
(allow file-read* (subpath "/usr/libexec"))
(allow file-read* (subpath "/System/Library"))
(allow file-read-metadata (subpath "/"))
(allow sysctl-read)

;; --- /dev: explicit minimal allowlist; everything else stays denied ---
(allow file-read* file-write* (literal "/dev/null"))
(allow file-read* file-write* (literal "/dev/zero"))
(allow file-read*              (literal "/dev/random"))
(allow file-read*              (literal "/dev/urandom"))
(allow file-read* file-write* (literal "/dev/tty"))
(allow file-read* file-write* (subpath "/dev/fd"))
(allow file-read* file-write* (literal "/dev/dtracehelper"))

;; --- per-policy fs_read paths (read-only) ---
(allow file-read* (subpath "<each policy.fs_read entry>"))
;; ... one line per entry

;; --- per-policy fs_write paths (read + write) ---
(allow file-read* file-write* (subpath "<each policy.fs_write entry>"))

;; --- network: only when Net::Allowlist; egress proxy enforces hosts ---
(allow network*)
```

### Mapping rules

| Policy field | `.sb` emission |
|---|---|
| `policy.net = Net::Deny` | (no network rule — `(deny default)` covers it) |
| `policy.net = Net::Allowlist(_)` | `(allow network*)` |
| `policy.fs_read[i]` | `(allow file-read* (subpath "<path>"))` |
| `policy.fs_write[i]` | `(allow file-read* file-write* (subpath "<path>"))` (single combined rule, not separate read+write) |
| `policy.env` | NOT in profile; cleared and re-applied via `Command::env_clear().envs(...)` |
| `policy.cpu_ms`, `policy.mem_mb` | not enforced this session (deferred to supervisor) |
| `policy.profile` | not used by macOS today (would distinguish allow/deny lists if seccomp-equivalent existed) |

### `/dev` posture

We do **not** emit `(allow ... (subpath "/dev"))`. Workers see only `null`, `zero`, `random`, `urandom`, `tty`, `fd/`, and `dtracehelper`. `/dev/disk*`, `/dev/auditpipe`, `/dev/audit`, `/dev/console`, `/dev/diskimages-helper`, `/dev/bpf*`, `/dev/pf`, `/dev/klog` and friends remain denied by default. Containment outcome equivalent to bwrap's `--dev /dev`; mechanism differs (MAC vs. mount).

### Acknowledged FS asymmetry vs. Linux

`(allow file-read-metadata (subpath "/"))` is a deliberate concession: `stat()` on path components leaks the existence of paths like `/Users/hherb`. Without it, dyld cannot resolve `/usr/lib/dyld`'s parent dirs and even `/usr/bin/true` fails to launch. The Linux equivalent (`--ro-bind /usr` only) actually hides the rest of the FS — structurally stronger. This is the "weaker of the two platforms" asymmetry that `threat-model.md` already records.

## Process glue (the `bwrap` analogues)

`spawn_under_policy` follows the same shape and same up-front validation as `linux_bwrap::spawn_under_policy`:

```rust
fn spawn_under_policy(
    &self,
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
) -> Result<Child, SandboxError> {
    for p in policy.fs_read.iter().chain(policy.fs_write.iter()) {
        if !p.is_absolute() {
            return Err(SandboxError::Backend(format!(
                "policy paths must be absolute, got {p:?}"
            )));
        }
    }
    let profile = build_profile(policy);
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p").arg(&profile);
    cmd.arg(program);
    cmd.args(args);
    cmd.env_clear();
    for (k, v) in &policy.env { cmd.env(k, v); }
    cmd.process_group(0);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.spawn()
        .map_err(|e| SandboxError::Backend(format!("sandbox-exec spawn failed: {e}")))
}
```

### bwrap-flag → macOS-equivalent table

| `bwrap` flag | macOS equivalent | Notes |
|---|---|---|
| `--unshare-all` | `(deny default)` in profile | macOS has no namespaces; Seatbelt MAC is the substitute |
| `--die-with-parent` | (gap, deferred) | Future supervisor work; for now `Command::process_group(0)` keeps it from leaking outside the test process |
| `--new-session` | `Command::process_group(0)` | Calls `setsid` via `posix_spawn` on macOS |
| `--as-pid-1` | (none) | Linux-only namespace-PID feature; no analogue |
| `--clearenv` + `--setenv K V` | `cmd.env_clear().envs(&policy.env)` | Done before `spawn()` |
| `--ro-bind /usr /usr` | `(allow file-read* (subpath "/usr/lib"))` etc. | Profile rules cover this layer |
| `--proc /proc` | (none) | macOS has no procfs |
| `--dev /dev` | explicit `/dev` allowlist in profile | Section above |
| `--tmpfs /tmp` | (none) | No mount layer; if a worker needs scratch, list it in `policy.fs_write` |

## Probe + skip pattern

Mirror Linux's `LinuxBwrap::probe()` so a host where Seatbelt is broken (corrupt install, SIP-related scope clipping, profile-syntax regression in a future macOS release) reports `[SKIP]` rather than a false-green test pass.

```rust
impl MacosSeatbelt {
    pub fn probe() -> Result<(), SandboxError> {
        let profile = "(version 1)\n(deny default)\n\
                       (allow process-fork)\n(allow process-exec*)\n\
                       (allow file-read* (subpath \"/usr/lib\"))\n\
                       (allow file-read* (subpath \"/System/Library\"))\n\
                       (allow file-read-metadata (subpath \"/\"))\n\
                       (allow sysctl-read)\n";
        let output = Command::new("sandbox-exec")
            .args(["-p", profile, "/usr/bin/true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SandboxError::Backend(format!("could not spawn sandbox-exec: {e}")))?;
        if output.status.success() { return Ok(()); }
        Err(SandboxError::Backend(format!(
            "sandbox-exec probe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
```

The probe profile is itself a minimal allowlist (not a no-op): without `process-fork`, `process-exec*`, dyld + System reads, metadata, `sysctl-read`, even `/usr/bin/true` fails to launch and the probe spuriously reports "broken Seatbelt" on a healthy host — the same trap the previous handover's bwrap probe fell into.

`macos_smoke.rs::skip_if_no_seatbelt()` calls `MacosSeatbelt::probe()` and prints `eprintln!("[SKIP] sandbox-exec probe failed: {e}")` on failure, exactly as `linux_smoke.rs::skip_if_no_userns()` does — so `cargo test --workspace -- --nocapture` is grep-able for false-greens identically on both platforms.

## Test plan

### Unit tests in `sandbox/src/macos_seatbelt.rs::tests`

Pure-function tests on `build_profile`. Run on every host where the `macos_seatbelt` module compiles (i.e., macOS only; the module is `cfg(target_os = "macos")`).

| Test | Asserts |
|---|---|
| `profile_starts_with_version_and_deny_default` | First non-blank lines contain `(version 1)` and `(deny default)` |
| `deny_does_not_allow_network` | With `Net::Deny`, profile does NOT contain `(allow network*)` |
| `allowlist_does_allow_network` | With `Net::Allowlist(...)`, profile DOES contain `(allow network*)` |
| `fs_read_emits_subpath_allow` | `policy.fs_read = [/etc/ssl]` → profile contains `(allow file-read* (subpath "/etc/ssl"))` |
| `fs_write_emits_read_and_write_subpath_allow` | `policy.fs_write = [...]` → profile contains `(allow file-read* file-write* (subpath ...))` and NOT a separate read-only line for the same path |
| `dev_allowlist_is_minimal` | Profile contains `(literal "/dev/null")` and `(literal "/dev/urandom")` but NOT `(subpath "/dev")` |

### Integration tests in `sandbox/tests/macos_smoke.rs` (`#![cfg(target_os = "macos")]`)

| Test | Asserts |
|---|---|
| `echo_runs_inside_sandbox` | `/bin/echo hello-from-jail` succeeds; stdout matches |
| `host_etc_master_passwd_is_invisible_when_not_in_policy` | `/bin/cat /etc/master.passwd` fails (the macOS shadow file; `/etc/passwd` itself is world-readable on macOS by design) |
| `host_users_dir_is_invisible_when_not_in_policy` | `/bin/ls /Users` does not list `hherb` |
| `fs_read_path_is_visible_when_listed` | `policy.fs_read = ["/etc/hosts"]` → `/bin/cat /etc/hosts` succeeds |
| `net_is_unreachable_under_deny` | `target/debug/net_probe` exits non-zero under `Net::Deny` |
| `relative_policy_paths_are_rejected` | `policy.fs_read = ["relative/path"]` → `Err(Backend(_))` before spawn |
| `reading_dev_disk0_is_denied` | `/bin/cat /dev/disk0` fails (raw disk node not in `/dev` allowlist) |

### Net-probe fixture binary

`sandbox/tests/fixtures/net_probe.rs`:

```rust
use std::net::TcpStream;
use std::time::Duration;
fn main() {
    match TcpStream::connect_timeout(
        &"1.1.1.1:443".parse().unwrap(),
        Duration::from_secs(2),
    ) {
        Ok(_)  => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}
```

Registered in `sandbox/Cargo.toml`:

```toml
[[bin]]
name = "net_probe"
path = "tests/fixtures/net_probe.rs"
test = false
doc  = false
```

Resolved at test time via `CARGO_MANIFEST_DIR + ../target/debug/net_probe`, the same trick `core/tests/shell_exec_e2e.rs::worker_binary` uses today.

The fixture is pure Rust + `std`, so it works on Linux too; we'll reuse it when the egress-proxy work needs cross-platform network-deny verification.

### Cross-platform `core/tests/shell_exec_e2e.rs`

Drop `#![cfg(target_os = "linux")]`. Add a per-OS probe helper:

```rust
#[cfg(target_os = "linux")]
fn skip_if_sandbox_unavailable() -> bool { /* LinuxBwrap::probe + [SKIP] */ }
#[cfg(target_os = "macos")]
fn skip_if_sandbox_unavailable() -> bool { /* MacosSeatbelt::probe + [SKIP] */ }

#[cfg(target_os = "linux")]
fn backend() -> Box<dyn SandboxBackend> { Box::new(LinuxBwrap::new()) }
#[cfg(target_os = "macos")]
fn backend() -> Box<dyn SandboxBackend> { Box::new(MacosSeatbelt::new()) }
```

The three existing tests (`echo_round_trip_through_sandboxed_worker`, `argv_outside_allowlist_is_rejected_by_worker_policy`, `unknown_method_yields_method_not_found`) keep their assertions; only the helpers vary by OS.

The policy already adds the worker binary to `policy.fs_read`, which on macOS becomes a `(literal "<worker-path>")` `file-read*` allow — `build_profile` emits this from the same input the Linux side does.

### Verification target

On macOS:
- protocol unit (3) + sandbox unit (6 macOS — Linux unit tests are cfg-gated out) + core unit (4) + macos_smoke (7) + shell_exec_e2e (3) = **23 tests**, 0 skips.

On Linux: 36 tests as today (the macOS unit tests don't compile on Linux).

`cargo test --workspace -- --nocapture` must show zero `[SKIP]` lines on a healthy host of either OS.

## `default_backend()` wiring

`sandbox/src/lib.rs`:

```rust
#[cfg(target_os = "linux")]
pub mod linux_bwrap;
#[cfg(target_os = "macos")]
pub mod macos_seatbelt;

pub fn default_backend() -> Box<dyn SandboxBackend> {
    #[cfg(target_os = "linux")] { Box::new(linux_bwrap::LinuxBwrap::new()) }
    #[cfg(target_os = "macos")] { Box::new(macos_seatbelt::MacosSeatbelt::new()) }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { Box::new(NotYetImplemented) }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct NotYetImplemented;
// impl unchanged; error message updated to mention "Linux or macOS"
```

The `NotYetImplemented` fallback survives so a build on FreeBSD/Windows still compiles and gives a clean runtime error.

## `docs/threat-model.md` updates

**Append to "Asymmetric platform note":**

> The macOS implementation shells out to `/usr/bin/sandbox-exec`, which Apple has marked as private API and emits a deprecation warning for, while continuing to ship and maintain it (it remains the foundation of the system's own sandboxing of daemons under `/usr/share/sandbox/`). We accept this risk explicitly: should Apple ever remove `sandbox-exec`, the migration path is the entitlement-based App Sandbox combined with Endpoint Security framework filters, both of which require code-signing and entitlements that we do not have today. Until that day, `sandbox-exec` is the best containment available without entitlements.

**Append to "Already shipped" under "Negative tests":**

> - `sandbox/tests/macos_smoke.rs` — Seatbelt denies `/etc/master.passwd`, `/Users/...`, raw `/dev/disk0`, and network under `Net::Deny`.

## Open items (not blocking this session)

- A `die-with-parent` equivalent on macOS. Will be picked up by the `launchd` LaunchAgent supervisor work; until then, `Command::process_group(0)` plus the test-process lifecycle is sufficient for verification.
- `setrlimit`-based CPU/memory/wallclock enforcement — same on both platforms; supervisor work item.
- Empirical verification of the "always-on" allow set on macOS Tahoe. The list in this design is the typical minimum; if a worker fails to launch, iterate from the `sandbox-exec` stderr error rather than guessing.
- The future stronger backend on macOS (Apple `container` CLI on Tahoe+) — separate backend, separate phase.
