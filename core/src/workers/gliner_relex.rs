//! GLiNER-Relex worker manifest + wire-shape types.
//!
//! See `docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`
//! for the design, and the Slice 2 section of
//! `docs/superpowers/plans/2026-05-18-gliner-relex-worker.md` for the
//! task-level breakdown this module implements.
//!
//! What this module owns:
//!
//! - [`GlinerRelexEnv`] — daemon-startup builder; carries the resolved
//!   weights/venv paths + model id + device selector.
//! - [`gliner_relex_entry`] — produces the [`crate::scheduler::ToolEntry`]
//!   that the dispatcher's [`crate::scheduler::ToolRegistry`] holds.
//! - [`ExtractRequest`] / [`ExtractResponse`] / [`Entity`] /
//!   [`TripleEntity`] / [`Triple`] — serde shape types matching the
//!   Python worker's wire contract (see
//!   `workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py`
//!   for the producing side + `workers/gliner-relex/README.md` for the
//!   field-by-field shape table).
//!
//! What this module deliberately does NOT own:
//!
//! - **A typed Rust client wrapping [`crate::tool_host::dispatch`]**.
//!   The dispatcher's `report_crash` chokepoint between `dispatch` and
//!   `map_dispatch_result` makes a standalone client either duplicate
//!   crash-classifier logic or couple to a lifecycle manager; the v2
//!   entity-extraction consumer slice will pick the right shape around
//!   its actual call site. See HANDOVER's design-spec section for the
//!   rationale.

use std::path::PathBuf;

use hhagent_sandbox::{Net, Profile, SandboxPolicy};
use serde::{Deserialize, Serialize};

use crate::scheduler::ToolEntry;
use crate::worker_lifecycle::{Contract, IdleTimeoutCaps, Lifecycle};

/// Resolved paths + config for the GLiNER-Relex worker.
///
/// Populated by the daemon's startup code from environment variables
/// (see `core/src/main.rs::build_gliner_relex_entry`) and passed into
/// [`gliner_relex_entry`] to build the manifest.
///
/// Production callers should construct this via the daemon helper;
/// tests build it directly to pin manifest shape without touching the
/// real filesystem.
#[derive(Debug, Clone)]
pub struct GlinerRelexEnv {
    /// Absolute path to the uv-generated console-script shim:
    /// `<worker_dir>/.venv/bin/hhagent-worker-gliner-relex`. This is
    /// the binary the dispatcher spawns under sandbox; `pyproject.toml`
    /// declares `[project.scripts] hhagent-worker-gliner-relex` so
    /// `uv sync` creates the file.
    pub script_path: PathBuf,
    /// Absolute path to the worker venv root: `<worker_dir>/.venv/`.
    /// Mounted read-only into the sandbox via `policy.fs_read` so the
    /// Python interpreter + site-packages are visible from inside the
    /// jail.
    pub venv_dir: PathBuf,
    /// Absolute path to the model snapshot directory; operator stages
    /// this via `scripts/workers/gliner-relex/install.sh`. Mounted
    /// read-only via `policy.fs_read`. Daemon refuses to register the
    /// worker if this path doesn't exist on disk at startup.
    pub weights_dir: PathBuf,
    /// HF repo ID matching the on-disk snapshot. One of
    /// `knowledgator/gliner-relex-multi-v1.0` (default) or
    /// `knowledgator/gliner-relex-large-v0.5`. Forwarded via env var
    /// to the worker for its own startup-time logging only — the
    /// worker loads from `weights_dir` directly.
    pub model_id: String,
    /// `auto` / `cuda` / `cpu`. `auto` lets the worker probe
    /// `torch.cuda.mem_get_info(0)` for >= 3 GiB free (per spike
    /// correction #4) and pick CUDA or fall back to CPU silently.
    /// `mps` is reserved for the macOS follow-up plan.
    pub device: String,
}

