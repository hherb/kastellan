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
    /// The operator overlay. **Never written by the installer** — it exists so
    /// hand-tuned settings survive the `kastellan.env` regeneration that
    /// otherwise drops them on every deploy (#458). Listed after `env_file` on
    /// the service spec, so its values win.
    pub env_local_file: PathBuf,
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
        env_local_file: config_dir.join("kastellan.env.local"),
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

/// Render the commented `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` help block (#492).
///
/// Split out as a pure function so the operator-facing wording — in particular
/// the two traps below — is unit-testable and cannot silently drift from the
/// rules `crate::egress::upstream_ca` enforces.
///
/// The two traps are the whole reason this block is more than one line:
///
/// 1. **`CA:TRUE` self-signed leaf.** `openssl req -x509` (the command everyone
///    reaches for) produces a self-signed certificate marked
///    `basicConstraints CA:TRUE`. If the origin then serves that same
///    certificate as its leaf, the proxy's rustls upstream validator rejects it
///    with `CaUsedAsEndEntity` — *even though `openssl verify` accepts it*. The
///    failure appears late, as a `mitm_failed` egress decision, not at startup.
/// 2. **Trust scope.** The anchor goes into that sidecar's whole upstream root
///    store, so it is trusted for every host that sidecar can reach. The daemon
///    therefore refuses to hand it to any worker that can reach more than the
///    one private origin — and, because keying is per-host, the operator has to
///    be told that co-located services on one address share the anchor.
pub fn render_upstream_ca_help() -> String {
    // One raw literal rather than a push_str chain: the block is prose, and it
    // is much easier to re-wrap and diff when it looks like what it renders.
    // The `help_block_is_entirely_commented_out` test guards the leading `#`s.
    r#"# Extra TLS trust anchor for a force-routed worker whose origin is PRIVATE and
# self-signed (e.g. a personal localmail on your LAN). JSON: origin -> PEM path.
# Unset (the default) = the egress proxy trusts only the public webpki roots.
# Only read when force-routing is enabled; on a host without it, this is inert.
# Rules, all enforced fail-closed at daemon startup or spawn:
#   * The origin MUST be a private/loopback IP LITERAL with NO port (10.x,
#     192.168.x, 127.0.0.1, fd00::/8 ...). A hostname is refused: the proxy's SSRF
#     guard blocks names that resolve into private ranges anyway, so it would be
#     unreachable regardless. A bad origin fails the daemon at startup.
#   * The PEM path must be ABSOLUTE and readable by the daemon.
#   * The worker's egress allowlist must contain ONLY that origin. The anchor is
#     trusted for every host that worker's sidecar can reach, so mixing a private
#     origin with a public one would let this CA impersonate the public host.
#   * CAVEAT: keying is per-HOST, not per-service. Every service sharing this
#     address is one origin to the rule above, so a second worker allowlisted to
#     e.g. 127.0.0.1:8888 also receives this anchor and trusts it for that port.
#     Give co-located private services distinct addresses if that matters.
#   * TRAP: the origin must serve a leaf certificate signed BY this CA, or a
#     self-signed leaf marked `basicConstraints CA:FALSE`. A self-signed cert marked
#     CA:TRUE and served as its own leaf is REJECTED at handshake time (rustls
#     `CaUsedAsEndEntity`) even though `openssl verify` accepts it — and
#     `openssl req -x509` produces exactly that shape by default. Check with:
#       openssl x509 -in <cert.pem> -noout -text | grep -A1 'Basic Constraints'
# KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA={"10.0.0.3":"/home/me/.config/localmail/tls/cert.pem"}
"#
    .to_string()
}

