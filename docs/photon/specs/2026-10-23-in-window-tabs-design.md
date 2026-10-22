# In-window project tabs

## Why

Photon opens every project as a native macOS window tab (`use_system_window_tabs: true`
plus the `photon:` tabbing hook in `crates/gpui_macos/src/window.rs`). Each native tab
is its own NSWindow with its own Metal layer and about three full-size drawables:
~67 MB per tab on a retina 1512x949pt window. Three tabs measured 222 MB of drawables
alone. Hiding a tab does not give the memory back (tried and reverted: the compositor
keeps the presented buffers, and clearing them causes a blank flash).

One window holding several projects pays for one set of drawables no matter how many
tabs are open. Zed already has the model for this: `MultiWorkspace`
(`crates/workspace/src/multi_workspace.rs`) holds several `Workspace`s in one window,
renders only the active one (`MultiWorkspace::render` uses `self.workspace()`), and
serializes them per window. Photon adds the tab strip and routes "open a project"
into it.

## Goals

- Every project opened in Photon becomes a tab in the current window, shown in the
  title row like today.
- Memory: a window with N projects keeps one window's drawables (3 IOSurface
  'CAMetalLayer Display Drawable' regions, not 3×N). Measured with `vmmap --summary`
  on the release build: 3 projects open → 3 drawables.
- No UX regression against today's native tabs: the same look, the same shortcuts,
  switching is instant with no blank frame, terminals in background tabs keep running
  and keep their scrollback, closing the last tab behaves like closing the window today.
- Session restore brings back each window with its tabs in order and the active tab
  selected.

## Non-goals

- Changing Zed's agent sidebar. It already lists the same project groups; both stay in
  sync because they read the same `MultiWorkspace`.
- Remote (SSH/WSL) projects as tabs beyond what `MultiWorkspace` already supports.
- Linux/Windows. They don't use native window tabs; the strip will work there because
  it is GPUI, but no extra work goes in.

## Anti-goals

- Forking `MultiWorkspace` or the title bar. Photon code lives in a Photon crate; Zed
  files get only marked `// photon:` hooks.
- A blank or flashing frame on tab switch or on launch.
- Losing a running terminal process when switching tabs.

## Design

### Model

A tab is a `Workspace` held by the window's `MultiWorkspace`. Tab order is
`MultiWorkspace::workspaces()` order; the active tab is `MultiWorkspace::workspace()`.
Switching calls `MultiWorkspace::activate`. Closing calls `MultiWorkspace::remove`.
No new state is stored: order and the active tab come from `MultiWorkspace`, and its
existing serialization (`serialize` / `restore_project_groups`) persists them.

`MultiWorkspace::multi_workspace_enabled` is false when AI is disabled, and then
`activate` drops the old workspace instead of keeping it. Photon needs tabs regardless,
so a `// photon:` hook makes workspaces always retained. The sidebar toggle keeps its
AI gate.

### Tab strip

- Lives in a new `crates/photon_tabs` crate. It is rendered by `platform_title_bar` in
  place of `SystemWindowTabs`, through one `// photon:` hook.
- Looks exactly like today's `SystemWindowTabs` strip (same height, padding, close
  button, active/inactive colors, drag to reorder). Shared UI comes from `ui`
  components; if a piece of `SystemWindowTabs` styling is needed it is reused, not
  copied.
- Shown when the window holds two or more projects, like macOS tabs today.
- Tab label: the project name as the title bar shows it today; a dot when the project
  has unsaved edits (the same signal the window's edited indicator uses).
- Right-click menu: Close Tab, Close Other Tabs, Move Tab to New Window (uses the
  existing `MultiWorkspace::open_project_group_in_new_window`).

### Opening projects

- Open Recent, File > Open, the CLI (`photon <path>`) and dropping a folder on the dock
  icon open into the frontmost window as a new tab (`MultiWorkspace::open_project`
  already does this and reuses an empty workspace instead of adding a tab).
- If the project is already open in a tab, that tab is activated instead.
- New Window (cmd-shift-n) still opens a separate window.
- New Tab: opens an empty workspace (terminal-first, as new windows do today) as a
  tab in the current window.

### Shortcuts

The actions that native tabs use today (`ShowNextWindowTab`, `ShowPreviousWindowTab`,
`MoveTabToNewWindow`, `MergeAllWindows`, and New Tab) keep their current bindings and
are handled by the in-window strip. `MergeAllWindows` moves every window's projects
into the current window as tabs.

### Native tabs off

`use_system_window_tabs` defaults to false in `assets/settings/default.json`, and the
`photon:` tabbing hook in `crates/gpui_macos/src/window.rs` is removed so the file is
back to upstream.

### Upgrade

Windows saved by the previous build (one native tab = one window) restore as separate
windows, as they were saved. Merge All Windows joins them into one; after that it stays
one window.

## Testing

- GPUI tests in `photon_tabs`: opening a second project adds a tab and activates it;
  opening an already open project activates its tab; closing the active tab activates
  its neighbour; the strip hides with one project; workspaces are retained with AI
  disabled; Merge All Windows leaves one window with every project; Move Tab to New
  Window leaves one project in each window.
- Restore: serialize a window with three tabs, restore, same order and active tab.
- Background terminal: a terminal in a background tab keeps its process and output
  across switches.
- Manual on the release build: footprint and `vmmap --summary` drawables with 1 and 3
  projects, against the numbers above; no blank frame when switching (screen recording).
- `script/photon-perf` stays within its budgets.

## Left to the builder

- How the strip's drag-to-reorder maps onto `MultiWorkspace`'s order (reordering
  `held`, or reusing `move_project_group_up/down`).
- Which upstream type the tab label reads (`project_group_key`, or the workspace's
  title).
- The exact place in `platform_title_bar` where the hook goes.
