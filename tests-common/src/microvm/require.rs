//! REQUIRE-aware preconditions for micro-VM suites (#679).
//!
//! # The defect this closes
//!
//! #667 gave the micro-VM preflight a knob,
//! [`REQUIRE_ENV`](super::REQUIRE_ENV): set it, and every unmet micro-VM
//! precondition panics naming itself instead of printing `[SKIP]` and
//! reporting green. It covers what `skip_if_no_microvm` itself checks — the
//! Firecracker probe, the launcher, and image freshness.
//!
//! It did not cover the preconditions asked **beside** it:
//!
//! ```ignore
//! if skip_if_no_microvm(VM_ROOTFS) || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
//!     return;
//! }
//! let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
//! ```
//!
//! `||` short-circuits, so on exactly the host the operator cares about —
//! KVM, vsock, a built launcher, fresh images — control reaches
//! `skip_if_no_supervisor()`, which knows nothing about the knob. Missing
//! `loginctl enable-linger` then produces the same silent skip-as-pass the
//! knob exists to abolish, one helper to the right.
//!
//! ⚠️ **The issue counted one syntactic shape; the property was broader — and
//! the first re-derivation was still one site short.** Grepping for the `||`
//! chain the issue described found 7 call sites. Asking instead "which
//! preconditions inside a micro-VM-gated test bypass the knob" found **11
//! tests across 6 kinds** — the `||` chain, the same three helpers written as
//! sequential `if`s, `pg_bin_dir_or_skip`, `skip_if_origin_unreachable`,
//! `egress_proxy_bin_or_skip`, and four hand-written `eprintln!("[SKIP] …")`
//! broker-binary checks — plus the live-Matrix `gate()`, whose three
//! value-dependency checks are a 7th kind.
//!
//! Reviewing *that* turned up a **12th** site, in
//! `net_demo_firecracker_egress_e2e.rs`: a hand-written `[SKIP]` whose
//! `eprintln!` and format string rustfmt had split across two lines. The first
//! version of the source guard (`microvm::guard`) matched the macro and the
//! literal on one line, so it could not see it — **the census and the guard
//! shared a blind spot.**
//! That is why the guard is now written *fail-closed* against a shape rather
//! than against a roster of known-bad names, and why rule 2 tracks a macro
//! invocation across the lines it wraps onto.
//!
//! # The vocabulary
//!
//! Two combinators, matching the two shapes a precondition comes in:
//!
//! * [`skip_unless_ready`] for a `bool` precondition — takes [`Probe`]s, i.e.
//!   the `*_or_reason` siblings that return a reason without rendering a
//!   verdict (#653's pattern, reused rather than re-invented).
//! * [`dep_or_skip`] for one that yields a value (`Result<T, String>`).
//!
//! Both route the reason through [`super::report_unmet_microvm`], so a
//! `[SKIP]` and a REQUIRE panic cannot disagree about what was unmet.

use std::io::Write;

/// A precondition probe: `None` when met, `Some(reason)` when not.
///
/// The `*_or_reason` half of the tree's `*_or_reason` / `skip_if_*` split.
///
/// Named for the *bindings*, not the call sites. An array of distinct `fn`
/// items has no common type, so `let probes: [Probe; N] = [&a, &b];` and
/// [`host_probes`]'s return type must spell the trait object out or hit
/// `E0308: different fn items have unique types`. A call site needs no
/// annotation — the expected parameter type propagates into the array literal,
/// which is why `skip_unless_ready(&[&|| origin_unreachable_reason(HOST)])`
/// compiles bare — but the name is what makes the annotation readable where
/// one *is* required.
pub type Probe<'a> = &'a dyn Fn() -> Option<String>;

/// The first unmet precondition among `probes`, in order, or `None` when all
/// are met.
///
/// Pure over its injected probes, so the ordering and the short-circuit are
/// unit-testable with no host in the loop — the same seam
/// [`super::preflight`] uses, and for the same reason: #680's review found
/// that the impure half of the last such check could be replaced wholesale
/// with nothing failing.
///
/// **Short-circuiting is behaviour, not an optimisation.** These probes spawn
/// processes and open sockets — `default_probe` shells out, and
/// [`crate::skip::origin_unreachable_reason`] waits up to 5 s *per resolved
/// address* and tries them all, so a dual-stack host pays a multiple of that.
/// A probe after the first failure must not run: an offline host would
/// otherwise pay seconds to be told something the first probe already
/// established.
pub fn first_unmet(probes: &[Probe]) -> Option<String> {
    probes.iter().find_map(|probe| probe())
}

