Finish a review in Photon: see if it can merge, merge it, and talk it through. Plus help when a language server won't start.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### PR review

- **Merge readiness at a glance.** A pill in the review header says the one thing that matters: Ready to merge, Checks failing, Conflicts, Changes requested, Review required, Behind main, Draft or Merged. Click it to see each check, the review decision and mergeability.
- **Merge from the review.** Squash, merge or rebase, whichever your repository allows. Photon asks first, warns about comments you haven't sent, and merges only the commits you reviewed: if someone pushed since, it asks you to refresh.
- **Conversation.** The first row of the steps list shows the description, comments, reviews and pushed commits in order, with a box to post a comment.
- **A sharper review agent.** It reviews like the developer who gets paged when the change breaks: logic bugs, security, data loss, performance, clean APIs, missing tests, lean code and your codebase's conventions. It confirms each finding with a concrete case, drops what it can't confirm, and labels findings must fix, should fix or nice to have.

### Language servers

- **Failed servers explain themselves.** When a language server can't download or start, the status bar says "Language server failed · Help": the error, a known fix (for example "jdtls needs Java 21 or newer" with the install command and setting), **Restart** and **Show Log**.
- **Missing servers get set up.** "No language server · Set up" shows the extension to install, any tool it needs with the command to install it, and **Detect & Start**.
