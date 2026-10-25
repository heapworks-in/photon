# Web editors and Excalidraw Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extensions can declare offline web editors for file types; Excalidraw ships as the first, built in.

**Architecture:** A new crate `photon_web_editor` opens matching files as a `WebEditorView` tab that wraps the file's ordinary text `Buffer` (so dirty state, save, reload-on-disk-change come from Photon's buffer machinery) and shows the editor page in a `wry` child web view. The page and Photon talk through a small JSON protocol; pages are served from the extension's folder (or Photon's embedded assets for built-ins) through a custom scheme, with everything else refused. The native view is shown only while the tab has focus; otherwise a snapshot image stands in, so GPUI overlays always draw on top.

**Tech Stack:** Rust, GPUI, `wry` 0.53 (WKWebView, macOS), objc2 (already in the workspace), npm + Vite only for building the committed Excalidraw bundle.

**Spec:** `docs/photon/specs/2026-10-25-excalidraw-editor-design.md`

## Global Constraints

- Fork rules (CLAUDE.md): new code in `crates/photon_web_editor`; upstream edits are minimal hooks marked `// photon:` (`# photon:` in TOML); `./script/clippy`; specs/plans under `docs/photon/`.
- Product rules: looks like Zed around the tab; no user-visible "Zed"; `cargo test -p photon_brand` stays green.
- Efficiency: nothing at startup or idle; no WebKit process until a web-editor tab is first shown; web view dropped after the tab is hidden 60 s; changes debounced ≈300 ms in the page; no polling; Excalidraw bundle < 10 MB on disk; idle CPU ~0 % with a diagram open.
- No network from any web editor: custom scheme only, other navigations/requests refused, CSP `default-src photon-web: 'unsafe-inline'; img-src photon-web: data: blob:; font-src photon-web: data:; connect-src 'none'`.
- Shortcuts forwarded from a focused web editor: `cmd-s`, `cmd-shift-s`, `cmd-w`, `cmd-p`, `cmd-shift-p`, `cmd-o`, `cmd-n`, `cmd-t`, `cmd-,`, `cmd-1`…`cmd-9`, `ctrl-tab`, `ctrl-shift-tab`, `cmd-shift-[`, `cmd-shift-]`. Nothing else.
- Excalidraw fonts shipped: Excalifont, Virgil, Nunito, Comic Shanns, Lilita One, Cascadia Code, Liberation Sans, Assistant. Not Xiaolai.
- macOS only: the native host is `#[cfg(target_os = "macos")]`; elsewhere web-editor files open as text.
- Every task's code is covered by tests (project rule).

## Review Focus

1. A file changed on disk while its diagram tab has unsaved edits: Photon's buffer conflict handling must apply (no silent overwrite either way). Test in Task 4.
2. Saving immediately after typing, before the page's 300 ms debounce fires: the save must include the latest drawing. The page flushes pending changes when Photon sends `flush`, and save waits for the reply. Test in Task 4.
3. A path request like `photon-web://excalidraw/../../etc/passwd` or a symlink inside an extension pointing outside it: refused. Test in Task 2.
4. Two tabs of the same `.excalidraw` file (split pane): both reflect edits from either, since they share the buffer. Test in Task 4.
5. A web editor extension uninstalled while its tab is open: the tab shows "This editor is no longer installed" and the buffer stays (dirty edits not lost). Test in Task 6.

---

### Task 1: `web_editors` in the extension manifest

**Files:**
- Modify: `crates/extension/src/extension_manifest.rs` (add field + entry type, one `// photon:` marker)

**Interfaces:**
- Produces: `ExtensionManifest::web_editors: BTreeMap<Arc<str>, WebEditorManifestEntry>`; `pub struct WebEditorManifestEntry { pub name: String, pub file_suffixes: Vec<String>, pub entry: RelPathBuf }` (Serialize, Deserialize, Clone, Debug, PartialEq, Eq; field `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]`).

