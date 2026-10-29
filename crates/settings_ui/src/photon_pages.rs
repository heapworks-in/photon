//! Photon's own settings in the Settings screen: command blocks on the Terminal page, and a
//! Pull Requests page.

use std::sync::Arc;

use gpui::Action as _;

use crate::{ActionLink, PROJECT, SettingField, SettingItem, SettingsPage, SettingsPageItem, USER};

pub(crate) fn command_blocks_section() -> [SettingsPageItem; 5] {
    [
        SettingsPageItem::SectionHeader("Layout and Blocks"),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Layout",
            description: "Where terminals open: in the center beside your files, or in the bottom panel like VS Code.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("terminal.layout"),
                pick: |settings_content| settings_content.terminal.as_ref()?.layout.as_ref(),
                write: |settings_content, value, _| {
                    settings_content.terminal.get_or_insert_default().layout = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Blocks",
            description: "Whether new zsh terminals show each command and its output as a block.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("terminal.blocks"),
                pick: |settings_content| settings_content.terminal.as_ref()?.blocks.as_ref(),
                write: |settings_content, value, _| {
                    settings_content.terminal.get_or_insert_default().blocks = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Smart Blocks",
            description: "Whether finished blocks recognise test runs, build errors, git output, servers, JSON and tables.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("terminal.smart_blocks"),
                pick: |settings_content| settings_content.terminal.as_ref()?.smart_blocks.as_ref(),
                write: |settings_content, value, _| {
                    settings_content
                        .terminal
                        .get_or_insert_default()
                        .smart_blocks = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Notify After Seconds",
            description: "How long a command must run before its finish notifies you while Photon is in the background. 0 turns notifications off.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("terminal.notify_after_seconds"),
                pick: |settings_content| {
                    settings_content
                        .terminal
                        .as_ref()?
                        .notify_after_seconds
                        .as_ref()
                },
                write: |settings_content, value, _| {
                    settings_content
                        .terminal
                        .get_or_insert_default()
                        .notify_after_seconds = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
    ]
}

pub(crate) fn pull_requests_page() -> SettingsPage {
    SettingsPage {
        title: "Pull Requests",
        items: Box::new([
            SettingsPageItem::SectionHeader("Reviewing"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Test Command",
                description: "The command typed (never run) in the terminal that Run Locally opens for a pull request.",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("pr_review.test_command"),
                    pick: |settings_content| {
                        settings_content.project.pr_review.as_ref()?.test_command.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .pr_review
                            .get_or_insert_default()
                            .test_command = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Agent Pre-Review Thoroughness",
                description: "What the agent pre-review reports: must-fix problems only, also should-fix ones, or everything.",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("pr_review.agent_review.thoroughness"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .pr_review
                            .as_ref()?
                            .agent_review
                            .as_ref()?
                            .thoroughness
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .pr_review
                            .get_or_insert_default()
                            .agent_review
                            .get_or_insert_default()
                            .thoroughness = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::ActionLink(ActionLink {
                title: "Lists, Checklists and Automatic Checks".into(),
                description: Some(
                    "The Pull Requests panel's lists, your review checklists and the checks Photon answers itself are lists, set in the settings file under pr_review.".into(),
                ),
                button_text: "Open Settings File".into(),
                on_click: Arc::new(|settings_window, window, cx| {
                    let Some(original_window) = settings_window.original_window else {
                        return;
                    };
                    original_window
                        .update(cx, |_workspace, original_window, cx| {
                            original_window
                                .dispatch_action(zed_actions::OpenSettingsFile.boxed_clone(), cx);
                            original_window.activate_window();
                        })
                        .ok();
                    window.remove_window();
                }),
                files: USER,
            }),
        ]),
    }
}

#[cfg(test)]
mod tests {
    #[gpui::test]
    fn photon_settings_have_a_place_in_the_settings_screen(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::test::register_settings(cx);
            workspace::AppState::set_global(workspace::AppState::test(cx), cx);
            let pages = crate::page_data::settings_data(cx);
            let titles = |page: &str| {
                pages
                    .iter()
                    .find(|candidate| candidate.title == page)
                    .unwrap_or_else(|| panic!("{page} page"))
                    .items
                    .iter()
                    .map(|item| format!("{item:?}"))
                    .collect::<Vec<_>>()
            };
            let terminal = titles("Terminal");
            for expected in [
                "SectionHeader(Layout and Blocks)",
                "SettingItem(Layout)",
                "SettingItem(Blocks)",
                "SettingItem(Smart Blocks)",
                "SettingItem(Notify After Seconds)",
            ] {
                assert!(terminal.iter().any(|item| item == expected), "{expected}");
            }
            let pull_requests = titles("Pull Requests");
            assert!(pull_requests.contains(&"SettingItem(Test Command)".to_string()));
            assert!(
                pull_requests.contains(&"SettingItem(Agent Pre-Review Thoroughness)".to_string())
            );
        });
    }
}
