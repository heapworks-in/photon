# Onboarding: Terminal, Panels, Font and Languages — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Photon's Terminal, Panels, Font and Languages sections to the welcome page, each writing ordinary settings (or installing an extension) the moment it's clicked.

**Architecture:** A new `photon_onboarding` crate renders the four sections as functions of `(&mut isize tab index, &mut App)`, like the onboarding page's own sections. The onboarding crate gains a `photon_onboarding` dependency and two marked calls in `render_basics_page`, plus one subscription so the page redraws when extensions install.

**Tech Stack:** Rust, GPUI, Zed's `ui` components (`ToggleButtonGroup`, `SwitchField`), `settings::update_settings_file`, `extension_host::ExtensionStore`.

**Spec:** `docs/photon/specs/2026-10-29-onboarding-and-languages-design.md`

## Global Constraints

- Order on the page: Theme → Terminal → Panels → Font → Base Keymap → Agent Setup → Languages → Import Settings → switches.
- Every control writes the user's settings file immediately (`update_settings_file`), like Vim Mode; onboarding stores nothing of its own.
- Settings written: `terminal.layout` (`center`/`panel`), `terminal.blocks`, `terminal.smart_blocks`, `terminal.notify_after_seconds` (on = 10, off = 0; on when > 0), `project_panel.dock` (left/right), `agent.dock` (left/right; Agent Panel row hidden when `disable_ai`).
- Font options Mono (Lilex), Hack, Menlo, Monaco dispatch the existing `photon::Use{Mono,Hack,Menlo,Monaco}FontStyle` actions by name; selected = the one whose buffer font family is `Lilex`/`Hack`/`Menlo`/`Monaco`.
- Languages: extension ids `java`, `kotlin`, `scala`, `php`, `ruby`, `elixir`, `dart`, `swift`; card shows Install / Installing… / ✓ Installed from `ExtensionStore` (`outstanding_operations`, `installed_extensions`); nothing fetched until a click.
- Ruling (plan vs spec): the onboarding page has no project to scan, so the Java card installs the extension only; the Java 21 check stays where it is (first `.java` file). The spec's install-failure state is dropped: the store doesn't keep a per-extension failure, and it already reports failures itself.
- Fork rules: Zed files get `// photon:` hooks only; no "Zed" in new text; Title Case for buttons, sentence case for descriptions.

## Review Focus

- A setting already customised (e.g. `notify_after_seconds: 30`) shows as on and isn't changed until clicked → test in Task 1.
- `disable_ai: true` hides the Agent Panel row → test in Task 2.
- A buffer font that's none of the four leaves no Font option selected → test in Task 2.
- An extension mid-install shows Installing… and clicking does nothing → test in Task 3.
- The page redraws when an install finishes (store event) → test in Task 3.

---

### Task 1: `photon_onboarding` crate with the Terminal section, hooked into the page

**Files:**
- Create: `crates/photon_onboarding/Cargo.toml` (`[lib] path = "src/photon_onboarding.rs"`), `crates/photon_onboarding/src/photon_onboarding.rs`
- Modify: root `Cargo.toml` (workspace member + `photon_onboarding` workspace dependency), `crates/onboarding/Cargo.toml` (`photon_onboarding.workspace = true # photon: …`), `crates/onboarding/src/basics_page.rs` (`render_basics_page`)

**Interfaces:**
- Produces: `pub fn render_terminal_section(tab_index: &mut isize, cx: &mut App) -> AnyElement`; `fn notify_is_on(seconds: u64) -> bool`.

- [ ] **Step 1: Failing tests** in `photon_onboarding.rs`: `notify_switch_reads_any_wait_as_on` (`notify_is_on(0) == false`, `notify_is_on(10)`, `notify_is_on(30)`); GPUI test `terminal_layout_choice_writes_the_setting`: render the section in a window with a test settings store and fake fs, click debug selector `onboarding-terminal-layout-panel`, run until parked, assert the settings file text contains `"layout": "panel"`.
- [ ] **Step 2:** `cargo test -p photon_onboarding` → fails (crate/functions missing).
- [ ] **Step 3:** Implement: header "Terminal"; two clickable layout tiles (Center / Bottom Panel, each a small window drawing with divs: sidebar + editor area; Center fills the editor area with a terminal block, Bottom Panel puts an editor on top and a terminal strip below), debug selectors `onboarding-terminal-layout-center` / `-panel`, selected tile bordered with the accent colour; then `SwitchField`s "Command Blocks", "Smart Blocks", "Notify When Long Commands Finish" with one-line descriptions. In `render_basics_page`, `.child(photon_onboarding::render_terminal_section(&mut tab_index, cx)) // photon: …` after the theme section.
- [ ] **Step 4:** `cargo test -p photon_onboarding -p onboarding` → pass.
- [ ] **Step 5:** Commit "Onboarding: a Terminal section for layout and command blocks".

