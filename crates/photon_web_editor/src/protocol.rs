use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

pub const BRIDGE_JS: &str = include_str!("bridge.js");

/// Injected into every frame: CSP `connect-src` does not cover WebRTC or DNS prefetching.
pub const NETWORK_GUARD_JS: &str = include_str!("network_guard.js");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToPage {
    Load {
        text: String,
        file_name: String,
        shortcuts: Vec<String>,
    },
    Theme {
        appearance: Appearance,
    },
    Flush {
        request: u64,
    },
    Library {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromPage {
    Ready { version: u32 },
    Changed { text: String },
    Flushed { request: u64 },
    Error { message: String },
    Shortcut { keystroke: String },
    LibraryChanged { text: String },
}

pub fn forwarded_shortcuts() -> Vec<String> {
    let fixed = [
        "cmd-s",
        "cmd-shift-s",
        "cmd-w",
        "cmd-p",
        "cmd-shift-p",
        "cmd-o",
        "cmd-n",
        "cmd-t",
        "cmd-,",
    ];
    let tabs = ["ctrl-tab", "ctrl-shift-tab", "cmd-shift-[", "cmd-shift-]"];
    fixed
        .into_iter()
        .map(String::from)
        .chain((1..=9).map(|number| format!("cmd-{number}")))
        .chain(tabs.into_iter().map(String::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_snake_case_json() {
        let to_page = [
            (
                ToPage::Load {
                    text: "{}".into(),
                    file_name: "a.excalidraw".into(),
                    shortcuts: vec!["cmd-s".into()],
                },
                r#"{"type":"load","text":"{}","file_name":"a.excalidraw","shortcuts":["cmd-s"]}"#,
            ),
            (
                ToPage::Theme {
                    appearance: Appearance::Dark,
                },
                r#"{"type":"theme","appearance":"dark"}"#,
            ),
            (
                ToPage::Theme {
                    appearance: Appearance::Light,
                },
                r#"{"type":"theme","appearance":"light"}"#,
            ),
            (
                ToPage::Flush { request: 7 },
                r#"{"type":"flush","request":7}"#,
            ),
            (
                ToPage::Library { text: "[]".into() },
                r#"{"type":"library","text":"[]"}"#,
            ),
        ];
        for (message, json) in to_page {
            assert_eq!(serde_json::to_string(&message).unwrap(), json);
            assert_eq!(serde_json::from_str::<ToPage>(json).unwrap(), message);
        }

        let from_page = [
            (
                FromPage::Ready { version: 1 },
                r#"{"type":"ready","version":1}"#,
            ),
            (
                FromPage::Changed { text: "{}".into() },
                r#"{"type":"changed","text":"{}"}"#,
            ),
            (
                FromPage::Flushed { request: 7 },
                r#"{"type":"flushed","request":7}"#,
            ),
            (
                FromPage::Error {
                    message: "boom".into(),
                },
                r#"{"type":"error","message":"boom"}"#,
            ),
            (
                FromPage::Shortcut {
                    keystroke: "cmd-shift-p".into(),
                },
                r#"{"type":"shortcut","keystroke":"cmd-shift-p"}"#,
            ),
            (
                FromPage::LibraryChanged { text: "[]".into() },
                r#"{"type":"library_changed","text":"[]"}"#,
            ),
        ];
        for (message, json) in from_page {
            assert_eq!(serde_json::to_string(&message).unwrap(), json);
            assert_eq!(serde_json::from_str::<FromPage>(json).unwrap(), message);
        }
    }

    #[test]
    fn forwarded_shortcuts_are_exactly_the_spec_list() {
        let expected = [
            "cmd-s",
            "cmd-shift-s",
            "cmd-w",
            "cmd-p",
            "cmd-shift-p",
            "cmd-o",
            "cmd-n",
            "cmd-t",
            "cmd-,",
            "cmd-1",
            "cmd-2",
            "cmd-3",
            "cmd-4",
            "cmd-5",
            "cmd-6",
            "cmd-7",
            "cmd-8",
            "cmd-9",
            "ctrl-tab",
            "ctrl-shift-tab",
            "cmd-shift-[",
            "cmd-shift-]",
        ];
        assert_eq!(forwarded_shortcuts(), expected);
    }

    #[test]
    fn bridge_script_names_the_protocol_version() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert!(BRIDGE_JS.contains("version: 1"));
    }

    const NODE_HARNESS: &str = r#"
const shortcuts = JSON.parse(process.env.SHORTCUTS);
const negatives = ["cmd-z", "cmd-a"];
let listener = null;
const posted = [];
const fakeWindow = {
  addEventListener: (type, callback) => { if (type === "keydown") listener = callback; },
  ipc: { postMessage: (text) => posted.push(JSON.parse(text)) },
};
new Function("window", process.env.BRIDGE)(fakeWindow);
fakeWindow.photon._receive(JSON.stringify({ type: "load", text: "", file_name: "a", shortcuts }));

const shifted = { "[": "{", "]": "}" };
const codes = { "[": "BracketLeft", "]": "BracketRight" };
const fire = (stroke) => {
  const parts = stroke.split("-");
  const base = parts.pop();
  const shift = parts.includes("shift");
  let key = base === "tab" ? "Tab" : base;
  if (shift && shifted[base]) key = shifted[base];
  else if (shift && /^[a-z]$/.test(base)) key = base.toUpperCase();
  const event = {
    metaKey: parts.includes("cmd"), ctrlKey: parts.includes("ctrl"),
    altKey: parts.includes("alt"), shiftKey: shift,
    key, code: codes[base] || "",
    defaultPrevented: false,
    preventDefault() { this.defaultPrevented = true; },
    stopPropagation() {},
  };
  posted.length = 0;
  listener(event);
  return { event, posted: [...posted] };
};

const failures = [];
for (const stroke of shortcuts) {
  const { event, posted: sent } = fire(stroke);
  const ok = event.defaultPrevented && sent.length === 1
    && sent[0].type === "shortcut" && sent[0].keystroke === stroke;
  if (!ok) failures.push(`${stroke} -> ${JSON.stringify(sent)}`);
}
for (const stroke of negatives) {
  const { event, posted: sent } = fire(stroke);
  if (event.defaultPrevented || sent.length) failures.push(`${stroke} was forwarded`);
}
if (failures.length) { console.error(failures.join("\n")); process.exit(1); }
"#;

    #[test]
    fn bridge_forwards_exactly_the_shortcut_list() {
        let output = smol::block_on(
            smol::process::Command::new("node")
                .args(["-e", NODE_HARNESS])
                .env("BRIDGE", BRIDGE_JS)
                .env(
                    "SHORTCUTS",
                    serde_json::to_string(&forwarded_shortcuts()).unwrap(),
                )
                .output(),
        )
        .expect("`node` must be on PATH to test bridge.js");
        assert!(
            output.status.success(),
            "bridge.js mismatch:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    const GUARD_HARNESS: &str = r#"
const peerNames = ["RTCPeerConnection", "webkitRTCPeerConnection", "RTCDataChannel", "RTCSessionDescription", "RTCIceCandidate"];
let observer = null;
const fakeWindow = {
  document: {},
  MutationObserver: class {
    constructor(callback) { observer = callback; }
    observe() {}
  },
};
for (const name of peerNames) fakeWindow[name] = function () {};
new Function("window", process.env.GUARD)(fakeWindow);

const failures = [];
for (const name of peerNames) {
  if (fakeWindow[name] !== undefined) failures.push(`${name} is still defined`);
}
const link = (rel) => ({
  nodeType: 1, tagName: "LINK", rel, removed: false,
  remove() { this.removed = true; },
  querySelectorAll() { return []; },
});
const hints = ["dns-prefetch", "preconnect", "prefetch", "prerender"].map(link);
const stylesheet = link("stylesheet");
const nested = link("dns-prefetch");
const container = { nodeType: 1, tagName: "DIV", querySelectorAll: () => [nested], remove() {} };
if (!observer) failures.push("no mutation observer");
else observer([{ addedNodes: [...hints, stylesheet, container] }]);
for (const hint of [...hints, nested]) if (!hint.removed) failures.push(`${hint.rel} link kept`);
if (stylesheet.removed) failures.push("stylesheet link removed");
if (failures.length) { console.error(failures.join("\n")); process.exit(1); }
"#;

    #[test]
    fn network_guard_removes_peer_connections_and_prefetch_links() {
        for name in [
            "RTCPeerConnection",
            "webkitRTCPeerConnection",
            "RTCDataChannel",
            "RTCSessionDescription",
            "RTCIceCandidate",
            "dns-prefetch",
        ] {
            assert!(NETWORK_GUARD_JS.contains(name), "{name}");
        }
        let output = smol::block_on(
            smol::process::Command::new("node")
                .args(["-e", GUARD_HARNESS])
                .env("GUARD", NETWORK_GUARD_JS)
                .output(),
        )
        .expect("`node` must be on PATH to test network_guard.js");
        assert!(
            output.status.success(),
            "network_guard.js mismatch:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
