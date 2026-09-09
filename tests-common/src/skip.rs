//! `[SKIP]` early-return helpers.
//!
//! The pattern: print `[SKIP] <reason>` to stderr and return `true` (or
//! `None`) so the calling test can `return` immediately. The eprintln!
//! is load-bearing — a green CI run with `[SKIP]` lines means the test
//! never executed its assertions, not that containment held. Visible
//! only under `cargo test -- --nocapture`.

use std::path::PathBuf;
use std::time::Duration;

use kastellan_db::{find_pg_bin_dir, pg_bin_dir_candidates_with_env_override};
use kastellan_supervisor::default_probe;

/// Render the one `[SKIP] <reason>` line every helper in this crate prints.
///
/// Pure, and that is the point: a `[SKIP]` line is **evidence** in this tree —
/// `cargo test -- --nocapture | grep -c '^\[SKIP\]'` is how a run is audited
/// for tests that reported green without executing anything. A unit test that
/// wants to pin the rendering must therefore be able to do so **without
/// emitting a line**, or it inflates the very count it is checking. Assert on
/// this; call the `skip_if_*` wrappers only from real fixtures.
///
/// `reason` is flattened to a single line. Probe errors are not single-line
/// values — both supervisor backends embed a `\n\n` operator hint
/// (`supervisor/src/systemd_user.rs`, `launchd_agents.rs`), and on macOS
/// unconditionally — so without this every such skip would emit a `[SKIP]`
/// line plus orphan continuation lines, and under
/// [`crate::gliner_e2e::UnmetAction::Fail`] a multi-line panic message. The
/// grep count survives either way; what does not is being able to say the
/// reason *is* one line, which the `*_or_reason` docs all promise.
pub fn skip_line(reason: &str) -> String {
    format!("\n[SKIP] {}\n", one_line(reason))
}

