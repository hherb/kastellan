//! What a test daemon is configured with, as data (issue [#634]).
//!
//! [`bring_up_daemon`](super::bring_up_daemon) used to take six
//! positional arguments, four of them `&str` in two adjacent pairs.
//! **Three** integration tests carried a hand-rolled copy of it
//! instead, ~70 identical lines each:
//!
//! * `observation_capture` wanted a seventh, eighth and ninth value — a
//!   real LLM model, an operator-supplied timeout, and a 15 s readiness
//!   budget;
//! * `guard_boot_row_e2e` wanted a 20 s budget and the four
//!   `KASTELLAN_LLM_GUARD_*` keys;
//! * `cli_ask_e2e` wanted **nothing the shared helper lacked** — its one
//!   divergence, the `KASTELLAN_SHELL_EXEC_BIN` registration, was
//!   already exactly what the old `extra_env` parameter carried for
//!   `cli_memory_l3_run_daemon_e2e`. It was duplication for no reason at
//!   all, which is the sharper half of the argument.
//!
//! What that costs is visible in [#635], which had to write one
//! stderr-on-failure fix **twice** — once in this shared helper and once
//! in `guard_boot_row_e2e`'s copy — while `cli_ask_e2e`'s and
//! `observation_capture`'s copies never received it at all and kept
//! reporting a dead daemon as its last polled status.
//!
//! So the parameters became a struct. The point is not brevity: it is
//! that a builder call names every value at the call site, which the
//! positional form did not. Transposing two adjacent `&str` arguments
//! compiles in silence — the same hazard
//! [#632](https://github.com/hherb/kastellan/issues/632) was filed
//! about one crate over.
//!
//! **The transform is pure.** Nothing here creates a directory,
//! installs a unit or opens a socket; [`DaemonSpec::service_spec`] turns
//! a spec plus three already-existing paths into a
//! [`ServiceSpec`](kastellan_supervisor::ServiceSpec) and nothing else.
//! The one exception is [`DaemonSpec::new`], which reads the clock and
//! `$USER` **once** to derive the values #641 removed from its
//! signature — deliberately eagerly, so that everything downstream is a
//! pure function of stored data.
//! That is what lets the assertions below run as `tests-common` unit
//! tests — the only target in `linux-check.yml` that covers this code,
//! where the daemon e2es these values feed are DGX-gated and run on no
//! PR at all.
//!
//! [#634]: https://github.com/hherb/kastellan/issues/634
//! [#635]: https://github.com/hherb/kastellan/pull/635

use std::path::{Path, PathBuf};
use std::time::Duration;

use kastellan_supervisor::specs::core_service_spec;
use kastellan_supervisor::{validate_service_name, ServiceSpec};

use crate::temp::{current_username, unique_suffix};

/// The planner model every test daemon gets unless it asks for another.
///
/// Only the live-LLM callers override it. The other two tiers keep it:
/// the inert-mock callers never dial the router at all, and the
/// *scripted*-mock ones (`cli_ask_e2e`, `guard_boot_row_e2e`,
/// `mail_daemon_e2e`'s tier-2a leg) do dial it but answer from a queue
/// without reading the model name. So the value is arbitrary — but it
/// must be *present*, because an unset model is a router config error
/// and the daemon refuses to boot.
pub const DEFAULT_LLM_MODEL: &str = "test-local-model";

/// The planner timeout every test daemon gets unless it asks for
/// another, in milliseconds as the env var spells it.
///
/// Loose enough for a slow CI runner; against the inert mock, which
/// answers synchronously on accept, a request is sub-millisecond.
pub const DEFAULT_LLM_TIMEOUT_MS: &str = "5000";

/// How long to wait for `"scheduler spawned"` unless the caller asks
/// for longer.
///
/// ⚠️ **10 s holds only for a caller that does not configure the guard
/// tier.** `scheduler spawned` is logged *after*
/// `GuardTier::from_router_config`, which on a configured tier spends
/// up to `PROBE_BUDGET_MS` (20 s) per attempt on the fatal `/props` call
/// — and since #609 keeps re-asking for up to `PROPS_WAIT_BUDGET_MS`
/// (60 s) while it answers connection-refused or **HTTP 503**, which is
/// what [`crate::scripted_llm::spawn_scripted_llm`] serves — plus
/// `PROBE_TOTAL_BUDGET_MS + PROBE_BUDGET_MS` (60 s) on the boot probe:
/// ~80 s in total, where the same two legs cost ~60 s before #626
/// doubled `PROBE_TOTAL_BUDGET_MS`.
///
/// A caller that *does* configure a guard must do **both**, not either:
///
/// * pin `KASTELLAN_LLM_GUARD_TIMEOUT_MS`, which skips the probe —
///   **but not the `/props` call**. `/props` is step 3 of
///   `from_router_config` and runs unconditionally, before the pin is
///   ever consulted; and
/// * raise this to at least 20 s, because that surviving `/props` leg
///   is itself capped at `PROBE_BUDGET_MS` — twice this default.
///
/// `guard_boot_row_e2e` does exactly that pair. Getting it wrong is a
/// nasty failure to read: the daemon is alive and probing, so
/// `stderr_tail` prints `<empty>` and the panic says only "should log
/// 'scheduler spawned' within 10s", which looks like a wedged daemon
/// rather than a budget that was documented as wrong for this
/// configuration.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(10);

