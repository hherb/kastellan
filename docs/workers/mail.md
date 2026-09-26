# mail worker — read-only localmail access

The `mail` worker gives the agent read-only access to a [localmail](https://github.com/hherb/localmail)
archive over its `/v1` REST API: search, message + attachment retrieval, with
attachments delivered as extracted text **or** as original-format files.

- **Design:** [`docs/superpowers/specs/2026-07-22-localmail-mail-worker-integration-design.md`](../superpowers/specs/2026-07-22-localmail-mail-worker-integration-design.md)
- **Crate:** `workers/mail` (`kastellan-worker-mail`); manifest `core/src/workers/mail.rs`

## Tools

| Tool | Purpose |
| --- | --- |
| `mail.search` | Hybrid semantic + full-text search. Filter by `date_from`/`date_to`/`from`/`to`/`subject`/`has_attachment`/`account_ids`/`folder_ids`/`lang`; `sort` = `rank`\|`date`; page with `cursor`/`next_cursor`. Each hit is projected to `message_id, account, subject, from, date, has_attachments, snippet` (120-char snippet). |
| `mail.get_message` | One message: headers, plaintext body, attachment list `[{index, filename, sha256, content_type, size}]` — `index` is written in by the worker. |
| `mail.list_messages` | Browse newest-first; `account_ids`/`folder_ids` filters; `cursor`. |
| `mail.list_accounts` | Accounts this agent may read. |
| `mail.get_attachment_text` | Server-extracted text of an attachment, **one 8,000-character page at a time** (`offset` + the returned `next_offset`/`total`). Use to **read** it. Address it by `{message_id, filename}` or `{message_id, index}` — or by `{sha256}`, but see *Addressing an attachment* below. |
| `mail.get_attachment` | Save an attachment in its **original format** (PDF, etc.) to the task output dir; returns `{path, size, content_type, filename}`. Use to **deliver** a file. |

The agent does the reasoning (e.g. extracting flight-booking fields into a CSV);
the worker only searches and retrieves. Files delivered by `mail.get_attachment`
are written **directly and durably** into the per-task output dir
`$KASTELLAN_ARTIFACTS_ROOT/<task_id>/` (default `~/.kastellan/artifacts/<task_id>/`)
— the path the tool returns is where the file actually is, and it survives the
task. An empty per-task dir (a task that saved no files) is pruned automatically;
retention/cleanup of delivered files is an operator concern.

### Addressing an attachment

`mail.get_attachment_text` takes **either** `{message_id, filename}` /
`{message_id, index}` **or** `{sha256}`, and the message forms are the ones to
prefer. A message-form attachment is fetched from localmail **by position**
(`/v1/messages/{id}/attachments/{index}`, #760), which re-checks the message's
ACL and serves the entry's own filename; a bare `{sha256}` keeps the hash routes.

A planner reaches a successful step's output through `extract_scannable_text`:
string values only, **keys discarded**, capped at 4 KiB
(`core::scheduler::inner_loop::summary`). A sha256 therefore arrives as an
unlabelled 64-character hex blob that the model must identify by shape and then
transcribe exactly — and on 2026-08-17 (task 160) it did not. The correct hash
was the 6th string in that head, at roughly byte 120; the planner emitted a
different 64 hex chars, localmail answered its `404 no extracted text for
attachment <hash>`, and the agent reported to the user that PDF extraction had
failed while the extracted text — 28 594 characters, including the figure the
user had asked for — sat in the database.

So:

- `{message_id}` alone is enough when the message has one attachment.
- `{message_id, filename}` picks one of several. Matching is exact, then
  case-insensitive, then a *unique* case-insensitive substring, so
  `e-ticket-DQXK68.pdf` finds `Download 470989752-e-ticket-DQXK68.pdf`. An
  ambiguous name is refused with the candidates listed, never guessed.
- `{message_id, index}` picks by position — the `index` `mail.get_message`
  writes into each attachment entry. It is the exact, short key for the case a
  filename cannot settle (below). A `filename` or `sha256` beside it must name
  the same attachment, or the call is refused.
- `{sha256}` still works, and is right when the hash is copied verbatim from a
  previous step's output in the same task.
- `{message_id, sha256}` together is **not** an error. The hash selects — it is
  exact — but is first checked against that message's attachments, so a wrong
  one is caught by the message rather than by localmail's ambiguous 404, and
  the file still gets the archive's own name. A **unique 12-char prefix** is
  accepted here, which is what makes the refusal messages below actionable.
  Naming *both* a `sha256` and a `filename` that resolve to **different**
  attachments is refused rather than silently resolved by precedence.

Not every message can be addressed by filename. Two parts of one message may
share a name (`image001.png` is the usual case) or carry none at all, and there
a repair that said "copy one exactly" would list two identical strings — advice
the planner cannot act on, so it re-sends the same value until the iteration
cap. In those messages the refusal lists the attachments' **`index`** values
instead, because that is the key that actually discriminates. (It listed 12-char
sha prefixes before #760; a unique prefix still selects.)

Three upstream states are also kept distinct, because they need opposite
repairs and localmail returns the same 404 for several of them: a message with
no attachments at all; attachments that are listed but whose content was never
stored (localmail emits `"sha256": null`); and a `message_id` that is either
absent or outside this agent's mail ACL.

`mail.get_attachment` (the deliver-a-file tool) takes the **same two forms**.
Its `filename` does double duty, and the two jobs do not conflict:

- with `message_id`, it *selects* the attachment, and the file is saved under
  the name the archive actually has for it — not the substring that matched, so
  asking for `e-ticket-DQXK68.pdf` still writes
  `<sha12>_Download_470989752-e-ticket-DQXK68.pdf`;
- with `sha256`, it names the output, exactly as it always did.

Both attachment tools translate localmail's 404 into advice whose wording
depends on **where the hash came from**: one the planner typed is most often
mistyped, while one this worker resolved out of a message is right by
construction and must not send the planner to re-copy it.

### Paged text

Extracted text is served **one page of 8,000 characters** at a time
(`workers/mail/src/text_page.rs`), with localmail's `offset`, `total` and
`next_offset`, plus a `more` note naming the next call while text remains. A
whole text (up to 2.1 MB live) used to overflow the planner's 16 KiB step view
and fall into the truncation path (#678). `next_offset` is **copied from
localmail, never computed** — offsets count code points, and arithmetic on the
returned text skips text on documents with characters above U+FFFF. A text body
without the paging fields is refused as a service fault rather than served as a
complete page.

### localmail version

Search and both attachment tools first ask `GET /v1/version` and refuse — naming
the upgrade — unless localmail reports `api_major` 1 and `api_minor` ≥ 3 (slice
D's index routes and paging, slice E's `fields`/`snippet_chars`). There is no
fallback to the older routes: an older server answers the index route with the
same 404 as a missing message and ignores paging, so trying would give wrong
answers rather than an error. The other three tools keep working against an
older localmail. A pass is remembered for the worker's lifetime; a refusal is
asked again.

### Naming the accounts to search

`mail.search` accepts `account_ids` / `folder_ids` **either** at the top level
(where `mail.list_messages` takes them) **or** inside `filters` — but not both,
which is refused rather than resolved by precedence. Ids may be written as
numbers or digit-strings either way; the worker sends localmail the strings its
`SearchFiltersModel` requires. Before this, a top-level `account_ids` was
rejected as an unknown field and a numeric one inside `filters` came back as a
raw FastAPI 422 that no planner could act on — between them they cost one live
task its entire iteration budget.

An explicit `null` (`{"filters": {"account_ids": null}}`) means *absent*, the
same as omitting it — matching how an optional `message_id` is read — and never
reaches localmail as a filter value. An explicitly **empty list** is refused,
because it asks localmail to filter by nothing.

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

   > The attachment is buffered in memory before it is written, so keep
   > `KASTELLAN_MAIL_ATTACHMENT_MAX_BYTES` comfortably below the worker's memory
   > budget (`mem_mb: 256` in `core/src/workers/mail.rs`). A cap larger than that
   > only means an oversized attachment gets the worker OOM-killed by its cgroup
   > (surfacing to the agent as an `OPERATION_FAILED` transport error), never a
   > containment breach — but it is a needless failure. Raise `mem_mb` too if you
   > genuinely need a larger cap.

   The worker's network allowlist is **derived from the endpoint** — it can reach
   exactly that `host:port` and nothing else. There is no separate allowlist to
   configure.

4. **localmail must be running** and reachable (`localmail serve`). Co-located
   loopback works through the egress proxy's allowlisted-IP-literal carve-out
   under force-routing; a remote endpoint is a normal allowlisted host (use HTTPS
   off-loopback).

5. **Self-signed / private-CA localmail over HTTPS.** Under force-routing the
   proxy re-originates upstream TLS with webpki trust only, so a localmail
   serving a self-signed or private-CA cert on a **private IP literal** also
   needs the operator anchor on the daemon:

   ```sh
   KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA='{"<private-ip>":"/abs/path/to/cert.pem"}'
   ```

   Trust-scope is enforced and fail-closed: the anchor is handed out only when
   the worker's allowlist resolves to a **single private origin** (enforced at
   parse/spawn; a refusal fails the spawn), and the PEM is read at daemon
   startup. The cert itself must be a **non-CA leaf** (`CA:FALSE`) — a `CA:TRUE`
   self-signed leaf, the common `openssl req -x509` shape, is rejected by the
   proxy's rustls at handshake (`CaUsedAsEndEntity`) even though `openssl
   verify` accepts it. Verify the shape with:

   ```sh
   openssl x509 -in <cert.pem> -noout -text | grep -A1 'Basic Constraints'
   ```

   This covers the mail **tool**'s MITM'd egress sidecar only; the email
   *channel*'s (`email-in`) force-routed sidecar is a transparent tunnel, which
   `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` does not affect.

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
