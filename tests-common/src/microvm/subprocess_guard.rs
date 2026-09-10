//! A source-level guard: no preflight may shell out without a budget (#690).
//!
//! # Why a source guard rather than a runtime one
//!
//! The defect this guards against has **no runtime signature**. An unbounded
//! `Command::output()` that never returns produces no `[SKIP]`, no `[WARN]`,
//! no panic and no message at all — it just stops. There is no assertion that
//! can fire, because nothing ever reaches an assertion. The only place the
//! defect is visible is the source, which is the same reasoning that made
//! [`super::guard::bypassed_gates`] a source scanner in #683.
//!
//! # The rule
//!
//! Every line in the scanned set that finishes a `Command` — `.output()` or
//! `.status()` — must either go through
//! [`kastellan_sandbox::bounded_command`] or carry an [`EXEMPT_MARKER`] with a
//! reason. **Fail-closed on a shape**, not on a roster of known-bad files:
//! #683's review found a denylist of names reporting green on a live instance
//! of the very defect it was built for, and the lesson was to key on the shape
//! and let each exception justify itself at the site.
//!
//! # What this scanner cannot see, stated rather than implied
//!
//! It decides "is this line a comment?" by asking whether the trimmed line
//! starts with `//`. That is crude, and the direction of its crudeness is what
//! matters: a `.output()` inside a *string literal* would be reported as a
//! violation (**fail-closed** — noisy, safe), while the only way to hide one
//! from it is to put real code after `//` on the same line, which is not code
//! at all. It deliberately does **not** reuse
//! [`super::guard`]'s string-aware stripper: that stripper models neither raw
//! strings nor `'"'` char literals, and both occur throughout the production
//! sources scanned here — it would blank whole files and report them clean,
//! which is the failure mode this module exists to refuse.
//!
//! The scanned set is **discovered**, never hand-listed, so a new file is
//! covered the day it is written.

/// The marker that exempts one shell-out, and must carry a reason after it.
///
/// Deliberately a different word from [`super::guard::EXEMPT_MARKER`]: the two
/// rules exempt different things, and one marker serving both would let an
/// exemption written for one silently satisfy the other.
pub(crate) const EXEMPT_MARKER: &str = "BOUNDED-EXEMPT";

/// How many lines above the offending line an [`EXEMPT_MARKER`] may sit.
///
/// Three, not two: a builder chain puts `.output()` several lines below the
/// `Command::new(...)` the exemption naturally comments on, and rustfmt moves
/// that distance around. The marker still has to be adjacent enough to be
/// about *this* call rather than a general disclaimer at the top of a file.
const EXEMPT_WINDOW: usize = 8;

/// The two ways a `Command` is finished in this workspace without a budget.
///
/// `.spawn()` is absent on purpose: a spawned child is one the caller goes on
/// to manage itself (that is what [`kastellan_sandbox::bounded_command`] does
/// internally, and what every worker launch does), so it is not the
/// fire-and-wait-forever shape.
const UNBOUNDED_FINISHERS: &[&str] = &[".output()", ".status()"];

/// One shell-out with no budget and no exemption.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UnboundedShellOut {
    /// 1-based line number, for an operator opening the file.
    pub line: usize,
    /// The offending line, trimmed.
    pub text: String,
}

/// Pure: the unbounded, unexempted shell-outs in one Rust source.
///
/// Pure so every arm is reachable from a unit test over a source this
/// directory does not contain — #683's review found a guard that agreed with
/// the census that scoped it because both were written from the same
/// assumption, and the remedy was to test the check against shapes nobody in
/// the tree had written yet.
pub(crate) fn unbounded_shell_outs(src: &str) -> Vec<UnboundedShellOut> {
    let lines: Vec<&str> = src.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| finishes_a_command(line))
        .filter(|(idx, _)| !exempt_near(&lines, *idx))
        .map(|(idx, line)| UnboundedShellOut { line: idx + 1, text: line.trim().to_string() })
        .collect()
}

/// Pure: does this line finish a `Command` without a budget?
///
/// A line whose *first* non-space characters are `//` is prose — including the
/// `//!` module docs and `///` item docs this tree writes a great deal of, one
/// of which shows an unbounded call as the thing it is warning about.
fn finishes_a_command(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") {
        return false;
    }
    UNBOUNDED_FINISHERS.iter().any(|f| trimmed.contains(f))
}

/// Pure: does an [`EXEMPT_MARKER`] sit on this line or within
/// [`EXEMPT_WINDOW`] lines above it?
fn exempt_near(lines: &[&str], idx: usize) -> bool {
    let first = idx.saturating_sub(EXEMPT_WINDOW);
    lines[first..=idx].iter().any(|l| l.contains(EXEMPT_MARKER))
}

/// Every `.rs` file under `dir`, recursively, as (repo-relative path, source).
///
/// ⚠️ An unreadable entry is a **panic**, never a skip. A walker that silently
/// drops what it cannot read is a guard that stops admitting what it can no
/// longer check — the exact three-way silent loss #688's review found in this
/// module's sibling.
pub(crate) fn rust_sources_under(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read dir {dir:?}: {e}"));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("dir entry under {dir:?}: {e}"));
        let path = entry.path();
        // `file_type()`, not `path.is_dir()`: the latter follows symlinks, so a
        // link at an ancestor recurses until the stack ends.
        let kind = entry.file_type().unwrap_or_else(|e| panic!("file type {path:?}: {e}"));
        if kind.is_dir() {
            out.extend(rust_sources_under(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            out.push((path.display().to_string(), src));
        }
    }
    out
}

#[cfg(test)]
mod tests;
