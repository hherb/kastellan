//! Naming an attachment without retyping its sha256.
//!
//! `mail.get_attachment_text` originally took one parameter — the 64-char
//! sha256 from a `mail.get_message` attachment list — and that turned out to be
//! a parameter the planner cannot reliably supply. Live failure, task 160
//! (2026-08-17): the correct hash was the 6th string in the 4 KiB head the
//! planner received, at roughly byte 120, and the model still emitted a
//! *different* 64 hex chars. localmail answered `404 no extracted text for
//! attachment <hash>`, and the agent reported to the user that PDF extraction
//! had failed — while the extracted text (28 594 chars, containing the GST
//! figure that was asked for) sat in the database the whole time.
//!
//! When this was written, the core-side head the planner read was built by
//! `extract_scannable_text`: string values only, **keys discarded**. So a hash
//! arrived as an unlabelled 64-char hex blob that the model had to identify by
//! shape and then transcribe exactly. Since #677 the planner reads labelled
//! JSON, so the hash sits under `sha256`, but it is still 64 characters to copy
//! exactly. A `filename` beside a `message_id` is shorter, self-describing, and
//! — unlike a hash — a value a reader can *check* against the message it came
//! from.
//!
//! So the tool now accepts either form, and this module owns the pure half:
//! which form was named ([`choose`]), which attachment a filename picks
//! ([`pick`]), and what to tell the planner when the answer is none of them.
//!
//! Every string here is planner-facing. `core` clamps a failed step's detail to
//! [`kastellan_protocol::STEP_ERR_DETAIL_MAX`] chars before the planner sees it
//! (`core::scheduler::inner_loop::summary`), so advice past the cut is dropped
//! as surely as if it were never written — the same budget `ids::explain` is
//! written against, and the same reason its tests import the constant rather
//! than mirroring it.

use crate::ids::LocalmailId;

/// Which attachment the planner named, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// A sha256 copied verbatim by the planner. May well be wrong — see the
    /// module docs — so a 404 on this form gets [`missing_text_advice`]'s
    /// hash-suspicion arm.
    ///
    /// Already through [`Picked::from_planner_sha`]: a malformed hash is a
    /// params error, found while the params are read and before the handler
    /// asks localmail anything (#765) — not at the first use of the hash.
    Sha(Picked),
    /// A message plus (optionally) a filename within it. The sha256 is
    /// resolved from the message itself, so it cannot be mistyped.
    ///
    /// `expect_sha` carries a hash the planner supplied *alongside* the
    /// message. It selects (a hash is exact), but is checked against that
    /// message's attachments first, so a hallucinated one is caught by the
    /// message rather than by localmail's ambiguous 404. A **unique prefix**
    /// of at least [`SHA_PREFIX_MIN`] chars selects too, so the 12-char stem
    /// `mail.get_attachment` prefixes saved files with is enough. (It used to
    /// be the advertised repair when `filename` cannot discriminate; since
    /// #760 that repair is `index`.)
    ///
    /// `index` is the attachment's position in that message's `attachments`
    /// array — the `index` key `mail.get_message` writes into each entry
    /// ([`crate::detail`]) and the segment localmail's
    /// `/v1/messages/{id}/attachments/{index}` route takes. It selects outright,
    /// and is the one key that tells apart two attachments sharing a filename.
    InMessage {
        message_id: LocalmailId,
        filename: Option<String>,
        expect_sha: Option<String>,
        index: Option<usize>,
    },
}

/// One usable attachment of a message: its position in the served
/// `attachments` array (which counts unusable entries too, because localmail's
/// index route does), its filename (`""` when it has none) and its vetted
/// sha256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cand<'a> {
    index: usize,
    name: &'a str,
    sha: &'a str,
}

/// Longest filename this module will quote back. Filenames in the live archive
/// run to ~40 chars (`Download 470989752-e-ticket-DQXK68.pdf` is 38); the cap
/// bounds a hostile or generated name so the advice cannot grow with its input.
const NAME_HEAD: usize = 44;

