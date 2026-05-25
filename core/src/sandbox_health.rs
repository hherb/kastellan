//! Daemon-startup sandbox health checks (issue #120).
//!
//! Today the only check is per-`container_image` presence on macOS:
//! walk the registered `ToolEntry`s, collect every distinct image tag
//! used by a `MacosContainer`-backed worker, and probe each tag via
//! [`hhagent_sandbox::macos_container::MacosContainer::probe_image`].
//!
//! Why a one-shot health check rather than per-acquire probing? The
//! resolver hot path runs once per dispatch; spawning a `container
//! image inspect` subprocess there would tax every workload. Doing the
//! probe once at daemon startup gives the operator a clear actionable
//! warning at boot ("you forgot to build the gliner-relex image, run
//! `scripts/workers/gliner-relex/build-image.sh`") without re-paying
//! the cost on every dispatch.
//!
//! Cross-platform shape: the entire module compiles to a thin shim on
//! Linux because `SandboxBackendKind::Container` does not exist on
//! Linux (the variant is `#[cfg(target_os = "macos")]`-gated in
//! `hhagent-sandbox`). The pure target-collection helper still
//! compiles cross-platform (it just always returns empty on Linux);
//! only the probe-and-log driver is macOS-only.
//!
//! Failure mode: a missing image yields a single `tracing::warn!`
//! line per missing tag and the daemon continues startup. The
//! corresponding worker's first dispatch attempt will then fail
//! through the lifecycle manager's normal spawn-error path; the
//! operator already saw the warning at boot and knows what to do.

use crate::scheduler::tool_dispatch::ToolEntry;
use std::collections::BTreeMap;

/// One container image tag and the tool name(s) that reference it.
///
/// The `tool_names` vec is for operator-facing logging only — when the
/// probe fails, the warning surfaces both the tag AND every tool that
/// would be unable to spawn, so the operator can grep their tool
/// inventory if the tag name alone isn't obvious.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerImageTarget {
    /// Image tag (e.g. `"hhagent/gliner-relex:dev"`).
    pub image_tag: String,
    /// Tool names that reference this image tag, sorted ascending for
    /// deterministic logging.
    pub tool_names: Vec<String>,
}

/// Walk `entries` and collect every distinct `container_image` tag that
/// belongs to a `MacosContainer`-backed worker.
///
/// Output is sorted by `image_tag` (ascending) for deterministic
/// logging. Tools whose `sandbox_backend != Some(Container)` are
/// skipped. Tools whose `container_image == None` are skipped even if
/// they ARE Container-backed (the resolver substitutes
/// `MacosContainer::DEFAULT_IMAGE` in that case — pinning is the
/// caller's job, not ours; we don't try to probe a "no tag specified"
/// default here because that would warn loudly even for ad-hoc smoke
/// tests).
///
/// On non-macOS targets this always returns an empty vec — the
/// `SandboxBackendKind::Container` variant doesn't exist on Linux so
/// no `ToolEntry` can carry it.
pub fn collect_container_image_targets<'a>(
    entries: impl Iterator<Item = (&'a str, &'a ToolEntry)>,
) -> Vec<ContainerImageTarget> {
    let mut buckets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, entry) in entries {
        if !is_container_backend(entry) {
            continue;
        }
        let Some(tag) = entry.container_image.as_deref() else {
            continue;
        };
        buckets
            .entry(tag.to_string())
            .or_default()
            .push(name.to_string());
    }
    buckets
        .into_iter()
        .map(|(image_tag, mut tool_names)| {
            tool_names.sort();
            ContainerImageTarget {
                image_tag,
                tool_names,
            }
        })
        .collect()
}

/// True iff `entry`'s sandbox-backend opt-in is `MacosContainer`. On
/// non-macOS targets always returns false (the `Container` variant
/// doesn't exist on Linux).
#[cfg(target_os = "macos")]
fn is_container_backend(entry: &ToolEntry) -> bool {
    matches!(
        entry.sandbox_backend,
        Some(hhagent_sandbox::SandboxBackendKind::Container)
    )
}

#[cfg(not(target_os = "macos"))]
fn is_container_backend(_entry: &ToolEntry) -> bool {
    false
}

