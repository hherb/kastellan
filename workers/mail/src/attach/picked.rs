//! [`Picked`]: one attachment, validated — the hash every URL path is built
//! from. Split out of `attach.rs` (movement only) so that file stays near the
//! 500-line cap; the selection logic that produces a `Picked` stays there.

use super::{Cand, SHA_HEAD};
use crate::ids::LocalmailId;

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
    /// [`super::pick`] has already vetted with [`is_sha256`].
    pub(super) fn resolved(message_id: LocalmailId, c: Cand<'_>) -> Self {
        debug_assert!(is_sha256(c.sha), "pick must only yield vetted hashes");
        Self {
            sha256: c.sha.to_string(),
            filename: (!c.name.is_empty()).then(|| c.name.to_string()),
            at: Some((message_id, c.index)),
        }
    }

    /// A hash the **planner** typed, with no message to vouch for it.
    ///
    /// The only public constructor, reached in production through [`choose`]
    /// so that a malformed hash is refused while the params are read (#765).
    /// It is fallible so the traversal guard on the `{sha256}` URL segment is
    /// structural rather than a rule each call site has to remember. There is
    /// no archive filename here: the planner named a hash, not a message, so
    /// there is nothing authoritative to save it under.
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
/// `false` into the planner's repair text) and by [`super::pick`] (which merely skips
/// an entry). Two copies of a traversal guard is one copy too many.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
