//! JSON-RPC dispatch for the six read-only `mail.*` tools. Each arm validates
//! params, calls the localmail REST client, and maps failures to `RpcError`.
//! Attachments come back either as extracted text (`get_attachment_text`) or as
//! original-format files written to the task workspace `out/` (`get_attachment`).

use std::cell::OnceCell;
use std::path::Path;

use kastellan_protocol::{codes, server::Handler, RpcError};

use crate::attach;
use crate::client::{MailClient, MailError};
use crate::ids::{self, LocalmailId};
use crate::problem;
use crate::search_params;
use crate::sort;
use crate::text_page;
use crate::version;

pub struct MailHandler {
    client: MailClient,
    /// Set once localmail has shown it serves an `api_minor` this worker can
    /// use (see [`version`]). Only a success is remembered: a refusal is asked
    /// again, so a localmail upgraded under a persistent worker is picked up.
    api_ok: OnceCell<()>,
}

/// The tools that use a route or request field localmail added in slice D or
/// E, and so must not run against an older server. The rest use routes every
/// `/v1` serves, and keep working against one — a mail worker that could still
/// list accounts is more use to an operator diagnosing an upgrade than one
/// that refuses everything.
fn needs_current_api(method: &str) -> bool {
    matches!(method, "mail.search" | "mail.get_attachment_text" | "mail.get_attachment")
}