/// Construct the [`ToolEntry`] for the gliner-relex worker.
///
/// The returned entry is registered in `core::main` when
/// `HHAGENT_GLINER_RELEX_ENABLE=1` and the weights directory exists
/// on disk. Without those preconditions the entry is skip-registered
/// (existing deployments byte-equivalent) and calls to `gliner-relex`
/// return `UNKNOWN_TOOL` from the dispatcher.
///
/// Manifest decisions worth knowing (all match the design spec):
///
/// - **`Lifecycle::IdleTimeout`** with 10-minute idle window, 10 000
///   request cap, daily age-out, and 5 s grace. This is the
///   first-ever idle-timeout consumer in the tree (see
///   `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`).
/// - **`Contract { stateless: true }`** — required by
///   `Lifecycle::idle_timeout`'s validator. The worker is genuinely
///   stateless: each `extract` request runs the model on its own
///   text and returns; no memory of prior requests.
/// - **`cpu_ms: 0`** — disables `setrlimit(RLIMIT_CPU)`. The rlimit
///   is cumulative across the process's whole lifetime; on a warm
///   worker doing thousands of inferences it would fire even when
///   no single request is pathological. The cgroup `cpu_quota_pct`
///   ceiling + `Lifecycle::max_age_seconds` rotation handle the
///   actual safety needs; per-request hang detection is dispatcher
///   work that the worker-lifecycle spec deliberately punts.
/// - **`wall_clock_ms: None`** — same logic. Warm workers are
///   long-lived by design; `Lifecycle::max_age_seconds` (24 h) is
///   the rotation budget.
/// - **`Net::Deny`** — the worker has no business reaching the
///   network. `HF_HUB_OFFLINE=1` + `TRANSFORMERS_OFFLINE=1` are
///   defense-in-depth env hints to the libraries themselves.
/// - **`mem_mb: 4_096`** — sized for `multi-v1.0` (~2-3 GB resident)
///   with headroom. Operators picking `large-v0.5` (~4-5 GB) need
///   to bump this; flagged in the README's env-var table.
pub fn gliner_relex_entry(env: &GlinerRelexEnv) -> ToolEntry {
    // The venv uses an editable install (uv's default for hatchling
    // workspace projects); `.venv/.../_editable_impl_*.pth` points at
    // `<worker_dir>/src`. Mounting only `.venv` would let Python start
    // but fail on `from hhagent_worker_gliner_relex.__main__ import
    // main` with ModuleNotFoundError. Compute the sibling `src/` from
    // the documented `<worker_dir>/.venv` contract on `venv_dir` and
    // bind it read-only too.
    let worker_src_dir = env
        .venv_dir
        .parent()
        .map(|worker_dir| worker_dir.join("src"))
        .unwrap_or_else(|| env.venv_dir.join("../src"));

    let policy = SandboxPolicy {
        fs_read: vec![
            env.weights_dir.clone(),
            env.venv_dir.clone(),
            worker_src_dir,
        ],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 4_096,
        profile: Profile::WorkerStrict,
        cpu_quota_pct: Some(400),
        tasks_max: Some(64),
        env: vec![
            (
                "HHAGENT_GLINER_RELEX_WEIGHTS_DIR".to_string(),
                env.weights_dir.to_string_lossy().into_owned(),
            ),
            (
                "HHAGENT_GLINER_RELEX_MODEL".to_string(),
                env.model_id.clone(),
            ),
            (
                "HHAGENT_GLINER_RELEX_DEVICE".to_string(),
                env.device.clone(),
            ),
            ("HF_HUB_OFFLINE".to_string(), "1".to_string()),
            ("TRANSFORMERS_OFFLINE".to_string(), "1".to_string()),
            // PyTorch's _dynamo (transitively imported by transformers)
            // calls getpass.getuser() at module-import time, which
            // falls back to pwd.getpwuid(os.getuid()) when no
            // LOGNAME/USER/LNAME/USERNAME is set. The sandbox has no
            // /etc/passwd, so that fallback raises KeyError and the
            // worker exits before serving any RPC. Setting USER skips
            // the pwd lookup entirely (getpass picks the first
            // non-empty env var). The value is arbitrary; we use
            // "hhagent" as a marker that this is the worker, not a
            // real user account.
            ("USER".to_string(), "hhagent".to_string()),
            // TORCHINDUCTOR_CACHE_DIR pre-empts the home-dir cache
            // computation that triggers the getpass.getuser path
            // above (defense in depth — the USER env var alone is
            // sufficient today, but a future torch refactor could
            // re-route through getuid()). /tmp is tmpfs inside the
            // sandbox so this is ephemeral per-spawn; no leakage to
            // the host. Slice 2 doesn't use torch.compile so the
            // cache stays effectively empty.
            (
                "TORCHINDUCTOR_CACHE_DIR".to_string(),
                "/tmp/torchinductor".to_string(),
            ),
        ],
    };

    let lifecycle = Lifecycle::idle_timeout(
        IdleTimeoutCaps {
            idle_seconds: 600,
            max_requests: 10_000,
            max_age_seconds: 86_400,
            grace_period_seconds: 5,
        },
        Contract { stateless: true },
    )
    .expect("manifest declares stateless = true; validator must accept");

    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        wall_clock_ms: None,
        lifecycle,
    }
}

