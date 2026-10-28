A pull request diff that tells you what each change means, and calmer chrome for Photon Black.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew update && brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### PR review diff

- **Old and new line numbers.** Every line shows its number before and after the change, with `+` and `−` and tinted number cells. Removed lines finally have numbers.
- **What each change means.** Every hunk is labelled: signature changed, behaviour change, rename only, moved or formatting only. Worked out from the syntax tree, not AI.
- **Renames fold away.** A hunk that only renames something becomes one line, `reason → message in 2 places · nothing else changed`. **Show lines** opens it.
- **A second review is just the new parts.** After the author pushes, changes since you last viewed get a cyan edge and a **new since you viewed** chip; the hunks you already reviewed fade and read **reviewed ✓**.
- **Tests and callers, on the hunk.** Each behaviour change says **✓ tests cover it** (hover names them) or **○ N lines without a test**. A changed signature shows **N callers not updated ›**, which opens them.

### Themes

- **Photon Black's chrome is neutral again.** Sidebars, menus, muted labels and hints are plain grey, and focus and links use Photon's cyan. Tokyo Night's colours stay in code, and text stays white.

### Install

- The Homebrew cask uses Homebrew's newer `postflight_steps`, so upgrading no longer prints a deprecation warning.