impl MailHandler {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self { client: MailClient::from_env()?, api_ok: OnceCell::new() })
    }

    /// A handler whose localmail is taken to be current, so a fake transport
    /// need not serve `/v1/version`. The gate itself is tested through
    /// [`Self::with_client_unverified`].
    #[cfg(test)]
    pub fn with_client(client: MailClient) -> Self {
        let api_ok = OnceCell::new();
        let _ = api_ok.set(());
        Self { client, api_ok }
    }

    #[cfg(test)]
    pub fn with_client_unverified(client: MailClient) -> Self {
        Self { client, api_ok: OnceCell::new() }
    }

    /// Refuse, with the upgrade named, unless localmail's `/v1/version` is one
    /// [`version::version_error`] accepts. One GET per worker (the mail worker
    /// is single-use, so in practice one per gated call) to an unauthenticated
    /// route on the same origin.
    fn require_current_api(&self) -> Result<(), RpcError> {
        if self.api_ok.get().is_some() {
            return Ok(());
        }
        let body = self.client.get_json("/v1/version").map_err(|e| match e {
            // Only a localmail older than every `api_minor` lacks the route.
            MailError::Upstream { status: 404, .. } => RpcError::new(
                codes::OPERATION_FAILED,
                version::too_old("one with no /v1/version route"),
            ),
            other => mail_err_to_rpc(other),
        })?;
        if let Some(why) = version::version_error(&body) {
            return Err(RpcError::new(codes::OPERATION_FAILED, why));
        }
        let _ = self.api_ok.set(());
        Ok(())
    }

    fn search(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct P {
            // Optional (#698): a filter-only search — "every message with an
            // attachment" — has no text, and localmail serves it as a
            // date-ordered walk over the filtered archive. Absent and `null`
            // both mean "no text" and are sent as `""`.
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
        let p: P = parse_params(params)?;
        let query = p.query.unwrap_or_default();
        let mut body = serde_json::json!({ "query": query });
        let filters = search_params::normalize_filters(p.filters, p.account_ids, p.folder_ids)
            .map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
        if let Some(f) = filters {
            body["filters"] = f;
        }
        // The ordering is the one property the response gets annotated with, so
        // it is decided up front rather than inferred from a field we may not
        // have sent. `plan_sort` also decides *whether* to send one at all: on a
        // paging request the cursor already carries the ordering, and defaulting
        // one there contradicts it (#561). And a blank query defaults to `date`,
        // because localmail refuses a stated `rank` it has no text to rank
        // (#698). Needs the cursor before the body is built, hence the early
        // `is_some`.
        let plan = sort::plan_sort(p.sort.as_deref(), p.cursor.is_some(), &query);
        if let sort::SortPlan::Send(s) = plan {
            body["sort"] = serde_json::json!(s);
        }
        if let Some(l) = p.limit {
            body["limit"] = serde_json::json!(l);
        }
        if let Some(c) = p.cursor {
            body["cursor"] = serde_json::json!(c);
        }
        // Compact hits (slice E): see `search_params::HIT_FIELDS` for what is
        // dropped and why. Sent on every page — localmail projects per request,
        // not per cursor.
        body["fields"] = serde_json::json!(search_params::HIT_FIELDS);
        body["snippet_chars"] = serde_json::json!(search_params::SNIPPET_CHARS);
        // `smart` (LLM query rewrite) deliberately never set — workers do not
        // call the LLM. The planner already decomposes/rewrites queries.
        let mut out = self.client.post_json("/v1/search", &body).map_err(mail_err_to_rpc)?;
        sort::annotate(&mut out, plan);
        Ok(out)
    }

    fn get_message(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct P {
            #[serde(deserialize_with = "ids::message_id")]
            message_id: LocalmailId,
            #[serde(default)]
            full_headers: bool,
        }
        let p: P = parse_params(params)?;
        self.client
            .get_json(&detail_path(p.message_id, p.full_headers))
            .map(crate::headers::header_names_as_values)
            .map(crate::detail::number_attachments)
            .map_err(mail_err_to_rpc)
    }

    fn list_messages(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct P {
            #[serde(default, deserialize_with = "ids::account_ids")]
            account_ids: Option<Vec<LocalmailId>>,
            #[serde(default, deserialize_with = "ids::folder_ids")]
            folder_ids: Option<Vec<LocalmailId>>,
            #[serde(default)]
            limit: Option<u32>,
            #[serde(default)]
            cursor: Option<String>,
        }
        let p: P = parse_params(params)?;
        let mut q: Vec<String> = Vec::new();
        if let Some(a) = &p.account_ids {
            q.push(format!("account_ids={}", join_ids(a)));
        }
        if let Some(f) = &p.folder_ids {
            q.push(format!("folder_ids={}", join_ids(f)));
        }
        if let Some(l) = p.limit {
            q.push(format!("limit={l}"));
        }
        if let Some(c) = &p.cursor {
            q.push(format!("cursor={}", urlencode(c)));
        }
        let path = if q.is_empty() {
            "/v1/messages".to_string()
        } else {
            format!("/v1/messages?{}", q.join("&"))
        };
        self.client.get_json(&path).map_err(mail_err_to_rpc)
    }

    fn list_accounts(&self) -> Result<serde_json::Value, RpcError> {
        self.client.get_json("/v1/accounts").map_err(mail_err_to_rpc)
    }

    /// Turn a [`attach::Selector`] into a sha256 that is safe to interpolate.
    ///
    /// The `InMessage` arm costs one extra GET, and buys the property the whole
    /// change is for: the hash comes out of localmail's own response rather than
    /// out of the planner's transcription of a 64-char string it saw once,
    /// unlabelled, in a key-stripped prompt head.
    fn resolve_attachment(&self, selector: attach::Selector) -> Result<attach::Picked, RpcError> {
        match selector {
            // Fallible construction is the whole guard: `from_planner_sha` is
            // the only way into `Picked` from here, so the traversal check on
            // the `{sha256}` URL segment cannot be forgotten at a call site.
            attach::Selector::Sha(sha256) => attach::Picked::from_planner_sha(&sha256)
                .map_err(|m| RpcError::new(codes::INVALID_PARAMS, m)),
            attach::Selector::InMessage { message_id, filename, expect_sha, index } => {
                // Compact headers: only `attachments` is read here, and full
                // headers would multiply the response for nothing.
                let msg = self
                    .client
                    .get_json(&detail_path(message_id, false))
                    // localmail returns the same 404 for "no such message" and
                    // "your ACL excludes it" (a deliberate choice — permission
                    // state must not be enumerable). Forwarding its sentence
                    // makes the agent tell the user the mail does not exist.
                    .map_err(|e| match e {
                        MailError::Upstream { status: 404, .. } => RpcError::new(
                            codes::INVALID_PARAMS,
                            format!(
                                "message {message_id} is not readable — either no such \
                                 message_id, or this agent's mail ACL excludes it. Copy a \
                                 message_id from a mail.search or mail.list_messages hit."
                            ),
                        ),
                        other => mail_err_to_rpc(other),
                    })?;
                // A message detail always carries an `attachments` array
                // (localmail emits `[]` for none). Absent-or-not-an-array is a
                // contract violation, and reporting it as "this message has no
                // attachments" would state something about the archive that we
                // did not learn from the archive.
                let attachments: &[serde_json::Value] = match msg.get("attachments") {
                    Some(serde_json::Value::Array(a)) => a.as_slice(),
                    None | Some(serde_json::Value::Null) => &[],
                    Some(_) => {
                        return Err(RpcError::new(
                            codes::OPERATION_FAILED,
                            format!(
                                "localmail returned message {message_id} with a non-array \
                                 `attachments` — this is a service fault, not an empty message."
                            ),
                        ))
                    }
                };
                attach::pick(attachments, filename.as_deref(), expect_sha.as_deref(), index, message_id)
                    .map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))
            }
        }
    }

    fn get_attachment_text(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
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
            /// Where the page starts, in characters: `0` (or absent) for the
            /// first page, else a `next_offset` a previous page returned.
            #[serde(default)]
            offset: Option<u64>,
        }
        let p: P = parse_params(params)?;
        let selector = attach::choose(p.sha256, p.message_id, p.filename, p.index)
            .map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
        // Whether the planner typed the hash decides which repair a 404 gets;
        // read it before `resolve_attachment` consumes the selector.
        let planner_supplied = matches!(selector, attach::Selector::Sha(_));
        let picked = self.resolve_attachment(selector)?;
        // `get_bytes` (the higher attachment cap, not the JSON cap) — a page is
        // small, but the cap is the attachment tools' one ceiling.
        let (_ct, bytes) = self
            .client
            .get_bytes(&picked.text_path(p.offset.unwrap_or(0), text_page::TEXT_PAGE_CHARS))
            // localmail answers 404 both for a blob it has never seen and for
            // one whose text is not extracted yet, and the two need opposite
            // repairs. Forwarding its sentence verbatim is what told the live
            // agent that extraction had failed when the hash was simply wrong.
            .map_err(|e| match e {
                MailError::Upstream { status: 404, .. } => RpcError::new(
                    codes::OPERATION_FAILED,
                    attach::missing_text_advice(picked.sha256(), planner_supplied),
                ),
                other => mail_err_to_rpc(other),
            })?;
        text_page::page_result(&picked, &bytes)
            .map_err(|m| RpcError::new(codes::OPERATION_FAILED, m))
    }

    fn get_attachment(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
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
        let p: P = parse_params(params)?;
        // `filename` does double duty here, and the two jobs do not conflict:
        // with `message_id` it *selects* the attachment, and the file is then
        // saved under the name the archive actually has for it; with `sha256`
        // (which already selects one) it names the output, exactly as before.
        // Kept before `choose` consumes it, because the sha form needs it.
        let requested_name = p.filename.clone();
        let selector = attach::choose(p.sha256, p.message_id, p.filename, p.index)
            .map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
        // Same rule as `get_attachment_text`: a hash this worker resolved out of
        // a message is right by construction, so a 404 on it must not send the
        // planner to re-copy it.
        let planner_supplied = matches!(selector, attach::Selector::Sha(_));
        let picked = self.resolve_attachment(selector)?;
        let out_dir = std::env::var("KASTELLAN_WORKER_OUT").map_err(|_| {
            RpcError::new(
                codes::OPERATION_FAILED,
                "no task output dir (KASTELLAN_WORKER_OUT unset) — attachment delivery unavailable"
                    .to_string(),
            )
        })?;
        let (content_type, bytes) = self
            .client
            .get_bytes(&picked.blob_path())
            // localmail answers 404 here for "no such blob" *and* for an ACL
            // denial, exactly as it does for extracted text. This tool used to
            // forward that sentence verbatim — the ambiguous upstream 404 the
            // sibling tool was fixed for, still live in the tool that sibling's
            // own advice recommends as the fallback.
            .map_err(|e| match e {
                MailError::Upstream { status: 404, .. } => RpcError::new(
                    codes::OPERATION_FAILED,
                    attach::missing_blob_advice(picked.sha256(), planner_supplied),
                ),
                other => mail_err_to_rpc(other),
            })?;
        // The archive's own name wins over the planner's: a substring match may
        // have selected `Download 470989752-e-ticket-DQXK68.pdf` for a request
        // that said `e-ticket-DQXK68.pdf`, and the file on disk should carry the
        // name the archive uses. Falls back to what was asked for (the sha form,
        // where there is no archive name), then to a sha-derived stem.
        let name =
            safe_attachment_name(picked.save_name().or(requested_name.as_deref()), picked.sha256());
        let dir = Path::new(&out_dir);
        let dest = dir.join(&name);
        // Per-process-unique .partial so an interrupted write or two concurrent
        // same-name fetches never share/clobber the scratch file (M-1).
        let partial = dir.join(format!(".{}.{name}.partial", std::process::id()));
        std::fs::write(&partial, &bytes).map_err(|e| {
            // Best-effort: reclaim any truncated scratch file so it neither
            // lingers nor blocks the runner's empty-dir prune.
            let _ = std::fs::remove_file(&partial);
            RpcError::new(codes::OPERATION_FAILED, format!("write attachment: {e}"))
        })?;
        std::fs::rename(&partial, &dest).map_err(|e| {
            // Rename failed → the .partial is orphaned; reclaim it (same reason).
            let _ = std::fs::remove_file(&partial);
            RpcError::new(codes::OPERATION_FAILED, format!("finalize attachment: {e}"))
        })?;
        Ok(serde_json::json!({
            "sha256": picked.sha256(),
            "index": picked.index(),
            "filename": name,
            "content_type": content_type,
            "size": bytes.len(),
            "path": dest.to_string_lossy(),
        }))
    }
}

