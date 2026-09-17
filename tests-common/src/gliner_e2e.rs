//! Preconditions for the gliner-relex end-to-end tiers, in one place.
//!
//! Three integration suites — `core/tests/gliner_relex_e2e.rs`,
//! `entity_extraction_e2e.rs` and `memory_entity_link_e2e.rs` — spawn the
//! **real** Python worker on the **real** 1.3 GB model. Each used to carry its
//! own copy of the precondition cascade and its own `GlinerRelexEnv` builder,
//! and the copies drifted: one kept `interpreter_root: None` after the other
//! two were fixed (#284), and all three then missed the symlink alias (#650).
//!
//! # The two problems this module exists to fix
//!
//! **1. A skip that cannot be turned into a failure is not a gate ([#653]).**
//! Every precondition below `[SKIP]`s by default, which is correct on an
//! unstaged host. But a *staged* host that trips one silently reports green,
//! and that is how #651's fixture bug survived for months: the DGX's `.venv`
//! was a copy of the Mac's, so `bin/python` named a path that cannot exist on
//! Linux, and four tests read as passing while containing nothing.
//!
//! Setting [`REQUIRE_ENV`] to a truthy value turns every one of those skips
//! into a **panic naming the unmet precondition**. That is the Rust half of
//! the `KASTELLAN_GLINER_RELEX_REQUIRE_E2E` knob `workers/gliner-relex/`
//! already honours on the Python side (`tests/live_support.py`), and it is
//! what makes the `[SKIP]` counts in `HANDOVER.md` mean something.
//!
//! **2. The fixtures spoke a different flag dialect than production
//! ([#654]).** #459 unified every operator-facing kastellan flag on
//! `1|true|yes|on` (trimmed, case-insensitive), and production reads
//! [`ENABLE_ENV`] through `env_flag_enabled`. The three fixtures kept a strict
//! `!= Some("1")`, so an operator whose `kastellan.env` legitimately reads
//! `KASTELLAN_GLINER_RELEX_ENABLE=true` — which *does* enable the daemon
//! worker — got a silent skip. Both halves now go through the same reader.
//!
//! # Scope
//!
//! [`REQUIRE_ENV`] covers the five host-mode preconditions #653 names, all of
//! them inside [`gliner_host_env`]: the opt-in flag, the sandbox probe, the
//! supervisor probe, the venv shim and the weights snapshot.
//!
//! A **sixth** is covered the same way but from outside this module: the
//! Postgres bring-up. [`crate::skip::pg_bin_dir_or_skip`] and
//! [`crate::skip::skip_if_no_supervisor`] stay skip-only for their ~70 other
//! callers, so the three gliner-relex suites call the `*_or_reason` forms and
//! route them through [`report_unmet`] in their own `bring_up_pg`. Without a
//! cluster the test body never runs, so leaving it skip-only would have left
//! the knob reporting green on the exact false premise it exists to abolish.
//!
//! That has a **blast radius worth stating**: `bring_up_pg` is shared with the
//! mock-extractor tiers, so on a host with no Postgres at all, setting
//! [`REQUIRE_ENV`] fails ~18 tests in `entity_extraction_e2e.rs` and
//! `memory_entity_link_e2e.rs` that never touch gliner-relex. That is the
//! intended reading — if you demanded a real run, you have Postgres — but it
//! is a consequence to know about rather than discover.
//!
//! The macOS **container** tier (`container` CLI present, image built) keeps
//! skip-only semantics for its own two probes and stays in
//! `gliner_relex_e2e.rs`: it is a separate and much heavier opt-in, so folding
//! it in here would make the knob unusable on a Mac that has the venv staged
//! but no image. Its Postgres bring-up is shared with the host tier and so
//! *is* require-aware.
//!
//! [#653]: https://github.com/hherb/kastellan/issues/653
//! [#654]: https://github.com/hherb/kastellan/issues/654

use std::path::{Path, PathBuf};

use kastellan_core::worker_lifecycle::force_route::env_flag_enabled;
use kastellan_core::workers::gliner_relex::GlinerRelexEnv;
use kastellan_core::workers::interpreter_deps::InterpreterRoot;

use crate::gliner_weights::weights_dir_or_reason;
// Re-exported, not merely imported: `UnmetAction` was defined here before the
// contract was shared (#714), and `crate::microvm` plus the three gliner suites
// already spell it `gliner_e2e::UnmetAction`. Keeping the path alive means
// hoisting the type costs no call-site churn — and a churn-free hoist is what
// makes the NEXT tier adopt it rather than copy it.
pub use crate::require::UnmetAction;

