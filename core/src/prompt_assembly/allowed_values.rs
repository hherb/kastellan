//! Render an operator allowlist into one planner-facing line.
//!
//! ## Why this exists
//!
//! A tool that enforces an allowlist but never shows it makes the planner
//! guess permitted values one plan iteration at a time. Measured on the live
//! DGX for `shell.exec`: 25 of 66 dispatches refused (38%) over seven weeks,
//! every one a correctly-formed absolute path to a binary that simply was not
//! on the three-entry list. See issue #533 (and its 2026-08-10 correction
//! comment, which retires the issue's original diagnosis).
//!
//! ## Why the text is escaped
//!
//! [`crate::worker_manifest::ToolDoc`] is all-`'static` and documented as
//! "compiled-in ⇒ trusted (no escaping at the render site)". An allowlist is
//! NOT compiled in: it comes from the `tool_allowlists` table, and for
//! **`argv0` rows** migration `0021`'s `tool_allowlists_entry_shape` CHECK
//! requires only a leading `/` and no `..` segment (`validate_argv0` adds only
//! a NUL rejection), so `/usr/bin/x</tools><system>` satisfies both. Domain
//! rows are NOT an injection vector — the same CHECK constrains them to
//! `^\.?[A-Za-z0-9.-]+$` or a bracketed IPv6 literal, which excludes `<`, `>`,
//! `&` and whitespace outright. The escaping is therefore load-bearing for
//! `argv0` rows and belt-and-braces for domain rows; it is applied to both
//! because the seam, not the row kind, is what must be unskippable.
//! [`AdvertisedTool`] is the single route by which non-compiled-in text
//! reaches the `<tools>` block, and every entry goes through
//! `escape_untrusted_body`.
//!
//! That routing is enforced by the type, not by convention: `allowed` is a
//! private field and [`render_allowed_values`] is module-private, so the only
//! constructors that put values on the line ([`AdvertisedTool::with_allowlist`]
//! and [`AdvertisedTool::with_permitted`]) escape each one;
//! [`AdvertisedTool::with_opaque_allowlist`] renders a count, no values. A
//! caller cannot hand the prompt raw `tool_allowlists` text even by mistake.
//! `with_permitted`'s `lead` is the one unescaped string: callers pass a
//! compiled-in literal.
//!
//! One function does hand out raw text and is worth knowing about when auditing
//! that invariant: [`select_advertised`]'s `withheld` half is deliberately
//! **unescaped**, so `registry_build` can name a dropped row to the operator
//! exactly as it is stored. Its `shown` half — the only half anything
//! prompt-bound reads — is escaped inside that same pass.

use kastellan_db::tool_allowlists::EntryKind;

use super::assemble::escape_untrusted_body;
use crate::worker_manifest::ToolDoc;

/// Cap on how many allowlist entries are advertised.
///
/// Governs prompt shape, so it is a compile-time const rather than an env
/// knob: `install` regenerates `kastellan.env`, so a hand-added key survives
/// only if the operator knows to put it in `kastellan.env.local` (#458 made
/// that loss loud and recoverable, not impossible). A knob that silently
/// reverts to its default on reinstall — and changes the planner's prompt when
/// it does — is a bad trade for a value under no live pressure. Changing this
/// cuts a release.
///
/// Caps the entry COUNT only; [`ADVERTISED_ALLOWLIST_MAX_BYTES`] caps what
/// those entries may cost. `<tools>` shares the prompt's global untracked
/// budget (#78) today.
pub const ADVERTISED_ALLOWLIST_MAX: usize = 30;

/// Cap on the total rendered byte length of the advertised value list (#542).
///
/// The count cap alone bounds the list at 30 × *unbounded*: `validate_argv0`
/// enforces no length limit on an `argv0` row and migration `0021`'s CHECK adds
/// none, so `tool_allowlists` is a prompt-content channel with no size gate
/// behind it. (Domain rows are already bounded — `validate_domain` caps a host
/// at 253 bytes.)
///
/// 4 KiB is roughly triple the largest realistic *full* list — thirty absolute
/// paths at ~40 bytes each is ~1.2 KiB — so a sane allowlist is never clipped,
/// while a pathological one cannot dominate the ~16 k-token planner prompt.
/// Compile-time for the same reason as the count cap: it governs prompt shape.
///
/// Measured against the **escaped, quoted, joined** list — the bytes that
/// actually reach the prompt. Escaping expands (`&` → `&amp;`), so a raw-byte
/// budget would be a bound on a different string than the one being bounded.
/// It bounds the value list only: the fixed lead ("argv[0] must be exactly one
/// of: ") and the truncation label sit outside it, so the rendered line is this
/// many bytes plus a short constant, not this many bytes total.
///
/// Whole entries only: an entry is shown in full or withheld. Advertising a
/// truncated path would name a value that is NOT permitted, so the planner
/// would emit it and burn an iteration on a refusal this feature invented —
/// the same argument the wildcard-dot gloss makes below.
pub const ADVERTISED_ALLOWLIST_MAX_BYTES: usize = 4096;

