# Test-lift refactors — proving "zero behaviour change"

When a source file grows past the ~500-LOC soft cap, we often **lift** its
inline `#[cfg(test)] mod tests { … }` block out to a sibling `tests.rs`
(the Rust 2018 sibling-directory module pattern: `mod tests;` in the parent
resolves to `<parent>/tests.rs`). Examples in-tree: `db/src/tests.rs`,
`core/src/memory/l0_seed/tests.rs`, `db/src/graph/tests.rs`.

The safety claim for this class of refactor is precise:

> **Zero behaviour change: the production region is byte-identical, and the
> lifted test body is a lossless one-level de-indent of the original block.**

This page is the playbook for *proving* that claim cheaply and decisively,
instead of re-deriving the argument on every lift. Run these two diffs before
you push; both must be empty / exact matches.

## The subtlety this catches

A naive "de-indent everything by 4 spaces" is wrong. Raw-string fixtures
(`r#"…"#` test *data*) sit at column 0 and must **not** shift, while the
surrounding Rust code shifts by exactly 4. The forward round-trip below proves
both invariants at once, so you never have to eyeball it.

## Recipe

Let `FILE` be the file you lifted *from*, `LIFTED` the new sibling
`tests.rs`, `N` the last line of the production region in `FILE` (the line
just before the old `#[cfg(test)]`), and `HEADER_LINES` the number of leading
doc/`use` lines you added at the top of `LIFTED` before the first test.

```sh
FILE=db/src/lib.rs
LIFTED=db/src/tests.rs
N=524              # last production line in FILE after the lift
HEADER_LINES=18    # module-doc + `use super::*;` lines prepended to LIFTED

# 1. Production region is byte-identical to main (nothing leaked across the cut).
diff <(git show "main:$FILE" | sed -n "1,${N}p") \
     <(sed -n "1,${N}p" "$FILE")

# 2. The de-indent is lossless: replay the exact operation on main's inline
#    block and compare to the lifted body.
git show "main:$FILE" \
  | awk '/^#\[cfg\(test\)\]$/{f=1} f' \
  | sed '1d;2d;$d' \
  | sed 's/^    //'                              > /tmp/expected_body.txt
tail -n +"$((HEADER_LINES + 1))" "$LIFTED"       > /tmp/actual_body.txt
diff /tmp/expected_body.txt /tmp/actual_body.txt   # must be an EXACT match
```

- Diff **1** empty ⇒ the production half of the file is unchanged byte-for-byte.
- Diff **2** empty ⇒ the lifted test body is the inline block minus its
  `#[cfg(test)]` / `mod tests {` header and trailing `}`, de-indented one
  level — and nothing else (column-0 raw strings stay put because the
  `s/^    //` only strips a *leading* 4-space run, which a column-0 line
  doesn't have).

If either diff is non-empty, the lift is not behaviour-preserving — stop and
inspect rather than asserting "no behaviour change" in the PR.

## When to use it

Any over-cap test-lift. Keep the lift PR single-purpose (just the move) so the
two diffs above are the *entire* review burden — no logic changes hiding in the
churn.

## Provenance

Recipe distilled during the review of PR #183 (the `l0_seed` lift); recorded
here per issue #184 so future item-9b lifts (`capture`, `recall`,
`macos_seatbelt`, …) reuse it instead of re-deriving the proof.