/// How many candidates a repair message lists. Three is the most that could
/// *ever* fit beside any prose inside the planner's clamp (a fourth name at
/// [`NAME_HEAD`] would leave ~10 chars for the sentence); at live filename
/// lengths three do fit under most arms, and a name at the full [`NAME_HEAD`]
/// leaves room for two. Beyond that the list is elided with `…`, which still
/// tells the planner the parameter exists and that more values are available
/// via `mail.get_message`. [`with_candidates`] derives the real budget at
/// runtime, so this is a ceiling, not a promise.
const MAX_LISTED: usize = 3;

/// Shortest sha256 prefix [`pick`] will accept beside a `message_id`.
///
/// Below the 12 chars `mail.get_attachment` prefixes saved files with, so that
/// stem — a key the planner has seen in this worker's own output — always
/// qualifies. Short enough to transcribe, and it must still resolve
/// to exactly one attachment *of that message* — a collision inside one
/// message is refused, not guessed.
const SHA_PREFIX_MIN: usize = 8;

/// How many chars of a sha256 a repair message quotes. Also `ids::explain`'s
/// convention and `safe_attachment_name`'s stem length.
const SHA_HEAD: usize = 12;

/// One attachment, as localmail itself names it.
///
/// The filename comes back beside the hash because `mail.get_attachment` writes
/// the file to disk and needs a name for it — and the *requested* filename is
/// not that name: substring matching means the planner may have asked for
/// `e-ticket-DQXK68.pdf` and been given
/// `Download 470989752-e-ticket-DQXK68.pdf`. Saving under what was typed rather
/// than what was found would put a file on disk under a name the archive does
/// not use.
/// Both fields are **private**, and every constructor validates the hash. That
/// is the `LocalmailId` rule applied to the other URL segment: a `Picked` that
/// exists is one whose `sha256` is safe to interpolate. It matters because the
/// fields used to be `pub` while the constructor was private, so `handler` —
/// a sibling module, which cannot see a private `fn new` — had no way to build
/// one *except* by struct literal, i.e. the design forced every caller outside
/// this module to bypass the validation. Nothing catches that at review time
/// twice running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    sha256: String,
    /// localmail's own filename for the blob; `None` when the entry carried none.
    filename: Option<String>,
    /// Where it sits: the message and the position within it. `None` for a
    /// planner-typed hash, which names bytes rather than a message entry.
    at: Option<(LocalmailId, usize)>,
}

impl Picked {
    /// An entry resolved out of a message's own attachment list, whose hash
    /// [`pick`] has already vetted with [`is_sha256`].
    fn resolved(message_id: LocalmailId, c: Cand<'_>) -> Self {
        debug_assert!(is_sha256(c.sha), "pick must only yield vetted hashes");
        Self {
            sha256: c.sha.to_string(),
            filename: (!c.name.is_empty()).then(|| c.name.to_string()),
            at: Some((message_id, c.index)),
        }
    }

    /// A hash the **planner** typed, with no message to vouch for it.
    ///
    /// The only entry point from outside this module, and it is fallible — so
    /// the traversal guard on the `{sha256}` URL segment is structural rather
    /// than a rule each call site has to remember. There is no archive
    /// filename here: the planner named a hash, not a message, so there is
    /// nothing authoritative to save it under.
    pub fn from_planner_sha(sha256: &str) -> Result<Self, String> {
        if is_sha256(sha256) {
            Ok(Self { sha256: sha256.to_string(), filename: None, at: None })
        } else {
            Err(format!(
                "sha256 must be 64 lowercase hex chars, got {:?}",
                sha256.chars().take(8).collect::<String>()
            ))
        }
    }

