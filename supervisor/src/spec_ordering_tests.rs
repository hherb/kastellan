use super::*;

#[test]
fn target_spec_holds_name_and_ordered_members() {
    let t = TargetSpec {
        name: "kastellan".into(),
        members: vec!["kastellan-postgres".into(), "kastellan-core".into()],
    };
    assert_eq!(t.name, "kastellan");
    assert_eq!(t.members, vec!["kastellan-postgres", "kastellan-core"]);
}

#[test]
fn service_spec_ordering_fields_default_when_absent() {
    // Proves #[serde(default)] supplies empty/None when the JSON omits
    // the new ordering fields — i.e. an old serialised spec still loads.
    let json = r#"{
        "name": "svc",
        "program": "/bin/true",
        "args": [],
        "env": [],
        "working_dir": null,
        "keep_alive": false,
        "stdout_log": null,
        "stderr_log": null
    }"#;
    let s: ServiceSpec = serde_json::from_str(json).expect("deserialize without ordering fields");
    assert!(s.after.is_empty());
    assert!(s.part_of.is_none());
    assert!(s.restart_backoff.is_none());
}

#[test]
fn a_stale_environment_file_key_is_refused_rather_than_defaulted() {
    // #532. #528 renamed `environment_file: Option<PathBuf>` to
    // `environment_files: Vec<EnvFileRef>`. Without
    // `deny_unknown_fields` a spec carrying the OLD key deserializes
    // cleanly: the unknown field is dropped, `environment_files` defaults
    // to empty, and the core daemon is installed with no environment file
    // and no error — #458's failure arriving through the serde layer.
    //
    // Latent today (nothing persists a `ServiceSpec`; it is code-built in
    // `supervisor::specs` and `core::install::plan`), which is exactly why
    // the guard is worth having before something starts persisting one.
    let json = r#"{
        "name": "svc",
        "program": "/bin/true",
        "args": [],
        "env": [],
        "working_dir": null,
        "keep_alive": false,
        "stdout_log": null,
        "stderr_log": null,
        "environment_file": "/home/u/.config/kastellan/kastellan.env"
    }"#;
    let err = serde_json::from_str::<ServiceSpec>(json)
        .expect_err("a renamed field must not silently default to empty");
    assert!(
        err.to_string().contains("environment_file"),
        "the error must name the offending key: {err}"
    );
}

#[test]
fn env_file_ref_round_trips_and_refuses_an_unknown_key() {
    // The `optional` flag is load-bearing in both directions — it renders
    // systemd's `-` prefix and tells the launchd backend to skip a missing
    // overlay rather than hard-error — so pin the round trip, and pin that
    // a misspelling of it cannot silently become `false` (which would turn
    // the normally-absent overlay into a failed install).
    let refs = vec![
        EnvFileRef { path: PathBuf::from("/home/u/.config/kastellan/kastellan.env"), optional: false },
        EnvFileRef { path: PathBuf::from("/home/u/.config/kastellan/kastellan.env.local"), optional: true },
    ];
    let json = serde_json::to_string(&refs).expect("serialize");
    let back: Vec<EnvFileRef> = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, refs);

    let err = serde_json::from_str::<EnvFileRef>(r#"{"path":"/a","optionl":true}"#)
        .expect_err("a misspelled `optional` must not silently become false");
    assert!(err.to_string().contains("optionl"), "{err}");
}

#[test]
fn service_spec_restart_backoff_round_trips() {
    let s = ServiceSpec {
        name: "svc".into(),
        program: std::path::PathBuf::from("/bin/true"),
        args: vec![],
        env: vec![],
        working_dir: None,
        keep_alive: true,
        stdout_log: None,
        stderr_log: None,
        after: vec![],
        part_of: None,
        restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
        environment_files: Vec::new(),
    };
    let json = serde_json::to_string(&s).expect("serialize");
    let back: ServiceSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        back.restart_backoff,
        Some(RestartBackoff { max_delay_sec: 300, steps: 8 })
    );
}
