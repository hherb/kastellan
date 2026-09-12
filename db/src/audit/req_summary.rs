//! A bounded record of *what was requested*, for rows the payload cap
//! replaces with a fingerprint envelope.
//!
//! ## The problem this exists for (issue #617)
//!
//! [`super::truncate_payload`]'s policy is that operators tail the audit log
//! to see *who did what*, not to recover request bodies — so an over-cap
//! payload becomes `{_truncated, sha256, len}` and the request goes with it.
//!
//! That premise holds for a tool like `web.fetch`, where the bulk is the
//! fetched body under `result` and the request is a URL recoverable from the
//! surrounding rows. **It does not hold for `shell.exec`, where the argv IS
//! the act being audited.** A dispatch whose payload exceeds the cap — an
//! agent-generated script, a long heredoc, or simply a command that printed
//! a lot — stored a row that recorded the guard's opinion of the act and not
//! the act itself.
//!
//! ## Why a summary rather than the request
//!
//! `req` cannot join [`super::PRESERVED_KEYS`] directly: it fails the first
//! admission criterion outright, being unbounded by construction, and
//! carrying a body through the cap under another name is precisely what the
//! cap exists to prevent. What *is* admissible is a record computed from the
//! request that is bounded **by construction**:
//!
//! ```json
//! {"head": "{\"argv\":[\"/bin/bash\",\"-c\",\"set -euo …", "sha256": "<64 hex>", "len": 41234}
//! ```
//!
//! * `head` — a **bare** prefix of the request's serialised form, capped at
//!   [`HEAD_MAX_BYTES`]. No ellipsis or truncation marker is appended (the
//!   `…` in the sketch above is the doc's, not the value's); `len` against
//!   the head's byte length is the signal, so a `jq` filter must not look
//!   for a suffix. This is the half that answers "what ran": for
//!   `shell.exec` it names the interpreter and the first arguments, for
//!   `web.fetch` the URL.
//!
//!   ⚠️ **That "names what ran" rests on key order.** `serde_json`'s `Map`
//!   is a `BTreeMap`, so the identifying field survives the cut only while
//!   it sorts early — true today (`argv`, `url`, `code`), and false the day
//!   a tool gains a bulky parameter sorting ahead of it (`attachments`,
//!   `body`). A prefix is deliberately generic — the chokepoint
//!   in `core::tool_host` dispatches every tool, and a summary shaped around
//!   one tool's parameter names (`argv0`, `argc`) would be empty noise for
//!   the rest and would put per-tool knowledge somewhere it does not belong.
//! * `sha256` — the digest of the **whole** serialised request, so two rows
//!   for the same request compare equal even when both heads were cut.
//! * `len` — the whole request's serialised byte length. `len` equal to the
//!   head's byte length is how a reader knows nothing was cut; `len` greater
//!   is how they know how much they are not seeing.
//!
//! ## Where it is computed, and why not at the producer
//!
//! Issue #617 proposed computing this at the producer, before the sink sees
//! the payload. It is computed **here**, inside [`super::truncate_payload`],
//! for the reason this tree keeps relearning: *count the producers*. There
//! are already two writers of `req` (`core::tool_host::post_process` and
//! `core::scheduler::tool_dispatch`), every future write site is one more,
//! and a producer-side rule is one each of them can silently omit. Derived
//! centrally it covers every write site that has ever existed and every one
//! that will, and a site with no request simply gets no summary.
//!
//! **Precisely: it covers every write site that spells the request
//! [`REQ_KEY`].** That is what the `const` exists to make true — a *rename*
//! is a compile error on both sides. It is not a guarantee against a future
//! producer inventing `params`, `args` or `body`: such a row gets no summary
//! and no marker, which is the #617 defect re-armed. The claim is "one rule,
//! centrally enforced", not "no producer can evade it by choosing another
//! word".
//!
//! It is also cheaper than a producer-side rule — the digest is taken only
//! on the path that was going to lose the request, not on every dispatch.
//! Not *free*, though: on the over-cap path the payload is serialised and
//! digested once, and then `req` is serialised and digested again. On the
//! motivating case, where `req` *is* the bulk, that roughly doubles the
//! work on this path.
//!
//! ## Secrets
//!
//! Both producers are safe, for two different reasons, and a reader
//! auditing either needs the one that applies to it.
//!
//! * **The dispatch chokepoint** records the **pre-substitution** snapshot
//!   (`core::tool_host::dispatch` clones `params` before redemption), so it
//!   holds opaque `secret://` references and never a redeemed plaintext.
//! * **The scheduler producer** writes its rows on pre-dispatch failure
//!   paths (`step.unknown_tool`, `step.spawn_failed`), before
//!   `core::tool_host::dispatch` is entered at all, so no substitution has
//!   run on the parameters it clones.
//!
//! Either way the head cannot surface a secret that the un-truncated `req`
//! would not already have written in the clear on every under-cap row.
//!
//! Pure: every function here is deterministic, does no I/O and touches no
//! global state.