use crate::require::RequireKnob;
use crate::sandbox::sandbox_unavailable_reason;
use crate::skip::supervisor_unavailable_reason;
use crate::venv_interpreter::venv_interpreter_binds;

/// The operator's opt-in for the real-model tiers. Production reads the same
/// variable through the same `env_flag_enabled` dialect
/// (`core/src/workers/gliner_relex/resolve.rs`).
pub const ENABLE_ENV: &str = "KASTELLAN_GLINER_RELEX_ENABLE";

/// The operator's demand that the tiers actually run. Same name and same
/// dialect as the Python suite's knob (`workers/gliner-relex/tests/live_support.py`).
pub const REQUIRE_ENV: &str = "KASTELLAN_GLINER_RELEX_REQUIRE_E2E";

/// The uv-generated console-script shim, relative to the workspace root.
/// `scripts/workers/gliner-relex/install.sh` is what creates it (via `uv sync`
/// honouring `[project.scripts]`), so this literal and that script must agree.
pub const VENV_SHIM_SUBPATH: &str =
    "workers/gliner-relex/.venv/bin/kastellan-worker-gliner-relex";

/// The model these tiers pin. Every host-mode fixture used its own copy of this
/// string; one definition means a model bump cannot land in two suites and miss
/// the third.
pub const MODEL_ID: &str = "knowledgator/gliner-relex-multi-v1.0";

/// The knob for this tier, as data.
///
/// The machinery behind it — the flag dialect, the skip/fail split, the
/// out-of-dialect warning, the `[E2E]` positive control — lives in
/// [`crate::require`] and is shared with every other gated tier (#714, #622).
/// This module keeps its own names as thin delegates because three suites and
/// their tests already call them, and because the *cascade* below is
/// gliner-specific even though the knob is not.
pub const KNOB: RequireKnob = RequireKnob::new(REQUIRE_ENV, "gliner-relex");

/// Pure: does this [`REQUIRE_ENV`] value demand a real run?
///
/// Delegates to [`crate::require::unmet_action`]; kept as a named re-export
/// because this module's own tests and the three suites' docs refer to it.
pub fn unmet_action(require_flag: Option<String>) -> UnmetAction {
    crate::require::unmet_action(require_flag)
}

/// Whether a tier's precondition set includes the [`ENABLE_ENV`] opt-in.
///
/// `gliner_relex_e2e.rs` runs whenever the venv and the weights are staged —
/// it has no separate opt-in — while the two tiers that drive the model
/// through the extraction/recall path wait for the operator's flag. (Postgres
/// is *not* the distinction: all three suites bring up a cluster.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnableFlag {
    /// The tier is gated on [`ENABLE_ENV`] being truthy.
    Checked,
    /// The tier ignores [`ENABLE_ENV`] entirely, in both directions.
    Ignored,
}

/// Pure: is the opt-in flag an unmet precondition for this tier? `Some(reason)`
/// when it is, `None` when the tier may proceed.
pub fn enable_gate_unmet(gate: EnableFlag, enable_flag: Option<String>) -> Option<String> {
    if gate == EnableFlag::Ignored || env_flag_enabled(enable_flag.clone()) {
        return None;
    }
    // Echo what was actually read. #654's whole bug class is "operator set the
    // flag to a spelling the reader rejects", and a message that names only the
    // variable leaves `ENABLE=ture` undiagnosable from the line itself.
    let observed = match enable_flag.as_deref() {
        None => "<unset>".to_string(),
        Some(v) => format!("{v:?}"),
    };
    Some(format!(
        "{ENABLE_ENV}={observed} is not truthy (1|true|yes|on) — this tier \
         loads the real 1.3 GB model and is opt-in"
    ))
}

/// Pure: where the venv shim lives under `workspace_root`.
pub fn venv_shim_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(VENV_SHIM_SUBPATH)
}

/// Pure: the venv root that owns `shim`, i.e. `<venv>/bin/<shim>`'s grandparent.
///
/// Derived from the shim rather than re-assembled from the workspace root, so
/// the path that is spawned and the path that is bound into the jail cannot
/// name two different venvs.
///
/// # Panics
///
/// If `shim` is not a `<venv>/bin/<name>` path. Two distinct ways, and the
/// second is why this is an `assert!` rather than only an `expect`: a
/// single-component path has no grandparent and `.parent()` yields `None`, but
/// a two-component **relative** path (`bin/x`) has one that is the *empty*
/// path — `Ok("")`, silently, which would then be handed to
/// `venv_interpreter_binds` and to the jail's `fs_read` as a venv root. Not
/// reachable from [`gliner_host_env`], whose input is always absolute and
/// `exists()`-checked, but this is `pub` and a future caller should not have to
/// discover that from the source.
pub fn venv_dir_of(shim: &Path) -> PathBuf {
    let venv = shim
        .parent()
        .and_then(|bin| bin.parent())
        .unwrap_or_else(|| panic!("{} is not a <venv>/bin/<name> path", shim.display()));
    assert!(
        !venv.as_os_str().is_empty(),
        "{} has no venv root above bin/ — a relative shim path cannot be used",
        shim.display()
    );
    venv.to_path_buf()
}

