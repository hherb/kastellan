//! What one `mail.*` call asks for, read out of its JSON params — the pure
//! half of dispatch.
//!
//! [`Request::parse`] finds every params error that can be found without
//! asking localmail, and sends nothing. The handler runs the localmail version
//! gate only on a call that parsed, and only then talks to localmail (#765).
//! When the gate ran first, a call with a missing `message_id` or a malformed
//! `sha256`, made against an old or unreachable localmail, came back as
//! "upgrade localmail" or a transport fault instead of `INVALID_PARAMS`: the
//! planner retried a call it should have corrected, and the operator was sent
//! to upgrade a service for a call that was never valid.
//!
//! What the handler can still refuse as `INVALID_PARAMS`, after the gate, is
//! what only the archive can answer: a `message_id` it cannot read, or a
//! `filename`/`index`/`sha256` prefix that matches no attachment of that
//! message (`attach::pick`). Values localmail alone defines — `sort` (passed
//! through on purpose, see `crate::sort`), `cursor`, the `limit` range, and
//! the keys inside `filters` — are not checked here at all.

use kastellan_protocol::{codes, RpcError};

use crate::attach;
use crate::ids::{self, LocalmailId};
use crate::search_params;

/// The six tools, from the JSON-RPC method name. [`Request::parse`] turns one
/// into a [`Request`], which the version gate and dispatch then both read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tool {
    Search,
    GetMessage,
    ListMessages,
    ListAccounts,
    GetAttachmentText,
    GetAttachment,
}

impl Tool {
    pub(super) fn from_method(method: &str) -> Option<Self> {
        Some(match method {
            "mail.search" => Self::Search,
            "mail.get_message" => Self::GetMessage,
            "mail.list_messages" => Self::ListMessages,
            "mail.list_accounts" => Self::ListAccounts,
            "mail.get_attachment_text" => Self::GetAttachmentText,
            "mail.get_attachment" => Self::GetAttachment,
            _ => return None,
        })
    }
}