/// The payload key under which a dispatch records the tool call's request
/// parameters.
///
/// A `const` rather than a literal for the same reason as
/// [`super::GUARD_KEY`]: the producers live in another crate
/// (`core::tool_host::post_process` and `core::scheduler::tool_dispatch`).
/// Spelled independently on each side, a rename would compile and
/// summarisation would simply stop happening — no failing test, no missing
/// key, just rows that quietly went back to recording nothing about the act.
/// Spelled once, that rename is a compile error.
pub const REQ_KEY: &str = "req";

/// The payload key under which [`super::truncate_payload`] records the
/// bounded summary of [`REQ_KEY`].
///
/// A **wire contract** in the same sense as [`super::TRUNCATED_MARKER_KEY`].
///
/// Read it with [`super::is_truncation_envelope`], not on its own. The
/// precise guarantee is: **on an envelope, this key is whatever
/// [`super::truncate_payload`] derived and nothing else** — it is removed
/// unconditionally before the derivation runs, so a payload cannot smuggle
/// its own answer onto the row by supplying the key and omitting
/// [`REQ_KEY`].
///
/// It is *not* a guarantee about a row that is **not** an envelope. An
/// under-cap payload is stored verbatim, as it always has been, so a
/// producer that wrote this key would see it survive. No producer does,
/// and adding one would be writing an answer to a question only the
/// truncation rule is allowed to answer.
pub const REQ_SUMMARY_KEY: &str = "req_summary";

/// Maximum bytes of the serialised request retained in the summary's `head`.
///
/// 512 bytes comfortably holds an interpreter path, its flags and the first
/// lines of a script — the part a forensic reader needs.
///
/// **This caps the prefix, not what it costs on the row.** `head` is a
/// prefix of *already-serialised* JSON and is then serialised again as a
/// JSON string value, so every `"` and `\` in it is escaped a second time.
/// A request is quote-dense by construction (`{"argv":["…","…"]}`), so a
/// 512-byte prefix can store as ~1026 bytes, and the finished envelope —
/// fingerprint, guard record and summary together — peaks near 1.4 KiB
/// against [`super::PAYLOAD_MAX_BYTES`]. That is **three to five times**
/// under the cap, not the order of magnitude an earlier version of this
/// comment claimed. 2× is the hard ceiling on the escape expansion:
/// multi-byte characters pass through unescaped, and control characters
/// cannot appear because `serde_json` has already escaped them.
///
/// The margin is what keeps the summary affordable; what actually stops it
/// starving the guard record is [`super::PRESERVED_KEYS`] **order**, which
/// admits `guard` first. Raising this constant erodes the first and does
/// not touch the second, so the relation is pinned at **compile time** by
/// the `const _: () = { … }` block in `super` rather than left to this
/// paragraph.
///
/// It is also deliberately **not** a multiple of 3. That does not by itself
/// exercise the walk in `char_boundary_prefix` at the production cap —
/// whether the walk runs depends on where a multi-byte character happens to
/// land, and a plain ASCII argv never straddles anything. What it does is
/// keep a *uniform multi-byte fixture* straddling, which is what
/// `a_multibyte_request_head_is_never_cut_mid_character` relies on. That is
/// the exact hole issue #591 documents in a copy of this idiom elsewhere in
/// the tree.
pub const HEAD_MAX_BYTES: usize = 512;

