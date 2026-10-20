The first Photon alpha: a fast, low-memory, terminal-first editor built on Zed, with Warp-style terminal blocks and PR review.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## Sub-project 1a: block terminal (zsh)

- **Command blocks.** Each command and its output form one block, showing the folder, git branch and duration. Failed commands get a red edge. Click a block to select it; cmd-↑/↓ moves between blocks. With a block selected:
  - cmd-shift-c copies the command;
  - cmd-alt-c copies the output;
  - cmd-alt-shift-c copies both;
  - cmd-shift-r runs it again.
- **Warp-style layout.** Output starts at the bottom right above the input, and each block has Warp's spacing and its folder and time line.
- **Warp-style input** pinned to the bottom of the terminal. It's a real text box with mouse selection, undo and multi-line editing (shift-enter). ↑ walks your history.
- **Folder and branch chips.** Click one to jump to a recent folder or switch git branch.
- **Search and completion.** ctrl-R searches your history across projects, and Tab completes using your own zsh setup.
- **Find and open.** cmd-F searches within a block. cmd-click a `path:line` in the output to open it beside the terminal; ctrl-cmd-t switches between the terminal and the editor.
- **Full-screen programs work as usual.** That includes vim, htop, less, ssh, sudo and REPLs.
- **Agent CLIs keep blocks working.** Devin, Claude Code and similar tools run as one block. Side panels opening, window resizes and `clear` keep blocks aligned.
- **Escape hatch.** `"terminal": { "blocks": false }` brings back the classic terminal.

## Sub-project 0: lean baseline

- Rebranded as Photon, with the Photon Black theme and Hack 13 font.
- No network calls at startup: no account, collaboration, telemetry or automatic updates.
- Measured: cold start under 300 ms and about 130 MB of memory at idle.

## Sub-project 1b: smart blocks

- **Test and build runs** from cargo, nextest, pytest, jest, vitest, go, rustc, tsc and gcc/clang get:
  - a summary chip, such as `2 failed · 41 passed`;
  - **Re-run failed** (ctrl-cmd-r), which runs only the failing tests;
  - **Open errors**, which opens each failure location beside the terminal; step through them with F8 / shift-F8;
  - a comparison with the previous run, such as `1 new failure · 2 fixed`.
- **`git status`** blocks get **stage**, **diff** and **discard** buttons on each file (discard always asks first), plus **Stage all** and **Open in Git panel**.
- **`git log`**: cmd-click a commit hash to open that commit.
- **Dev servers** get an **Open localhost:5173** chip and a **Stop** button.
- **Notifications:** a macOS notification when a command that ran 10 s or longer finishes while Photon is in the background, and when a backgrounded dev server prints an error, at most once a minute. Click the notification to jump to the block. Set the threshold with `terminal.notify_after_seconds` (`0` turns notifications off).
- **Output views:** **Open as JSON** (folding and highlighting), **Open as table** (sortable and filterable) and **Copy as CSV**, for JSON output and column-aligned output like `kubectl get` or `docker ps`.
- Every action is also in the block's right-click menu and the command palette.
- **Off switch:** `"terminal": { "smart_blocks": false }` restores plain blocks.

## Also in this release

- Photon's new wave icon.
- Blocks stay aligned when side panels open and the terminal narrows.
- Agent CLIs like Devin and Claude Code no longer switch the terminal back to the classic view.
- `clear` leaves an empty screen with the input at the bottom.

## Sub-project 4a: PR review

- **PR inbox** (a dock panel):
  - Lists open PRs on every host `gh` is logged into, in three groups: *Review requested*, *Yours* and *Mentioned*.
  - Each row shows the checks status, unresolved threads and "N new commits since your review".
  - Row actions: **Start/Continue review** and **Run locally**.
  - It loads only when you open or refresh it. There is no polling.
- **Review workspace:**
  - **Review map:** the change is grouped into ordered steps, with callees before callers and tests last.
  - **Diff with context:** each changed hunk is followed by the unchanged code it calls or is called from, dimmed.
  - **Impact panel:** lists every caller of the function under the cursor. A caller is marked **not updated** when the PR changed the function's signature but not that caller.
- **Comments anywhere:**
  - Select any line, changed or not, and press cmd-alt-m to write a draft.
  - Drafts save as you type, survive restarts, follow their lines when the author pushes, and can be edited or deleted before you submit.
  - Comments on unchanged code land on the nearest diff line, or in the review summary, with a quote and a permalink.
- **Submit** sends one review: Comment, Approve or Request changes.
  - Reply, Resolve and the Viewed checkbox work as on GitHub.
  - Nothing is posted without your click, and nothing is posted twice, even after a crash.
  - A failed submit keeps every draft and shows GitHub's error.
- **Changes since your review** shows only what the author changed after your last review, including across a rebase. Reviews you submitted on github.com count too.
- **Run locally:**
  - Opens a terminal in a separate copy of the PR's code, so your own checkout, branches and stash are never touched.
  - Pre-fills `pr_review.test_command` without running it. Set it in your settings or in your clone's `.zed/settings.json`.

## Safety

- **No code from the PR runs** until you trust it:
  - no git hooks;
  - no language servers until you click Trust;
  - links in the PR are never followed.
- **Trust doesn't carry over.** It is asked again when the author pushes new commits.
- **Your GitHub credentials stay with `gh`.** Photon never reads or stores tokens.

## PR review needs

The GitHub CLI: `brew install gh`, then `gh auth login`.

## Fixes

- Photon now launches while Zed is open. Before, it handed off to Zed and quit.
- Clicking **Start review** or **Run locally** in the PR inbox no longer crashes Photon.
- Builds no longer carry Zed's signing identity or provisioning profile.
