//! Unit tests for [`DaemonSpec`] (issue #634).
//!
//! Hermetic and pure — no directory is created, no unit is installed, no
//! socket is opened. That matters more here than it usually does:
//! `linux-check.yml` runs `cargo test -p kastellan-tests-common` on every
//! PR, and it is the only target there that reaches this code — the
//! **six** daemon e2es these values configure are DGX-gated and run on no
//! PR at all. So this file is the only CI coverage the bring-up contract
//! has.

use super::*;

/// A spec with every default left alone.
///
/// ⚠️ **Each call returns a spec with a DIFFERENT service name**, since
/// #641 made `new` draw its own `unique_suffix()`. A test comparing a
/// name against itself must therefore bind one spec and reuse it —
/// calling `base_spec()` twice compares two different daemons.
fn base_spec() -> DaemonSpec {
    DaemonSpec::new(
        "gboot",
        Path::new("/tmp/pgdata"),
        LlmEndpoint::Base("http://127.0.0.1:9999".into()),
    )
}

fn built(spec: &DaemonSpec) -> ServiceSpec {
    spec.service_spec(
        Path::new("/nonexistent/kastellan"),
        Path::new("/nonexistent/logs"),
        Path::new("/nonexistent/state"),
    )
}

/// The value the supervisor materialises for `key`.
///
/// `service_spec` collapses duplicates on the way out, so this is the
/// only entry — `rfind` rather than `find` purely so that a regression
/// reintroducing duplicates is read the way the renderer would read
/// them, instead of turning this helper into a second, disagreeing
/// definition of "effective".
fn effective(spec: &ServiceSpec, key: &str) -> Option<String> {
    spec.env
        .iter()
        .rfind(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

/// Every key appears exactly once in a built unit.
fn count_entries(spec: &ServiceSpec, key: &str) -> usize {
    spec.env.iter().filter(|(k, _)| k == key).count()
}

/// `Base` is a base: the compat segment is appended.
#[test]
fn an_llm_base_url_gains_the_compat_segment() {
    let s = built(&base_spec());
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_URL").as_deref(),
        Some("http://127.0.0.1:9999/v1"),
    );
}

/// `Verbatim` is complete: **nothing** is appended.
///
/// This is the one migration hazard in #634 that fails *silently*.
/// The operator-driven callers read `KASTELLAN_LLM_LOCAL_URL` (or
/// `KASTELLAN_MAIL_LIVE_LLM_URL`) from the environment, whose documented
/// value already ends in `/v1` (`http://127.0.0.1:8000/v1`, per
/// `observation_capture`'s own panic message). Routing that through the
/// `Base` arm yields `.../v1/v1`, and because those tests drive a *real*
/// LLM the failure surfaces as an unreachable backend rather than as
/// anything naming the URL.
///
/// Both such callers now reach this variant through
/// [`LlmEndpoint::from_operator_url`] rather than constructing it
/// directly, so the arm is exercised by that constructor's tests too —
/// which matters, because both of those tests are `#[ignore]`d and their
/// call sites are compiled but never executed.
///
/// Deliberately asserts the whole string rather than
/// `!url.ends_with("/v1/v1")`: the second form passes for a URL that
/// gained any other suffix, and the property under test is "used
/// verbatim", not "did not gain this one mistake".
#[test]
fn a_verbatim_llm_url_gains_nothing() {
    let spec = DaemonSpec::new(
        "obs",
        Path::new("/tmp/pgdata"),
        LlmEndpoint::Verbatim("http://127.0.0.1:8000/v1".into()),
    );
    assert_eq!(
        effective(&built(&spec), "KASTELLAN_LLM_LOCAL_URL").as_deref(),
        Some("http://127.0.0.1:8000/v1"),
    );
}

/// A spec that sets nothing extra reproduces what the shared helper did
/// before this type existed — the migration's whole no-behaviour-change
/// claim, in one test.
///
/// ⚠️ **Deliberately LITERALS, not the constants.** Asserting
/// `Some(DEFAULT_LLM_MODEL)` would put the constant on both sides, so
/// editing it moves the expectation with it and the test passes at any
/// value — the same-value-on-both-sides blind spot #633 recorded one
/// crate over. The values pinned here are the ones the pre-#634 helper
/// hardcoded, which is exactly what "behaves identically" means, so a
/// change to any of them is a change to five e2es' boot config (all six
/// but `observation_capture`, which overrides all three) and should have
/// to argue for itself here.
///
/// The `assert_eq!`s against the constants that follow are the other
/// half of that pair: they keep the constants and the literals from
/// diverging, so a reader of either finds one answer.
#[test]
fn the_defaults_are_the_values_the_helper_used_to_hardcode() {
    let s = built(&base_spec());
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_MODEL").as_deref(),
        Some("test-local-model"),
    );
    assert_eq!(effective(&s, "KASTELLAN_LLM_TIMEOUT_MS").as_deref(), Some("5000"));
    assert_eq!(base_spec().ready_timeout_value(), Duration::from_secs(10));

    assert_eq!(DEFAULT_LLM_MODEL, "test-local-model");
    assert_eq!(DEFAULT_LLM_TIMEOUT_MS, "5000");
    assert_eq!(DEFAULT_READY_TIMEOUT, Duration::from_secs(10));
}