impl Handler for MailHandler {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        if needs_current_api(method) {
            self.require_current_api()?;
        }
        match method {
            "mail.search" => self.search(params),
            "mail.get_message" => self.get_message(params),
            "mail.list_messages" => self.list_messages(params),
            "mail.list_accounts" => self.list_accounts(),
            "mail.get_attachment_text" => self.get_attachment_text(params),
            "mail.get_attachment" => self.get_attachment(params),
            _ => Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            )),
        }
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))
}

fn mail_err_to_rpc(e: MailError) -> RpcError {
    match e {
        MailError::BadParams(m) => RpcError::new(codes::INVALID_PARAMS, m),
        MailError::Upstream { status: 401 | 403, .. } => RpcError::new(
            codes::POLICY_DENIED,
            "localmail auth/permission denied (check token / account ACL)".to_string(),
        ),
        // localmail reports a caller error as problem+json, where only `detail`
        // is written for the caller. Forwarding the whole envelope spent 91 of
        // the planner's 200-char budget on `type`/`title`/`status` and pushed
        // the sort/cursor advice 7 chars over the clamp, truncating it mid-word
        // — measured live, see `problem`. Fall back to the raw body for
        // anything that is not problem+json.
        MailError::Upstream { status, body } => {
            let shown = problem::problem_detail(&body).unwrap_or(body);
            RpcError::new(codes::OPERATION_FAILED, format!("localmail {status}: {shown}"))
        }
        MailError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("transport: {m}"))
        }
    }
}