    /// The hash to interpolate. 64 lowercase hex chars by construction.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The filename to save under, or `None` when the archive had no name for
    /// it (so the caller falls back to what the planner asked for, then to a
    /// sha-derived stem).
    pub fn save_name(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// The entry's position in its message, when it was resolved from one.
    pub fn index(&self) -> Option<usize> {
        self.at.map(|(_, i)| i)
    }

    /// Where localmail serves the original bytes.
    ///
    /// An entry resolved from a message is fetched **by position**
    /// (slice D): that route re-checks the message's ACL and sends the entry's
    /// own filename, where the hash route sends whichever name the *earliest*
    /// carrying message used — wrong for 1,109 live blobs that carry more than
    /// one. A planner-typed hash has no message, so it keeps the hash route.
    ///
    /// Every segment is interpolated from a validated type (`LocalmailId`, a
    /// `usize`, a vetted sha), so no string the planner wrote reaches a path.
    pub fn blob_path(&self) -> String {
        match self.at {
            Some((message_id, index)) => {
                format!("/v1/messages/{message_id}/attachments/{index}")
            }
            None => format!("/v1/attachments/{}", self.sha256),
        }
    }

    /// Where localmail serves one page of the extracted text: `limit`
    /// characters from character `offset`. Both text routes page the same way.
    ///
    /// Unlike [`Self::verify_bytes`], nothing checks that a page fetched by
    /// position belongs to [`Self::sha256`]: extracted text carries no hash to
    /// compare. The `sha256` a text result reports for a message-resolved pick
    /// is therefore the one the message *listed* — sound while localmail's
    /// index route resolves positions in `get_message`'s order, which it
    /// documents (`serve/routes/messages.py::message_attachment_text`).
    pub fn text_path(&self, offset: u64, limit: u32) -> String {
        format!("{}/text?offset={offset}&limit={limit}", self.blob_path())
    }

    /// Check that bytes fetched from [`Self::blob_path`] are the blob this pick
    /// names. `Err` is planner-facing service-fault text.
    ///
    /// Before slice D every fetch was by hash, so the `sha256` reported beside
    /// the saved file was true by construction. A message-resolved pick is now
    /// fetched **by position**, and the hash is only what the listing said sat
    /// there — so a localmail whose index route ever counted differently from
    /// `get_message`, or a message that changed between the two requests,
    /// would put one document on disk under another's hash and filename, and
    /// report success. localmail stores a blob under the sha256 of its bytes
    /// (`attachments.py`), so hashing them restores the guarantee.
    ///
    /// A planner-typed hash is fetched *by* that hash, so localmail has already
    /// matched it; it is not re-hashed.
    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        use sha2::{Digest, Sha256};
        let Some((message_id, index)) = self.at else {
            return Ok(());
        };
        if format!("{:x}", Sha256::digest(bytes)) == self.sha256 {
            return Ok(());
        }
        let prefix: String = self.sha256.chars().take(SHA_HEAD).collect();
        Err(format!(
            "localmail served attachment {index} of message {message_id} with bytes that are \
             not the sha256 its listing gave — a service fault, nothing was saved. Tell the \
             operator. Listed: {prefix}"
        ))
    }
}

/// Is this a sha256 this worker will interpolate into a URL path?
///
/// The single rule, shared by [`Picked::from_planner_sha`] (which turns a
/// `false` into the planner's repair text) and by [`pick`] (which merely skips
/// an entry). Two copies of a traversal guard is one copy too many.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Decide which form the planner used.
///
/// `Err` is the planner-facing repair text for a params object that names no
/// attachment, names two, or names one by a malformed `sha256`.
///
/// A `sha256` **with** a `filename` is accepted, and the filename ignored: the
/// two cannot contradict each other without fetching the message, the sha
/// already addresses one attachment unambiguously, and `mail.get_attachment`
/// takes a `filename` meaning something else entirely (the output name), so a
/// planner carrying it across from that tool is confused about the parameter,
/// not about which file it wants.
///
/// A `sha256` **with** a `message_id` used to be refused outright, on the
/// argument that the two could disagree. Live task 4 (Mac, 2026-08-17) showed
/// what that costs when they *agree*: the planner sent both, was refused, spent
/// an iteration, retried with the bare hash — and the file landed on disk as
/// `71aac4580932_attachment`, because the sha form has no archive name to save
/// under. Refusing a pair that is merely redundant is friction, not safety. The
/// message form is taken instead and the hash *verified* against that message's
/// attachments, which is strictly stronger than either alone: a hallucinated
/// hash is caught by the message it does not belong to, and the file still gets
/// the archive's own name.
///
/// An `index` without a `message_id` is refused: it is a position *within* one
/// message and means nothing on its own — and silently ignoring it beside a
/// `sha256` would fetch by hash while the planner believes it chose by position.
pub fn choose(
    sha256: Option<String>,
    message_id: Option<LocalmailId>,
    filename: Option<String>,
    index: Option<usize>,
) -> Result<Selector, String> {
    match (sha256, message_id) {
        (expect_sha, Some(message_id)) => {
            Ok(Selector::InMessage { message_id, filename, expect_sha, index })
        }
        (_, None) if index.is_some() => Err(
            "`index` is a position within one message — add the `message_id` of the \
             mail.get_message step that listed the attachment."
                .to_string(),
        ),
        (Some(sha256), None) => Picked::from_planner_sha(&sha256).map(Selector::Sha),
        (None, None) if filename.is_some() => Err(
            "`filename` alone cannot address an attachment — add the `message_id` of the \
             mail.get_message step that listed it."
                .to_string(),
        ),
        (None, None) => Err(
            "Name the attachment: pass the `message_id` of a mail.get_message step plus its \
             `filename`, or the attachment's `sha256`."
                .to_string(),
        ),
    }
}

