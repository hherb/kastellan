//! Landlock LSM enforcement, applied from inside the worker process.
//!
//! Landlock is a kernel feature (Linux 5.13+) that lets an unprivileged
//! process restrict its own filesystem access. Once
//! [`landlock::RulesetCreated::restrict_self`] returns, the filter is
//! permanent for this process and all its future children — there is no
//! syscall to relax it.
//!
//! ## What we restrict
//!
//! By default we install a Landlock ruleset that allows:
//!
//!   * **Read + execute** under `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`,
//!     `/etc/ld.so.cache`. This keeps the dynamic linker, libc, and any
//!     allow-listed exec target reachable. (`bwrap` already mounts these
//!     read-only; Landlock is a second, kernel-side check.)
//!   * **All FS rights** under any path listed in the
//!     `HHAGENT_LANDLOCK_RW` env var (JSON array of absolute paths).
//!     This is the worker's scratch dir.
//!
//! Everything else — including `/etc`, `/home`, `/root`, `/var` — is denied.
//!
//! ## Kernel compatibility
//!
//! We request Landlock ABI v1 (5.13+) which gives us the basic FS access
//! rights needed for read/write/exec gating. On older kernels,
//! `restrict_self` returns [`landlock::RulesetStatus::NotEnforced`]; we
//! report this via [`LandlockReport::KernelTooOld`] and continue (bwrap is
//! still in effect from the parent side).

use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetError, RulesetStatus,
};

use crate::{LandlockReport, LockdownError};

/// The Landlock ABI version we target. v1 = Linux 5.13+. v2 (5.19+) adds
/// `Refer` (rename across boundaries) which we do not need. Stick to v1
/// for maximum compatibility — we only need read/write/exec gating.
const TARGET_ABI: ABI = ABI::V1;

/// Coarse-grained read-and-execute roots that every worker needs in order
/// to load shared libraries and exec helper binaries that the parent has
/// allowlisted (via `bwrap` mount). Each path is best-effort: if it does
/// not exist on this host, it is silently skipped.
///
/// `/dev` is included because Rust's `Command::output()` opens `/dev/null`
/// for the child's stdin when the caller doesn't supply one — without
/// allowing /dev, every shell-exec call would die at posix_spawn time
/// with EACCES on `/dev/null`. bwrap has already restricted /dev to a
/// minimal tmpfs containing only `null`, `zero`, `random`, `urandom`,
/// `tty`, `console`, so granting Landlock access here is not a
/// meaningful capability uplift — the dangerous device nodes are simply
/// not present inside the jail.
const DEFAULT_RO_EXEC_ROOTS: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/bin",
    "/sbin",
    "/etc/ld.so.cache",
    "/dev",
    "/proc",
];

/// Read [`HHAGENT_LANDLOCK_RW`] from the environment and apply the ruleset.
/// Used by [`crate::lock_down`].
pub fn apply_from_env() -> Result<LandlockReport, LockdownError> {
    let rw_paths = parse_rw_env_var()?;
    apply(&rw_paths)
}

/// Pure parser for the `HHAGENT_LANDLOCK_RW` env var. Exposed for testing.
///
/// Accepted: missing, empty, or a JSON array of absolute path strings.
/// Returns an error on malformed JSON or relative paths.
pub fn parse_rw_env_var() -> Result<Vec<PathBuf>, LockdownError> {
    let raw = match std::env::var("HHAGENT_LANDLOCK_RW") {
        Ok(s) if !s.is_empty() => s,
        _ => return Ok(Vec::new()),
    };
    parse_rw_string(&raw)
}

/// Pure parser used by [`parse_rw_env_var`]. Split out so tests can drive
/// it without mucking with process env state.
pub fn parse_rw_string(raw: &str) -> Result<Vec<PathBuf>, LockdownError> {
    let parsed: Vec<String> = serde_json::from_str(raw).map_err(|e| {
        LockdownError::Env(format!(
            "HHAGENT_LANDLOCK_RW must be a JSON array of strings: {e}"
        ))
    })?;
    let mut out = Vec::with_capacity(parsed.len());
    for p in parsed {
        let pb = PathBuf::from(&p);
        if !pb.is_absolute() {
            return Err(LockdownError::Env(format!(
                "HHAGENT_LANDLOCK_RW path {p:?} must be absolute"
            )));
        }
        out.push(pb);
    }
    Ok(out)
}