mod llm_endpoint;

pub use llm_endpoint::{LlmEndpoint, COMPAT_SEGMENT};

/// Everything [`bring_up_daemon`](super::bring_up_daemon) needs.
///
/// Built with [`DaemonSpec::new`] plus the setters below. The **three**
/// values in `new` are the ones no caller can omit and no default can
/// supply; everything else has a default that matches what the shared
/// helper did before this type existed, so a migrated caller that sets
/// nothing extra behaves identically.
///
/// The third, `llm`, is worth naming separately: it is the one parameter
/// carrying a choice that fails *silently* if made wrong. See
/// [`LlmEndpoint`].
#[derive(Debug, Clone)]
pub struct DaemonSpec {
    label: String,
    suffix: String,
    data_dir: PathBuf,
    user: String,
    llm: LlmEndpoint,
    llm_model: String,
    llm_timeout_ms: String,
    ready_timeout: Duration,
    force_routing: bool,
    extra_env: Vec<(String, String)>,
}

impl DaemonSpec {
    /// `label` distinguishes co-running tests' temp dirs and service
    /// names (`"l3run"` → `kastellan-supervisor-test-core-l3run-<suffix>`);
    /// `data_dir` is the per-test Postgres cluster's data directory;
    /// `llm` is where the planner router dials, whose two shapes are
    /// **not** interchangeable — see [`LlmEndpoint`].
    ///
    /// # Why only three parameters (issue [#641])
    ///
    /// It took five, of which `label`, `suffix` and `user` were all
    /// `impl Into<String>`. Any permutation of those three compiled in
    /// silence — the exact hazard [#632] was filed about one crate over,
    /// reproduced in the constructor of the thing that removes it. The
    /// apparent barrier, `data_dir` sitting between them, was
    /// accidental: `impl Into<PathBuf>` accepts a `&str` just as
    /// happily, so it would have evaporated the first time someone
    /// passed a string path.
    ///
    /// Both removed values were **always** the same expression at all
    /// six call sites — `unique_suffix()` and `current_username()` — so
    /// deriving them here does not lose a choice any caller was making.
    /// It makes them impossible to get wrong rather than merely hard to,
    /// and no two remaining parameters share a type.
    ///
    /// Should a caller ever genuinely need to name its own, add
    /// `.suffix(…)` / `.user(…)` setters. None is added speculatively:
    /// an unused setter is a hatch that re-opens the transposition this
    /// signature closes.
    ///
    /// # Reads the environment; the transform stays pure
    ///
    /// This is the one function in the module that is not a pure
    /// transform: [`unique_suffix`] reads the clock and a counter, and
    /// [`current_username`] reads `$USER`. Both are read **once, here**,
    /// precisely so that everything downstream —
    /// [`service_spec`](Self::service_spec) above all — is a pure
    /// function of stored data and can be asserted without a filesystem
    /// or a supervisor.
    ///
    /// One consequence worth knowing: the suffix in the unit name is no
    /// longer the same string as the sibling Postgres cluster's, because
    /// each is now drawn separately. Nothing reads that correspondence
    /// today; if [#548]'s stale-unit sweep ever wants to correlate a
    /// leaked unit with its leaked cluster, a `.suffix(…)` setter
    /// restores it.
    ///
    /// # Panics
    ///
    /// If `label` yields a service name the supervisor would refuse —
    /// too long, or carrying a character outside `[A-Za-z0-9._-]`. The
    /// check is [`kastellan_supervisor::validate_service_name`], the
    /// same predicate `install` applies, so this cannot drift from it.
    /// Failing here rather than at `install` names the wrong `label` at
    /// the line that supplied it. Since `suffix` is derived and is
    /// always digits and dashes, `label` is the only input that can
    /// fail.
    ///
    /// [#632]: https://github.com/hherb/kastellan/issues/632
    /// [#548]: https://github.com/hherb/kastellan/issues/548
    /// [#641]: https://github.com/hherb/kastellan/issues/641
    pub fn new(label: impl Into<String>, data_dir: impl Into<PathBuf>, llm: LlmEndpoint) -> Self {
        let spec = Self {
            label: label.into(),
            suffix: unique_suffix(),
            data_dir: data_dir.into(),
            user: current_username(),
            llm,
            llm_model: DEFAULT_LLM_MODEL.to_string(),
            llm_timeout_ms: DEFAULT_LLM_TIMEOUT_MS.to_string(),
            ready_timeout: DEFAULT_READY_TIMEOUT,
            force_routing: true,
            extra_env: Vec::new(),
        };
        // Validated at construction, not in `service_spec`: the name is
        // fully determined by `label` plus the suffix just drawn, so
        // there is nothing later that could change the answer — and
        // `service_name()` is `pub`, so a check living only in
        // `service_spec` left one public path unguarded.
        if let Err(e) = validate_service_name(&spec.service_name()) {
            panic!(
                "DaemonSpec label {:?} yields a service name the supervisor refuses: {e}. \
                 Labels must be short and match [A-Za-z0-9._-]",
                spec.label,
            );
        }
        spec
    }

