//! Pure prompt assembler. No I/O, no async, no errors.
//!
//! Output framing (always L0 → L1 → skills → recalled → handoff → base in this order):
//!
//! ```text
//! <l0_meta_rules>
//! - {body of newest L0 row per l0_rule_id}
//! - {next L0 row body}
//! </l0_meta_rules>
//!
//! <l1_insights>
//! - {body of L1 row, newest-first}
//! </l1_insights>
//!
//! <skills>
//! - {name}: {description}
//!   params: {p0.name} ({p0.description}), ...
//! </skills>
//!
//! <recalled>
//! - {body of recall row #1 (RRF-ranked-first)}
//! - {body of recall row #2}
//! </recalled>
//!
//! <handoff>
//! {protocol for expanding a stashed oversized tool result via the
//!  reserved `handoff`/`fetch` built-in — always present}
//! </handoff>
//!
//! <base>
//! {agent_planner.md verbatim}
//! </base>
//! ```
//!
//! Rules:
//!
//! 1. Empty layers omit their entire tag block — no `<l1_insights>`
//!    when L1 has zero rows. The `<handoff>` and `<base>` blocks are
//!    always present (`<handoff>` is constant compiled-in text).
//! 2. One blank line between sections.
//! 3. Each row renders as `- {body}` (one row per line).
//! 4. L0/L1/skills bodies pass through verbatim (no HTML-style escaping
//!    of `<` `>`). L0 and surfaced skills are operator-gated, and L1's
//!    own delimiter (`</l1_insights>`) plus newlines are rejected at
//!    write time by `validate_l1_body`, so none can break their block.
//!    **`<recalled>` bodies are escaped** — see the note below.
//!
//!    **Note for `<skills>` bodies:** Surfaced skills are
//!    operator-approved (`user_approved` / `pinned` trust marker
//!    required — see [`crate::memory::l3_surface::is_surfaceable`]).
//!    Because they are operator-gated they sit with the curated layers
//!    (L0/L1), before the unverified `recalled` output. The
//!    `<skills>` block is omitted entirely when the slice is empty.
//!
//!    **SAFETY — `<recalled>` bodies are untrusted (adversary #6).**
//!    Recall bodies are the lowest-trust memory source: any process with
//!    `INSERT` on `memories` writes them, including the agent-raised L1
//!    promotion writer, which launders untrusted LLM output into the
//!    store. Recall surfaces whatever the (layer-agnostic) lanes return.
//!    Two layers defend the planner prompt: (a) catalogue screening in
//!    [`crate::recall_assembly`] drops rows that trip the injection
//!    guard before they reach a [`RecalledContext`], and (b) this
//!    assembler escapes `&`/`<`/`>` in every recalled body via
//!    [`escape_recalled_body`] so no stored row can close `<recalled>`
//!    or forge framing. The `recalled_block_escapes_framing_delimiters`
//!    test pins the escaping; the recall-builder tests pin the screen.
//!    Threat-model reference: `docs/threat-model.md` (adversary #6) and
//!    `docs/security-audit-2026-07-02.md` (finding #1).
//! 5. The `<recalled>` block is omitted when the
//!    [`RecalledContext`] is empty (the
//!    failure-degraded state). Recall is enrichment, not policy —
//!    this asymmetry is deliberate.
//! 6. Deterministic: same `(l0, l1, skills, recalled, base)` produces
//!    the same bytes.

use crate::handoff::{MAX_FETCH_BYTES, SUMMARY_HEAD_BYTES};
use crate::memory::l3_surface::{render_skill_entry, SurfacedSkill};
use crate::recall_assembly::RecalledContext;
use crate::scheduler::tool_dispatch::{HANDOFF_METHOD_FETCH, HANDOFF_TOOL};
use kastellan_db::memories::Memory;

