//! Unit tests for [`super::container_images`] — the CLI's listing format, the
//! exact-reference match, and the built-image registry (#684, #687).
//!
//! Not cfg-gated to macOS: the parsing and the registry are pure, so the DGX
//! runs them too. See `container_tests` for why that split matters.

use std::path::Path;

use super::container::{
    container_preflight, find_image, normalize_reference, parse_image_list, ContainerImage,
    PYTHON_EXEC_BUILD_INPUTS, PYTHON_EXEC_BUILD_SCRIPT, PYTHON_EXEC_SOURCE_DIRS,
};

/// The reference the three container suites actually ask for.
const PY_IMAGE: &str = "kastellan/python-exec:dev";

/// A trimmed but REAL `container image list --format json` capture from this
/// Mac (2026-09-09), not a hand-written ideal.
///
/// Keeping the CLI's own shape matters: it nests the reference under
/// `configuration.name` and the timestamp under `configuration.creationDate`,
/// stores Docker Hub images under a `docker.io/library/` prefix while a
/// locally-built image keeps its bare name, and emits `architecture:
/// "unknown"` variants with no `created` field. A hand-simplified fixture
/// would have hidden all four.
const REAL_LIST_JSON: &str = r#"[
  {
    "configuration": {
      "creationDate": "2026-04-16T23:53:24Z",
      "name": "docker.io/library/alpine:3.20"
    },
    "id": "d9e853e87e55526f",
    "variants": [
      {"config": {"architecture": "arm64", "os": "linux", "created": "2026-04-16T23:53:24.896953537Z"}},
      {"config": {"architecture": "unknown", "os": "unknown"}}
    ]
  },
  {
    "configuration": {
      "creationDate": "2026-06-26T03:54:57Z",
      "name": "kastellan/python-exec:dev"
    },
    "id": "b2f1d4d64bd503ef",
    "variants": [
      {"config": {"architecture": "arm64", "os": "linux", "created": "2026-06-26T03:54:57.096049841Z"}}
    ]
  }
]"#;

/// 2026-06-26T03:54:57Z, the measured build time of this Mac's image.
const IMAGE_BUILT: i64 = 1_782_446_097;

// ---------------------------------------------------------------------------
// parse_image_list
// ---------------------------------------------------------------------------

#[test]
fn parse_image_list_reads_reference_and_creation_time() {
    let images = parse_image_list(REAL_LIST_JSON).expect("real CLI output must parse");
    let found = find_image(&images, "kastellan/python-exec:dev").expect("image is in the capture");
    assert_eq!(found.created_unix, Some(IMAGE_BUILT));
}

#[test]
fn parse_image_list_rejects_output_that_is_not_json() {
    // The CLI prints a human-readable error and exits 0 when a plugin is
    // missing (measured: `container images list` → "Plugin not found", exit
    // 0). Parsing must fail loudly rather than yield an empty image set,
    // which would render as "image not present" — the folding this fixes.
    let err = parse_image_list("Error: Plugin 'container-images' not found.")
        .expect_err("non-JSON output must be an error, not an empty list");
    assert!(
        !err.is_empty(),
        "the error must carry a reason the operator can act on"
    );
}

#[test]
fn parse_image_list_accepts_an_empty_image_set() {
    // A host with the CLI working and no images at all is a legitimate,
    // different state from a broken CLI. It must parse to zero images.
    let images = parse_image_list("[]").expect("an empty list is valid CLI output");
    assert!(images.is_empty());
}

// ---------------------------------------------------------------------------
// find_image — #684's substring defect
// ---------------------------------------------------------------------------

#[test]
fn a_bare_substring_does_not_satisfy_a_full_reference() {
    // THE #684 DEFECT, as a test. The shipped helpers asked
    // `stdout.contains("python-exec")`, so any image whose name merely
    // contained that substring certified the run.
    let images = parse_image_list(REAL_LIST_JSON).unwrap();
    assert!(
        find_image(&images, "python-exec").is_none(),
        "a bare substring must not match kastellan/python-exec:dev"
    );
}

#[test]
fn a_different_tag_of_the_same_image_does_not_match() {
    // The other half of the substring defect: `kastellan/python-exec:v0.0.1`
    // is a DIFFERENT image from `:dev`, and matching on the name alone let a
    // stale hand-built tag certify a run of the default one.
    let images = parse_image_list(REAL_LIST_JSON).unwrap();
    assert!(
        find_image(&images, "kastellan/python-exec:v0.0.1").is_none(),
        "a different tag must not match"
    );
}

#[test]
fn a_docker_hub_image_matches_its_bare_reference() {
    // Measured: the CLI stores `alpine:3.20` as `docker.io/library/alpine:3.20`
    // while `lifecycle_container_routing_e2e` asks for `alpine:3.20`. Without
    // normalisation the lookup fails and the suite skips on a host that has
    // the image.
    let images = parse_image_list(REAL_LIST_JSON).unwrap();
    let found = find_image(&images, "alpine:3.20").expect("alpine must be found by bare reference");
    assert_eq!(found.reference, "alpine:3.20");
}

#[test]
fn normalize_reference_strips_only_the_docker_hub_prefix() {
    assert_eq!(normalize_reference("docker.io/library/alpine:3.20"), "alpine:3.20");
    // Non-widening: nothing else is stripped, so a partial name still fails.
    assert_eq!(normalize_reference("kastellan/python-exec:dev"), "kastellan/python-exec:dev");
    assert_eq!(normalize_reference("ghcr.io/apple/x:1"), "ghcr.io/apple/x:1");
}