- [ ] **Step 1: Write the failing test** `web_editors_parse_from_extension_toml` in the manifest's test module: parse a TOML with the spec's `[web_editors.excalidraw]` block and assert `name == "Excalidraw"`, `file_suffixes == ["excalidraw"]`, `entry` is `editor/index.html`; parse a manifest without the block and assert `web_editors.is_empty()`.
- [ ] **Step 2: Run** `cargo test -p extension web_editors_parse` — Expected: FAIL (no field).
- [ ] **Step 3: Implement** the field and type.
- [ ] **Step 4: Run** `cargo test -p extension` — Expected: all pass (existing manifests unaffected).
- [ ] **Step 5: Commit** `Extension manifest: web editors`.

### Task 2: The `photon_web_editor` crate: registry and file serving

**Files:**
- Create: `crates/photon_web_editor/Cargo.toml` (`[lib] path = "src/photon_web_editor.rs"`), `src/photon_web_editor.rs`, `src/registry.rs`, `src/serve.rs`
- Modify: root `Cargo.toml` workspace members and `[workspace.dependencies]` (`photon_web_editor`, `wry = { version = "0.53", default-features = false }` with a `# photon:` marker)

**Interfaces:**
- Produces:
  - `pub enum WebEditorSource { Embedded { prefix: String }, Directory(PathBuf) }`
  - `pub struct WebEditor { pub id: Arc<str>, pub extension_id: Arc<str>, pub name: String, pub file_suffixes: Vec<String>, pub entry: String, pub source: WebEditorSource, pub built_in: bool }`
  - `pub struct WebEditorRegistry` (gpui `Global`): `register(editor: WebEditor)`, `unregister_extension(extension_id: &str)`, `editor_for_path(path: &Path) -> Option<&WebEditor>`, `editor(id: &str) -> Option<&WebEditor>`.
  - `pub fn serve(editor: &WebEditor, request_path: &str, assets: &dyn AssetSource) -> Result<Served, ServeError>`; `pub struct Served { pub bytes: Cow<'static, [u8]>, pub mime: &'static str }`; `pub const CSP: &str` (the Global Constraints value); `pub const SCHEME: &str = "photon-web"`.
- Matching rules: suffix match is on the file name, case-insensitive, after a dot (`a.excalidraw` matches `excalidraw`; `a.excalidraw.svg` and `excalidraw` do not); the longest matching suffix wins; an installed (non-built-in) editor wins over a built-in one for the same suffix.

- [ ] **Step 1: Write failing tests** in `registry.rs`: `suffixes_match_whole_dotted_names` (cases above), `installed_editor_overrides_built_in`, `unregistering_an_extension_removes_its_editors`. In `serve.rs`: `serves_files_inside_the_editor_folder` (Directory source with a temp dir: `index.html` → `text/html`, `fonts/x.woff2` → `font/woff2`, `app.js` → `text/javascript`); `refuses_paths_outside_the_folder` (`../secret`, `/etc/passwd`, `%2e%2e/secret`, a symlink inside the temp dir pointing outside → `ServeError::OutsideEditor`); `serves_embedded_built_ins` (Embedded source with a fake `AssetSource`).
- [ ] **Step 2: Run** `cargo test -p photon_web_editor` — Expected: FAIL (types missing).
- [ ] **Step 3: Implement.** Directory paths are canonicalized and must stay under the canonical folder; unknown extensions get `application/octet-stream`.
- [ ] **Step 4: Run** the same — Expected: PASS.
- [ ] **Step 5: Commit** `Web editors: registry and serving`.

### Task 3: The page protocol and bridge script

**Files:**
- Create: `crates/photon_web_editor/src/protocol.rs`, `crates/photon_web_editor/src/bridge.js`

**Interfaces:**
- Produces:
  - `pub const PROTOCOL_VERSION: u32 = 1;`
  - `#[serde(tag = "type", rename_all = "snake_case")] pub enum ToPage { Load { text: String, file_name: String, shortcuts: Vec<String> }, Theme { appearance: Appearance }, Flush { request: u64 }, Library { text: String } }` with `pub enum Appearance { Light, Dark }` (snake_case).
  - `#[serde(tag = "type", rename_all = "snake_case")] pub enum FromPage { Ready { version: u32 }, Changed { text: String }, Flushed { request: u64 }, Error { message: String }, Shortcut { keystroke: String }, LibraryChanged { text: String } }`
  - `pub fn forwarded_shortcuts() -> Vec<String>` returning exactly the Global Constraints list.
  - `pub const BRIDGE_JS: &str = include_str!("bridge.js");` — defines `window.photon = { version, onMessage(cb), send(msg) }` over wry IPC, and a capturing `keydown` listener that, for a keystroke in the last `Load.shortcuts`, calls `preventDefault()` and sends `Shortcut`. Keystroke strings use Photon's keystroke syntax (`cmd-shift-p`).