/// Probe every container image tag referenced by the registered
/// `ToolEntry`s and emit a tracing line per tag (info on success,
/// warn on miss). Does NOT propagate errors — daemon startup
/// continues regardless of probe outcomes. A failing probe means the
/// corresponding worker's first dispatch will fail loudly via the
/// lifecycle manager's normal error path; the operator has already
/// been warned at boot.
///
/// Returns the list of `(image_tag, probe_result)` pairs in the same
/// order they were probed so callers can collect deltas (e.g. tests
/// asserting "exactly one Err for the bogus tag, exactly one Ok for
/// the cached tag").
///
/// This function compiles ONLY on macOS — Linux callers should not
/// reference it at all (use a `#[cfg(target_os = "macos")]` guard at
/// the call site). Rationale: on Linux there are no
/// `Container`-backed workers (the variant doesn't exist), so the
/// walk would always be a no-op; embedding the cfg here would buy us
/// nothing.
#[cfg(target_os = "macos")]
pub fn probe_registered_container_images<'a>(
    entries: impl Iterator<Item = (&'a str, &'a ToolEntry)>,
) -> Vec<(String, Result<(), hhagent_sandbox::SandboxError>)> {
    let targets = collect_container_image_targets(entries);
    if targets.is_empty() {
        return Vec::new();
    }
    // Single backend-level probe before the per-tag walk: if Apple
    // `container` itself isn't installed / its system service isn't
    // running, every per-tag probe would error with the same spawn
    // failure — emitting one WARN per registered image clutters the
    // log with N copies of the same actionable hint. Collapse to one
    // WARN line and return empty (caller's perspective: no probes
    // were run, same as if no Container-backed entries were
    // registered). The downstream worker spawn will still fail
    // through the normal lifecycle-manager error path on first
    // dispatch — the operator has been warned at boot regardless.
    if let Err(e) = hhagent_sandbox::macos_container::MacosContainer::probe() {
        let affected_tools: Vec<String> = targets
            .iter()
            .flat_map(|t| t.tool_names.iter().cloned())
            .collect();
        tracing::warn!(
            target: "hhagent::sandbox_health",
            error = %e,
            affected_tools = %affected_tools.join(", "),
            "Apple `container` unavailable; skipping image health check \
             (affected workers will fail on first dispatch — install with \
             `brew install container && container system start \
             --enable-kernel-install`)",
        );
        return Vec::new();
    }
    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        let probe = hhagent_sandbox::macos_container::MacosContainer::probe_image(
            &target.image_tag,
        );
        // Render tools as a comma-joined string rather than Debug
        // (`["a", "b"]`) for cleaner operator-log lines. The Vec stays
        // on the returned tuple for test inspection / future
        // structured-logging consumers.
        let tools_joined = target.tool_names.join(", ");
        match &probe {
            Ok(()) => {
                tracing::info!(
                    target: "hhagent::sandbox_health",
                    image_tag = %target.image_tag,
                    tools = %tools_joined,
                    "container image present in local store",
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "hhagent::sandbox_health",
                    image_tag = %target.image_tag,
                    tools = %tools_joined,
                    error = %e,
                    "container image NOT present — affected tools will fail on first dispatch; \
                     build with the worker's `scripts/workers/<worker>/build-image.sh`",
                );
            }
        }
        results.push((target.image_tag, probe));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::tool_dispatch::ToolEntry;
    use crate::worker_lifecycle::Lifecycle;
    use hhagent_sandbox::{SandboxPolicy, SandboxBackendKind};
    use std::path::PathBuf;

    /// Build a minimal `ToolEntry` with all fields explicit so a future
    /// new field surfaces in this fixture (`..Default::default()` would
    /// silently absorb it).
    fn make_entry(
        sandbox_backend: Option<SandboxBackendKind>,
        container_image: Option<&str>,
    ) -> ToolEntry {
        ToolEntry {
            binary: PathBuf::from("/dev/null"),
            policy: SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle: Lifecycle::SingleUse,
            sandbox_backend,
            container_image: container_image.map(String::from),
        }
    }

    #[test]
    fn collect_returns_empty_for_empty_registry() {
        let entries: Vec<(&str, ToolEntry)> = vec![];
        let targets = collect_container_image_targets(
            entries.iter().map(|(n, e)| (*n, e)),
        );
        assert!(
            targets.is_empty(),
            "empty registry must yield no targets; got {targets:?}"
        );
    }

    #[test]
    fn collect_skips_non_container_backends() {
        // shell-exec-shaped entry: sandbox_backend = None (per-OS default).
        let entries = vec![("shell-exec", make_entry(None, Some("ignored:dev")))];
        let targets = collect_container_image_targets(
            entries.iter().map(|(n, e)| (*n, e)),
        );
        assert!(
            targets.is_empty(),
            "None-backend entries must be skipped even with container_image set; got {targets:?}"
        );
    }

    /// Linux has no `Container` variant so the function is a no-op there
    /// regardless of inputs. On macOS the `Container` variant exists and
    /// the entry IS collected.
    #[test]
    #[cfg(target_os = "macos")]
    fn collect_includes_container_backend_with_image() {
        let entries = vec![(
            "gliner-relex",
            make_entry(
                Some(SandboxBackendKind::Container),
                Some("hhagent/gliner-relex:dev"),
            ),
        )];
        let targets = collect_container_image_targets(
            entries.iter().map(|(n, e)| (*n, e)),
        );
        assert_eq!(targets.len(), 1, "expected exactly one target; got {targets:?}");
        assert_eq!(targets[0].image_tag, "hhagent/gliner-relex:dev");
        assert_eq!(targets[0].tool_names, vec!["gliner-relex".to_string()]);
    }

    /// A Container-backed entry with no image_tag is skipped — the
    /// resolver falls back to `MacosContainer::DEFAULT_IMAGE` and we
    /// don't want to warn loudly on ad-hoc smoke-test code paths.
    #[test]
    #[cfg(target_os = "macos")]
    fn collect_skips_container_backend_with_no_image_tag() {
        let entries = vec![(
            "smoke-tool",
            make_entry(Some(SandboxBackendKind::Container), None),
        )];
        let targets = collect_container_image_targets(
            entries.iter().map(|(n, e)| (*n, e)),
        );
        assert!(
            targets.is_empty(),
            "Container-backed entries with no image_tag must be skipped; got {targets:?}"
        );
    }

    /// Two distinct tools referencing the SAME image tag bucket together
    /// — one target carries both tool names (sorted) — so the
    /// operator-warning fires once per missing image, not once per tool.
    #[test]
    #[cfg(target_os = "macos")]
    fn collect_deduplicates_distinct_tools_sharing_an_image() {
        let entries = vec![
            (
                "z-tool",
                make_entry(
                    Some(SandboxBackendKind::Container),
                    Some("shared:dev"),
                ),
            ),
            (
                "a-tool",
                make_entry(
                    Some(SandboxBackendKind::Container),
                    Some("shared:dev"),
                ),
            ),
        ];
        let targets = collect_container_image_targets(
            entries.iter().map(|(n, e)| (*n, e)),
        );
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].image_tag, "shared:dev");
        assert_eq!(
            targets[0].tool_names,
            vec!["a-tool".to_string(), "z-tool".to_string()],
            "tool_names must be sorted ascending for deterministic logging"
        );
    }

    /// Two distinct tools with different image tags surface as two
    /// targets, sorted by image_tag ascending.
    #[test]
    #[cfg(target_os = "macos")]
    fn collect_returns_one_target_per_distinct_image_sorted() {
        let entries = vec![
            (
                "tool-z",
                make_entry(
                    Some(SandboxBackendKind::Container),
                    Some("zzz:tag"),
                ),
            ),
            (
                "tool-a",
                make_entry(
                    Some(SandboxBackendKind::Container),
                    Some("aaa:tag"),
                ),
            ),
        ];
        let targets = collect_container_image_targets(
            entries.iter().map(|(n, e)| (*n, e)),
        );
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].image_tag, "aaa:tag");
        assert_eq!(targets[1].image_tag, "zzz:tag");
    }
}
