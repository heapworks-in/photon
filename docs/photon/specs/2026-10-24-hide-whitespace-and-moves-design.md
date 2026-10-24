# Hide whitespace and moved code

Two toggles in the PR review header: **Hide whitespace** and **Hide moved code**. Both are off
when a review opens.

## What they hide

- **Whitespace:** lines that differ only in leading, trailing, or repeated whitespace count as
  unchanged. Spaces added or removed inside a line (`f(a,b)` → `f(a, b)`) still show.
- **Moved code:** a block of at least 3 non-blank lines deleted in one place and added in another
  (same file or another file), equal apart from whitespace. A moved block with an edit inside
  splits at the edit: only the parts of 3+ lines on either side count.

## How

- `diff_view::simplified_bases` builds a stand-in base text per file in which hidden changes are
  unchanged lines. Each file's `BufferDiff` gets that base, so the editor's own diff shows only the
  remaining changes. The real base is kept and restored when both toggles are off.
- Applies to whichever file set is shown, including Changes since your review.
- Files left with no changes drop out of the review map. The map's **Hidden from the diff** list
  names each move (`from:line → to:line`) and each such file.

## Limits

- Comments on removed lines are refused while a toggle is on, and existing comments on removed
  lines aren't drawn inline: the stand-in base's line numbers aren't the old file's.
- Commentable lines and GitHub's line mapping still come from the real diff.
- Not remembered between reviews.
