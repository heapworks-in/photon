The Pull requests panel now helps you set up GitHub, including GitHub Enterprise.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

- **GitHub Enterprise repositories show up.** If `gh` was logged in to github.com but not your enterprise host, the enterprise repository was silently skipped. It now shows as "Not logged in to <host>".
- **Fix it from the panel.** Every GitHub CLI problem has a button: **Install GitHub CLI**, **Update GitHub CLI**, or **Log in to <host>**. The command runs in a Photon terminal, and the panel reloads when it finishes.
- **SSH aliases work.** A remote like `git@work-gh:org/repo.git` is matched to the real host behind the alias.
- **Clearer messages.** GitLab, Bitbucket and other non-GitHub remotes say so instead of "not logged in", and the panel shows which repository it found.
- **Docs site:** every capability, illustrated, at https://heapworks-in.github.io/photon/
