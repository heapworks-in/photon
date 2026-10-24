# Review checklists

## Why

Reviewers want a repeatable list of things to confirm on every PR (security, tests, migrations),
without the review turning into form-filling. The mockups offered six options; the user chose the
recommended mix:

- **D. Photon checks for you:** items Photon can prove are ticked automatically, with the evidence.
- **B. On the steps:** questions that belong to certain files show up under the review-map step that
  holds those files.
- **C. At the end:** the answers go into the review text the author sees on GitHub.
- **E. Where items come from:** the user's settings, a team file in the repo, and the author's
  checkboxes in the PR description.

Not in scope: progress in the inbox (option F), a separate checklist tab (option A).

## Goals

- Most of the list is done before the reviewer starts.
- A question appears next to the code it is about.
- Nothing blocks approving: unanswered items are reported, never enforced.
- With no checklists configured, the review looks as it does today plus a short, read-only
  "Photon checked" list.

## Anti-goals

- Photon never ticks a judgement call. Only facts it can show evidence for.
- A PR can't change the checks it is reviewed against.
- No network calls beyond the ones the review already makes (the PR body joins the existing detail
  query).

## Where items come from

1. **Settings** `pr_review.checklists`: a list of `{ name, when?, items }`.
   - `items` are strings.
   - `when.paths` are globs matched against the PR's changed files; `when.repos` are `owner/repo`
     names (case-insensitive). A missing `when` or an empty list means "always".
   - A checklist applies when both its `repos` and `paths` rules match.
2. **Team file** `.photon/review.json` in the repository, same shape: `{ "checklists": [...] }`.
   Read with `git show <merge-base>:.photon/review.json` from the review checkout, so it comes from
   the base branch and the PR can't edit it. A missing file is silent; a malformed one shows one
   error line in the checklist section.
3. **The author's checkboxes:** `- [ ]` / `- [x]` lines (also `*`) in the PR description, shown
   read-only under "The author says". They aren't counted in progress.

Each item has a stable key, `"{source}:{checklist name}:{item text}"`, so answers survive reloads.

## Auto checks (D)

`pr_review.auto_checks` lists which ones run; the default is all five:

| Name | Passes when | Fails when | Unknown when |
|---|---|---|---|
| `checks_pass` | CI rollup is passing | failing | pending, or no checks |
| `threads_resolved` | every review thread is resolved (or there are none) | any is unresolved | — |
| `tests_changed` | a changed file is a test file (the review map's own rule) | none is | — |
| `ran_locally` | a test command run in the PR's Run locally terminal exited 0 on the current head | it exited non-zero | nothing was run |
| `no_debug_leftovers` | no added line contains `TODO`, `FIXME`, `dbg!(`, `console.log(`, `debugger;`, `breakpoint()` | one does (first `path:line` given) | — |

- Every result carries a one-line evidence text.
- Results are recomputed from the current PR data, so a new push updates them. The Run locally
  record is tied to a head commit and ignored once the head moves.
- An auto check that is unknown can be answered by hand like any item. One that failed can only
  be marked N/A: evidence of a failure overrides an earlier Yes (say, given while CI was pending).
- **Ran locally record:** when Run locally opens a terminal, Photon watches that terminal's command
  blocks. A finished command counts when it starts with `pr_review.test_command` (if set), or
  one of its words is `test`, starts with `test:`, or is a known test runner (`pytest`, `jest`, …). The latest such command and its exit code are
  stored per PR and head commit.

## Answering (B)

- Manual items are answered **Yes**, **No** or **N/A** (click again to clear).
- A **No** opens a short note. It is edited in the sidebar list; under a step it shows as text.
- Items from a checklist with `when.paths` also appear under the selected review-map step, but only
  if that step contains a matching file. The same answer shows in both places.
- Answers are stored locally per PR, like viewed steps. Notes use the existing composer-text
  storage.

## Layout

- **Review map sidebar:** a "Checklist · {done}/{total}" section between the steps and the legend,
  with three groups:
  - "Photon checked": auto checks that passed, each with its evidence.
  - "For you": manual items, plus auto checks that didn't pass.
  - "The author says": read-only.
- **Header:** the progress text becomes "2 of 5 steps viewed · checklist 6/9".
- **"Your review":** one line above the summary, "Checklist 6/9 · 1 No".
- **Progress counting:**
  - total = applicable auto checks + manual items;
  - done = auto checks that passed + items answered Yes or N/A;
  - a No counts as answered, not done, and is shown separately.

## Submitting (C)

If any manual item exists or any item was answered by hand, the review body ends with:

```
### Checklist (6/9)
- [x] CI checks pass: all checks passed
- [ ] Safe to replay: **No**, needs an idempotency key
- N/A Migration is reversible
- [ ] Card data isn't logged: not checked
```

- The block closes the review body, after the summary and any comments that couldn't sit on a line.
- A review where only passing auto checks exist adds nothing, so default reviews look the same on
  GitHub.

## Builder's calls

- Settings shape: items are plain strings. Auto checks are a separate list, not items.
- The note editor exists only for No answers.
- Glob matching uses the workspace's `globset`.
