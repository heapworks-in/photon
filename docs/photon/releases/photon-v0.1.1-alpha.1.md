Same Photon as 0.1.0-alpha.1, now built and published by the local release script. The README is rewritten too.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

- A new README that says what Photon is for.
- Releases are built on a Mac with `script/photon-release`, which takes minutes instead of an hour. The GitHub workflow stays as a manual backup.
