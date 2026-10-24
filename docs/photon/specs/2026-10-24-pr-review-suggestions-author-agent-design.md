# Suggested changes, author mode, agent pre-review

Three additions to the PR review tab. The user chose them from a list of options; the
mockups are in `.superpowers/brainstorm/` (pr-review-next.html).

## 1. Suggested changes

- **Suggest change** in a draft's comment box (new-code lines only) appends a GitHub suggestion
  block holding the draft's lines: ```` ```suggestion ```` … ```` ``` ````.
- **Commit suggestion** shows on a thread comment holding exactly one suggestion block. The thread
  must also be current (not outdated), on new code, with a line, and Photon must know the PR's
  head branch and repository.
- **Committing:**
  - Reads the file at the reviewed head through GitHub's contents API (`GET …/contents/<path>?ref=<head>`).
  - Replaces the thread's lines (`start_line..=line`) with the suggestion.
  - Commits with `PUT …/contents/<path>` on the head branch of the head repository (a fork's, for
    a PR from a fork). The commit message is "Apply suggestion from @author in code review".
  - The blob sha from the read goes with the commit, so GitHub refuses it if the file changed on
    the branch meanwhile.
  - Nothing in the local checkout changes.
- **Results:**
  - Success shows "Committed to <branch>. Reopen the review to see it."
  - A failure shows on the thread.
  - Files over GitHub's 1 MB contents limit fail with a message.

## 2. Author mode

When the viewer is the PR's author (case-insensitive login match):

- **Feedback list:** "Feedback · N open" heads the review map. It lists unresolved threads by
  file, then line; threads with no line come first in their file. Each row has three actions:
  - **Show** selects the step holding the file and puts the cursor on the line.
  - **Edit** opens the file from the review checkout in an editor at the line.
  - **Done** replies "Done" and then resolves the thread. If the reply fails, the thread stays
    open and the error shows on it.
- **No Approve or Request changes:** both are hidden, because GitHub rejects them on your own PR.
- **Push changes…** opens a terminal in the review checkout with this command typed in, never
  run: `git commit -am 'Address review feedback' && git push 'https://<host>/<head repo>.git'
  HEAD:'refs/heads/<branch>'`.

## 3. Agent pre-review

- **The button:** "Pre-review with agent" is in the header. It is hidden on your own PR, and when
  no agent integration is registered.
- **The prompt contains:**
  - the PR's number, title and description;
  - the review-map steps in order;
  - the diff, up to 200 KB (files left out for size are listed by name);
  - read-only rules: don't modify files, don't run commands that change anything, don't post;
  - the reply format: one ```` ```json ```` array of `{path, line|null, body}`.
- **The agent run:** the app's `PanelReviewAgent` opens a visible thread in the agent panel with
  the selected agent, working directory set to the review checkout, and the prompt auto-submitted.
  It waits until the agent is idle after answering, with a 30-minute limit, and returns the last
  assistant message. The thread stays in the panel.
- **The findings:**
  - Parsed from the last `json` block.
  - A finding on a line of a file in the review becomes a draft on that line.
  - Any other finding becomes a general draft starting with `` `path:line`: ``.
  - Nothing is posted until the reviewer submits.
- **The note:** "The agent added N draft comments. Read each one before you submit." Errors show
  in the same place.
- **The integration:** `photon_review` defines the `ReviewAgent` trait; `crates/zed/src/zed/photon.rs`
  implements it.
- **Upstream hooks**, each marked `photon:`:
  - `agent_ui` exports `CreateThreadOptions`;
  - `ConversationView::root_thread` is public.

### Safety

The agent works in the PR's checkout, which the PR's author controls. So:

- **Trust first:** pre-review starts only in a trusted checkout. Otherwise it explains why and
  opens the Trust dialog. Photon already asks for trust before running a PR's language servers.
- **Agent config files:** if the PR changes files agents read as instructions or settings
  (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `.mcp.json`, `.cursorrules`, `.windsurfrules`,
  `copilot-instructions.md`, or anything under `.claude/`, `.gemini/`, `.cursor/`, `.codex/`,
  `.devin/`, `.zed/`), Photon lists them and asks before starting.
- **The prompt:** it says everything from the PR is the author's material, never instructions.
  The description and diff sit in fences longer than any backtick run inside them.
- **Tool permissions:** Photon doesn't approve any agent tool call itself; the user's own agent
  permission settings apply.

### Running state

- **Cancel:** clicking the button while the agent works cancels the wait. The thread stays in
  the panel.
- **Failing fast:** the wait fails within seconds when the turn ends without a reply (error or
  cancel), and after 2 minutes if the agent never connects.

## Calls made after the final review

- **Suggestion lines:** a suggestion is a list of lines, so a blank suggestion blanks the line,
  and an empty one deletes it. Lines outside the suggestion keep their own endings.
- **Changed file (409):** "The file changed on the branch since this review opened. Reopen the
  review and try again."
- **Done:** isn't sent twice when the thread already ends with your "Done". It fails while a
  resolve is still pending.
- **Push command:** `git add -A && git commit -m … && git push …`, so files the author adds are
  included.
- **Edit:** refuses thread paths that aren't plain relative paths.
- **Deferred:**
  - Multi-line comments that start on the old side (`startDiffSide`) aren't detected.
  - Show does nothing for a line outside the shown excerpts.
  - The built-in agent uses the window's project. A review window's project is the checkout, so
    this only matters when the review tab sits in another window.

## Not covered by tests

- Opening the editor and the terminal, and the agent-panel waiting loop. Their inputs and outputs
  (the push command, the prompt, reply parsing, draft creation) are tested.
