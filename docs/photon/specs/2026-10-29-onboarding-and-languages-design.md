# Onboarding: Photon's settings and languages up front

## Why

Photon's welcome page is Zed's, with Photon's name on it. Users keep asking for settings that only
Photon has (where terminals open, command blocks), and Java users wait through a slow, piecemeal
setup the first time they open a `.java` file. The welcome page is where new users decide how
Photon works for them, so it should ask about what makes Photon different.

## Goals

- Keep Zed's welcome page layout, components and tone: a single scrolling page of sections, each a
  label, a short description and a control. It should look like Zed shipped it.
- Add three sections, after Theme and before Base Keymap: **Terminal**, **Panels** and **Font**.
- Add a **Languages** section, after Agent Setup, that installs the language extensions Photon
  doesn't ship with.
- Every choice writes the user's settings file immediately, as Theme and Vim Mode do today, and every
  one can be changed later on the Settings screen. Onboarding never owns a setting of its own.

## Non-goals

- No new onboarding pages, steps, wizard or Next buttons.
- No PR review settings: they matter only once a PR is open. The Settings screen has them.
- No change to the Settings screen's structure beyond what parity needs (it already has every panel
  setting, the terminal layout, command blocks and the buffer font).

## Anti-goals

- A page that looks different from the rest of Photon or from Zed.
- Help text explaining controls; if a control needs it, the control is wrong.
- Anything that installs or downloads without the user clicking.

## The sections

Order: Theme → **Terminal** → **Panels** → **Font** → Base Keymap → Agent Setup → **Languages** →
Import Settings → Vim Mode / trust switches. Telemetry stays hidden.

### Terminal

- **Layout**: two options, **Center** and **Bottom Panel**, each a small picture of a window (like
  the Theme previews): Center shows a terminal tab filling the editor area; Bottom Panel shows an
  editor above and a terminal strip below it, full width. Writes `terminal.layout`
  (`center` / `panel`). Bottom Panel is the VS Code arrangement; the panel's side (bottom/right) is
  not offered here, only on the Settings screen (`terminal.dock`).
- **Command Blocks** switch → `terminal.blocks`.
- **Smart Blocks** switch → `terminal.smart_blocks`.
- **Notify When Long Commands Finish** switch → `terminal.notify_after_seconds` (on = 10, off = 0;
  shows on when the value is above 0). Turning it off and on resets a custom value to 10. *(My call.)*

### Panels

- **File Panel**: Left / Right → `project_panel.dock`.
- **Agent Panel**: Left / Right → `agent.dock`. Hidden when AI is disabled (`disable_ai`), as Agent
  Setup is.

### Font

Four options, each showing one line of sample code in that style: **Mono** (Lilex), **Hack**,
**Menlo**, **Monaco**. Picking one applies the same settings as the existing `photon: use … font
style` actions (buffer and UI fonts together). The selected option is the one whose buffer font
family matches the current `buffer_font_family` (buffer font only: Menlo and Monaco share a UI font);
none is selected for any other font. *(My call.)*

### Languages

- A row of language cards for extensions Photon doesn't ship: **Java**, Kotlin, Scala, PHP, Ruby,
  Elixir, Dart, Swift, using the extension ids from `extensions_ui`'s suggestion mapping (`java`,
  `kotlin`, `scala`, …). Each card: language name, and **Install**, **Installing…**,
  **✓ Installed**, or, when an install fails (offline, registry error), the error and **Try Again**,
  from the extension store's state. *(My call.)*
- The section always shows; nothing is fetched until the user clicks Install, so it needs no gate on
  `suggest_extensions`. An extension installed here never gets the "install this extension?"
  suggestion later, because that suggestion already checks installed state. *(My call.)*
- **Java**: once the extension is installed, the card runs the
  existing Java scan (`JavaSetup`) right away instead of waiting for the first `.java` file. If a
  usable Java 21 is found, it's used silently, as today. If not, the card shows **Install Java 21**,
  which runs the existing install (a click, never automatic). Progress shows on the card, not in the
  status bar. *(My call.)*

## Java install speed (outside onboarding too)

How it works today (checked in the code): opening a `.java` file without the extension shows the
"install the Java extension?" suggestion. After the extension installs, the extension downloads the
Java language server (jdtls) itself, the first time it starts. Separately, the first time a `.java`
file is active, Photon scans once for Java 21 and, if none is usable, shows a notice whose button
installs it (`brew install openjdk@21` in a terminal, or a download link).

**Decided:** the slow part was the call trace itself, not installing Java, so the install flow stays
as it is. The call trace's wait is handled in the Call Hierarchy panel instead: it now names the
Java server's current work while it loads ("Loading… Java: Building workspace · 45%").

## Not in this design: call hierarchy that never loads

Tracked separately (it is a bug, not onboarding). On a large Java project ⌘K ⌘H opened Zed's call
hierarchy popup, which waits on jdtls with no time limit or progress. The plan there: route ⌘K ⌘H to
Photon's Call Hierarchy panel, show the language server's progress while it works, and give up with
a message and **Try Again** after about 60 seconds. The root cause waits on the user's log from the
large repository.

## Constraints

- Photon fork rules: the onboarding crate is Zed's; changes there are small hooks marked
  `// photon:`. Photon's sections live in a new `photon_onboarding` crate (it can depend on
  `extension_host` and `photon_navigation` without touching onboarding's dependencies), and the
  onboarding page calls into it. *(My call.)*
- The Languages card's Java scan needs a workspace and project: it uses the window the onboarding
  page is in, and skips the scan (installing the extension only) when that window has no local
  project. *(My call.)*
- No "Zed" in any new text.
- Tests: each new control changes its setting (GPUI test driving the page), the Font option shown as
  selected matches the settings, and Languages reflects installed/installing state.

## Left to the builder

- Exact card sizes and the window pictures' drawing (they follow the Theme preview's style).
- Which existing component draws the two- and four-option choices (`ToggleButtonGroup` as the Theme
  and Base Keymap sections use, unless something closer exists).
