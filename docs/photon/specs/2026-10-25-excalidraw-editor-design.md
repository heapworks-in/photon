# Web editors, and Excalidraw as the first one

## Why

The user spends much of their time on documentation and draws its diagrams in Excalidraw. Today
they use VS Code's Excalidraw extension, which is the full excalidraw.com editor inside the editor.
Photon should open `.excalidraw` files in a tab with that same editor.

Zed's extensions can't show UI, so Photon adds that ability to its extension system. An extension
can declare a **web editor**: a bundled, offline web app that Photon shows in a tab for certain
file types. Excalidraw is the first web editor and ships inside Photon as a built-in extension.
Others (draw.io, a Mermaid editor, a CSV grid) can follow without changing Photon.

## Goals

**The web-editor capability (Photon infrastructure):**

- An extension declares web editors in `extension.toml`:
  ```toml
  [web_editors.excalidraw]
  name = "Excalidraw"
  file_suffixes = ["excalidraw"]
  entry = "editor/index.html"
  ```
- Opening a matching file shows a tab whose content is the extension's page in a web view.
- **Photon owns the file.** It reads it, writes it on save, tracks unsaved changes, keeps unsaved
  work across quit and restart, asks before closing a dirty tab, and reloads a clean tab when the
  file changes on disk. The page never touches the file system.
- **One small message protocol** between Photon and every page:
  - Photon → page: `load` (file text, file name), `theme` (light or dark).
  - Page → Photon: `ready`, `changed` (new file text), `error` (message to show).
  - The page's script gets these through a tiny `photon` object Photon injects; the protocol is
    versioned so pages can check it.
- **No network, for any web editor.** Pages are served from the extension's own folder through a
  custom URL scheme; any other request or navigation is refused, and the page gets a strict
  Content-Security-Policy. A page can't read other extensions' files or anything outside its folder.
- Photon's shortcuts, theme and overlays keep working (see Constraints).
- Installable like any extension: built in, or from a folder with **zed: install dev extension**.

**Excalidraw (the first web editor, a built-in extension):**

- Opening a `.excalidraw` file opens the real Excalidraw editor (`@excalidraw/excalidraw`, the
  component excalidraw.com uses) with all its drawing tools.
- **Files stay compatible both ways** with excalidraw.com and VS Code's extension. Saving uses
  Excalidraw's own `serializeAsJSON`. A file Photon didn't change is never rewritten.
- **Fonts ship with it:** Excalifont, Virgil, Nunito, Comic Shanns, Lilita One, Cascadia Code,
  Liberation Sans and Assistant. Xiaolai (the large CJK handwriting font) doesn't; CJK text falls
  back to system fonts with no request made.
- Theme follows Photon's light or dark appearance.
- Imported `.excalidrawlib` shape libraries persist between sessions (stored by Photon on the
  page's behalf; the protocol grows a `library` message for this).
- An empty (0-byte) file opens as a blank diagram. Any other file that isn't valid Excalidraw JSON
  shows an error in the tab instead of a blank scene a save would write over it.

## Efficiency (ground rule: Photon is never resource hungry)

- **Nothing at idle or startup.** With no web-editor tab open, Photon starts no WebKit process and
  loads no web-editor code; the lean baseline (≤200 MB idle, ≤300 ms cold start,
  `tooling/photon-perf`) must not move.
- **One WebKit content process for all web-editor tabs** (a shared process pool), not one per tab.
- **Created when first shown, dropped when not needed.** A tab's web view is created the first time
  it's visible. A tab hidden for 60 s drops its web view (Photon already holds the latest content,
  so it's recreated on return with nothing lost). Closing the last web-editor tab ends the content
  process.
- **No polling.** Changes arrive as `changed` messages, debounced in the page (≈300 ms); Photon only
  writes on save.
- **Bundle size:** Excalidraw's built extension stays under 10 MB on disk, fonts included.
- **Measured:** the perf tooling gains a run with one diagram open, reporting memory of Photon plus
  its WebKit processes and idle CPU (must be ~0 % when nothing changes).

## Non-goals (v1)

- Online features of Excalidraw: the library browser, share links, live collaboration and AI
  text-to-diagram. They're removed from its UI, not just blocked.
- `.excalidraw.svg` / `.excalidraw.png` files; diagrams in Markdown preview.
- Publishing Photon extensions to Zed's extension store (they wouldn't run in Zed).
- Linux and Windows; macOS only, like Photon's releases.
- Web editors running code with file-system or network access.

## Anti-goals

- Any network connection from a web editor, including fonts, analytics or error reporting.
- Losing a drawing: no save may write a partial or empty scene over a good file.
- Photon's own UI hidden behind a web view.
- A cost paid by people who never open a web editor.

## How (decided)

- **Infrastructure:** a new crate `crates/photon_web_editor` (the item/tab, the web view host, the
  protocol, the scheme handler), registered from `crates/zed/src/zed/photon.rs`. Upstream hooks,
  each marked `photon:`: one `web_editors` field on `ExtensionManifest` (Zed ignores unknown
  fields, so Zed manifests are unaffected), and one call where `extension_host` loads and unloads
  extensions, so web editors register and unregister with them.
- **Web view:** `wry` (WKWebView on macOS), created as a child of the Photon window through its raw
  window handle, using a shared process pool.
- **Excalidraw extension:** `extensions/photon-excalidraw/` in the repo: `extension.toml`, a small
  adapter page and the built Excalidraw bundle with fonts and licence, built once with npm and
  committed, so building Photon needs no npm or network. A script rebuilds it when bumping
  Excalidraw. Photon installs it as a built-in extension from its assets.

## Constraints and risks the builder must handle

- **Overlays:** the native web view draws above GPUI. Hide it whenever a GPUI overlay covers the
  tab (context menus, command palette, modals, popovers) or the tab isn't visible; keep its frame in
  sync with the tab's bounds on every layout.
- **Shortcuts:** while a web editor has focus, these Photon shortcuts still work: ⌘S, ⌘⇧S, ⌘W, ⌘P,
  ⌘⇧P, ⌘O, ⌘N, ⌘T, ⌘, and tab or pane switching (⌘1–9, ⌃Tab, ⌘⇧[ and ⌘⇧]). Every other key,
  including ⌘Z, ⌘⇧Z, ⌘A, ⌘D and ⌘G, belongs to the page.
- **Network check:** WKWebView's helper processes are started by launchd, not as Photon's children,
  so `script/photon-netcheck` must also watch the WebKit processes started for Photon, with a
  diagram open.
- **Must be tested** (every change is covered): manifest parsing; registering and unregistering
  with extensions; the protocol; dirty, save, reload and restore; refusing non-local requests and
  out-of-folder paths; the overlay-hiding logic; Excalidraw file round-trip; empty and corrupt
  files; the efficiency rules (no web view before first show, dropped after hiding, one process).

## Left to the builder

- Exact Rust types and message field names beyond those listed.
- How Excalidraw's adapter page is laid out, as long as the online features are absent.
- How libraries are stored on disk.