/// The workspace root, resolved at compile time from this crate's manifest dir.
///
/// The `.parent()` is correct only because `tests-common/` sits *directly*
/// under the workspace root. Move this crate to `crates/tests-common/` and the
/// derivation silently returns `<ws>/crates`, so [`VENV_SHIM_SUBPATH`] resolves
/// against the wrong base and every host skips forever with a reason naming a
/// path nobody ever installs to — or, under [`REQUIRE_ENV`], panics naming it.
/// The assertion turns that silent misdirection into an immediate, explicit
/// failure.
///
/// # Panics
///
/// If the resolved path is not the kastellan workspace root.
pub fn workspace_root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent — broken workspace layout")
        .to_path_buf();
    assert!(
        root.join("Cargo.toml").is_file() && root.join("core").is_dir(),
        "{} is not the kastellan workspace root — tests-common has moved, and \
         VENV_SHIM_SUBPATH now resolves against the wrong base",
        root.display()
    );
    root
}

/// Resolve the in-tree venv shim, or say why it is not there.
///
/// Deliberately does **not** honour the daemon's
/// `KASTELLAN_GLINER_RELEX_VENV_DIR` override: tests always run against the
/// in-tree `workers/gliner-relex/.venv/`, so a stray operator export cannot
/// point a test run at a different worker than the one just built.
pub fn venv_shim_or_reason() -> Result<PathBuf, String> {
    let shim = venv_shim_path(&workspace_root());
    if shim.exists() {
        Ok(shim)
    } else {
        Err(format!(
            "gliner-relex venv shim not built at {} — run scripts/workers/gliner-relex/install.sh",
            shim.display()
        ))
    }
}

/// Read [`REQUIRE_ENV`] from the process environment.
///
/// Delegates to [`RequireKnob::action`], which also emits the out-of-dialect
/// warning when the value is neither truthy nor a recognised opt-out.
pub fn require_action() -> UnmetAction {
    KNOB.action()
}

/// Warn when `value` is neither truthy nor a recognised opt-out.
///
/// Delegates to [`crate::require::warn_if_out_of_dialect`]. Kept as a named
/// re-export because this module's tests pin the rendering through it.
pub fn warn_if_out_of_dialect(var: &str, value: Option<&str>, out: &mut dyn std::io::Write) {
    crate::require::warn_if_out_of_dialect(var, value, out)
}

/// Act on an unmet precondition: skip cleanly, or fail loudly.
///
/// Delegates to [`RequireKnob::report_unmet`]. Generic in the return type so a
/// caller inside a `-> Option<_>` fixture can `return report_unmet(..)`
/// directly — omitting the `return` is a **compile error** (E0282), not a
/// silent fall-through.
///
/// `pub` so a suite can route a precondition that is **not** part of
/// [`gliner_host_env`]'s cascade through the same knob.
///
/// # Panics
///
/// Under [`UnmetAction::Fail`], naming both [`REQUIRE_ENV`] and `reason`.
pub fn report_unmet<T>(action: UnmetAction, reason: &str) -> Option<T> {
    KNOB.report_unmet(action, reason)
}

/// [`report_unmet`] with the skip line written to `out` instead of stderr.
///
/// Exists so a unit test can prove the Skip arm **emits** the line without
/// inflating the very `[SKIP]` count it is protecting.
///
/// # Panics
///
/// Under [`UnmetAction::Fail`], exactly as [`report_unmet`] does.
pub fn report_unmet_to<T>(
    action: UnmetAction,
    reason: &str,
    out: &mut dyn std::io::Write,
) -> Option<T> {
    KNOB.report_unmet_to(action, reason, out)
}

