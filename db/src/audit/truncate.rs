//! The payload cap: [`truncate_payload`] and the keys that survive it.
//!
//! Split out of `audit.rs` in a movement-only commit (the module doc there
//! still holds the policy prose, under "Truncation policy"). Every public
//! item below is re-exported from [`super`], so `kastellan_db::audit::…`
//! paths are unchanged.

use super::nul_escape::{self, NUL_ESCAPED_KEY};
use super::req_summary::{self, HEAD_MAX_BYTES, REQ_KEY, REQ_SUMMARY_KEY};

/// Maximum size in bytes of a serialised `audit_log.payload` JSONB
/// value before [`truncate_payload`] replaces it with a fingerprint
/// envelope.
///
/// 4 KiB is the same threshold called out in HANDOVER's Option I
/// brief. It comfortably holds a typical tool-call request/response
/// summary (`{"req": {...}, "result": {...}, "ms": 12}`) while
/// preventing any single row from dominating the `audit_log` heap or
/// the JSONL mirror line count.
pub const PAYLOAD_MAX_BYTES: usize = 4096;

/// Payload keys carried THROUGH truncation instead of being replaced by
/// the fingerprint envelope.
///
/// A key earns a place here only if it is all three of:
///
/// 1. **bounded by construction** — a fixed set of scalars, so it cannot
///    itself push the envelope over [`PAYLOAD_MAX_BYTES`];
/// 2. **a decision record, not data** — the outcome of a control, not the
///    document the control ran on. The cap exists to stop bodies dominating
///    the heap, and preserving a body under another name would defeat it;
/// 3. **irrecoverable** — it exists nowhere else *on the path that matters*.
///    `req` and `result` can be reconstructed from the worker and the
///    surrounding rows. A guard score is computed once, in memory; a Block
///    mirrors its `p`/`tau` onto the forensic `policy` / `injection.blocked`
///    row, but a **clear writes no second row at all**, and the cleared half
///    is exactly the half D5 needs.
///
/// `guard` is the wiring slice's per-dispatch guard-tier report
/// (`{state, p, tau, ms, body_byte_len, truncated, error_kind}`). Before it was listed
/// here, a tool result over the cap took the score with it — measured live
/// on 2026-08-23 at 85,352 bytes — which silently inverted spec D5: blocked
/// dispatches kept their score (their result is a short placeholder) while
/// *cleared* ones lost theirs, leaving a size-selected sample that reads
/// like a score distribution.
///
/// This is a **wire contract** in the same sense as
/// [`TRUNCATED_MARKER_KEY`]: readers may rely on a preserved key meaning
/// exactly what it meant in the untruncated payload.
///
/// **Array order is priority order.** Keys are admitted left to right
/// against a shared budget, so an earlier member can starve a later one.
///
/// **`guard` precedes `req_summary` deliberately (issue #617).** Both meet
/// criterion 3, so this is a judgement call and not a derivation: the guard
/// record is ~110 bytes against the summary's ~1.1 KiB worst case, and
/// losing it was the *measured* live defect that created this list, while
/// the summary's loss is at least *named* by [`DROPPED_PRESERVED_KEY`] when
/// it happens. Small, irrecoverable and historically lost beats large and
/// self-announcing.
///
/// **[`NUL_ESCAPED_KEY`] comes last (issue #816).** It meets all three
/// criteria — a single integer, the record of the NUL escape, and
/// irrecoverable once `␀` has replaced the NULs (a worker can send a
/// literal `␀`). It is the smallest member, so last costs it nothing in
/// practice; and if it were ever starved, [`DROPPED_PRESERVED_KEY`] would
/// still name it.
///
/// Contention is not reachable in production today — a real envelope peaks
/// near 1.4 KiB against a 4032-byte working budget — so the order is a
/// standing decision for the member after next rather than a live
/// tie-break. A re-ordering of the first two members is caught **twice**:
/// by `wire_key_literals_are_pinned`, which pins the array itself, and
/// behaviourally by `the_guard_record_is_admitted_before_the_req_summary`,
/// whose fixture makes each key fit alone but not both. Verified by
/// mutation, because the behavioural half was previously vacuous — its
/// fixture could not fit in either slot, so it passed with the array
/// reversed while claiming to assert the order. [`NUL_ESCAPED_KEY`]'s
/// position is pinned by `wire_key_literals_are_pinned` alone.
pub const PRESERVED_KEYS: &[&str] = &[GUARD_KEY, REQ_SUMMARY_KEY, NUL_ESCAPED_KEY];

