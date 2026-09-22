use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tracing::{info, warn};

// Daemon bring-up + shutdown helpers and the Matrix channel bring-up live in
// sibling files under `main/` to keep this binary entrypoint under the 500-LOC
// cap (Item 9b). `#[path]` is required because `main.rs` is a crate root — a
// bare `mod bootstrap;` would resolve to `src/bootstrap.rs`, not `src/main/`.
#[path = "main/bootstrap.rs"]
mod bootstrap;
#[path = "main/email_boot.rs"]
mod email_boot;
#[path = "main/matrix_boot.rs"]
mod matrix_boot;

/// #388.1: install-dir trust probe. Defence-in-depth backstop for the
/// documented "install dir must not be user-writable" deploy assumption: warn
/// (or, with `KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR` set, fail closed) when the
/// directory workers are discovered from is writable by a principal other than
/// root or the daemon's own euid. The normal per-user install
/// (`~/.local/lib/kastellan`, self-owned 0755) passes silently.
#[cfg(unix)]
fn probe_install_dir_trust(exe_dir: Option<&std::path::Path>) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Some(dir) = exe_dir else { return Ok(()) };
    let meta = match std::fs::metadata(dir) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e,
                "could not stat install dir for the trust probe (continuing)");
            return Ok(());
        }
    };
    let facts = kastellan_core::worker_manifest::InstallDirFacts {
        owner_uid: meta.uid(),
        mode: meta.mode(),
    };
    // SAFETY: geteuid() has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if let kastellan_core::worker_manifest::InstallDirTrust::Untrusted { reason } =
        kastellan_core::worker_manifest::assess_install_dir(euid, &facts)
    {
        let strict = kastellan_core::worker_lifecycle::force_route::env_flag_enabled(
            std::env::var("KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR").ok(),
        );
        if strict {
            anyhow::bail!(
                "install dir {} is untrusted ({reason}); refusing to start because \
                 KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR is set",
                dir.display()
            );
        }
        tracing::error!(
            dir = %dir.display(), reason = %reason,
            "install dir is writable by a principal other than root/self; a malicious \
             sibling worker binary could be registered on restart. Set \
             KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR=1 to fail closed."
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn probe_install_dir_trust(_exe_dir: Option<&std::path::Path>) -> Result<()> {
    Ok(())
}

/// Report the guard tier once, at boot, and record a queryable audit row.
///
/// Slice-1's D1 requires the tier's state be reported "once at boot, loudly,
/// not per-call — a per-call warning on the dispatcher hot path is its own
/// denial of service". This is that report, and it deliberately extends the
/// existing "log what was actually resolved, once" block rather than inventing
/// a second pattern.
///
/// **A `Clamped::ToCeiling` timeout is a `warn!`, not an `info!`.** It is the
/// one basis that reports a reduction in coverage: on that host, documents
/// large enough to matter will time out and fail open to catalogue-only
/// screening. Everything else is routine, and warning about routine things is
/// how the one that matters gets scrolled past.
async fn report_guard_tier(
    tier: Option<&kastellan_core::cassandra::guard_model::GuardTier>,
    cfg: &kastellan_llm_router::RouterConfig,
    pool: &sqlx::PgPool,
) {
    use kastellan_core::cassandra::guard_model::boot_report::{
        boot_payload, not_configured_payload, timeout_ms, ReportedRates,
    };

    // One helper for both arms: the row is non-fatal by design — a guard
    // tier that boots correctly must not be prevented from running by an
    // audit sink that cannot take its boot row.
    //
    // Non-fatal, but not silent, and the difference is the whole point of
    // the row. `audit_log` is the ONLY durable record of what the guard
    // tier's timeout basis was at this boot; tracing logs rotate and the
    // `warn!` below fires only when there is a coverage finding. So on
    // the failure path the payload itself goes into the log line — it is
    // bounded well under a kilobyte by construction (closed-set
    // `&'static str` findings, a 16-char digest, and scalars) — and at
    // `error!`, matching `secret_scrub.rs` and `main/email_boot.rs`
    // rather than the best-effort `cli_audit/*` rows. A lost boot row is
    // not best-effort bookkeeping: nothing anywhere in the tree reads a
    // MISSING `guard_tier.boot` row as a signal, so without this line the
    // loss is terminally silent.
    async fn record(pool: &sqlx::PgPool, payload: serde_json::Value) {
        // Cloned before the move so the failure arm can report WHICH row
        // was lost. One `configured` flag distinguishes the two arms; the
        // full payload makes the loss recoverable by hand.
        let echo = payload.clone();
        if let Err(e) = kastellan_db::audit::insert(pool, "policy", "guard_tier.boot", payload).await
        {
            tracing::error!(
                error = %e,
                configured = %echo["configured"],
                timeout_basis = %echo["timeout_basis"],
                payload = %echo,
                "guard_tier.boot audit insert failed (non-fatal; the tier still runs, but \
                 this boot's timeout provenance is now unrecorded)"
            );
        }
    }

    let Some(tier) = tier else {
        info!(
            "guard tier NOT configured -- tool output is screened by the deterministic \
             catalogue only. Set KASTELLAN_LLM_GUARD_URL, KASTELLAN_LLM_GUARD_MODEL and \
             KASTELLAN_LLM_GUARD_TAU to enable it."
        );
        record(pool, not_configured_payload()).await;
        return;
    };

    let budget = tier.timeout();
    // What #627 actually moved, stated precisely because the obvious
    // stronger claim ("every number here now comes from one place") is
    // false and would mislead the next reader.
    //
    // `boot_report` owns the two things that were DECIDED here and are
    // not decided here any more: the millisecond derivation of the budget
    // (`timeout_ms`, which the log sites and the payload each used to
    // spell with their own `as_millis() as u64`) and the rate assignment
    // (`BootRates::from_basis`, the transposition #627 is about).
    //
    // The rest are read twice and agree by coincidence, not by
    // construction: `kind()` below and again inside `boot_payload`;
    // `tier.tau()` / `tier.n_ctx()` in the `info!` line and again as
    // `boot_payload`'s arguments. That is tolerable because each is a
    // single accessor call with no arithmetic — unlike the two above,
    // there is no second SPELLING that could drift — but it is not the
    // same guarantee, and calling it one is how a later session stops
    // checking.
    let budget_ms = timeout_ms(budget);
    let basis = budget.basis.kind();
    // ONE mapping into the reporting vocabulary, shared with the durable
    // row below (#643). Both tracing lines used to rename the fields
    // themselves — two untested copies of a transposition the payload
    // version was guarded against.
    let reported = ReportedRates::from_basis(&budget.basis);

    info!(
        url = %cfg.guard_url.as_deref().unwrap_or("<unset>"),
        model = %cfg.guard_model.as_deref().unwrap_or("<unset>"),
        tau = tier.tau(),
        timeout_ms = budget_ms,
        timeout_basis = basis,
        // The reporting vocabulary is frozen at `tok_per_s` even though
        // the struct field is `fastest_tok_per_s`: the durable
        // `guard_tier.boot` key cannot move (live rows carry it, and the
        // operator query `slowest_tok_per_s < tok_per_s / 2` is written
        // against it), and a log line naming this number differently
        // from the audit row it accompanies would read as a second
        // measurement. Since #643 the rename happens once, in
        // `ReportedRates`, so each line below is name-for-name identity
        // and a swap is visible on its face.
        tok_per_s = reported.tok_per_s,
        slowest_tok_per_s = reported.slowest_tok_per_s,
        measured_samples = reported.measured_samples,
        attempted_samples = reported.attempted_samples,
        n_ctx = tier.n_ctx(),
        policy_digest = %kastellan_core::cassandra::guard_model::policy::policy_digest(),
        "guard tier configured -- ADVISORY defence-in-depth, not a gate (65% recall at \
         the fitted tau; weakest against narrative indirect injection). Nothing \
         downstream may relax on it."
    );

    // The finding TEXT comes from the basis, because the FIVE bases that
    // qualify are five different findings — a ceiling clamp, a probe that
    // never returned, a probe that FAILED (which predicts a tier that
    // fails open on every dispatch, and used to be reported at `info!`),
    // and an operator pin below the floor or above the ceiling (#615).
    // The last two are findings about the CONFIGURATION rather than the
    // host, and they come through this same channel deliberately: an
    // operator reading a boot line wants one place that says "this
    // deployment screens less than it looks like it does".
    // (`basis.rs` said "three" here too until #619's review; keeping the
    // count in two places is what let them drift, so if a sixth lands,
    // grep for the number.)
    if let Some(finding) = budget.basis.coverage_finding() {
        tracing::warn!(
            timeout_ms = budget_ms,
            timeout_basis = basis,
            // No `unwrap_or(0.0)` anywhere in `BootRates`: a fabricated
            // zero would be logged as if it were measured. Only a real
            // `Probed` basis carries a rate.
            //
            // The spread and the counts belong HERE too, not only in the
            // durable row below. For a ceiling finding these are the
            // diagnostic that separates "slow host" from "busy boot" --
            // which is the whole of #624 -- and this `warn!` is the line
            // an operator actually reads.
            tok_per_s = reported.tok_per_s,
            slowest_tok_per_s = reported.slowest_tok_per_s,
            measured_samples = reported.measured_samples,
            attempted_samples = reported.attempted_samples,
            "{finding}"
        );
    }

    record(pool, boot_payload(tier.tau(), tier.n_ctx(), budget)).await;
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // ⚠️ The `fmt` layer DISCARDS its writer's `Result` unless this is set.
        // Without it a full disk or a restarted journal makes every worker
        // death, channel-down and retirement report vanish with nothing said —
        // and the delivery check (#734) cannot see it, because the event WAS
        // enabled; it is the write that failed. That is the one remaining way
        // for a report to reach nobody silently, and this is the only channel
        // left that can complain about it.
        .log_internal_errors(true)
        .json()
        .init();

    info!(
        version = kastellan_core::VERSION,
        "kastellan core starting"
    );

    // Confirm the operator overlay actually took effect (#531). Logged ahead of
    // every tuned setting the daemon reads EXCEPT the log filter itself, which
    // is necessarily read above to build the subscriber that emits this line —
    // so `RUST_LOG` in the overlay is the one setting consumed before its own
    // confirmation. Do not "fix" that by moving this call up: there would be no
    // subscriber yet and the output would vanish.
    bootstrap::report_operator_overlay();

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

    // Overdue operator asks from a previous daemon life (#564 slice 1b).
    // The periodic sweep inside `spawn_scheduler` covers the running
    // daemon; this one covers the gap across a restart, so a task does not
    // wait a full interval to learn its ask timed out days ago.
    // Non-fatal for the same reason the crash sweep above is: a degraded
    // audit story is better than refusing to start.
    match kastellan_core::scheduler::asks::sweep_expired_and_audit(&pool).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(count = n, "expired overdue operator asks at startup"),
        Err(e) => tracing::warn!(error = %e, "asks::sweep_expired_and_audit failed (non-fatal)"),
    }

    // LLM router (existing skeleton).
    let router_cfg = kastellan_llm_router::RouterConfig::from_env()
        .map_err(|e| anyhow!("RouterConfig::from_env: {e}"))?;
    // Log what was actually resolved, once. A misconfigured endpoint surfaces
    // much later as a bare `Transport("error sending request")` at the first
    // plan, and the single most useful fact then is what the daemon was
    // dialling — which is otherwise nowhere in the logs. Values only, no
    // secrets: the frontier API key is fetched at dispatch time, not here.
    info!(
        local_url = %router_cfg.local_url,
        local_model = %router_cfg.local_model,
        embedding_url = %router_cfg.embedding_url,
        embedding_model = %router_cfg.embedding_model,
        timeout_ms = router_cfg.timeout.as_millis() as u64,
        disable_thinking = router_cfg.disable_thinking,
        "llm router configured"
    );

    // ── The Shieldstral guard tier (wiring slice). ──
    //
    // Built and reported ONCE, here, beside the router it shares an endpoint
    // seam with. FIVE things can stop the daemon, and all five are the same
    // failure wearing different clothes — a security control that is off while
    // looking configured (D6):
    //
    //   * a half-configured tier (URL without model, or either without a tau);
    //   * a tau outside (0.0, 1.0], both ends of which are silent failures;
    //   * an operator-pinned KASTELLAN_LLM_GUARD_TIMEOUT_MS of 0, which would
    //     time out every adjudication and so disable the tier while it logged
    //     as configured;
    //   * a backend whose /props is UNREACHABLE — so if the guard server is
    //     not running, the daemon does not start. This is the most likely boot
    //     failure on a host with the tier configured and the one an operator
    //     most needs to see named here;
    //   * a backend whose context cannot hold a worst-case document (#604),
    //     which would otherwise fail OPEN at runtime on exactly the dense
    //     adversarial documents the tier exists for.
    //
    // The counter-argument — a down daemon protects nothing — was weighed and
    // rejected: "loud error at boot" is precisely what gets scrolled past.
    //
    // A SIXTH, opt-in: KASTELLAN_REQUIRE_GUARD=1 makes an *unconfigured* tier
    // fatal too. That door needs its own flag because losing all three guard
    // keys at once — which is what `install` regenerating `kastellan.env`
    // actually does — lands on the deliberate-opt-out arm, not on any of the
    // five above.
    //
    // The throughput probe underneath this is deliberately NOT fatal: it picks
    // a timeout, it does not verify a control.

    // A pre-epoch clock would make the cache-buster constant across boots, which silently
    // disables the ONLY defence against M2's 4x cache over-estimate on a backend
    // that caches without reporting `cached_tokens` (#608). Unlikely, but a
    // silent fallback here is invisible in exactly the way that measurement is.
    let probe_nanos = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "system clock is before the epoch; the guard boot probe's cache-buster \
                 will not vary between boots and its throughput sample may be a cache hit"
            );
            0
        }
    };
    let guard_probe_cache_buster = format!("kastellan-guard-probe-{probe_nanos}");
    let guard_tier = kastellan_core::cassandra::guard_model::GuardTier::from_router_config(
        &router_cfg,
        &guard_probe_cache_buster,
    )
    .await
    .map_err(|e| anyhow!("guard tier: {e}"))?
    .map(std::sync::Arc::new);
    kastellan_core::cassandra::guard_model::tier::boot::require_tier(
        guard_tier.as_deref(),
        kastellan_core::worker_lifecycle::force_route::env_flag_enabled(
            std::env::var("KASTELLAN_REQUIRE_GUARD").ok(),
        ),
    )
    .map_err(|e| anyhow!("guard tier: {e}"))?;
    report_guard_tier(guard_tier.as_deref(), &router_cfg, &pool).await;

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

    // #388.1: probe the install dir before wiring worker discovery off it.
    probe_install_dir_trust(exe_dir.as_deref())?;

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
    } else {
        use kastellan_core::egress::force_routing_notice as frn;
        // `observed` names what the daemon actually read: every spelling other
        // than 1/true/yes/on is off, so an operator who set `=enabled` and then
        // read DISABLED would otherwise conclude the line was stale rather than
        // that their value was rejected.
        warn!(
            env_var = frn::ENV_VAR,
            observed = ?std::env::var(frn::ENV_VAR).ok(),
            "{}",
            frn::force_routing_disabled_message()
        );
        // Best-effort: the posture belongs in the oversight record, not only in
        // a plaintext log with no role gating. A failed insert must not stop a
        // daemon that is otherwise healthy.
        if let Err(e) = kastellan_db::audit::insert(
            &pool,
            frn::DAEMON_ACTOR,
            frn::ACTION_FORCE_ROUTING_DISABLED,
            frn::force_routing_disabled_payload(),
        )
        .await
        {
            warn!(error = %e, "could not audit the disabled force-routing posture");
        }
    }

    // Broker configs (unified): discover each kind's sidecar binary. No daemon
    // enable gate — a manifest opts a worker in; the daemon holds a config iff the
    // binary resolves, and the spawn chokepoint fails closed if a declaring worker
    // has none.
    let broker_configs = kastellan_core::broker::BrokerConfigs {
        embed: kastellan_core::broker::config::from_env(
            kastellan_core::broker::BrokerKind::Embed, exe_dir.as_deref()),
        search: kastellan_core::broker::config::from_env(
            kastellan_core::broker::BrokerKind::Search, exe_dir.as_deref()),
    };
    if broker_configs.embed.is_some() {
        info!("embed-broker AVAILABLE — embed-declaring workers get a trusted embedding sidecar");
    }
    if broker_configs.search.is_some() {
        info!("search-broker AVAILABLE — search-declaring workers get a trusted search sidecar");
    }

    let lifecycle: Arc<dyn kastellan_core::worker_lifecycle::WorkerLifecycleManager> = Arc::new(
        kastellan_core::worker_lifecycle::CompositeLifecycle::with_backoff_and_force_routing(
            Arc::clone(&sandboxes),
            kastellan_core::worker_lifecycle::RestartBackoff::default(),
            // Cloned (cheap — `Option<Arc<_>>`): the matrix channel spawn
            // block below also needs the resolved config to build its own
            // `MatrixEgress`, after `force_routing` is moved in here.
            force_routing.clone(),
            broker_configs,
        ),
    );

    // Tool registry: each tool the scheduler may dispatch is opted in via its
    // WorkerManifest (see kastellan_core::registry_build::WORKER_MANIFESTS). The
    // registry is the host-side allowlist of *which* tools exist (separate from
    // the per-tool argv allowlist, which lives in the `tool_allowlists` DB
    // table). A worker whose binary/preconditions are absent is simply not
    // registered — `dispatch_step` then returns `UNKNOWN_TOOL`.
    let (registry, loaded_tool_records, tool_docs) =
        kastellan_core::registry_build::build_tool_registry(&pool, exe_dir).await?;
    let tool_docs: std::sync::Arc<[kastellan_core::prompt_assembly::AdvertisedTool]> =
        std::sync::Arc::from(tool_docs);
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

    // Planner "now" timezone — feeds the trusted <now> block so date-relative
    // questions stop web-searching for the current date (the root cause of the
    // plan_iteration_cap loop). KASTELLAN_TIMEZONE = IANA name; unset → host
    // system tz; invalid → UTC (fail-safe).
    let (planner_tz, tz_source) = kastellan_core::prompt_assembly::resolve_timezone(
        std::env::var("KASTELLAN_TIMEZONE").ok().as_deref(),
    );
    // Log the EFFECTIVE zone, not just the source: `TzSource::System` can
    // silently degrade to UTC when jiff can't resolve the host zone, and an
    // IANA-less fixed-offset zone yields `None`. Surfacing the resolved name
    // makes "System" vs. a silent UTC fallback distinguishable in the log.
    info!(
        ?tz_source,
        zone = planner_tz.iana_name().unwrap_or("<fixed-offset>"),
        "planner <now> timezone resolved"
    );

    // PlanFormulator — takes the extractor as 5th arg (Task 14 widened
    // the signature; Task 15 supplies the constructed extractor).
    let formulator: Arc<dyn kastellan_core::scheduler::agent::PlanFormulator> =
        Arc::new(kastellan_core::scheduler::agent::RouterAgent::new(
            router.clone(),
            prompts.clone(),
            Arc::new(
                kastellan_core::prompt_assembly::PgSystemPromptBuilder::new(pool.clone())
                    .with_tool_docs(tool_docs.clone())
                    .with_timezone(planner_tz),
            ),
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
                guard_tier,
            ),
        );

    // The core-initiated-outbound registry, created HERE because both sides
    // need it and neither can own it: the scheduler is spawned on the next
    // line, the channel supervisors below it, and each supervisor restarts
    // its bus underneath. See `channel::outbox`.
    let outbox = std::sync::Arc::new(kastellan_core::channel::outbox::ChannelOutbox::new());

    let scheduler = kastellan_core::scheduler::spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        entity_extractor.clone(),
        embedder,
        Some(outbox.clone()),
    );
    info!("scheduler spawned (lane_fast + lane_long)");

    // ── Channel bus (comms slice #2 — Matrix). ──
    // Gated on KASTELLAN_MATRIX_HOMESERVER_URL (checked inside): unset ⇒ the
    // supervisor task returns immediately and the daemon is byte-identical to
    // a Matrix-less build. When set, it spawns the sandboxed live worker and
    // runs a ChannelBus over the DB-backed pairing/authorizer + the tasks-queue
    // event/completion seams — retrying with capped backoff until that
    // succeeds (#514), so a transient failure in the startup window no longer
    // leaves the bot deaf for the life of the process. A statically-dead
    // homeserver still stops, loudly. Returns without awaiting: bring-up now
    // proceeds in the background rather than holding startup for up to the
    // 60s login timeout. See `main/matrix_boot.rs`.
    let matrix =
        matrix_boot::supervise_matrix_channel(&pool, &sandboxes, &force_routing, outbox.clone());

    // ── Channel bus (Phase 2 slice #5 — email fallback). ──
    // Gated on KASTELLAN_EMAIL_ENDPOINT (checked inside): unset ⇒ the daemon is
    // byte-identical to an email-less build. Same supervision as Matrix, with
    // one classification difference: a set-but-PARTIAL config is FATAL (the
    // environment cannot change under a running daemon, so retrying would spin
    // while telling the operator to restart), whereas a worker spawn failure
    // is retried. Deliberately never an abort: this is the FALLBACK channel
    // (it exists because Matrix has no homeserver failover), so a typo in its
    // config must not take Matrix, the scheduler, and the graceful-shutdown
    // path below down with it. Nothing on that path returns `Result`, so no
    // future `?` can reinstate the abort. See `main/email_boot.rs`.
    let email =
        email_boot::supervise_email_channel(&pool, &sandboxes, &force_routing, outbox.clone());

    bootstrap::wait_for_shutdown().await?;

    // Stop the channel supervisors first: each stops its bus if it started one,
    // so no further inbound messages are enqueued and each worker's stdin
    // closes (clean worker exit). Unconditional — a supervisor that never
    // started a channel, or is still retrying, shuts down to a no-op.
    matrix.shutdown().await;
    email.shutdown().await;

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
