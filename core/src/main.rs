use anyhow::{anyhow, Context, Result};
use hhagent_core::audit_mirror::{self, MirrorHandle};
use hhagent_db::conn::ConnectSpec;
use hhagent_db::default_data_dir;
use sqlx::PgPool;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    info!(
        version = hhagent_core::VERSION,
        "hhagent core starting"
    );

    // Bring up the database before announcing readiness or accepting
    // any (future) work. Fail-closed: any error here propagates `?` to
    // a non-zero exit, the supervisor sees the failure, and the next
    // restart attempt re-runs the probe. Running degraded against a
    // half-bootstrapped database would silently lose audit-log rows
    // and corrupt memory writes — a much worse failure mode than a
    // restart loop, which at least surfaces in logs.
    let spec = bring_up_database().await?;

    // Open the daemon-scoped pool and start the audit-log JSONL
    // mirror task. The pool's `after_connect` hook drops privilege to
    // `hhagent_runtime` on every dialed connection (see
    // `db::pool` module docs); the mirror replicates committed
    // `audit_log` rows to `~/.local/state/hhagent/audit-*.jsonl` so
    // operators can `tail -f` without a DB client.
    //
    // Pool failures here are fatal (the dispatcher write site needs
    // them); mirror failures are NOT fatal — the mirror is an
    // operator-visibility layer, not a correctness requirement.
    let pool = hhagent_db::pool::connect_runtime_pool(&spec)
        .await
        .context("opening daemon-scoped Postgres pool")?;
    let mirror = start_audit_mirror(pool.clone()).await;

    // Crash sweep: any task left in 'running' from a previous daemon
    // instance whose lease has elapsed gets marked 'crashed'. Each
    // recovered task also gets one `scheduler/task.crashed` audit row
    // so observation-phase queries see the lifecycle transition.
    // Idempotent.
    match hhagent_core::scheduler::crash_recovery::sweep_and_audit(&pool).await {
        Ok(0) => {}
        Ok(n) => info!(crashed_tasks = n, "crash_recovery: swept tasks to 'crashed'"),
        Err(e) => tracing::warn!(error = %e, "crash_recovery::sweep_and_audit failed (non-fatal)"),
    }

    // LLM router (existing skeleton).
    let router_cfg = hhagent_llm_router::RouterConfig::from_env()
        .map_err(|e| anyhow!("RouterConfig::from_env: {e}"))?;
    let router = Arc::new(
        hhagent_llm_router::Router::new(router_cfg)
            .map_err(|e| anyhow!("Router::new: {e}"))?,
    );

    // Production review pipeline: stub stages in this scope (see spec
    // §6.1). Real implementations replace these structs in place.
    let review = Arc::new(
        hhagent_core::cassandra::review::ChainReviewStage::new(vec![
            Arc::new(hhagent_core::cassandra::review::ConstitutionalGuard),
            Arc::new(hhagent_core::cassandra::review::DeterministicPolicy),
        ]),
    );

    // System-prompt builder: loads L0 (meta-rules) + L1 (insight index)
    // from the runtime pool on every plan iteration and frames them as
    // <l0_meta_rules>/<l1_insights>/<base> before each LLM call. Holds
    // PgPool by value (sqlx wraps connections in an internal Arc so
    // pool.clone() is cheap).
    // Sandbox-backend bundle (Slice 2). On darwin holds both Seatbelt
    // (the per-OS default) and the Container backend so individual
    // workers can opt in to memory enforcement via
    // `ToolEntry.sandbox_backend = Some(SandboxBackendKind::Container)`.
    // On linux holds just `LinuxBwrap`. Cheap to construct; each backend
    // is a unit-like struct with no I/O at construction.
    let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());

    // Worker lifecycle (spec
    // `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`).
    //
    // Created once and shared between the step dispatcher (existing
    // consumer) and the v2 entity-extraction client (new consumer). The
    // same `Arc` is the same warm-keep slot for gliner-relex regardless
    // of whether the call originates from a PlannedStep or an extractor
    // invocation.
    //
    // The dispatcher gets a single `Arc<dyn WorkerLifecycleManager>`,
    // but `ToolEntry.lifecycle` may carry either `SingleUse`
    // (shell-exec — per-request isolation is its security model) or
    // `IdleTimeout` (gliner-relex — warm-keep the model across calls).
    // `CompositeLifecycle` routes each `acquire` call to the right
    // inner manager by inspecting `entry.lifecycle`. For deployments
    // that register only `SingleUse` entries (the default — gliner-relex
    // is opt-in via env), behaviour is byte-equivalent to the prior
    // single-use-only wiring.
    let lifecycle: Arc<dyn hhagent_core::worker_lifecycle::WorkerLifecycleManager> = Arc::new(
        hhagent_core::worker_lifecycle::CompositeLifecycle::new(Arc::clone(&sandboxes)),
    );

    // Resolve gliner-relex once. The resolver emits its own
    // info!/error! line on each skip-reason; calling it twice (as an
    // earlier wiring did) would double up that signal in the operator
    // log. The `Option<ToolEntry>` flows into BOTH the registry insert
    // (so `PlannedStep`-routed callers can reach the same warm slot)
    // and the extractor construction (so `RouterAgent::formulate_plan`
    // gets a real extractor). `ToolEntry` is `Clone`; the duplication
    // is intentional and load-bearing per the v2 design spec.
    let gliner_relex_entry = build_gliner_relex_entry();

    // Tool registry: each tool the scheduler may dispatch is opted in
    // here. The registry is the host-side allowlist of *which* tools
    // exist (separate from the per-tool argv allowlist, which lives
    // in the `tool_allowlists` DB table).
    //
    // Operators control which tools are reachable via the DB. An
    // absent / empty `HHAGENT_SHELL_EXEC_BIN` (e.g. the worker binary
    // wasn't installed) means shell-exec is simply not registered —
    // `dispatch_step` then returns `UNKNOWN_TOOL` for any plan trying
    // to use it, and the inner loop replans accordingly. This is the
    // same deny-by-default posture used in the egress proxy plan
    // (Phase 3).
    let tool_registry = Arc::new(build_tool_registry(&pool, gliner_relex_entry.clone()).await?);

    // Container-image health check (issue #120). Walks every registered
    // ToolEntry, collects each distinct `container_image` tag owned by
    // a Container-backed worker, and probes each tag via `container
    // image inspect`. A missing image yields one `tracing::warn!` line
    // per affected tag (naming the affected tools) and the daemon
    // continues bring-up — the worker's first dispatch will fail via
    // the normal spawn-error path, but the operator was already
    // warned at boot with an actionable diagnostic ("run
    // scripts/workers/<worker>/build-image.sh").
    //
    // macOS-only because the `Container` variant of
    // `SandboxBackendKind` is cfg-gated to darwin; on Linux the walk
    // is structurally a no-op (cf.
    // `sandbox_health::collect_container_image_targets` Linux stub).
    // The bare-feature inversion (cfg on call site, not on module) is
    // deliberate — the pure target-collection helper compiles
    // cross-platform so unit tests still exercise the bucket-sort and
    // dedup logic on Linux runners.
    #[cfg(target_os = "macos")]
    {
        let _probe_results = hhagent_core::sandbox_health::probe_registered_container_images(
            tool_registry.entries(),
        );
    }

    // Entity extractor (v2). When gliner-relex is configured, builds a
    // typed Client over the shared lifecycle Arc + worker manifest and
    // returns GlinerRelexExtractor. When the worker isn't configured
    // (HHAGENT_GLINER_RELEX_ENABLE=0 or preconditions failed), falls
    // back to NoOpEntityExtractor — daemon stays up; graph lane stays
    // empty. The skip-reason log was already emitted by
    // `build_gliner_relex_entry` above; this branch only logs the
    // post-resolution wiring outcome.
    let entity_extractor: Arc<dyn hhagent_core::entity_extraction::EntityExtractor> =
        match gliner_relex_entry {
            Some(entry) => {
                tracing::info!(
                    target: "hhagent::main",
                    "gliner-relex configured; constructing v2 entity extractor",
                );
                let client = hhagent_core::workers::gliner_relex::Client::new(
                    lifecycle.clone(),
                    pool.clone(),
                    entry,
                );
                Arc::new(
                    hhagent_core::entity_extraction::gliner_relex::GlinerRelexExtractor::new(
                        client,
                        pool.clone(),
                    ),
                )
            }
            None => {
                // WARN level per the v2 design spec's failure-mode
                // matrix ("HHAGENT_GLINER_RELEX_ENABLE=0 (default) or
                // weights missing | Daemon starts; one WARN line at
                // startup"). The resolver's own info!/error! line was
                // already emitted; this is the wiring-outcome breadcrumb.
                tracing::warn!(
                    target: "hhagent::main",
                    "gliner-relex not configured; using NoOpEntityExtractor (graph lane disabled)",
                );
                Arc::new(hhagent_core::entity_extraction::NoOpEntityExtractor::new())
            }
        };

    // Load every prompts/*.md, hash, upsert into agent_prompts.
    let prompts_dir = std::env::var("HHAGENT_PROMPTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("prompts"));
    let prompts = hhagent_core::scheduler::prompts::load_prompts_from_dir(&pool, &prompts_dir)
        .await
        .with_context(|| format!("loading prompts from {:?}", prompts_dir))?;

    // Seed L0 (meta-rule) rows from the operator-edited TOML file.
    // Default: `seeds/memory/l0_meta_rules.toml` relative to CWD.
    // Override: `HHAGENT_L0_RULES_FILE` env var. Missing file is
    // logged at info level and skipped (daemon still comes up).
    // Malformed file is fatal (loader returns Err, ? propagates) —
    // matches probe::run fail-closed posture.
    let l0_path = std::env::var("HHAGENT_L0_RULES_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("seeds/memory/l0_meta_rules.toml"));
    if l0_path.exists() {
        let report = hhagent_core::memory::l0_seed::seed_l0_from_file(
            &pool, &*entity_extractor, &l0_path,
        )
        .await
        .with_context(|| format!("seeding L0 rules from {:?}", l0_path))?;
        // Best-effort audit row: a transient DB failure here must not
        // block daemon bring-up. The L0 rows themselves are already
        // committed; mirrors `write_registry_loaded_row` posture.
        if let Err(e) = write_l0_seeded_row(&pool, &report).await {
            tracing::warn!(error = %e, "l0.seeded audit row insert failed");
        }
        info!(
            rules = report.rules_loaded,
            new = report.new_rows_written,
            unchanged = report.unchanged_skipped,
            entities_linked = report.entities_linked,
            link_failures = report.link_failures,
            "L0 seed loader completed"
        );
    } else {
        info!(path = ?l0_path, "no L0 rules file found, skipping seed");
    }

    // PlanFormulator — takes the extractor as 5th arg (Task 14 widened
    // the signature; Task 15 supplies the constructed extractor).
    let formulator: Arc<dyn hhagent_core::scheduler::agent::PlanFormulator> =
        Arc::new(hhagent_core::scheduler::agent::RouterAgent::new(
            router.clone(),
            prompts.clone(),
            Arc::new(hhagent_core::prompt_assembly::PgSystemPromptBuilder::new(pool.clone())),
            Arc::new(hhagent_core::recall_assembly::PgRecallBuilder::new(
                pool.clone(),
                router.clone(),
            )),
            entity_extractor.clone(),
        ));

    let dispatcher: Arc<dyn hhagent_core::scheduler::inner_loop::StepDispatcher> =
        Arc::new(
            hhagent_core::scheduler::tool_dispatch::ToolHostStepDispatcher::new(
                pool.clone(),
                lifecycle,
                tool_registry,
            ),
        );

    let scheduler = hhagent_core::scheduler::spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        entity_extractor.clone(),
    );
    info!("scheduler spawned (lane_fast + lane_long)");

    wait_for_shutdown().await?;

    // Stop the scheduler before the audit-mirror so any final audit
    // rows it writes during graceful drain land in the mirror's
    // catch-up SELECT.
    scheduler.shutdown().await;

    // Graceful shutdown: stop the mirror task first so any in-flight
    // catch-up SELECT completes its fsync, then close the pool.
    if let Some(handle) = mirror {
        handle.shutdown().await;
    }
    pool.close().await;

    info!("hhagent core shutting down");
    Ok(())
}

