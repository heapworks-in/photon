# In-window Project Tabs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Projects open as tabs inside one window (one set of Metal drawables) instead of native macOS window tabs.

**Architecture:** A tab is a `Workspace` held by the window's `MultiWorkspace` (`crates/workspace/src/multi_workspace.rs`), which already renders only the active workspace and serializes the rest. A new `photon_tabs` crate draws the strip in the title bar and handles the tab actions; Zed files get small `// photon:` hooks. Upstream already opens projects into the current window (`default_open_behavior` / `cli_default_open_behavior` are `existing_window`).

**Tech Stack:** Rust, GPUI, Zed `workspace` / `platform_title_bar` / `ui` crates.

**Spec:** `docs/photon/specs/2026-10-23-in-window-tabs-design.md`

## Global Constraints

- Photon code in `crates/photon_tabs` (new) or `crates/zed/src/zed/photon.rs`; Zed files get minimal hooks, each marked `// photon: <why>` (`# photon:` in TOML/JSON comments).
- Never delete upstream code; prefer defaults in `assets/settings/default.json`.
- New crate: `[lib] path = "src/photon_tabs.rs"`, `doctest = false`, license `GPL-3.0-or-later`, `[lints] workspace = true` (copy `crates/photon_brand/Cargo.toml`).
- No `unwrap()` outside tests, no `let _ =` on fallible calls (use `.log_err()` / `detach_and_log_err`).
- Strip visuals match `SystemWindowTabs::render_tab` (`crates/platform_title_bar/src/system_window_tabs.rs:142`): height `Tab::content_height(cx)`, `px(DynamicSpacing::Base16)`, `border_l_1` in `colors().border`, active bg `title_bar_background`, inactive bg `tab_bar_background`, label `LabelSize::Small` truncated, `Color::Default` active / `Color::Muted` inactive, close button placement from `ItemSettings` (`close_position`, `show_close_button`), min width `rem_size * 10`.
- The strip shows only when the window holds 2 or more workspaces.
- Lint with `./script/clippy`. Commits carry no Claude attribution.

## Review Focus

1. Closing a tab with unsaved files prompts; cancelling keeps the tab and its edits (Task 3 test `close_tab_with_unsaved_changes_can_be_cancelled`).
2. Two empty projects are two tabs, not merged into one (Task 3 test `empty_projects_are_separate_tabs`).
3. Closing the active tab activates its right neighbour, or the left one when it was last (Task 3 test `closing_active_tab_activates_neighbour`).
4. After a tab switch, keyboard focus is in the new tab, so typing goes to its terminal/editor (Task 3 test `switching_tab_moves_focus`).
5. Opening a project that is already a tab activates that tab instead of adding another (Task 1 test `open_existing_project_activates_its_tab`).

---

### Task 1: Keep every workspace in the window, even with AI disabled

`MultiWorkspace` retains background workspaces only when `multi_workspace_enabled` (AI on). Tabs need retention always; the sidebar keeps its AI gate.

**Files:**
- Modify: `crates/workspace/src/multi_workspace.rs` (add method; use it at `activate` ~line 1321 and `open_project` ~line 1931)
- Modify: `crates/workspace/src/workspace.rs` (~line 11354, the `use_existing_window` target filter)
- Test: `crates/workspace/src/multi_workspace_tests.rs`

**Interfaces:**
- Produces: `MultiWorkspace::retains_workspaces(&self, cx: &App) -> bool` — always `true` (`// photon: tabs keep every project in the window`).

