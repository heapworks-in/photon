# Photon

**The terminal is where the work happens. Photon puts it at the center.**

Most editors treat the terminal as a drawer at the bottom of the screen. Photon starts there, with a
terminal that understands your commands, and builds the editor, code review and AI agents around it.
It's native, quick to open, and quiet: no account, no telemetry, and no network calls you didn't ask
for.

> Photon is in **alpha**. It's fast and usable every day, and it still has some rough edges.

```sh
brew tap heapworks-in/photon
brew install photon
```

macOS on Apple silicon. Run `photon .` to open the current folder.

---

## A terminal that remembers what happened

Every command and its output become one **block**. A block shows where the command ran, on which
branch, how long it took, and whether it worked. Select a block to copy its command or its output,
or to run it again. You don't have to scroll back through the noise to find what you need.

- **The input behaves like an editor.** You can select text with the mouse, undo, and write
  commands over several lines. ↑ walks your history, and ctrl-R searches it across every project.
- **Tab completion comes from your own zsh setup**, so it completes things the way your shell already
  does.
- **cmd-click `src/main.rs:42` in any output** to open that line right beside the terminal.
- **Full-screen programs work as usual:** vim, htop, ssh, REPLs, and agent CLIs.

## Blocks that know what they're looking at

Photon recognizes common kinds of output and offers the next step:

| You ran                        | Photon offers                                                               |
| ------------------------------ | --------------------------------------------------------------------------- |
| `cargo test`, `pytest`, `jest`… | `2 failed · 41 passed`, **Re-run failed**, jump to each error with F8       |
| `git status`                   | stage, diff or discard each file in place                                   |
| `git log`                      | cmd-click a commit hash to open it                                          |
| a dev server                   | **Open localhost:5173** and **Stop**                                        |
| JSON, `kubectl get`, `docker ps` | **Open as table**, **Open as JSON**, **Copy as CSV**                      |

When a long build finishes while Photon is in the background, it sends a macOS notification. Click
it to land back on that block.

## Code review that reads like code

Pull requests usually arrive as a list of files in alphabetical order. Photon instead arranges a PR
in the order you'd want to read it:

- **A review map** groups the change into steps: functions that get called come before the code that
  calls them, and tests come last.
- **Each change shows the surrounding code it touches,** both the functions it calls and the code
  that calls it.
- **An impact panel** lists every caller of the function under your cursor. It flags the callers the
  PR forgot to update.
- **You can comment on any line,** not only lines in the diff. Drafts save as you type and survive
  restarts.
- **"Changes since your review"** shows only what the author changed after your last review, even if
  they rebased.
- **"Run locally"** opens the PR in a separate copy of the repo, so your own branch and stash are
  never touched.

PR code is untrusted by default. Photon runs no git hooks and starts no language servers until you
click **Trust**, and it asks again when the author pushes new commits. Your GitHub login stays with
`gh`; Photon never reads or stores tokens.

## Your agents, on your plans

Photon doesn't sell you an AI subscription. It connects to the coding agents you already pay for,
such as Claude Code, Devin, or any agent that supports the
[Agent Client Protocol](https://agentclientprotocol.com). The agent runs as you, and Photon has no
server in between.

## Lean on purpose

|                       | Photon             |
| --------------------- | ------------------ |
| Cold start            | under 300 ms       |
| Memory at idle        | about 130 MB       |
| Network at startup    | none               |
| Account required      | no                 |
| Telemetry             | off, and stays off |

## Make it yours

- **Font styles:** from the command palette (cmd-shift-p), choose *Photon: Use Mono / Hack / Menlo /
  Monaco font style*. The editor, menus and terminal change together.
- **Theme:** Photon Black is the default. Switch with cmd-k cmd-t.
- **Off switches:** `"terminal": { "blocks": false }` restores the classic terminal.
  `"terminal": { "smart_blocks": false }` keeps blocks but turns off the extra actions.

## Coming next

- **Call graphs:** what an endpoint or Kafka consumer actually does. You'll see which APIs it
  calls, which topics it produces to, which tables it writes and how many queries it makes. Java and
  Kotlin come first.
- **Tabs and a smoother review flow:** every PR in one review window.
- **Sketching:** diagrams next to your code.

## Build from source

You need Xcode, rustup, and `brew install cmake`.

```sh
cargo run -p zed                   # debug build
script/bundle-mac                  # Photon.app
script/photon-release 0.1.1-alpha.1  # build, publish, and update the Homebrew tap
```

Design notes and plans live in [`docs/photon/`](./docs/photon/).

## Credits and license

Photon is built on the open-source code of [Zed](https://github.com/zed-industries/zed) by Zed
Industries. Its editor core, GPUI framework and language tooling come from Zed, and we're grateful
to that team for publishing it. Photon is an independent project: it is not affiliated with or
endorsed by Zed Industries, and "Zed" is their trademark.

Like the code it derives from, Photon is licensed under **GPL-3.0-or-later** ([LICENSE-GPL](./LICENSE-GPL)),
with components under **Apache-2.0** ([LICENSE-APACHE](./LICENSE-APACHE)) where marked. Every change
Photon makes to Zed's files is marked `photon:` in the source. Run `git grep -n "photon:"` to list
them.

Third-party licenses are checked with [`cargo-about`](https://github.com/EmbarkStudios/cargo-about).
New crates need `publish = false`, and accepted licenses are listed in
`script/licenses/zed-licenses.toml`.
