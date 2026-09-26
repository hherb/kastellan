// The localmail wire facts this worker depends on AND the live shape gate
// checks against the real service (#763).
//
// ⚠️ This file is compiled three times: here, as `mod localmail_contract`; by
// the live shape gate, `core/tests/mail_live_shape_e2e.rs`; and by
// `mock_localmail`'s tests in tests-common. Both `include!` it, so they check the
// values this worker actually sends rather than a hand-copied list (this crate
// is bin-only, so neither can import it). Two consequences:
//
// * Keep it to `pub const` items of plain types — no `use`, no inner `//!`
//   docs (an `include!`d file may not carry inner attributes), nothing that
//   needs this crate's other modules.
// * Every constant here must be USED by the live gate. The gate does not
//   silence dead code (the mock's tests do, deliberately), so a constant it
//   ignores is a compile warning there, which CI's `-D warnings` turns into an
//   error — adding a wire fact here without checking it against the live
//   service does not build.

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