/// Pick one attachment out of a `mail.get_message` `attachments` array, given
/// the index, hash and/or filename the planner named.
///
/// Resolution order: an `index` selects outright (see [`pick_by_index`]), and
/// any hash or filename beside it is a second opinion that must agree; then an
/// `expect_sha` (exact, or a unique prefix — see [`find_by_sha`]) selects,
/// because a hash is the exact identifier and the message has vouched for it;
/// otherwise the `filename` ladder runs (see [`pick_by_filename`]); a lone
/// attachment needs none of them. Ambiguity is refused rather than guessed, and
/// so is a second opinion that *contradicts* the selector: serving a document
/// the planner did not ask for is the failure this module exists to end, and a
/// wrong answer is worse than a repairable error.
///
/// Every refusal names a key that actually selects the candidates it lists —
/// see [`how_to_name`] for why a filename is not always one.
///
/// `message_id` goes into the repair text — the planner has to be able to match
/// the advice to the step it wrote — and into the [`Picked`], which fetches by
/// position within that message.
pub fn pick(
    attachments: &[serde_json::Value],
    filename: Option<&str>,
    expect_sha: Option<&str>,
    index: Option<usize>,
    message_id: LocalmailId,
) -> Result<Picked, String> {
    // An entry is usable only if it carries a sha256 of the right shape. A
    // missing one is real (localmail emits `"sha256": null` for a part whose
    // blob was never stored); a malformed one would reach a URL path. The
    // unusable names are kept rather than dropped: they are in the message the
    // planner just read, so "no attachment has that filename" about one of
    // them is a false statement about an entry it can see. Positions count
    // *every* entry, usable or not, because localmail's index route does.
    let mut usable: Vec<Cand<'_>> = Vec::new();
    let mut unstored: Vec<&str> = Vec::new();
    for (i, a) in attachments.iter().enumerate() {
        let name = a.get("filename").and_then(serde_json::Value::as_str).unwrap_or("");
        match a.get("sha256").and_then(serde_json::Value::as_str) {
            Some(sha) if is_sha256(sha) => usable.push(Cand { index: i, name, sha }),
            _ => unstored.push(name),
        }
    }

    if usable.is_empty() {
        // Three upstream states used to share one sentence, and it told the
        // planner to "re-check the mail.get_message output for one with a
        // sha256" — output that, in the commonest of them, visibly has none.
        return Err(if attachments.is_empty() {
            format!(
                "message {message_id} has no attachments — there is no file here to read, \
                 and its own text is already in the mail.get_message output."
            )
        } else {
            format!(
                "message {message_id} lists {} attachment(s), but localmail has stored the \
                 content of none of them, so none can be read.",
                attachments.len()
            )
        });
    }

    if let Some(i) = index {
        let c = pick_by_index(attachments.len(), &usable, i, message_id)?;
        // Second opinions beside an index must agree with it, for the reason
        // the sha/filename pair below must: two selectors that disagree are two
        // requests, and serving one of them silently is a wrong answer.
        if let Some(want_sha) = expect_sha {
            let want = want_sha.to_ascii_lowercase();
            // A correct but too-short prefix is not a disagreement, and calling
            // it one sends the planner to drop the wrong selector.
            if want.len() < SHA_PREFIX_MIN && !want.is_empty() && c.sha.starts_with(&want) {
                return Err(format!(
                    "a `sha256` prefix needs at least {SHA_PREFIX_MIN} characters — `index` \
                     alone already selects attachment {i} of message {message_id}."
                ));
            }
            if find_by_sha(&[c], &want).is_none() {
                return Err(format!(
                    "`index` and `sha256` name different attachments of message {message_id} \
                     — pass one, not both. `index` {i} is {}.",
                    head(c.name)
                ));
            }
        }
        // An empty filename "agrees" with every entry (`contains("")`), which
        // is the right answer rather than a gap: it names nothing, so it cannot
        // contradict the index, and the index alone selects.
        if let Some(want_name) = filename {
            if !c.name.to_lowercase().contains(&want_name.to_lowercase()) {
                return Err(format!(
                    "`index` and `filename` name different attachments of message \
                     {message_id} — pass one, not both. `index` {i} is {}.",
                    head(c.name)
                ));
            }
        }
        return Ok(Picked::resolved(message_id, c));
    }

    // A hash the planner supplied beside the message selects exactly, once the
    // message has vouched for it. Checked before `filename`, which in this form
    // is at most a second opinion — but a second opinion that *contradicts* is
    // refused rather than overridden.
    if let Some(want_sha) = expect_sha {
        let want = want_sha.to_ascii_lowercase();
        let Some(by_sha) = find_by_sha(&usable, &want) else {
            return Err(with_candidates(
                &format!(
                    "that `sha256` is not an attachment of message {message_id} — {}, \
                     exactly one of:",
                    how_to_name(&usable).0
                ),
                &usable,
            ));
        };
        // Both selectors present and naming different attachments: the planner
        // asked for two things and would silently get one. That is precisely
        // what `search_params::normalize_filters` refuses for a doubled id
        // filter, and here the cost is higher — `mail.get_attachment` would put
        // the *other* document on disk, under a name that matches it, and
        // report success.
        if let Some(want_name) = filename {
            if let Ok(by_name) = pick_by_filename(&usable, want_name) {
                if by_name.index != by_sha.index {
                    return Err(format!(
                        "`sha256` and `filename` name different attachments of message \
                         {message_id} — pass one, not both. The `filename` names {}.",
                        head(by_name.name)
                    ));
                }
            }
        }
        return Ok(Picked::resolved(message_id, by_sha));
    }

    let Some(want) = filename else {
        if let [only] = usable[..] {
            return Ok(Picked::resolved(message_id, only));
        }
        return Err(with_candidates(
            &format!(
                "message {message_id} has {} attachments — {}, exactly one of:",
                usable.len(),
                how_to_name(&usable).0
            ),
            &usable,
        ));
    };

    match pick_by_filename(&usable, want) {
        Ok(hit) => Ok(Picked::resolved(message_id, hit)),
        Err(candidates) if !candidates.is_empty() => Err(with_candidates(
            &format!(
                "`filename` matches {} attachments in message {message_id} — {}, exactly one of:",
                candidates.len(),
                how_to_name(&candidates).0
            ),
            &candidates,
        )),
        Err(_) if unstored.iter().any(|n| n.eq_ignore_ascii_case(want)) => Err(format!(
            "attachment {} of message {message_id} is listed but its content was never \
             stored, so it cannot be read.",
            head(want)
        )),
        Err(_) => Err(with_candidates(
            &format!(
                "No attachment in message {message_id} has that `filename` — {}, exactly one of:",
                how_to_name(&usable).0
            ),
            &usable,
        )),
    }
}