/// Resolve cluster connection params from the environment, run the
/// `hhagent-db` probe, emit the bring-up `audit_log` row, and return
/// the resolved [`ConnectSpec`] for downstream pool/mirror setup.
///
/// Knobs:
///   * `HHAGENT_DATA_DIR` (optional) — absolute path to the cluster
///     data dir. The probe assumes
///     `default_socket_dir(data_dir) = <data_dir>/sockets`. Used by
///     integration tests (`core/tests/supervisor_e2e.rs`) to point
///     a test build of `hhagent` at a per-test temp cluster instead
///     of the user's installed one. Production deployments leave
///     this unset and rely on the `$HOME` default below.
///   * `$HOME` — used by `default_data_dir()` when
///     `HHAGENT_DATA_DIR` is unset.
///   * `$USER` — peer-auth role identity (read by
///     `ConnectSpec::default_for`). systemd's `--user` manager and
///     macOS launchd both inherit it from the operator's login
///     record; the probe fails closed if it's missing.
async fn bring_up_database() -> Result<ConnectSpec> {
    let data_dir = match std::env::var_os("HHAGENT_DATA_DIR") {
        Some(p) => std::path::PathBuf::from(p),
        None => default_data_dir()
            .ok_or_else(|| anyhow!("$HOME unset; cannot resolve cluster data dir"))?,
    };
    let spec = ConnectSpec::default_for(&data_dir)
        .context("resolving Postgres connection from environment")?;

    info!(
        data_dir = %data_dir.display(),
        socket_dir = %spec.socket_dir.display(),
        user = %spec.user,
        database = %spec.database,
        "running database probe"
    );

    hhagent_db::probe::run(
        &spec,
        "core",
        "startup",
        serde_json::json!({
            "version": hhagent_core::VERSION,
        }),
    )
    .await
    .context("hhagent_db::probe::run failed")?;

    info!("{}", hhagent_core::STARTUP_READY_MSG);
    Ok(spec)
}

