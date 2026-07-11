use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tracing::info;

// Daemon bring-up + shutdown helpers and the Matrix channel bring-up live in
// sibling files under `main/` to keep this binary entrypoint under the 500-LOC
// cap (Item 9b). `#[path]` is required because `main.rs` is a crate root — a
// bare `mod bootstrap;` would resolve to `src/bootstrap.rs`, not `src/main/`.
#[path = "main/bootstrap.rs"]
mod bootstrap;
#[path = "main/matrix_boot.rs"]
mod matrix_boot;

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
        version = kastellan_core::VERSION,
        "kastellan core starting"
    );

    // Bring up the database before announcing readiness or accepting
    // any (future) work. Fail-closed: any error here propagates `?` to
    // a non-zero exit, the supervisor sees the failure, and the next
    // restart attempt re-runs the probe. Running degraded against a
    // half-bootstrapped database would silently lose audit-log rows
    // and corrupt memory writes — a much worse failure mode than a
    // restart loop, which at least surfaces in logs.
    let spec = bootstrap::bring_up_database().await?;

    // Open the daemon-scoped pool and start the audit-log JSONL
    // mirror task. The pool's `after_connect` hook drops privilege to
    // `kastellan_runtime` on every dialed connection (see
    // `db::pool` module docs); the mirror replicates committed
    // `audit_log` rows to `~/.local/state/kastellan/audit-*.jsonl` so
    // operators can `tail -f` without a DB client.
    //
    // Pool failures here are fatal (the dispatcher write site needs
    // them); mirror failures are NOT fatal — the mirror is an
    // operator-visibility layer, not a correctness requirement.
    let pool = kastellan_db::pool::connect_runtime_pool(&spec)
        .await
        .context("opening daemon-scoped Postgres pool")?;
    let mirror = bootstrap::start_audit_mirror(pool.clone()).await;

    // Crash sweep: any task left in 'running' from a previous daemon
    // instance whose lease has elapsed gets marked 'crashed'. Each
    // recovered task also gets one `scheduler/task.crashed` audit row
    // so observation-phase queries see the lifecycle transition.
    // Idempotent.
    match kastellan_core::scheduler::crash_recovery::sweep_and_audit(&pool).await {
        Ok(0) => {}
        Ok(n) => info!(crashed_tasks = n, "crash_recovery: swept tasks to 'crashed'"),
        Err(e) => tracing::warn!(error = %e, "crash_recovery::sweep_and_audit failed (non-fatal)"),
    }

    // LLM router (existing skeleton).
    let router_cfg = kastellan_llm_router::RouterConfig::from_env()
        .map_err(|e| anyhow!("RouterConfig::from_env: {e}"))?;
    let router = Arc::new(
        kastellan_llm_router::Router::new(router_cfg)
            .map_err(|e| anyhow!("Router::new: {e}"))?,
    );

    // Production review pipeline: stub stages in this scope (see spec
    // §6.1). Real implementations replace these structs in place.
    let review = Arc::new(
        kastellan_core::cassandra::review::ChainReviewStage::new(vec![
            Arc::new(kastellan_core::cassandra::review::ConstitutionalGuard),
            Arc::new(kastellan_core::cassandra::review::DeterministicPolicy),
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
    let sandboxes = Arc::new(kastellan_sandbox::SandboxBackends::default_for_current_os());

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
    // Directory of the running `kastellan` binary — seeds exe-relative sibling
    // discovery so plain workers (e.g. shell-exec) are found in a flat install
    // with no KASTELLAN_*_BIN env set. None (rare current_exe() failure) ⇒
    // override-env-only discovery.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    // Egress force-routing (slice #2 Task 4.4) — opt-in via
    // KASTELLAN_EGRESS_FORCE_ROUTING. Off ⇒ `None` ⇒ the lifecycle's spawn path
    // is byte-identical to the legacy direct-spawn behaviour (existing
    // deployments + the Mac e2e are unaffected). On ⇒ every `Net::Allowlist`
    // worker is force-routed through a per-worker egress-proxy sidecar. Built
    // here because it needs the runtime pool + handle (decision audit sink) and
    // exe_dir (proxy-binary discovery). Fail-closed: enabled-but-no-proxy-binary
    // returns Err and aborts startup rather than running net workers unrouted.
    let force_routing = kastellan_core::worker_lifecycle::force_route::from_env(
        pool.clone(),
        tokio::runtime::Handle::current(),
        exe_dir.as_deref(),
    )
    .context("building egress force-routing config")?;
    if let Some(fr) = force_routing.as_ref() {
        info!("egress force-routing ENABLED — Net::Allowlist workers route through the egress proxy");
        // Reclaim per-worker scratch dirs orphaned by a prior daemon that was
        // SIGKILLed before its RAII cleanup ran (#251). Best-effort: a leak,
        // never a safety issue — egress is gated by the OS netns/Seatbelt
        // barrier, not by scratch hygiene. Conservative (only sweeps dead,
        // non-self pids), so it is safe alongside a concurrent daemon.
        let swept = fr.sweep_stale_scratch_dirs();
        if swept > 0 {
            info!(dirs = swept, "egress: reclaimed stale per-worker scratch dirs from a prior daemon");
        }
    }

    // Embed-broker config (Slice B) — the trusted embedding-broker sidecar. No
    // daemon enable gate: the *manifest* opts a worker in (web-research's
    // KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER), so we always try to discover the
    // broker binary and hold a config iff it resolves. `None` (binary absent) is
    // fine unless a worker actually requests a broker — then the spawn chokepoint
    // fails closed. Built here for exe_dir (broker-binary discovery).
    let embed_broker_cfg = kastellan_core::embed_broker::config::from_env(exe_dir.as_deref());
    if embed_broker_cfg.is_some() {
        info!("embed-broker AVAILABLE — broker-declaring workers get a trusted embedding sidecar");
    }

    let lifecycle: Arc<dyn kastellan_core::worker_lifecycle::WorkerLifecycleManager> = Arc::new(
        kastellan_core::worker_lifecycle::CompositeLifecycle::with_backoff_and_force_routing(
            Arc::clone(&sandboxes),
            kastellan_core::worker_lifecycle::RestartBackoff::default(),
            // Cloned (cheap — `Option<Arc<_>>`): the matrix channel spawn
            // block below also needs the resolved config to build its own
            // `MatrixEgress`, after `force_routing` is moved in here.
            force_routing.clone(),
            embed_broker_cfg,
        ),
    );

    // Tool registry: each tool the scheduler may dispatch is opted in via its
    // WorkerManifest (see kastellan_core::registry_build::WORKER_MANIFESTS). The
    // registry is the host-side allowlist of *which* tools exist (separate from
    // the per-tool argv allowlist, which lives in the `tool_allowlists` DB
    // table). A worker whose binary/preconditions are absent is simply not
    // registered — `dispatch_step` then returns `UNKNOWN_TOOL`.
    let (registry, loaded_tool_records, _tool_docs) =
        kastellan_core::registry_build::build_tool_registry(&pool, exe_dir).await?;
    let tool_registry = Arc::new(registry);
    // Best-effort audit row (was previously written inside build_tool_registry;
    // moved here now that the builder is side-effect-free).
    if let Err(e) = bootstrap::write_registry_loaded_row(&pool, &loaded_tool_records).await {
        tracing::warn!(error = %e, "registry.loaded audit row insert failed");
    }

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
        // The return value is the (image_tag, probe_result) list, kept on
        // the function signature so integration tests can assert on probe
        // outcomes directly. Production daemon doesn't need it — the
        // side-effect contract is the tracing::info!/warn! line per tag
        // emitted from inside the function. Discard explicitly.
        let _probe_results = kastellan_core::sandbox_health::probe_registered_container_images(
            tool_registry.entries(),
        );
    }

    // Entity extractor (v2). When gliner-relex is configured, builds a
    // typed Client over the shared lifecycle Arc + worker manifest and
    // returns GlinerRelexExtractor. When the worker isn't configured
    // (KASTELLAN_GLINER_RELEX_ENABLE=0 or preconditions failed), falls
    // back to NoOpEntityExtractor — daemon stays up; graph lane stays
    // empty. Reads the resolved entry back from the registry — single
    // resolution, registry as source of truth.
    // Shared embedder for every forward embed path: L1 promotion (via the
    // scheduler) AND entity embed-on-insert (via the extractor below). Built
    // once from the same pool + router so backfilled, L1, and entity vectors
    // are all byte-identical.
    let embedder: std::sync::Arc<dyn kastellan_core::memory::Embedder> =
        std::sync::Arc::new(kastellan_core::memory::RouterEmbedder::new(
            pool.clone(),
            router.clone(),
        ));

    let entity_extractor: Arc<dyn kastellan_core::entity_extraction::EntityExtractor> =
        match tool_registry
            .lookup(kastellan_core::workers::gliner_relex::Client::TOOL_NAME)
            .cloned()
        {
            Some(entry) => {
                tracing::info!(
                    target: "kastellan::main",
                    "gliner-relex configured; constructing v2 entity extractor",
                );
                let client = kastellan_core::workers::gliner_relex::Client::new(
                    lifecycle.clone(),
                    pool.clone(),
                    entry,
                );
                Arc::new(
                    kastellan_core::entity_extraction::gliner_relex::GlinerRelexExtractor::new(
                        client,
                        pool.clone(),
                        embedder.clone(),
                    ),
                )
            }
            None => {
                // WARN level per the v2 design spec's failure-mode
                // matrix ("KASTELLAN_GLINER_RELEX_ENABLE=0 (default) or
                // weights missing | Daemon starts; one WARN line at
                // startup"). The resolver's own info!/error! line was
                // already emitted; this is the wiring-outcome breadcrumb.
                tracing::warn!(
                    target: "kastellan::main",
                    "gliner-relex not configured; using NoOpEntityExtractor (graph lane disabled)",
                );
                Arc::new(kastellan_core::entity_extraction::NoOpEntityExtractor::new())
            }
        };

    // Load every prompts/*.md, hash, upsert into agent_prompts.
    let prompts_dir = std::env::var("KASTELLAN_PROMPTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("prompts"));
    let prompts = kastellan_core::scheduler::prompts::load_prompts_from_dir(&pool, &prompts_dir)
        .await
        .with_context(|| format!("loading prompts from {:?}", prompts_dir))?;

    // Seed L0 (meta-rule) rows from the operator-edited TOML file.
    // Default: `seeds/memory/l0_meta_rules.toml` relative to CWD.
    // Override: `KASTELLAN_L0_RULES_FILE` env var. Missing file is
    // logged at info level and skipped (daemon still comes up).
    // Malformed file is fatal (loader returns Err, ? propagates) —
    // matches probe::run fail-closed posture.
    let l0_path = std::env::var("KASTELLAN_L0_RULES_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("seeds/memory/l0_meta_rules.toml"));
    if l0_path.exists() {
        let report = kastellan_core::memory::l0_seed::seed_l0_from_file(
            &pool, &*entity_extractor, &l0_path,
        )
        .await
        .with_context(|| format!("seeding L0 rules from {:?}", l0_path))?;
        // Best-effort audit row: a transient DB failure here must not
        // block daemon bring-up. The L0 rows themselves are already
        // committed; mirrors `write_registry_loaded_row` posture.
        if let Err(e) = bootstrap::write_l0_seeded_row(&pool, &report).await {
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
    let formulator: Arc<dyn kastellan_core::scheduler::agent::PlanFormulator> =
        Arc::new(kastellan_core::scheduler::agent::RouterAgent::new(
            router.clone(),
            prompts.clone(),
            Arc::new(kastellan_core::prompt_assembly::PgSystemPromptBuilder::new(pool.clone())),
            Arc::new(kastellan_core::recall_assembly::PgRecallBuilder::new(
                pool.clone(),
                router.clone(),
            )),
            entity_extractor.clone(),
        ));

    // ── Bootstrap secret materialization vault (Item 31, slice 1). ──
    //
    // KASTELLAN_BOOTSTRAP_SECRETS = "name1,name2,name3" — comma-separated
    // names that must each exist in the `secrets` table. Missing names
    // fail bring-up (fail-closed: a configured-but-missing secret is
    // operator error). The ref string itself is NOT logged — only the
    // ref_hash. Test fixtures reconstruct refs via their own
    // Vault::materialize calls.
    let vault = std::sync::Arc::new(kastellan_core::secrets::Vault::new());
    if let Ok(names_csv) = std::env::var("KASTELLAN_BOOTSTRAP_SECRETS") {
        let names = bootstrap::parse_bootstrap_secrets_csv(&names_csv);
        if !names.is_empty() {
            let key_provider = kastellan_db::secrets::OsKeyringProvider::ensure_initialized()
                .context("KASTELLAN_BOOTSTRAP_SECRETS: failed to initialize OS keyring provider")?;
            for name in names {
                let secret_ref = vault
                    .materialize(&pool, &key_provider, name, "core:bootstrap")
                    .await
                    .with_context(|| format!("KASTELLAN_BOOTSTRAP_SECRETS: materialize({name:?}) failed"))?;
                tracing::info!(
                    name = %name,
                    ref_hash = %secret_ref.ref_hash(),
                    "secret materialized at bootstrap"
                );
            }
        }
    }

    // ── TEST-ONLY Vault seed seam (#298). ──
    //
    // `KASTELLAN_TEST_VAULT_SEED = "<8hex>=<plaintext>"` binds a caller-known
    // `secret://<8hex>` ref to `<plaintext>` so a separate-process e2e (the CLI
    // in `cli_memory_l3py_run_daemon_e2e`) can pass that ref as a `params` value
    // and assert the output scrub. Neither the ref nor the plaintext is logged.
    //
    // This whole block is `#[cfg(debug_assertions)]`-gated, so it is PHYSICALLY
    // ABSENT from a release build (`cargo build --release` disables
    // `debug_assertions`; the deployed daemon is built that way — see
    // `scripts/build-release.sh`). A production binary has no code path that can
    // read this env var or bind a caller-known plaintext to a known ref.
    #[cfg(debug_assertions)]
    if let Ok(spec) = std::env::var("KASTELLAN_TEST_VAULT_SEED") {
        if let Some((ref_hex, plaintext)) = bootstrap::parse_test_vault_seed(&spec) {
            vault
                .seed_known_ref_for_test(ref_hex, plaintext.as_bytes())
                .context("KASTELLAN_TEST_VAULT_SEED: seed_known_ref_for_test failed")?;
        }
    }

    let handoff_cache = std::sync::Arc::new(kastellan_core::handoff::HandoffCache::new());
    let dispatcher: Arc<dyn kastellan_core::scheduler::inner_loop::StepDispatcher> =
        Arc::new(
            kastellan_core::scheduler::tool_dispatch::ToolHostStepDispatcher::new(
                pool.clone(),
                vault.clone(),
                lifecycle,
                tool_registry,
                handoff_cache,
            ),
        );

    let scheduler = kastellan_core::scheduler::spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        entity_extractor.clone(),
        embedder,
    );
    info!("scheduler spawned (lane_fast + lane_long)");

    // ── Channel bus (comms slice #2 — Matrix). ──
    // Gated on KASTELLAN_MATRIX_HOMESERVER_URL (checked inside): unset ⇒ `None`,
    // and the daemon is byte-identical to a Matrix-less build. When set, spawns
    // the sandboxed live worker and runs a ChannelBus over the DB-backed
    // pairing/authorizer + the tasks-queue event/completion seams. Fail-soft:
    // any spawn/login/connect failure logs and yields `None`. See
    // `main/matrix_boot.rs`.
    let matrix_bus = matrix_boot::spawn_matrix_channel(&pool, &sandboxes, &force_routing).await;

    bootstrap::wait_for_shutdown().await?;

    // Stop the channel bus first so no further inbound messages are enqueued and
    // the worker's stdin closes (clean worker exit).
    if let Some(bus) = matrix_bus {
        bus.shutdown().await;
    }

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

    info!("kastellan core shutting down");
    Ok(())
}
