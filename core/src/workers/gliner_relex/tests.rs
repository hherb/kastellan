//! Unit tests for the `gliner_relex` worker module.
//!
//! `use super::*` resolves to the parent `gliner_relex` module per the
//! Rust 2018 sibling-directory module pattern. Integration tests that hit
//! a real PG cluster / live GLiNER worker live in
//! `core/tests/gliner_relex_e2e.rs`.

use super::*;

#[test]
fn extract_request_serialises_with_expected_keys() {
    let req = ExtractRequest {
        text: "Smith treats asthma.".to_string(),
        entity_labels: vec!["person".to_string(), "disease".to_string()],
        relation_labels: vec!["treats".to_string()],
        threshold: Some(0.5),
        relation_threshold: Some(0.5),
        max_entities: Some(64),
    };
    let v = serde_json::to_value(&req).unwrap();
    let obj = v.as_object().unwrap();
    let keys: std::collections::BTreeSet<&str> =
        obj.keys().map(|s| s.as_str()).collect();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from([
            "text",
            "entity_labels",
            "relation_labels",
            "threshold",
            "relation_threshold",
            "max_entities",
        ]),
    );
}

#[test]
fn extract_request_omits_optional_fields_when_none() {
    let req = ExtractRequest {
        text: "x".to_string(),
        entity_labels: vec!["x".to_string()],
        relation_labels: vec![],
        threshold: None,
        relation_threshold: None,
        max_entities: None,
    };
    let v = serde_json::to_value(&req).unwrap();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("threshold"));
    assert!(!obj.contains_key("relation_threshold"));
    assert!(!obj.contains_key("max_entities"));
}