    /// Override [`DEFAULT_LLM_MODEL`] — for a caller driving a real LLM.
    pub fn llm_model(mut self, model: impl Into<String>) -> Self {
        self.llm_model = model.into();
        self
    }

    /// Override [`DEFAULT_LLM_TIMEOUT_MS`]. Takes the string the env
    /// var carries, not a `Duration`, because the callers that need it
    /// read it straight out of an operator-supplied env var and never
    /// parse it.
    pub fn llm_timeout_ms(mut self, ms: impl Into<String>) -> Self {
        self.llm_timeout_ms = ms.into();
        self
    }

    /// Override [`DEFAULT_READY_TIMEOUT`] — read its ⚠️ first.
    ///
    /// The reason a caller needs longer is usually a configured guard
    /// tier, and pinning `KASTELLAN_LLM_GUARD_TIMEOUT_MS` is **not** an
    /// alternative to raising this: the pin skips the boot probe but not
    /// the fatal `/props` call, which is capped at `PROBE_BUDGET_MS`
    /// (20 s) on its own. Such a caller needs both.
    pub fn ready_timeout(mut self, d: Duration) -> Self {
        self.ready_timeout = d;
        self
    }

    /// Add one test-specific environment variable.
    ///
    /// Applied **after** the common keys *and* after everything
    /// `core_service_spec` bakes in, so an entry naming a key either of
    /// them already set wins. Until #634 that guarantee was a comment at
    /// a call site with nothing testing it;
    /// `extra_env_wins_over_a_default_it_names` below is what makes it a
    /// property.
    ///
    /// This is a guarantee `env` makes so that a caller *can* rely on it,
    /// not one any caller currently needs. The tree's one live override
    /// — `mail_daemon_e2e` turning force routing off — goes through
    /// [`DaemonSpec::force_routing`] instead, precisely so a containment
    /// control is not disabled through a hatch that looks identical to
    /// registering a worker binary.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    /// Add several test-specific environment variables, in order.
    pub fn envs(mut self, vars: impl IntoIterator<Item = (String, String)>) -> Self {
        self.extra_env.extend(vars);
        self
    }

    /// Turn the inherited `KASTELLAN_EGRESS_FORCE_ROUTING=1` off.
    ///
    /// ⚠️ **This disables a containment control**, not a convenience.
    /// With force routing on, a worker gets a private netns and reaches
    /// its allowlist only through the egress proxy; with it off, the
    /// worker takes a direct route. `core_service_spec` bakes the `1` in
    /// (`supervisor/src/specs.rs`) and every test daemon inherits it.
    ///
    /// It exists as a named setter rather than as an `env` entry so that
    /// `grep force_routing` finds every test that opts out. Exactly one
    /// does: `mail_daemon_e2e`, whose mock localmail origin is plain HTTP
    /// on loopback and cannot be reached through the proxy's MITM
    /// upstream (which trusts webpki roots only).
    ///
    /// Passing `true` is a no-op — the inherited default already says so,
    /// and re-stating it would put a second `Environment=` line in the
    /// unit for no reason.
    pub fn force_routing(mut self, on: bool) -> Self {
        self.force_routing = on;
        self
    }

    /// The supervisor unit name this spec installs under.
    pub fn service_name(&self) -> String {
        format!(
            "kastellan-supervisor-test-core-{}-{}",
            self.label, self.suffix
        )
    }

    /// The temp-root infixes for this daemon's log and state dirs.
    pub(crate) fn log_dir_infix(&self) -> String {
        format!("cli-{}-clog", self.label)
    }

    pub(crate) fn state_dir_infix(&self) -> String {
        format!("cli-{}-state", self.label)
    }

    pub(crate) fn ready_timeout_value(&self) -> Duration {
        self.ready_timeout
    }

