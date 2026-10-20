# SP4a.1: Smooth PR review

## Why

The user tried SP4a (v0.4.0) on real PRs. Their verdict was that everything felt bad: PR switches were not smooth, every PR opened a new window, and the whole experience was jittery. Their rule for this work is not to compromise on user experience.

A code trace (2026-10-20) found the causes:

- A click runs about nine sequential steps with no feedback before anything appears:
  - `gh auth status`;
  - a serial scan of recent projects to find the clone;
  - a `git fetch` on every open;
  - worktree setup;
  - a GraphQL detail call that could run alongside the fetch;
  - one `git show` per file, in series;
  - opening a new, empty window;
  - loading every file before showing any.
- After the review appears, the screen moves without the user doing anything:
  - the map reshapes when parsing finishes, then reorders when call edges arrive (with retries up to about 30 s);
  - context excerpts are inserted into the diff;
  - a status strip appears and disappears, moving the editor;
  - scroll resets on every step switch or expand;
  - all comment cards are redrawn once per re-anchored draft;
  - "Since review" resets the whole map.
- A second click while a PR is opening silently does nothing.

## Goal

**Click → a tab opens instantly showing the file list → nothing jumps afterwards → switching PRs is instant.**

## Decisions (the user's)

- **The inbox shows only PRs for the repo(s) open in the current window.**
  - It shows nothing for windows without a GitHub repo.
  - Remote (SSH) windows show an empty inbox with a note. *(Claude's call: `review_root()` is local.)*
- **All PRs open as tabs in one dedicated Review window.**
  - The first click opens the Review window. Later clicks add or focus a tab in it.
  - Switching PRs is a tab switch.
  - The user first asked for tabs in the current window. That turned out to be unsafe (see §1), and the user approved the Review window instead on 2026-10-20.
- **Smoothness is not negotiable.** When speed and correctness of the smart ordering conflict, the screen staying still wins.

## Design

### 1. One Review window, one tab per PR

**Why not tabs in the user's own window:** Zed treats invisible worktrees as internal.

- `TrustedWorktrees::can_trust` returns `true` for them (`trusted_worktrees.rs`, the `!is_visible()` early return), so a hidden PR worktree would be auto-trusted. The PR's `.zed/settings.json` would also load.
- `lsp_store` attaches invisible-worktree buffers to an existing server from a visible worktree (`register_language_server_for_invisible_worktree`). Call hierarchy would then answer from the user's checkout, not the PR head.

Making them visible would clutter the user's project panel, search and file finder.

**Design:**

- **The Review window:**
  - It is a normal workspace window whose project holds only PR worktrees, each one **visible**.
  - Trust, `forget_trust`, refusing parent trust under `review_root()`, and language servers per worktree all work exactly as in SP4a.
  - There is at most one Review window. Photon finds it by looking for a window whose project contains only worktrees under `review_root()`, or by remembering its handle.
- **Opening a PR:**
  - If the Review window doesn't exist, the first open creates it.
  - Otherwise, the PR's worktree is added to the Review window's project and its tab is activated.
  - The window is brought to the front either way.
- **Closing a PR's tab** removes its worktree from the project, which also stops that worktree's language servers. Closing the Review window closes every PR tab.
- **One tab per PR across the whole app.** Because of this, Reload never moves files under another window.
- **The inbox stays in the user's own window** and shows only PRs for that window's repository.

### 2. Instant tab

- The tab item is created on click, in the Review window (created on the first open), from the inbox's `PrSummary`, before its worktree exists.
- It shows straight away:
  - title, number, author and branch;
  - a single fixed-height progress line naming the current stage, such as "Fetching commits…" or "Loading 12 files…".
- Files appear as they load, in path order. The tab never waits for all of them.
- The inbox button shows "Opening…" while the tab loads. A second click focuses the tab.
- **Errors:** shown in the tab body, with what to do next and a **Retry** button. A toast is not enough on its own.

### 3. Less waiting before the first diff

- **`gh auth status`:** cached for the session. It is checked again only after a `gh` call fails with an auth error.
- **Clone lookup:** the clone is the inbox window's repository, so the recent-projects scan is gone.
  - When the PR's repo matches no remote there, for example a window opened on a fork, Photon falls back to its private clone under `review_root()` as today. *(Claude's call.)*
- **Detail call:** the GraphQL detail call runs in parallel with the fetch.
- **Warm open:** if the PR's worktree already exists at the inbox's head SHA, the diff is shown from it with **no network on the critical path**. A background fetch then checks for new commits.
- **Base text:** read for all files in one `git cat-file --batch` process, not one `git show` per file.

### 4. Nothing moves unless the user moves it

- **Map:**
  - The map first appears in a stable order: files in path order, tests last.
  - Smarter grouping or ordering from parsing and call edges is offered as a chip, "Order by call flow", and applied only when the user clicks it.
  - The chosen ordering is remembered per PR.
- **Status strip:** always present at a fixed height, so it never shifts the editor.
- **Context code (callers and callees):**
  - Each changed hunk gets exactly one context line from the start.
  - It reads "callers: loading…", then "3 callers · 1 not updated ▸".
  - The code expands only on click.
  - Loading context never changes the line count of anything above the cursor.
- **Scroll and cursor:**
  - kept per step, and restored when returning to a step;
  - expanding or collapsing keeps the clicked line where it is on screen.
- **Comment cards:**
  - Re-anchoring finishes before the cards are drawn: all drafts are re-anchored first, then the cards are drawn once.
  - Later changes update only the affected card.
- **Since review:** swaps only the diff content. The map, selection and loaded context stay.
- **Impact panel:** fixed width. Its rows fill into reserved space with a loading row, and nothing pops in.

### 5. New commits while a tab is open

A chip reads "N new commits · Reload". Reload runs in the same tab:

- forget trust;
- move the worktree;
- re-anchor drafts.

Its scroll and step are restored where they still exist. This replaces the toast "Close the window for PR #N…". The tab keeps showing the old head until the user clicks Reload.

## Budgets

These are measured in tests where possible, otherwise in a manual walkthrough on a real PR.

- The tab is visible on the frame after the click.
- First diff in under 1 s for a warm worktree with an unchanged head.
- Zero layout shift after the first diff, except from user actions. A test asserts that the selected step's row positions and scroll position are unchanged when analysis, call edges, context and drafts arrive.
- Switching PR tabs keeps each tab's scroll, step and selection.

## Non-goals

- Inbox across all repos (cut by the user's decision; may return later as a toggle).
- New review features. Call graphs and document review are later sub-projects.
- Sharing one language server across PRs. Each PR worktree gets its own once trusted, as in SP4a.
- Reviewing over SSH/remote projects.

## Anti-goals

- Anything that moves on screen without a user action.
- A blank wait with no stage shown.
- Opening a new window for each PR. Only the one Review window is allowed.
- Weakening any SP4a safety rule for speed.

## Left to the builder

- How the progress stages are worded.
- The inbox's empty state for a window with no GitHub repo.
- How stored per-PR UI state (ordering choice, per-step scroll) is kept: in memory or in the review DB.
- The exact chip styling, using existing Zed components.