/// Build the host-mode [`GlinerRelexEnv`] the three e2e tiers share, or report
/// the first unmet precondition.
///
/// The cascade, in order — each step is skipped-or-failed by
/// [`report_unmet`] according to [`REQUIRE_ENV`]:
///
/// 1. the [`ENABLE_ENV`] opt-in (only when `gate` is [`EnableFlag::Checked`]),
/// 2. the per-OS sandbox probe,
/// 3. the user-level supervisor probe,
/// 4. the venv shim,
/// 5. the weights snapshot.
///
/// Ordered cheapest-and-most-likely-first so an unstaged host reports the
/// precondition an operator would fix first, not the last one that happened to
/// fail. The probes are short-circuited rather than evaluated together because
/// the sandbox probe spawns a real `bwrap`.
///
/// The interpreter binds come from [`venv_interpreter_binds`], which calls the
/// **production** resolver — that is deliberate, and it is what #650 was about:
/// a hand-rolled copy in a fixture silently misses a fix to the real one.
pub fn gliner_host_env(gate: EnableFlag) -> Option<GlinerRelexEnv> {
    let action = require_action();
    match first_unmet_precondition(
        gate,
        std::env::var(ENABLE_ENV).ok(),
        sandbox_unavailable_reason,
        supervisor_unavailable_reason,
        venv_shim_or_reason,
        weights_dir_or_reason,
    ) {
        Ok((script_path, weights_dir)) => {
            // The one impure step left, and deliberately outside `host_env_from`:
            // it calls the PRODUCTION resolver against a real venv on disk (#650),
            // and panics loudly on a venv staged for another host (#651). Keeping
            // it here is what lets the field assembly below be unit-tested.
            let binds = venv_interpreter_binds(&venv_dir_of(&script_path));
            Some(host_env_from(script_path, weights_dir, binds))
        }
        Err(reason) => report_unmet(action, &reason),
    }
}

