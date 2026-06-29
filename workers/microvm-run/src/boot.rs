//! Firecracker process invocation (pure argv) + spawn helper.

/// Build the firecracker argv. `fc_bin` is argv[0] — `"firecracker"` (resolved
/// via $PATH) on the bare path, or an absolute path on the confined path (the
/// bwrap jail has no $PATH, so the backend resolves + binds it and passes it here).
/// `--no-api` + `--config-file` boots a fully pre-described VM; `--log-path` sends
/// Firecracker's own logs (including the guest kernel console it captures) to a file,
/// keeping our stdout clean for JSON-RPC.
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
