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
//!     `KASTELLAN_LANDLOCK_RW` env var (JSON array of absolute paths).
//!     This is the worker's scratch dir.
//!
//! Everything else — including `/etc`, `/home`, `/root`, `/var` — is denied.
//!
//! ## Kernel compatibility
//!
//! We request Landlock ABI v6 (Linux 6.12+). The user's primary host is on
//! Linux 6.17, so this lifts the report from `PartiallyEnforced` to
//! `FullyEnforced`. On older kernels, the `landlock` crate's compatibility
//! layer transparently falls back to the highest ABI the kernel does
//! support — we still get whatever subset is enforceable. If the kernel is
//! too old to support Landlock at all (< 5.13), `restrict_self` returns
//! [`landlock::RulesetStatus::NotEnforced`]; we report this via
//! [`LandlockReport::KernelTooOld`] and continue (bwrap is still in
//! effect from the parent side).
//!
//! ## ABI v6 access rights audit
//!
//! Bumping from v1 → v6 introduces four new restricted accesses that we
//! must explicitly handle (otherwise they fall outside the ruleset and
//! the kernel never enforces them):
//!
//!   * **`Refer` (v2)** — rename/link across rule boundaries. We allow
//!     it *within* the worker's RW scratch dir (renames within scratch
//!     are needed by anything that does atomic-write-then-rename). We
//!     do not add it to RO+exec roots — they have no write rights so
//!     the question is moot.
//!   * **`Truncate` (v3)** — truncating a file via `O_TRUNC`,
//!     `truncate()`, or `ftruncate()`. Allowed on RW scratch (every
//!     write-then-overwrite path needs it). Denied on RO+exec roots.
//!   * **`IoctlDev` (v5)** — `ioctl()` on character/block devices.
//!     Allowed on `/dev` because libc and the dynamic linker probe
//!     terminal-ness on stdio with `TCGETS`-style ioctls. The risk
//!     surface is minimal because bwrap already restricts `/dev` to a
//!     tmpfs with only `null`/`zero`/`random`/`urandom`/`tty`/`console`
//!     — there are no dangerous device nodes left to ioctl.
//!   * **Scope: `AbstractUnixSocket` (v6)** — connecting to abstract
//!     UDS created outside the sandbox. We deny: no Phase 0 worker has
//!     a legitimate need for an external abstract UDS, and DBus or
//!     similar would be a Phase-3 concern with its own profile.
//!   * **Scope: `Signal` (v6)** — sending signals to processes outside
//!     the sandbox. Denied: a worker should never need to signal
//!     anything but its own children.

use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetError, RulesetStatus, Scope,
};

use crate::{LandlockReport, LockdownError};

/// The Landlock ABI version we target. v6 = Linux 6.12+ (host on 6.17).
/// The crate's compatibility layer downgrades on older kernels — we
/// still benefit from whatever subset is enforceable. See the module
/// doc-comment for the per-version access-rights audit.
const TARGET_ABI: ABI = ABI::V6;

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

/// Name of the env var that can disable the Landlock layer. Value `"none"`
/// skips the ruleset entirely; unset / any other value keeps the default
/// behavior. Used by workers whose filesystem surface is not yet validated
/// against a Landlock ruleset (e.g. browser-driver/Chromium), where bwrap's
/// mount namespace remains the filesystem-containment layer.
pub const LANDLOCK_PROFILE_ENV: &str = "KASTELLAN_LANDLOCK_PROFILE";

/// Pure predicate: should the Landlock layer be skipped for this profile value?
/// Only the exact string `"none"` disables it (mirrors the seccomp `"none"`
/// convention). Split out so it is unit-testable without touching process env.
///
/// Note: an empty string (`""`) does NOT disable Landlock — unlike the seccomp
/// profile parser, which also treats `""` as `None`. An empty value here is far
/// more likely a misconfigured env var than a deliberate opt-out, so we
/// fail-safe and keep the ruleset.
pub fn landlock_disabled_by_profile(profile: Option<&str>) -> bool {
    profile == Some("none")
}

