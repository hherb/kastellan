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
//! 4. Bodies pass through verbatim (no HTML-style escaping of `<` `>`).
//!    Operators curate L0/L1 content; trust posture matches the rest
//!    of the memory store.
//!
//!    **SAFETY — prompt-injection seam.** This contract holds *only*
//!    while every body fed into the assembler is operator-curated. If
//!    any agent-writable layer (e.g. a future L1 promotion writer that
//!    sources content from agent output) flows rows in here, the lack
//!    of escaping becomes a prompt-injection vector: agent-controlled
//!    text could close the `<l1_insights>` block and inject new
//!    framing the model trusts at meta-rule level. See the L1-writer
//!    follow-up in `docs/devel/handovers/HANDOVER.md` ("recall lane
//!    wiring" / future "L3/L4 writers" — if any *promotion* writer is
//!    added that pulls from agent-authored content, revisit this
//!    contract before merging). Threat-model reference:
//!    `docs/threat-model.md` (LLM-compromise scenario).
//!
//!    **Note for `<skills>` bodies:** Surfaced skills are
//!    operator-approved (`user_approved` / `pinned` trust marker
//!    required — see [`crate::memory::l3_surface::is_surfaceable`]).
//!    Because they are operator-gated they sit with the curated layers
//!    (L0/L1), before the unverified `recalled` output. The
//!    `<skills>` block is omitted entirely when the slice is empty.
//!
//!    **Note for `<recalled>` bodies:** Unlike L0/L1, recall bodies are
//!    *not* operator-curated — any process with `INSERT` privilege on
//!    `memories` writes them, and recall surfaces whatever the lanes
//!    return. Phase 1 trusts the model's tokeniser for recall rows on
//!    the same basis as L0/L1; if an adversarial-input scenario is
//!    identified (e.g. attacker-supplied content in `memories` flowing
//!    here via the recall lane), sanitise before passing to this
//!    function. The `recalled_block_passes_xml_chars_in_body_verbatim`
//!    test pins the current pass-through posture so any future
//!    sanitiser is a deliberate behaviour change, not a silent fix.
//! 5. The `<recalled>` block is omitted when the
//!    [`RecalledContext`] is empty (the
//!    failure-degraded state). Recall is enrichment, not policy —
//!    this asymmetry is deliberate.
//! 6. Deterministic: same `(l0, l1, skills, recalled, base)` produces
//!    the same bytes.

use crate::memory::l3_surface::{render_skill_entry, SurfacedSkill};
use crate::recall_assembly::RecalledContext;
use crate::scheduler::tool_dispatch::{HANDOFF_METHOD_FETCH, HANDOFF_TOOL};
use hhagent_db::memories::Memory;

/// Render the supplied memory slices, surfaced skills, recall context, and
/// base prompt into a single LLM-ready system message.
///
/// See the module-level docstring for the framing rules. Surfaced skills are
/// operator-approved (high-trust) so the `<skills>` block sits after L1 and
/// before the unverified `recalled` output; an empty `skills` slice omits
/// the block entirely. An empty [`RecalledContext`] omits the `<recalled>`
/// tag entirely so the output is byte-identical to the v1 (no-recall)
/// assembler when both `skills` and `recalled` are empty.
/// Render the always-present `<handoff>` block.
///
/// Teaches the planner the `fetch_handoff` protocol: how to recognise the
/// placeholder the dispatcher leaves in place of an oversized tool result, and
/// how to pull the full body back in slices. Compiled-in next to the mechanism
/// ([`crate::handoff`] / [`crate::scheduler::tool_dispatch`]); the tool and
/// method names come from their source-of-truth constants so the instruction
/// cannot drift from the code that serves it. Constant text — no empty state —
/// so unlike the memory-layer blocks it is emitted unconditionally.
fn render_handoff_block() -> String {
    format!(
        "<handoff>\n\
         Some tool results are too large for the context window. When a tool \
         result is the placeholder object {{handoff_ref, byte_len, summary_head, \
         truncated: true}}, the full output was stashed and only summary_head — \
         the readable first ~1 KiB — is shown inline. That head is often enough \
         to proceed without fetching anything more.\n\
         To read more of the body, emit a step with tool=\"{tool}\" \
         method=\"{method}\" and parameters={{handoff_ref, offset?, len?}} \
         (offset defaults to 0; len defaults to and is clamped at 256 KiB). The \
         step returns {{handoff_ref, offset, len, data, encoding, eof}}, where \
         len is the number of bytes actually returned. To read the whole body, \
         repeat the fetch with offset increased by len until eof is true.\n\
         </handoff>\n\n",
        tool = HANDOFF_TOOL,
        method = HANDOFF_METHOD_FETCH,
    )
}

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
            out.push_str(body);
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
