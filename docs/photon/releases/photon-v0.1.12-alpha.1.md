Review a plan before anyone builds it, comments you can work through, four themes on black, and a new website.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### Plan review

- **A plan opens as a review.** Open a plan from Superpowers, Spec Kit, Kiro or OpenSpec (or any Markdown with tasks) and click **Review this plan**. Its tasks become steps with tick boxes; each task shows the files it touches, the interfaces it produces and uses, and its tests.
- **Gaps, found for you.** A file no task creates, a function nobody defines, code with no test, two tasks on one file, a spec requirement with no task. Each one is a card where it applies, with **Not a problem** or **Add to review**.
- **Your verdict goes back.** Comment on any paragraph with `c`, then **Approve plan** or **Request changes**: your comments go to a new thread in the agent panel or to a `.review.md` next to the plan. ⌥T asks the agent about the task you're on.

### PR comments

- **Every comment in one list.** **Comments ›** at the top of the steps lists every thread with Open, Resolved, Mine, AI and This file filters; `n` and `p` move between open comments across files, `r` replies, `⇧R` resolves.
- **Replies that cost one click.** **Done** replies and resolves; **Quote** quotes the selected text; typing `@` offers the pull request's author, reviewers and commenters; **Preview** shows your Markdown before you send.
- Comments by the pull request's author carry an **author** chip. Chips and headers now use the UI font; only code keeps the code font.

### Themes

- **Photon Black is Tokyo Night on black**: purple keywords, blue functions, green strings, off-white text, muted greens and roses in diffs. The old single neon green is gone.
- **Three more, all on black**: Photon Mocha, Photon Rosé and Photon Vesper, offered at onboarding and under ⌘K ⌘T.

### Call Trails

- Panel text uses the UI font like the rest of Photon; call-site lines and signatures stay in the code font. The map measures its labels properly.

### Website

- New site, with large staged screenshots of the real app and a hero that plays the terminal, a review and the call map: https://heapworks-in.github.io/photon/
