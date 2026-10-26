A call hierarchy tree, navigation that works without a language server, and PRs you can open from a link.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

- **Call hierarchy tree.** In the call hierarchy popup (⌘K ⌘H), **Keep Open** moves it into a **Call Hierarchy** panel. Switch between **Callers** and **Calls**, expand any level with the chevrons or arrow keys, and click a row to jump to the call. Recursive calls are marked; right-click a row to make it the root.
- **⌘-click without a language server.** When a file has no language server, go to definition searches the project for a definition with that name: one match opens it, several open as a list.
- **"No language server · Set up".** The status bar says when a file has no language server; **Set up** opens the extension that provides one.
- **Open a pull request from its URL.** In the Pull requests panel, click the link icon and paste a GitHub PR link.
- **No more blank files in PR review.** A file missing from the review's checkout is shown from git, so its diff always appears. If it can't be read at all, the review says so.