/// Each override reaches the unit, and only the one asked for moves.
#[test]
fn each_override_replaces_exactly_its_own_default() {
    let s = built(&base_spec().llm_model("gemma4:26b").llm_timeout_ms("240000"));
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_MODEL").as_deref(),
        Some("gemma4:26b"),
    );
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_TIMEOUT_MS").as_deref(),
        Some("240000"),
    );
    // Untouched by either setter.
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_URL").as_deref(),
        Some("http://127.0.0.1:9999/v1"),
    );
    assert_eq!(
        base_spec().ready_timeout(Duration::from_secs(20)).ready_timeout_value(),
        Duration::from_secs(20),
    );
}

/// `extra_env` is applied LAST, so an entry naming a key the spec
/// already set wins.
///
/// Until #634 this was a comment at a call site with nothing testing it.
/// No caller depends on it *today* — `mail_daemon_e2e`'s two overrides
/// went to the `llm_model` and `force_routing` setters — so this test is
/// what keeps the guarantee true for the caller that next needs it.
/// Asserted through `effective`, i.e. through the same last-wins rule
/// both supervisor backends implement, rather than by counting entries,
/// because what matters is the value the daemon's process environment
/// ends up with.
#[test]
fn extra_env_wins_over_a_default_it_names() {
    let s = built(
        &base_spec()
            .llm_model("ignored-by-the-override")
            .env("KASTELLAN_LLM_LOCAL_MODEL", "the-real-model"),
    );
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_MODEL").as_deref(),
        Some("the-real-model"),
    );
    // ONE entry, not two. `service_spec` collapses duplicates rather
    // than leaving the winner to be decided by the renderer — systemd
    // documents last-wins for a repeated `Environment=`, but launchd
    // gets a plist dict with a duplicate key, whose resolution the
    // format does not define and which nothing in `kastellan-supervisor`
    // tests. Asserting the count is what keeps that collapse from being
    // silently undone.
    assert_eq!(count_entries(&s, "KASTELLAN_LLM_LOCAL_MODEL"), 1);
}

/// The collapse keeps the LAST value, at the LAST position, and leaves
/// every other entry's relative order alone.
///
/// Exercised directly rather than only through `service_spec`, because
/// the position half is invisible there: `effective` reads by key, so a
/// collapse that kept the right value at the *first* position would pass
/// every assertion above while reordering the unit.
#[test]
fn collapsing_duplicates_keeps_the_last_value_in_the_last_position() {
    let mut env = vec![
        ("A".to_string(), "1".to_string()),
        ("B".to_string(), "2".to_string()),
        ("A".to_string(), "3".to_string()),
        ("C".to_string(), "4".to_string()),
    ];
    dedup_last_wins(&mut env);
    assert_eq!(
        env,
        vec![
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "3".to_string()),
            ("C".to_string(), "4".to_string()),
        ],
    );

    // A list with nothing to collapse is returned untouched — the
    // common case, and the one a "collapse everything" bug would eat.
    let mut untouched = vec![
        ("X".to_string(), "1".to_string()),
        ("Y".to_string(), "2".to_string()),
    ];
    let before = untouched.clone();
    dedup_last_wins(&mut untouched);
    assert_eq!(untouched, before);

    // Three of a kind collapse to one, not to two.
    let mut triple = vec![
        ("K".to_string(), "a".to_string()),
        ("K".to_string(), "b".to_string()),
        ("K".to_string(), "c".to_string()),
    ];
    dedup_last_wins(&mut triple);
    assert_eq!(triple, vec![("K".to_string(), "c".to_string())]);
}

