//! Pure planning for `kastellan-cli install`: per-user layout, the
//! EnvironmentFile contents, the binary sets to copy, and the
//! `ServiceSpec`s. No I/O — every function is deterministic.

use std::path::{Path, PathBuf};

use kastellan_supervisor::specs::{core_service_spec, kastellan_target_spec, postgres_service_spec};
use kastellan_supervisor::{ServiceSpec, TargetSpec};

/// Resolved per-user install paths.
pub struct Layout {
    pub home: PathBuf,
    pub user: String,
    pub bin_dir: PathBuf,
    pub assets_dir: PathBuf,
    pub prompts_dir: PathBuf,
    pub l0_rules_file: PathBuf,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub env_file: PathBuf,
    pub log_dir: PathBuf,
}

/// Compute the per-user layout from `$HOME` + `$USER`. Pure.
pub fn resolve_layout(home: &Path, user: &str) -> Layout {
    let assets_dir = home.join(".local/share/kastellan");
    let config_dir = home.join(".config/kastellan");
    Layout {
        home: home.to_path_buf(),
        user: user.to_string(),
        bin_dir: home.join(".local/lib/kastellan"),
        prompts_dir: assets_dir.join("prompts"),
        l0_rules_file: assets_dir.join("seeds/memory/l0_meta_rules.toml"),
        data_dir: assets_dir.join("pg/data"),
        env_file: config_dir.join("kastellan.env"),
        log_dir: home.join(".local/state/kastellan"),
        assets_dir,
        config_dir,
    }
}

/// The default local LLM URL per OS (Linux: vLLM/SGLang :8000; macOS: Ollama :11434).
pub fn default_llm_url() -> &'static str {
    if cfg!(target_os = "macos") {
        "http://127.0.0.1:11434"
    } else {
        "http://127.0.0.1:8000"
    }
}

/// Render the `kastellan.env` EnvironmentFile contents.
pub fn render_env_file(model: &str, url: &str, embedding_model: Option<&str>, layout: &Layout) -> String {
    let mut s = String::new();
    s.push_str(&format!("KASTELLAN_LLM_LOCAL_URL={url}\n"));
    s.push_str(&format!("KASTELLAN_LLM_LOCAL_MODEL={model}\n"));
    s.push_str(&format!("KASTELLAN_LLM_EMBEDDING_URL={url}\n"));
    if let Some(em) = embedding_model {
        s.push_str(&format!("KASTELLAN_LLM_EMBEDDING_MODEL={em}\n"));
    }
    s.push_str(&format!("KASTELLAN_PROMPTS_DIR={}\n", layout.prompts_dir.display()));
    s.push_str(&format!("KASTELLAN_L0_RULES_FILE={}\n", layout.l0_rules_file.display()));
    s.push_str(&format!("KASTELLAN_DATA_DIR={}\n", layout.data_dir.display()));
    s
}

/// Binaries whose absence aborts the install (daemon + db-init + the
/// fail-closed egress proxy + the operator CLI).
pub fn required_binaries() -> &'static [&'static str] {
    &[
        "kastellan",
        "kastellan-cli",
        "kastellan-db-init",
        "kastellan-worker-egress-proxy",
    ]
}

/// On-demand workers: copied when present in the build dir, skipped (with
/// a log line) when not — their absence only disables that one tool.
pub fn optional_binaries() -> &'static [&'static str] {
    &[
        "kastellan-worker-shell-exec",
        "kastellan-worker-web-fetch",
        "kastellan-worker-web-search",
        "kastellan-worker-python-exec",
        "kastellan-worker-matrix",
        "kastellan-worker-lockdown-exec",
    ]
}

/// The supervisor specs + target for the install, with absolute prefix paths.
pub struct InstallSpecs {
    pub members: Vec<ServiceSpec>,
    pub target: TargetSpec,
}

/// Build the postgres + core service specs (start order) + the target.
/// `postgres_binary` is the resolved absolute path to the `postgres` exe.
pub fn build_specs(layout: &Layout, postgres_binary: &Path) -> InstallSpecs {
    let postgres = postgres_service_spec(postgres_binary, &layout.data_dir, &layout.log_dir);
    let mut core = core_service_spec(&layout.bin_dir.join("kastellan"), &layout.log_dir);
    core.environment_file = Some(layout.env_file.clone());
    InstallSpecs { members: vec![postgres, core], target: kastellan_target_spec() }
}

/// Parsed `install` arguments.
pub struct InstallArgs {
    pub llm_model: String,
    pub llm_url: String,
    pub embedding_model: Option<String>,
    pub pg_bin_dir: Option<PathBuf>,
    pub from: Option<PathBuf>,
    pub no_start: bool,
}

