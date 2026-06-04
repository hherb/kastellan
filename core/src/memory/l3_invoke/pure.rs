//! Pure L3-invocation engine: argument parsing, template substitution, the
//! trust gates, the [`prepare_invocation`] decision (reusing the approval
//! gate against the *live* tool set), and the issue-#179 registry-divergence
//! classifier. No I/O, no async — every function here is deterministic and
//! unit-testable in isolation. Shared verbatim by both the operator path
//! ([`super::operator`]) and the agent path ([`super::agent`]).

use std::collections::{BTreeMap, BTreeSet};

use crate::cassandra::types::{DataClass, L3SkillCandidate, L3TemplateStep, PlannedStep};
use crate::memory::l3_approval::{evaluate_approval, ApprovalDecision, SkillTrust};

/// Max bytes for a single operator-supplied argument value. A value is
/// just a tool argument (shell-exec does no shell interpretation and
/// `argv[0]` stays operator-allowlisted), but keeping it bounded + clean
/// mirrors the template guards.
pub const L3_ARG_MAX_VALUE_BYTES: usize = 1024;

/// Errors from the pure invocation front-end (arg parse + substitution).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvokeError {
    #[error("argument '{0}' is not of the form name=value")]
    MalformedArg(String),
    #[error("argument name '{0}' is not snake_case")]
    BadArgName(String),
    #[error("duplicate argument '{0}'")]
    DuplicateArg(String),
    #[error("missing value for declared parameter(s): {0}")]
    MissingArgs(String),
    #[error("unknown argument(s) not declared by the skill: {0}")]
    UnknownArgs(String),
    #[error("argument '{name}' value contains a newline, control character, or '{{{{' / '}}}}' sequence")]
    BadArgValue { name: String },
    #[error("argument '{name}' value exceeds {max} bytes ({got})")]
    ArgValueTooLong { name: String, max: usize, got: usize },
    #[error("placeholder '{{{{{0}}}}}' survived substitution (internal error)")]
    UnsubstitutedPlaceholder(String),
}

/// `true` iff `s` is a strict snake_case identifier (`[a-z][a-z0-9_]*`).
/// Mirrors `l3_crystallise::is_snake_ident` (kept local to avoid widening
/// that module's visibility).
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Parse `name=value` tokens (the CLI strips the `--arg` flag) into a map.
/// Splits on the FIRST `=` so values may contain `=`. Rejects a token with
/// no `=`, a non-snake_case name, or a duplicate name.
pub fn parse_args(tokens: &[String]) -> Result<BTreeMap<String, String>, InvokeError> {
    let mut map = BTreeMap::new();
    for tok in tokens {
        let (name, value) = tok
            .split_once('=')
            .ok_or_else(|| InvokeError::MalformedArg(tok.clone()))?;
        if !is_snake_ident(name) {
            return Err(InvokeError::BadArgName(name.to_string()));
        }
        if map.insert(name.to_string(), value.to_string()).is_some() {
            return Err(InvokeError::DuplicateArg(name.to_string()));
        }
    }
    Ok(map)
}

/// Replace every `{{name}}` occurrence inside a single string with the
/// supplied value. `args` is guaranteed complete by the caller's arity
/// check, so a `{{name}}` whose name is absent is left intact and caught
/// by the post-condition scan. Mirrors the writer's `scan_placeholders`
/// byte walk.
fn interpolate(s: &str, args: &BTreeMap<String, String>) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // find closing }}
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                let name = &s[start..j];
                if let Some(v) = args.get(name) {
                    out.push_str(v);
                    i = j + 2;
                    continue;
                }
            }
        }
        // not a (resolvable) placeholder start — copy one char
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Recursively interpolate every string leaf of a JSON value.
fn interpolate_value(v: &serde_json::Value, args: &BTreeMap<String, String>) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(interpolate(s, args)),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(|e| interpolate_value(e, args)).collect())
        }
        serde_json::Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, val) in m {
                out.insert(k.clone(), interpolate_value(val, args));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Returns the name of the first `{{name}}` placeholder still present in any
/// string leaf, or `None` if none remain. (A degenerate `{{}}` yields
/// `Some("")`, but empty names are impossible in a validated template.)
fn has_placeholder(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            let b = s.as_bytes();
            let mut i = 0;
            while i + 1 < b.len() {
                if b[i] == b'{' && b[i + 1] == b'{' {
                    let start = i + 2;
                    let mut j = start;
                    while j + 1 < b.len() && !(b[j] == b'}' && b[j + 1] == b'}') {
                        j += 1;
                    }
                    if j + 1 < b.len() {
                        return Some(s[start..j].to_string());
                    }
                }
                i += 1;
            }
            None
        }
        serde_json::Value::Array(a) => a.iter().find_map(has_placeholder),
        serde_json::Value::Object(m) => m.values().find_map(has_placeholder),
        _ => None,
    }
}

