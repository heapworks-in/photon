# PR review diff: what the change means

Mocks: `2026-10-28-pr-diff-mock.html` (approved: all five parts) and
`2026-10-28-pr-diff-styles-mock.html` (style B, "GitHub-style", is the base).

Today the review shows Zed's inline diff: removed lines in a red block with no line numbers, added
lines with green numbers, nothing marking the words that changed. The goal is a diff that reads
like GitHub's and then tells the reviewer what each change means, so a review is faster.

## 1. GitHub-style lines (base)

In the review's diff only (not ordinary editors; a per-editor setting the review turns on):

- Two number columns: old line number and new line number. Unchanged lines show both; removed
  lines show only the old; added lines only the new.
- A `+` / `−` marker column after the numbers.
- Tints: added lines `version_control.added` at ~8% on the line and ~14% on the number cells;
  removed lines the same with `version_control.deleted`. No bright bar.
- Token-level highlights: inside a changed line pair, only the changed tokens are highlighted
  (`version_control.word_added` / `word_deleted`), using the existing word-diff machinery.

## 2. Meaning chips and collapsed renames

Each hunk gets a header row with chips, computed from tree-sitter and the language server, never AI:

- **signature changed**: a function's parameters, return type or visibility changed.
- **behaviour change**: the body changed beyond renames and formatting.
- **rename only**: every change in the hunk is the same identifier renamed; such a hunk folds to one
  line, "`old` → `new` in N places · nothing else changed", with "show lines".
- **moved**: the block exists elsewhere in the base (reuse the moved-code detection in
  `diff_view.rs`), "from line A to B".
- **formatting only**: whitespace and punctuation-only changes (reuse the whitespace view logic).
The file header gains a one-line summary: "3 hunks: 1 behaviour change, 1 rename, 1 moved".

## 3. Reviewed vs new since you viewed

On a review where the user viewed a step at an older head (the existing per-revision viewed marks
and since-review machinery): lines added or changed after that head get an accent edge and a
"new since you viewed" chip on their hunk; hunks entirely from before fade (~40% opacity) and
read "reviewed ✓". First reviews look normal.

## 4. Test marks and inline impact

- Gutter mark per changed line: ✓ when a test function changed or added in the PR reaches the
  function containing the line (call graph data the review already loads; one level of callers is
  enough), ○ on changed lines in a function no changed test reaches. Hover names the test.
- On a hunk with "signature changed", the existing "N not updated" caller data shows as
  "N callers not updated ›", opening the callers popover.

## Rules

Look like Zed; the editor changes are minimal `// photon:` hooks behind a review-only flag; nothing
at idle (all chips computed once per head when the diff loads); every part covered by GPUI tests
that fail without it; the website's review screenshots regenerated.