/// Install the Landlock ruleset. `rw_paths` is the worker's writable
/// scratch list (typically just one entry).
pub fn apply(rw_paths: &[PathBuf]) -> Result<LandlockReport, LockdownError> {
    let access_all = AccessFs::from_all(TARGET_ABI);
    // RO roots get read **and** execute, otherwise the worker can read
    // /usr/bin/echo but cannot exec it — `from_read` is read-only and
    // omits `Execute`. Without this, every shell-exec call would die
    // with EACCES at `execve` time. The dynamic linker also needs
    // Execute when it mmap's libc.so.6 with PROT_EXEC (PROT_EXEC mmap
    // is a Landlock-`Execute` access).
    let access_read_exec = AccessFs::from_read(TARGET_ABI) | AccessFs::Execute;

    let ruleset = match Ruleset::default()
        .handle_access(access_all)
        .and_then(|r| r.create())
    {
        Ok(r) => r,
        Err(RulesetError::CreateRuleset(e)) => {
            // Kernel does not support Landlock at all (ENOSYS) or our requested
            // ABI. Treat as "kernel too old": bwrap stays in effect, but we
            // can't add the second layer.
            return Ok(report_for_create_failure(e));
        }
        Err(e) => return Err(LockdownError::Landlock(e.to_string())),
    };

    let mut ruleset = ruleset;
    for root in DEFAULT_RO_EXEC_ROOTS {
        ruleset = add_path_rule(ruleset, Path::new(root), access_read_exec)?;
    }
    for p in rw_paths {
        ruleset = add_path_rule(ruleset, p, access_all)?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| LockdownError::Landlock(format!("restrict_self failed: {e}")))?;

    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => LandlockReport::FullyEnforced,
        RulesetStatus::PartiallyEnforced => LandlockReport::PartiallyEnforced,
        RulesetStatus::NotEnforced => LandlockReport::KernelTooOld,
    })
}

/// Best-effort helper: if `path` exists, add it to the ruleset; otherwise
/// silently skip. We use `add_rule` rather than `add_rules` so each path is
/// tried independently — a missing `/lib64` on a multilib-only host should
/// not break the worker.
fn add_path_rule(
    ruleset: landlock::RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<landlock::RulesetCreated, LockdownError> {
    let fd = match PathFd::new(path) {
        Ok(fd) => fd,
        Err(_) => return Ok(ruleset), // ENOENT: skip silently
    };
    ruleset
        .add_rule(PathBeneath::new(fd, access))
        .map_err(|e| LockdownError::Landlock(format!("add_rule {path:?}: {e}")))
}

/// Convert a `CreateRuleset` error into a [`LandlockReport`]. ENOSYS means
/// the kernel lacks Landlock entirely; everything else we conservatively
/// also call "too old" so the worker still proceeds (bwrap is the primary
/// containment layer).
fn report_for_create_failure(_e: impl std::fmt::Display) -> LandlockReport {
    LandlockReport::KernelTooOld
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rw_string_empty_array_yields_empty_vec() {
        assert!(parse_rw_string("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_rw_string_accepts_absolute_paths() {
        let v = parse_rw_string(r#"["/tmp/scratch","/var/lib/hhagent/work"]"#).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], PathBuf::from("/tmp/scratch"));
    }

    #[test]
    fn parse_rw_string_rejects_relative_paths() {
        let err = parse_rw_string(r#"["scratch"]"#).unwrap_err();
        assert!(matches!(err, LockdownError::Env(_)));
    }

    #[test]
    fn parse_rw_string_rejects_bad_json() {
        let err = parse_rw_string("not-json").unwrap_err();
        assert!(matches!(err, LockdownError::Env(_)));
    }
}
