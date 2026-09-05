//! One definition of "which characters in untrusted text are dangerous", and
//! the two neutralisations built on it.
//!
//! Untrusted text reaches two very different renderers in this system, and the
//! same characters are hostile in both:
//!
//! - **The model**, via recalled memories and agent-raised L1 insights. There
//!   the risk is framing: a body that closes a block or forges a tag.
//!   `prompt_assembly::escape_untrusted_body` handles that case, adding
//!   `&`/`<`/`>` escaping on top of this class.
//! - **The operator's terminal**, via the daemon log. A worker's stderr is
//!   drained into `tracing` (see [`crate::worker_stderr`]), and a compromised
//!   worker is explicitly in scope — so an ESC or an 8-bit CSI in that stream
//!   is an ANSI sequence executing in whatever terminal is tailing the log.
//!   [`neutralise_controls`] handles that case.
//!
//! The class itself is #544's, arrived at by widening a `< 0x20` rule that
//! neutralised the 7-bit ESC while letting U+009B — the 8-bit CSI a terminal
//! reads identically — straight through.

/// The character class the untrusted-text neutralisers replace with a space.
///
/// Two callers, and keeping them on ONE definition is the point of this module:
/// [`crate::prompt_assembly`]'s `escape_untrusted_body` (what the model may
/// read) and [`neutralise_controls`] (what an operator's terminal may render).
/// The threat is the same character in both places — a compromised worker is in
/// scope (`docs/threat-model.md`) — and a second copy of a security predicate is
/// how #642 and #661 each shipped green.
///
/// Enumerated rather than taken from a Unicode-property crate: the set is
/// small, stable, and each group is here for a stated reason, so a reader can
/// check the guarantee against the list without resolving a dependency.
///
/// 1. **Unicode control codes** — category `Cc`, i.e. C0 (`\n`, `\r`, NUL, the
///    ESC that introduces an ANSI sequence), DEL, and the C1 block. The old
///    rule was `< 0x20`, which stopped at C0 and so neutralised the 7-bit ESC
///    while letting through U+009B, the 8-bit CSI that does the same job in a
///    terminal — and U+0085 (NEL), a line break. Taking the whole category
///    removes the seam instead of listing exceptions to it.
/// 2. **The two line separators outside `Cc`** — U+2028 (LINE SEPARATOR) and
///    U+2029 (PARAGRAPH SEPARATOR). Line breaks to any consumer following the
///    Unicode line-breaking algorithm, so leaving them in would make the
///    one-row-per-line contract a claim about Rust's `char` rather than about
///    the reader (#544).
/// 3. **Bidi formatting controls** — the marks, embeddings, overrides and
///    isolates (U+061C, U+200E, U+200F, U+202A–U+202E, U+2066–U+2069). They are
///    invisible and reorder the *displayed* text that follows, so a stored row
///    can read one way to the operator auditing it and another way in the
///    prompt. Only the control characters go; RTL script itself is untouched.
///
/// All three are replaced with a **space**, never deleted: deleting would
/// silently join the tokens on either side (`a<U+202E>b` → `ab`), and one rule
/// for the whole class is one fewer thing to get wrong than a per-character
/// policy.
pub(crate) fn is_neutralised_control(c: char) -> bool {
    // 1. Category Cc — C0, DEL, C1 (which contains U+0085 NEL and U+009B CSI).
    c.is_control()
        // 2. The line separators that sit outside Cc.
        || matches!(c, '\u{2028}' | '\u{2029}')
        // 3. Bidi formatting controls.
        || matches!(
            c,
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

/// Replace every [`is_neutralised_control`] character in `s` with a space.
///
/// The log-side sibling of `prompt_assembly::escape_untrusted_body`, without
/// the `&`/`<`/`>` escaping: a daemon log line is read by a human in a
/// terminal, not parsed as markup, so entity-escaping would only make it
/// harder to read while buying nothing.
///
/// A space rather than a deletion, deliberately: removing the character would
/// silently join two tokens into one that never appeared in the worker's
/// output, and a log line is evidence.
pub(crate) fn neutralise_controls(s: &str) -> String {
    s.chars()
        .map(|c| if is_neutralised_control(c) { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_class_covers_c0_del_and_c1() {
        // The ESC that opens a 7-bit ANSI sequence, and the U+009B that opens
        // the 8-bit one — the pair whose split was #544's actual defect.
        for c in ['\u{1b}', '\u{9b}', '\u{0}', '\u{7f}', '\u{85}', '\n', '\r', '\t'] {
            assert!(is_neutralised_control(c), "{c:?} must be neutralised");
        }
    }

    #[test]
    fn the_class_covers_line_and_bidi_separators_outside_cc() {
        for c in ['\u{2028}', '\u{2029}', '\u{061c}', '\u{200e}', '\u{202e}', '\u{2069}'] {
            assert!(is_neutralised_control(c), "{c:?} must be neutralised");
        }
    }

    #[test]
    fn ordinary_text_and_zero_width_chars_pass_through() {
        // Zero-width characters are a documented NON-goal (#544): they cannot
        // forge a tag or drive a terminal, so they are a legibility concern.
        for c in ['a', ' ', 'é', '中', '\u{200b}', '\u{feff}'] {
            assert!(!is_neutralised_control(c), "{c:?} must pass through");
        }
    }

    #[test]
    fn neutralise_replaces_rather_than_deletes() {
        // Deleting would join `red` and `text` into a token the worker never
        // wrote, and a log line is evidence.
        assert_eq!(neutralise_controls("red\u{1b}[31mtext"), "red [31mtext");
        assert_eq!(neutralise_controls("plain"), "plain");
        assert_eq!(neutralise_controls(""), "");
    }
}