/// Read [`LANDLOCK_PROFILE_ENV`], [`KASTELLAN_LANDLOCK_RW`], and
/// [`KASTELLAN_LANDLOCK_RO`] from the environment and apply the ruleset — or
/// return [`LandlockReport::Disabled`] when the profile is `"none"`. Used by
/// [`crate::lock_down`].
pub fn apply_from_env() -> Result<LandlockReport, LockdownError> {
    // Explicit opt-out: a worker that sets KASTELLAN_LANDLOCK_PROFILE=none gets
    // no Landlock ruleset. bwrap's mount namespace still contains it.
    let profile = std::env::var(LANDLOCK_PROFILE_ENV).ok();
    if landlock_disabled_by_profile(profile.as_deref()) {
        return Ok(LandlockReport::Disabled);
    }
    let rw_paths = parse_rw_env_var()?;
    let ro_paths = parse_ro_env_var()?;
    apply(&rw_paths, &ro_paths)
}

/// Pure parser for the `KASTELLAN_LANDLOCK_RW` env var. Exposed for testing.
///
/// Accepted: missing, empty, or a JSON array of absolute path strings.
/// Returns an error on malformed JSON or relative paths.
pub fn parse_rw_env_var() -> Result<Vec<PathBuf>, LockdownError> {
    let raw = match std::env::var("KASTELLAN_LANDLOCK_RW") {
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
            "KASTELLAN_LANDLOCK_RW must be a JSON array of strings: {e}"
        ))
    })?;
    let mut out = Vec::with_capacity(parsed.len());
    for p in parsed {
        let pb = PathBuf::from(&p);
        if !pb.is_absolute() {
            return Err(LockdownError::Env(format!(
                "KASTELLAN_LANDLOCK_RW path {p:?} must be absolute"
            )));
        }
        out.push(pb);
    }
    Ok(out)
}

/// Pure parser for the `KASTELLAN_LANDLOCK_RO` env var. Exposed for testing.
///
/// Accepted: missing, empty, or a JSON array of absolute path strings.
/// Returns an error on malformed JSON or relative paths.
pub fn parse_ro_env_var() -> Result<Vec<PathBuf>, LockdownError> {
    let raw = match std::env::var("KASTELLAN_LANDLOCK_RO") {
        Ok(s) if !s.is_empty() => s,
        _ => return Ok(Vec::new()),
    };
    parse_ro_string(&raw)
}

/// Pure parser used by [`parse_ro_env_var`]. Split out so tests can drive
/// it without mucking with process env state.
pub fn parse_ro_string(raw: &str) -> Result<Vec<PathBuf>, LockdownError> {
    let parsed: Vec<String> = serde_json::from_str(raw).map_err(|e| {
        LockdownError::Env(format!(
            "KASTELLAN_LANDLOCK_RO must be a JSON array of strings: {e}"
        ))
    })?;
    let mut out = Vec::with_capacity(parsed.len());
    for p in parsed {
        let pb = PathBuf::from(&p);
        if !pb.is_absolute() {
            return Err(LockdownError::Env(format!(
                "KASTELLAN_LANDLOCK_RO path {p:?} must be absolute"
            )));
        }
        out.push(pb);
    }
    Ok(out)
}