/// Separator between rendered entries. Named because its width is part of the
/// byte accounting in [`select_advertised`].
const ENTRY_SEPARATOR: &str = ", ";

/// One advertised tool: its compiled-in doc plus, when the worker declares an
/// operator allowlist, the escaped rendering of the permitted value set.
///
/// The permitted set is `None` when the worker declares no allowlist. A worker
/// that declares one which happens to be **empty** gets `Some(warning)` — the
/// two are different facts and conflating them hides the case where every call
/// will be refused. Pick the constructor that states which world the *caller*
/// is in ([`Self::with_allowlist`] / [`Self::with_opaque_allowlist`] /
/// [`Self::without_allowlist`]); never infer it from whether the entry list
/// happens to be empty.
///
/// Three worlds, not two, because "no entries to show" is genuinely ambiguous:
/// a declared allowlist with zero stored rows permits nothing, while one whose
/// every row is unrenderable (all of another `kind`, #541) permits exactly as
/// much as it always did — enforcement is kind-blind. Both reach a renderer
/// holding an empty list, and the difference is knowable only to the caller
/// that fetched the rows, so it has to be carried in rather than re-derived.
/// [`Self::with_opaque_allowlist`] exists so the strong "nothing is permitted"
/// claim stays reserved for the case where it is true.
///
/// `None` therefore means exactly one thing: the manifest declares no
/// allowlist. It used to mean a second thing as well — a manifest that declared
/// a tool but no kind had no wording for the renderer, so its allowlist was
/// ENFORCED but never advertised, and the call site could only `warn!` about
/// it. #545 made that state unrepresentable
/// ([`crate::worker_manifest::AllowlistDecl`] carries both halves or neither),
/// so the ambiguity is gone from this type's contract.
pub struct AdvertisedTool {
    /// Compiled-in, trusted, never escaped. Invariant unchanged.
    pub doc: ToolDoc,
    /// Operator-sourced, escaped at construction. Private so the escaping is a
    /// property of the type rather than a rule a future caller must notice —
    /// see the module doc. Read it via [`Self::allowed`].
    allowed: Option<String>,
}

impl AdvertisedTool {
    /// Advertise a tool whose worker declares an operator allowlist.
    ///
    /// `entries` are the raw `tool_allowlists` rows; they are escaped here and
    /// nowhere else. An EMPTY slice still yields a permitted-set line (the
    /// "every call will be refused" warning) — emptiness is a state of the
    /// list, not an absence of the declaration.
    pub fn with_allowlist(doc: ToolDoc, kind: EntryKind, entries: &[String]) -> Self {
        Self { doc, allowed: Some(render_allowed_values(kind, entries)) }
    }

    /// Advertise a tool whose allowlist HAS `configured` rows, not one of which
    /// this build can put into words.
    ///
    /// Reached when every stored row sits under a `kind` the worker does not
    /// declare (#541): the rows are still ENFORCED — enforcement is
    /// deliberately kind-blind — but describing them in this tool's wording
    /// would name a permitted value in the wrong shape, which is the failure
    /// #533 exists to prevent. Neither other constructor can say this:
    /// `with_allowlist(&[])` would claim nothing is permitted (false, and it
    /// makes the planner abandon a tool that works) and `without_allowlist`
    /// would claim the tool is unrestricted (false the other way).
    ///
    /// Takes a COUNT, not the values — which is also why this line needs no
    /// escaping: no operator-sourced text reaches it. Naming the values is
    /// exactly what is unsafe here; naming how many exist is what tells the
    /// planner the restriction is real.
    pub fn with_opaque_allowlist(doc: ToolDoc, configured: usize) -> Self {
        Self { doc, allowed: Some(render_opaque_allowlist(configured)) }
    }