/// The payload key under which `core::tool_host::post_process` records the
/// guard tier's per-dispatch verdict — and the first member of
/// [`PRESERVED_KEYS`], admitted ahead of [`REQ_SUMMARY_KEY`].
///
/// It is a `const` rather than two independent string literals because the
/// producer lives in another crate. Spelled twice, a rename on either side
/// would compile, and preservation would stop silently: cleared scores
/// would vanish above the cap while blocked ones survived, which is
/// precisely the defect [`PRESERVED_KEYS`] was added to fix, restored
/// without a single failing test. Spelled once, that rename is a compile
/// error.
pub const GUARD_KEY: &str = "guard";

/// Payload key naming the [`PRESERVED_KEYS`] that did **not** fit.
///
/// Without it, an envelope whose preserved key was dropped for size carries
/// the *same key set* as one whose payload never carried that key (the
/// fingerprints differ, but they describe inputs a reader does not have) —
/// so a reader cannot tell "the control never ran" from "the control ran,
/// and its verdict was discarded here". That ambiguity *is* the defect this
/// allowlist exists to fix, one function further down; leaving it in place
/// would be fixing the loss and keeping the silence.
///
/// A **wire contract** in the same sense as [`TRUNCATED_MARKER_KEY`]:
/// present only on an envelope, and only when something was actually lost.
pub const DROPPED_PRESERVED_KEY: &str = "_dropped_preserved";

/// Bytes held back from [`PAYLOAD_MAX_BYTES`] so [`DROPPED_PRESERVED_KEY`]
/// is *always* affordable.
///
/// A drop that could not be recorded would be a silent drop again, so the
/// room for the record is reserved before any preserved key is admitted
/// rather than hoped for afterwards. [`drop_marker_worst_case`] is checked
/// against this at compile time.
///
/// Public because [`truncate_payload`]'s one hard postcondition is stated
/// in terms of it: a caller reasoning about what will survive the cap
/// cannot do so without this number.
///
/// ⚠️ Headroom is thin: with three [`PRESERVED_KEYS`] the worst case is 61
/// of these 64 bytes, so a fourth member will need this raised. The
/// compile-time assertion refuses the build if it is not.
pub const DROP_MARKER_RESERVE: usize = 64;

/// The envelope's fingerprint keys. Private — [`is_truncation_envelope`] is
/// the supported way to read an envelope — but named once so the producer
/// and the compile-time shadow check below cannot drift apart.
const FINGERPRINT_SHA256_KEY: &str = "sha256";
/// See [`FINGERPRINT_SHA256_KEY`].
const FINGERPRINT_LEN_KEY: &str = "len";

/// `a == b` for `&str` in const context, which `PartialEq` is not.
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Worst-case serialised cost of adding [`DROPPED_PRESERVED_KEY`] to an
/// object that already has keys: one separating comma, the quoted key, a
/// colon, and an array naming every member of [`PRESERVED_KEYS`].
///
/// The comma is counted, not located. `serde_json`'s `Map` is a `BTreeMap`
/// unless `preserve_order` is enabled, so `_dropped_preserved` actually
/// sorts *before* `_truncated` and its separator is emitted after the pair
/// rather than before it — exactly one comma either way, and the count is
/// what the reserve is spent on.
///
/// Deliberately an over-estimate: the per-member term charges a trailing
/// comma to every element while `serde_json` emits only `N-1` of them.
const fn drop_marker_worst_case() -> usize {
    // `,"<key>":[]`
    let mut n = DROPPED_PRESERVED_KEY.len() + 6;
    let mut i = 0;
    while i < PRESERVED_KEYS.len() {
        // `"<key>",`
        n += PRESERVED_KEYS[i].len() + 3;
        i += 1;
    }
    n
}

