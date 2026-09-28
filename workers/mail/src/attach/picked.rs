//! [`Picked`]: one attachment, validated — the hash every URL path is built
//! from. Split out of `attach.rs` to bring that file back toward the 500-line
//! cap; the selection logic that produces a `Picked` stays there.

use super::{Cand, SHA_HEAD};
use crate::ids::LocalmailId;
// The route spellings, shared with the live shape gate (#767).
use crate::localmail_contract as contract;

/// One attachment, as localmail itself names it.
///
/// The filename comes back beside the hash because `mail.get_attachment` writes
/// the file to disk and needs a name for it — and the *requested* filename is
/// not that name: substring matching means the planner may have asked for
/// `e-ticket-DQXK68.pdf` and been given
/// `Download 470989752-e-ticket-DQXK68.pdf`. Saving under what was typed rather
/// than what was found would put a file on disk under a name the archive does
/// not use.
///
/// Every field is **private**, and every constructor validates the hash. That
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
    /// [`super::pick`] has already vetted with [`is_sha256`].
    ///
    /// Re-checked with `assert!`, not `debug_assert!`: this hash goes into a
    /// URL path, and a release build strips the debug form, leaving the guard
    /// a rule the one caller has to keep remembering.
    pub(super) fn resolved(message_id: LocalmailId, c: Cand<'_>) -> Self {
        assert!(is_sha256(c.sha), "pick must only yield vetted hashes");
        Self {
            sha256: c.sha.to_string(),
            filename: (!c.name.is_empty()).then(|| c.name.to_string()),
            at: Some((message_id, c.index)),
        }
    }

    /// A hash the **planner** typed, with no message to vouch for it.
    ///
    /// The only public constructor, reached in production through [`super::choose`]
    /// so that a malformed hash is refused while the params are read (#765).
    /// It is fallible so the traversal guard on the `{sha256}` URL segment is
    /// structural rather than a rule each call site has to remember. There is
    /// no archive filename here: the planner named a hash, not a message, so
    /// there is nothing authoritative to save it under.
    ///
    /// Uppercase hex is accepted and lowercased first (#768): it is the same
    /// hash, and the `message_id` form ([`ShaPrefix`]) has always taken it, so
    /// refusing it here made the same 64 characters valid in one form and not
    /// the other. The traversal guard is [`is_sha256`], applied *after*.
    pub fn from_planner_sha(sha256: &str) -> Result<Self, String> {
        let lower = sha256.to_ascii_lowercase();
        if is_sha256(&lower) {
            Ok(Self { sha256: lower, filename: None, at: None })
        } else {
            // What the planner sent, not the lowercased copy: the repair text
            // should quote its own input back to it.
            Err(format!(
                "sha256 must be 64 hex chars, got {:?}",
                sha256.chars().take(8).collect::<String>()
            ))
        }
    }

    /// Did the planner type this hash? Only then can a localmail 404 mean "you
    /// mistyped it" (see [`super::missing_text_advice`]); a hash resolved out
    /// of the message's own listing cannot be mistyped. Asked of the pick
    /// itself — the value actually fetched — rather than of the selector that
    /// produced it.
    pub fn is_planner_typed(&self) -> bool {
        self.at.is_none()
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
            Some((message_id, index)) => contract::attachment_by_index_path(message_id, index),
            None => contract::attachment_by_sha_path(&self.sha256),
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
        contract::text_page_path(&self.blob_path(), offset, limit)
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
/// `false` into the planner's repair text) and by [`super::pick`] (which merely skips
/// an entry). Two copies of a traversal guard is one copy too many.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A hash, or a hex prefix of one, that the planner sent **beside** a
/// `message_id` — lowercased, and of a shape some attachment could match.
///
/// Built only by [`ShaPrefix::parse`], while the params are read
/// ([`super::choose`]), so a value no attachment could ever match is refused
/// before the version gate and a GET of the message (#765), and every later
/// reader sees one spelling (#768). Whether it is the *right* prefix, and long
/// enough to select, only that message can say: [`super::pick`] decides.
///
/// Not a [`Picked`]: a prefix is not safe to interpolate into a URL, and is
/// never used as one — it only selects among the message's own vetted hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaPrefix(String);

impl ShaPrefix {
    /// `Err` is planner-facing repair text, and never echoes more than 8
    /// chars of what was sent (it may be a path).
    pub fn parse(sha: &str) -> Result<Self, String> {
        if !sha.is_empty() && sha.len() <= 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Self(sha.to_ascii_lowercase()));
        }
        Err(format!(
            "`sha256` beside a `message_id` must be the attachment's hash or a hex prefix of it, \
             got {:?} — or drop it: `index` selects within the message.",
            sha.chars().take(8).collect::<String>()
        ))
    }

    /// The lowercase hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