    /// Turn this spec plus three already-created paths into the
    /// supervisor unit to install.
    ///
    /// Pure: no directory is created, no unit is written, no socket is
    /// opened. The caller owns the three paths' lifetimes (they are
    /// `PathGuard`-backed temp roots) precisely so this half can be
    /// asserted without a supervisor or a filesystem.
    ///
    /// The `stdout_log`/`stderr_log` fields `core_service_spec` derives
    /// from `CORE_SERVICE_NAME` are **replaced**, because this spec
    /// renames the unit and the log file names follow the unit name.
    pub(crate) fn service_spec(
        &self,
        binary: &Path,
        core_log_dir: &Path,
        state_dir: &Path,
    ) -> ServiceSpec {
        let mut spec = core_service_spec(binary, core_log_dir);
        // Already validated in `new` against the supervisor's own
        // `validate_service_name` (#642). It used to be a hand-rolled
        // `len() <= 200` here — a third, unlinked copy of a cap that was
        // `const` private in both backends, checking the half that
        // essentially cannot fire (a real name is ~60 chars) and
        // skipping the charset half that can.
        spec.name = self.service_name();
        spec.stdout_log = Some(core_log_dir.join(format!("{}.out", spec.name)));
        spec.stderr_log = Some(core_log_dir.join(format!("{}.err", spec.name)));

        spec.env.push((
            "KASTELLAN_DATA_DIR".into(),
            self.data_dir.to_string_lossy().into_owned(),
        ));
        spec.env.push(("USER".into(), self.user.clone()));
        spec.env.push((
            "KASTELLAN_STATE_DIR".into(),
            state_dir.to_string_lossy().into_owned(),
        ));

        // Prompts: the daemon's prompt loader fails closed if the dir
        // is missing. `CARGO_MANIFEST_DIR` is `tests-common/` here,
        // whose parent is the workspace root — the same `<root>/prompts`
        // a test crate would resolve.
        let workspace_prompts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("prompts");
        spec.env.push((
            "KASTELLAN_PROMPTS_DIR".into(),
            workspace_prompts.to_string_lossy().into_owned(),
        ));

        spec.env
            .push(("KASTELLAN_LLM_LOCAL_URL".into(), self.llm.url()));
        spec.env
            .push(("KASTELLAN_LLM_LOCAL_MODEL".into(), self.llm_model.clone()));
        spec.env.push((
            "KASTELLAN_LLM_TIMEOUT_MS".into(),
            self.llm_timeout_ms.clone(),
        ));

        // Only on the opt-out path: `core_service_spec` already pushed
        // the `1`, so re-stating it would add a second `Environment=`
        // line saying what the first one says. See `force_routing`.
        if !self.force_routing {
            spec.env
                .push(("KASTELLAN_EGRESS_FORCE_ROUTING".into(), "0".into()));
        }

        // LAST, so a caller naming a key set above overrides it. See
        // `env`'s doc.
        spec.env.extend(self.extra_env.iter().cloned());

        // ...and then collapse duplicates, keeping the last.
        //
        // **Last-wins is the documented contract; this is what stops it
        // from resting on an undefined behaviour.** Without the collapse,
        // an override reaches the daemon as two entries for one key and
        // the winner is decided by the *renderer*: systemd emits one
        // `Environment=` line each and documents last-wins, but launchd
        // emits a plist dict with a duplicate key, whose resolution the
        // plist format does not define. Nothing in `kastellan-supervisor`
        // tests either, so the guarantee `env` makes — and that
        // `force_routing(false)`, a containment control, depends on —
        // would be a belief about `CFPropertyList` on one of the two
        // first-class platforms.
        //
        // Collapsing here makes the rendered unit unambiguous on both,
        // and preserves the documented semantics exactly: the value kept
        // is the one last-wins would have chosen. The general question —
        // any other `ServiceSpec` producer passing duplicate keys — is
        // #644.
        dedup_last_wins(&mut spec.env);

        spec
    }
}

/// Collapse repeated keys, keeping the **last** occurrence's value at
/// the last occurrence's position.
///
/// Order among the survivors is otherwise preserved, so the unit reads
/// the way it was built. Separate from [`DaemonSpec::service_spec`]
/// because it is a pure list operation with its own unit tests, and
/// because "which entry wins" is the whole point of the function's
/// existence rather than a detail of building a spec.
fn dedup_last_wins(env: &mut Vec<(String, String)>) {
    let mut seen_from_the_right: Vec<&str> = Vec::new();
    let mut keep = vec![false; env.len()];
    for (i, (k, _)) in env.iter().enumerate().rev() {
        if !seen_from_the_right.contains(&k.as_str()) {
            seen_from_the_right.push(k.as_str());
            keep[i] = true;
        }
    }
    let mut i = 0;
    env.retain(|_| {
        let k = keep[i];
        i += 1;
        k
    });
}

#[cfg(test)]
mod tests;