#[test]
fn extract_response_round_trips_real_wire_shape() {
    // Sampled from the operator smoke test of 2026-05-18 against
    // real `knowledgator/gliner-relex-multi-v1.0` weights — the
    // shape that landed the install.sh + README fix (commit
    // `1c36f56`). Nested head/tail use `type` (not `label`) +
    // `entity_idx`; no nested `score`.
    let canned = serde_json::json!({
        "entities": [
            {"text": "Dr Smith", "label": "person",  "start": 0,  "end": 8,  "score": 0.999},
            {"text": "asthma",   "label": "disease", "start": 16, "end": 22, "score": 0.999}
        ],
        "triples":  [
            {
                "head":     {"text": "Dr Smith", "type": "person",  "start": 0,  "end": 8,  "entity_idx": 0},
                "tail":     {"text": "asthma",   "type": "disease", "start": 16, "end": 22, "entity_idx": 1},
                "relation": "treats",
                "score":    0.995
            }
        ],
    });
    let resp: ExtractResponse =
        serde_json::from_value(canned.clone()).expect("decode real wire shape");
    assert_eq!(resp.entities.len(), 2);
    assert_eq!(resp.entities[0].text, "Dr Smith");
    assert_eq!(resp.entities[0].label, "person");
    assert_eq!(resp.triples[0].head.text, "Dr Smith");
    // CRITICAL: nested head/tail use `type`, not `label`. If a
    // future refactor renames `TripleEntity::r#type` to `label`,
    // this assertion would still compile but the from_value above
    // would fail to decode.
    assert_eq!(resp.triples[0].head.r#type, "person");
    assert_eq!(resp.triples[0].head.entity_idx, 0);
    assert_eq!(resp.triples[0].relation, "treats");
    // Round-trip back through Rust types is shape-identical
    // (`PartialEq` on the structs). We don't compare against the
    // raw `canned` Value: f32 → JSON Number → f32 widens through
    // the json::Number f64 carrier (`0.999_f32` round-trips as
    // `0.9990000128746033`), which is a serde_json artifact, not
    // a real shape drift. The decode-then-decode equality below
    // catches every field-rename or field-add bug we care about.
    let re_serialised = serde_json::to_value(&resp).unwrap();
    let resp_again: ExtractResponse = serde_json::from_value(re_serialised).unwrap();
    assert_eq!(resp, resp_again);
}

#[test]
fn label_caps_match_python_side() {
    // Pinned at the values used by the Python validators (see
    // workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py
    // MAX_TEXT_BYTES / MAX_ENTITY_LABELS / MAX_RELATION_LABELS).
    // A drift here would let the Rust caller generate inputs the
    // Python side immediately rejects with INVALID_INPUT.
    assert_eq!(MAX_ENTITY_LABELS, 64);
    assert_eq!(MAX_RELATION_LABELS, 64);
    assert_eq!(MAX_TEXT_BYTES, 8192);
}

/// Shared test fixture: a GlinerRelexEnv pointing at /tmp paths
/// that won't actually be touched (the manifest constructor is
/// pure — no filesystem access). Path strings are visible in
/// assertions below so a refactor that changes them gets caught.
fn test_env() -> GlinerRelexEnv {
    GlinerRelexEnv {
        script_path: PathBuf::from("/tmp/fake/.venv/bin/hhagent-worker-gliner-relex"),
        venv_dir: PathBuf::from("/tmp/fake/.venv"),
        weights_dir: PathBuf::from("/tmp/fake/weights/multi-v1.0"),
        model_id: "knowledgator/gliner-relex-multi-v1.0".to_string(),
        device: "auto".to_string(),
        use_container_backend: false,
        container_image: None,
    }
}

#[test]
fn entry_carries_idle_timeout_lifecycle_with_spec_caps() {
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    match entry.lifecycle {
        Lifecycle::IdleTimeout { caps, contract } => {
            assert!(
                contract.stateless,
                "must declare stateless=true for idle_timeout"
            );
            assert_eq!(caps.idle_seconds, 600);
            assert_eq!(caps.max_requests, 10_000);
            assert_eq!(caps.max_age_seconds, 86_400);
            assert_eq!(caps.grace_period_seconds, 5);
        }
        Lifecycle::SingleUse => panic!("expected IdleTimeout, got SingleUse"),
    }
}

#[test]
fn entry_disables_per_request_kill_switches_for_warm_worker() {
    // The two knobs that are *deliberately* off for warm workers
    // — see the design spec + the per-field rationale on
    // gliner_relex_entry. Pinning here so a future "harden the
    // worker" pass doesn't quietly re-enable either without an
    // explicit revisit of the lifecycle semantics.
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    assert_eq!(
        entry.policy.cpu_ms, 0,
        "cpu_ms must be 0; RLIMIT_CPU is cumulative and would fire across many warm calls"
    );
    assert!(
        entry.wall_clock_ms.is_none(),
        "wall_clock_ms must be None; lifecycle.max_age_seconds is the rotation budget"
    );
}

#[test]
fn entry_denies_network() {
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    match entry.policy.net {
        Net::Deny => {}
        other => panic!("expected Net::Deny, got {other:?}"),
    }
}

#[test]
fn entry_uses_strict_profile() {
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    match entry.policy.profile {
        Profile::WorkerStrict => {}
        other => panic!("expected Profile::WorkerStrict, got {other:?}"),
    }
}

#[test]
fn entry_mounts_weights_and_venv_and_src_read_only_no_writes() {
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    assert!(
        entry.policy.fs_read.contains(&env.weights_dir),
        "weights dir must be in fs_read so the model can load"
    );
    assert!(
        entry.policy.fs_read.contains(&env.venv_dir),
        "venv dir must be in fs_read so the Python interpreter + site-packages are visible"
    );
    // Editable-install source dir, computed as <worker_dir>/src
    // where <worker_dir> == venv_dir.parent(). The venv ships a
    // `.pth` file that points Python here; without the mount, the
    // worker fails to import its own package inside the sandbox.
    let expected_src = env
        .venv_dir
        .parent()
        .expect("test_env venv_dir has a parent")
        .join("src");
    assert!(
        entry.policy.fs_read.contains(&expected_src),
        "editable-install src dir must be in fs_read; got {:?}",
        entry.policy.fs_read
    );
    assert!(
        entry.policy.fs_write.is_empty(),
        "stateless worker writes nothing; fs_write must stay empty"
    );
}

#[test]
fn entry_carries_offline_and_routing_env_vars() {
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    // Build a map view; the order in the Vec<(K, V)> is incidental.
    let env_map: std::collections::HashMap<&str, &str> = entry
        .policy
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    assert_eq!(env_map.get("HF_HUB_OFFLINE"), Some(&"1"));
    assert_eq!(env_map.get("TRANSFORMERS_OFFLINE"), Some(&"1"));
    assert_eq!(
        env_map.get("HHAGENT_GLINER_RELEX_MODEL"),
        Some(&env.model_id.as_str())
    );
    assert_eq!(
        env_map.get("HHAGENT_GLINER_RELEX_DEVICE"),
        Some(&env.device.as_str())
    );
    // The weights path is plumbed via env so the worker's
    // __main__.py knows where to load from. Compare the stringified
    // form because the policy env stores `String`, not `PathBuf`.
    let expected_weights = env.weights_dir.to_string_lossy().into_owned();
    assert_eq!(
        env_map.get("HHAGENT_GLINER_RELEX_WEIGHTS_DIR"),
        Some(&expected_weights.as_str())
    );
    // USER + TORCHINDUCTOR_CACHE_DIR are sandbox-hygiene shims
    // that keep PyTorch's _dynamo import from blowing up on the
    // missing /etc/passwd. See the long comment on
    // gliner_relex_entry for the failure mode they avoid.
    assert!(
        env_map.contains_key("USER"),
        "USER env var must be set; otherwise getpass.getuser() in torch._dynamo crashes on missing /etc/passwd"
    );
    assert_eq!(
        env_map.get("TORCHINDUCTOR_CACHE_DIR"),
        Some(&"/tmp/torchinductor")
    );
}

#[test]
fn entry_forwards_device_verbatim_regardless_of_value() {
    // The manifest is the cross-platform layer; per-platform
    // device-legality enforcement lives in
    // `workers/gliner-relex/.../__main__._resolve_device`. The
    // manifest's job is only to forward whatever the operator
    // (or `auto`-resolution upstream) chose into
    // `HHAGENT_GLINER_RELEX_DEVICE` so the Python startup path
    // sees it. Pinning the forwarding of `"mps"` here so a future
    // refactor that adds platform branches to gliner_relex_entry
    // — moving validation out of Python — has to update this
    // test deliberately. (The macOS slice 2026-05-21 explicitly
    // chose not to add platform branches at the manifest layer
    // to keep the Rust manifest one-shape across Linux + darwin
    // and centralise the per-platform device rules in one place.)
    for device in &["auto", "cpu", "cuda", "mps", "unknown-future-device"] {
        let env = GlinerRelexEnv {
            device: device.to_string(),
            ..test_env()
        };
        let entry = gliner_relex_entry(&env);
        let env_map: std::collections::HashMap<&str, &str> = entry
            .policy
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            env_map.get("HHAGENT_GLINER_RELEX_DEVICE"),
            Some(device),
            "manifest must forward device={device:?} verbatim into the env",
        );
    }
}