/// Render the always-present `<handoff>` block.
///
/// Teaches the planner the `fetch_handoff` protocol: how to recognise the
/// placeholder the dispatcher leaves in place of an oversized tool result, and
/// how to pull the full body back in slices. Compiled-in next to the mechanism
/// ([`crate::handoff`] / [`crate::scheduler::tool_dispatch`]); the tool/method
/// names *and* the byte sizes are interpolated from their source-of-truth
/// constants ([`SUMMARY_HEAD_BYTES`], [`MAX_FETCH_BYTES`]) so the instruction
/// cannot drift from the code that serves it. Constant text — no empty state —
/// so unlike the memory-layer blocks it is emitted unconditionally.
fn render_handoff_block() -> String {
    // Express the byte caps in KiB straight from their constants, so a retuned
    // cap rewrites the prose instead of leaving the planner a stale number.
    let head_kib = SUMMARY_HEAD_BYTES / 1024;
    let max_fetch_kib = MAX_FETCH_BYTES / 1024;
    format!(
        "<handoff>\n\
         Some tool results are too large for the context window. When a tool \
         result is the placeholder object {{handoff_ref, byte_len, summary_head, \
         truncated: true}}, the full output was stashed and only summary_head — \
         the readable first ~{head_kib} KiB — is shown inline. That head is often \
         enough to proceed without fetching anything more.\n\
         To read more of the body, emit a step with tool=\"{tool}\" \
         method=\"{method}\" and parameters={{handoff_ref, offset?, len?}} \
         (offset defaults to 0; len defaults to {max_fetch_kib} KiB and is clamped \
         to that maximum). The \
         step returns {{handoff_ref, offset, len, data, encoding, eof}}, where \
         len is the number of bytes actually returned. To read the whole body, \
         repeat the fetch with offset increased by len until eof is true.\n\
         </handoff>\n\n",
        tool = HANDOFF_TOOL,
        method = HANDOFF_METHOD_FETCH,
    )
}

/// Neutralise prompt-framing delimiters in an untrusted recalled body so a
/// stored memory cannot close the `<recalled>` block — or forge any other
/// framing tag (`<base>`, `<system>`, a chat-template token) — and inject
/// content the planner reads as higher-trust structure.
///
/// Recall bodies are the lowest-trust memory source: *any* process with
/// `INSERT` on `memories` writes them, including the agent-raised L1
/// promotion writer that launders untrusted LLM output into the store
/// (threat-model adversary #6). Escaping `&` / `<` / `>` means no `<tag>`
/// sequence can form in a recalled body, closing the delimiter-breakout
/// vector regardless of what any upstream writer or builder allowed. This is
/// the render-level guarantee; catalogue screening in
/// [`crate::recall_assembly`] is the complementary content-trust layer.
///
/// `&` is escaped first so an already-`&amp;`-looking body round-trips
/// unambiguously. Only the `<recalled>` block is escaped — L0/skills are
/// operator-gated and L1's own delimiter is blocked at write time by
/// `validate_l1_body`.
fn escape_recalled_body(body: &str) -> String {
    body.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the supplied memory slices, surfaced skills, recall context, and
/// base prompt into a single LLM-ready system message.
///
/// See the module-level docstring for the framing rules. Surfaced skills are
/// operator-approved (high-trust) so the `<skills>` block sits after L1 and
/// before the unverified `recalled` output; an empty `skills` slice omits
/// the block entirely, as does an empty [`RecalledContext`] for `<recalled>`.
/// The `<handoff>` and `<base>` blocks are always emitted, so even with every
/// memory slice empty the output is `<handoff>…</handoff>` followed by
/// `<base>…</base>` (not the bare `<base>` block of the pre-handoff assembler).
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    skills: &[SurfacedSkill],
    recalled: &RecalledContext,
    base: &str,
) -> String {
    let mut out = String::new();

    if !l0.is_empty() {
        out.push_str("<l0_meta_rules>\n");
        for row in l0 {
            out.push_str("- ");
            out.push_str(&row.body);
            out.push('\n');
        }
        out.push_str("</l0_meta_rules>\n\n");
    }

    if !l1.is_empty() {
        out.push_str("<l1_insights>\n");
        for row in l1 {
            out.push_str("- ");
            out.push_str(&row.body);
            out.push('\n');
        }
        out.push_str("</l1_insights>\n\n");
    }

    if !skills.is_empty() {
        out.push_str("<skills>\n");
        for skill in skills {
            out.push_str(&render_skill_entry(skill));
        }
        out.push_str("</skills>\n\n");
    }

    if !recalled.is_empty() {
        out.push_str("<recalled>\n");
        for body in &recalled.bodies {
            out.push_str("- ");
            out.push_str(&escape_recalled_body(body));
            out.push('\n');
        }
        out.push_str("</recalled>\n\n");
    }

    // Always-present capability instruction for the reserved `handoff` built-in.
    // Compiled-in trusted text; sits flush before the base operating prompt so
    // `<base>` stays the terminal block.
    out.push_str(&render_handoff_block());

    out.push_str("<base>\n");
    // Collapse 0..N trailing newlines on `base` to exactly one. The
    // closing `</base>\n` then always sits flush against the body —
    // no blank line in front of it — regardless of how the prompt
    // file (or caller) chose to terminate.
    out.push_str(base.trim_end_matches('\n'));
    out.push('\n');
    out.push_str("</base>\n");

    out
}

#[cfg(test)]
mod tests;
