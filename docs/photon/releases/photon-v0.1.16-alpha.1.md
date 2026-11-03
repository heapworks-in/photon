A call hierarchy that traces whole flows, tooltips on every icon, and language servers that recover when they stop answering.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew update && brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### Call hierarchy

- **Flow** traces callers back to where a flow starts and calls through to where it ends, in one map. Library and JDK code shows once, one level deep, without following it further.
- **Callers**, **Calls** and **Flow** are icon buttons with tooltips.

### Language servers

- A language server that stops answering is restarted automatically. When two requests in a row time out while the server sends nothing at all, Photon restarts it and says so. A server that's busy indexing keeps reporting progress and is left alone. This fixes call traces that kept loading forever on Java projects.

### Tooltips

- Every icon button now has a tooltip: the terminal's folder and branch chips, project tab close, and the command block toolbar and row actions, which are now icons.