#[test]
fn entry_sets_cgroup_ceilings_for_warm_inference() {
    // cpu_quota_pct=400 (4 CPUs) and tasks_max=64 are
    // worker-specific defaults; explicit pin so a global default
    // tweak doesn't silently widen what the gliner-relex worker
    // gets.
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    assert_eq!(entry.policy.cpu_quota_pct, Some(400));
    assert_eq!(entry.policy.tasks_max, Some(64));
    assert_eq!(
        entry.policy.mem_mb, 4_096,
        "4 GiB sized for multi-v1.0; large-v0.5 operators must bump"
    );
}

#[test]
fn entry_binary_points_at_the_venv_shim() {
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    assert_eq!(entry.binary, env.script_path);
}

/// Pin the host-mode shape stays byte-equivalent to today:
/// container_image must be None on a host-mode entry (the existing 7
/// `entry_*` tests are the regression pin for everything else;
/// this one adds the new-field default to the suite).
#[test]
fn entry_host_mode_container_image_is_none() {
    let env = test_env();
    assert!(!env.use_container_backend, "test_env defaults to host mode");
    let entry = gliner_relex_entry(&env);
    assert!(
        entry.container_image.is_none(),
        "host-mode entry must have container_image == None; got {:?}",
        entry.container_image
    );
    assert!(
        entry.sandbox_backend.is_none(),
        "host-mode entry must have sandbox_backend == None; got {:?}",
        entry.sandbox_backend
    );
}