/// Pure: run the cascade over injected probes, returning the shim + weights
/// paths or the **first** unmet precondition's reason.
///
/// Split out of [`gliner_host_env`] so the two things that actually matter here
/// are reachable from a unit test: the **order** (an operator must be told the
/// precondition they would fix first, not the last one that happened to fail)
/// and the **short-circuiting** (`sandbox` spawns a real `bwrap`, so a later
/// probe must not run once an earlier one has failed). Both were previously
/// asserted only by a doc comment. The probes are `FnOnce` precisely so a test
/// can prove the un-run ones were never called.
pub fn first_unmet_precondition(
    gate: EnableFlag,
    enable_flag: Option<String>,
    sandbox: impl FnOnce() -> Option<String>,
    supervisor: impl FnOnce() -> Option<String>,
    shim: impl FnOnce() -> Result<PathBuf, String>,
    weights: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<(PathBuf, PathBuf), String> {
    if let Some(reason) = enable_gate_unmet(gate, enable_flag) {
        return Err(reason);
    }
    if let Some(reason) = sandbox() {
        return Err(reason);
    }
    if let Some(reason) = supervisor() {
        return Err(reason);
    }
    Ok((shim()?, weights()?))
}

/// Pure: assemble the host-mode [`GlinerRelexEnv`] from the resolved paths and
/// the already-computed interpreter binds.
///
/// Every remaining field is a constant of this tier, and putting them here is
/// what lets a unit test pin them. `use_container_backend: false` in particular
/// is load-bearing — flipping it would route all three host-mode suites at the
/// container backend, and nothing observed it before.
///
/// `binds` is a parameter rather than resolved here on purpose: computing it
/// requires a real venv on disk (`venv_interpreter_binds` panics on a dangling
/// interpreter, #651), which would make this function untestable with synthetic
/// paths — and an untestable builder is how `use_container_backend` went
/// unpinned in the first place.
pub fn host_env_from(
    script_path: PathBuf,
    weights_dir: PathBuf,
    binds: (Option<InterpreterRoot>, Vec<PathBuf>),
) -> GlinerRelexEnv {
    let venv_dir = venv_dir_of(&script_path);
    let (interpreter_root, interpreter_lib_dirs) = binds;
    GlinerRelexEnv {
        script_path,
        venv_dir,
        weights_dir,
        model_id: MODEL_ID.to_string(),
        device: "auto".to_string(),
        use_container_backend: false,
        container_image: None,
        interpreter_root,
        interpreter_lib_dirs,
    }
}

/// The binary the Linux tiers spawn the worker through.
///
/// Deliberately declared **outside** the `cfg` in
/// [`gliner_host_lockdown_shim`]. A name that lives only inside the Linux arm
/// is compiled away on macOS, so a Mac test run cannot tell it from any other
/// string — the same one-host blind spot that makes an unused `cfg(linux)`
/// helper visible only to the DGX's `-D dead-code` gate. Out here, both hosts
/// compile it and both hosts pin it.
pub const LOCKDOWN_SHIM_BIN: &str = "kastellan-worker-lockdown-exec";

/// The lockdown-exec shim the host-mode tiers spawn the worker through.
///
/// On Linux the worker must run under the `ml_client` seccomp filter (#281),
/// which the production manifest arranges via `discover_binary`; the e2e tiers
/// mirror it. On macOS Seatbelt is applied from the parent, so there is no shim
/// and this is `None`.
pub fn gliner_host_lockdown_shim() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Some(crate::binaries::workspace_target_binary(LOCKDOWN_SHIM_BIN))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The project flag dialect (#459), asserted at *this* call site rather
    /// than trusted from `env_flag_enabled`'s own tests — the regression this
    /// guards against is someone re-introducing a strict `Some("1")` here.
    #[test]
    fn the_require_knob_takes_the_whole_project_flag_dialect() {
        for truthy in ["1", "true", "TRUE", "Yes", " yes ", "on", " 1\n"] {
            assert_eq!(
                unmet_action(Some(truthy.to_string())),
                UnmetAction::Fail,
                "{truthy:?} should demand a real run"
            );
        }
    }

    /// Unset is the default and must stay the default: a plain `cargo test`
    /// on an unstaged host has to keep skipping cleanly.
    #[test]
    fn an_absent_or_falsey_require_knob_still_skips() {
        let falsey: [Option<&str>; 6] = [None, Some(""), Some("0"), Some("off"), Some("no"), Some("maybe")];
        for value in falsey {
            assert_eq!(
                unmet_action(value.map(str::to_string)),
                UnmetAction::Skip,
                "{value:?} must not demand a run"
            );
        }
    }

    /// #654: the operator-facing enable flag, at the fixture's own call site.
    /// `true`/`yes`/`on` enable the daemon worker, so they must enable the
    /// tests too — a fixture that only understands `"1"` turns a correct
    /// `kastellan.env` into a silent skip.
    #[test]
    fn the_enable_gate_accepts_the_dialect_not_just_the_literal_one() {
        for truthy in ["1", "true", " TRUE ", "yes", "on", " 1\n"] {
            assert!(
                enable_gate_unmet(EnableFlag::Checked, Some(truthy.to_string())).is_none(),
                "{truthy:?} should opt the real-model tier in"
            );
        }
        for falsey in [None, Some(""), Some("0"), Some("off")] {
            assert!(
                enable_gate_unmet(EnableFlag::Checked, falsey.map(str::to_string)).is_some(),
                "{falsey:?} should leave the tier opted out"
            );
        }
    }

    /// The tier that has no opt-in flag of its own (`gliner_relex_e2e.rs` runs
    /// whenever the venv + weights are staged) must ignore the flag in BOTH
    /// directions — including when it is set to something falsey.
    #[test]
    fn an_ignored_enable_gate_is_never_unmet() {
        for value in [None, Some(""), Some("0"), Some("1"), Some("true")] {
            assert!(
                enable_gate_unmet(EnableFlag::Ignored, value.map(str::to_string)).is_none(),
                "{value:?} must not gate a tier that ignores the flag"
            );
        }
    }

    /// The shim path is a cross-script constant:
    /// `scripts/workers/gliner-relex/install.sh` writes it and `uv sync`
    /// creates it. Asserting the literal is the only thing that catches a typo.
    #[test]
    fn the_venv_shim_path_is_the_one_install_sh_writes() {
        assert_eq!(
            VENV_SHIM_SUBPATH,
            "workers/gliner-relex/.venv/bin/kastellan-worker-gliner-relex"
        );
        assert_eq!(
            venv_shim_path(Path::new("/srv/kastellan")),
            PathBuf::from(
                "/srv/kastellan/workers/gliner-relex/.venv/bin/kastellan-worker-gliner-relex"
            )
        );
    }

    /// The venv dir is the shim's grandparent, and the fixture derives it
    /// rather than re-deriving the path from the workspace root — so the two
    /// cannot disagree about which venv is under test.
    #[test]
    fn the_venv_dir_is_the_shims_grandparent() {
        let shim = venv_shim_path(Path::new("/srv/kastellan"));
        assert_eq!(
            venv_dir_of(&shim),
            PathBuf::from("/srv/kastellan/workers/gliner-relex/.venv")
        );
    }

    /// The model id was a fourth string copied per fixture. Pinning the
    /// literal is what catches a bump that lands in two suites and misses the
    /// third — the exact way the interpreter binds drifted (#284).
    #[test]
    fn the_model_id_is_the_one_the_weights_snapshot_holds() {
        assert_eq!(MODEL_ID, "knowledgator/gliner-relex-multi-v1.0");
    }

    /// The shim binary's NAME, pinned on every host.
    ///
    /// [`LOCKDOWN_SHIM_BIN`] sits outside the `cfg` precisely so this assertion
    /// runs on the Mac too. Inside the Linux arm it would be compiled away
    /// here, and a typo in it would be provable only on the DGX.
    #[test]
    fn the_lockdown_shim_names_the_ml_client_filter_binary() {
        assert_eq!(LOCKDOWN_SHIM_BIN, "kastellan-worker-lockdown-exec");
    }

    /// The platform contract itself, asserted in BOTH directions.
    ///
    /// Written with `cfg!()` rather than `#[cfg]` on purpose: a `#[cfg]` ladder
    /// compiles only its own host's arm, so each host silently stops checking
    /// the other's. Here both arms compile everywhere and one runs — which is
    /// as far as a single host can go, since only Linux can observe the `Some`.
    #[test]
    fn the_lockdown_shim_is_linux_only() {
        let shim = gliner_host_lockdown_shim();
        if cfg!(target_os = "linux") {
            let path = shim.expect("Linux runs the worker under the ml_client filter (#281)");
            assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some(LOCKDOWN_SHIM_BIN)
            );
        } else {
            assert!(
                shim.is_none(),
                "off Linux, Seatbelt is applied from the parent — there is no shim"
            );
        }
    }

    /// The default renders a `[SKIP]` line and nothing else.
    ///
    /// Asserted against [`crate::skip::skip_line`] rather than by calling
    /// [`report_unmet`], on purpose: `report_unmet`'s skip arm *prints* that
    /// line, and `grep -c '^\[SKIP\]'` over a `--nocapture` run is how this
    /// tree audits for tests that went green without executing anything. A unit
    /// test that emitted one would inflate the count it exists to protect — it
    /// did, and showed up as a fifth `[SKIP]` in a DGX gate whose other four
    /// were real.
    ///
    /// The `None` return itself needs no test: `report_unmet` is generic in `T`
    /// and never constructs one, so the skip arm cannot be mutated to return
    /// `Some(..)` and still compile.
    #[test]
    fn an_unmet_precondition_renders_one_skip_line_by_default() {
        assert_eq!(
            crate::skip::skip_line("weights dir missing at /nowhere"),
            "\n[SKIP] weights dir missing at /nowhere\n"
        );
    }

    /// A multi-line reason still renders as ONE `[SKIP]` line.
    ///
    /// Not hypothetical: both supervisor backends embed a `\n\n` operator hint,
    /// unconditionally on macOS. Without flattening, one skip emits a `[SKIP]`
    /// line plus orphan continuation lines, and a demanded run panics with a
    /// multi-line message.
    #[test]
    fn a_multi_line_reason_is_flattened_into_one_skip_line() {
        let rendered = crate::skip::skip_line("probe failed: no bus\n\nRun `loginctl\nenable-linger`.");
        assert_eq!(
            rendered,
            "\n[SKIP] probe failed: no bus Run `loginctl enable-linger`.\n"
        );
        assert_eq!(
            rendered.matches('\n').count(),
            2,
            "exactly the leading and trailing newline: {rendered:?}"
        );
    }

    /// The Skip arm must actually **emit** the line, not merely be able to
    /// render it.
    ///
    /// Asserting on `skip_line` alone leaves the `eprint!` deletable,
    /// `print!`-able (stdout, which the audit never reads) or droppable with
    /// every other test still green — a mutation strictly worse than the bug
    /// #653 fixed, because `grep -c '^\[SKIP\]'` would then report a *clean*
    /// run. Writing into a `Vec<u8>` pins the emission without adding a real
    /// `[SKIP]` line to the count this suite protects.
    #[test]
    fn the_skip_arm_writes_the_line_and_returns_none() {
        let mut sink = Vec::new();
        let got: Option<()> =
            report_unmet_to(UnmetAction::Skip, "weights dir missing at /nowhere", &mut sink);
        assert!(got.is_none(), "the skip arm never yields a value");
        assert_eq!(
            String::from_utf8(sink).expect("utf8"),
            crate::skip::skip_line("weights dir missing at /nowhere"),
            "the skip arm must write exactly the audited line"
        );
    }

    /// `require_action` must read [`REQUIRE_ENV`] — the wiring, not the rule.
    ///
    /// Without this, `unmet_action(None)` in place of the env read passes every
    /// other test in this module while leaving the knob permanently inert:
    /// #653's entire fix, silently reverted, undetectable. Proven by mutation,
    /// which is why it is here.
    #[test]
    fn require_action_reads_the_environment_not_a_constant() {
        let _guard = crate::env::env_lock();

        let _set = crate::env::EnvVarGuard::set(REQUIRE_ENV, "1");
        assert_eq!(require_action(), UnmetAction::Fail, "set truthy must demand");
        drop(_set);

        let _dialect = crate::env::EnvVarGuard::set(REQUIRE_ENV, "TRUE");
        assert_eq!(
            require_action(),
            UnmetAction::Fail,
            "the #654 dialect must reach the env read too"
        );
        drop(_dialect);

        let _off = crate::env::EnvVarGuard::set(REQUIRE_ENV, "0");
        assert_eq!(require_action(), UnmetAction::Skip, "an explicit opt-out skips");
        drop(_off);

        let _unset = crate::env::EnvVarGuard::unset(REQUIRE_ENV);
        assert_eq!(require_action(), UnmetAction::Skip, "unset is the default");
    }

    /// An out-of-dialect value warns rather than silently no-opping.
    ///
    /// The anti-silent-skip knob must not go silent on itself: `=y` is operator
    /// error, and treating it as unset hands back the green run they were
    /// trying to rule out.
    #[test]
    fn an_out_of_dialect_require_value_warns() {
        for typo in ["y", "2", "enabled", " Y "] {
            let mut sink = Vec::new();
            warn_if_out_of_dialect(REQUIRE_ENV, Some(typo), &mut sink);
            let got = String::from_utf8(sink).expect("utf8");
            assert!(got.contains("[WARN]"), "{typo:?} should warn, got {got:?}");
            assert!(got.contains(REQUIRE_ENV), "the warning must name the knob");
        }
    }

    /// ...but a deliberate opt-out, or an absent value, stays silent.
    #[test]
    fn a_falsey_or_absent_require_value_does_not_warn() {
        for quiet in [None, Some(""), Some("  "), Some("0"), Some("off"), Some("NO"), Some("false")] {
            let mut sink = Vec::new();
            warn_if_out_of_dialect(REQUIRE_ENV, quiet, &mut sink);
            assert!(
                sink.is_empty(),
                "{quiet:?} is a deliberate opt-out, not a typo: {:?}",
                String::from_utf8_lossy(&sink)
            );
        }
    }

    // --- the cascade itself: order, short-circuiting, and the built env ---

    fn ok_shim() -> Result<PathBuf, String> {
        Ok(PathBuf::from("/srv/kastellan").join(VENV_SHIM_SUBPATH))
    }
    fn ok_weights() -> Result<PathBuf, String> {
        Ok(PathBuf::from("/w"))
    }

    /// The opt-in flag is reported ahead of everything else, and no probe runs.
    #[test]
    fn the_enable_gate_is_checked_first_and_short_circuits_the_probes() {
        let mut sandbox_ran = false;
        let got = first_unmet_precondition(
            EnableFlag::Checked,
            None,
            || {
                sandbox_ran = true;
                Some("sandbox".to_string())
            },
            || panic!("supervisor probe must not run"),
            || panic!("shim resolution must not run"),
            || panic!("weights resolution must not run"),
        );
        assert!(got.unwrap_err().contains(ENABLE_ENV));
        assert!(!sandbox_ran, "the bwrap-spawning probe must not run");
    }

    /// The sandbox probe precedes the supervisor probe, and stops it.
    #[test]
    fn the_sandbox_probe_precedes_and_short_circuits_the_supervisor_probe() {
        let got = first_unmet_precondition(
            EnableFlag::Ignored,
            None,
            || Some("bwrap probe failed: nope".to_string()),
            || panic!("supervisor probe must not run once sandbox has failed"),
            || panic!("shim resolution must not run"),
            || panic!("weights resolution must not run"),
        );
        assert_eq!(got.unwrap_err(), "bwrap probe failed: nope");
    }

    /// The supervisor probe precedes the two path lookups, and stops them.
    #[test]
    fn the_supervisor_probe_precedes_and_short_circuits_the_path_lookups() {
        let got = first_unmet_precondition(
            EnableFlag::Ignored,
            None,
            || None,
            || Some("supervisor probe failed: no bus".to_string()),
            || panic!("shim resolution must not run"),
            || panic!("weights resolution must not run"),
        );
        assert_eq!(got.unwrap_err(), "supervisor probe failed: no bus");
    }

    /// The venv shim precedes the weights, and stops them.
    #[test]
    fn the_venv_shim_precedes_and_short_circuits_the_weights_lookup() {
        let got = first_unmet_precondition(
            EnableFlag::Ignored,
            None,
            || None,
            || None,
            || Err("shim not built at /x".to_string()),
            || panic!("weights resolution must not run"),
        );
        assert_eq!(got.unwrap_err(), "shim not built at /x");
    }

    /// The weights snapshot is the last precondition, and its reason surfaces.
    #[test]
    fn the_weights_snapshot_is_the_last_precondition() {
        let got = first_unmet_precondition(
            EnableFlag::Ignored,
            None,
            || None,
            || None,
            ok_shim,
            || Err("weights dir missing at /nowhere".to_string()),
        );
        assert_eq!(got.unwrap_err(), "weights dir missing at /nowhere");
    }

    /// All met: both paths come back, in that order.
    ///
    /// The success case is reachable *only* because the probes are injected —
    /// on a real unstaged host it never is, which is exactly the
    /// "unreachable success path proves nothing" trap this split avoids.
    #[test]
    fn a_fully_staged_host_yields_the_shim_and_the_weights() {
        let (shim, weights) = first_unmet_precondition(
            EnableFlag::Checked,
            Some("true".to_string()),
            || None,
            || None,
            ok_shim,
            ok_weights,
        )
        .expect("every precondition met");
        assert_eq!(shim, ok_shim().unwrap());
        assert_eq!(weights, PathBuf::from("/w"));
    }

    /// The tier's constant fields, pinned where they are actually built.
    ///
    /// `use_container_backend` is the load-bearing one: flipping it would route
    /// all three host-mode suites at the container backend, and until this test
    /// nothing observed it. `model_id` matters too — pinning the `MODEL_ID`
    /// constant elsewhere proves the constant, not that this builder uses it.
    #[test]
    fn the_host_env_is_built_host_mode_with_the_pinned_model() {
        // Synthetic binds: the real resolver needs a venv on disk, which is
        // exactly why it is not this function's job.
        let env = host_env_from(ok_shim().unwrap(), PathBuf::from("/w"), (None, vec![]));
        assert!(
            !env.use_container_backend,
            "this is the HOST-mode builder; container mode is a separate tier"
        );
        assert_eq!(env.container_image, None);
        assert_eq!(env.model_id, MODEL_ID);
        assert_eq!(env.device, "auto");
        assert_eq!(env.weights_dir, PathBuf::from("/w"));
        assert_eq!(
            env.venv_dir,
            PathBuf::from("/srv/kastellan/workers/gliner-relex/.venv"),
            "venv_dir must be derived from the shim, not re-assembled"
        );
    }

    /// The shim resolver names the path `install.sh` writes, whether or not
    /// this host has staged it.
    ///
    /// Pins `workspace_root()`'s derivation: drop its `.parent()` and the shim
    /// is looked for under `tests-common/`, a path no host can ever have — every
    /// suite skips forever, with a plausible-looking reason.
    #[test]
    fn the_shim_resolver_looks_under_the_workspace_root() {
        let named = match venv_shim_or_reason() {
            Ok(p) => p.display().to_string(),
            Err(reason) => reason,
        };
        assert!(
            named.contains(VENV_SHIM_SUBPATH),
            "must name the install.sh path, got {named:?}"
        );
        assert!(
            !named.contains("tests-common"),
            "the workspace root must be tests-common's PARENT, got {named:?}"
        );
    }

    /// A relative shim has no venv root, and must say so rather than yielding
    /// an empty path that would be bound into the jail.
    #[test]
    #[should_panic(expected = "has no venv root")]
    fn a_relative_shim_path_is_rejected_rather_than_yielding_an_empty_venv() {
        let _ = venv_dir_of(Path::new("bin/kastellan-worker-gliner-relex"));
    }

    /// The panic must name the knob, so an operator who set it knows which
    /// demand is being reported rather than reading it as a test regression.
    #[test]
    #[should_panic(expected = "KASTELLAN_GLINER_RELEX_REQUIRE_E2E")]
    fn a_demanded_run_panics_naming_the_knob() {
        let _: Option<()> = report_unmet(UnmetAction::Fail, "weights dir missing at /nowhere");
    }

    /// ...and the reason, because "a precondition failed" without saying which
    /// one sends the operator back to `--nocapture` archaeology.
    #[test]
    #[should_panic(expected = "weights dir missing at /nowhere")]
    fn a_demanded_run_panics_naming_the_unmet_precondition() {
        let _: Option<()> = report_unmet(UnmetAction::Fail, "weights dir missing at /nowhere");
    }

    /// The panic flattens a multi-line reason, exactly as the `[SKIP]` line
    /// does.
    ///
    /// The two verdicts must not disagree about what the reason *is*. Both
    /// supervisor backends embed a `\n\n` hint, so without this the demanded
    /// run — the path an operator only reaches deliberately — is the one that
    /// reports the reason worst: a first line that stops before it.
    #[test]
    #[should_panic(expected = "probe failed: no bus Run `loginctl enable-linger`.")]
    fn a_demanded_run_flattens_a_multi_line_reason_into_the_panic() {
        let _: Option<()> = report_unmet(
            UnmetAction::Fail,
            "probe failed: no bus\n\nRun `loginctl\nenable-linger`.",
        );
    }
}