/// The two host preconditions every micro-VM **daemon** e2e needs, in
/// diagnosis order.
///
/// The 11 daemon-e2e sites #679 covers ask for exactly this pair, so naming it
/// once keeps the call sites to one line and stops the pair drifting apart the
/// way the byte-copied `[SKIP]` helpers this module was created to end did.
/// (The live-Matrix tier needs neither probe, and takes only [`dep_or_skip`].)
///
/// The order is a claim about usefulness, not an accident. Both are
/// independent host facts, but their remedies are not equally load-bearing:
/// `loginctl enable-linger` is a prerequisite for the Postgres cluster every
/// one of these sites brings up two lines later — `bring_up_pg_cluster`
/// installs and starts a *user service* — whereas the sandbox probe's remedy,
/// `scripts/linux/install-bwrap-apparmor-profile.sh`, unblocks only the
/// sandbox. Reporting the supervisor first therefore sends the operator to the
/// fix that unblocks the most, rather than to a database that was never the
/// problem.
pub fn host_probes() -> [Probe<'static>; 2] {
    [&HOST_PROBE_TABLE[0].1, &HOST_PROBE_TABLE[1].1]
}

/// A host precondition probe with the name it is reported under.
///
/// A plain `fn` rather than a [`Probe`], because [`HOST_PROBE_TABLE`] is a
/// `static` and a `dyn Fn` cannot be one.
type NamedProbe = (&'static str, fn() -> Option<String>);

/// [`host_probes`]' entries, name-tagged, in diagnosis order.
///
/// A table rather than an array literal inside [`host_probes`] so the
/// documented order can be *asserted*. The obvious test — compare
/// `host_probes()[0]` against `&supervisor_unavailable_reason` — cannot work:
/// a fn item is zero-sized, so every `&fn_item` shares one address and the
/// comparison is meaningless in both directions. Naming the entries gives the
/// order something to be wrong about.
///
/// [`host_probes`] is a mechanical projection of this table directly beneath
/// it; that projection is short enough to read but is not itself pinned by a
/// test, which is the honest limit of this arrangement.
static HOST_PROBE_TABLE: [NamedProbe; 2] = [
    ("supervisor", crate::skip::supervisor_unavailable_reason),
    ("sandbox", crate::sandbox::sandbox_unavailable_reason),
];

/// The order [`host_probes`] reports in, for the test that pins it.
#[cfg(test)]
pub(crate) fn host_probe_order() -> [&'static str; 2] {
    [HOST_PROBE_TABLE[0].0, HOST_PROBE_TABLE[1].0]
}

/// `[SKIP]` + `true` when any precondition is unmet, or panic when
/// [`super::REQUIRE_ENV`] demanded a real run.
///
/// The REQUIRE-aware replacement for OR-ing `skip_if_*` helpers beside
/// `skip_if_no_microvm`. Returns `true` so the call site stays the one-liner
/// it was:
///
/// ```ignore
/// if skip_if_no_microvm(VM_ROOTFS)
///     || skip_unless_ready(&[&supervisor_unavailable_reason, &sandbox_unavailable_reason])
/// {
///     return;
/// }
/// ```
///
/// # Panics
///
/// Under [`crate::gliner_e2e::UnmetAction::Fail`], naming both the knob and
/// the reason — see [`super::report_unmet_microvm`].
pub fn skip_unless_ready(probes: &[Probe]) -> bool {
    skip_unless_ready_to(probes, &mut std::io::stderr())
}

/// [`skip_unless_ready`] with the `[SKIP]` line written to `out`.
///
/// Exists so a unit test can prove the Skip arm **emits** the line without
/// emitting a real `[SKIP]` into the run it is protecting — asserting on
/// [`crate::skip::skip_line`] alone would leave the write deletable with the
/// suite still green, and `grep -c '^\[SKIP\]'` is how a run is audited here.
///
/// # Panics
///
/// As [`skip_unless_ready`].
pub fn skip_unless_ready_to(probes: &[Probe], out: &mut dyn Write) -> bool {
    match first_unmet(probes) {
        Some(reason) => super::report_unmet_microvm_to(&reason, out),
        None => false,
    }
}

/// Hand a required dependency through, or `[SKIP]` + `None` — or panic when
/// [`super::REQUIRE_ENV`] demanded a real run.
///
/// The value-returning sibling of [`skip_unless_ready`], for the
/// `let ... else { return; }` call sites:
///
/// ```ignore
/// let Some(bin_dir) = dep_or_skip(pg_bin_dir_or_reason()) else { return; };
/// ```
///
/// A missing dependency is as fatal under REQUIRE as a failed probe: both say
/// the run would prove nothing. That one arrives as a `Result` and the other
/// as an `Option<String>` is a shape difference, not a severity difference.
///
/// # Panics
///
/// As [`skip_unless_ready`].
pub fn dep_or_skip<T>(dep: Result<T, String>) -> Option<T> {
    dep_or_skip_to(dep, &mut std::io::stderr())
}

/// [`dep_or_skip`] with the `[SKIP]` line written to `out`, for the same
/// reason [`skip_unless_ready_to`] exists.
///
/// # Panics
///
/// As [`skip_unless_ready`].
pub fn dep_or_skip_to<T>(dep: Result<T, String>, out: &mut dyn Write) -> Option<T> {
    match dep {
        Ok(value) => Some(value),
        Err(reason) => {
            super::report_unmet_microvm_to(&reason, out);
            None
        }
    }
}