/// Container-mode entry emits the in-container binary path, mounts
/// only `weights_dir` (venv + src baked into image), and populates
/// sandbox_backend + container_image.
///
/// macOS-only: container mode is gated to macOS (issue #144) — the
/// `SandboxBackendKind::Container` variant and `gliner_relex_entry`'s
/// container branch don't exist on Linux.
#[cfg(target_os = "macos")]
#[test]
fn entry_container_mode_emits_in_container_binary_and_weights_only_fs_read() {
    let env = GlinerRelexEnv {
        use_container_backend: true,
        ..test_env()
    };
    let entry = gliner_relex_entry(&env);

    assert_eq!(
        entry.binary,
        PathBuf::from("/usr/local/bin/hhagent-worker-gliner-relex"),
        "container-mode binary must be the in-container shim path"
    );
    assert_eq!(
        entry.policy.fs_read,
        vec![env.weights_dir.clone()],
        "container-mode fs_read must contain ONLY weights_dir (venv + src baked into image)"
    );
    assert_eq!(
        entry.sandbox_backend,
        Some(hhagent_sandbox::SandboxBackendKind::Container),
    );
    assert_eq!(
        entry.container_image.as_deref(),
        Some("hhagent/gliner-relex:dev"),
        "container_image defaults to CONTAINER_IMAGE_DEFAULT when env override absent"
    );
}

/// Operator-supplied image tag (HHAGENT_GLINER_RELEX_IMAGE) flows
/// through GlinerRelexEnv.container_image into the entry.
///
/// macOS-only: see issue #144 — container mode is gated to macOS.
#[cfg(target_os = "macos")]
#[test]
fn entry_container_mode_honours_custom_image_tag() {
    let env = GlinerRelexEnv {
        use_container_backend: true,
        container_image: Some("hhagent/gliner-relex:v0.0.1".to_string()),
        ..test_env()
    };
    let entry = gliner_relex_entry(&env);
    assert_eq!(
        entry.container_image.as_deref(),
        Some("hhagent/gliner-relex:v0.0.1"),
        "operator-supplied image tag must flow into entry.container_image"
    );
}

// ---- resolve_env unit tests --------------------------------------
//
// The resolver is the pure core wrapped by `GlinerRelexManifest::resolve`.
// Tests pass in-memory env-var + filesystem fakes so every skip-register
// branch is reachable without touching the process environment or the
// real filesystem. Production behaviour is exercised by the e2e tests
// in `core/tests/gliner_relex_e2e.rs`.

use std::collections::{HashMap, HashSet};

/// Build an env-lookup closure backed by a fixed map.
fn env_map_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// Match-anything fs predicate (every path exists / is a dir).
fn always_true(_: &Path) -> bool {
    true
}

/// Match-nothing fs predicate (no path exists / is a dir).
fn always_false(_: &Path) -> bool {
    false
}

#[test]
fn resolve_env_disabled_when_enable_unset() {
    let env = env_map_of(&[]);
    let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
    assert_eq!(r, Err(ResolveSkipReason::Disabled));
}

#[test]
fn resolve_env_disabled_when_enable_is_zero_or_truthy_alias() {
    for v in ["0", "true", "yes", "on", ""] {
        let env = env_map_of(&[("HHAGENT_GLINER_RELEX_ENABLE", v)]);
        let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
        assert_eq!(
            r,
            Err(ResolveSkipReason::Disabled),
            "enable={v:?} must be Disabled (strict on the value, only \"1\" enables)"
        );
    }
}

