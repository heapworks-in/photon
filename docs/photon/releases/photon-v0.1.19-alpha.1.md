Diagnose Code Navigation: when go to definition or the call hierarchy finds nothing, Photon tells you why, in a report you can share.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew update && brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### Diagnose Code Navigation

Put the cursor where navigation finds nothing and click **Diagnose** in the Call Hierarchy panel, or run **diagnose navigation** from the command palette. Photon asks the language server that's already running what it sees at the cursor and opens a report:

- **The root cause in one line**, with the evidence and what to do: the build can't load, the file's module isn't loaded, the cursor isn't on a function's name, the server sees no code in the file, the server didn't answer, and more.
- **Every check and its answer**: call hierarchy asked directly and as Photon showed it (so a Photon bug can't hide), go to definition, hover, the file's symbols, the modules the server loaded, build errors, and the server's version and saved workspace.
- **A prompt to ask for help**, ready to paste with the report to the Photon team or an AI assistant.

The report has file paths, module names and the word under the cursor, but no source code.

### Call hierarchy

- The panel keeps retrying the last file while you read a diagnosis report.
