A call hierarchy you can actually read: a trail, a map, and the lines that make each call.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### Call Trails

- **A header that tells you what you're looking at.** The function and its signature, "changed in PR #N" when an open review touches it, **Callers** / **Calls**, and a summary like "4 callers · 2 entry points · 1 test".
- **The trail.** A breadcrumb shows the path from your function to the row you selected, so you always know how you got there.
- **Rows that mean something.** Entry points, HTTP handlers, tests and recursive calls are marked; ×2 says how many places make the call. No more "No callers" rows.
- **See the call itself.** Press <kbd>space</kbd> on a caller to show the lines where it makes the call, with the call highlighted. <kbd>↵</kbd> opens the file; <kbd>⌥←</kbd> makes the selected function the new starting point.
- **A map of the same trail.** Switch to the map to see the calls drawn left to right. The path you selected is lit, the rest dimmed; click a function to open it, double-click to expand it.
- **Explain this trail…** sends the trail to your agent in a new thread, only when you click it.

### Website

- The site now leads with the terminal, and every capability has a close-up of the real app instead of a full window.