/// Render the commented email-fallback-channel help block (Phase 2 slice #5).
///
/// Purely informational — none of the five `KASTELLAN_EMAIL_*` vars is set by
/// this slice's install flow (no `--email-*` flags), so unlike the Matrix
/// block below this one is unconditional and entirely commented out, same
/// posture as [`render_upstream_ca_help`]. Split out as its own pure function
/// for the identical reason: the three operator traps below are
/// unit-tested wording, not free-floating prose that can silently drift from
/// what `channel::email::gate` and `channel::email::config` actually enforce.
///
/// The three traps are the whole reason this is more than one line:
///
/// 1. **authserv-id must match exactly.** `gate::trusted_dmarc_pass` only
///    trusts the TOPMOST `Authentication-Results` header, and only when it
///    names this exact id — anyone can forge their own such header in a
///    message they send, so a wrong id here fails every message closed.
/// 2. **Pairing is operator-only.** There is no in-channel pairing over
///    email by design (`kastellan-cli pair issue-token`) — see
///    `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`'s
///    D8: an unpaired sender's `Rejected` outcome deliberately skips the
///    carve-out for any transport that supplies evidence.
/// 3. **A self-signed localmail needs `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA`.**
///    This channel's force-routed sidecar terminates the worker's TLS and
///    re-originates upstream, so the operator anchor named for this origin is
///    what lets it validate a self-signed cert. Two constraints come with it,
///    both inherited from #492: the cert must be a real CA that signed the
///    origin leaf **or** a self-signed leaf with `basicConstraints CA:FALSE`
///    (a `CA:TRUE` self-signed leaf is rejected at handshake time by rustls as
///    `CaUsedAsEndEntity`, even though `openssl verify` accepts it — and
///    `openssl req -x509` commonly produces exactly that shape); and the
///    anchor is trusted for every host that sidecar can reach, so this
///    worker's allowlist must resolve to that single private origin. Verify a
///    cert's shape with:
///    `openssl x509 -in <cert.pem> -noout -text | grep -A1 'Basic Constraints'`
///    **Also:** `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` is ONE global map, keyed
///    by host only (not host:port — see [`render_upstream_ca_help`]'s
///    CAVEAT). An entry configured for this channel's origin is handed to
///    EVERY worker whose allowlist resolves to that same address, including
///    the separate `kastellan-worker-mail` TOOL if it points at the same
///    host. Two services sharing an address (e.g. localmail on `:8443` and a
///    search service on `:8888`, both `10.0.0.3`) cannot be distinguished by
///    this map: there is no log, no error, just a silently widened upstream
///    trust for whichever other worker resolves there. Don't conflate this
///    channel with the `kastellan-worker-mail` tool when reasoning about who
///    an anchor reaches.
pub fn render_email_help() -> String {
    r#"# --- Email fallback channel (Phase 2 slice #5) -------------------------------
# Inbound only in this slice: the agent can receive and act on email, but
# replies still go out over Matrix until slice 2 ships the SMTP worker.
#
# All five are required together once you set the first one. A partial config
# does NOT silently skip the channel and does NOT take the daemon down: the
# daemon logs a loud "@@CHANNEL_DISABLED@@" error (on a line carrying
# channel="email") naming every missing variable, then comes up with Matrix and
# the scheduler running and the email channel OFF. That one is NOT retried —
# the process environment cannot change under a running daemon — so fix the
# config and restart. Grep the startup log for that phrase before assuming the
# channel is live. (This is the fallback channel; a typo in it must never
# remove the primary one.)
#
# A TRANSIENT failure is handled differently since #514: a worker-spawn or
# LISTEN/NOTIFY failure is RETRIED with capped backoff (1s -> x2 -> 60s cap)
# until the channel comes up, so a blip during startup no longer leaves the
# channel off for the life of the process. Since #517 the same applies AFTER
# the channel is up: if it stops working it is restarted the same way, and the
# death is recorded as @@BOOT_DIED@@ carrying ran_ms (how long it had been
# working — what tells a real outage apart from a flapping channel). The
# first attempts are durable in audit_log as @@BOOT_FAILED@@:
#   SELECT ts, action, payload FROM audit_log
#    WHERE action IN ('@@BOOT_STARTED@@','@@BOOT_FAILED@@','@@BOOT_DIED@@')
#      AND payload->>'channel' = 'email' ORDER BY ts DESC LIMIT 20;
# @@BOOT_FAILED@@ is RATE-LIMITED twice over: by the downtime clock (every
# attempt until the outage is first reported as @@CHANNEL_STILL_DOWN@@ — 5 min
# of continuous downtime — then only on each repeat of that line, every
# 30 min) and by an attempt-rate gate (5 failed attempts inside an hour,
# then one row per 30 min), whichever engages first. A 24-hour outage
# produces ~53 rows instead of ~1440: the first five attempts, then each
# escalation. @@BOOT_DIED@@
# is RATE-LIMITED by a separate flap alarm: every death until 5 deaths within
# an hour first reports @@CHANNEL_FLAPPING@@, then only on each repeat. These are
# independent: a channel that keeps recovering and dying (e.g., cycles up 61s
# then dies repeatedly) may never produce @@CHANNEL_STILL_DOWN@@ at all, yet will
# still stop writing @@BOOT_DIED@@ rows once the flap alarm fires — and in that
# same cycling regime a restart attempt that fails transiently is bounded by
# the attempt-rate gate (the first few rows carry the failure cause, then one
# row per 30 min keeps sampling it), where it used to write one ungated row
# per cycle. @@BOOT_STARTED@@
# is NOT rate-limited: every successful start writes a row, always. So a gap
# between rows means "still broken, still saying the same thing", NOT
# "recovered" — look for a @@BOOT_STARTED@@ row for recovery. The daemon log
# (~/.local/state/kastellan/*.out) is the per-event record.
# CAVEAT: a channel most often dies because Postgres went away — and the row
# above needs that same Postgres, so exactly that outage writes no rows until
# it is over. The daemon log is the record for it.
#KASTELLAN_EMAIL_ENDPOINT=https://10.0.0.3:8443
#KASTELLAN_EMAIL_SUBSCRIPTION=kastellan
#KASTELLAN_EMAIL_ADDRESS=kastellan@example.org
#KASTELLAN_EMAIL_TOKEN_FILE=/home/hherb/.config/kastellan/localmail-channel.token
#
# TRAP 1: KASTELLAN_EMAIL_AUTHSERV_ID must be your own MX's identifier
# EXACTLY as it appears in the Authentication-Results headers it writes.
# Only the TOPMOST such header is consulted — anyone can write their own
# Authentication-Results lines into a message they send. Get this wrong and
# every message fails closed (silently rejected, never delivered).
#KASTELLAN_EMAIL_AUTHSERV_ID=mx.example.net
#
# TRAP 2: pairing is operator-only, never in-channel:
#   kastellan-cli pair issue-token --channel email --peer you@example.org
# The printed token must appear in the BODY of every message from that peer.
# There is no in-channel pairing over email by design.
# Send PLAIN TEXT (or multipart/alternative including a text part): the token is
# only ever read from the message's text body, so an HTML-only message has no
# token to find and is rejected with reason `no_token` in audit_log. Check there
# first if a correctly-paired address is being turned away.
#
# TRAP 3: a self-signed localmail needs KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA
# above. This channel's force-routed sidecar terminates the worker's TLS and
# re-originates upstream, so the operator anchor named for this origin is
# what lets it validate a self-signed cert. Two constraints come with it,
# both inherited from #492: the cert must be a real CA that signed the origin
# leaf, or a self-signed leaf marked basicConstraints CA:FALSE (a CA:TRUE
# self-signed leaf is REJECTED at handshake time by rustls as
# CaUsedAsEndEntity, even though `openssl verify` accepts it — and
# `openssl req -x509` commonly produces exactly that shape); and the anchor is
# trusted for every host that sidecar can reach, so this worker's egress
# allowlist must resolve to that single private origin. Verify a cert's shape
# with:
#   openssl x509 -in <cert.pem> -noout -text | grep -A1 'Basic Constraints'
#
# NOTE: KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA is ONE GLOBAL map, keyed by HOST
# only (not host:port — see the CAVEAT in the upstream-CA help block above).
# An entry added for THIS channel's origin is handed to EVERY worker whose
# allowlist resolves to that same address — including the SEPARATE
# kastellan-worker-mail TOOL, if it points at the same host. Two services
# sharing an address (e.g. localmail on :8443 and a search service on :8888,
# both 10.0.0.3) cannot be distinguished by this map: nothing logs or errors
# when a second worker's sidecar silently inherits the first's anchor. Don't
# conflate this channel with the kastellan-worker-mail tool.
"#
    // Substituted rather than `format!`-interpolated so the raw block above stays
    // free of brace escaping (it contains none today, but a future `${...}` shell
    // snippet in this help would silently break a format string).
    .replace("@@CHANNEL_DISABLED@@", crate::channel::boot_supervisor::CHANNEL_DISABLED_LOG_PHRASE)
    // Same reason the phrase above is substituted rather than typed twice: this
    // block tells an operator the exact `action` values to query, and #516's
    // review found that pairing had already drifted once. Interpolating the
    // constants makes a renamed — or, as in #517, a NEWLY ADDED — action a
    // compile-time edit here rather than a query that silently returns less
    // than the operator thinks it does.
    .replace("@@BOOT_STARTED@@", crate::channel::actions::BOOT_STARTED)
    .replace("@@BOOT_FAILED@@", crate::channel::actions::BOOT_FAILED)
    .replace("@@BOOT_DIED@@", crate::channel::actions::CHANNEL_DIED)
    // Same reason as the three substitutions above: the flap alarm's phrase is
    // a `const` specifically so it cannot drift from what an operator greps
    // for (see `CHANNEL_FLAPPING_LOG_PHRASE`'s doc comment) — a literal here
    // would defeat that the moment either side was renamed.
    .replace(
        "@@CHANNEL_FLAPPING@@",
        crate::channel::boot_supervisor::CHANNEL_FLAPPING_LOG_PHRASE,
    )
    // Third instance of the same class (#524): the escalator's phrase,
    // substituted for the same reason as the two phrase replaces above.
    .replace(
        "@@CHANNEL_STILL_DOWN@@",
        crate::channel::boot_supervisor::CHANNEL_STILL_DOWN_LOG_PHRASE,
    )
}

