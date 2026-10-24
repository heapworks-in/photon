PR reviews get checklists, suggested changes, an author mode and an agent pre-review.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

- **Every open PR shows up.** The Pull requests panel now has an **Other open** list, so a repository's PRs appear even when nobody asked you to review them. Choose your own lists with `pr_review.sections` in settings. Each list is a GitHub search, like `label:backend`.
- **Review checklists.**
  - Photon checks what it can prove and shows why: CI passing, threads resolved, tests changed, tests run in the Run locally terminal, and no TODOs or debug prints added.
  - Add your own questions with `pr_review.checklists`, or for a whole team in `.photon/review.json`. Questions limited to certain paths also show next to that code.
  - Your answers and notes go at the end of the review.
- **Suggested changes.** **Suggest change** turns your comment into a GitHub suggestion. **Commit suggestion** commits one to the PR's branch, the same way github.com does.
- **Your own PRs.** A **Feedback** list walks you through open threads. Each one has **Show**, **Edit** and **Done**, which replies and resolves. **Push changes…** types the commit and push into a terminal for you to run.
- **Agent pre-review.**
  - Your agent reads the PR in the agent panel and leaves **draft** comments. Nothing is posted until you submit.
  - It only runs in a trusted checkout, and it warns you before starting if the PR changes agent instruction files.
- **Comment with the mouse.** Select lines, right-click, and choose **Add comment**.
- **Branches you fetch** appear in the terminal's branch picker. Picking one checks it out.
- **No more blue sessions.** Clicking into an agent or other CLI that's running in a block no longer highlights the whole block.
- **Photon's own version and help.** About, Finder and `photon --version` show Photon's version. The Help menu links to Photon's docs, releases and issues.
