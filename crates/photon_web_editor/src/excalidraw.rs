//! Tests on the committed Excalidraw bundle (`assets/web_editors/excalidraw`) and its adapter.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const FONTS: [&str; 8] = [
    "Assistant",
    "Cascadia",
    "ComicShanns",
    "Excalifont",
    "Liberation",
    "Lilita",
    "Nunito",
    "Virgil",
];

const ADAPTER: &str = include_str!("../../../extensions/photon-excalidraw/src/main.tsx");
const ADAPTER_CSS: &str = include_str!("../../../extensions/photon-excalidraw/src/photon.css");
const SESSION_JS: &str = include_str!("../../../extensions/photon-excalidraw/src/session.js");

fn bundle() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/web_editors/excalidraw")
}

fn files_under(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("reading {}: {error}", directory.display()));
        for entry in entries {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

fn size_of(files: &[PathBuf]) -> u64 {
    files
        .iter()
        .map(|file| std::fs::metadata(file).expect("file metadata").len())
        .sum()
}

#[derive(serde::Deserialize)]
struct BundledLibrary {
    name: String,
    authors: Vec<String>,
    file: String,
    license: String,
}

#[test]
fn bundled_libraries_are_the_popular_mit_ones() {
    let libraries_dir = bundle().join("libraries");
    let index: Vec<BundledLibrary> = serde_json::from_slice(
        &std::fs::read(libraries_dir.join("index.json")).expect("libraries/index.json exists"),
    )
    .expect("index.json parses");
    assert!(
        (25..=30).contains(&index.len()),
        "{} libraries bundled",
        index.len()
    );

    let third_party =
        std::fs::read_to_string(bundle().join("THIRD_PARTY.md")).expect("THIRD_PARTY.md exists");
    for library in &index {
        assert_eq!(library.license, "MIT", "{}", library.name);
        assert!(
            !library.authors.is_empty(),
            "{} has no authors",
            library.name
        );
        assert!(
            library.file.ends_with(".excalidrawlib") && !library.file.contains('/'),
            "{}",
            library.file
        );
        assert!(
            third_party.contains(&library.name),
            "{} not credited",
            library.name
        );

        let data: serde_json::Value = serde_json::from_slice(
            &std::fs::read(libraries_dir.join(&library.file))
                .unwrap_or_else(|error| panic!("{}: {error}", library.file)),
        )
        .unwrap_or_else(|error| panic!("{}: {error}", library.file));
        assert_eq!(data["type"], "excalidrawlib", "{}", library.file);
        let items = data
            .get("libraryItems")
            .or_else(|| data.get("library"))
            .and_then(|items| items.as_array())
            .unwrap_or_else(|| panic!("{} has no items", library.file));
        assert!(!items.is_empty(), "{} is empty", library.file);
    }
}

#[test]
fn bundled_libraries_are_under_three_megabytes() {
    let size = size_of(&files_under(&bundle().join("libraries")));
    assert!(size <= 3_000_000, "libraries are {size} bytes");
}

#[test]
fn excalidraw_bundle_is_under_ten_megabytes() {
    let libraries = bundle().join("libraries");
    let files: Vec<PathBuf> = files_under(&bundle())
        .into_iter()
        .filter(|file| !file.starts_with(&libraries))
        .collect();
    let size = size_of(&files);
    assert!(size < 10_000_000, "the bundle is {size} bytes");
}

#[test]
fn excalidraw_bundle_ships_the_allowed_fonts_only() {
    let fonts = bundle().join("fonts");
    let families: BTreeSet<String> = std::fs::read_dir(&fonts)
        .expect("fonts directory exists")
        .map(|entry| {
            entry
                .expect("font entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        families,
        FONTS.iter().map(|family| family.to_string()).collect()
    );
    for family in FONTS {
        assert!(
            files_under(&fonts.join(family)).iter().any(|file| file
                .extension()
                .is_some_and(|extension| extension == "woff2")),
            "{family} has no woff2 file"
        );
    }
    for file in files_under(&bundle()) {
        assert!(
            !file.to_string_lossy().contains("Xiaolai"),
            "{} is shipped",
            file.display()
        );
    }
}

// Each host appears in Excalidraw's or Mermaid's own code. None is reachable: CSP allows only
// `photon-web:`, Photon refuses every other request and navigation, and the features that use
// them are removed from the UI (checked below). A new host after an upgrade fails this test so it
// gets looked at.
const KNOWN_HOSTS: &[&str] = &[
    // XML namespaces, identifiers rather than requests.
    "http://www.w3.org",
    // Links in help text, error messages, the main menu's socials and source comments.
    "https://chevrotain.io",
    "https://github.com",
    "https://gist.github.com",
    "https://mermaid.js.org",
    "https://plus.excalidraw.com",
    "https://app.excalidraw.com",
    "https://docs.excalidraw.com",
    "http://docs.excalidraw.com",
    "https://discord.gg",
    "https://x.com",
    "https://twitter.com",
    "https://reactjs.org",
    "https://rolldown.rs",
    "https://langium.org",
    "https://en.wikipedia.org",
    // The library browser and publishing, hidden by photon.css.
    "https://libraries.excalidraw.com",
    "https://us-central1-excalidraw-room-persistence.cloudfunctions.net",
    // Share links, collaboration and AI, which need host callbacks the adapter doesn't pass.
    "https://json.excalidraw.com",
    "https://oss-collab.excalidraw.com",
    "https://oss-ai.excalidraw.com",
    "https://excalidraw-room-persistence.firebaseio.com",
    // Web embeds, all refused by validateEmbeddable.
    "https://www.youtube.com",
    "https://youtube.com",
    "https://player.vimeo.com",
    "https://www.figma.com",
    "https://platform.twitter.com",
    "https://reddit.com",
    "https://embed.reddit.com",
    "https://giphy.com",
];

#[test]
fn excalidraw_bundle_makes_no_remote_references() {
    let mut unknown = BTreeSet::new();
    for file in files_under(&bundle()) {
        let extension = file
            .extension()
            .map(|extension| extension.to_string_lossy().into_owned());
        let Some(extension @ ("js" | "css" | "html")) = extension.as_deref() else {
            continue;
        };
        let text = std::fs::read_to_string(&file).expect("text file");
        for (start, _) in text.match_indices("http") {
            let rest = &text[start..];
            let Some(scheme_length) = ["https://", "http://"]
                .iter()
                .find(|scheme| rest.starts_with(**scheme))
                .map(|scheme| scheme.len())
            else {
                continue;
            };
            let host_length = rest[scheme_length..]
                .find(|character: char| {
                    !(character.is_ascii_alphanumeric() || character == '.' || character == '-')
                })
                .unwrap_or(rest.len() - scheme_length);
            let origin = &rest[..scheme_length + host_length];
            // A bare scheme is a prefix test in code, not a reference.
            if host_length == 0 {
                continue;
            }
            // Pages and styles Photon loads must not point anywhere but photon-web:.
            if extension != "js" && origin != "http://www.w3.org" {
                unknown.insert(format!("{origin} in {}", file.display()));
            } else if !KNOWN_HOSTS.contains(&origin) {
                unknown.insert(format!("{origin} in {}", file.display()));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "unexpected remote references:\n{unknown:#?}"
    );

    let index = std::fs::read_to_string(bundle().join("editor/index.html")).expect("entry page");
    assert!(index.contains(r#"window.EXCALIDRAW_ASSET_PATH = "photon-web://excalidraw/""#));
    for flag in [
        "loadScene: false",
        "saveToActiveFile: false",
        "export: false",
        "toggleTheme: false",
        "aiEnabled={false}",
        "isCollaborating={false}",
        "validateEmbeddable={() => false}",
        "onLinkOpen={(_element, event) => event.preventDefault()}",
    ] {
        assert!(ADAPTER.contains(flag), "the adapter lacks {flag}");
    }
    assert!(
        !ADAPTER.contains("onExportToBackend"),
        "export to link needs onExportToBackend"
    );
    assert!(!ADAPTER.contains("LiveCollaborationTrigger"));
    assert!(!ADAPTER.contains("Socials"));
    assert!(
        ADAPTER.contains(r#"case "notice":"#)
            && ADAPTER.contains("showNotice(api, message.message)"),
        "the adapter does not show Photon's notices"
    );
    assert!(ADAPTER_CSS.contains(".library-menu-browse-button"));
    let shipped_css: String = files_under(&bundle().join("editor"))
        .iter()
        .filter(|file| file.extension().is_some_and(|extension| extension == "css"))
        .map(|file| std::fs::read_to_string(file).expect("css"))
        .collect();
    assert!(
        shipped_css.contains(".photon-error") && shipped_css.contains("lib-dropdown--remove"),
        "photon.css is not in the bundle"
    );
}

#[test]
fn exported_svgs_never_point_fonts_at_a_remote_url() {
    let shipped_js: String = files_under(&bundle().join("editor"))
        .iter()
        .filter(|file| file.extension().is_some_and(|extension| extension == "js"))
        .map(|file| std::fs::read_to_string(file).expect("js"))
        .collect();
    // Excalidraw's last-resort font source, which SVG exports used when subsetting failed.
    assert!(
        !shipped_js.contains("esm.sh"),
        "the CDN font fallback ships"
    );
    assert!(
        !shipped_js.contains("this.urls[this.urls.length-1]"),
        "a font that can't be embedded still falls back to its URL"
    );
    // A font with no embeddable content is left out of the SVG rather than written as url().
    let guarded = shipped_js
        .match_indices("`@font-face {")
        .all(|(start, _)| shipped_js[..start].ends_with("&&"));
    assert!(
        guarded && shipped_js.contains("`@font-face {"),
        "an @font-face is written without content"
    );
    // Subsetting runs harfbuzz as WebAssembly.
    assert!(crate::CSP.contains("'wasm-unsafe-eval'"));
}

#[test]
fn bundled_npm_packages_and_fonts_ship_their_licences() {
    let licences = std::fs::read_to_string(bundle().join("THIRD_PARTY_LICENSES.md"))
        .expect("THIRD_PARTY_LICENSES.md exists");
    for package in [
        "## react - ",
        "## react-dom - ",
        "## mermaid - ",
        "## @excalidraw/excalidraw - ",
    ] {
        assert!(licences.contains(package), "{package} is not listed");
    }
    let third_party =
        std::fs::read_to_string(bundle().join("THIRD_PARTY.md")).expect("THIRD_PARTY.md exists");
    assert!(third_party.contains("THIRD_PARTY_LICENSES.md"));
    for family in FONTS {
        let licence =
            std::fs::read_to_string(bundle().join("fonts").join(family).join("LICENSE.txt"))
                .unwrap_or_else(|error| panic!("{family} has no LICENSE.txt: {error}"));
        assert!(
            licence.contains("SIL Open Font License") || licence.starts_with("MIT License"),
            "{family}'s licence is neither OFL nor MIT"
        );
    }
}

#[test]
fn excalidraw_ships_its_licence() {
    let licence = std::fs::read_to_string(bundle().join("LICENSE")).expect("LICENSE exists");
    assert!(licence.starts_with("MIT License"));
    assert!(licence.contains("Excalidraw"));
}

const SESSION_HARNESS: &str = r##"
const session = await import("data:text/javascript," + encodeURIComponent(process.env.SESSION));
const sent = [];
// A fake clock: `advance` runs the timers that fall due, in order.
let now = 0;
let nextTimer = 0;
const timers = new Map();
const fakeTimers = {
  setTimeout: (callback, ms) => { timers.set(nextTimer, { callback, due: now + ms }); return nextTimer++; },
  clearTimeout: (id) => { timers.delete(id); },
};
const advance = (ms) => {
  const end = now + ms;
  for (;;) {
    let next = null;
    for (const [id, timer] of timers) if (timer.due <= end && (!next || timer.due < next[1].due)) next = [id, timer];
    if (!next) break;
    timers.delete(next[0]);
    now = next[1].due;
    next[1].callback();
  }
  now = end;
};
const runTimers = () => advance(1e9);
// Stand-ins for Excalidraw: a scene version is the sum of element versions.
const sceneVersion = (elements) => elements.reduce((sum, element) => sum + element.version, 0);
const restore = (data) => {
  if (data && data.elements && data.elements.some((element) => element.broken)) throw new Error("bad element");
  return { elements: data ? data.elements ?? [] : [], appState: { viewBackgroundColor: data?.appState?.viewBackgroundColor ?? "#ffffff", gridModeEnabled: false, gridSize: 20, gridStep: 5 }, files: {} };
};
const serialize = (elements, appState) => JSON.stringify({ type: "excalidraw", elements, appState });
const make = () => session.createSession({ restore, serialize, sceneVersion, send: (m) => sent.push(m), timers: fakeTimers });
const failures = [];
const expect = (label, condition) => { if (!condition) failures.push(label + ": " + JSON.stringify(sent)); };

const file = JSON.stringify({ type: "excalidraw", version: 2, elements: [{ id: "a", version: 3 }], appState: { viewBackgroundColor: "#ffffff" } });

let s = make();
let scene = s.load(file);
s.change(scene.elements, scene.appState, {});
runTimers();
expect("load sends nothing", sent.length === 0);

s.change([{ id: "a", version: 4 }], scene.appState, {});
expect("the first change is sent at once", sent.length === 1 && sent[0].type === "changed" && JSON.parse(sent[0].text).elements[0].version === 4);
runTimers();
expect("a sent change is not sent again", sent.length === 1);
s.change([{ id: "a", version: 4 }], scene.appState, {});
runTimers();
expect("an unchanged scene is not resent", sent.length === 1);

s.change(scene.elements, { viewBackgroundColor: "#000000" }, {});
runTimers();
expect("a background change is sent", sent.length === 2 && sent[1].type === "changed");

const background = { ...scene.appState, viewBackgroundColor: "#000000" };
for (const [field, value] of [["gridModeEnabled", true], ["gridSize", 40], ["gridStep", 10]]) {
  const before = sent.length;
  s.change(scene.elements, { ...background, [field]: value }, {});
  runTimers();
  expect(field + " change is sent", sent.length === before + 1 && sent[before].type === "changed");
  s.change(scene.elements, background, {});
  runTimers();
}

sent.length = 0;
s.change([{ id: "a", version: 9 }], scene.appState, {});
s.change([{ id: "a", version: 10 }], scene.appState, {});
s.flush(7);
expect("flush sends the pending change then flushed", sent.length === 3 && sent[1].type === "changed" && JSON.parse(sent[1].text).elements[0].version === 10 && sent[2].type === "flushed" && sent[2].request === 7);
runTimers();
expect("a flushed change is not sent twice", sent.length === 3);

// A burst: the first change at once, the rest as one more send once the edits pause.
sent.length = 0;
let version = 20;
s.change([{ id: "a", version: version++ }], scene.appState, {});
expect("a burst's first change is sent at once", sent.length === 1);
for (let step = 0; step < 6; step++) {
  advance(50);
  s.change([{ id: "a", version: version++ }], scene.appState, {});
}
expect("the rest of a burst waits for a pause", sent.length === 1);
advance(299);
expect("the pause is the debounce", sent.length === 1);
advance(1);
expect("a burst sends at most one more", sent.length === 2 && JSON.parse(sent[1].text).elements[0].version === version - 1);
runTimers();
expect("nothing more after a burst", sent.length === 2);

// Continuous edits: none waits longer than the max wait to be sent.
sent.length = 0;
const sentAt = [];
const changedAt = [];
const continuous = session.createSession({ restore, serialize, sceneVersion, send: (m) => { sent.push(m); sentAt.push(now); }, timers: fakeTimers });
scene = continuous.load(file);
for (let step = 0; step < 50; step++) {
  changedAt.push(now);
  continuous.change([{ id: "a", version: 100 + step }], scene.appState, {});
  advance(100);
}
runTimers();
const late = changedAt.filter((at) => !sentAt.some((sentTime) => sentTime >= at && sentTime <= at + 1000));
expect("continuous changes are sent within the max wait (late: " + late.join(",") + ")", late.length === 0);
expect("continuous changes are still debounced", sent.length <= 8);

sent.length = 0;
s = make();
scene = s.load("");
expect("an empty file is an empty scene", scene && scene.elements.length === 0);
s.change([], scene.appState, {});
runTimers();
expect("an empty file is not dirtied", sent.length === 0);

for (const text of ["{not json", JSON.stringify({ type: "something-else" }), "[]", JSON.stringify({ type: "excalidraw", elements: [{ broken: true }] })]) {
  sent.length = 0;
  s = make();
  expect("invalid file returns no scene", s.load(text) === null);
  expect("invalid file sends error", sent.length === 1 && sent[0].type === "error" && sent[0].message.length > 0);
  s.change([{ id: "a", version: 1 }], {}, {});
  runTimers();
  expect("nothing is saved after an error", sent.length === 1);
}

const toasts = [];
session.showNotice({ setToast: (toast) => toasts.push(toast) }, "Saved a.png to Downloads");
session.showNotice(null, "before Excalidraw is ready");
expect("a notice is shown as an Excalidraw toast (" + JSON.stringify(toasts) + ")", toasts.length === 1 && toasts[0].message === "Saved a.png to Downloads" && toasts[0].closable === true && toasts[0].duration === 3000);

expect("bundled items are recognised", session.isBundledLibraryItem({ id: session.BUNDLED_ID_PREFIX + "x" }) && !session.isBundledLibraryItem({ id: "x" }));

if (failures.length) { console.error(failures.join("\n")); process.exit(1); }
"##;

#[test]
fn excalidraw_adapter_sends_changes_at_once_then_debounced() {
    let output = smol::block_on(
        smol::process::Command::new("node")
            .args(["--input-type=module", "-e", SESSION_HARNESS])
            .env("SESSION", SESSION_JS)
            .output(),
    )
    .expect("`node` must be on PATH to test the Excalidraw adapter");
    assert!(
        output.status.success(),
        "session.js mismatch:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
