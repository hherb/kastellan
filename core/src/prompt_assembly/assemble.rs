//! Pure prompt assembler. No I/O, no async, no errors.
//!
//! Output framing (always now → L0 → L1 → skills → recalled → tools → handoff → base in this order):
//!
//! ```text
//! <now>
//! Current date and time: {weekday, date, minute, tz abbrev + offset}
//! </now>
//!
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
//! <tools>
//! - {name} (method: {method}): {summary}
//!   params: {p0.name} ({p0.description}) [required|optional], ...
//! </tools>
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
//!    **Note for `<tools>` bodies:** Tool descriptions are compiled-in
//!    Rust literals (each worker's `tool_doc()`), so they are trusted
//!    like L0 — rendered verbatim, no escaping. The `<tools>` block sits
//!    with `<handoff>` (both describe callable mechanisms) and is omitted
//!    entirely when no tool is registered.
//! 4. L0/skills bodies pass through verbatim (no HTML-style escaping
//!    of `<` `>`); **L1 and `<recalled>` bodies are escaped** via
//!    [`escape_untrusted_body`] — see the note below. L0 and surfaced
//!    skills are operator-gated, so they cannot carry laundered content.
//!
//!    **Note for `<skills>` bodies:** Surfaced skills are
//!    operator-approved (`user_approved` / `pinned` trust marker
//!    required — see [`crate::memory::l3_surface::is_surfaceable`]).
//!    Because they are operator-gated they sit with the curated layers
//!    (L0/L1), before the unverified `recalled` output. The
//!    `<skills>` block is omitted entirely when the slice is empty.
//!
//!    **SAFETY — L1 and `<recalled>` bodies are untrusted (adversary #6).**
//!    Both blocks can carry laundered LLM output: `<recalled>` is the
//!    lowest-trust memory source (any process with `INSERT` on `memories`
//!    writes it), and `<l1_insights>` mixes operator-curated rows with
//!    agent-raised rows promoted from `Plan.l1_insight` by the L1
//!    promotion writer — the same untrusted channel. `validate_l1_body`
//!    blocks only L1's *own* delimiter (`</l1_insights>`) and newlines at
//!    write time, so a stored L1 body can still forge *other* framing
//!    (`<recalled>`, `<system>`, a chat-template token) unless escaped
//!    here. This assembler therefore escapes `&`/`<`/`>` and neutralises
//!    control chars (incl. newlines) in **every L1 and recalled body** via
//!    [`escape_untrusted_body`], so no stored row can close a block, forge
//!    framing, or forge an extra `- ` row. Recall gains a second layer:
//!    catalogue screening in [`crate::recall_assembly`] drops rows that
//!    trip the injection guard before they reach a [`RecalledContext`].
//!    The `recalled_block_escapes_framing_delimiters` and
//!    `l1_block_escapes_framing_delimiters` tests pin the escaping; the
//!    recall-builder tests pin the screen. Threat-model reference:
//!    `docs/threat-model.md` (adversary #6) and
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
use crate::worker_manifest::ToolDoc;
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

/// Neutralise prompt-framing in an untrusted memory body so a stored row
/// cannot close its block — or forge any other framing tag (`<base>`,
/// `<system>`, a chat-template token) — and cannot forge an extra `- ` row,
/// injecting content the planner reads as higher-trust structure.
///
/// Applied to **L1 and `<recalled>` bodies** — both carry laundered LLM
/// output (threat-model adversary #6): `<recalled>` is written by any process
/// with `INSERT` on `memories`, and `<l1_insights>` mixes operator rows with
/// agent-raised rows the L1 promotion writer sources from `Plan.l1_insight`.
///
/// Two neutralisations, both render-level guarantees that hold regardless of
/// what any upstream writer or builder allowed:
/// - Escaping `&` / `<` / `>` means no `<tag>` sequence can form, closing the
///   delimiter-breakout / framing-forgery vector.
/// - Replacing every C0 control char (`< 0x20`, which includes `\n` and `\r`)
///   with a space keeps the one-row-per-line contract, so a body cannot forge
///   a sibling `- ` row (nor smuggle NUL / ANSI escapes into the prompt).
///
/// Single pass: `&` maps to `&amp;` unconditionally, so an already-`&amp;`-
/// looking body round-trips exactly as the old chained-`replace` form did.
/// L0/skills are left verbatim — they are operator-gated, not laundered.
fn escape_untrusted_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for c in body.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Render the `<tools>` block: one entry per advertised tool. Trusted
/// compiled-in text (authored in each worker's `tool_doc()`), so — unlike the
/// L1/recalled blocks — bodies are NOT escaped. Emitted only when non-empty.
fn render_tools_block(tools: &[ToolDoc]) -> String {
    let mut out = String::from("<tools>\n");
    for t in tools {
        out.push_str("- ");
        out.push_str(t.name);
        out.push_str(" (method: ");
        out.push_str(t.method);
        out.push_str("): ");
        out.push_str(t.summary);
        out.push('\n');
        if !t.params.is_empty() {
            out.push_str("  params: ");
            let rendered: Vec<String> = t
                .params
                .iter()
                .map(|p| {
                    format!(
                        "{} ({}) [{}]",
                        p.name,
                        p.description,
                        if p.required { "required" } else { "optional" }
                    )
                })
                .collect();
            out.push_str(&rendered.join(", "));
            out.push('\n');
        }
    }
    out.push_str("</tools>\n\n");
    out
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
///
/// `tools` and `now` are appended LAST in the parameter list so existing call
/// sites update with a pure append; render position is decoupled from param
/// order — the `<tools>` block is emitted between `<recalled>` and `<handoff>`,
/// and the trusted `<now>` block (when `Some`) is emitted FIRST, before `<l0>`.
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    skills: &[SurfacedSkill],
    recalled: &RecalledContext,
    base: &str,
    tools: &[ToolDoc],
    now: Option<&str>,
) -> String {
    let mut out = String::new();

    // Trusted, system-generated grounding fact (the current date/time) — emitted
    // FIRST, verbatim (NOT escaped: not adversary-influenced). Omitted entirely
    // when `None`, so the output is byte-identical to the pre-`<now>` assembler.
    // `now` carries its own `<now>…</now>` framing (see `now::render_now_block`).
    if let Some(block) = now {
        if !block.is_empty() {
            out.push_str(block);
            out.push('\n');
        }
    }

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
            // L1 mixes operator rows with agent-raised (laundered) rows; escape
            // every body so a stored row cannot forge framing (audit finding #1).
            out.push_str(&escape_untrusted_body(&row.body));
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
            out.push_str(&escape_untrusted_body(body));
            out.push('\n');
        }
        out.push_str("</recalled>\n\n");
    }

    // Advertised tools (trusted, compiled-in). Grouped with <handoff> as the
    // two capability-describing blocks; omitted entirely when nothing registered.
    if !tools.is_empty() {
        out.push_str(&render_tools_block(tools));
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
