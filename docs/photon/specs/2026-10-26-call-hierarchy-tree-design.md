# Call hierarchy tree panel

## Why

Zed's call hierarchy popup (⌘K ⌘H) shows one level at a time and closes when you click a call.
Following a chain of callers means expanding, going back, and reopening the popup. A panel that
stays open and shows several levels as a tree lets you walk the chain and jump to call sites
without losing your place.

## What it does

1. **Panel.** `photon_call_trails::CallHierarchyPanel`, in the right dock next to the outline and
   PR panels (300px, the outline panel's width). Status bar tooltip and title: "Call Hierarchy".
   It starts closed and isn't restored on relaunch.
2. **Opening it.**
   - **Keep Open** in the popup's footer opens the panel on the popup's current root and
     direction, and closes the popup. It's the popup's only Photon hook: a `call_hierarchy::KeepOpen`
     action, hidden from the command palette.
   - `photon_call_trails::ShowInPanel` ("Shows the call hierarchy of the symbol under the cursor
     in a panel.") roots the panel on the function under the cursor, through the same
     `prepare_call_hierarchy` request the popup uses.
3. **Header.** The root's label, highlighted like the popup's rows, and a **Callers | Calls**
   toggle. Switching rebuilds the tree from the root.
4. **Rows.** Indent per level, a disclosure chevron, the call's label, ×N when the caller calls it
   N times, and the muted file path. Children load on first expand through
   `call_hierarchy::fetch_calls`; a muted "Loading…" row shows while they load and "No callers" /
   "No calls" when there are none. Collapsing keeps fetched children, so expanding again doesn't
   ask the language server again.
5. **Recursion.** A function that already appears on its own ancestor path shows "↻ recursive"
   and has no chevron.
6. **Actions.** Click a row to open its call site in the active pane (the popup's confirm). Up and
   down select, right expands (or steps into the first child), left collapses (or steps to the
   parent), Enter opens. Right-click: **Make Root**, **Copy Name**.
7. **Nothing to show.** One muted line: "Put the cursor on a function to see its callers." or
   "This file's language server doesn't support call hierarchy." No toasts.

## Cost

No subscriptions or polling. Requests go out only for ShowInPanel, Keep Open, a direction switch,
Make Root, or a first expand. Closing the panel drops in-flight fetches; a node that was loading
folds back up, and a root that was loading fetches again when the panel reopens. A new root drops
the old tree's fetches.

## Upstream hooks (`// photon:`)

- `crates/call_hierarchy`: the `KeepOpen` action and footer button, `CallHierarchyView::root_and_mode`,
  `call_for_item` (labels a root like a call), and `render_item` made public.
- `crates/zed`: init, panel load in `initialize_panels`, the action namespace list, and hiding
  `KeepOpen` from the command palette.

## Tests

`crates/photon_call_trails` runs a fake language server that answers prepare, incoming and
outgoing requests for a four-function file: rooting via ShowInPanel, lazy expand with no refetch,
the Loading row, Calls direction, recursion, click-to-open, keyboard, Make Root and Copy Name via
the context menu, Keep Open clicked in the popup, dropped fetches on close, refetch on reopen, both
messages, and a guard on the popup's path format that ×N replaces.