/// Substitute operator-supplied args into a stored skill template,
/// producing concrete (placeholder-free) steps.
///
/// Closed-world: the supplied arg names must EXACTLY equal the declared
/// parameter names. Each value must be free of newlines/control chars, must
/// not contain the `{{`/`}}` template-brace sequences, and must be within
/// [`L3_ARG_MAX_VALUE_BYTES`]. Asserts no `{{…}}` survives.
pub fn substitute_template(
    template: &L3SkillCandidate,
    args: &BTreeMap<String, String>,
) -> Result<Vec<L3TemplateStep>, InvokeError> {
    let declared: BTreeSet<&str> = template.parameters.iter().map(|p| p.name.as_str()).collect();
    let supplied: BTreeSet<&str> = args.keys().map(|s| s.as_str()).collect();

    let missing: Vec<&str> = declared.difference(&supplied).copied().collect();
    if !missing.is_empty() {
        return Err(InvokeError::MissingArgs(missing.join(", ")));
    }
    let unknown: Vec<&str> = supplied.difference(&declared).copied().collect();
    if !unknown.is_empty() {
        return Err(InvokeError::UnknownArgs(unknown.join(", ")));
    }

    for (name, value) in args {
        if value.len() > L3_ARG_MAX_VALUE_BYTES {
            return Err(InvokeError::ArgValueTooLong {
                name: name.clone(),
                max: L3_ARG_MAX_VALUE_BYTES,
                got: value.len(),
            });
        }
        // Reject control chars AND the template-brace sequences `{{`/`}}`.
        // The brace check is load-bearing: without it, a value that legitimately
        // contained `{{x}}` would, after interpolation, look like a surviving
        // placeholder and trip the spurious `UnsubstitutedPlaceholder` post-condition
        // below. Rejecting only the two-char sequences (not single braces) keeps
        // single-brace values like `{"json":true}` valid.
        if value.bytes().any(|b| b < 0x20) || value.contains("{{") || value.contains("}}") {
            return Err(InvokeError::BadArgValue { name: name.clone() });
        }
    }

    let mut out = Vec::with_capacity(template.steps.len());
    for step in &template.steps {
        let parameters = interpolate_value(&step.parameters, args);
        if let Some(name) = has_placeholder(&parameters) {
            return Err(InvokeError::UnsubstitutedPlaceholder(name));
        }
        out.push(L3TemplateStep {
            tool: step.tool.clone(),
            method: step.method.clone(),
            parameters,
        });
    }
    Ok(out)
}

/// A refusal to invoke, carrying every human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvokeRefusal {
    pub reasons: Vec<String>,
}

/// PURE trust gate: only `user_approved` / `pinned` skills run. Identical
/// membership to [`crate::memory::l3_surface::is_surfaceable`] (pinned in
/// sync by a test) — a skill the planner may *see* is exactly a skill the
/// operator may *run*.
pub fn is_runnable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::UserApproved | SkillTrust::Pinned)
}

/// PURE stricter gate for AGENT-autonomous invocation: only `pinned`
/// skills may be invoked by the agent itself. A strict subset of
/// [`is_runnable`] (the operator-CLI gate, which also allows
/// `user_approved`) and of
/// [`crate::memory::l3_surface::is_surfaceable`]. Granting autonomy is a
/// distinct human action (`memory l3 pin`) gated on a prior `approve`;
/// pinned-in-sync by `autonomy_ladder_is_subset_of_runnable_and_surfaceable`.
pub fn is_autonomously_invocable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::Pinned)
}

/// Synthesize a [`PlannedStep`] from a concrete template step, with an
/// explicit `classification`. `returns` / `done_when` are empty (unused
/// by `dispatch_step`). The agent path passes `plan.data_ceiling` so the
/// deterministic policy's I2/I3 invariants hold automatically.
pub fn planned_step_from_l3_with_class(step: &L3TemplateStep, class: DataClass) -> PlannedStep {
    PlannedStep {
        tool: step.tool.clone(),
        method: step.method.clone(),
        parameters: step.parameters.clone(),
        returns: String::new(),
        done_when: String::new(),
        classification: class,
    }
}

/// Operator-path mapper: `classification` is the most conservative class
/// (`Secret`) and is UNUSED on that path (`dispatch_step` reads only
/// `tool` / `method` / `parameters`). Delegates to
/// [`planned_step_from_l3_with_class`].
pub fn planned_step_from_l3(step: &L3TemplateStep) -> PlannedStep {
    planned_step_from_l3_with_class(step, DataClass::Secret)
}