/// The envelope's own keys are reserved, and the marker is affordable.
///
/// [`PRESERVED_KEYS`]' three admission criteria are prose, and a key can
/// satisfy all three and still be catastrophic: `len` or `sha256` would
/// silently overwrite the fingerprint — so two rows for the same body stop
/// comparing equal, which is precisely what
/// `a_preserved_key_does_not_change_the_fingerprint` was written to protect
/// and cannot catch for a key it does not name. [`TRUNCATED_MARKER_KEY`] is
/// worse still: shadowing it makes [`is_truncation_envelope`] report
/// whatever the payload happened to carry, resurrecting the issue-#62
/// misclassification the predicate exists to prevent.
///
/// Prose cannot enforce that. This can, at compile time, for every future
/// member — which is the same move the size postcondition already makes.
///
/// [`DROPPED_PRESERVED_KEY`] is checked against the same reserved names, and
/// not only against [`PRESERVED_KEYS`]: it is written onto the envelope by
/// the same `insert`, so pointing it at `len` would overwrite the
/// fingerprint just as surely — the failure this block exists to make
/// impossible, reachable through the one key it used not to look at.
const _: () = {
    // The three keys the envelope builds itself from. Shadowing any of
    // them means writing over the fingerprint, or over the marker that
    // makes the row self-declaring.
    //
    // `plan` is not one of them, and is here for a different reason:
    // promoting it would falsify
    // `core::observation::CapturedPlan::source_truncated`'s documented
    // guarantee that a null `plan_json` on a truncated row is an artefact
    // of truncation. A future member meets that reader here rather than in
    // production.
    let reserved =
        [TRUNCATED_MARKER_KEY, FINGERPRINT_SHA256_KEY, FINGERPRINT_LEN_KEY, "plan"];

    let mut j = 0;
    while j < reserved.len() {
        let mut i = 0;
        while i < PRESERVED_KEYS.len() {
            assert!(
                !str_eq(PRESERVED_KEYS[i], reserved[j]),
                "a PRESERVED_KEYS member may not shadow a reserved payload key"
            );
            i += 1;
        }
        // The marker is written onto the same envelope by the same
        // `insert`, so pointing it at `len` would overwrite the
        // fingerprint just as surely. Checking only PRESERVED_KEYS left
        // this reachable through the one key the block did not look at.
        assert!(
            !str_eq(DROPPED_PRESERVED_KEY, reserved[j]),
            "DROPPED_PRESERVED_KEY may not shadow a reserved payload key"
        );
        j += 1;
    }

    let mut i = 0;
    while i < PRESERVED_KEYS.len() {
        assert!(
            !str_eq(PRESERVED_KEYS[i], DROPPED_PRESERVED_KEY),
            "a PRESERVED_KEYS member may not shadow the dropped-key marker"
        );
        i += 1;
    }

    assert!(
        drop_marker_worst_case() <= DROP_MARKER_RESERVE,
        "DROP_MARKER_RESERVE no longer covers the marker PRESERVED_KEYS can produce"
    );

    // `req` may never be preserved. This is the single most damaging edit
    // possible to the list: `req` is unbounded by construction, so
    // allowlisting it would carry whole request bodies past the cap under
    // an allowlisted name — defeating the cap outright, which is the one
    // thing it exists to do. It fails admission criterion 1 and it is the
    // reason `req_summary` exists at all.
    //
    // It was prose in `req_summary`'s module doc, three feet from a block
    // already const-asserting five weaker invariants. A 3 KB `req` added
    // to the list would have compiled and shipped with nothing failing.
    let mut i = 0;
    while i < PRESERVED_KEYS.len() {
        assert!(
            !str_eq(PRESERVED_KEYS[i], REQ_KEY),
            "req is unbounded by construction and may never join PRESERVED_KEYS; \
             preserve the derived req_summary instead"
        );
        i += 1;
    }

    // The head must stay small enough that the summary cannot crowd the
    // budget. Order is what decides who wins under contention (`guard`
    // first), but order only matters once something does not fit, and a
    // later bump of HEAD_MAX_BYTES is what would get it there. The factor
    // of 2 is the worst-case re-escaping expansion documented on
    // HEAD_MAX_BYTES; 256 covers the fingerprint, the guard record and the
    // summary's own sub-keys.
    assert!(
        HEAD_MAX_BYTES * 2 + 256 + DROP_MARKER_RESERVE <= PAYLOAD_MAX_BYTES,
        "HEAD_MAX_BYTES is now large enough that a req_summary could starve the guard record"
    );
};

/// Payload key that marks a [`truncate_payload`] envelope. This is a **wire
/// contract**: readers in other crates (e.g. `kastellan-core`'s observation
/// capture, issue #62) detect truncation via [`is_truncation_envelope`], so
/// the writer and every reader share this single definition rather than
/// re-spelling the literal.
pub const TRUNCATED_MARKER_KEY: &str = "_truncated";

