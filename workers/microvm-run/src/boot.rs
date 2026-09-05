//! Firecracker process invocation (pure argv) + spawn helper.

/// Build the firecracker argv. `fc_bin` is argv[0] — `"firecracker"` (resolved
/// via $PATH) on the bare path, or an absolute path on the confined path (the
/// bwrap jail has no $PATH, so the backend resolves + binds it and passes it here).
/// `--no-api` + `--config-file` boots a fully pre-described VM; `--log-path` sends
/// Firecracker's OWN logs to a file, keeping our stdout clean for JSON-RPC.
///
/// It does NOT capture the guest kernel console: that rides firecracker's
/// **stdout**, which the caller redirects to `console.log` in the per-spawn run
/// dir (see [`crate::console`]). Keep the two straight — an earlier version of
/// this comment claimed `--log-path` captured the guest console, and that one
/// wrong sentence cost a session: three distinct micro-VM defects all surfaced
/// as an identical contentless `Protocol(EarlyExit)` because every guest-side
/// diagnostic was being discarded and this comment said otherwise (#666).
pub fn firecracker_argv(fc_bin: &str, config_path: &str, log_path: &str) -> Vec<String> {
    vec![
        fc_bin.into(),
        "--no-api".into(),
        "--config-file".into(), config_path.into(),
        "--log-path".into(), log_path.into(),
        "--level".into(), "Warn".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firecracker_argv_uses_given_binary_path() {
        let a = firecracker_argv("/abs/firecracker", "/run/fc.json", "/run/fc.log");
        assert_eq!(a[0], "/abs/firecracker");
        assert!(a.windows(2).any(|w| w[0] == "--config-file" && w[1] == "/run/fc.json"));
        assert!(a.windows(2).any(|w| w[0] == "--log-path" && w[1] == "/run/fc.log"));
    }

    #[test]
    fn firecracker_argv_defaults_to_bare_name() {
        let a = firecracker_argv("firecracker", "/c", "/l");
        assert_eq!(a[0], "firecracker");
    }
}