/// `envs` adds in order, and composes with `env`.
///
/// The two `A` entries are the point. An earlier draft used two
/// *distinct* keys and a trailing `.env("A", …)`, which could not observe
/// ordering at all: reversing the iteration inside `envs` left the
/// distinct key's value untouched and the trailing `env` masked the
/// other's position, so the mutation survived a test named for order.
#[test]
fn envs_adds_every_entry_in_order() {
    let s = built(&base_spec().envs(vec![
        ("A".to_string(), "1".to_string()),
        ("A".to_string(), "2".to_string()),
        ("B".to_string(), "9".to_string()),
    ]));
    // Reversing `envs`' iteration makes this "1".
    assert_eq!(effective(&s, "A").as_deref(), Some("2"));
    assert_eq!(effective(&s, "B").as_deref(), Some("9"));

    // ...and `env` appends after whatever `envs` added.
    let s = built(&base_spec().envs(vec![("A".to_string(), "1".to_string())]).env("A", "3"));
    assert_eq!(effective(&s, "A").as_deref(), Some("3"));
}

/// The unit name carries the label, then a per-spec suffix.
///
/// Before #641 this pinned a whole literal, because `suffix` was a
/// caller-supplied `&str` sitting next to `label` and a transposition
/// between them was the hazard worth catching. `new` now draws the
/// suffix itself, so there is no transposition left to catch and no
/// literal to pin — what remains is the *shape*: the fixed prefix, the
/// caller's label, and a suffix that is not empty.
#[test]
fn the_service_name_carries_the_label_then_a_suffix() {
    let spec = base_spec();
    let name = spec.service_name();
    let prefix = "kastellan-supervisor-test-core-gboot-";
    assert!(
        name.starts_with(prefix),
        "expected the prefix and label to lead the name, got {name}",
    );
    assert!(
        !name[prefix.len()..].is_empty(),
        "the suffix must not be empty or co-running tests collide: {name}",
    );
}

/// One spec keeps ONE name, and the built unit uses that same name.
///
/// Split from the test above because the two properties failed together
/// when they shared a body: with the suffix drawn in `new`, calling
/// `base_spec()` twice compares two different daemons, and an
/// assertion written that way fails for a reason that has nothing to do
/// with what it means to check.
#[test]
fn the_built_unit_uses_the_specs_own_name() {
    let spec = base_spec();
    assert_eq!(spec.service_name(), spec.service_name());
    assert_eq!(built(&spec).name, spec.service_name());
}

/// Two specs with the SAME label get different names.
///
/// This is the whole reason a suffix exists, and until #641 no test
/// stated it — the suffix was a literal the caller passed, so
/// uniqueness was the caller's problem and silently everyone's. Now
/// that `new` owns it, this is the property that keeps six co-running
/// daemon e2es from installing over each other.
#[test]
fn two_specs_with_one_label_do_not_collide() {
    assert_ne!(base_spec().service_name(), base_spec().service_name());
}

/// The log file names follow the RENAMED unit, not `core_service_spec`'s
/// `CORE_SERVICE_NAME` default.
///
/// `core_service_spec` derives both paths from the production service
/// name; every hand-rolled copy overwrote them after renaming the unit,
/// and a migration that renamed the unit and forgot the logs would leave
/// four co-running daemons appending to one pair of files.
#[test]
fn the_log_paths_follow_the_renamed_unit() {
    // ONE spec: `base_spec()` draws a fresh suffix per call (#641).
    let spec = base_spec();
    let s = built(&spec);
    let name = spec.service_name();
    assert_eq!(
        s.stdout_log,
        Some(Path::new("/nonexistent/logs").join(format!("{name}.out"))),
    );
    assert_eq!(
        s.stderr_log,
        Some(Path::new("/nonexistent/logs").join(format!("{name}.err"))),
    );
}