/// True iff `payload` is a truncation envelope produced by
/// [`truncate_payload`] — i.e. the original payload was over budget and
/// its keys were replaced by the `{_truncated, sha256, len}` fingerprint,
/// except any listed in [`PRESERVED_KEYS`], which ride along unchanged (and
/// [`DROPPED_PRESERVED_KEY`], which names those that could not).
///
/// The predicate deliberately tests only the marker, so adding a preserved
/// key stays additive for every existing reader.
///
/// Lives next to the producer so the two cannot drift: a shape change to the
/// envelope must update this predicate (and the shape-pin test below) in the
/// same file. Pure.
pub fn is_truncation_envelope(payload: &serde_json::Value) -> bool {
    payload
        .get(TRUNCATED_MARKER_KEY)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Returns the JSONB payload to *actually store* for a given input.
///
/// **First, every NUL is escaped** ([`nul_escape::escape_payload`], issue
/// #816): Postgres would refuse the row otherwise. Everything below — the
/// size test, the fingerprint, the request summary — sees that escaped
/// form, because it is the form that gets stored. A payload with no NUL is
/// untouched by the step.
///
/// If the escaped payload serialises to ≤ [`PAYLOAD_MAX_BYTES`], it is
/// returned unchanged. Otherwise it is replaced with:
///
/// ```json
/// { "_truncated": true, "sha256": "<64 hex>", "len": <bytes> }
/// ```
///
/// where `len` is the serialised byte length of the escaped input and
/// `sha256` is the lowercase-hex SHA-256 digest of the same bytes — **of the
/// escaped input, not of the envelope**, so two rows for the same body still
/// compare equal whatever else they carry. Both describe the stored form,
/// not the bytes the worker sent. An object body with NULs and the same body
/// with literal `␀`s differ in their [`NUL_ESCAPED_KEY`], so they do not
/// collide; a bare string or array has no key to differ in, and does.
///
/// Any [`PRESERVED_KEYS`] present in the escaped payload are then copied onto the
/// envelope **verbatim, whatever their value** — including a `null` or a
/// scalar, since this function judges keys and not shapes — because a
/// bounded decision record is not what the cap is defending against and is
/// worth more than the bytes it costs.
///
/// Keys are admitted **one at a time**, each against the budget less
/// [`DROP_MARKER_RESERVE`]. Two properties follow that an all-or-nothing
/// copy does not have: one oversized key cannot take a bounded sibling
/// down with it, and anything refused can still be *named*, under
/// [`DROPPED_PRESERVED_KEY`]. Nothing in this workspace can produce an
/// oversized preserved key. One is written by a caller and two by **this
/// function itself**, all bounded: `guard` by `core::tool_host::post_process`
/// via `GuardReport::audit_value` (a fixed handful of small scalars),
/// `req_summary` by `req_summary::summarize_req` ([`HEAD_MAX_BYTES`] plus a
/// digest and a length), and [`NUL_ESCAPED_KEY`] by
/// [`nul_escape::escape_payload`] (one integer). But the signature permits
/// an oversized one, and [`preserve_onto`] is tested against multi-key
/// lists that exercise it.
///
/// The budget postcondition is therefore structural rather than checked
/// after the fact: every admitted key left [`DROP_MARKER_RESERVE`] bytes
/// free, and the compile-time assertion above pins the marker's worst case
/// under that reserve.
///
/// Pure: deterministic, no I/O, no global state. Same input → same
/// output, every call.
pub fn truncate_payload(payload: serde_json::Value) -> serde_json::Value {
    // Escape before measuring: the stored form is what the cap, the
    // fingerprint and the request summary must all describe (issue #816).
    let mut payload = nul_escape::escape_payload(payload);

    // `to_vec` is infallible for `serde_json::Value` (the value is
    // already valid JSON in memory). The serialised form is what
    // Postgres will see — so that's the form we measure.
    let bytes = serde_json::to_vec(&payload).expect("serde_json::Value cannot fail to serialise");
    if bytes.len() <= PAYLOAD_MAX_BYTES {
        return payload;
    }

    // One renderer, shared with the request summary's digest: readers
    // compare both across rows, and two hex spellings that disagreed on
    // zero-padding would make rows silently incomparable.
    let hex = req_summary::sha256_hex(&bytes);

    let mut bare = serde_json::Map::new();
    bare.insert(TRUNCATED_MARKER_KEY.to_string(), serde_json::Value::Bool(true));
    bare.insert(FINGERPRINT_SHA256_KEY.to_string(), serde_json::Value::String(hex));
    bare.insert(FINGERPRINT_LEN_KEY.to_string(), serde_json::json!(bytes.len()));

    // ── Derive the bounded request summary (issue #617). ──
    //
    // Strictly after the fingerprint above, which is computed over `bytes`
    // — the serialisation of the escaped payload, the stored form. Inserting the
    // summary first would fold it into the digest and two rows for one
    // request body would stop comparing equal, which is the one thing the
    // fingerprint exists to do. `the_req_summary_does_not_change_the_
    // envelope_fingerprint` pins that order.
    //
    // Written onto the payload rather than handed to `preserve_onto`
    // separately so the summary is admitted, budgeted and — if it ever did
    // not fit — *named* by exactly the same machinery as every other
    // preserved key, with no second code path to keep in step. `payload` is
    // owned here and about to be dropped, so the insert costs one small
    // value and no clone of the oversized body.
    //
    // The derivation is the SOLE authority for this key, so the key is
    // cleared unconditionally and then re-derived. An overwrite that ran
    // only when a summary was produced left one escape: a payload carrying
    // `req_summary` with no `req` derived nothing, so nothing overwrote it,
    // and `preserve_onto` then copied the payload's own forged answer onto
    // the envelope verbatim — indistinguishable from a derived one.
    //
    // Computing the summary first also keeps this to a single object check
    // instead of the two nested ones it used to take, and those two tested
    // the same discriminant: `summarize_req` returns `Some` only for an
    // object, so the inner arm could never fail. Unreachable by coincidence
    // of two predicates is not the same as unreachable by construction, and
    // the failure it would have hidden — a summary silently evaporating —
    // is the shape this module exists to eliminate.
    let summary = req_summary::summarize_req(&payload);
    if let Some(obj) = payload.as_object_mut() {
        match summary {
            Some(s) => obj.insert(REQ_SUMMARY_KEY.to_string(), s),
            None => obj.remove(REQ_SUMMARY_KEY),
        };
    }

    // Only an object can have keys to preserve; a bare string or array
    // over the cap is all data by definition. Nothing is lost silently
    // here that the fingerprint does not already record: a non-object
    // payload has no keys, so no preserved key can have gone missing.
    let Some(source) = payload.as_object() else {
        return serde_json::Value::Object(bare);
    };

    preserve_onto(bare, source, PRESERVED_KEYS)
}

/// Copy each of `keys` from `source` onto `envelope`, admitting one at a
/// time against [`PAYLOAD_MAX_BYTES`] less [`DROP_MARKER_RESERVE`], and
/// naming under [`DROPPED_PRESERVED_KEY`] any that would not fit.
///
/// Split out of [`truncate_payload`] so `keys` can be a **parameter**.
///
/// With more than one member every branch here is *structurally* live — a
/// running envelope differs from a fixed one, a starved sibling is
/// expressible, and [`DROPPED_PRESERVED_KEY`] can name several keys. What
/// keeps the starvation arm from occurring in production is **sizing, not
/// cardinality**: a real envelope peaks near 1.4 KiB (fingerprint ~110 B,
/// guard ~110 B, summary ~1.1 KiB worst case, NUL count ~20 B) against a
/// 4032-byte working budget, and the compile-time block above pins
/// [`HEAD_MAX_BYTES`] so a later bump cannot quietly change that.
///
/// This paragraph used to say the multi-key half was unreachable, which
/// was true at one member and was falsified by the member added for issue
/// #617 — while still reading as an invitation to treat the arm as test
/// scaffolding. The properties are [`truncate_payload`]'s documented
/// postconditions either way, so they are tested here against multi-key
/// lists rather than argued.
///
/// Measurement is exact rather than estimated: the candidate is inserted,
/// the whole object is serialised, and a key that does not fit is taken
/// back out. `displaced` restores a value this would otherwise clobber —
/// unreachable for [`PRESERVED_KEYS`], which the compile-time block above
/// forbids from shadowing an envelope key, but `keys` is a parameter and a
/// helper that silently ate a fingerprint under test would be its own
/// small version of the defect this file is about.
///
/// Pure.
fn preserve_onto(
    mut envelope: serde_json::Map<String, serde_json::Value>,
    source: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> serde_json::Value {
    let mut dropped: Vec<&str> = Vec::new();
    for &key in keys {
        let Some(value) = source.get(key) else { continue };
        let displaced = envelope.insert(key.to_string(), value.clone());
        // Infallible for a reason stronger than "Values serialise": this
        // subtree was already serialised whole by the caller, so a second
        // pass over a subset of it cannot fail.
        let grown = serde_json::to_vec(&envelope).expect("a JSON object cannot fail to serialise");
        // The reserve is what keeps the ELSE arm affordable: a key refused
        // for size must still leave room to say so.
        if grown.len() + DROP_MARKER_RESERVE <= PAYLOAD_MAX_BYTES {
            continue;
        }
        match displaced {
            Some(prev) => envelope.insert(key.to_string(), prev),
            None => envelope.remove(key),
        };
        dropped.push(key);
    }

    if !dropped.is_empty() {
        envelope.insert(DROPPED_PRESERVED_KEY.to_string(), serde_json::json!(dropped));
    }
    serde_json::Value::Object(envelope)
}

#[cfg(test)]
mod tests;
