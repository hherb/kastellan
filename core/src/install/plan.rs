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
    /// Symlink placed on the operator's PATH (`~/.local/bin/kastellan-cli`)
    /// pointing at `bin_dir/kastellan-cli`. The flat prefix (`bin_dir`) lives
    /// under `~/.local/lib/` — not on PATH — so without this link operators
    /// can't reach the CLI and tend to hand-copy a binary elsewhere (which
    /// then goes stale). `current_exe()` resolves through the symlink to the
    /// real prefix path, so worker sibling-discovery is unaffected.
    pub cli_link: PathBuf,
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
        cli_link: home.join(".local/bin/kastellan-cli"),
        assets_dir,
        config_dir,
    }
}

/// System-wide bin dirs the per-user CLI symlink must take precedence over.
/// A machine may host one Kastellan per user, so each user's `~/.local/bin`
/// has to win over any global install — otherwise a system-wide (or stale,
/// hand-copied) `kastellan-cli` shadows every user's per-user one.
const GLOBAL_BIN_DIRS: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// Check that the per-user CLI dir (`~/.local/bin`) will take precedence over
/// system-wide bin dirs on the operator's `PATH`. Pure: PATH string in, advice
/// out. Returns `None` when precedence is already correct, or `Some(warning)`
/// with exact remediation when `~/.local/bin` is absent from PATH or sits
/// *after* a global bin dir (so a global `kastellan-cli` would shadow the
/// per-user one). The check is per-user by design — essential on a host that
/// runs one Kastellan instance per user.
pub fn cli_path_precedence_note(path_var: &str, home: &Path) -> Option<String> {
    let local_bin = home.join(".local/bin");
    let local_bin = local_bin.to_string_lossy();
    let entries: Vec<&str> = path_var.split(':').filter(|s| !s.is_empty()).collect();
    let local_idx = entries.iter().position(|e| *e == local_bin);

    let remedy = "Put it first so the per-user install always wins (essential on a \
         multi-user host): add to your shell rc — export PATH=\"$HOME/.local/bin:$PATH\"";
    match local_idx {
        None => Some(format!(
            "warning: {local_bin} (the per-user CLI dir) is not on PATH — `kastellan-cli` won't be found there. {remedy}"
        )),
        Some(idx) => {
            // Any global bin dir appearing *before* ~/.local/bin would shadow it.
            let shadower = entries[..idx].iter().find(|e| GLOBAL_BIN_DIRS.contains(e));
            shadower.map(|g| format!(
                "warning: {g} precedes {local_bin} on PATH, so a system-wide `kastellan-cli` there would shadow this per-user install. {remedy}"
            ))
        }
    }
}

/// The default local LLM URL: Ollama `:11434` on both OSes — it pairs with the
/// Ollama default models ([`DEFAULT_LLM_MODEL`]/[`DEFAULT_EMBEDDING_MODEL`]) and
/// is the backend the installer can `ollama pull` into. Operators on vLLM/MLX/etc.
/// override with `--llm-url` (and the matching `--llm-model`).
pub fn default_llm_url() -> &'static str {
    "http://127.0.0.1:11434"
}

