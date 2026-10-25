use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

pub const BRIDGE_JS: &str = include_str!("bridge.js");

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
}