/// Spawn the audit-log JSONL mirror task.
///
/// Uses [`audit_mirror::ENV_STATE_DIR`] when set (test seam, mirroring
/// `HHAGENT_DATA_DIR` for the cluster path), otherwise
/// [`audit_mirror::default_state_dir`] = `$HOME/.local/state/hhagent`.
///
/// Returns `None` if the mirror task could not be spawned. We log the
/// error and continue rather than aborting daemon startup: the audit
/// row in Postgres is the source of truth, and missing JSONL output
/// is an operator-visibility regression, not a correctness one. A
/// future hardening pass could promote this to fail-closed if the
/// JSONL stream becomes a contractual signal for any consumer.
async fn start_audit_mirror(pool: PgPool) -> Option<MirrorHandle> {
    let state_dir = match std::env::var_os(audit_mirror::ENV_STATE_DIR) {
        Some(p) => std::path::PathBuf::from(p),
        None => match audit_mirror::default_state_dir() {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "$HOME unset; audit_mirror disabled (operator visibility \
                     reduced — DB row is still the source of truth)"
                );
                return None;
            }
        },
    };
    match audit_mirror::spawn_mirror(pool, state_dir.clone()).await {
        Ok(h) => {
            info!(state_dir = %state_dir.display(), "audit_mirror spawned");
            Some(h)
        }
        Err(e) => {
            tracing::error!(
                state_dir = %state_dir.display(),
                error = %e,
                "audit_mirror spawn failed; continuing without on-disk JSONL"
            );
            None
        }
    }
}