- [ ] **Step 1: Write failing tests** `messages_round_trip_as_snake_case_json` (each variant serializes to the exact JSON, e.g. `{"type":"changed","text":"{}"}`, and parses back), `forwarded_shortcuts_are_exactly_the_spec_list`, `bridge_script_names_the_protocol_version` (BRIDGE_JS contains `version: 1`).
- [ ] **Step 2: Run** `cargo test -p photon_web_editor protocol` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `Web editors: page protocol`.

### Task 4: The document and the tab

**Files:**
- Create: `crates/photon_web_editor/src/document.rs`, `crates/photon_web_editor/src/view.rs`

**Interfaces:**
- Consumes: Task 2 registry, Task 3 protocol.
- Produces:
  - `pub struct WebDocument { buffer: Entity<Buffer>, editor_id: Arc<str> }` implementing `project::ProjectItem`: `try_open` returns `Some` only when `WebEditorRegistry::editor_for_path` matches, and opens the buffer with `project.open_buffer`; `entry_id`, `project_path`, `is_dirty` delegate to the buffer.
  - `pub trait PageConnection { fn send(&self, message: ToPage); }` — the view talks to the page only through this, so tests use a fake.
  - `pub struct WebEditorView` implementing `Item` (tab title = file name, `is_dirty`, `can_save`, `save`, `save_as`, `reload` via the buffer), `ProjectItem` (`type Item = WebDocument`), `SerializableItem` (keeps unsaved text across restart in a `web_editor_unsaved` key-value entry keyed by absolute path), and `pub fn handle_page_message(&mut self, message: FromPage, window, cx)`.
  - Behaviour: on `Ready`, send `Load` with the buffer text and `Theme`; on `Changed { text }`, replace the buffer text only if different; on buffer reload from disk, send `Load`; on theme change, send `Theme`; `save` first sends `Flush { request }` and saves after the matching `Flushed` (500 ms timeout, then saves what it has); `Shortcut` dispatches the keystroke to the workspace.
- [ ] **Step 1: Write failing GPUI tests** in `view.rs` using `FakePage` (records sent messages, lets the test inject `FromPage`):
  - `ready_loads_the_file_and_theme` — after `Ready`, `FakePage` saw `Load { text: <file>, file_name: "a.excalidraw", shortcuts: forwarded_shortcuts() }` then `Theme`.
  - `changes_make_the_tab_dirty_and_save_writes_them` — `Changed` → `is_dirty`; save → the fake fs file equals the changed text; not dirty.
  - `unchanged_scene_is_never_written` — `Changed` with identical text → not dirty, no write on save.
  - `save_flushes_the_page_first` — save sends `Flush`; replying `Flushed` after a `Changed` writes the newer text (Review Focus 2).
  - `disk_change_reloads_a_clean_tab` and `disk_change_on_a_dirty_tab_keeps_edits` (Review Focus 1: buffer conflict state, no overwrite).
  - `two_tabs_share_one_document` (Review Focus 4).
  - `unsaved_edits_survive_a_restart` — serialize, drop, deserialize → dirty with the same text.
  - `empty_file_loads_as_empty_text` — `Load { text: "" }` (the page shows a blank diagram).
  - `forwarded_shortcut_dispatches_the_action` — `Shortcut { keystroke: "cmd-s" }` saves.
  - `page_error_is_shown_and_nothing_is_written` — `Error { message }` shows the message in the tab; a save leaves the file byte-for-byte unchanged.
  - `libraries_persist_between_sessions` — `LibraryChanged { text }` writes `paths::data_dir()/web_editors/<editor id>/library.json`; a new view's `Ready` is followed by `Library { text }` with that content.
- [ ] **Step 2: Run** `cargo test -p photon_web_editor view` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `Web editors: document and tab`.

### Task 5: The native web view host