/// Collision- and traversal-safe filename under `out/`: take only the final
/// path component of the requested name, keep `[A-Za-z0-9._-]`, drop leading
/// dots, then prefix the first 12 sha256 chars so two messages sharing bytes
/// under different names never clobber one another.
fn safe_attachment_name(requested: Option<&str>, sha256: &str) -> String {
    let base = requested
        .and_then(|r| Path::new(r).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    let stem = if cleaned.is_empty() { "attachment" } else { cleaned };
    let prefix: String = sha256.chars().take(12).collect();
    format!("{prefix}_{stem}")
}

/// localmail's message-detail URL.
///
/// The tool's public parameter is the boolean `full_headers` and stays that way
/// — it is the advertised schema. The service, however, reads a differently
/// *named* query parameter and derives the flag from its *value*
/// (`serve/routes/messages.py::detail`: `full_headers=(headers == "full")`), so
/// FastAPI silently dropped the `?full_headers=<bool>` this worker used to send
/// and every response came back without `headers`. Translating here keeps the
/// mismatch at the one boundary where it belongs.
///
/// Compact is the service's default, so the parameter is omitted rather than
/// sent as `headers=compact`.
///
/// Takes a [`LocalmailId`], not an `i64`: this is the URL-path interpolation the
/// whole traversal argument is about, so the validated type reaches it rather
/// than stopping one call short. `LocalmailId` cannot be turned back into an
/// `i64` anywhere in this crate — only `Display`ed — which is what makes the
/// guard structural instead of positional.
fn detail_path(message_id: LocalmailId, full_headers: bool) -> String {
    if full_headers {
        format!("/v1/messages/{message_id}?headers=full")
    } else {
        format!("/v1/messages/{message_id}")
    }
}

/// `/v1/accounts` and get_message's `folders` both serve ids as strings, so
/// these arrive in either shape; `LocalmailId` has already validated them to
/// digits by the time they get here, and an empty list was refused at
/// deserialization (it would render as a bare `account_ids=`).
fn join_ids(v: &[LocalmailId]) -> String {
    v.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

/// Percent-encode an opaque query value (the pagination cursor).
fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests;
