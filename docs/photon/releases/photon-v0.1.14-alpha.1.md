A calmer, more consistent PR review in GitHub's colours, and a pass over the whole app so the same things look and behave the same way.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew update && brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### PR review

- **GitHub's diff colours and a reading font.** Photon Black shows changes in GitHub Dark's green and red instead of olive and pink, everywhere changes appear. The review diff reads in JetBrains Mono with roomier lines.
- **Comment where you click.** Right-clicking a line outside your selection comments on that line. Press `c` in the diff to comment, as in plan review.
- **Move through a review from the keyboard.** `]` and `[` go to the next and previous step; **Next Unviewed** jumps to the next step you haven't viewed.
- **New and deleted files at a glance.** A step whose files are all new or all deleted is marked even when it's folded.
- **Pull Requests panel.** Click a row to open its review; **Run Locally** is in the row's right-click menu.
- **Disabled buttons say why**, and actions use one word each: **Delete**, **Try Again**, **Squash and Merge**.

### Terminal

- **Stage All stages only the files the block lists**, not the whole repository.
- **Re-run Command** is greyed out while a command runs instead of doing nothing.
- Blocks take their colours from your theme, so light themes look right.

### Shortcuts

- **⌘N** makes a new file from a terminal too.
- **⌘W** on an empty pane closes only its project tab, not the whole window.
- The agent's selection keys are now **⌃⌥T** (ask about the selection) and **⌃⌥E** (queue an edit), so ⌥ types accented characters again.
- In plan review, **⌘↩** in a comment box saves the comment.

### Settings and menus

- **Settings** has a **Command Blocks** section on the Terminal page and a **Pull Requests** page.
- The View menu lists **Pull Requests** and **Call Hierarchy**. Buttons and menus use Title Case throughout.
- **Revert Turn** asks before putting files back.

### Project tabs and Call Hierarchy

- The tab bar and its **+** button show even with one project.
- The Call Hierarchy panel has a start screen with recent start points, says what a language server is doing, and gives the known fix when one fails to start.
- Java projects find or install Java 21 for the language server.