/// The usable entry at position `i` of a message with `len` attachments.
///
/// Out of range and unstored are told apart, because they need different
/// repairs: one is a wrong number, the other a real entry with nothing behind
/// it (the planner can see it listed, so "no such attachment" would be false).
fn pick_by_index<'a>(
    len: usize,
    usable: &[Cand<'a>],
    i: usize,
    message_id: LocalmailId,
) -> Result<Cand<'a>, String> {
    if i >= len {
        return Err(format!(
            "message {message_id} has {len} attachment(s), numbered from 0 — `index` {i} is \
             past the end. Copy an `index` from the mail.get_message step."
        ));
    }
    usable.iter().find(|c| c.index == i).copied().ok_or_else(|| {
        format!(
            "attachment {i} of message {message_id} is listed but its content was never \
             stored, so it cannot be read."
        )
    })
}

/// Resolve `want` against the attachment list by filename.
///
/// `Err` carries the ambiguous candidate set — empty when nothing matched at
/// all, so the caller can tell "which of these did you mean" from "none of
/// these". Matching is a ladder — exact, then case-insensitive, then
/// case-insensitive substring — and **the first tier with exactly one match
/// wins**. The substring tier is what lets a planner name `e-ticket-DQXK68.pdf`
/// for the archive's `Download 470989752-e-ticket-DQXK68.pdf`, which is the
/// right file by any reading.
///
/// The tier *order* reads strictest-first, but the result does not depend on
/// it, and saying otherwise would be prose asserting what the code does not do.
/// `exact ⊆ ci ⊆ sub`, so the tiers are nested and their sizes are monotone;
/// therefore any two tiers that each hold exactly one element hold the *same*
/// element, and a tier that does not hold exactly one returns nothing wherever
/// it sits. Reordering the array is a mutation no test can kill — deliberately,
/// and recorded here so the next reader does not go hunting for the test that
/// pins it. What *is* pinned is the behaviour that order was mistakenly
/// credited with: `receipt.pdf` beside `flight-receipt.pdf` resolves rather
/// than being refused, because the exact tier holds one name while the
/// substring tier holds two.
fn pick_by_filename<'a>(usable: &[Cand<'a>], want: &str) -> Result<Cand<'a>, Vec<Cand<'a>>> {
    let lower = want.to_lowercase();
    let exact: Vec<_> = usable.iter().filter(|c| c.name == want).copied().collect();
    let ci: Vec<_> = usable.iter().filter(|c| c.name.to_lowercase() == lower).copied().collect();
    let sub: Vec<_> =
        usable.iter().filter(|c| c.name.to_lowercase().contains(&lower)).copied().collect();
    for tier in [&exact, &ci, &sub] {
        if let [hit] = tier[..] {
            return Ok(hit);
        }
    }
    Err(sub)
}