/// Block until the supervisor (or an interactive operator) tells us
/// to stop. systemd's `systemctl --user stop` sends SIGTERM by default;
/// macOS launchd's `bootout` sends SIGTERM too. SIGINT is the Ctrl-C
/// path for `cargo run` in dev. Either signal returns Ok and lets
/// `main` log a clean shutdown line and exit 0 — exactly what
/// `Restart=on-failure` (systemd's translation of `keep_alive=true`)
/// treats as success, so a stop-induced exit doesn't trip the restart
/// policy and trigger an unwanted respawn.
async fn wait_for_shutdown() -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}

/// Build the registry of tools the scheduler may dispatch.
///
/// Reads the shell-exec argv allowlist from the `tool_allowlists` DB
/// table (migration `0009`). `HHAGENT_SHELL_EXEC_ALLOWLIST` is no
/// longer honored — a WARN is emitted if it is still set so operators
/// know to migrate.
///
/// If `HHAGENT_SHELL_EXEC_BIN` is unset or the path does not exist,
/// shell-exec is simply not registered. A plan that tries to use it
/// will surface `UNKNOWN_TOOL` from the dispatcher.
///
/// Emits one `actor='core' action='registry.loaded'` audit row carrying
/// a per-tool summary (name, binary path, allowlist length, SHA-256 of
/// the canonical-form allowlist). DB error during the load aborts
/// bring-up; error during the audit row insert is best-effort only (a
/// WARN is logged and bring-up continues).
async fn build_tool_registry(
    pool: &sqlx::PgPool,
    gliner_relex_entry: Option<hhagent_core::scheduler::tool_dispatch::ToolEntry>,
) -> anyhow::Result<hhagent_core::scheduler::ToolRegistry> {
    use anyhow::Context as _;
    let mut reg = hhagent_core::scheduler::ToolRegistry::new();
    let mut loaded: Vec<LoadedToolRecord> = Vec::new();

    if let Some(bin_os) = std::env::var_os("HHAGENT_SHELL_EXEC_BIN") {
        let binary = std::path::PathBuf::from(&bin_os);
        if binary.is_file() {
            let allowlist = hhagent_db::tool_allowlists::list_for_tool(pool, "shell-exec")
                .await
                .context("loading shell-exec allowlist from DB")?;
            let entry = hhagent_core::scheduler::shell_exec_entry(binary.clone(), &allowlist);
            info!(
                tool = "shell-exec",
                binary = %binary.display(),
                allowlist_len = allowlist.len(),
                "registering tool"
            );
            loaded.push(LoadedToolRecord {
                name: "shell-exec".to_string(),
                binary: binary.display().to_string(),
                allowlist_len: allowlist.len(),
                allowlist_sha256: sha256_argv0_list(&allowlist),
            });
            reg.insert("shell-exec", entry);
        } else {
            tracing::warn!(
                binary = %binary.display(),
                "HHAGENT_SHELL_EXEC_BIN does not point to an existing file; \
                 shell-exec NOT registered"
            );
        }
    }

    // Deprecation warning — does not block bring-up.
    if std::env::var_os("HHAGENT_SHELL_EXEC_ALLOWLIST").is_some() {
        tracing::warn!(
            "HHAGENT_SHELL_EXEC_ALLOWLIST is no longer honored; \
             use 'hhagent-cli tools allowlist add <tool> <argv0>' to populate the DB"
        );
    }

    // gliner-relex (opt-in: env-gated). Skip-register by default so
    // existing deployments are byte-equivalent to pre-slice main. The
    // entry is resolved once at `main()` startup and threaded in via
    // the parameter so the skip-reason log line fires exactly once per
    // bring-up.
    if let Some(entry) = gliner_relex_entry {
        info!(
            tool = hhagent_core::workers::gliner_relex::Client::TOOL_NAME,
            binary = %entry.binary.display(),
            "registering tool"
        );
        // No allowlist concept for gliner-relex (it has one method,
        // `extract`; no argv-style command surface). The
        // `LoadedToolRecord` shape requires `allowlist_*` fields so
        // the audit-row schema stays uniform; populate them with the
        // empty-list canonical form.
        loaded.push(LoadedToolRecord {
            name: hhagent_core::workers::gliner_relex::Client::TOOL_NAME.to_string(),
            binary: entry.binary.display().to_string(),
            allowlist_len: 0,
            allowlist_sha256: sha256_argv0_list(&[]),
        });
        reg.insert(hhagent_core::workers::gliner_relex::Client::TOOL_NAME, entry);
    }

    // Best-effort audit row: a transient DB failure here must not
    // block daemon bring-up. The allowlist itself has already been
    // loaded successfully.
    if let Err(e) = write_registry_loaded_row(pool, &loaded).await {
        tracing::warn!(error = %e, "registry.loaded audit row insert failed");
    }

    Ok(reg)
}