/// Every key a daemon needs to boot is present.
///
/// Spelled out rather than derived, so *dropping* one during the
/// migration fails here. The daemon's own failure for a missing
/// `KASTELLAN_PROMPTS_DIR` is a fail-closed abort with an empty stdout,
/// which is the shape that costs a session to diagnose.
#[test]
fn the_common_key_set_is_complete() {
    let s = built(&base_spec());
    for key in [
        "KASTELLAN_DATA_DIR",
        "USER",
        "KASTELLAN_STATE_DIR",
        "KASTELLAN_PROMPTS_DIR",
        "KASTELLAN_LLM_LOCAL_URL",
        "KASTELLAN_LLM_LOCAL_MODEL",
        "KASTELLAN_LLM_TIMEOUT_MS",
    ] {
        assert!(
            effective(&s, key).is_some(),
            "a booting daemon needs {key}; the spec did not set it",
        );
    }
    // Read back by VALUE, not by presence. `KASTELLAN_DATA_DIR` and
    // `KASTELLAN_STATE_DIR` are both path-shaped and both pushed in the
    // same block, so presence alone would survive a swap between them.
    assert_eq!(effective(&s, "KASTELLAN_DATA_DIR").as_deref(), Some("/tmp/pgdata"));
    assert_eq!(effective(&s, "KASTELLAN_STATE_DIR").as_deref(), Some("/nonexistent/state"));

    // `USER` is derived by `new` since #641, so there is no caller
    // literal left to compare against. Comparing it to
    // `current_username()` would put the same helper on both sides and
    // pass at any value — the #633 failure shape. Cross-check it
    // against the environment itself instead: when `$USER` is set, that
    // is what the daemon must run as. (`current_username` falls back to
    // `whoami` and then to `"kastellan"`, so a host without `$USER`
    // still gets a value — hence the guard rather than an unconditional
    // assert.)
    let user = effective(&s, "USER").expect("asserted present above");
    assert!(!user.is_empty(), "the daemon needs a non-empty USER");
    if let Ok(env_user) = std::env::var("USER") {
        if !env_user.is_empty() {
            assert_eq!(
                user, env_user,
                "the spec's USER must be the user this process runs as",
            );
        }
    }

    // The prompts dir is DERIVED, so presence is not enough: a dropped
    // `.parent()` yields `tests-common/prompts` and a typo'd `join`
    // yields anything at all, both of which are present and both of
    // which abort the daemon at boot with an empty stdout — the failure
    // this test's doc names as costing a session. Checking the directory
    // exists is a read, not a write, so the module stays pure.
    let prompts = effective(&s, "KASTELLAN_PROMPTS_DIR").expect("asserted present above");
    assert!(
        prompts.ends_with("/prompts"),
        "the prompts dir must be a `prompts` directory, got {prompts}",
    );
    assert!(
        Path::new(&prompts).is_dir(),
        "the prompts dir must actually exist or the daemon fails closed at boot; got {prompts}",
    );
}

/// The binary reaches `ServiceSpec.program`.
///
/// `service_spec` takes three adjacent `&Path`, and the other two are
/// each pinned by a test above (`core_log_dir` through `stdout_log`,
/// `state_dir` through `KASTELLAN_STATE_DIR`). This one closes the
/// triple: without it, transposing `binary` and `core_log_dir` at the
/// `core_service_spec` call survives every test in this file, because
/// `stdout_log`/`stderr_log` are overwritten from `core_log_dir` two
/// lines later and leave the swap no trace. The daemon would then be
/// installed with a directory as its `ExecStart`, failing all six e2es
/// on the DGX and none of CI.
#[test]
fn the_binary_reaches_the_units_program() {
    assert_eq!(
        built(&base_spec()).program,
        Path::new("/nonexistent/kastellan"),
    );
}

/// `core_service_spec`'s own env survives — the force-routing default in
/// particular, which is a containment control rather than a convenience.
#[test]
fn the_inherited_force_routing_default_is_not_dropped() {
    assert_eq!(
        effective(&built(&base_spec()), "KASTELLAN_EGRESS_FORCE_ROUTING").as_deref(),
        Some("1"),
    );
}