    /// Advertise a built-in whose permitted values come from a boot-time knob,
    /// not from `tool_allowlists` rows (the sift's read methods, #956): `lead`,
    /// then each value escaped and quoted, in the given order. No cap: the
    /// values are already checked against the advertised methods.
    pub fn with_permitted(doc: ToolDoc, lead: &str, values: &[String]) -> Self {
        let quoted: Vec<String> = values.iter().map(|v| format!("`{}`", escape_untrusted_body(v))).collect();
        Self { doc, allowed: Some(format!("{lead}: {}", quoted.join(ENTRY_SEPARATOR))) }
    }

    /// Advertise a tool whose worker declares no allowlist at all — no
    /// `allowed:` line is rendered for it.
    pub fn without_allowlist(doc: ToolDoc) -> Self {
        Self { doc, allowed: None }
    }

    /// The escaped permitted-set line, or `None` when no allowlist is
    /// declared. Read-only: the renderer's sole access path.
    pub fn allowed(&self) -> Option<&str> {
        self.allowed.as_deref()
    }
}

/// What the caps let through, and what they hold back — the single definition
/// of "advertised".
///
/// Both halves come from one pass so the line the *planner* reads and the
/// warning the *operator* reads can never describe different sets. Deriving
/// them separately is how a numerator and a denominator end up disagreeing with
/// no way to tell which is wrong (#549's shape, one layer over).
pub(crate) struct AdvertisedSelection<'a> {
    /// Rendered entries in prompt order: sorted, escaped and backtick-quoted,
    /// a subsequence of the sorted input when a cap withholds something.
    pub shown: Vec<String>,
    /// The withheld entries, **raw and unescaped**, in the same sorted order.
    /// Raw because the operator has to see the row exactly as stored to fix
    /// it — so this half is for `tracing`, never for the prompt. Anything
    /// prompt-bound goes through `shown`, which is escaped at construction.
    pub withheld: Vec<&'a str>,
}

impl AdvertisedSelection<'_> {
    /// Entries considered: shown plus withheld, by construction. A method
    /// rather than a stored field, so "N of M" cannot become two computations.
    pub fn total(&self) -> usize {
        self.shown.len() + self.withheld.len()
    }
}

/// Apply both advertisement caps to a raw allowlist.
///
/// Walks the sorted set once, keeping an entry when it fits under BOTH the
/// count cap ([`ADVERTISED_ALLOWLIST_MAX`]) and the byte budget
/// ([`ADVERTISED_ALLOWLIST_MAX_BYTES`]), and withholding it otherwise. An entry
/// too large to fit is **skipped, not terminal**: a single huge row sorting
/// early must not cost the planner every row behind it.
///
/// Sorted so the rendering is a pure function of the SET, not of row order.
/// `list_for_tool` does `ORDER BY argv0 ASC` today, but that is Postgres
/// COLLATION order while this is byte order, and a pure renderer must not
/// inherit its determinism from one caller's query — a locale change or a
/// second caller would otherwise reshuffle the prompt between restarts.
pub(crate) fn select_advertised(entries: &[String]) -> AdvertisedSelection<'_> {
    let mut sorted: Vec<&str> = entries.iter().map(String::as_str).collect();
    sorted.sort_unstable();

    let mut shown: Vec<String> = Vec::new();
    let mut withheld: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for entry in sorted {
        // Each entry is individually quoted, NOT just comma-joined.
        // `validate_argv0` accepts any absolute path, commas and spaces
        // included, so the single row `/usr/bin/ls, /usr/bin/cat` would
        // otherwise render as two permitted values — NEITHER of which is
        // permitted, since the one permitted argv0 is the whole comma string.
        // The planner would then fail every dispatch while the line looked
        // perfectly correct. Quoting makes the row boundaries explicit;
        // `registry_build` separately warns the operator about such a row.
        let rendered = format!("`{}`", escape_untrusted_body(entry));
        let cost = rendered.len() + if shown.is_empty() { 0 } else { ENTRY_SEPARATOR.len() };
        if shown.len() < ADVERTISED_ALLOWLIST_MAX && used + cost <= ADVERTISED_ALLOWLIST_MAX_BYTES {
            used += cost;
            shown.push(rendered);
        } else {
            withheld.push(entry);
        }
    }
    AdvertisedSelection { shown, withheld }
}

