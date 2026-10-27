Review only what changed, trust what the AI reviewer tells you, check every agent turn, and an editor that rests when you do.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### PR review

- **Only what you haven't reviewed.** Viewed marks belong to the push you reviewed. When the author pushes again, steps they changed say "Changed since you viewed it", and **View → Only what you haven't reviewed** shows just that.
- **An AI reviewer that checks itself.** After your agent reviews the pull request, it re-checks every finding against the code and drops what it can't confirm: "Kept 5 of 8 findings". Choose how much you hear with `pr_review.agent_review.thoroughness` (quiet, normal or thorough). Deleting an AI draft asks why, and later reviews of that repository don't repeat it.
- **New commits load properly.** Opening a pull request whose author pushed shows "Loading new commits…" and reloads just its tab. Before, it could sit on the old commits and ask you to close your window.
- **A clearer title and description.** The title has its own line; the description is rendered for reading, and a pull request you haven't seen opens on it.
- **Comments you can read at a glance.** Each comment shows who wrote it and when, with Draft, AI, severity, Outdated and Resolved marked only when they matter. Suggestions show as a small diff, and resolved threads fold to one line.

### Agents

- **Review every turn.** Photon snapshots your files before each Claude Code or Codex turn. The files a turn changed appear as chips that open its diff, and **Revert Turn** puts them back.
- **Ask about a selection** with <kbd>⌥T</kbd>, and **queue edits** with <kbd>⌥E</kbd>, then send them together with **Apply All**.
- **This File** in the thread list shows the threads that touched the file you're in.

### Lighter

- **Idle means idle.** About 0.4% CPU when Photon sits focused and idle (it was about 4%), and 0.6% with a diagram open (it was about 10%). The terminal cursor stops blinking after 5 seconds without typing.

### Fixes

- Empty Markdown files open in the editor instead of an empty preview.