/// A label long enough to breach the supervisor's name cap is refused
/// loudly, at the line that supplied the bad label.
///
/// The panic now comes from `kastellan_supervisor::validate_service_name`
/// rather than from a hand-rolled `len() <= 200` (#642), so the expected
/// text is the supervisor's own — which is what makes it impossible for
/// this test to keep passing against a cap the supervisor has changed.
#[test]
#[should_panic(expected = "service name longer than 200 chars")]
fn an_overlong_service_name_panics_at_construction() {
    let _ = DaemonSpec::new(
        "x".repeat(250),
        Path::new("/tmp/pgdata"),
        LlmEndpoint::Base("http://127.0.0.1:1".into()),
    );
}

/// **The half the hand-copy could not see.**
///
/// A label carrying a character outside `[A-Za-z0-9._-]` passed the old
/// `len() <= 200` assert untouched and died much later inside
/// `sup.install(&spec)`, by which point the failure names a service name
/// rather than the label that produced it. Unlike the length rule, this
/// one is reachable by an ordinary mistake: a label with a space, a
/// slash or a colon is exactly what someone types.
#[test]
#[should_panic(expected = "illegal character")]
fn a_label_with_an_illegal_character_panics_at_construction() {
    let _ = DaemonSpec::new(
        "has space",
        Path::new("/tmp/pgdata"),
        LlmEndpoint::Base("http://127.0.0.1:1".into()),
    );
}

/// The gate is applied to the **service name**, not to the label.
///
/// Found by mutating the check to `validate_service_name(&spec.label)`,
/// which survives both `#[should_panic]` tests above: a 250-char label
/// and a label with a space are each illegal on their own, so checking
/// the label alone still panics for both. What separates the two is a
/// label that is *legal in isolation* but whose full name is not —
/// `kastellan-supervisor-test-core-` (31) plus the label plus a
/// ~28-char suffix, so a 163-char label is fine and its name is not.
///
/// This matters beyond the mutant: the label is a fragment and the name
/// is what reaches the filesystem, so a check on the fragment is the
/// wrong check even when it happens to fire.
#[test]
#[should_panic(expected = "service name longer than 200 chars")]
fn the_cap_is_applied_to_the_whole_name_not_just_the_label() {
    let label = "x".repeat(163);
    assert!(
        kastellan_supervisor::validate_service_name(&label).is_ok(),
        "the label must be legal ON ITS OWN or this test proves nothing",
    );
    let _ = DaemonSpec::new(
        label,
        Path::new("/tmp/pgdata"),
        LlmEndpoint::Base("http://127.0.0.1:1".into()),
    );
}

/// A label the supervisor accepts must not be refused.
///
/// The accepting arm of the same gate — without it, a mutant that
/// panics unconditionally in `new` passes both `#[should_panic]` tests
/// above [[unreachable-success-path-proves-nothing]]. `base_spec()`
/// exercising it everywhere else is coverage by accident; this states
/// it.
#[test]
fn a_legal_label_constructs() {
    let spec = DaemonSpec::new(
        "l3pyrun",
        Path::new("/tmp/pgdata"),
        LlmEndpoint::Base("http://127.0.0.1:1".into()),
    );
    kastellan_supervisor::validate_service_name(&spec.service_name())
        .expect("a spec that constructed must name an installable unit");
}

/// `from_operator_url` normalises both shapes to exactly one compat
/// segment.
///
/// This is the constructor for a URL whose shape the caller cannot know
/// because an *operator* supplied it. Both rows below reached the daemon
/// as `…/v1` before #634, via a `strip_suffix("/v1")` at the call site
/// plus the old helper's unconditional append; the migration replaced
/// that pair with a bare `Verbatim` and silently dropped the bare-base
/// row, which is what this test exists to stop happening again.
///
/// The bare form is not hypothetical: `OLLAMA_LLM_URL` in
/// `core/src/install/plan.rs` is `http://127.0.0.1:11434`, so it is the
/// value an operator is most likely to copy.
#[test]
fn an_operator_url_normalises_to_exactly_one_compat_segment() {
    for input in [
        "http://127.0.0.1:11434",
        "http://127.0.0.1:11434/",
        "http://127.0.0.1:11434/v1",
        "http://127.0.0.1:11434/v1/",
    ] {
        let spec = DaemonSpec::new(
            "op",
            Path::new("/tmp/pgdata"),
            LlmEndpoint::from_operator_url(input),
        );
        assert_eq!(
            effective(&built(&spec), "KASTELLAN_LLM_LOCAL_URL").as_deref(),
            Some("http://127.0.0.1:11434/v1"),
            "operator URL {input} must normalise to exactly one {COMPAT_SEGMENT}",
        );
    }
}