#[test]
fn resolve_env_trims_whitespace_on_enable() {
    // Common operator footgun: `echo "1" > /etc/hhagent/env` yields
    // a value ending in `\n`. The README documents `=1` but trimming
    // is cheap insurance.
    let env = env_map_of(&[
        ("HHAGENT_GLINER_RELEX_ENABLE", " 1\n"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
        ("HHAGENT_DATA_DIR", "/srv/data"),
    ]);
    let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
    assert!(
        r.is_ok(),
        "trimmed \" 1\\n\" must be accepted, got {r:?}"
    );
}

#[test]
fn resolve_env_returns_weights_env_missing() {
    let env = env_map_of(&[("HHAGENT_GLINER_RELEX_ENABLE", "1")]);
    let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
    assert_eq!(r, Err(ResolveSkipReason::WeightsDirEnvMissing));
}

#[test]
fn resolve_env_returns_weights_dir_not_a_dir() {
    let env = env_map_of(&[
        ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/missing"),
        ("HHAGENT_DATA_DIR", "/srv/data"),
    ]);
    let r = resolve_env(|k| env.get(k).cloned(), always_false, always_true);
    match r {
        Err(ResolveSkipReason::WeightsDirNotADir { path }) => {
            assert_eq!(path, PathBuf::from("/srv/missing"));
        }
        other => panic!("expected WeightsDirNotADir, got {other:?}"),
    }
}

#[test]
fn resolve_env_returns_venv_unresolvable_when_no_anchor() {
    // Enable + weights set + dir exists, but none of the three venv
    // anchors set. Pre-refactor this would silently fall through to
    // `/tmp/.local/share/hhagent/...`; now it surfaces a structured
    // skip reason so the operator log says exactly what's missing.
    let env = env_map_of(&[
        ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
    ]);
    let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
    assert_eq!(r, Err(ResolveSkipReason::VenvDirUnresolvable));
}

#[test]
fn resolve_env_returns_script_shim_missing() {
    // Weights dir exists but the venv shim doesn't (operator
    // staged the weights but forgot `uv sync`).
    let env = env_map_of(&[
        ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
        ("HHAGENT_GLINER_RELEX_VENV_DIR", "/opt/glr/.venv"),
    ]);
    // weights dir is a dir; script doesn't exist.
    let r = resolve_env(
        |k| env.get(k).cloned(),
        |p| p == Path::new("/srv/weights"),
        always_false,
    );
    match r {
        Err(ResolveSkipReason::ScriptShimMissing { path }) => {
            assert_eq!(
                path,
                PathBuf::from("/opt/glr/.venv/bin/hhagent-worker-gliner-relex")
            );
        }
        other => panic!("expected ScriptShimMissing, got {other:?}"),
    }
}

#[test]
fn resolve_env_happy_path_explicit_venv_dir_wins() {
    // Explicit `HHAGENT_GLINER_RELEX_VENV_DIR` must override the
    // `HHAGENT_DATA_DIR`-derived default, even when both are set.
    let env = env_map_of(&[
        ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
        ("HHAGENT_GLINER_RELEX_VENV_DIR", "/opt/explicit/.venv"),
        ("HHAGENT_DATA_DIR", "/srv/data"),
    ]);
    let exists_paths: HashSet<PathBuf> = ["/srv/weights", "/opt/explicit/.venv/bin/hhagent-worker-gliner-relex"]
        .iter()
        .map(PathBuf::from)
        .collect();
    let r = resolve_env(
        |k| env.get(k).cloned(),
        |p| exists_paths.contains(p),
        |p| exists_paths.contains(p),
    )
    .expect("happy path");
    assert_eq!(r.venv_dir, PathBuf::from("/opt/explicit/.venv"));
    assert_eq!(
        r.script_path,
        PathBuf::from("/opt/explicit/.venv/bin/hhagent-worker-gliner-relex")
    );
    assert_eq!(r.weights_dir, PathBuf::from("/srv/weights"));
    assert_eq!(r.model_id, "knowledgator/gliner-relex-multi-v1.0");
    assert_eq!(r.device, "auto");
}

#[test]
fn resolve_env_happy_path_uses_hhagent_data_dir() {
    let env = env_map_of(&[
        ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
        ("HHAGENT_DATA_DIR", "/srv/data"),
        ("HHAGENT_GLINER_RELEX_MODEL", "knowledgator/gliner-relex-large-v0.5"),
        ("HHAGENT_GLINER_RELEX_DEVICE", "cuda"),
    ]);
    let exists_paths: HashSet<PathBuf> = [
        "/srv/weights",
        "/srv/data/workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    let r = resolve_env(
        |k| env.get(k).cloned(),
        |p| exists_paths.contains(p),
        |p| exists_paths.contains(p),
    )
    .expect("happy path");
    assert_eq!(r.venv_dir, PathBuf::from("/srv/data/workers/gliner-relex/.venv"));
    assert_eq!(r.model_id, "knowledgator/gliner-relex-large-v0.5");
    assert_eq!(r.device, "cuda");
}

#[test]
fn resolve_env_happy_path_home_fallback_when_no_data_dir() {
    let env = env_map_of(&[
        ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
        ("HOME", "/home/op"),
    ]);
    let exists_paths: HashSet<PathBuf> = [
        "/srv/weights",
        "/home/op/.local/share/hhagent/workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    let r = resolve_env(
        |k| env.get(k).cloned(),
        |p| exists_paths.contains(p),
        |p| exists_paths.contains(p),
    )
    .expect("happy path");
    assert_eq!(
        r.venv_dir,
        PathBuf::from("/home/op/.local/share/hhagent/workers/gliner-relex/.venv")
    );
}

// macOS-only: on Linux `resolve_env` forces `use_container_backend`
// to a compile-time `false` (issue #144), so this assertion only holds
// on macOS where the env var is actually read.
#[cfg(target_os = "macos")]
#[test]
fn resolve_env_sets_use_container_backend_when_env_var_is_one() {
    let env_map = std::collections::HashMap::from([
        ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/tmp/fake-weights"),
        ("HHAGENT_GLINER_RELEX_USE_CONTAINER", "1"),
    ]);
    let env_lookup = |k: &str| env_map.get(k).map(|v| v.to_string());
    let is_dir = |_: &Path| true;   // pretend /tmp/fake-weights exists
    let exists = |_: &Path| true;   // pretend any script_path exists
    let env = resolve_env(env_lookup, is_dir, exists).expect("resolve_env ok");
    assert!(
        env.use_container_backend,
        "HHAGENT_GLINER_RELEX_USE_CONTAINER=1 must set use_container_backend = true"
    );
}

// macOS-only: the strict-"1" parsing of HHAGENT_GLINER_RELEX_USE_CONTAINER
// only runs on macOS; on Linux the flag is compile-time `false` (issue #144).
#[cfg(target_os = "macos")]
#[test]
fn resolve_env_strict_about_use_container_value() {
    // Only "1" (after trim) counts — symmetric with HHAGENT_GLINER_RELEX_ENABLE
    // strictness. Surface dialect debate ("true", "yes", "on") would
    // creep in over time without this pin.
    for value in &["true", "yes", "on", "0", " 1 \n"] {
        let env_map = std::collections::HashMap::from([
            ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
            ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/tmp/fake-weights"),
            ("HHAGENT_GLINER_RELEX_USE_CONTAINER", *value),
            // Anchor required so host-mode path can resolve venv dir
            // (non-"1" values fall through to host mode, which needs
            // at least one of VENV_DIR / DATA_DIR / HOME set).
            ("HOME", "/tmp/fake-home"),
        ]);
        let env_lookup = |k: &str| env_map.get(k).map(|v| v.to_string());
        let is_dir = |_: &Path| true;
        let exists = |_: &Path| true;
        let env = resolve_env(env_lookup, is_dir, exists).expect("resolve_env ok");
        // " 1 \n" → trim() == "1" so it DOES count; others don't.
        let expected = value.trim() == "1";
        assert_eq!(
            env.use_container_backend, expected,
            "value {value:?} should yield use_container_backend = {expected}"
        );
    }
}

// macOS-only: container mode (and thus the venv-check skip) only exists
// on macOS (issue #144); on Linux the same env always resolves host mode.
#[cfg(target_os = "macos")]
#[test]
fn resolve_env_skips_venv_existence_check_in_container_mode() {
    // In container mode the host venv is unused (the worker shim lives
    // inside the image at /usr/local/bin/...). Don't force operators to
    // maintain a host venv when they're running container-mode-only.
    let env_map = std::collections::HashMap::from([
        ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/tmp/fake-weights"),
        ("HHAGENT_GLINER_RELEX_USE_CONTAINER", "1"),
        ("HHAGENT_DATA_DIR", "/nonexistent/data-dir"),
    ]);
    let env_lookup = |k: &str| env_map.get(k).map(|v| v.to_string());
    let is_dir = |p: &Path| p == Path::new("/tmp/fake-weights");
    let exists = |_: &Path| false;  // host venv shim DOES NOT exist anywhere
    let result = resolve_env(env_lookup, is_dir, exists);
    let env = result.expect("container mode must skip venv check; got ScriptShimMissing");
    assert!(env.use_container_backend);
    assert_eq!(env.script_path, PathBuf::new(), "script_path empty in container mode");
    assert_eq!(env.venv_dir, PathBuf::new(), "venv_dir empty in container mode");
    assert_eq!(env.weights_dir, PathBuf::from("/tmp/fake-weights"));
}

// macOS-only: the scenario sets USE_CONTAINER=1 and expects the
// container path; on Linux that env resolves to host mode and (lacking a
// venv anchor) would return VenvDirUnresolvable. Gated per issue #144.
#[cfg(target_os = "macos")]
#[test]
fn resolve_env_picks_up_container_image_override() {
    let env_map = std::collections::HashMap::from([
        ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
        ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/tmp/fake-weights"),
        ("HHAGENT_GLINER_RELEX_USE_CONTAINER", "1"),
        ("HHAGENT_GLINER_RELEX_IMAGE", "hhagent/gliner-relex:v0.0.1"),
    ]);
    let env_lookup = |k: &str| env_map.get(k).map(|v| v.to_string());
    let is_dir = |_: &Path| true;
    let exists = |_: &Path| true;
    let env = resolve_env(env_lookup, is_dir, exists).expect("resolve_env ok");
    assert_eq!(
        env.container_image.as_deref(),
        Some("hhagent/gliner-relex:v0.0.1"),
        "HHAGENT_GLINER_RELEX_IMAGE override must flow into GlinerRelexEnv.container_image"
    );
}

#[test]
fn client_error_display_pins_format() {
    // The `Display` impl is wire-stable: operator-facing log
    // messages and audit-row error strings rely on these exact
    // forms. A refactor that shuffles the `#[error(...)]`
    // attributes will trip these assertions before it can land.
    let e = ClientError::EncodeError("bad json".into());
    assert_eq!(e.to_string(), "encode error: bad json");

    let e = ClientError::WorkerSpawnFailed("no venv".into());
    assert_eq!(e.to_string(), "worker spawn failed: no venv");

    let e = ClientError::WorkerDead("EOF".into());
    assert_eq!(e.to_string(), "worker dead mid-call: EOF");

    let e = ClientError::RpcError {
        code: -32001,
        message: "INVALID_INPUT".into(),
    };
    assert_eq!(e.to_string(), "rpc error code=-32001: INVALID_INPUT");

    let e = ClientError::DecodeError("not an ExtractResponse".into());
    assert_eq!(e.to_string(), "decode error: not an ExtractResponse");
}

// ---- GlinerRelexManifest unit tests --------------------------------
//
// Tests for the WorkerManifest impl: disabled, misconfigured, and happy-
// path (register) outcomes. Uses the same in-memory env-var + fs fakes
// as the resolve_env tests above.

use crate::worker_manifest::{ResolveCtx, Resolution, WorkerManifest};

/// Build a ResolveCtx whose env is a closure over a fixed map. fs probes
/// are supplied per-test; allowlist is unused for gliner (returns empty).
fn gliner_ctx<'a>(
    get_env: &'a dyn Fn(&str) -> Option<String>,
    is_dir: &'a dyn Fn(&Path) -> bool,
    exists: &'a dyn Fn(&Path) -> bool,
) -> ResolveCtx<'a> {
    ResolveCtx {
        get_env,
        exists,
        is_dir,
        exe_dir: None,
        allowlist: &|_t| Vec::new(),
    }
}