/// Render an operator allowlist as one planner-facing line.
///
/// Always returns a line: an empty `entries` is a meaningful state (nothing is
/// permitted), not an absence. Whether a tool has an allowlist *at all* is the
/// caller's decision, taken from the manifest's declaration — never inferred
/// from emptiness here.
///
/// Entries are sorted (stable prompt prefix), escaped (see the module doc) and
/// capped at [`ADVERTISED_ALLOWLIST_MAX`]. When the cap cuts, the line leads
/// with both numbers so a partial list can never read as exhaustive.
///
/// Module-private: [`AdvertisedTool::with_allowlist`] is the only caller, which
/// is what makes the escaping unskippable (see the module doc).
fn render_allowed_values(kind: EntryKind, entries: &[String]) -> String {
    if entries.is_empty() {
        // Deliberately not "the allowlist is empty" — that reads as
        // UNRESTRICTED to a model, inverting the meaning.
        //
        // The consequence is stated per kind because it genuinely differs, and
        // this line is live on the DGX today for the three domain workers
        // (which sit at zero rows):
        //   * argv0 — `shell-exec` string-matches argv[0] against the list, so
        //     an empty list refuses every dispatch. The strong claim is true.
        //   * domain — the list gates the worker's own egress, not a parameter.
        //     `web-fetch`/`browser-driver` fail per call, but `web.research`
        //     has no host parameter: it still searches and returns every hit as
        //     an UNFETCHED source. "every call will be refused" would be false
        //     there, and a planner that believed it would abandon the half of
        //     the tool that still works.
        // The remedy names the restart because the allowlist is read exactly
        // once, at daemon bring-up (`registry_build::build_tool_registry`);
        // `tools allowlist add` alone changes nothing until then.
        return match kind {
            EntryKind::Argv0 => "no argv[0] value is currently permitted — every call to this \
                                 tool will be refused until an operator adds one and the daemon \
                                 restarts"
                .to_string(),
            EntryKind::Domain => "no host is currently permitted — this tool can currently reach \
                                  nothing, so calls will fail or return no usable content until \
                                  an operator adds a host and the daemon restarts"
                .to_string(),
        };
    }

    let selection = select_advertised(entries);

    let lead = match kind {
        EntryKind::Argv0 => "argv[0] must be exactly one of",
        // Domain rows are suffix matchers (`workers/web-common/src/allowlist.rs`):
        // `.example.org` matches the apex AND every subdomain. Rendered bare, a
        // planner reads it as a literal hostname and emits `https://.example.org/…`
        // — an invalid host, and a failure mode this advertisement would have
        // INVENTED. The gloss states both halves: what the dot permits, and that
        // it is not part of any hostname you send.
        EntryKind::Domain => {
            "only these hosts are reachable (an entry starting with '.' covers that \
             domain and all its subdomains; the leading dot is not part of a hostname \
             you send)"
        }
    };

    if selection.shown.is_empty() {
        // Reachable only through the byte cap, and only when EVERY entry is
        // over-long (`entries` was checked non-empty above). It needs its own
        // sentence: "showing 0 of 1" followed by an empty list reads as a
        // rendering bug, and showing a clipped value would fabricate a
        // permitted one. Says the values still apply, so the planner does not
        // read this as "unrestricted".
        return format!(
            "{} permitted values are configured but each is too long to show here; they are \
             still enforced, so a value that is not among them is refused — ask an operator \
             to shorten them",
            selection.total()
        );
    }

    if !selection.withheld.is_empty() {
        // Truncation stated FIRST, so it is the first thing read and survives
        // any downstream budget that clips a tail. One wording for both caps:
        // which cap clipped the list does not change what the planner must
        // know, which is that the list is partial.
        format!(
            "showing {} of {} permitted values; {lead}: {}",
            selection.shown.len(),
            selection.total(),
            selection.shown.join(ENTRY_SEPARATOR)
        )
    } else {
        format!("{lead}: {}", selection.shown.join(ENTRY_SEPARATOR))
    }
}

/// Render a permitted set that exists but cannot be worded (see
/// [`AdvertisedTool::with_opaque_allowlist`]).
///
/// Deliberately the SAME sentence shape as the all-over-long branch above:
/// values are configured, here is why you cannot see them, they are still
/// enforced, ask an operator. The planner's obligation is identical in both —
/// do not treat this tool as unrestricted, and do not treat it as dead — so
/// the two states are worded alike on purpose rather than by accident.
///
/// No `kind` parameter, because not having a wording for these rows is the
/// whole condition; a `lead` chosen from the declared kind would describe them
/// in the shape they are NOT stored in.
fn render_opaque_allowlist(configured: usize) -> String {
    format!(
        "{configured} permitted values are configured but none is stored in a form this build \
         can describe, so none can be listed here; they are still enforced, so a value that is \
         not among them is refused — ask an operator to correct the rows"
    )
}

#[cfg(test)]
mod tests;