**Files:**
- Create: `crates/photon_web_editor/src/lifecycle.rs`, `crates/photon_web_editor/src/native.rs` (`#[cfg(target_os = "macos")]`)

**Interfaces:**
- Consumes: Task 2 `serve`/`CSP`/`SCHEME`, Task 3 `BRIDGE_JS`/`FromPage`, Task 4 `PageConnection`/`handle_page_message`.
- Produces:
  - `pub struct Lifecycle` (pure, no WebKit): `fn on_frame(&mut self, visible: bool, focused: bool, now: Instant) -> Action`; `enum Action { None, Create, ShowNative, ShowSnapshot, Drop }`. Rules: create on the first frame where `visible`; native view only while `visible && focused`, otherwise snapshot (taken on the transition); `Drop` once not visible for 60 s; a later visible frame creates again.
  - `pub struct NativePage` implementing `PageConnection`: owns the `wry::WebView` built with `build_as_child` on the window's raw handle, the shared `wry::WebContext`, a custom protocol `photon-web` backed by `serve` (adds the `Content-Security-Policy` header), a navigation handler allowing only `photon-web:`, IPC → `FromPage` → the view, `set_bounds` every frame, `set_visible`, and snapshot via WKWebView `takeSnapshot` into a GPUI image.
  - One `WebContext` per app (a `Global`), created with the first page and dropped with the last.
- [ ] **Step 1: Write failing tests** in `lifecycle.rs`: `nothing_is_created_until_first_shown`, `native_view_only_while_focused`, `unfocusing_switches_to_a_snapshot_once`, `hidden_for_sixty_seconds_drops_the_view`, `shown_again_after_drop_recreates_it`, `hidden_less_than_sixty_seconds_keeps_it` (fake `Instant`s).
- [ ] **Step 2: Run** `cargo test -p photon_web_editor lifecycle` — Expected: FAIL.
- [ ] **Step 3: Implement** `Lifecycle`, then `NativePage` and wire `WebEditorView`'s render: paint the snapshot (or nothing) in GPUI; call `Lifecycle::on_frame` from the element's prepaint with the element bounds and `focus_handle.contains_focused`; clicking the snapshot focuses the view and makes the web view first responder.
- [ ] **Step 4: Run** the lifecycle tests — Expected: PASS. Then the manual check: `cargo run -p zed` with a sample `.excalidraw`, open the command palette over the diagram (drawn on top), switch tabs, wait 60 s, return (diagram intact). Record the result in the commit message.
- [ ] **Step 5: Commit** `Web editors: native web view host`.

### Task 6: Editors follow extensions; app wiring

**Files:**
- Create: `crates/photon_web_editor/src/extensions.rs`
- Modify: `crates/zed/src/zed/photon.rs` (call `photon_web_editor::init(cx)`), `crates/zed/src/main.rs` or `zed.rs` init list (one `// photon:` call), `crates/zed/Cargo.toml`