#[test]
fn manifest_disabled_when_enable_flag_absent() {
    let get_env = |_k: &str| None;
    let is_dir = |_p: &Path| false;
    let exists = |_p: &Path| false;
    let c = gliner_ctx(&get_env, &is_dir, &exists);
    match GlinerRelexManifest.resolve(&c) {
        Resolution::Disabled { .. } => {}
        _ => panic!("expected Disabled when HHAGENT_GLINER_RELEX_ENABLE unset"),
    }
}

#[test]
fn manifest_misconfigured_when_weights_dir_env_missing() {
    let get_env =
        |k: &str| (k == "HHAGENT_GLINER_RELEX_ENABLE").then(|| "1".to_string());
    let is_dir = |_p: &Path| false;
    let exists = |_p: &Path| false;
    let c = gliner_ctx(&get_env, &is_dir, &exists);
    match GlinerRelexManifest.resolve(&c) {
        Resolution::Misconfigured { detail } => {
            assert!(detail.contains("HHAGENT_GLINER_RELEX_WEIGHTS_DIR"), "detail: {detail}");
        }
        _ => panic!("expected Misconfigured when weights dir env missing"),
    }
}

#[test]
fn manifest_registers_on_happy_path() {
    // enable=1, weights dir is a dir, explicit venv dir, shim exists.
    let get_env = |k: &str| match k {
        "HHAGENT_GLINER_RELEX_ENABLE" => Some("1".to_string()),
        "HHAGENT_GLINER_RELEX_WEIGHTS_DIR" => Some("/weights".to_string()),
        "HHAGENT_GLINER_RELEX_VENV_DIR" => Some("/data/.venv".to_string()),
        _ => None,
    };
    let is_dir = |p: &Path| p == Path::new("/weights");
    // resolve_env checks the shim path `<venv>/bin/hhagent-worker-gliner-relex`.
    // Confirmed: line 520 of gliner_relex.rs builds
    // `venv_dir.join("bin").join("hhagent-worker-gliner-relex")`,
    // so for venv `/data/.venv` → `/data/.venv/bin/hhagent-worker-gliner-relex`.
    let exists = |p: &Path| p == Path::new("/data/.venv/bin/hhagent-worker-gliner-relex");
    let c = gliner_ctx(&get_env, &is_dir, &exists);
    match GlinerRelexManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(
                matches!(entry.lifecycle, crate::worker_lifecycle::Lifecycle::IdleTimeout { .. }),
                "gliner must register IdleTimeout"
            );
        }
        _ => panic!("expected Register on the happy path"),
    }
}

#[test]
fn client_error_variants_are_distinct() {
    // Compile-time exhaustiveness pin: every variant must be
    // reachable by an explicit arm. If a future variant is added
    // to `ClientError` without updating this classifier, the
    // build fails with a non-exhaustive-match error — forcing the
    // caller-side branch logic to be revisited.
    fn classify(e: &ClientError) -> &'static str {
        match e {
            ClientError::EncodeError(_) => "encode",
            ClientError::WorkerSpawnFailed(_) => "spawn",
            ClientError::WorkerDead(_) => "dead",
            ClientError::RpcError { .. } => "rpc",
            ClientError::DecodeError(_) => "decode",
        }
    }
    assert_eq!(classify(&ClientError::EncodeError("x".into())), "encode");
    assert_eq!(
        classify(&ClientError::WorkerSpawnFailed("x".into())),
        "spawn"
    );
    assert_eq!(classify(&ClientError::WorkerDead("x".into())), "dead");
    assert_eq!(
        classify(&ClientError::RpcError {
            code: 0,
            message: "x".into()
        }),
        "rpc"
    );
    assert_eq!(classify(&ClientError::DecodeError("x".into())), "decode");
}
