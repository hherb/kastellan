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

/// The default local LLM URL: Ollama `:11434` on both OSes — it pairs with the
/// Ollama default models ([`DEFAULT_LLM_MODEL`]/[`DEFAULT_EMBEDDING_MODEL`]) and
/// is the backend the installer can `ollama pull` into. Operators on vLLM/MLX/etc.
/// override with `--llm-url` (and the matching `--llm-model`).
pub fn default_llm_url() -> &'static str {
    "http://127.0.0.1:11434"
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
    Ok(InstallArgs {
        llm_model: model.unwrap_or_else(|| DEFAULT_LLM_MODEL.to_string()),
        llm_url: url.unwrap_or_else(|| default_llm_url().to_string()),
        embedding_model: Some(emb.unwrap_or_else(|| DEFAULT_EMBEDDING_MODEL.to_string())),
        pg_bin_dir: pg,
        from,
        no_start,
    })
}

/// Default chat model (Ollama tag). Spike-tested as a strong general-purpose
/// default; override with `--llm-model`.
pub const DEFAULT_LLM_MODEL: &str = "gemma4:26b-a4b-it-q8_0";

/// Default embedding model (Ollama tag). Override with `--embedding-model`.
pub const DEFAULT_EMBEDDING_MODEL: &str = "embeddinggemma";

/// True when `url` looks like a *local* Ollama endpoint (loopback `:11434`),
/// the only case where the installer can drive `ollama pull` to fetch a model.
pub fn is_local_ollama(url: &str) -> bool {
    // authority = between "://" and the next "/", minus any "user@"
    let rest = url.split("://").nth(1).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = if let Some(after) = hostport.strip_prefix('[') {
        // [ipv6]:port
        match after.split_once("]:") {
            Some((h, p)) => (h.to_string(), p.to_string()),
            None => (after.trim_end_matches(']').to_string(), String::new()),
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.to_string()),
            None => (hostport.to_string(), String::new()),
        }
    };
    let host = host.to_ascii_lowercase();
    (host == "127.0.0.1" || host == "localhost" || host == "::1") && port == "11434"
}

/// Parse the parameter count from an Ollama model tag, e.g. `gemma4:26b-a4b…`
/// → 26e9 (the *total* params — what must fit in memory; the `aNb` active-param
/// figure is about compute, not footprint). Returns `None` if no `<n>b` token
/// is present. Decimal sizes like `1.5b` are supported.
pub fn parse_param_count(tag: &str) -> Option<u64> {
    let lower = tag.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    // Find the FIRST `<number>b` token (the total-param size).
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'b' {
                if let Ok(n) = lower[start..i].parse::<f64>() {
                    return Some((n * 1e9) as u64);
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Rough RAM footprint (bytes) for an Ollama model tag, from its parameter
/// count × bytes-per-param for the quantization. Approximate by design — used
/// only as a "will it obviously not fit" guard, not a precise sizing.
pub fn estimate_model_bytes(tag: &str) -> Option<u64> {
    let params = parse_param_count(tag)?;
    let lower = tag.to_ascii_lowercase();
    // bytes per parameter by quant family (q8≈1B, q6≈0.82, q5≈0.68, q4≈0.56,
    // fp16/f16≈2B); default to ~1B (conservative) when unlabelled.
    let bpp = if lower.contains("q8") {
        1.06
    } else if lower.contains("q6") {
        0.82
    } else if lower.contains("q5") {
        0.68
    } else if lower.contains("q4") {
        0.56
    } else if lower.contains("fp16") || lower.contains("f16") {
        2.0
    } else {
        1.0
    };
    Some((params as f64 * bpp) as u64)
}

/// Whether `total_mem_bytes` is enough to run a model of `model_bytes`: require
/// 1.2× the weights (KV cache / activations headroom) plus a 2 GiB OS reserve.
pub fn memory_suffices(model_bytes: u64, total_mem_bytes: u64) -> bool {
    let needed = model_bytes.saturating_mul(12) / 10 + 2 * 1024 * 1024 * 1024;
    total_mem_bytes >= needed
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
    fn parse_defaults_models_and_url_overridable() {
        // No flags → both models + url default.
        let d = parse_install_args(&[]).unwrap();
        assert_eq!(d.llm_model, DEFAULT_LLM_MODEL);
        assert_eq!(d.embedding_model.as_deref(), Some(DEFAULT_EMBEDDING_MODEL));
        assert_eq!(d.llm_url, default_llm_url());
        assert!(!d.no_start);
        // Overrides.
        let a = parse_install_args(&[
            "--llm-model".into(), "m".into(),
            "--embedding-model".into(), "e".into(),
            "--llm-url".into(), "http://x:1".into(),
            "--no-start".into(),
        ]).unwrap();
        assert_eq!(a.llm_model, "m");
        assert_eq!(a.embedding_model.as_deref(), Some("e"));
        assert_eq!(a.llm_url, "http://x:1");
        assert!(a.no_start);
        assert!(parse_install_args(&["--bogus".into()]).is_err());
    }

    #[test]
    fn parses_param_count_total_not_active() {
        assert_eq!(parse_param_count("gemma4:26b-a4b-it-q8_0"), Some(26_000_000_000));
        assert_eq!(parse_param_count("qwen3.5:9b-q8_0"), Some(9_000_000_000));
        assert_eq!(parse_param_count("gpt-oss:120B"), Some(120_000_000_000));
        assert_eq!(parse_param_count("nomic-embed-text-v2-moe:latest"), None);
        assert_eq!(parse_param_count("embeddinggemma"), None);
    }

    #[test]
    fn estimates_model_bytes_by_quant() {
        // 26B q8 ≈ 26e9 * 1.06 ≈ 27.6 GB
        let q8 = estimate_model_bytes("gemma4:26b-a4b-it-q8_0").unwrap();
        assert!((27_000_000_000..30_000_000_000).contains(&q8), "got {q8}");
        // q4 of the same is much smaller than q8.
        let q4 = estimate_model_bytes("foo:26b-q4_0").unwrap();
        assert!(q4 < q8);
        assert_eq!(estimate_model_bytes("embeddinggemma"), None);
    }

    #[test]
    fn memory_suffices_requires_headroom() {
        let twenty_gb = 20u64 * 1024 * 1024 * 1024;
        // 20 GB model needs 1.2x + 2 GB ≈ 26 GB → 32 GB suffices, 24 GB does not.
        assert!(memory_suffices(twenty_gb, 32 * 1024 * 1024 * 1024));
        assert!(!memory_suffices(twenty_gb, 24 * 1024 * 1024 * 1024));
    }

    #[test]
    fn detects_local_ollama_url() {
        assert!(is_local_ollama("http://127.0.0.1:11434"));
        assert!(is_local_ollama("http://localhost:11434"));
        assert!(!is_local_ollama("http://127.0.0.1:8000"));
        assert!(!is_local_ollama("http://10.0.0.5:11434")); // remote — can't drive `ollama pull`
        assert!(!is_local_ollama("http://127.0.0.1.evil.com:11434")); // not loopback host
        assert!(!is_local_ollama("http://127.0.0.1:114340"));         // not port 11434
        assert!(!is_local_ollama("http://127.0.0.1:11434x"));         // not numeric port 11434
        assert!(is_local_ollama("http://[::1]:11434"));               // ipv6 loopback
    }
}