/// Sub-key of the summary object holding the bounded request prefix.
const HEAD_KEY: &str = "head";
/// Sub-key of the summary object holding the whole request's digest.
const SHA256_KEY: &str = "sha256";
/// Sub-key of the summary object holding the whole request's byte length.
const LEN_KEY: &str = "len";

/// The longest prefix of `s` that is at most `cap` **bytes** and ends on a
/// UTF-8 character boundary.
///
/// `&s[..cap]` panics when `cap` lands inside a multi-byte character, so the
/// index is walked back until it does not. Index `0` is always a boundary,
/// so the walk terminates.
///
/// Returns `s` unchanged when it already fits, and `""` for `cap == 0`.
///
/// **On duplication:** the same six-line idiom is hand-written in five other
/// places in this workspace, which is issue #591. This copy is not the
/// shared home that issue asks for and must not become it — the other five
/// live in `core` and in three workers, and workers deliberately do not
/// depend on `kastellan-db` (memory access is core-only, a threat-model
/// invariant). Folding them together needs a crate all six can see. What
/// this copy *does* carry is the straddling test #591 records as the one the
/// duplicates got wrong.
///
/// `pub(crate)` deliberately, and not `pub`: exporting it from
/// `kastellan-db` is the mechanism by which this copy would *become* the
/// shared home the paragraph above forbids, whichever crate reached for the
/// convenient existing symbol first. The prohibition gets a compiler behind
/// it rather than only a comment.
pub(crate) fn char_boundary_prefix(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    // `is_char_boundary(0)` is always true, so this cannot run past the
    // start of the string and `end` cannot underflow.
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Lowercase-hex SHA-256 of `bytes`.
///
/// Shared with [`super::truncate_payload`]'s envelope fingerprint rather
/// than written twice: the two digests are compared by readers looking for
/// the same request or the same payload across rows, and two independent
/// hex renderings that disagreed on, say, zero-padding would make rows
/// silently incomparable rather than fail anything.
///
/// ⚠️ **That argument was applied inside this module and not one module
/// over.** `crate::asks::sha256_hex` is the same algorithm and the same
/// rendering over a `&str`, so `kastellan-db` currently defines this twice.
/// Issue #696. Note this is **not** the #591 duplication and #591's excuse
/// does not cover it: those five copies cannot be folded together because
/// they live in `core` and in three workers, and workers deliberately do
/// not depend on `kastellan-db`. Both of these are inside this crate.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    use std::fmt::Write;

    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        // Two lowercase hex chars per byte, width-padded so a leading zero
        // in any byte survives — the canonical reproducible-hex idiom.
        write!(&mut hex, "{:02x}", b).expect("write to String cannot fail");
    }
    hex
}

/// The bounded summary of `payload`'s [`REQ_KEY`], or `None` when there is
/// no request to summarise.
///
/// `None` is returned for a payload that is not a JSON object and for one
/// that carries no [`REQ_KEY`] — an audit row with no request claims
/// nothing about one, which keeps "the producer wrote no request" distinct
/// from "the request was summarised". A request that is present but `null`
/// *is* summarised, because this function judges the key and not the shape,
/// exactly as [`super::truncate_payload`] does for preserved keys.
///
/// `pub(crate)`: [`super::truncate_payload`] is the sole authority for this
/// key and the only caller. A second caller outside this crate could put a
/// summary on a row that the truncation rule never derived.
pub(crate) fn summarize_req(payload: &serde_json::Value) -> Option<serde_json::Value> {
    let req = payload.as_object()?.get(REQ_KEY)?;

    // Infallible: `req` is already a valid `serde_json::Value` in memory.
    // The serialised form is what an operator would have read off the row,
    // so it is the form measured, digested and cut.
    let text = serde_json::to_string(req).expect("serde_json::Value cannot fail to serialise");

    Some(serde_json::json!({
        HEAD_KEY: char_boundary_prefix(&text, HEAD_MAX_BYTES),
        SHA256_KEY: sha256_hex(text.as_bytes()),
        LEN_KEY: text.len(),
    }))
}

#[cfg(test)]
mod tests;
