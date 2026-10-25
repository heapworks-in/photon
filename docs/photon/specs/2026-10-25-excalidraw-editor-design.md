# Excalidraw editor

## Why

The user spends much of their time on documentation and draws its diagrams in Excalidraw. Today
they use VS Code's Excalidraw extension, which is the full excalidraw.com editor inside the editor.
Photon should open `.excalidraw` files in a tab with the same editor, so diagrams are drawn where
the docs and code are.

## Goals

- Opening a `.excalidraw` file opens it in a Photon tab running **the real Excalidraw editor**
  (the `@excalidraw/excalidraw` component excalidraw.com uses), with all its drawing tools.
- **Files stay compatible both ways:** what Photon saves opens on excalidraw.com and in VS Code's
  extension, and theirs open in Photon. Photon never rewrites a file it didn't change.
- **No network, ever.** Excalidraw and **all its fonts** (Excalifont, Virgil, Nunito, Comic Shanns,
  Lilita One, Cascadia Code) ship inside Photon. The web view refuses any request that isn't for
  Photon's bundled files.
- **Behaves like a Photon file tab:** edits mark the tab dirty, ⌘S saves, closing a dirty tab asks
  first, the tab title is the file name, and a change on disk (e.g. `git pull`) reloads a clean tab.
- **Theme:** Excalidraw's light or dark mode follows Photon's current theme appearance.
- **Local shape libraries:** importing a `.excalidrawlib` file works, and imported libraries persist
  between sessions in Photon's data directory.

## Non-goals (v1)

- Online features: the library browser (libraries.excalidraw.com), share links, live
  collaboration, AI text-to-diagram. They are removed from the UI, not just blocked.
- `.excalidraw.svg` / `.excalidraw.png` files (the user saves plain `.excalidraw`).
- Showing diagrams inside Markdown preview.
- Linux and Windows. macOS only, like Photon's releases.
- Looking like Photon inside the tab: it looks like Excalidraw.

## Anti-goals

- Any network connection, including fonts, analytics, or error reporting from the web page.
- Losing a drawing: no save may write a partial or empty scene over a good file.
- Breaking Photon's own UI: menus, the command palette and other overlays must never be hidden
  behind the diagram.

## How (decided)

- **A Photon crate, not an extension.** Zed extensions can't show UI. New code goes in
  `crates/photon_excalidraw`, registered from `crates/zed/src/zed/photon.rs`.
- **Web view:** `wry` (Tauri's web-view crate, WKWebView on macOS), created as a child of the Photon
  window via its raw window handle. Bundled files are served through a custom URL scheme from
  memory; navigation to any other scheme or host is cancelled. The served page carries a strict
  Content-Security-Policy that allows only that scheme.
- **The bundle:** a small page wrapping `@excalidraw/excalidraw`, built once with npm into
  `assets/excalidraw/` (JS, CSS, fonts, licence) and committed, so building Photon needs no npm or
  network. `EXCALIDRAW_ASSET_PATH` points at the bundled fonts. A script rebuilds it when bumping
  Excalidraw's version.
- **Messages between Photon and the page** (JSON over wry's IPC): Photon sends the file's contents
  and the theme; the page sends the scene back on change (debounced), and Photon owns dirty state
  and saving. The file is only written on save, from the last scene the page reported, serialized
  with Excalidraw's own `serializeAsJSON` so the format matches excalidraw.com.

## Constraints and risks the builder must handle

- **Overlays:** the native web view draws above GPUI. Hide it (or snapshot it) whenever a GPUI
  overlay covers the tab (context menus, command palette, modals, popovers) and whenever the tab
  isn't visible; keep its frame in sync with the tab's bounds on every layout.
- **Keyboard:** while the diagram has focus, Photon's app shortcuts (⌘S, ⌘W, ⌘P, ⌘⇧P, tab switching)
  still work; Excalidraw's own shortcuts work for everything else.
- **Corrupt or unreadable file:** show an error in the tab, never open it as an empty scene that a
  save would write over it.
- **Must be tested** (project rule: every change is covered): file round-trip, dirty/save/reload,
  network refusal (extend `script/photon-netcheck` with a diagram open), theme message, and the
  overlay hiding logic.

## Calls made after review (the author's, not the user's; flagged for the user to check)

- **Shortcuts:** a fixed allowlist of Photon's app-level shortcuts works while a diagram has focus:
  ⌘S, ⌘⇧S, ⌘W, ⌘P, ⌘⇧P, ⌘O, ⌘N, ⌘T, ⌘, and tab/pane switching (⌘1–9, ⌃Tab, ⌘⇧[ and ⌘⇧]). Every other
  key, including ⌘Z, ⌘⇧Z, ⌘A, ⌘D and ⌘G, belongs to Excalidraw. This matches how VS Code's extension
  behaves.
- **Unsaved edits survive quitting and restarting**, like Photon's text files: the last reported
  scene is kept in Photon's database and the tab comes back dirty. Quitting never silently drops a
  drawing.
- **Fonts:** the six listed plus Liberation Sans and Assistant ship in the bundle. Xiaolai (the
  large CJK handwriting font) doesn't; CJK text falls back to system fonts and no request is made.
- **Memory budget:** the lean baseline (≤200 MB idle, ≤300 ms cold start) is measured with no
  diagram open and must not change. A diagram tab creates its web view when first shown and
  destroys it when the tab closes; one web view per tab.
- **Network check:** WKWebView's helper processes are started by launchd, not as Photon's children,
  so `script/photon-netcheck` must also watch the WebKit processes started for Photon.
- **Always a diagram:** `.excalidraw` files always open in the editor in v1; there's no "open as
  text" path.
- **An empty (0-byte) file is a new blank diagram**, so creating `x.excalidraw` from the project
  panel works. Any other file that isn't valid Excalidraw JSON shows the error.

## Left to the builder

- Exact message names and debounce interval.
- Whether imported libraries are stored as one file or one per library.
- How the bundle page is laid out, as long as the online features are absent.