/// Exact sha match, else a **unique** prefix of at least [`SHA_PREFIX_MIN`]
/// hex chars. `want` must already be lowercased.
///
/// The prefix arm is what lets the planner repair with a 12-char key rather
/// than 64 chars — the length `mail.get_attachment` prefixes saved files with
/// and the tool schema advertises. Resolution is still against
/// *this message's* attachments, and a prefix shared by two of them is refused
/// by the uniqueness check rather than guessed — so the widening cannot select
/// an attachment the planner did not name.
fn find_by_sha<'a>(usable: &[Cand<'a>], want: &str) -> Option<Cand<'a>> {
    if let Some(hit) = usable.iter().find(|c| c.sha == want) {
        return Some(*hit);
    }
    if want.len() < SHA_PREFIX_MIN || !want.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut hits = usable.iter().filter(|c| c.sha.starts_with(want));
    let first = *hits.next()?;
    hits.next().is_none().then_some(first)
}

/// Which parameter can actually select among these candidates — and therefore
/// which key [`with_candidates`] lists them under.
///
/// A `filename` selects only when the names are distinct and non-empty. Two
/// parts of one message really can share a name (`image001.png` is the
/// canonical case) or carry none at all, and there "copy one exactly" is
/// advice the planner cannot act on: it copies the name it was given, gets a
/// byte-identical error, and repeats to the iteration cap — the exact loop
/// this module exists to end. The position is the key that always
/// discriminates — 1–2 digits, where it used to be a 12-char sha prefix, which
/// also discriminates but has to be transcribed exactly (#760).
fn how_to_name(cands: &[Cand<'_>]) -> (&'static str, bool) {
    let names: std::collections::HashSet<&str> = cands.iter().map(|c| c.name).collect();
    if cands.iter().all(|c| !c.name.is_empty()) && names.len() == cands.len() {
        ("copy its `filename`", true)
    } else {
        ("pass its `index`", false)
    }
}

/// What to tell the planner when localmail has no extracted text for a sha256.
///
/// localmail returns the *same* 404 whether the blob is unknown or merely has
/// no `attachment_text` row yet (`api/attachments.py::get_attachment_text`), so
/// the two cases cannot be told apart from the response. What *can* be told
/// apart is where the hash came from: one this worker resolved out of a message
/// is right by construction, so suggesting the planner mistyped it would send
/// it to repair a parameter that was never wrong — the #536 defect, in a new
/// costume. Hence `planner_supplied`.
///
/// The sha is quoted **last** and as a 12-char prefix — the `ids::explain`
/// convention, so every word of advice sits at a fixed offset from the start
/// and the fit stops being an arithmetic property to re-check.
pub fn missing_text_advice(sha256: &str, planner_supplied: bool) -> String {
    let prefix: String = sha256.chars().take(SHA_HEAD).collect();
    if planner_supplied {
        format!(
            "No extracted text for that sha256 — most often a mistyped hash. Retry with \
             `message_id` + `index` (or `filename`) from the mail.get_message step, not a \
             hash. Got: {prefix}"
        )
    } else {
        format!(
            "That attachment has no extracted text (scanned image, or extraction has not run). \
             Use mail.get_attachment to save the original file instead. Got: {prefix}"
        )
    }
}

/// What to tell the planner when localmail has no *blob* for a sha256.
///
/// The `mail.get_attachment` counterpart of [`missing_text_advice`], and it
/// exists for the same reason: `attachments.py::_lookup_blob_row` raises the
/// same `NotFound` for a blob it has never seen and for one this agent's ACL
/// excludes, so the upstream sentence cannot tell the two apart and forwarding
/// it verbatim is what makes an agent report a fault that is not there.
///
/// The split on `planner_supplied` matters more here than for text, because
/// [`missing_text_advice`]'s resolved arm sends the planner *to this tool* —
/// so a raw 404 here is the second step of an advice chain, and the planner
/// has by then done everything it was told to.
pub fn missing_blob_advice(sha256: &str, planner_supplied: bool) -> String {
    let prefix: String = sha256.chars().take(SHA_HEAD).collect();
    if planner_supplied {
        format!(
            "localmail has no attachment with that sha256 — most often a mistyped hash. Retry \
             with `message_id` + `index` (or `filename`) from the mail.get_message step, not a \
             hash. Got: {prefix}"
        )
    } else {
        format!(
            "That attachment is listed on the message but its content is not stored (or is \
             outside this agent's mail ACL), so there is no file to save. Got: {prefix}"
        )
    }
}

/// Append as many candidates to `prose` as the planner's clamp will carry,
/// eliding the rest with `…`.
///
/// Each is rendered under the key that actually selects it ([`how_to_name`]):
/// its filename when the names discriminate, its `index` when they do not —
/// so the list is always something the planner can send back and have resolve.
///
/// The budget is *derived* from [`kastellan_protocol::STEP_ERR_DETAIL_MAX`]
/// rather than arithmetic done once in a comment, so lengthening the prose or
/// raising [`NAME_HEAD`] cannot silently push the list — the actionable half of
/// the message — past the cut. Room for the elision marker is reserved before
/// each name is admitted, which is what keeps the result inside the budget in
/// the listing branches; the zero-listed branch appends only 2 chars and is
/// bounded by the prose itself, whose longest arm is ~115 chars even with a
/// 19-digit id.
fn with_candidates(prose: &str, cands: &[Cand<'_>]) -> String {
    const ELISION: &str = ", …";
    let by_name = how_to_name(cands).1;
    let budget = kastellan_protocol::STEP_ERR_DETAIL_MAX;
    let mut out = prose.to_string();
    let mut listed = 0usize;
    for c in cands.iter().take(MAX_LISTED) {
        let shown = if by_name { head(c.name) } else { c.index.to_string() };
        let sep = if listed == 0 { " " } else { ", " };
        let need = sep.chars().count() + shown.chars().count() + ELISION.chars().count();
        if out.chars().count() + need > budget {
            break;
        }
        out.push_str(sep);
        out.push_str(&shown);
        listed += 1;
    }
    if listed < cands.len() {
        out.push_str(if listed == 0 { " …" } else { ELISION });
    }
    out
}

/// Truncate a rendered value so a message cannot grow with its input.
fn head(s: &str) -> String {
    if s.chars().count() <= NAME_HEAD {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(NAME_HEAD).collect::<String>())
    }
}

#[cfg(test)]
mod tests;
