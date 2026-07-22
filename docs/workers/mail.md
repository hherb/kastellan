# mail worker — read-only localmail access

The `mail` worker gives the agent read-only access to a [localmail](https://github.com/hherb/localmail)
archive over its `/v1` REST API: search, message + attachment retrieval, with
attachments delivered as extracted text **or** as original-format files.

- **Design:** [`docs/superpowers/specs/2026-07-22-localmail-mail-worker-integration-design.md`](../superpowers/specs/2026-07-22-localmail-mail-worker-integration-design.md)
- **Crate:** `workers/mail` (`kastellan-worker-mail`); manifest `core/src/workers/mail.rs`

## Tools

| Tool | Purpose |
| --- | --- |
| `mail.search` | Hybrid semantic + full-text search. Filter by `date_from`/`date_to`/`from`/`to`/`subject`/`has_attachment`/`account_ids`/`folder_ids`/`lang`; `sort` = `rank`\|`date`; page with `cursor`/`next_cursor`. |
| `mail.get_message` | One message: headers, plaintext body, attachment list `[{filename, sha256, content_type, size}]`. |
| `mail.list_messages` | Browse newest-first; `account_ids`/`folder_ids` filters; `cursor`. |
| `mail.list_accounts` | Accounts this agent may read. |
| `mail.get_attachment_text` | Server-extracted text of an attachment (`{sha256}`). Use to **read** it. |
| `mail.get_attachment` | Save an attachment in its **original format** (PDF, etc.) to the task output dir; returns `{path, size, content_type, filename}`. Use to **deliver** a file. |

The agent does the reasoning (e.g. extracting flight-booking fields into a CSV);
the worker only searches and retrieves. Attachments delivered by
`mail.get_attachment` land in the task's workspace `out/`, harvested at task
finalize to `$KASTELLAN_ARTIFACTS_ROOT/<task_id>/` (default
`~/.kastellan/artifacts/<task_id>/`).

## One-time operator setup

1. **localmail — a dedicated agent API user.** Create an API user, grant it the
   accounts/folders the agent may read (this ACL *is* the agent's mail scope),
   and mint a bearer token for it (see the localmail CLI / admin UI). Localmail
   enforces the ACL server-side per token.

2. **kastellan — the token file.** Write the token to a **`0600`** file, e.g.:

   ```sh
   umask 077
   printf %s '<the-bearer-token>' > ~/.config/kastellan/mail-token
   ```

   The token stays in this file; only its **path** is passed to the worker
   (`KASTELLAN_MAIL_TOKEN_FILE`), never the plaintext in the environment. The
   file is bind-mounted read-only into the worker's jail.

   > **Follow-up:** vault-backed storage (`kastellan-cli secret put
   > localmail-agent-token`) is not yet wired for this worker — the tool registry
   > is built before the daemon's secret vault exists, so resolve-time
   > materialization needs a bring-up reorder. Tracked for a later pass.

3. **kastellan — env.** Set on the daemon:

   ```sh
   KASTELLAN_MAIL_ENDPOINT=http://127.0.0.1:8000        # co-located loopback
   # or  https://mail.host.vpn:8443                      # remote over LAN/VPN
   KASTELLAN_MAIL_TOKEN_FILE=/home/you/.config/kastellan/mail-token
   # optional: cap for original-format downloads (default 25 MiB)
   KASTELLAN_MAIL_ATTACHMENT_MAX_BYTES=26214400
   ```

   The worker's network allowlist is **derived from the endpoint** — it can reach
   exactly that `host:port` and nothing else. There is no separate allowlist to
   configure.

4. **localmail must be running** and reachable (`localmail serve`). Co-located
   loopback works through the egress proxy's allowlisted-IP-literal carve-out
   under force-routing; a remote endpoint is a normal allowlisted host (use HTTPS
   off-loopback).

## Behaviour notes

- **Read-only.** Only GET endpoints plus `POST /v1/search` (a POST solely to
  carry the query body) are wired. No send/delete/modify.
- **`smart` query rewrite is off** — workers do not call the LLM; the planner
  already decomposes queries. Base hybrid + rerank is full-fidelity without it.
- **Resolution.** `KASTELLAN_MAIL_ENDPOINT` unset → the worker is *disabled*
  (not registered). Endpoint set but the token file is missing, or the worker
  binary can't be found → *misconfigured* (logged at ERROR; the daemon still
  starts). A `localhost`-**name** endpoint under force-routing is refused by the
  generic endpoint guard — use a literal `127.0.0.1` for loopback.
- **Auth failures** (401/403 from localmail) surface to the agent as a distinct
  "auth/permission denied" message so you know to re-provision the token or fix
  the API user's ACL.