/// Parse `install [--llm-model <m>] [--llm-url <u>] [--embedding-model <m>] [--pg-bin-dir <d>] [--from <d>] [--no-start]`.
pub fn parse_install_args(args: &[String]) -> Result<InstallArgs, String> {
    let (mut model, mut url, mut emb, mut pg, mut from, mut no_start) =
        (None::<String>, None::<String>, None::<String>, None::<PathBuf>, None::<PathBuf>, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--llm-model" => { model = Some(take(args, &mut i, "--llm-model")?); }
            "--llm-url" => { url = Some(take(args, &mut i, "--llm-url")?); }
            "--embedding-model" => { emb = Some(take(args, &mut i, "--embedding-model")?); }
            "--pg-bin-dir" => { pg = Some(PathBuf::from(take(args, &mut i, "--pg-bin-dir")?)); }
            "--from" => { from = Some(PathBuf::from(take(args, &mut i, "--from")?)); }
            "--no-start" => { no_start = true; i += 1; }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let llm_model = model.ok_or("install requires --llm-model <name>")?;
    Ok(InstallArgs {
        llm_model,
        llm_url: url.unwrap_or_else(|| default_llm_url().to_string()),
        embedding_model: emb,
        pg_bin_dir: pg,
        from,
        no_start,
    })
}

fn take(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    let v = args.get(*i + 1).ok_or_else(|| format!("{flag} requires a value"))?.clone();
    *i += 2;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn layout() -> Layout {
        resolve_layout(Path::new("/home/u"), "u")
    }

    #[test]
    fn layout_uses_xdg_per_user_paths() {
        let l = layout();
        assert_eq!(l.bin_dir, PathBuf::from("/home/u/.local/lib/kastellan"));
        assert_eq!(l.assets_dir, PathBuf::from("/home/u/.local/share/kastellan"));
        assert_eq!(l.prompts_dir, PathBuf::from("/home/u/.local/share/kastellan/prompts"));
        assert_eq!(l.l0_rules_file, PathBuf::from("/home/u/.local/share/kastellan/seeds/memory/l0_meta_rules.toml"));
        assert_eq!(l.data_dir, PathBuf::from("/home/u/.local/share/kastellan/pg/data"));
        assert_eq!(l.config_dir, PathBuf::from("/home/u/.config/kastellan"));
        assert_eq!(l.env_file, PathBuf::from("/home/u/.config/kastellan/kastellan.env"));
        assert_eq!(l.log_dir, PathBuf::from("/home/u/.local/state/kastellan"));
    }

    #[test]
    fn env_file_has_all_required_keys_and_prefix_paths() {
        let l = layout();
        let s = render_env_file("my-model", "http://127.0.0.1:8000", Some("emb-model"), &l);
        assert!(s.contains("KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:8000\n"));
        assert!(s.contains("KASTELLAN_LLM_LOCAL_MODEL=my-model\n"));
        assert!(s.contains("KASTELLAN_LLM_EMBEDDING_URL=http://127.0.0.1:8000\n"));
        assert!(s.contains("KASTELLAN_LLM_EMBEDDING_MODEL=emb-model\n"));
        assert!(s.contains("KASTELLAN_PROMPTS_DIR=/home/u/.local/share/kastellan/prompts\n"));
        assert!(s.contains("KASTELLAN_L0_RULES_FILE=/home/u/.local/share/kastellan/seeds/memory/l0_meta_rules.toml\n"));
        assert!(s.contains("KASTELLAN_DATA_DIR=/home/u/.local/share/kastellan/pg/data\n"));
    }

    #[test]
    fn env_file_omits_embedding_model_when_absent() {
        let s = render_env_file("m", "u", None, &layout());
        assert!(!s.contains("KASTELLAN_LLM_EMBEDDING_MODEL="));
    }

    #[test]
    fn required_binaries_include_daemon_and_egress_proxy() {
        let r = required_binaries();
        assert!(r.contains(&"kastellan"));
        assert!(r.contains(&"kastellan-db-init"));
        assert!(r.contains(&"kastellan-worker-egress-proxy"));
        // optional set holds the on-demand workers
        assert!(optional_binaries().contains(&"kastellan-worker-matrix"));
        assert!(optional_binaries().contains(&"kastellan-worker-lockdown-exec"));
    }

    #[test]
    fn specs_point_core_at_installed_binary_and_env_file() {
        let l = layout();
        let specs = build_specs(&l, Path::new("/usr/lib/postgresql/18/bin/postgres"));
        assert_eq!(specs.members.len(), 2);
        let core = specs.members.iter().find(|s| s.name == "kastellan-core").unwrap();
        assert_eq!(core.program, PathBuf::from("/home/u/.local/lib/kastellan/kastellan"));
        assert_eq!(core.environment_file.as_deref(), Some(l.env_file.as_path()));
        let pg = specs.members.iter().find(|s| s.name == "kastellan-postgres").unwrap();
        assert_eq!(pg.program, PathBuf::from("/usr/lib/postgresql/18/bin/postgres"));
        assert!(specs.target.members.contains(&"kastellan-postgres".to_string()));
        assert!(specs.target.members.contains(&"kastellan-core".to_string()));
    }

    #[test]
    fn parse_requires_llm_model_and_defaults_url() {
        let a = parse_install_args(&["--llm-model".into(), "m".into()]).unwrap();
        assert_eq!(a.llm_model, "m");
        assert_eq!(a.llm_url, default_llm_url());
        assert!(!a.no_start);
        assert!(parse_install_args(&[]).is_err()); // missing --llm-model
        assert!(parse_install_args(&["--bogus".into()]).is_err());
        let a2 = parse_install_args(&["--llm-model".into(), "m".into(), "--llm-url".into(), "http://x:1".into(), "--no-start".into()]).unwrap();
        assert_eq!(a2.llm_url, "http://x:1");
        assert!(a2.no_start);
    }
}
