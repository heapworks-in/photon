Projects now open as tabs inside one window, like Warp. PR reviews open as tabs too.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

- **Project tabs.** Opening a project adds a tab to the current window instead of a new window. Each extra project uses about 67 MB less memory than a separate window.
  - Tabs restore in order after a restart.
  - You can drag tabs to reorder them.
  - Right-click a tab to close it, close the others, or move it to its own window.
  - Shortcuts: ⌃⇧] next tab, ⌃⇧[ previous tab, ⌃⇧T new tab, ⌃⇧W close tab.
  - **Merge All Windows** in the command palette gathers windows from older versions into one.
- **PR reviews open as tabs** in the current window.
- **Terminal:**
  - Terminals always open in the center.
  - The status bar has a New Terminal button.
  - New empty tabs offer your recent projects over the terminal.
  - The welcome page no longer flashes when a window opens.
- **Pull requests panel:**
  - Aligned header and padded sections.
  - Clear empty states.
  - Real buttons that all share one size.
  - The PR inbox shows only the window's repos.
- Includes the latest upstream Zed changes.