/// A base merely *ending* in the characters `v1` is still a base.
///
/// `ends_with("/v1")` rather than `ends_with("v1")`: the second would
/// read `http://host/apiv1` as already-complete and dial a different
/// server entirely. `llm-router`'s `props_url` documents having needed
/// exactly this distinction.
#[test]
fn a_base_ending_in_v1_without_the_separator_is_not_complete() {
    assert_eq!(
        LlmEndpoint::from_operator_url("http://127.0.0.1:8000/apiv1"),
        LlmEndpoint::Base("http://127.0.0.1:8000/apiv1".to_string()),
    );
}

/// A `Base` that already carries `/v1` is refused, not silently doubled.
///
/// The symmetric half of `a_verbatim_llm_url_gains_nothing`. Without
/// this, `LlmEndpoint::Base("http://h:8000/v1")` compiles, boots, and
/// dials `http://h:8000/v1/v1` — and `RouterError::HttpStatus` carries
/// the status and body but never the URL, so the operator sees a 404
/// naming nothing. No current caller does this; the assert is what keeps
/// the next one from doing it in a test that runs on no PR.
#[test]
#[should_panic(expected = "must not already carry /v1")]
fn a_base_that_already_carries_the_compat_segment_is_refused() {
    let spec = DaemonSpec::new(
        "dbl",
        Path::new("/tmp/pgdata"),
        LlmEndpoint::Base("http://127.0.0.1:8000/v1".into()),
    );
    let _ = built(&spec);
}

/// `force_routing(false)` is the ONLY way a spec turns the containment
/// control off, and it does so through a key that outranks the inherited
/// default.
///
/// Asserted through `effective` rather than by looking for the entry,
/// because `core_service_spec`'s `1` is still in the vec — what decides
/// the daemon's behaviour is which of the two the backend renders last.
#[test]
fn force_routing_off_overrides_the_inherited_default() {
    let s = built(&base_spec().force_routing(false));
    assert_eq!(
        effective(&s, "KASTELLAN_EGRESS_FORCE_ROUTING").as_deref(),
        Some("0"),
    );

    // `true` is a no-op: the inherited `1` already says so, and a second
    // entry restating it would be noise in the unit.
    let on = built(&base_spec().force_routing(true));
    assert_eq!(
        effective(&on, "KASTELLAN_EGRESS_FORCE_ROUTING").as_deref(),
        Some("1"),
    );
    assert_eq!(
        count_entries(&on, "KASTELLAN_EGRESS_FORCE_ROUTING"),
        1,
        "force_routing(true) must not add a second entry",
    );
    // ...and the opt-out leaves one entry too, not the inherited `1`
    // beside it. On macOS the two-entry form would leave a containment
    // control's value to an undefined plist-dict resolution.
    assert_eq!(count_entries(&s, "KASTELLAN_EGRESS_FORCE_ROUTING"), 1);
}

/// The daemon e2es script classic-shaped plan replies, so the spec pins
/// `classic` since the daemon's default became the notebook (#922 slice 4);
/// a test may still choose the notebook, last value winning.
#[test]
fn the_planner_mode_is_pinned_to_classic_and_overridable() {
    let s = built(&base_spec());
    assert_eq!(effective(&s, "KASTELLAN_PLANNER_MODE").as_deref(), Some("classic"));
    let nb = built(&base_spec().env("KASTELLAN_PLANNER_MODE", "notebook"));
    assert_eq!(effective(&nb, "KASTELLAN_PLANNER_MODE").as_deref(), Some("notebook"));
    assert_eq!(count_entries(&nb, "KASTELLAN_PLANNER_MODE"), 1);
}