/// Maximum number of distinct entity labels per `extract` request.
///
/// Pinned to the matching `MAX_ENTITY_LABELS` constant on the Python
/// side at
/// `workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py`.
/// Bumping either side requires bumping both: the Python validator
/// will reject inputs the Rust caller could otherwise generate.
pub const MAX_ENTITY_LABELS: usize = 64;

/// Maximum number of distinct relation labels per `extract` request.
/// Empty is valid and signals entity-only mode (no relations returned).
pub const MAX_RELATION_LABELS: usize = 64;

/// Maximum UTF-8 byte length of the `text` field.
pub const MAX_TEXT_BYTES: usize = 8192;

/// Wire shape of an `extract` request's `params`.
///
/// `threshold` and `max_entities` are optional on the wire (the Python
/// server applies defaults of 0.5 and 64). `relation_threshold` is
/// captured separately per spike correction #3 — the GLiNER-Relex
/// model is noisy at low thresholds and production callers should pass
/// ≥ 0.5 for relations to suppress dense candidate-triple noise from
/// overlapping entity subspans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractRequest {
    pub text: String,
    pub entity_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entities: Option<u32>,
}

/// Wire shape of an `extract` response's `result`.
///
/// `entities` carries top-level entity dicts (see [`Entity`]); `triples`
/// carries relations whose `head` and `tail` are *nested* entity refs
/// (see [`TripleEntity`]) — a deliberately different shape with `type`
/// instead of `label` and an `entity_idx` back-pointer, no nested
/// `score`. The smoke test on real `multi-v1.0` weights established
/// this naming (see `workers/gliner-relex/README.md` "Field-key naming
/// observed on real `multi-v1.0` output" for the table).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractResponse {
    pub entities: Vec<Entity>,
    pub triples: Vec<Triple>,
}

/// A top-level entity in [`ExtractResponse::entities`].
///
/// Distinct from [`TripleEntity`] because the upstream GLiNER-Relex
/// envelope uses different field names + a different field set for the
/// two positions: top-level entities carry `label` + `score`; nested
/// triple head/tail carry `type` + `entity_idx` (and no `score`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub text: String,
    pub label: String,
    pub start: u32,
    pub end: u32,
    pub score: f32,
}

/// A nested entity reference inside [`Triple::head`] / [`Triple::tail`].
///
/// Real `knowledgator/gliner-relex-multi-v1.0` output uses `type` (NOT
/// `label`) for the entity category and adds an `entity_idx`
/// back-pointer into the top-level [`ExtractResponse::entities`]
/// array. There is no per-position `score`; consumers wanting the
/// score look up `entities[entity_idx].score`. See
/// `workers/gliner-relex/README.md` "Field-key naming observed on
/// real `multi-v1.0` output" for the empirical confirmation (smoke
/// test 2026-05-18, fixed in `1c36f56`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TripleEntity {
    pub text: String,
    /// The entity type. Named `type` on the wire (matching upstream)
    /// but Rust requires the `r#` raw-identifier prefix for the
    /// keyword. Serde's `rename` keeps the wire side clean.
    #[serde(rename = "type")]
    pub r#type: String,
    pub start: u32,
    pub end: u32,
    /// Index back into the top-level [`ExtractResponse::entities`]
    /// array. Stable for a single response only.
    pub entity_idx: u32,
}

/// A relation triple in [`ExtractResponse::triples`].
///
/// Field names match upstream's [GLiNER-Relex inference envelope][gr]:
/// `head` and `tail` (NOT `subject` / `object`) carry full nested
/// entity dicts via [`TripleEntity`]; `relation` is the predicate
/// label; `score` is the model's confidence. See spike correction #2
/// at `docs/superpowers/specs/2026-05-18-gliner-relex-spike-notes.md`.
///
/// [gr]: https://github.com/urchade/GLiNER
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Triple {
    pub head: TripleEntity,
    pub tail: TripleEntity,
    pub relation: String,
    pub score: f32,
}

#[cfg(test)]
mod tests {
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
}