// ---------------------------------------------------------------------------
// The registry, pinned against the thing it copies
// ---------------------------------------------------------------------------

#[test]
fn the_source_closure_matches_cargo_metadata() {
    // PYTHON_EXEC_SOURCE_DIRS is a hand-maintained copy of a closure cargo
    // already computes. Pin it against the real thing, so adding a workspace
    // dependency to the worker fails HERE rather than silently shrinking what
    // the freshness rule watches.
    let closure = super::container::workspace_closure_dirs("kastellan-worker-python-exec")
        .expect("cargo metadata must be readable in the workspace");
    let mut expected: Vec<String> = closure;
    expected.sort();
    let mut declared: Vec<String> = PYTHON_EXEC_SOURCE_DIRS.iter().map(|s| s.to_string()).collect();
    declared.sort();
    assert_eq!(
        declared, expected,
        "PYTHON_EXEC_SOURCE_DIRS has drifted from the worker's real workspace closure"
    );
}

#[test]
fn every_declared_build_input_exists_on_disk() {
    // A path typo would silently become an `unstat` entry — a check that
    // quietly stops checking.
    for rel in PYTHON_EXEC_BUILD_INPUTS {
        let path = super::repo_root().join(rel);
        assert!(path.is_file(), "declared build input is missing: {rel}");
    }
}

#[test]
fn every_declared_source_dir_exists_on_disk() {
    for rel in PYTHON_EXEC_SOURCE_DIRS {
        let path = super::repo_root().join(rel);
        assert!(path.is_dir(), "declared source dir is missing: {rel}");
    }
}

// ---------------------------------------------------------------------------
// source_stamps — the real filesystem walk, on both hosts
// ---------------------------------------------------------------------------

#[test]
fn source_stamps_reads_the_real_worker_sources() {
    let (stamps, unstat) = super::container::source_stamps();
    assert!(
        unstat.is_empty(),
        "every declared source must be readable in a checkout: {unstat:?}"
    );
    assert!(
        stamps.iter().any(|s| s.path.ends_with("workers/python-exec/Containerfile")),
        "the Containerfile decides the runtime image, so it must participate"
    );
    assert!(
        stamps.iter().any(|s| s.path == Path::new("Cargo.lock")),
        "the cross-build runs --locked, so Cargo.lock must participate"
    );
    assert!(
        stamps.iter().any(|s| s.path.starts_with("workers/prelude")),
        "the worker links the prelude, so a prelude change must invalidate the image"
    );
}

#[test]
fn source_stamps_ignores_build_output() {
    // A `target/` directory under a crate would swamp the comparison with
    // artefacts whose mtimes move on every build — reintroducing exactly the
    // #667 relink problem this rule was chosen to avoid.
    let (stamps, _) = super::container::source_stamps();
    assert!(
        !stamps.iter().any(|s| s.path.components().any(|c| c.as_os_str() == "target")),
        "build output must not participate in a SOURCE mtime rule"
    );
}

#[test]
fn source_stamps_finds_more_than_a_handful_of_files() {
    // Vacuity guard: an enumeration that silently returns almost nothing
    // would make the age rule certify anything. Three crates plus two build
    // inputs is comfortably more than 20 files.
    let (stamps, _) = super::container::source_stamps();
    assert!(
        stamps.len() > 20,
        "expected the whole closure, got {} files",
        stamps.len()
    );
}

// ---------------------------------------------------------------------------
// The built-image registry — a staleness rule needs a source closure to exist
// ---------------------------------------------------------------------------

#[test]
fn an_upstream_image_has_no_source_closure_and_so_no_staleness_rule() {
    // Caught by `lifecycle_container_routing_e2e` on the real host: the first
    // version applied the python-exec closure to `alpine:3.20` and told the
    // operator to "rebuild it with build-image.sh" — an upstream image this
    // repo does not build, cannot rebuild, and has no source to be stale
    // against. A staleness verdict is only meaningful for an image whose
    // sources this tree owns.
    assert!(
        super::container::built_image("alpine:3.20").is_none(),
        "alpine is upstream; it must carry no staleness rule"
    );
    assert!(
        super::container::built_image(PY_IMAGE).is_some(),
        "the python-exec image IS built here, so it must carry one"
    );
}

#[test]
fn an_unregistered_image_skips_the_age_check_entirely() {
    // The preflight must not merely ignore the verdict — it must never ASK,
    // because asking would stat a closure that has nothing to do with this
    // image.
    let skipped = container_preflight(
        "alpine:3.20",
        || Ok(()),
        || {
            Ok(Some(ContainerImage {
                reference: "alpine:3.20".to_string(),
                created_unix: Some(IMAGE_BUILT),
            }))
        },
        |_, _| panic!("an upstream image must not be age-checked"),
        |_| panic!("nothing is unmet"),
        |_| panic!("nothing to warn about"),
    );
    assert!(!skipped, "a present upstream image must run the test");
}

#[test]
fn each_built_image_names_its_own_build_script() {
    // The remedy must be the script that builds THAT image. One hard-coded
    // script is how alpine got told to rebuild itself with the python-exec
    // builder.
    let built = super::container::built_image(PY_IMAGE).expect("registered");
    assert_eq!(built.build_script, PYTHON_EXEC_BUILD_SCRIPT);
    assert!(
        super::repo_root().join(built.build_script).is_file(),
        "a registered build script must exist on disk"
    );
}
