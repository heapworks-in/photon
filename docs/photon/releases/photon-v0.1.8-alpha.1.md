Markdown you can read first, and PR reviews where every comment is easy to find.

## Install

```sh
brew tap heapworks-in/photon
brew install photon
```

Already installed? `brew upgrade photon`.

Or download `Photon-aarch64.dmg` below (Apple silicon). Alpha builds aren't notarized yet: on first launch from the DMG, right-click Photon and choose **Open**. The Homebrew install handles this for you.

## What's new

### Markdown

- **Opens for reading.** Markdown files open in the preview. **⌘⇧V** flips the same tab between preview and source; a file with unsaved changes keeps its editor tab.
- **Edit where you're reading.** Double-click anywhere in the preview to jump to that spot in the source.
- **Full-size images and diagrams.** Click an image to see it at the size of the window; hover a Mermaid diagram and click the full-size button. Esc closes it.
- **Missing images say so** instead of disappearing.
- **Excalidraw diagrams in your docs.** Save a diagram as `name.excalidraw.svg` and reference it with `![](name.excalidraw.svg)`: the preview shows it, and opening the file edits it in the Excalidraw tab. Saving keeps the drawing inside the SVG, so it stays editable here and in VS Code.

### PR review

- **See where the comments are.** Steps and files show how many open comments they have, and comments that aren't on a line of the diff (outdated, or on a whole file) are listed at the top of the steps, with **Resolve**.
- **Resizable steps.** Drag the edge of the steps list; double-click it to reset.
- **A sharper review agent.** Its prompt now covers correctness, failure modes, reliability, security, compatibility, performance, tests and code smells, asks it to confirm each problem before reporting it, and includes your team's checklist. Findings show their category and severity, most severe first, and one-line fixes come as suggestions you can commit.
