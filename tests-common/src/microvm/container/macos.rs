use super::{
    classify_inspect_exit, container_preflight, find_image, image_age, parse_image_list,
    ContainerImage, InspectExit, CliFault,
};

/// Look one image up: `Ok(Some)` present, `Ok(None)` absent, `Err` a
/// fault — [`CliFault::Unavailable`] when the CLI could not answer,
/// [`CliFault::Unreadable`] when it answered unintelligibly.
///
/// Uses `container image inspect`, via the **production** argv producer
/// [`kastellan_sandbox::macos_container::build_image_inspect_argv`] rather
/// than a second spelling of the same command. #669 found the same bwrap
/// flag pair written out in three places and two of them wrong; the lesson
/// was to count the producers, so this crate does not become a fourth.
///
/// One call yields both facts the preflight needs — presence (exit status)
/// and build time (the JSON) — which is why it replaced the
/// `container image list` scan the three suites used to do. That scan is
/// also what made the substring defect possible: `inspect` takes the tag
/// as an argument, so there is nothing to match loosely.
pub fn inspect_image(reference: &str) -> Result<Option<ContainerImage>, CliFault> {
    let argv = kastellan_sandbox::macos_container::build_image_inspect_argv(reference);
    let command = argv.join(" ");
    // Bounded (#690): Apple `container` is daemon-backed, so a wedged
    // apiserver makes this BLOCK rather than fail. Unbounded, that stalled
    // a whole `cargo test --workspace` with no output at all.
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    let out = kastellan_sandbox::bounded_command::probe_output(
        &mut cmd,
        kastellan_sandbox::bounded_command::PROBE_BUDGET,
    )
    .map_err(|failure| match failure {
        kastellan_sandbox::bounded_command::ProbeFailure::Wedged(t) => {
            CliFault::Wedged(kastellan_sandbox::bounded_command::timed_out_reason(
                &command,
                &t,
                kastellan_sandbox::macos_container::WEDGED_APISERVER_HINT,
            ))
        }
        kastellan_sandbox::bounded_command::ProbeFailure::Spawn(e) => {
            CliFault::Unavailable(format!("could not spawn `{command}`: {e}"))
        }
    })?;
    // The exit status is the presence signal, and ONLY exit 1 means
    // absence — see `ABSENT_IMAGE_EXIT`. The classification is a pure
    // function so its fault arms are reachable from a unit test on any
    // host; this wiring must stay thin enough to have nothing of its own
    // to get wrong.
    let stderr = String::from_utf8_lossy(&out.stderr);
    match classify_inspect_exit(out.status.code(), &stderr, &command)? {
        InspectExit::Absent => return Ok(None),
        InspectExit::Present => {}
    }
    // A CLI that answered but cannot be parsed is `Unreadable`, never a
    // service fault: the remedy is this gate, not `container system start`.
    let images = parse_image_list(&String::from_utf8_lossy(&out.stdout))
        .map_err(CliFault::Unreadable)?;
    // Re-check the name the CLI handed back rather than trusting that it
    // answered the question asked. A record whose reference does not match
    // is reported as absent, which is the fail-closed direction.
    Ok(find_image(&images, reference).cloned())
}

/// `[SKIP]` + `true` when the macOS container tier cannot give this run
/// meaning — or panic when [`super::super::REQUIRE_ENV`] demanded a real
/// micro-VM run.
///
/// The single entry point the three container suites call, replacing the
/// three byte-copied private helpers that each folded a CLI fault into
/// "image not present" (#684) and none of which checked staleness at all
/// (#687).
///
/// # Panics
///
/// Under [`crate::gliner_e2e::UnmetAction::Fail`], naming the knob and the
/// reason.
pub fn skip_if_no_container(reference: &str) -> bool {
    container_preflight(
        reference,
        // `probe_fault`, not `probe`: the flattened `SandboxError` could
        // only be re-classified by matching on its prose, which is what
        // this module refuses to do anywhere else (#690).
        || {
            kastellan_sandbox::macos_container::MacosContainer::probe_fault().map_err(
                |fault| match fault {
                    kastellan_sandbox::macos_container::ProbeFault::Wedged(reason) => {
                        CliFault::Wedged(reason)
                    }
                    kastellan_sandbox::macos_container::ProbeFault::Unavailable(reason) => {
                        CliFault::Unavailable(reason)
                    }
                },
            )
        },
        || inspect_image(reference),
        |image, built| {
            let (stamps, unstat) = super::stamps_for(built);
            image_age(image.created_unix, &stamps, &unstat, super::now_unix())
        },
        super::super::report_unmet_microvm,
        super::super::report_caveat_microvm,
    )
}
