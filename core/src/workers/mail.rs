//! Host-side manifest + `ToolEntry` constructor for the mail worker
//! (read-only localmail access over its `/v1` REST API).
//!
//! **Allowlist** is derived from the single `KASTELLAN_MAIL_ENDPOINT` (the
//! web-search pattern via [`net_entries_from_endpoint`]), NOT the
//! `tool_allowlists` table: the worker ever dials exactly one endpoint, so the
//! allowlist *is* that endpoint's `host:port`. Deriving it here guarantees the
//! allowlist can never disagree with the endpoint the client actually reaches.
//!
//! **Bearer token** is provided by the operator in a `0600` file named by
//! `KASTELLAN_MAIL_TOKEN_FILE` — the path travels in `policy.env`, the plaintext
//! stays in the file (`fs_read`-bound into the jail), never in `policy.env`
//! (the Matrix-worker convention). Vault-backed `kastellan-cli secret put`
//! materialization is a documented follow-up: `build_tool_registry` runs before
//! the daemon's `Vault` exists, so resolve-time materialization would need a
//! bring-up reordering, deliberately deferred.
//!
//! **Force-routing** is applied at spawn (`rewrite_worker_policy` sets
//! `proxy_uds` + appends the per-instance CA), exactly as for web-fetch: mail is
//! a `Net::Allowlist` net worker and is *not* in `disable_mitm_for`, so it
//! reaches localmail through the egress proxy (loopback via the allowlisted-IP
//! carve-out; remote as a normal allowlisted host).

use std::path::{Path, PathBuf};

use kastellan_sandbox::{Net, Profile, SandboxPolicy};
use url::Url;

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{
    discover_binary, ResolveCtx, Resolution, ToolDoc, ToolParam, WorkerManifest,
};

/// Tool name the registry/planner keys the mail worker on.
const TOOL_NAME: &str = "mail";
/// Operator override for the worker binary path.
const BIN_ENV: &str = "KASTELLAN_MAIL_BIN";
/// Exe-relative sibling default (cargo `target/debug` + flat installs).
const DEFAULT_BIN_NAME: &str = "kastellan-worker-mail";
/// Base URL of the localmail `serve` instance (loopback or LAN/VPN).
const ENDPOINT_ENV: &str = "KASTELLAN_MAIL_ENDPOINT";
/// Path to the `0600` file holding the localmail bearer token.
const TOKEN_FILE_ENV: &str = "KASTELLAN_MAIL_TOKEN_FILE";