/// PURE decision: may this stored skill run with these args against this
/// live tool set, and if so, what are the concrete steps?
///
/// 1. trust must be runnable ([`is_runnable`]);
/// 2. re-run the approval gate ([`evaluate_approval`]) against `live_tools`
///    — the TOCTOU close (structural re-validation + `secret://` re-scan +
///    every tool must exist in the registry as it is now);
/// 3. substitute args into the template ([`substitute_template`]).
///
/// On any failure returns an [`InvokeRefusal`] collecting the reason(s).
pub fn prepare_invocation(
    template: &L3SkillCandidate,
    stored_trust: SkillTrust,
    args: &BTreeMap<String, String>,
    live_tools: &BTreeSet<String>,
) -> Result<Vec<L3TemplateStep>, InvokeRefusal> {
    if !is_runnable(stored_trust) {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "skill trust '{}' is not runnable (only user_approved / pinned)",
                stored_trust.as_str()
            )],
        });
    }

    // Re-validate the STORED template against the live registry first
    // (structural + secret-ref + tool existence). This guards against a
    // skill approved against a now-stale snapshot, and short-circuits on a
    // structurally broken template before substitution.
    match evaluate_approval(template, live_tools) {
        ApprovalDecision::Approve => {}
        ApprovalDecision::Reject { reasons } => {
            return Err(InvokeRefusal {
                reasons: reasons.iter().map(|r| r.to_string()).collect(),
            });
        }
    }

    // Substitution can still fail on operator-arg problems (missing /
    // unknown / bad value) — surface those as refusal reasons too.
    substitute_template(template, args).map_err(|e| InvokeRefusal { reasons: vec![e.to_string()] })
}

/// Why a tool a skill needs is absent from the live in-process registry,
/// classified by comparing the live set against the daemon's recorded
/// `registry.loaded` snapshot. Drives the operator-facing hint on the
/// `memory l3 run` refusal path (issue #179). Advisory only — it changes
/// nothing about what is or isn't runnable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryDivergence {
    /// In the daemon's snapshot but missing from the live rebuild — almost
    /// always an unset env var (e.g. `HHAGENT_SHELL_EXEC_BIN`) in the
    /// operator's shell. THIS is the #179 usability cliff.
    MissingLocallyButInSnapshot { tool: String },
    /// Missing locally and no daemon snapshot exists to compare against —
    /// likely an env problem, but unconfirmable (has the daemon ever run?).
    MissingLocallyNoSnapshot { tool: String },
    /// In neither the live registry nor the snapshot — a genuinely unknown
    /// tool, not an environment problem (the legitimate refusal).
    UnknownEverywhere { tool: String },
}

impl std::fmt::Display for RegistryDivergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryDivergence::MissingLocallyButInSnapshot { tool } => write!(
                f,
                "'{tool}' is registered by the daemon but missing from your \
                 environment — is the tool's env var (e.g. HHAGENT_SHELL_EXEC_BIN) \
                 set? Run with the same environment the daemon uses."
            ),
            RegistryDivergence::MissingLocallyNoSnapshot { tool } => write!(
                f,
                "'{tool}' is missing from your environment and no daemon registry \
                 snapshot exists to compare against (has the daemon run at least once?)."
            ),
            RegistryDivergence::UnknownEverywhere { tool } => write!(
                f,
                "'{tool}' is unknown to both your environment and the daemon's last \
                 snapshot — the skill references a tool that is no longer registered."
            ),
        }
    }
}

/// Classify every tool the skill NEEDS that is absent from the live registry,
/// using the daemon's recorded `registry.loaded` snapshot to distinguish an
/// unset-env cliff from a genuinely unknown tool (issue #179).
///
/// Returns empty when every needed tool is present locally — so the caller
/// stays silent on refusals that are not about missing tools (trust,
/// `secret://`, arg errors). `snapshot_tools == None` means the daemon has
/// never recorded a snapshot. Output order is deterministic (sorted, by the
/// `BTreeSet` iteration of `needed_tools`).
pub fn diagnose_registry_divergence(
    needed_tools: &BTreeSet<String>,
    live_tools: &BTreeSet<String>,
    snapshot_tools: Option<&BTreeSet<String>>,
) -> Vec<RegistryDivergence> {
    needed_tools
        .iter()
        .filter(|t| !live_tools.contains(*t))
        .map(|tool| match snapshot_tools {
            Some(snap) if snap.contains(tool) => {
                RegistryDivergence::MissingLocallyButInSnapshot { tool: tool.clone() }
            }
            Some(_) => RegistryDivergence::UnknownEverywhere { tool: tool.clone() },
            None => RegistryDivergence::MissingLocallyNoSnapshot { tool: tool.clone() },
        })
        .collect()
}
