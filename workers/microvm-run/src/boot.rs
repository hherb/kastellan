//! Firecracker process invocation (pure argv) + spawn helper.

/// Build the firecracker argv. `--no-api` + `--config-file` boots a fully
/// pre-described VM; `--log-path` sends Firecracker's own logs (including the
/// guest kernel console it captures) to a file, keeping our stdout clean for
/// JSON-RPC.
pub fn firecracker_argv(config_path: &str, log_path: &str) -> Vec<String> {
    vec![
        "firecracker".into(),
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
    fn firecracker_argv_uses_no_api_and_config() {
        let argv = firecracker_argv("/run/fc.json", "/run/fc.log");
        assert_eq!(argv[0], "firecracker");
        assert!(argv.iter().any(|a| a == "--no-api"));
        assert!(argv.windows(2).any(|w| w[0] == "--config-file" && w[1] == "/run/fc.json"));
        // Kernel console must be redirected away from our stdout.
        assert!(argv.windows(2).any(|w| w[0] == "--log-path" && w[1] == "/run/fc.log"));
    }
}
