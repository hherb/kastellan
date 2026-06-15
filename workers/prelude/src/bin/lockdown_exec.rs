//! `kastellan-worker-lockdown-exec`: production exec-shim that applies the
//! worker prelude lockdown (rlimit + Landlock + seccomp, all read from env)
//! and then `execve`s into a target binary. The target inherits the seccomp
//! filter (and Landlock ruleset, when enabled) because seccomp filters survive
//! `execve` under `PR_SET_NO_NEW_PRIVS`, which `lock_down` sets.
//!
//! Why it exists: pure-Python venv workers (browser-driver, gliner-relex) are
//! console scripts that `linux_bwrap` spawns directly — they never run the Rust
//! prelude, so without this shim they get no worker-side seccomp on Linux
//! (issue #281). Wrapping their spawn in this shim closes that gap.
//!
//! Reads the exact env `core::tool_host::derive_lockdown_env` already injects
//! for every worker (`KASTELLAN_SECCOMP_PROFILE`, `KASTELLAN_CPU_MS`,
//! `KASTELLAN_LANDLOCK_RW` / `_RO` / `_PROFILE`). No new host-side plumbing.
//!
//! Platform: this shim is **only meaningful on Linux**, where `lock_down`
//! installs a real seccomp filter (and optionally Landlock). On every other
//! platform `lock_down` is a no-op, so applying it and then `execve`ing the
//! target would run the target *unconfined* — exactly the gap this shim exists
//! to close. The binary still compiles everywhere (so the workspace builds on
//! macOS), but it **refuses to exec off Linux** rather than silently running an
//! uncontained child. macOS workers are contained by Seatbelt from the parent
//! and the manifest never routes them through this shim.
//!
//! Exit codes (a successful `execve` never returns):
//!   64 usage error (no target)   70 lock_down failed
//!   71 execve failed             72 rlimit failed
//!   73 refused: not Linux (would run unconfined)

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let target: OsString = match args.next() {
        Some(t) => t,
        None => {
            eprintln!("usage: kastellan-worker-lockdown-exec <target-binary> [args...]");
            return ExitCode::from(64);
        }
    };
    let rest: Vec<OsString> = args.collect();

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;

        // rlimit first (matches serve_stdio: arm the CPU ceiling before any
        // seccomp restriction on the prlimit family). No-op when KASTELLAN_CPU_MS
        // is unset.
        if let Err(e) = kastellan_worker_prelude::rlimit::apply_from_env() {
            eprintln!("kastellan-worker-lockdown-exec: rlimit error: {e}");
            return ExitCode::from(72);
        }
        // Landlock (env-gated; KASTELLAN_LANDLOCK_PROFILE=none skips it) + seccomp.
        match kastellan_worker_prelude::lock_down() {
            Ok(report) => eprintln!("kastellan-worker-lockdown-exec: lockdown {report:?}"),
            Err(e) => {
                eprintln!("kastellan-worker-lockdown-exec: lockdown error: {e}");
                return ExitCode::from(70);
            }
        }
        // `exec` replaces this process image; the seccomp filter persists.
        let err = std::process::Command::new(&target).args(&rest).exec();
        eprintln!("kastellan-worker-lockdown-exec: exec({target:?}) failed: {err}");
        ExitCode::from(71)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Fail closed: off Linux, lock_down() does not install a seccomp filter,
        // so exec'ing the target would run it UNCONFINED. Refuse rather than
        // provide a false sense of containment. The manifest never inserts this
        // shim off Linux, so this path is unreachable in normal operation — it
        // exists purely as a defense-in-depth backstop.
        let _ = (&target, &rest);
        eprintln!(
            "kastellan-worker-lockdown-exec: refusing to exec on non-Linux \
             (lock_down is a no-op here; the target would run unconfined)"
        );
        ExitCode::from(73)
    }
}
