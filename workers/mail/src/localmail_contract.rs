// The localmail wire facts this worker depends on AND the live shape gate
// checks against the real service (#763).
//
// ⚠️ This file is compiled three times: here, as `mod localmail_contract`; by
// the live shape gate, `core/tests/mail_live_shape_e2e.rs`; and by
// `mock_localmail`'s tests in tests-common. Both `include!` it, so they check the
// values this worker actually sends rather than a hand-copied list (this crate
// is bin-only, so neither can import it). Two consequences:
//
// * Keep it to `pub const` items of plain types and pure `pub fn`s over plain
//   types — no `use` (spell `std::fmt::Display` out), no inner `//!` docs (an
//   `include!`d file may not carry inner attributes), nothing that needs this
//   crate's other modules. A route is a function rather than const fragments
//   (#767) so its separators and parameter order have one source too.
// * Every item here must be USED by the live gate. The gate does not
//   silence dead code (the mock's tests do, deliberately), so an item it
//   ignores is a `dead_code` warning there, which CI's
//   `cargo clippy … -D warnings` refuses — a wire fact added here without the
//   live gate referencing it does not pass CI. (`cargo build`/`test` only
//   warn.) Referenced is not asserted: review that the new use actually checks
//   the value against the service, not merely sends it.

/// The `api_major` this worker speaks. A different major is a different API.
pub const API_MAJOR: u64 = 1;

/// The oldest `api_minor` this worker works against — slice E's `fields` /
/// `snippet_chars`, which is also the newest thing it uses. Compared with `>=`,
/// the way localmail's own clients pin it, so a later additive bump is fine.
pub const MIN_API_MINOR: u64 = 3;

/// The keys each `mail.search` hit is projected to — localmail's `fields`
/// (slice E, `api_minor` 3), sent on every search (#760).
///
/// Every hit used to carry eleven keys, and the planner reads a step through a
/// 16 KiB pruned view: #677's real searches came back at 27 KB and 17 KB, so
/// the tail of a `limit: 50` page never reached it. Dropped, with the reason:
///
/// * `folder`, `to` — always `null` / `[]` on the search path (localmail's own
///   census, and seen live 2026-09-26);
/// * `score`, `matched_arms` — ranking internals nothing downstream reads;
/// * `snippet_html` — replaced by `snippet`, the same plain text under an
///   honest name (it never held HTML).
///
/// Kept: the id to fetch with, the account (the search filters on it), and
/// what a reader needs to pick a message — subject, sender, date, whether it
/// has attachments, and a snippet. localmail refuses an unknown name with a
/// 400, so a typo here fails every search loudly rather than dropping a key.
pub const HIT_FIELDS: [&str; 7] =
    ["message_id", "account", "subject", "from", "date", "has_attachments", "snippet"];

/// Snippet window, in characters, sent as `snippet_chars`. localmail's default
/// is 200 and it may add a `…` at each end. 120 keeps enough of a sentence to
/// recognise a message; with [`HIT_FIELDS`] it took two live 50-hit pages from
/// 25.8 KB / 26.6 KB to 18.1 KB / 18.8 KB (Mac archive, 2026-09-26). Must stay within
/// localmail's `1 ..= snippet_max_chars` (default 1000), or every search 400s.
pub const SNIPPET_CHARS: u32 = 120;

/// The query `mail.get_message` adds to ask for the header list: localmail's
/// `headers` parameter, whose VALUE picks the shape — `list` is one
/// `{name, value}` per occurrence, in wire order (#760, localmail #381). The
/// parameter's NAME is the #500 lesson: this worker once sent
/// `full_headers=true`, which localmail silently drops, and every test agreed
/// with it because each was written from the same reading.
pub const HEADER_LIST_QUERY: &str = "headers=list";

/// localmail's route for the original bytes of one attachment, **by its
/// position** in a message's `attachments` array (slice D, `api_minor` 2).
/// The worker fetches every message-resolved attachment this way
/// (`attach::Picked::blob_path`): the route re-checks the message's ACL and
/// serves that entry's own filename. `/text` under it is the extracted text.
pub fn attachment_by_index_path(message_id: impl std::fmt::Display, index: usize) -> String {
    format!("/v1/messages/{message_id}/attachments/{index}")
}

/// localmail's route for the original bytes of a stored blob, **by its
/// sha256** — used only for a hash the planner typed, which names no message.
/// `/text` under it is the extracted text.
pub fn attachment_by_sha_path(sha256: &str) -> String {
    format!("/v1/attachments/{sha256}")
}

/// One page of an attachment's extracted text: `limit` characters from
/// character `offset`, under either route above (`blob_path` is what one of
/// them returned). Both text routes page the same way. The parameter NAMES are
/// the #500 lesson again: a renamed one is dropped silently by the service,
/// which then serves its default window, and every test written from our own
/// reading would still agree.
pub fn text_page_path(blob_path: &str, offset: u64, limit: u32) -> String {
    format!("{blob_path}/text?offset={offset}&limit={limit}")
}