- [ ] **Step 1: Write failing tests** in `multi_workspace_tests.rs` (use the file's existing setup, which already toggles `DisableAiSettings`):
  - `photon_activate_retains_workspace_with_ai_disabled`: with `disable_ai: true`, create workspace A, `activate` workspace B; assert `workspaces().count() == 2` and A is still listed.
  - `photon_open_project_adds_tab_with_ai_disabled`: with `disable_ai: true` and A holding a worktree, `open_project(vec![other_dir], OpenMode::Activate, …)`; assert 2 workspaces and the active one is the new project.
  - `open_existing_project_activates_its_tab`: A and B open, B active; `open_project` with A's path; assert `workspaces().count() == 2` and `workspace() == A`.
- [ ] **Step 2: Run** `cargo nextest run -p workspace photon_ open_existing_project` — Expected: the two `photon_` tests FAIL (count is 1); `open_existing_project_activates_its_tab` may already pass.
- [ ] **Step 3: Implement** `retains_workspaces` and replace `self.multi_workspace_enabled(cx)` with `self.retains_workspaces(cx)` at the two `MultiWorkspace` sites and `mw.multi_workspace_enabled(cx)` with `mw.retains_workspaces(cx)` at the `workspace.rs` site; each call site gets a `// photon:` comment. Leave every sidebar/render use untouched.
- [ ] **Step 4: Run** `cargo nextest run -p workspace` — Expected: all pass (upstream tests asserting `!multi_workspace_enabled` still pass because that method is unchanged).
- [ ] **Step 5: Commit** "Keep every project in the window so it can be a tab".

### Task 2: Reorder workspaces

**Files:**
- Modify: `crates/workspace/src/multi_workspace.rs`
- Test: `crates/workspace/src/multi_workspace_tests.rs`

**Interfaces:**
- Produces: `MultiWorkspace::move_workspace(&mut self, from: usize, to: usize, cx: &mut Context<Self>)` — reorders `held`; out-of-range indices are a no-op; calls `serialize(cx)` and `cx.notify()`. Marked `// photon: drag to reorder tabs`.

- [ ] **Step 1: Write failing test** `photon_move_workspace_reorders`: workspaces A, B, C; `move_workspace(0, 2)` → order B, C, A; `move_workspace(5, 0)` → unchanged; the active workspace is the same entity before and after.
- [ ] **Step 2: Run** `cargo nextest run -p workspace photon_move_workspace` — Expected: FAIL (no method).
- [ ] **Step 3: Implement.** The displayed index is derived from `held`; keep `active_workspace_id` pointing at the same entity.
- [ ] **Step 4: Run** `cargo nextest run -p workspace` — Expected: all pass.
- [ ] **Step 5: Commit** "Let projects in a window be reordered".

### Task 3: `photon_tabs` crate — the strip

**Files:**
- Create: `crates/photon_tabs/Cargo.toml`, `crates/photon_tabs/src/photon_tabs.rs`
- Modify: root `Cargo.toml` (workspace member + `photon_tabs = { path = "crates/photon_tabs" }`)

**Interfaces:**
- Consumes: `retains_workspaces` (Task 1), `move_workspace` (Task 2), upstream `MultiWorkspace::{workspaces, workspace, activate, remove, open_project_group_in_new_window}`, `RemovalIntent::CloseProject`.
- Produces:
  - `pub struct ProjectTabs` (a view) with `pub fn new(multi_workspace: WeakEntity<MultiWorkspace>, cx: &mut Context<Self>) -> Self`; observes the `MultiWorkspace` and each workspace so labels and dirty dots update.
  - `pub fn tab_label(workspace: &Workspace, cx: &App) -> SharedString` — `project_group_key(cx).display_name(&HashMap::default())`, or `"empty project"` when that is empty (the text the window title shows today).
  - `pub fn close_tab(multi_workspace: &mut MultiWorkspace, workspace: Entity<Workspace>, window: &mut Window, cx: &mut Context<MultiWorkspace>)` — if it is the only workspace, dispatch `workspace::CloseWindow`; otherwise activate the right neighbour (left if it was last) when closing the active tab, then `remove([workspace], RemovalIntent::CloseProject, …)` and `detach_and_log_err`.

Behaviour: one tab per entry of `workspaces()` in order; active = `workspace()`; nothing rendered with fewer than 2. Click → `activate(ws, None, window, cx)`. Close button and middle click → `close_tab`. Drag (`DraggedProjectTab { ix, title, width, … }`) → `move_workspace`. Right click → context menu: "Close Tab", "Close Other Tabs", "Move Tab to New Window" (`open_project_group_in_new_window(&key)` then `close_tab`). A dot (`Indicator::dot()`, as pane tabs use) after the label when `workspace.is_edited()`.

- [ ] **Step 1: Write failing GPUI tests** in `photon_tabs.rs` `#[cfg(test)]` (build a window with `MultiWorkspace::test_new` / `test_add_workspace` and `FakeFs`):
  - `tab_labels_follow_projects`: projects `/a/alpha` and `/b/beta` → labels `["alpha", "beta"]`.
  - `empty_projects_are_separate_tabs`: two empty workspaces → 2 tabs, both labelled `"empty project"`.
  - `strip_hidden_with_one_project`: one workspace → `cx.debug_bounds("photon-project-tab-0")` is `None`; after adding a second project it is `Some`.
  - `dirty_project_shows_dot`: edit a buffer in B without saving → B's tab renders the dot (`debug_bounds("photon-project-tab-dirty-1")` is `Some`); after save it is `None`.
  - `closing_active_tab_activates_neighbour`: tabs A, B, C with B active → close B → active C; close C → active A.
  - `close_tab_with_unsaved_changes_can_be_cancelled`: dirty buffer in B, close B, answer the save prompt with Cancel (`cx.simulate_prompt_answer("Cancel")`) → still 2 tabs and the buffer is still dirty.
  - `switching_tab_moves_focus`: click tab B → B's active item's focus handle `is_focused(window)`.
  - `background_tab_keeps_its_items`: open an item in A, switch to B and back → A's active item has the same `entity_id`.
  Give each tab element the debug selector `photon-project-tab-{ix}` and its dot `photon-project-tab-dirty-{ix}` so tests can find bounds and click them.
- [ ] **Step 2: Run** `cargo nextest run -p photon_tabs` — Expected: compile failure / FAIL (nothing implemented).
- [ ] **Step 3: Implement** `ProjectTabs`, `tab_label`, `close_tab` per the Interfaces and Global Constraints visuals.
- [ ] **Step 4: Run** `cargo nextest run -p photon_tabs` — Expected: all pass. Then `./script/clippy` clean.
- [ ] **Step 5: Commit** "Draw project tabs inside the window".

### Task 4: Tab actions and shortcuts

**Files:**
- Modify: `crates/photon_tabs/src/photon_tabs.rs`
- Modify: `crates/zed/src/zed/photon.rs` (call `photon_tabs::init`), `crates/zed/Cargo.toml`
- Modify: `assets/keymaps/default-macos.json`, `default-linux.json`, `default-windows.json` (bindings in the `Workspace` context, each block marked `// photon:`)

**Interfaces:**
- Consumes: Task 3 `close_tab`.
- Produces:
  - `actions!(photon_tabs, [NewTab, CloseTab])`; reuses upstream `ShowNextWindowTab`, `ShowPreviousWindowTab`, `MoveTabToNewWindow`, `MergeAllWindows` from `platform_title_bar`.
  - `pub fn init(new_tab: fn(&mut Workspace, &mut Window, &mut Context<Workspace>), cx: &mut App)` — registers the handlers on every `Workspace` (`cx.observe_new::<Workspace>` + `register_action`), resolving the window's `MultiWorkspace` with `window.root::<MultiWorkspace>()`. `new_tab` initializes a fresh empty workspace; `photon.rs` passes `ensure_center_terminal` so new tabs open a terminal like new windows do.
- Behaviour: Next/Previous wrap around. `NewTab` creates an empty local workspace the way `MultiWorkspace::remove` builds its replacement (`Project::local(...)` + `Workspace::new(None, …)`), activates it, then calls `new_tab`. `CloseTab` → `close_tab` on the active workspace. `MergeAllWindows`: for every other `MultiWorkspace` window, open each of its workspaces' paths here with `find_or_create_local_workspace(…, OpenMode::Add, …)` (empty ones become new empty tabs), then close that window with `CloseWindow` (save prompts as usual). `MoveTabToNewWindow`: as the context-menu item.
- Bindings (macOS; on Linux/Windows replace `cmd` with `ctrl-alt`): `ctrl-shift-]` ShowNextWindowTab, `ctrl-shift-[` ShowPreviousWindowTab, `cmd-ctrl-t` NewTab, `cmd-ctrl-w` CloseTab. All four were checked free in `default-macos.json`.

- [ ] **Step 1: Write failing tests** in `photon_tabs.rs`:
  - `next_and_previous_wrap`: tabs A, B, C active A; dispatch `ShowPreviousWindowTab` → C; `ShowNextWindowTab` twice → B.
  - `new_tab_adds_empty_project_and_runs_init`: dispatch `NewTab` → 3 workspaces from 2, the new one active and empty, and the `new_tab` callback ran once on it (test passes a callback that sets a flag).
  - `merge_all_windows_collects_projects`: window 1 with A, window 2 with B and C → dispatch `MergeAllWindows` in window 1 → one `MultiWorkspace` window whose tab labels are `["a", "b", "c"]`.
  - `move_tab_to_new_window_splits`: window with A, B, B active → `MoveTabToNewWindow` → two windows, labels `["a"]` and `["b"]`.
- [ ] **Step 2: Run** `cargo nextest run -p photon_tabs` — Expected: the new tests FAIL.
- [ ] **Step 3: Implement** `init` and the handlers; wire `photon_tabs::init(photon::ensure_center_terminal, cx)` from `photon.rs`'s init; add keymap bindings.
- [ ] **Step 4: Run** `cargo nextest run -p photon_tabs -p zed` — Expected: all pass (zed's keymap tests validate the new bindings parse).
- [ ] **Step 5: Commit** "Switch, open and close project tabs from the keyboard".

### Task 5: Show the strip in the title bar and turn native tabs off

**Files:**
- Modify: `crates/platform_title_bar/src/platform_title_bar.rs` (field + render ~line 356), `crates/platform_title_bar/Cargo.toml`
- Modify: `assets/settings/default.json:613` → `"use_system_window_tabs": false, // photon: projects are tabs inside one window (photon_tabs)`
- Modify: `crates/gpui_macos/src/window.rs` — restore the upstream `should_add_as_tab` expression (remove the `photon:` hook from `c6144d6a69`)
- Test: `crates/zed/src/zed.rs` tests

**Interfaces:**
- Consumes: `ProjectTabs::new` (Task 3).
- Hook: `PlatformTitleBar` gets `project_tabs: Option<Entity<ProjectTabs>>`, created lazily on first render from `window.root::<MultiWorkspace>()` (`// photon: in-window project tabs`), rendered as a child right after `system_window_tabs`. `SystemWindowTabs` already renders nothing when `use_system_window_tabs` is false.

- [ ] **Step 1: Write failing tests** in `crates/zed/src/zed.rs` tests:
  - `photon_open_second_project_shows_two_tabs`: open `/a`, then open `/b` via `workspace::open_paths` with default `OpenOptions` → exactly one window, `MultiWorkspace::workspaces().count() == 2`, and `debug_bounds("photon-project-tab-1")` is `Some`.
  - `photon_tabs_restore_in_order`: open `/a`, `/b`, `/c` in one window, `move_workspace(2, 0)`, activate `/a`, flush serialization (`flush_all_serialization`), drop and restore the window the way `test_restore_*` tests in this file do → tab labels `["c", "a", "b"]` and the active label `"a"`.
  - `photon_native_window_tabs_off_by_default`: `WorkspaceSettings::get_global(cx).use_system_window_tabs == false`.
- [ ] **Step 2: Run** `cargo nextest run -p zed photon_` — Expected: the three new tests FAIL. If `photon_tabs_restore_in_order` fails only on order after the hook is in, serialization does not keep `held` order: rule on it in the ledger (persist order through the existing serialized project-group list) rather than dropping the test.
- [ ] **Step 3: Implement** the hook, the default, and the `gpui_macos` revert.
- [ ] **Step 4: Run** `cargo nextest run -p zed -p workspace -p photon_tabs -p platform_title_bar` and `./script/clippy` — Expected: all pass, clippy clean; `git grep -n "photon:" crates/gpui_macos` returns nothing.
- [ ] **Step 5: Manual check on the release build** (`script/bundle-mac aarch64-apple-darwin`, copy app, user data dir in the scratchpad): open 3 projects; `vmmap --summary <pid> | grep -c "CAMetalLayer Display Drawable"`-style count of drawables is 3 (was 9); `footprint` within ~40 MB per extra project of the 1-project value; switching tabs shows no blank frame (screen recording, test window frontmost); `script/photon-perf --no-build --binary <app binary>` passes its budgets.
- [ ] **Step 6: Commit** "Show project tabs in the title bar and turn native window tabs off".