/// Install the Landlock ruleset.
///
/// `rw_paths` is the worker's writable scratch list (typically just one
/// entry, from `KASTELLAN_LANDLOCK_RW`).
///
/// `ro_paths` is the list of additional read-only paths derived from
/// `SandboxPolicy.fs_read` (from `KASTELLAN_LANDLOCK_RO`). These are
/// bind-mounted read-only by bwrap; Landlock must also grant read rights
/// so the worker can access them after `lock_down()`. For example,
/// `/etc/resolv.conf` for DNS in the web-fetch worker.
pub fn apply(rw_paths: &[PathBuf], ro_paths: &[PathBuf]) -> Result<LandlockReport, LockdownError> {
    // Full read+write+rename+truncate+ioctl access — granted only to the
    // worker's RW scratch dirs.
    let access_all = AccessFs::from_all(TARGET_ABI);

    // Read+exec only — granted to the dynamic-linker / shared-library
    // roots and to additional fs_read paths. `from_read(V6)` already
    // includes `Execute` (see landlock::AccessFs::from_read), so the
    // bitflag union is just explicit-readability; the kernel sees the
    // same set either way.
    let access_read_exec = AccessFs::from_read(TARGET_ABI);

    // Same as `access_read_exec` plus `IoctlDev` — granted only to
    // `/dev`. libc/glibc and the dynamic linker probe terminal-ness on
    // stdio with `TCGETS`-style ioctls; without `IoctlDev` allowed
    // somewhere, those return EACCES instead of ENOTTY and break.
    let access_read_exec_dev = access_read_exec | AccessFs::IoctlDev;

    // Scope rights are v6-only. Handling them flips the kernel into
    // "deny by default for this scope" mode for this process tree.
    // There are no per-path rules for scopes — once handled, the
    // restriction is global.
    let scope_all = Scope::from_all(TARGET_ABI);

    let ruleset = match Ruleset::default()
        .handle_access(access_all)
        .and_then(|r| r.scope(scope_all))
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
        // /dev gets IoctlDev too; everything else is plain read+exec.
        let access = if *root == "/dev" {
            access_read_exec_dev
        } else {
            access_read_exec
        };
        ruleset = add_path_rule(ruleset, Path::new(root), access)?;
    }
    for p in rw_paths {
        ruleset = add_path_rule(ruleset, p, access_all)?;
    }
    // Additional read-only paths from SandboxPolicy.fs_read (e.g.
    // /etc/resolv.conf for DNS). Uses the same best-effort/skip-if-missing
    // helper as DEFAULT_RO_EXEC_ROOTS — nonexistent paths are silently
    // skipped so a stale policy entry doesn't kill the worker.
    for p in ro_paths {
        ruleset = add_path_rule(ruleset, p, access_read_exec)?;
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
///
/// The kernel rejects `EINVAL` if a `PathBeneath` rule on a *file* lists
/// directory-only rights like `ReadDir` / `MakeDir` / `Refer`. The
/// `landlock` crate copes by stripping those silently, but it also flips
/// the ruleset's compat state to `Partial`, downgrading the eventual
/// status report from `FullyEnforced` to `PartiallyEnforced`. We avoid
/// that by stat-ing the path here and passing only the file-applicable
/// subset for files. The on-disk enforcement is identical either way.
fn add_path_rule(
    ruleset: landlock::RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
) -> Result<landlock::RulesetCreated, LockdownError> {
    let fd = match PathFd::new(path) {
        Ok(fd) => fd,
        Err(_) => return Ok(ruleset), // ENOENT: skip silently
    };
    let access = match path.metadata() {
        Ok(md) if md.is_file() => access & AccessFs::from_file(TARGET_ABI),
        _ => access,
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
        let v = parse_rw_string(r#"["/tmp/scratch","/var/lib/kastellan/work"]"#).unwrap();
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

    // ── parse_ro_string tests (mirror the RW suite) ──────────────────────

    #[test]
    fn parse_ro_string_empty_array_yields_empty_vec() {
        assert!(parse_ro_string("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_ro_string_accepts_absolute_paths() {
        let v =
            parse_ro_string(r#"["/etc/resolv.conf","/etc/ssl/certs"]"#).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], PathBuf::from("/etc/resolv.conf"));
        assert_eq!(v[1], PathBuf::from("/etc/ssl/certs"));
    }

    #[test]
    fn parse_ro_string_rejects_relative_paths() {
        let err = parse_ro_string(r#"["etc/resolv.conf"]"#).unwrap_err();
        assert!(matches!(err, LockdownError::Env(_)));
    }

    #[test]
    fn parse_ro_string_rejects_bad_json() {
        let err = parse_ro_string("not-json").unwrap_err();
        assert!(matches!(err, LockdownError::Env(_)));
    }

    // ── KASTELLAN_LANDLOCK_PROFILE disable signal ────────────────────────

    #[test]
    fn landlock_disabled_only_for_explicit_none() {
        assert!(landlock_disabled_by_profile(Some("none")));
        assert!(!landlock_disabled_by_profile(Some("")));
        assert!(!landlock_disabled_by_profile(Some("strict")));
        assert!(!landlock_disabled_by_profile(None));
    }
}
