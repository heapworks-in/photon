# PR comments, upgraded

Mock: `2026-10-27-pr-comments-mock.html`. Three of its four parts are approved; compact mode is not
in this round.

## 1. Comments view and keys

- The steps column gets a second view, **Comments**, reached from a "Comments ›" link at the top
  of the steps (and "Steps ›" to come back). It lists every thread in the pull request: open first
  (newest activity last), then a folded "Resolved · N" group.
- Each row: author (bold), age, chips (AI, severity, suggestion, draft, replies count), the first
  line of the comment, and `path:line` in the buffer font. Drafts that aren't sent yet are rows too.
- Filter chips above the list: Open, Resolved, Mine, AI, This file. Several can be on; the set is
  kept per review while it's open.
- Clicking a row selects the step holding that line and scrolls the diff to the card. The selected
  row is marked.
- Keys in the review: `n` / `p` move to the next and previous open comment across files, in diff
  order; `r` focuses the reply box of the comment under the cursor; `⇧R` resolves it. Hints in the
  footer.

## 2. Replies that cost one click

- **Done** on a thread: posts the reply "Done" and resolves the thread, as one action with one
  error path. Shown for threads on the user's own pull request and for threads the user is asked
  to act on (any thread, for simplicity in v1).
- **Quote**: with text selected in the comment card or the diff, inserts it as a `>` quote at the
  top of the reply box, followed by a blank line.
- `@` completion in reply and draft boxes: typing `@` offers the pull request's participants
  (author, reviewers, commenters, from the detail already loaded; no extra request), with their
  role as a hint. Enter inserts `@login `.

## 3. Draft preview and the author chip

- Reply and draft composers get a **Write / Preview** toggle; Preview renders the Markdown with
  the same element the cards use. The toggle state is per composer.
- Comments written by the pull request's author carry an `author` chip after the name.

## Rules

photon_review only; look like Zed; nothing at idle; every behaviour covered by GPUI tests driven
through clicks and keys with the fake gh runner; the website's `cu-review-comment` close-up and a
new `cu-review-comments-view` scene regenerated.