/// Ensure an OpenAI-compatible base URL ends in exactly one `/v1` segment — the
/// path the LLM router appends `/chat/completions` onto. Ollama and vLLM both
/// serve their OpenAI-compatible API under `/v1`; the installer's default
/// `…:11434` base omits it, so without this the router hits `…/chat/completions`
/// → HTTP 404. Idempotent (a URL already ending in `/v1` is returned unchanged).
///
/// Deliberately assumes an OpenAI-style base: it appends `/v1` to *any* URL that
/// doesn't already end in one (so `http://h:8000` → `…/v1`). A backend exposing
/// its OpenAI API under a different path is out of scope — set `--llm-url` to the
/// full base (ending in `/v1`) and this is a no-op.
pub fn ensure_v1_suffix(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

/// Render the `kastellan.env` EnvironmentFile contents.
pub fn render_env_file(args: &InstallArgs, layout: &Layout) -> String {
    let mut s = String::new();
    let router_url = ensure_v1_suffix(&args.llm_url);
    s.push_str(&format!("KASTELLAN_LLM_LOCAL_URL={router_url}\n"));
    s.push_str(&format!("KASTELLAN_LLM_LOCAL_MODEL={}\n", args.llm_model));
    s.push_str(&format!("KASTELLAN_LLM_EMBEDDING_URL={router_url}\n"));
    if let Some(em) = args.embedding_model.as_deref() {
        s.push_str(&format!("KASTELLAN_LLM_EMBEDDING_MODEL={em}\n"));
    }
    s.push_str(&format!("KASTELLAN_PROMPTS_DIR={}\n", layout.prompts_dir.display()));
    s.push_str(&format!("KASTELLAN_L0_RULES_FILE={}\n", layout.l0_rules_file.display()));
    s.push_str(&format!("KASTELLAN_DATA_DIR={}\n", layout.data_dir.display()));
    // Planner "now" timezone (IANA name) for the trusted <now> block that stops
    // date-relative questions from web-searching for the current date. Unset →
    // host system tz; invalid → UTC. Commented by default so the host tz is used.
    s.push_str("# KASTELLAN_TIMEZONE=Australia/Sydney\n");
    // web.search_batch size cap (queries per batch). Commented → the worker
    // default (8) applies; raise/lower to tune how many independent searches the
    // planner may issue in one dispatch. Clamped by the worker to [1, 32].
    s.push_str("# KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES=8\n");
    if let (Some(hs), Some(user)) =
        (args.matrix_homeserver_url.as_deref(), args.matrix_user.as_deref())
    {
        // Matrix inbound channel (comms slice #2). The worker must be the
        // `live-matrix` build; run `kastellan-cli matrix probe` once after
        // install to seed its E2E session + cross-signing. Worker-side
        // seccomp (`matrix_client`, applied across all threads via TSYNC) +
        // Landlock are enforced by default (`=1`); set `=0` only as an operator
        // debug escape hatch. Egress force-routing remains a separate follow-up.
        s.push_str(&format!("KASTELLAN_MATRIX_HOMESERVER_URL={hs}\n"));
        s.push_str(&format!("KASTELLAN_MATRIX_USER={user}\n"));
        s.push_str("KASTELLAN_MATRIX_ENFORCE_SANDBOX=1\n");
    }
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
    /// When both are set, the installer writes the Matrix channel env so the
    /// daemon brings up the inbound channel (comms slice #2). Requires the
    /// `live-matrix` worker build + a one-time `kastellan-cli matrix probe` to
    /// seed the E2E session/cross-signing.
    pub matrix_homeserver_url: Option<String>,
    pub matrix_user: Option<String>,
}

/// Parse `install [--llm-model <m>] [--llm-url <u>] [--embedding-model <m>] [--pg-bin-dir <d>] [--from <d>]
/// [--matrix-homeserver-url <u> --matrix-user <@u:server>] [--no-start]`. The two `--matrix-*` flags must be
/// given together (one without the other is an error).
pub fn parse_install_args(args: &[String]) -> Result<InstallArgs, String> {
    let (mut model, mut url, mut emb, mut pg, mut from, mut no_start) =
        (None::<String>, None::<String>, None::<String>, None::<PathBuf>, None::<PathBuf>, false);
    let (mut matrix_hs, mut matrix_user) = (None::<String>, None::<String>);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--llm-model" => { model = Some(take(args, &mut i, "--llm-model")?); }
            "--llm-url" => { url = Some(take(args, &mut i, "--llm-url")?); }
            "--embedding-model" => { emb = Some(take(args, &mut i, "--embedding-model")?); }
            "--pg-bin-dir" => { pg = Some(PathBuf::from(take(args, &mut i, "--pg-bin-dir")?)); }
            "--from" => { from = Some(PathBuf::from(take(args, &mut i, "--from")?)); }
            "--matrix-homeserver-url" => { matrix_hs = Some(take(args, &mut i, "--matrix-homeserver-url")?); }
            "--matrix-user" => { matrix_user = Some(take(args, &mut i, "--matrix-user")?); }
            "--no-start" => { no_start = true; i += 1; }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if matrix_hs.is_some() != matrix_user.is_some() {
        return Err(
            "--matrix-homeserver-url and --matrix-user must be given together".to_string(),
        );
    }
    Ok(InstallArgs {
        llm_model: model.unwrap_or_else(|| DEFAULT_LLM_MODEL.to_string()),
        llm_url: url.unwrap_or_else(|| default_llm_url().to_string()),
        embedding_model: Some(emb.unwrap_or_else(|| DEFAULT_EMBEDDING_MODEL.to_string())),
        pg_bin_dir: pg,
        from,
        no_start,
        matrix_homeserver_url: matrix_hs,
        matrix_user,
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
        // The operator CLI is symlinked onto PATH (~/.local/bin), not left in the lib prefix.
        assert_eq!(l.cli_link, PathBuf::from("/home/u/.local/bin/kastellan-cli"));
    }

    #[test]
    fn cli_path_precedence_ok_when_local_bin_is_first() {
        let home = Path::new("/home/u");
        // ~/.local/bin ahead of the global dirs → no warning.
        assert_eq!(
            cli_path_precedence_note("/home/u/.local/bin:/usr/local/bin:/usr/bin", home),
            None
        );
    }

    #[test]
    fn cli_path_precedence_warns_when_global_precedes_local() {
        let home = Path::new("/home/u");
        // /usr/local/bin BEFORE ~/.local/bin → a global CLI would shadow the per-user one.
        let note = cli_path_precedence_note("/usr/local/bin:/home/u/.local/bin", home)
            .expect("should warn");
        assert!(note.contains("/usr/local/bin"), "{note}");
        assert!(note.contains("/home/u/.local/bin"), "{note}");
        assert!(note.contains("export PATH"), "{note}");
    }

    #[test]
    fn cli_path_precedence_warns_when_local_bin_absent() {
        let home = Path::new("/home/u");
        let note = cli_path_precedence_note("/usr/bin:/bin", home).expect("should warn");
        assert!(note.contains("not on PATH"), "{note}");
    }

    fn test_args(model: &str, url: &str, embedding_model: Option<&str>) -> InstallArgs {
        InstallArgs {
            llm_model: model.to_string(),
            llm_url: url.to_string(),
            embedding_model: embedding_model.map(str::to_string),
            pg_bin_dir: None,
            from: None,
            no_start: false,
            matrix_homeserver_url: None,
            matrix_user: None,
        }
    }

    #[test]
    fn env_file_has_all_required_keys_and_prefix_paths() {
        let l = layout();
        let s = render_env_file(&test_args("my-model", "http://127.0.0.1:8000", Some("emb-model")), &l);
        // URLs are normalized to the router's `/v1` base.
        assert!(s.contains("KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:8000/v1\n"), "{s}");
        assert!(s.contains("KASTELLAN_LLM_LOCAL_MODEL=my-model\n"));
        assert!(s.contains("KASTELLAN_LLM_EMBEDDING_URL=http://127.0.0.1:8000/v1\n"), "{s}");
        assert!(s.contains("KASTELLAN_LLM_EMBEDDING_MODEL=emb-model\n"));
        assert!(s.contains("KASTELLAN_PROMPTS_DIR=/home/u/.local/share/kastellan/prompts\n"));
        assert!(s.contains("KASTELLAN_L0_RULES_FILE=/home/u/.local/share/kastellan/seeds/memory/l0_meta_rules.toml\n"));
        assert!(s.contains("KASTELLAN_DATA_DIR=/home/u/.local/share/kastellan/pg/data\n"));
        // Planner timezone documented (commented — unset uses the host tz).
        assert!(s.contains("# KASTELLAN_TIMEZONE=Australia/Sydney\n"), "{s}");
        // web.search_batch size cap documented (commented — worker default 8).
        assert!(
            s.contains("# KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES=8\n"),
            "{s}"
        );
        // No matrix block unless configured.
        assert!(!s.contains("KASTELLAN_MATRIX_HOMESERVER_URL"));
    }

    #[test]
    fn env_file_omits_embedding_model_when_absent() {
        let s = render_env_file(&test_args("m", "http://h:1", None), &layout());
        assert!(!s.contains("KASTELLAN_LLM_EMBEDDING_MODEL="));
    }

    #[test]
    fn ensure_v1_suffix_idempotent_and_appends() {
        assert_eq!(ensure_v1_suffix("http://127.0.0.1:11434"), "http://127.0.0.1:11434/v1");
        assert_eq!(ensure_v1_suffix("http://127.0.0.1:11434/"), "http://127.0.0.1:11434/v1");
        assert_eq!(ensure_v1_suffix("http://x:8000/v1"), "http://x:8000/v1");
        assert_eq!(ensure_v1_suffix("http://x:8000/v1/"), "http://x:8000/v1");
    }

    #[test]
    fn env_file_writes_matrix_block_when_configured() {
        let mut a = test_args("m", "http://127.0.0.1:11434", Some("e"));
        a.matrix_homeserver_url = Some("https://matrix.example.org".to_string());
        a.matrix_user = Some("@bot:matrix.example.org".to_string());
        let s = render_env_file(&a, &layout());
        assert!(s.contains("KASTELLAN_MATRIX_HOMESERVER_URL=https://matrix.example.org\n"), "{s}");
        assert!(s.contains("KASTELLAN_MATRIX_USER=@bot:matrix.example.org\n"), "{s}");
        assert!(s.contains("KASTELLAN_MATRIX_ENFORCE_SANDBOX=1\n"), "{s}");
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