**Interfaces:**
- Consumes: Task 1 manifest field, Task 2 registry, Task 4 view.
- Produces: `pub fn init(cx: &mut App)` — registers `WebEditorView` as a project item and serializable item, registers built-ins (Task 7's Excalidraw as `WebEditorSource::Embedded { prefix: "web_editors/excalidraw/" }`, `built_in: true`), and subscribes to `ExtensionStore` `Event::ExtensionsUpdated` to re-sync installed extensions' `web_editors` (`WebEditorSource::Directory(paths::extensions_dir().join("installed").join(id))`). No work besides this registration at startup.
- [ ] **Step 1: Write failing tests:** `installed_extension_registers_its_editors` and `uninstalled_extension_unregisters_them` (fake `ExtensionStore` index); `open_tab_survives_its_editor_being_uninstalled` (Review Focus 5: tab shows "This editor is no longer installed", buffer and dirty text kept); `opening_an_excalidraw_file_opens_a_web_editor_tab` (GPUI test: workspace opens `/project/a.excalidraw` → active item is `WebEditorView`; `/project/a.json` → `Editor`).
- [ ] **Step 2: Run** `cargo test -p photon_web_editor extensions` and `cargo test -p zed photon_web_editor` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS; `cargo test -p zed` all pass.
- [ ] **Step 5: Commit** `Web editors follow installed extensions`.

### Task 7: The Excalidraw extension

**Files:**
- Create: `extensions/photon-excalidraw/extension.toml`, `package.json` (pins `@excalidraw/excalidraw`, `react`, `react-dom` exact versions), `src/main.tsx` (adapter), `vite.config.ts`, `script/build-photon-excalidraw` (runs npm ci + vite build, copies the allowed fonts, writes `assets/web_editors/excalidraw/`), `assets/web_editors/excalidraw/**` (built output, committed, with Excalidraw's MIT licence)
- Modify: `crates/assets/src/assets.rs` include list (`"web_editors/**/*", // photon: built-in web editors`)

**Interfaces:**
- Consumes: Task 3 protocol (via `window.photon`).
- Adapter behaviour: `window.EXCALIDRAW_ASSET_PATH = "photon-web://excalidraw/"`; on `Load`, parse with Excalidraw's `restore` (empty text → empty scene; invalid JSON → `photon.send({type:"error", message})` and render nothing); on change, debounce 300 ms and send `Changed` with `serializeAsJSON(elements, appState, files, "local")` only when `getSceneVersion` changed; on `Flush`, send any pending change then `Flushed`; on `Theme`, set `theme`; `UIOptions` hide the library browser link, collaboration, share/export-to-link and AI actions; no `onLinkOpen` navigation.
- Bundled libraries: `script/build-photon-excalidraw` reads `stats.json` and `libraries.json` from `excalidraw/excalidraw-libraries` at a pinned commit (recorded in the script), takes the 30 most-downloaded libraries whose licence is MIT, and writes them to `assets/web_editors/excalidraw/libraries/` with an `index.json` (`[{ "name", "authors", "file" }]`). The adapter loads them on `Load` into Excalidraw's library as items grouped by library name, alongside the user's own library from `Library`.
- [ ] **Step 1: Write failing tests** in `crates/photon_web_editor` (they read the committed bundle): `bundled_libraries_are_the_popular_mit_ones` (index lists 25–30 libraries, every file exists and parses as `.excalidrawlib`, all MIT), `bundled_libraries_are_under_three_megabytes`, `excalidraw_bundle_is_under_ten_megabytes` (excluding `libraries/`), `excalidraw_bundle_ships_the_allowed_fonts_only` (each listed family present, no `Xiaolai`), `excalidraw_bundle_makes_no_remote_references` (no `https://` or `http://` in shipped `.js`/`.css`/`.html` except inside licence text and Excalidraw's own `libraries.excalidraw.com` string, which must be unreachable because the UI hides it — assert the adapter sets the UIOptions flags), `excalidraw_extension_manifest_declares_the_editor`.
- [ ] **Step 2: Run** `cargo test -p photon_web_editor excalidraw` — Expected: FAIL.
- [ ] **Step 3: Implement** the adapter and run `script/build-photon-excalidraw`.
- [ ] **Step 4: Run** — Expected: PASS. Manual round-trip: open a file saved on excalidraw.com, edit, save, open the result on excalidraw.com; record in the commit message.
- [ ] **Step 5: Commit** `Excalidraw as a built-in web editor`.

### Task 8: Network silence and resource checks

**Files:**
- Modify: `script/photon-netcheck`, `tooling/photon-perf` (its run script), `.github/workflows/photon.yml` (add `-p photon_web_editor` to clippy and tests, and `package(photon_web_editor)` to the filter)

**Interfaces:**
- Consumes: everything above.
- [ ] **Step 1:** netcheck gains `--with-diagram`: opens `tooling/photon-perf/sample/diagram.excalidraw` and also watches `com.apple.WebKit.*` processes whose parent responsible process is the Photon PID (via `launchctl procinfo`/`ps -o ppid` + responsible-pid lookup). It must fail when given a page that fetches `https://example.com` (proved with a test page, like the existing `-- CMD` proof).
- [ ] **Step 2:** perf gains a "diagram open" run reporting Photon + WebKit RSS and idle CPU over 10 s; idle CPU must be < 1 %.
- [ ] **Step 3: Run** `script/photon-netcheck --with-diagram` — Expected: pass; the proof page — Expected: fail. Run perf — Expected: baseline unchanged, diagram run reported.
- [ ] **Step 4: Commit** `Netcheck and perf cover web editors`.
