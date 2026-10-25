Excalidraw built in, a cleaner PR review, and Photon's own first-launch setup.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

- **Excalidraw, built in.** Open any `.excalidraw` file and you get the real Excalidraw editor in a tab, with all its tools, its fonts and 30 popular community libraries.
  - Files stay compatible with excalidraw.com and VS Code's extension. Opening a diagram never marks it as changed.
  - Edits save like any file: the unsaved dot, ⌘S, reload when the file changes on disk, and unsaved drawings survive a restart.
  - No network, ever: everything ships inside Photon. Online features (collaboration, share links, the library browser, AI) are left out.
  - Export to PNG or SVG saves to Downloads; SVGs carry their fonts.
  - Built on a new **web editors** capability: an extension can declare an offline web editor for a file type in its `extension.toml`. Excalidraw is the first.
- **A cleaner PR review.**
  - One-line header with a single **Submit review** button; the verdict (Comment, Approve, Request changes) is chosen inside it.
  - A **View** menu: hide whitespace changes, moved code, and generated or lock files, or show only changes since your review.
  - Occasional actions (agent pre-review, run locally, open on GitHub) live in a **⋯** menu.
  - Each step has one **viewed** checkbox; a file is marked viewed on GitHub once all its steps are. Files show added, changed or deleted icons with +/− counts.
  - The checklist sits at the top and folds to one line. A default **Review** checklist is included, plus Dependencies, Database and CI checklists that appear when a PR touches those files. Add your own from the checklist's **Add your own checklist…**.
- **Photon's own first launch.** A fresh install opens Photon's onboarding (theme, keymap, agents, settings import). **Finish Setup** drops you into a terminal. The Photon theme family is the default.
- **Photon everywhere.** Screens, settings descriptions, tooltips and messages say Photon. The built-in Zed agent is turned off; agents run through ACP, with Claude Code as the default.
