A welcome page that sets up Photon your way, terminals in the center or the bottom panel, and a call hierarchy that works on Java projects.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew update && brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### Welcome page

- **Terminal**: choose where terminals open, **Center** (beside your files) or **Bottom Panel** (like VS Code), and turn command blocks, smart blocks and long-command notifications on or off.
- **Panels**: put the file panel and the agent panel on the left or right.
- **Font**: pick Mono, Hack, Menlo or Monaco, each shown in its own font.
- **Languages**: install Java, Kotlin, Scala, PHP, Ruby, Elixir, Dart or Swift support up front, so a project opens ready.
- Every choice is an ordinary setting you can change later in **Settings**.

### Terminal layout

- New `terminal.layout` setting (**Settings → Terminal → Layout and Blocks**): `center` keeps terminals as tabs beside your files; `panel` puts them in the bottom panel with the editor above, like VS Code, with the panel's button in the status bar and **⌃\`** to show and hide it. Same block terminal either way.

### Call hierarchy

- **⌘K ⌘H** opens the Call Hierarchy panel from anywhere in the window, including while the panel itself has focus.
- While a trace loads, the panel says what the language server is doing ("Java: Building workspace · 45%"), since the first search on a large project waits on its indexing.
- When there's no function at the cursor, the panel says so, and when the file has errors, that the server may not see the project's sources.

### Java

- IntelliJ projects without Maven or Gradle now work: Photon tells the Java language server the source folders from the project's `.iml`, so imports resolve and the call hierarchy finds callers. Nothing is written to your repository.