/// `mail.search`'s params, validated.
#[derive(Debug)]
pub(super) struct Search {
    /// `""` for a filter-only search (#698): absent and `null` both mean "no text".
    pub query: String,
    /// The top-level `account_ids`/`folder_ids` folded in and typed — by
    /// construction, since only `search_params::normalize_filters` makes one.
    pub filters: Option<search_params::NormalizedFilters>,
    /// `None` when absent, `null` or `""` (#768) — "no sort", which
    /// `sort::plan_sort` then decides. Any other value is passed through for
    /// localmail to validate (see `crate::sort`).
    pub sort: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

/// `mail.list_messages`'s params, validated.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListMessages {
    #[serde(default, deserialize_with = "ids::account_ids")]
    pub account_ids: Option<Vec<LocalmailId>>,
    #[serde(default, deserialize_with = "ids::folder_ids")]
    pub folder_ids: Option<Vec<LocalmailId>>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// One call, with its params read and checked.
#[derive(Debug)]
pub(super) enum Request {
    Search(Search),
    GetMessage { message_id: LocalmailId, full_headers: bool },
    ListMessages(ListMessages),
    ListAccounts,
    GetAttachmentText {
        selector: attach::Selector,
        /// Where the page starts, in characters: `0` for the first page, else
        /// a `next_offset` a previous page returned.
        offset: u64,
    },
    GetAttachment {
        selector: attach::Selector,
        /// What the planner passed as `filename`, kept for the output name
        /// when the archive supplies none: always for the `sha256` form, and
        /// for a nameless entry in the message form (see `get_attachment`).
        requested_name: Option<String>,
    },
}

impl Request {
    /// Read `params` for `tool`. Every `Err` is `INVALID_PARAMS`, and nothing
    /// is sent to localmail on the way.
    pub(super) fn parse(tool: Tool, params: serde_json::Value) -> Result<Self, RpcError> {
        match tool {
            Tool::Search => parse_search(params).map(Self::Search),
            Tool::GetMessage => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct P {
                    #[serde(deserialize_with = "ids::message_id")]
                    message_id: LocalmailId,
                    #[serde(default)]
                    full_headers: bool,
                }
                let p: P = from_params(params)?;
                Ok(Self::GetMessage { message_id: p.message_id, full_headers: p.full_headers })
            }
            Tool::ListMessages => from_params(params).map(Self::ListMessages),
            Tool::ListAccounts => {
                // Takes no params. It used to ignore any it was sent, so a
                // planner that carried `account_ids` over from list_messages
                // got every account back and believed it had filtered.
                let empty = params.is_null() || params.as_object().is_some_and(|o| o.is_empty());
                if !empty {
                    return Err(RpcError::new(
                        codes::INVALID_PARAMS,
                        "mail.list_accounts takes no params — it lists every account this \
                         agent may read. Filter by account in mail.list_messages or mail.search."
                            .to_string(),
                    ));
                }
                Ok(Self::ListAccounts)
            }
            Tool::GetAttachmentText => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct P {
                    #[serde(default)]
                    sha256: Option<String>,
                    #[serde(default, deserialize_with = "ids::opt_message_id")]
                    message_id: Option<LocalmailId>,
                    #[serde(default)]
                    filename: Option<String>,
                    #[serde(default)]
                    index: Option<usize>,
                    #[serde(default)]
                    offset: Option<u64>,
                }
                let p: P = from_params(params)?;
                let selector = choose(p.sha256, p.message_id, p.filename, p.index)?;
                Ok(Self::GetAttachmentText { selector, offset: p.offset.unwrap_or(0) })
            }
            Tool::GetAttachment => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct P {
                    #[serde(default)]
                    sha256: Option<String>,
                    #[serde(default, deserialize_with = "ids::opt_message_id")]
                    message_id: Option<LocalmailId>,
                    #[serde(default)]
                    filename: Option<String>,
                    #[serde(default)]
                    index: Option<usize>,
                }
                let p: P = from_params(params)?;
                // `filename` does double duty here, and the two jobs do not
                // conflict: with `message_id` it *selects* the attachment, and
                // with `sha256` (which already selects one) it names the output.
                // Cloned before `choose` consumes it, because the sha form needs it.
                let requested_name = p.filename.clone();
                let selector = choose(p.sha256, p.message_id, p.filename, p.index)?;
                Ok(Self::GetAttachment { selector, requested_name })
            }
        }
    }

    /// Does this call use a route or request field localmail added after
    /// `/v1` began (see `crate::version`), and so must not run against an
    /// older server? The rest use routes every `/v1` serves, and keep working
    /// against one — a mail worker that could still list accounts is more use
    /// to an operator diagnosing an upgrade than one that refuses everything.
    ///
    /// `mail.get_message` is gated only when it asks for headers: it then sends
    /// `?headers=list`, which an older server answers with a 200 and **no**
    /// `headers` key rather than an error. A compact read works everywhere.
    /// Decided from the **parsed** `full_headers`, so the gate and the request
    /// that follows cannot disagree about whether headers were asked for.
    ///
    /// Exhaustive on purpose (no `_` arm): a new tool does not compile until
    /// someone decides whether it is gated. When this was a string `matches!`
    /// beside the dispatch `match`, a new slice D/E tool left off it would
    /// have skipped the gate silently.
    pub(super) fn needs_current_api(&self) -> bool {
        match self {
            Self::Search(_) | Self::GetAttachmentText { .. } | Self::GetAttachment { .. } => true,
            Self::GetMessage { full_headers, .. } => *full_headers,
            Self::ListMessages(_) | Self::ListAccounts => false,
        }
    }
}

fn parse_search(params: serde_json::Value) -> Result<Search, RpcError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct P {
        // Optional (#698): a filter-only search — "every message with an
        // attachment" — has no text, and localmail serves it as a
        // date-ordered walk over the filtered archive.
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        filters: Option<serde_json::Value>,
        // Accepted at the top level as well as inside `filters`, because
        // `mail.list_messages` takes them there and the planner carried the
        // shape across (twice, in one live task). `search_params` folds them
        // inward and fixes their type.
        #[serde(default, deserialize_with = "ids::account_ids")]
        account_ids: Option<Vec<LocalmailId>>,
        #[serde(default, deserialize_with = "ids::folder_ids")]
        folder_ids: Option<Vec<LocalmailId>>,
        #[serde(default)]
        sort: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        cursor: Option<String>,
    }
    let p: P = from_params(params)?;
    let filters = search_params::normalize_filters(p.filters, p.account_ids, p.folder_ids)
        .map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
    Ok(Search {
        query: p.query.unwrap_or_default(),
        filters,
        sort: p.sort.filter(|s| !s.is_empty()),
        limit: p.limit,
        cursor: p.cursor,
    })
}

/// [`attach::choose`], with its planner-facing refusal as `INVALID_PARAMS`.
fn choose(
    sha256: Option<String>,
    message_id: Option<LocalmailId>,
    filename: Option<String>,
    index: Option<usize>,
) -> Result<attach::Selector, RpcError> {
    attach::choose(sha256, message_id, filename, index).map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))
}

fn from_params<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))
}