/// Collapse a probe reason to a single line.
///
/// Shared by [`skip_line`] and by [`crate::gliner_e2e::report_unmet_to`]'s
/// panic arm, so a reason renders the same way whichever verdict a caller puts
/// on it — a `[SKIP]` line and a demanded-run panic should not disagree about
/// what the reason *is*.
pub fn one_line(reason: &str) -> String {
    reason.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render a `[WARN] <reason>` line on the same stream as [`skip_line`].
///
/// The third state a preflight can be in. `[SKIP]` says a precondition is
/// unmet and the test did not run; a plain pass says it ran. Neither can say
/// *"it ran, but something about the run was not what you think"* — which is
/// exactly what an out-of-dialect `REQUIRE` flag and an unverifiable rootfs
/// image both are. Both were previously silent, and a silent caveat on a green
/// run is the false-green pattern one step removed.
///
/// Pure, and one renderer rather than the hand-written copy it replaces plus
/// the second copy this change would otherwise have added, for the same
/// reason [`skip_line`] is: `grep -c '^\[WARN\]'` over a run is evidence, so
/// the shape must not drift between callers. Flattened to a single line for
/// the same reason too.
pub fn warn_line(reason: &str) -> String {
    format!("\n[WARN] {}\n", one_line(reason))
}

/// How long to wait for a TCP connect when probing a real origin's reachability.
///
/// Paid **per resolved address**, not per probe: `origin_unreachable_reason_at`
/// tries every A/AAAA record before giving up, so a dual-stack host with a
/// blocked route waits a multiple of this. That is the cost
/// [`crate::microvm::first_unmet`]'s short-circuit exists to avoid paying twice.
const ORIGIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the user-level supervisor is unusable on this host, or `None` when it
/// is fine. The string is a *reason*, with no `[SKIP]` prefix, so a caller that
/// must not skip (see [`crate::gliner_e2e`]) can render it as a failure
/// instead. It may span lines; [`skip_line`] flattens it.
///
/// Probe failures are normal on headless Linux without
/// `loginctl enable-linger`, and on SSH-only macOS sessions where
/// `gui/<uid>` is unreachable.
///
/// `SupervisorError::Probe` — the variant both backends' `probe()` returns on
/// an unreachable user manager — already renders as `supervisor probe failed:
/// …`, so prefixing it again produced `supervisor probe failed: supervisor
/// probe failed: …`. But `default_probe` can also surface `Io`,
/// `CommandFailed` or `NotImplemented`, and the last of those carries no
/// "supervisor" context at all, so the prefix is added only when it is missing
/// rather than dropped outright.
pub fn supervisor_unavailable_reason() -> Option<String> {
    default_probe().err().map(|e| prefix_supervisor_context(&e.to_string()))
}

/// Pure: give a supervisor error the `supervisor …` context it may already have.
fn prefix_supervisor_context(rendered: &str) -> String {
    if rendered.starts_with("supervisor ") {
        rendered.to_string()
    } else {
        format!("supervisor unavailable: {rendered}")
    }
}


/// Returns `true` if the user-level supervisor probe fails. Caller
/// should `return` immediately so the test body never runs.
///
/// The skip-as-pass half of [`supervisor_unavailable_reason`].
pub fn skip_if_no_supervisor() -> bool {
    match supervisor_unavailable_reason() {
        Some(reason) => {
            eprint!("{}", skip_line(&reason));
            true
        }
        None => false,
    }
}

/// Returns the discovered Postgres `bin/` directory, or the *reason* no known
/// PGDG / Homebrew layout was found on this host — no `[SKIP]` prefix, so a
/// caller that must not skip can render it as a failure.
///
/// Honours the `KASTELLAN_PG_BIN_DIR` env var via
/// [`pg_bin_dir_candidates_with_env_override`] so operators running on
/// Postgres.app or any non-standard install can opt in by exporting the
/// bin-dir path; see that helper's doc-comment for semantics.
pub fn pg_bin_dir_or_reason() -> Result<PathBuf, String> {
    find_pg_bin_dir(&pg_bin_dir_candidates_with_env_override())
        .map_err(|e| format!("no Postgres install found: {e}"))
}

/// The skip-as-pass half of [`pg_bin_dir_or_reason`]: print `[SKIP] <reason>`
/// and return `None` so test runs stay auditable.
pub fn pg_bin_dir_or_skip() -> Option<PathBuf> {
    match pg_bin_dir_or_reason() {
        Ok(dir) => Some(dir),
        Err(reason) => {
            eprint!("{}", skip_line(&reason));
            None
        }
    }
}

/// Why `host:443` is not reachable from this box, or `None` when it is.
///
/// A *reason*, with no `[SKIP]` prefix, so the call site decides the verdict —
/// #653's split. `skip_unless_ready(&[&|| origin_unreachable_reason(HOST)])`
/// renders it as a skip, or as a **failure** when the operator set
/// `KASTELLAN_MICROVM_REQUIRE_E2E`; see [`crate::microvm::skip_unless_ready`].
/// The rendering half (`skip_if_origin_unreachable`) was retired with #679's
/// review: every caller had moved to the knob-aware form, and leaving a
/// `[SKIP]`-rendering sibling alive is an invitation to bypass the knob again.
///
/// # Why these tiers need a real origin at all
///
/// Some egress e2e tiers need a **real public HTTPS origin** and cannot be made
/// hermetic. Two independent reasons, both structural rather than laziness:
///
/// * A **transparent-tunnel** (no-MITM) worker such as browser-driver does its
///   own end-to-end TLS, so it must trust the origin's certificate on its own
///   root store — a self-signed loopback origin would need a CA installed in the
///   guest's trust store.
/// * A **MITM** worker such as web-fetch has the reverse problem one hop later:
///   the egress proxy re-originates the connection and validates the origin
///   against `webpki_roots` only (`egress-proxy`'s `build_upstream_client_config`
///   has no extra-root knob), so a self-signed loopback origin fails at the
///   proxy's upstream leg.
///
/// Widening either trust store to make a test pass would weaken production, so
/// these tiers take the real-network dependency instead — and report cleanly
/// when the network is absent. That report is load-bearing: a silent skip is
/// exactly the false-green pattern `CLAUDE.md` warns about.
///
/// The two arms are distinguished on purpose: "cannot resolve" and "cannot
/// reach" are different remedies (DNS vs egress), and a reason that merged
/// them would send the operator to the wrong one.
pub fn origin_unreachable_reason(host: &str) -> Option<String> {
    origin_unreachable_reason_at(host, 443)
}

/// [`origin_unreachable_reason`] against an explicit port.
///
/// The port is a parameter only so both arms are reachable from a unit test: a
/// bound ephemeral listener exercises the `None` arm and a closed one the
/// "cannot reach" arm, with no network and no root. Before this seam the only
/// test could reach `Some`, so "never returns `None`" and "the two reasons are
/// swapped" both survived the suite.
///
/// Callers want [`origin_unreachable_reason`]: 443 is the port these tiers
/// actually need, and a caller free to pick one could probe a port the tier
/// never uses and call it reachable.
pub fn origin_unreachable_reason_at(host: &str, port: u16) -> Option<String> {
    use std::net::ToSocketAddrs;
    let addrs = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect::<Vec<_>>(),
        Err(e) => {
            return Some(format!("cannot resolve {host}: {e} (this tier needs outbound HTTPS)"))
        }
    };
    for addr in &addrs {
        if std::net::TcpStream::connect_timeout(addr, ORIGIN_PROBE_TIMEOUT).is_ok() {
            return None;
        }
    }
    Some(format!("cannot reach {host}:{port} (this tier needs outbound HTTPS)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `Probe` variant already names itself; prefixing again gave the
    /// operator `supervisor probe failed: supervisor probe failed: …`.
    #[test]
    fn an_already_contextual_supervisor_error_is_not_prefixed_twice() {
        assert_eq!(
            prefix_supervisor_context("supervisor probe failed: no bus"),
            "supervisor probe failed: no bus"
        );
        assert_eq!(
            prefix_supervisor_context("supervisor I/O error: broken pipe"),
            "supervisor I/O error: broken pipe"
        );
    }

    /// ...but a variant that does not name the subsystem still must, or the
    /// reason reads as an unattributed failure in a panic naming no component.
    #[test]
    fn a_context_free_supervisor_error_gets_the_prefix() {
        assert_eq!(
            prefix_supervisor_context("not yet implemented: default_probe"),
            "supervisor unavailable: not yet implemented: default_probe"
        );
    }

    /// `[WARN]` must be greppable on its own line and must NOT read as a
    /// skip: the two are counted separately when a run is audited, and a
    /// caveat that inflated the skip count would misattribute a test that
    /// actually ran.
    #[test]
    fn warn_line_is_greppable_and_is_not_a_skip() {
        let rendered = warn_line("image freshness could not be established");
        assert!(rendered.starts_with("\n[WARN] "), "must be its own line: {rendered:?}");
        assert!(rendered.ends_with('\n'), "must terminate the line: {rendered:?}");
        assert!(!rendered.contains("[SKIP]"), "a warning is not a skip: {rendered:?}");
        assert!(rendered.contains("freshness"), "must carry the reason: {rendered:?}");
    }

    /// Flattened for the same reason a `[SKIP]` reason is: an orphan
    /// continuation line cannot be attributed to the warning above it.
    #[test]
    fn warn_line_flattens_a_multi_line_reason() {
        assert_eq!(warn_line("two\n\n   lines"), "\n[WARN] two lines\n");
    }

    /// An unreachable origin yields a reason naming the host, whichever arm
    /// fires. `.invalid` is reserved by RFC 2606 precisely so it can never
    /// resolve, so this exercises the resolve arm on any sane resolver — and
    /// on a resolver that hijacks NXDOMAIN it falls through to the connect
    /// arm, which also returns a reason. Either way the contract holds:
    /// unreachable means `Some`, and the reason names the host.
    #[test]
    fn an_unreachable_origin_reason_names_the_host() {
        let reason = origin_unreachable_reason("kastellan-no-such-host.invalid")
            .expect("a reserved-TLD host is never reachable");
        assert!(reason.contains("kastellan-no-such-host.invalid"), "names the host: {reason}");
        assert!(!reason.contains("[SKIP]"), "a reason carries no verdict: {reason}");
    }

    /// **A reachable origin yields `None`.** Without this the whole function
    /// could stop returning `None` — every micro-VM tier that needs a real
    /// origin would then skip (or, under the knob, fail) forever, and the one
    /// test above would not notice. A bound ephemeral loopback listener is a
    /// reachable "origin" needing no network and no root.
    #[test]
    fn a_reachable_origin_yields_no_reason() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        assert_eq!(
            origin_unreachable_reason_at("127.0.0.1", port),
            None,
            "a listening port is reachable"
        );
    }

    /// The two arms say **different** things, and the distinction is the whole
    /// point: "cannot resolve" sends the operator to DNS, "cannot reach" to
    /// egress. Swapping the strings is invisible to any test that only checks
    /// the host is named.
    /// A loopback port that is closed **right now**, confirmed rather than
    /// assumed.
    ///
    /// ⚠️ Binding an ephemeral port and dropping the listener does NOT reserve
    /// it: the OS is free to hand the freed port straight to another process,
    /// and under a full parallel `cargo test --workspace` — which opens
    /// hundreds of sockets — it sometimes does. That raced in the sweep for
    /// PR #688 (`a closed port is unreachable`, once in three full sweeps,
    /// and **zero times in 40 isolated runs**), which is precisely the shape
    /// that gets mis-attributed to whatever change is in flight.
    ///
    /// Confirming and retrying makes the precondition true instead of likely.
    /// The retries are bounded because an environment where many consecutive
    /// freed ephemeral ports are instantly re-taken is one the caller should
    /// hear about, not one to spin in.
    fn a_closed_loopback_port() -> u16 {
        for _ in 0..16 {
            let port = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
                let p = l.local_addr().expect("local addr").port();
                drop(l);
                p
            };
            if origin_unreachable_reason_at("127.0.0.1", port).is_some() {
                return port;
            }
        }
        panic!("could not obtain a closed loopback port in 16 attempts");
    }

    #[test]
    fn the_resolve_and_reach_arms_name_different_remedies() {
        // Closed port on loopback: resolves, does not connect.
        let port = a_closed_loopback_port();
        let unreachable =
            origin_unreachable_reason_at("127.0.0.1", port).expect("a closed port is unreachable");
        assert!(unreachable.contains("cannot reach"), "connect arm: {unreachable}");
        assert!(unreachable.contains(&port.to_string()), "names the port: {unreachable}");

        let unresolvable = origin_unreachable_reason_at("kastellan-no-such-host.invalid", 443)
            .expect("a reserved-TLD host is never reachable");
        if unresolvable.contains("cannot reach") {
            // A resolver that hijacks NXDOMAIN reaches the connect arm; the
            // resolve arm is then untestable on this host and the assertion
            // below would be a lie rather than a check.
            eprint!(
                "{}",
                warn_line("resolver hijacks NXDOMAIN; the resolve arm was not exercised")
            );
        } else {
            assert!(unresolvable.contains("cannot resolve"), "resolve arm: {unresolvable}");
        }
    }

    /// A reason is one line whatever the probe embedded in it.
    #[test]
    fn one_line_collapses_embedded_hints_and_indentation() {
        assert_eq!(
            one_line("failed: no bus\n\n   The per-user manager\n   is not running."),
            "failed: no bus The per-user manager is not running."
        );
        assert_eq!(one_line("already one line"), "already one line");
    }
}