### Task 2: Panels and Font sections

**Files:** Modify `crates/photon_onboarding/src/photon_onboarding.rs`, `crates/onboarding/src/basics_page.rs`.

**Interfaces:** Produces `pub fn render_panels_section(tab_index: &mut isize, cx: &mut App) -> AnyElement`, `pub fn render_font_section(tab_index: &mut isize, cx: &mut App) -> AnyElement`, `fn selected_font_style(buffer_font: &str) -> Option<usize>`.

- [ ] **Step 1: Failing tests:** `font_selection_follows_the_buffer_font` (`"Lilex"→Some(0)`, `"Hack"→Some(1)`, `"Menlo"→Some(2)`, `"Monaco"→Some(3)`, `"Fira Code"→None`); GPUI `panel_sides_write_their_settings` (click `onboarding-file-panel-right` → `"dock": "right"` under `project_panel`); GPUI `agent_panel_row_hides_without_ai` (with `disable_ai: true`, selector `onboarding-agent-panel` not drawn).
- [ ] **Step 2:** run → fail.
- [ ] **Step 3:** Implement: "Panels" with two labelled `ToggleButtonGroup::single_row` rows, File Panel (Left/Right) and Agent Panel (Left/Right, hidden when `DisableAiSettings` says AI is off); "Font" with four tiles, each a name and `fn main() {}` set in that font, clicking dispatches `cx.build_action("photon::Use<Style>FontStyle", None)` via `cx.dispatch_action`. Add both to `render_basics_page` after Terminal, marked `// photon:`.
- [ ] **Step 4:** run → pass.
- [ ] **Step 5:** Commit "Onboarding: Panels and Font sections".

### Task 3: Languages section

**Files:** Modify `crates/photon_onboarding/src/photon_onboarding.rs` (+ `extension_host` dependency), `crates/onboarding/src/basics_page.rs`, `crates/onboarding/src/onboarding.rs` (one subscription).

**Interfaces:** Produces `pub fn render_languages_section(cx: &mut App) -> AnyElement`; `pub fn observe_languages<T: 'static>(cx: &mut Context<T>) -> Option<Subscription>`; `enum CardState { Install, Installing, Installed }`; `fn card_state(id: &str, installed: bool, installing: bool) -> CardState`.

- [ ] **Step 1: Failing tests:** `card_state_prefers_installed_then_installing` (all four combinations); `languages_list_is_photons_choice` (ids in the constraint order).
- [ ] **Step 2:** run → fail.
- [ ] **Step 3:** Implement cards (name + Install button / "Installing…" label / "✓ Installed"); Install calls `ExtensionStore::global(cx).update(|store, cx| store.install_latest_extension(id.into(), cx))`. The section renders nothing when `ExtensionStore::try_global` is absent (tests, remote). `observe_languages` returns `cx.observe(&store, |_, _, cx| cx.notify())` when a store exists; `Onboarding::new` keeps it in its subscriptions (`// photon:`). Add the section after Agent Setup.
- [ ] **Step 4:** run → pass; `cargo check -p zed`.
- [ ] **Step 5:** Commit "Onboarding: install language extensions up front".

### Task 4: Release

- [ ] Clippy for `photon_onboarding`, `onboarding`; full `cargo test -p photon_onboarding -p onboarding`.
- [ ] Release notes `docs/photon/releases/photon-v0.1.15-alpha.1.md` (onboarding, terminal layout setting, call hierarchy fixes, Java source folders); commit; push `main`; `script/photon-release 0.1.15-alpha.1` with `LK_CUSTOM_WEBRTC`; verify release, cask, version.