/// Render the `kastellan.env` EnvironmentFile contents.
pub fn render_env_file(args: &InstallArgs, layout: &Layout) -> String {
    let mut s = String::new();
    s.push_str(
        "# GENERATED BY `kastellan-cli install` — DO NOT HAND-EDIT.\n\
         # This file is regenerated from CLI flags on every install, so any key\n\
         # you add here is dropped and any value you tune here reverts.\n\
         # Put operator settings in kastellan.env.local instead: the installer\n\
         # never writes that file, and its values override the ones below.\n\n",
    );
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
    // planner may issue in one dispatch. Clamped by the worker to [1, 32]. The
    // searches run sequentially under a 60 s wall; the worker also enforces a
    // soft batch deadline, so an oversized/slow batch returns per-query "budget
    // reached" errors for the queries it couldn't reach rather than losing the
    // whole batch.
    s.push_str("# KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES=8\n");
    // Upstream extra CA for a force-routed worker whose origin is PRIVATE and
    // self-signed (#492) — e.g. a personal localmail on the LAN. Commented out:
    // unset means the egress proxy validates every re-originated connection
    // against the public webpki roots only, which is what you want unless you
    // run such an origin yourself. See render_upstream_ca_help for the traps
    // this doc block warns about; they are the ones that actually bite.
    s.push_str(&render_upstream_ca_help());
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
    // Email fallback channel (Phase 2 slice #5) — commented informational
    // block only; unlike Matrix above, this slice has no `--email-*` install
    // flags, so the five KASTELLAN_EMAIL_* vars are never set here. See
    // render_email_help for the three operator traps.
    s.push_str(&render_email_help());
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

/// On-demand binaries: copied when present in the build dir, skipped (with
/// a log line) when not.
///
/// **This list is load-bearing for deployability.** The daemon discovers a
/// worker as a `current_exe()`-relative sibling, so a binary missing here is
/// never copied into `bin_dir` and the capability it backs is silently
/// absent — an otherwise completely healthy install, every unit `active`.
/// How loudly the absence shows up differs by entry kind:
///
///   * **Tool workers** (everything but the two brokers) are resolved
///     unconditionally while the registry is assembled, so a missing binary
///     costs exactly one startup `ERROR` naming the tool
///     (`worker misconfigured; skipping`), and disables only that tool.
///   * **The trusted sidecars** (`embed-broker`, `search-broker`) are not
///     tools and are not resolved this way. `broker::config::from_env`
///     treats "binary absent" as the legitimate never-opted-in state and
///     logs **nothing** at any level, so their absence is entirely silent
///     until an operator opts a worker into broker mode — at which point
///     `assemble_registry` refuses the *consuming* worker by name (#459),
///     with the spawn chokepoint fail-closed behind it.
///     One sidecar can back several tools: `search-broker` serves both
///     `web-search` (`KASTELLAN_WEB_SEARCH_USE_BROKER`) and `web-research`
///     (`KASTELLAN_WEB_RESEARCH_USE_SEARCH_BROKER`), so a single missing
///     binary can refuse more than one tool.
///
/// The list lagged the workspace by five workers before issue #504 caught
/// it; `tests-common::installable` now fails the build when **any** binary
/// the workspace declares is neither listed here nor explicitly exempted —
/// so adding a worker crate forces the question to be answered.
pub fn optional_binaries() -> &'static [&'static str] {
    &[
        "kastellan-worker-shell-exec",
        "kastellan-worker-web-fetch",
        "kastellan-worker-web-search",
        "kastellan-worker-web-research",
        "kastellan-worker-python-exec",
        "kastellan-worker-matrix",
        "kastellan-worker-mail",
        "kastellan-worker-email-in",
        "kastellan-worker-lockdown-exec",
        // The two trusted sidecars. Not tools themselves: a worker reaches
        // them over a UDS the sandbox binds in, and both are resolved by the
        // same exe-relative discovery as everything above.
        "kastellan-worker-embed-broker",
        "kastellan-worker-search-broker",
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
    core.environment_files = vec![
        kastellan_supervisor::EnvFileRef { path: layout.env_file.clone(), optional: false },
        kastellan_supervisor::EnvFileRef { path: layout.env_local_file.clone(), optional: true },
    ];
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
        assert_eq!(l.env_local_file, PathBuf::from("/home/u/.config/kastellan/kastellan.env.local"));
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
        let pg = specs.members.iter().find(|s| s.name == "kastellan-postgres").unwrap();
        assert_eq!(pg.program, PathBuf::from("/usr/lib/postgresql/18/bin/postgres"));
        assert!(specs.target.members.contains(&"kastellan-postgres".to_string()));
        assert!(specs.target.members.contains(&"kastellan-core".to_string()));
    }

    #[test]
    fn specs_point_core_at_the_generated_env_then_the_operator_overlay() {
        let l = layout();
        let specs = build_specs(&l, Path::new("/usr/lib/postgresql/16/bin/postgres"));
        let core = specs.members.iter().find(|s| s.name == "kastellan-core").unwrap();
        // Order is the mechanism: systemd applies these in order and the LATER
        // file wins, so the operator's `.local` overrides anything `install`
        // regenerates. Reversing them would silently restore #458.
        assert_eq!(core.environment_files.len(), 2);
        assert_eq!(core.environment_files[0].path, l.env_file);
        assert!(!core.environment_files[0].optional);
        assert_eq!(core.environment_files[1].path, l.env_local_file);
        assert!(
            core.environment_files[1].optional,
            "the overlay must be optional — the installer never creates it"
        );
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

    /// Every line of the help block must be a comment. If the example line ever
    /// lost its leading `#`, a fresh install would silently widen the egress
    /// proxy's upstream trust to a path that does not exist on that host — a
    /// setting no operator asked for, in the one file they will not re-read.
    #[test]
    fn help_block_is_entirely_commented_out() {
        for line in render_upstream_ca_help().lines() {
            assert!(
                line.starts_with('#'),
                "help block must stay inert; this line would be read as config: {line:?}"
            );
        }
    }

    #[test]
    fn help_block_names_the_env_var_and_both_traps() {
        let help = render_upstream_ca_help();
        assert!(help.contains("KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA"), "{help}");
        // Trap 1: the CA:TRUE self-signed leaf, which fails late and opaquely.
        assert!(help.contains("CA:FALSE"), "must state the working cert shape: {help}");
        assert!(help.contains("CaUsedAsEndEntity"), "must name the rustls error: {help}");
        // Trap 2: the trust scope that motivates the single-origin rule.
        assert!(help.contains("impersonate"), "must state the trust-scope hazard: {help}");
        // The private-literal rule, which is the most surprising refusal.
        assert!(help.contains("LITERAL"), "must state the IP-literal rule: {help}");
        // The per-host (not per-service) keying granularity: an operator running
        // two private services on one address has to know the anchor is shared.
        assert!(help.contains("per-HOST"), "must state the keying granularity: {help}");
    }

    #[test]
    fn env_file_includes_the_help_block() {
        let args = test_args("m", "http://h:1", None);
        let layout = layout();
        assert!(render_env_file(&args, &layout).contains("KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA"));
    }

    /// Same guard as `help_block_is_entirely_commented_out`, for the email
    /// block: a stray uncommented example line would silently start the email
    /// channel against a placeholder endpoint on the operator's next daemon
    /// restart, polling a config nobody configured.
    #[test]
    fn email_help_block_is_entirely_commented_out() {
        for line in render_email_help().lines() {
            assert!(
                line.starts_with('#'),
                "help block must stay inert; this line would be read as config: {line:?}"
            );
        }
    }

    #[test]
    fn email_help_block_names_the_env_var_and_traps() {
        let help = render_email_help();
        assert!(help.contains("KASTELLAN_EMAIL_ENDPOINT"), "{help}");
        assert!(help.contains("KASTELLAN_EMAIL_AUTHSERV_ID"), "{help}");
        // Trap 1: the authserv-id must match the MX's own header value exactly,
        // and only the topmost Authentication-Results header is trusted.
        assert!(help.contains("EXACTLY"), "must state the exact-match rule: {help}");
        assert!(help.contains("TOPMOST"), "must state only the topmost header is trusted: {help}");
        // Trap 2: pairing is operator-only, never in-channel.
        assert!(help.contains("pair issue-token"), "must name the pairing command: {help}");
        assert!(help.contains("in-channel pairing"), "must state there is no in-channel pairing: {help}");
        // The token is read only from the text body, so an HTML-only sender is
        // rejected `no_token` however correctly they are paired — an operator
        // cannot diagnose that without being told where to look.
        assert!(help.contains("PLAIN TEXT"), "must tell the operator to send plain text: {help}");
        assert!(help.contains("no_token"), "must name the audit reason to grep for: {help}");
        // Trap 3: the force-routed sidecar for THIS channel intercepts (it
        // terminates the worker's TLS and re-originates upstream), so
        // KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA is what lets it validate a
        // self-signed localmail — must say so plainly, not the old (now
        // false) claim that the var has no effect here.
        assert!(help.contains("KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA"), "{help}");
        assert!(help.contains("CA:FALSE"), "must state the working cert shape: {help}");
        assert!(help.contains("CaUsedAsEndEntity"), "must name the rustls error: {help}");
        assert!(
            help.contains("single private origin"),
            "must state the single-origin allowlist constraint: {help}"
        );
        // The #492 single-origin rule is inherited verbatim by this channel's
        // own intercepting sidecar now, not just the separate mail tool.
        assert!(help.contains("#492"), "must reference the constraining issue: {help}");
        assert!(
            !help.contains("NO EFFECT"),
            "stale claim: the var now has an effect on this channel's path: {help}"
        );
        // The map is global and host-keyed (not per-service): an anchor added
        // for this channel's origin silently reaches every other worker whose
        // allowlist resolves to the same address, including the separate
        // kastellan-worker-mail tool — this must be spelled out so an operator
        // doesn't conflate the two when reasoning about who an anchor reaches.
        assert!(
            help.contains("kastellan-worker-mail"),
            "must name the tool that can silently inherit this channel's anchor: {help}"
        );
        assert!(
            help.contains("GLOBAL") || help.contains("global"),
            "must state the map is global across workers, not scoped to this channel: {help}"
        );
        assert!(
            help.contains("cannot be distinguished"),
            "must state two services sharing an address are indistinguishable to the map: {help}"
        );
        // Partial-config behaviour: the CHANNEL is disabled, the DAEMON is not.
        // The operator's only signal is a startup log line, so the help must
        // name it verbatim and must NOT still claim the daemon aborts.
        //
        // Asserted against the CONST the supervisor's `error!` interpolates, not
        // against a literal typed here: a literal is what let the help go on
        // telling operators to grep for `EMAIL CHANNEL DISABLED` after #514 moved
        // the line into `boot_supervisor` and dropped the channel name from the
        // message. A literal here would have stayed green through exactly that.
        assert!(
            help.contains(crate::channel::boot_supervisor::CHANNEL_DISABLED_LOG_PHRASE),
            "must name the exact startup log phrase to grep for: {help}"
        );
        // Same reasoning, for the flap alarm's phrase: asserted through the
        // const `report()` interpolates, never as a literal typed a second
        // time here — a literal is exactly what would stay green while the
        // help and the log line drifted apart.
        assert!(
            help.contains(crate::channel::boot_supervisor::CHANNEL_FLAPPING_LOG_PHRASE),
            "must name the exact flap-alarm log phrase to grep for: {help}"
        );
        // Third instance of the same class (#524): the downtime escalator's
        // phrase, asserted through the const `report()` interpolates rather
        // than a literal typed a second time here — a literal is exactly what
        // stayed green while the help and the log line drifted apart, twice.
        assert!(
            help.contains(crate::channel::boot_supervisor::CHANNEL_STILL_DOWN_LOG_PHRASE),
            "must name the exact still-down log phrase to grep for: {help}"
        );
        assert!(
            !help.contains("@@"),
            "every placeholder must be substituted, not shipped to the operator: {help}"
        );
        assert!(
            !help.contains("aborts daemon startup"),
            "stale claim: a partial config no longer aborts the daemon: {help}"
        );
        // Transient failures are retried since #514; help that implies any
        // failure is terminal would send an operator restarting for no reason.
        //
        // The action names are asserted through `channel::actions` rather than
        // as literals for the same reason as the log phrase above — and #517
        // showed the *other* half of that drift: adding an action nobody
        // reflected here left the documented query silently returning less than
        // the operator believed it did. Iterating the set means a new one is a
        // failing test, not a quiet omission.
        assert!(help.contains("RETRIED"), "must state transient bring-up failures retry: {help}");
        for action in [
            crate::channel::actions::BOOT_STARTED,
            crate::channel::actions::BOOT_FAILED,
            crate::channel::actions::CHANNEL_DIED,
        ] {
            assert!(
                help.contains(action),
                "the documented audit query must name every channel bring-up/liveness \
                 action, and is missing {action}: {help}"
            );
        }
    }

    #[test]
    fn env_file_includes_the_email_help_block() {
        let args = test_args("m", "http://h:1", None);
        let layout = layout();
        let s = render_env_file(&args, &layout);
        assert!(s.contains("KASTELLAN_EMAIL_ENDPOINT"), "{s}");
        assert!(s.contains("KASTELLAN_EMAIL_AUTHSERV_ID"), "{s}");
    }

    #[test]
    fn the_generated_env_file_tells_operators_about_the_overlay() {
        // The generated file is the first place an operator looks, and it is
        // the file that gets destroyed. It must name its own successor.
        let s = render_env_file(&test_args("m", "http://h:1", None), &layout());
        assert!(s.contains("kastellan.env.local"), "{s}");
        assert!(
            s.contains("regenerated"),
            "it must say this file is regenerated, not merely that another exists: {s}"
        );
    }
}