/// Build the GLiNER-Relex tool entry from environment variables.
///
/// Thin daemon-startup wrapper around
/// [`hhagent_core::workers::gliner_relex::resolve_env`]: passes the
/// real `std::env::var` + [`std::path::Path::is_dir`] /
/// [`std::path::Path::exists`] predicates, then converts the typed
/// [`hhagent_core::workers::gliner_relex::ResolveSkipReason`] into a
/// structured `tracing::info!` / `tracing::error!` line. Returns `None`
/// on every skip path so the daemon boots without the worker. Fail-closed
/// per the design spec — the daemon continues but the operator log says
/// exactly why the worker isn't reachable.
///
/// Env vars consulted (full list documented on `resolve_env`):
///
/// - `HHAGENT_GLINER_RELEX_ENABLE` — must be `"1"` (trimmed). Default
///   skip-register.
/// - `HHAGENT_GLINER_RELEX_WEIGHTS_DIR` — required; absolute path.
/// - `HHAGENT_GLINER_RELEX_MODEL` (default `multi-v1.0`).
/// - `HHAGENT_GLINER_RELEX_DEVICE` (default `auto`).
/// - `HHAGENT_GLINER_RELEX_VENV_DIR` (default
///   `$HHAGENT_DATA_DIR/workers/gliner-relex/.venv`, last-resort
///   `$HOME/.local/share/hhagent/...`).
fn build_gliner_relex_entry()
-> Option<hhagent_core::scheduler::tool_dispatch::ToolEntry> {
    use hhagent_core::workers::gliner_relex::{gliner_relex_entry, resolve_env};

    match resolve_env(
        |k| std::env::var(k).ok(),
        |p| p.is_dir(),
        |p| p.exists(),
    ) {
        Ok(env) => Some(gliner_relex_entry(&env)),
        Err(reason) => {
            log_gliner_relex_skip(&reason);
            None
        }
    }
}

