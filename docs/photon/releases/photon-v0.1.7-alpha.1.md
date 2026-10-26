Claude Code is back in the agent panel on a fresh install.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

- **Claude Code and Codex are always in the agent panel.** On a machine where they were never installed, the **+** menu now lists them, and Photon's default agent, Claude Code, works. The agent list downloads the first time you open the agent panel, never at startup. Picking an agent before it has downloaded waits for it, and a failed download says why.
- **Measurements match a real launch.** Photon's network and performance checks now start it as an app, the way you do, and trust their sample folder so no dialog takes focus.