/// Derive the `Net::Allowlist` `host:port` entry from the endpoint URL. Returns
/// an empty list if the endpoint is unset or unparseable (the worker then fails
/// closed — correct: mail is disabled without a usable endpoint).
fn net_entries_from_endpoint(endpoint: &str) -> Vec<String> {
    match Url::parse(endpoint) {
        Ok(u) => match u.host_str() {
            Some(host) => {
                let port = u.port_or_known_default().unwrap_or(443);
                vec![format!("{host}:{port}")]
            }
            None => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

/// Build the [`ToolEntry`] for the mail worker. `SingleUse` (each call is a
/// fresh sandbox; attachment files accumulate in the task-scoped `out/` across
/// spawns). `mem_mb: 256` — JSON + a capped in-memory attachment copy, lighter
/// than web-fetch's HTML/PDF parsing.
pub fn mail_entry(binary: PathBuf, endpoint: &str, token_file: &str) -> ToolEntry {
    let net_entries = net_entries_from_endpoint(endpoint);
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from(token_file),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries),
        cpu_ms: 10_000,
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        env: vec![
            (ENDPOINT_ENV.to_string(), endpoint.to_string()),
            (TOKEN_FILE_ENV.to_string(), token_file.to_string()),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: None,
    }
}

pub struct MailManifest;

impl WorkerManifest for MailManifest {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn tool_docs(&self) -> Vec<ToolDoc> {
        vec![
            ToolDoc {
                name: TOOL_NAME,
                method: "mail.search",
                summary: "Search the mail archive (hybrid semantic + full-text). Filter by \
                          date range, from/to, subject, has_attachment, account/folder. Page \
                          forward with next_cursor.",
                params: &[
                    ToolParam { name: "query", description: "free-text search query", required: true },
                    ToolParam {
                        name: "filters",
                        description: "object: date_from, date_to, from, to, subject, has_attachment, account_ids, folder_ids, lang",
                        required: false,
                    },
                    ToolParam { name: "sort", description: "'rank' (default) or 'date'", required: false },
                    ToolParam { name: "limit", description: "max hits (default 50)", required: false },
                    ToolParam { name: "cursor", description: "next_cursor from a prior page", required: false },
                ],
            },
            ToolDoc {
                name: TOOL_NAME,
                method: "mail.get_message",
                summary: "Fetch one message: headers, plaintext body, and attachment list \
                          [{filename, sha256, content_type, size}].",
                params: &[
                    ToolParam { name: "message_id", description: "message id from a search/list hit", required: true },
                    ToolParam { name: "full_headers", description: "include full headers (default false)", required: false },
                ],
            },
            ToolDoc {
                name: TOOL_NAME,
                method: "mail.list_messages",
                summary: "Browse messages newest-first; filter by account/folder. Page with next_cursor.",
                params: &[
                    ToolParam { name: "account_ids", description: "restrict to these account ids", required: false },
                    ToolParam { name: "folder_ids", description: "restrict to these folder ids", required: false },
                    ToolParam { name: "limit", description: "max rows (default 50)", required: false },
                    ToolParam { name: "cursor", description: "next_cursor from a prior page", required: false },
                ],
            },
            ToolDoc {
                name: TOOL_NAME,
                method: "mail.list_accounts",
                summary: "List the mail accounts this agent may read.",
                params: &[],
            },
            ToolDoc {
                name: TOOL_NAME,
                method: "mail.get_attachment_text",
                summary: "Extracted text of an attachment (server-side PDF/office extraction). \
                          Use to READ an attachment's contents.",
                params: &[ToolParam { name: "sha256", description: "attachment sha256 from get_message", required: true }],
            },
            ToolDoc {
                name: TOOL_NAME,
                method: "mail.get_attachment",
                summary: "Save an attachment in its ORIGINAL format (PDF, etc.) to the task \
                          output dir; returns its path, size and content_type. Use to DELIVER a file.",
                params: &[
                    ToolParam { name: "sha256", description: "attachment sha256 from get_message", required: true },
                    ToolParam { name: "filename", description: "suggested filename (sanitized)", required: false },
                ],
            },
        ]
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let Some(endpoint) = (ctx.get_env)(ENDPOINT_ENV) else {
            return Resolution::Disabled {
                detail: format!("{ENDPOINT_ENV} unset — mail worker disabled"),
            };
        };
        if net_entries_from_endpoint(&endpoint).is_empty() {
            return Resolution::Misconfigured {
                detail: format!("{ENDPOINT_ENV} is not a URL with a host: {endpoint:?}"),
            };
        }
        let Some(token_file) = (ctx.get_env)(TOKEN_FILE_ENV) else {
            return Resolution::Misconfigured {
                detail: format!(
                    "{TOKEN_FILE_ENV} unset — provide a 0600 file holding the localmail bearer token"
                ),
            };
        };
        if !(ctx.exists)(Path::new(&token_file)) {
            return Resolution::Misconfigured {
                detail: format!("{TOKEN_FILE_ENV} does not exist: {token_file}"),
            };
        }
        let binary = match discover_binary(ctx, BIN_ENV, DEFAULT_BIN_NAME) {
            Some(b) => b,
            None => {
                return Resolution::Misconfigured {
                    detail: format!(
                        "could not resolve worker binary: {BIN_ENV} set but not a runnable \
                         file, or unset with no sibling {DEFAULT_BIN_NAME} found"
                    ),
                };
            }
        };
        Resolution::Register(mail_entry(binary, &endpoint, &token_file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal stub `ResolveCtx` driven by closures over fixed maps.
    fn ctx<'a>(
        env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env: env,
            exists,
            is_dir: &|_p| false,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist: &|_t| Vec::new(),
        }
    }

    #[test]
    fn disabled_when_endpoint_unset() {
        let env = |_k: &str| None;
        let exists = |_p: &Path| true;
        match MailManifest.resolve(&ctx(&env, &exists)) {
            Resolution::Disabled { .. } => {}
            _ => panic!("expected Disabled when endpoint unset"),
        }
    }

    #[test]
    fn misconfigured_when_token_file_missing() {
        let env = |k: &str| match k {
            "KASTELLAN_MAIL_ENDPOINT" => Some("http://127.0.0.1:8000".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| false;
        match MailManifest.resolve(&ctx(&env, &exists)) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("KASTELLAN_MAIL_TOKEN_FILE"), "detail: {detail}");
            }
            _ => panic!("expected Misconfigured when token file missing"),
        }
    }

    #[test]
    fn entry_allowlists_only_the_endpoint_and_binds_token_file() {
        let entry = mail_entry(
            PathBuf::from("/opt/kastellan-worker-mail"),
            "http://127.0.0.1:8000",
            "/run/kastellan/mail-token",
        );
        match &entry.policy.net {
            Net::Allowlist(entries) => {
                assert_eq!(entries, &vec!["127.0.0.1:8000".to_string()], "only the endpoint");
            }
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        assert!(
            entry.policy.fs_read.contains(&PathBuf::from("/run/kastellan/mail-token")),
            "token file must be readable in the jail"
        );
        // Token PATH is in env; the plaintext is never in policy.env.
        assert!(entry
            .policy
            .env
            .iter()
            .any(|(k, v)| k == "KASTELLAN_MAIL_TOKEN_FILE" && v == "/run/kastellan/mail-token"));
        assert!(matches!(entry.lifecycle, crate::worker_lifecycle::Lifecycle::SingleUse));
    }

    #[test]
    fn endpoint_with_explicit_port_and_default_port() {
        assert_eq!(net_entries_from_endpoint("http://127.0.0.1:8000"), vec!["127.0.0.1:8000"]);
        assert_eq!(net_entries_from_endpoint("https://mail.vpn.example"), vec!["mail.vpn.example:443"]);
        assert!(net_entries_from_endpoint("not a url").is_empty());
    }

    #[test]
    fn advertises_all_six_tools() {
        let methods: Vec<&str> = MailManifest.tool_docs().iter().map(|d| d.method).collect();
        assert_eq!(
            methods,
            vec![
                "mail.search",
                "mail.get_message",
                "mail.list_messages",
                "mail.list_accounts",
                "mail.get_attachment_text",
                "mail.get_attachment",
            ]
        );
    }
}