/// Convert a typed [`hhagent_core::workers::gliner_relex::ResolveSkipReason`]
/// into the appropriate `tracing` line. Kept separate from
/// `build_gliner_relex_entry` so the resolver-result branches stay
/// trivially reviewable.
fn log_gliner_relex_skip(
    reason: &hhagent_core::workers::gliner_relex::ResolveSkipReason,
) {
    use hhagent_core::workers::gliner_relex::ResolveSkipReason as R;
    match reason {
        R::Disabled => tracing::info!(
            "gliner-relex: HHAGENT_GLINER_RELEX_ENABLE != \"1\"; skip registering"
        ),
        R::WeightsDirEnvMissing => tracing::error!(
            "gliner-relex enabled but HHAGENT_GLINER_RELEX_WEIGHTS_DIR unset; \
             skip registering"
        ),
        R::WeightsDirNotADir { path } => tracing::error!(
            weights_dir = %path.display(),
            "gliner-relex enabled but weights dir missing on disk; skip registering"
        ),
        R::VenvDirUnresolvable => tracing::error!(
            "gliner-relex enabled but venv dir unresolvable \
             (HHAGENT_GLINER_RELEX_VENV_DIR, HHAGENT_DATA_DIR, and HOME all unset); \
             skip registering"
        ),
        R::ScriptShimMissing { path } => tracing::error!(
            script_path = %path.display(),
            "gliner-relex enabled but venv shim missing; skip registering"
        ),
    }
}

/// One per-tool record carried in the `registry.loaded` audit-row
/// payload.
#[derive(serde::Serialize)]
struct LoadedToolRecord {
    name: String,
    binary: String,
    allowlist_len: usize,
    /// SHA-256 of the canonical-form allowlist:
    /// `argv0_1 || '\n' || argv0_2 || '\n' || …` where the list is
    /// lexicographically sorted and a trailing newline follows the
    /// last entry. Empty list → SHA-256 of the empty string.
    allowlist_sha256: String,
}

fn sha256_argv0_list(argv0s: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<&String> = argv0s.iter().collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    for argv0 in sorted {
        hasher.update(argv0.as_bytes());
        hasher.update(b"\n");
    }
    let bytes = hasher.finalize();
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

async fn write_registry_loaded_row(
    pool: &sqlx::PgPool,
    tools: &[LoadedToolRecord],
) -> Result<(), hhagent_db::DbError> {
    let payload = serde_json::json!({ "tools": tools });
    hhagent_db::audit::insert(
        pool,
        "core",
        hhagent_core::scheduler::audit::ACTION_REGISTRY_LOADED,
        payload,
    )
    .await
    .map(|_| ())
}

async fn write_l0_seeded_row(
    pool: &sqlx::PgPool,
    report: &hhagent_core::memory::l0_seed::L0SeedReport,
) -> Result<(), hhagent_db::DbError> {
    let payload = serde_json::json!({
        "rules_loaded": report.rules_loaded,
        "new_rows_written": report.new_rows_written,
        "unchanged_skipped": report.unchanged_skipped,
        "source_path": report.source_path.to_string_lossy(),
        "source_sha256": report.source_sha256,
        "entities_linked": report.entities_linked,
        "link_failures": report.link_failures,
    });
    hhagent_db::audit::insert(
        pool,
        "core",
        hhagent_core::scheduler::audit::ACTION_L0_SEEDED,
        payload,
    )
    .await
    .map(|_| ())
}

