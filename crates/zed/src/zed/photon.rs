use command_palette_hooks::CommandPaletteFilter;
use editor::Editor;
use gpui::{App, Context, Focusable as _, Window};
use terminal_view::TerminalView;
use ui::{IconButton, IconName, IconSize, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ItemHandle, NewCenterTerminal, NewTerminal, StatusItemView, Workspace,
};

/// Runs the PR review's agent pre-review as a visible thread in the agent panel, with the user's
/// selected agent, and returns the agent's final reply.
pub struct PanelReviewAgent;

pub fn register_review_agent(cx: &mut App) {
    photon_review::set_review_agent(std::rc::Rc::new(PanelReviewAgent), cx);
}

impl photon_review::ReviewAgent for PanelReviewAgent {
    fn review(
        &self,
        workspace: gpui::WeakEntity<Workspace>,
        prompt: String,
        work_dir: std::path::PathBuf,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::Task<anyhow::Result<String>> {
        use agent_client_protocol::schema::v2 as acp_v2;
        use agent_ui::{AgentInitialContent, AgentPanel, AgentThreadSource, CreateThreadOptions};

        let Some(workspace) = workspace.upgrade() else {
            return gpui::Task::ready(Err(anyhow::anyhow!("The review's window is closed.")));
        };
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return gpui::Task::ready(Err(anyhow::anyhow!(
                "This window has no agent panel. Open it once, then try again."
            )));
        };
        let blocks = vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(prompt))];
        let thread_id = panel.update(cx, |panel, cx| {
            let thread_id = panel.create_thread_with_options(
                CreateThreadOptions {
                    title: Some("Agent pre-review".into()),
                    initial_content: Some(AgentInitialContent::ContentBlock {
                        blocks,
                        auto_submit: true,
                    }),
                    work_dirs: Some(util::path_list::PathList::new(&[work_dir])),
                    ..CreateThreadOptions::default()
                },
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
            panel.activate_retained_thread(thread_id, false, window, cx);
            thread_id
        });
        workspace.update(cx, |workspace, cx| {
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
        let panel = panel.downgrade();
        cx.spawn(async move |cx| wait_for_agent_reply(panel, thread_id, cx).await)
    }
}

/// Call Trails flags a root the open PR review changes, and explains a trail through the agent
/// panel, with the user's selected agent.
pub fn register_call_trail_hooks(cx: &mut App) {
    photon_call_trails::set_hooks(
        photon_call_trails::CallTrailHooks {
            changed_in_review: Some(std::rc::Rc::new(photon_review::pr_changing_rows)),
            explain: Some(std::rc::Rc::new(explain_call_trail)),
        },
        cx,
    );
}

fn explain_call_trail(
    workspace: &mut Workspace,
    prompt: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_agent_thread(
        workspace,
        "Explain call hierarchy",
        prompt,
        true,
        window,
        cx,
    );
}

/// Plan review sends its verdict, and questions about a task, to a new agent thread.
pub fn register_plan_review_hooks(cx: &mut App) {
    photon_plan_review::set_hooks(
        photon_plan_review::PlanReviewHooks {
            send_to_agent: Some(std::rc::Rc::new(|workspace, message, window, cx| {
                open_agent_thread(
                    workspace,
                    &message.title,
                    message.prompt,
                    message.send,
                    window,
                    cx,
                )
            })),
        },
        cx,
    );
}

/// Opens a new thread in the agent panel with `prompt`, sent when `auto_submit`, otherwise left
/// in the message box for the user to finish.
fn open_agent_thread(
    workspace: &mut Workspace,
    title: &str,
    prompt: String,
    auto_submit: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    use agent_client_protocol::schema::v2 as acp_v2;
    use agent_ui::{AgentInitialContent, AgentPanel, AgentThreadSource, CreateThreadOptions};
    use workspace::{Toast, notifications::NotificationId};

    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<AgentPanel>(),
                "This window has no agent panel. Open it once, then try again.",
            ),
            cx,
        );
        return;
    };
    let blocks = vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(prompt))];
    panel.update(cx, |panel, cx| {
        let thread_id = panel.create_thread_with_options(
            CreateThreadOptions {
                title: Some(title.to_string().into()),
                initial_content: Some(AgentInitialContent::ContentBlock {
                    blocks,
                    auto_submit,
                }),
                ..CreateThreadOptions::default()
            },
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        panel.activate_retained_thread(thread_id, false, window, cx);
    });
    workspace.focus_panel::<AgentPanel>(window, cx);
}

enum AgentProgress {
    Connecting,
    Working,
    StoppedWithoutReply,
    Replied(String),
}

/// Waits until the thread's agent has answered the prompt and stopped, then returns its last
/// message. The thread stays in the panel, so the user can follow and continue it.
async fn wait_for_agent_reply(
    panel: gpui::WeakEntity<agent_ui::AgentPanel>,
    thread_id: agent_ui::ThreadId,
    cx: &mut gpui::AsyncApp,
) -> anyhow::Result<String> {
    use acp_thread::{AgentThreadEntry, ThreadStatus};
    use anyhow::Context as _;
    use std::time::{Duration, Instant};

    const CONNECT_TIMEOUT: Duration = Duration::from_secs(2 * 60);
    const TIMEOUT: Duration = Duration::from_secs(30 * 60);
    // Idle polls in a row, after the prompt, with no reply: the turn ended in an error or was
    // cancelled. More than one, since the thread is briefly idle between appending the prompt
    // and starting to generate.
    const IDLE_POLLS: usize = 3;
    let started = Instant::now();
    let mut idle_polls = 0;
    loop {
        cx.background_executor().timer(Duration::from_secs(1)).await;
        let progress = panel.update(cx, |panel, cx| -> anyhow::Result<AgentProgress> {
            let view = panel
                .conversation_view_for_id(&thread_id, cx)
                .context("The agent thread was closed.")?;
            let Some(thread) = view.read(cx).root_thread(cx) else {
                return Ok(AgentProgress::Connecting);
            };
            let thread = thread.read(cx);
            if thread.status() != ThreadStatus::Idle {
                return Ok(AgentProgress::Working);
            }
            let entries = thread.entries();
            let Some(prompt) = entries
                .iter()
                .rposition(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
            else {
                return Ok(AgentProgress::Working);
            };
            Ok(entries
                .get(prompt + 1..)
                .unwrap_or_default()
                .iter()
                .rev()
                .find_map(|entry| match entry {
                    AgentThreadEntry::AssistantMessage(message) => {
                        Some(AgentProgress::Replied(message.to_markdown(cx)))
                    }
                    _ => None,
                })
                .unwrap_or(AgentProgress::StoppedWithoutReply))
        })??;
        match progress {
            AgentProgress::Replied(reply) => return Ok(reply),
            AgentProgress::Connecting if started.elapsed() > CONNECT_TIMEOUT => {
                anyhow::bail!("The agent didn't start. See the agent panel for why.");
            }
            AgentProgress::StoppedWithoutReply => {
                idle_polls += 1;
                if idle_polls >= IDLE_POLLS {
                    anyhow::bail!(
                        "The agent stopped without answering. See the agent panel for why."
                    );
                }
            }
            AgentProgress::Connecting | AgentProgress::Working => idle_polls = 0,
        }
        if started.elapsed() > TIMEOUT {
            anyhow::bail!("The agent didn't finish within 30 minutes.");
        }
    }
}

pub const REPO_URL: &str = "https://github.com/heapworks-in/photon";
pub const DOCS_URL: &str = "https://heapworks-in.github.io/photon/";

/// Release notes for this build, or every release for a local build.
pub fn release_notes_url() -> String {
    match release_channel::PHOTON_VERSION {
        "dev" => format!("{REPO_URL}/releases"),
        version => format!("{REPO_URL}/releases/tag/photon-v{version}"),
    }
}

/// Hides actions for services Photon keeps switched off (Zed account, collaboration, auto-update)
/// and for the bottom terminal panel, which Photon doesn't use.
pub fn hide_offline_actions(cx: &mut App) {
    CommandPaletteFilter::update_global(cx, |filter, _| {
        for namespace in [
            "client",
            "collab",
            "collab_panel",
            "channel_modal",
            "auto_update",
            // Terminals always open in the center, so the bottom terminal panel stays out of the way.
            "terminal_panel",
        ] {
            filter.hide_namespace(namespace);
        }
        filter.hide_action_types(&[
            std::any::TypeId::of::<zed_actions::OpenAccountSettings>(),
            // "New Terminal" already opens in the center.
            std::any::TypeId::of::<NewCenterTerminal>(),
            // Zed's own channels; Photon has no email, status page, shop or telemetry.
            std::any::TypeId::of::<zed_actions::feedback::EmailZed>(),
            std::any::TypeId::of::<zed_actions::OpenTelemetryLog>(),
            std::any::TypeId::of::<super::OpenStatusPage>(),
            std::any::TypeId::of::<super::GetMerch>(),
            // The built-in agent is off; agents run over ACP.
            std::any::TypeId::of::<agent_ui::NewNativeAgentThreadFromSummary>(),
            // Only the call hierarchy popup's Keep Open button can use it.
            std::any::TypeId::of::<call_hierarchy::KeepOpen>(),
        ]);
    });
}

/// Photon is terminal-first: a workspace with nothing open starts with a terminal in the center.
pub fn ensure_center_terminal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if should_open_center_terminal(workspace, cx) {
        TerminalView::deploy(workspace, &center_terminal_action(), window, cx);
    }
}

/// [`ensure_center_terminal`] for a window opened asynchronously (e.g. a remote/SSH project).
pub fn ensure_center_terminal_in(
    window: gpui::WindowHandle<workspace::MultiWorkspace>,
    cx: &mut gpui::AsyncApp,
) {
    window
        .update(cx, |multi_workspace, window, cx| {
            multi_workspace.workspace().update(cx, |workspace, cx| {
                ensure_center_terminal(workspace, window, cx)
            })
        })
        .log_err();
}

fn should_open_center_terminal(workspace: &Workspace, cx: &App) -> bool {
    workspace.items(cx).next().is_none()
}

fn center_terminal_action() -> NewCenterTerminal {
    NewCenterTerminal { local: false }
}

gpui::actions!(
    photon,
    [
        /// Moves focus between the most recently used center terminal and editor, opening a terminal if none is open.
        ToggleTerminalEditorFocus,
        /// Font style: Mono for code and the terminal, Sans for menus (the default).
        UseMonoFontStyle,
        /// Font style: Hack everywhere, menus included.
        UseHackFontStyle,
        /// Font style: Menlo for code and the terminal, the macOS system font for menus (Terminal.app's look).
        UseMenloFontStyle,
        /// Font style: Monaco for code and the terminal, the macOS system font for menus (classic Mac).
        UseMonacoFontStyle,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq)]
enum FontStyle {
    Mono,
    Hack,
    Menlo,
    Monaco,
}

impl FontStyle {
    /// The code and terminal font, then the menu font.
    fn families(self) -> (&'static str, &'static str) {
        match self {
            FontStyle::Mono => ("Lilex", "IBM Plex Sans"),
            FontStyle::Hack => ("Hack", "Hack"),
            FontStyle::Menlo => ("Menlo", ".SystemUIFont"),
            FontStyle::Monaco => ("Monaco", ".SystemUIFont"),
        }
    }

    /// Clears any terminal-only font, so the terminal keeps following the code font.
    fn apply(self, settings: &mut settings::SettingsContent) {
        let (code, ui) = self.families();
        settings.theme.buffer_font_family = Some(settings::FontFamilyName(code.into()));
        settings.theme.ui_font_family = Some(settings::FontFamilyName(ui.into()));
        if let Some(terminal) = settings.terminal.as_mut() {
            terminal.font_family = None;
        }
    }
}

/// Registers the font style commands, which write the chosen fonts to the user's settings.
pub fn register_font_styles(cx: &mut App) {
    fn set(style: FontStyle, cx: &mut App) {
        settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |settings, _| {
            style.apply(settings)
        });
    }
    cx.on_action(|_: &UseMonoFontStyle, cx| set(FontStyle::Mono, cx));
    cx.on_action(|_: &UseHackFontStyle, cx| set(FontStyle::Hack, cx));
    cx.on_action(|_: &UseMenloFontStyle, cx| set(FontStyle::Menlo, cx));
    cx.on_action(|_: &UseMonacoFontStyle, cx| set(FontStyle::Monaco, cx));
}

pub fn toggle_terminal_editor_focus(
    workspace: &mut Workspace,
    _: &ToggleTerminalEditorFocus,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let active_pane = workspace.active_pane().clone();
    if !active_pane.focus_handle(cx).contains_focused(window, cx) {
        return;
    }
    let on_terminal = active_pane
        .read(cx)
        .active_item()
        .is_some_and(|item| item.downcast::<TerminalView>().is_some());
    let target = if on_terminal {
        most_recently_active_item::<Editor>(workspace, cx)
    } else {
        most_recently_active_item::<TerminalView>(workspace, cx)
    };
    match target {
        Some(target) => {
            workspace.activate_item(target.as_ref(), true, true, window, cx);
        }
        // The user closed every terminal; this is the way back to one.
        None if !on_terminal => {
            TerminalView::deploy(workspace, &center_terminal_action(), window, cx)
        }
        None => {}
    }
}

/// Status bar button that opens a new terminal, so one is always a click away after closing the last.
pub struct NewTerminalButton;

impl gpui::Render for NewTerminalButton {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        IconButton::new("photon-new-terminal", IconName::Terminal)
            .icon_size(IconSize::Small)
            .aria_label("New Terminal")
            .tooltip(|_, cx| Tooltip::for_action("New Terminal", &NewTerminal::default(), cx))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(NewTerminal::default()), cx))
    }
}

impl StatusItemView for NewTerminalButton {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

/// Pane activation timestamps come from one counter shared by the workspace, so they order items across panes.
fn most_recently_active_item<T: 'static>(
    workspace: &Workspace,
    cx: &App,
) -> Option<Box<dyn ItemHandle>> {
    workspace
        .panes()
        .iter()
        .flat_map(|pane| {
            let pane = pane.read(cx);
            pane.activation_history().iter().filter_map(move |entry| {
                let item = pane
                    .items()
                    .find(|item| item.item_id() == entry.entity_id)?;
                item.downcast::<T>()?;
                Some((entry.timestamp, item.boxed_clone()))
            })
        })
        .max_by_key(|(timestamp, _)| *timestamp)
        .map(|(_, item)| item)
}

/// The note shown under the version when the About icon is clicked. Kept quiet on purpose.
pub fn about_icon_click_note(modifiers: &gpui::Modifiers) -> Option<&'static str> {
    (modifiers.alt && !modifiers.control && !modifiers.platform && !modifiers.shift)
        .then_some(photon_brand::DEDICATION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// `cmd-ctrl-t` and `ctrl-cmd-t` are the same keystroke, and a JSON object
    /// keeps only one of two equal keys, so a clash silently drops a binding.
    #[test]
    fn help_links_point_at_photon() {
        assert_eq!(
            release_notes_url(),
            "https://github.com/heapworks-in/photon/releases"
        );
        assert!(super::super::DOCS_URL.starts_with("https://heapworks-in.github.io/photon"));
    }

    #[test]
    fn new_file_shortcut_makes_a_file_from_a_terminal_too() {
        let macos = include_str!("../../../../assets/keymaps/default-macos.json");
        let windows = include_str!("../../../../assets/keymaps/default-windows.json");
        assert!(!macos.contains("\"cmd-n\": \"workspace::NewTerminal\""));
        assert!(!windows.contains("\"ctrl-n\": \"workspace::NewTerminal\""));
    }

    #[test]
    fn photon_tab_shortcuts_are_unique_in_their_keymap_block() {
        fn normalize(keystrokes: &str) -> String {
            keystrokes
                .split(' ')
                .map(|keystroke| {
                    let mut parts = keystroke.split('-').collect::<Vec<_>>();
                    let key = parts.pop().unwrap_or_default();
                    parts.sort_unstable();
                    format!("{}|{key}", parts.join("-"))
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
        let keymap = include_str!("../../../../assets/keymaps/default-macos.json");
        for block in keymap.split("\"bindings\"") {
            if !block.contains("photon_tabs::") {
                continue;
            }
            let block = block.split("\n    }").next().unwrap_or(block);
            let mut seen = std::collections::HashMap::new();
            for line in block.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix('"') else {
                    continue;
                };
                let Some((keystrokes, _)) = rest.split_once("\":") else {
                    continue;
                };
                if let Some(previous) = seen.insert(normalize(keystrokes), line) {
                    panic!("{previous} and {line} bind the same keys");
                }
            }
        }
    }

    #[gpui::test]
    fn offline_actions_hidden(cx: &mut TestAppContext) {
        cx.update(|cx| {
            command_palette_hooks::init(cx);
            hide_offline_actions(cx);
            let filter = CommandPaletteFilter::try_global(cx).expect("filter initialized");
            assert!(filter.is_hidden(&client::SignIn));
            assert!(filter.is_hidden(&auto_update::Check));
            assert!(filter.is_hidden(&zed_actions::OpenAccountSettings));
            assert!(filter.is_hidden(&workspace::NewCenterTerminal::default()));
            assert!(filter.is_hidden(&call_hierarchy::KeepOpen));
            assert!(!filter.is_hidden(&workspace::NewTerminal::default()));
            let from_summary: agent_ui::NewNativeAgentThreadFromSummary =
                serde_json::from_value(serde_json::json!({ "from_session_id": "session" }))
                    .expect("action");
            assert!(filter.is_hidden(&from_summary));
        });
    }

    #[gpui::test]
    fn user_settings_override_photon_defaults(cx: &mut TestAppContext) {
        use gpui::UpdateGlobal as _;
        use settings::{Settings as _, SettingsStore};
        cx.update(|cx| {
            // The real defaults; `SettingsStore::test` swaps in a test theme.
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            let dark_theme = |cx: &gpui::App| {
                theme_settings::ThemeSettings::get_global(cx)
                    .theme
                    .name(gpui::WindowAppearance::Dark.into())
                    .0
            };
            assert_eq!(dark_theme(cx).as_ref(), "Photon Black");
            assert!(!client::TelemetrySettings::get_global(cx).metrics);

            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(
                        r#"{"theme": {"mode": "dark", "light": "One Light", "dark": "One Dark"},
                            "telemetry": {"metrics": true}}"#,
                        cx,
                    )
                    .unwrap();
            });
            assert_eq!(dark_theme(cx).as_ref(), "One Dark");
            assert!(client::TelemetrySettings::get_global(cx).metrics);
        });
    }

    #[test]
    fn font_styles_set_code_and_menu_fonts_and_clear_the_terminal_font() {
        let mut settings = settings::SettingsContent::default();
        settings.terminal.get_or_insert_default().font_family =
            Some(settings::FontFamilyName("Menlo".into()));
        FontStyle::Hack.apply(&mut settings);
        let family = |name: &Option<settings::FontFamilyName>| name.as_ref().map(|n| n.0.clone());
        assert_eq!(
            family(&settings.theme.buffer_font_family).as_deref(),
            Some("Hack")
        );
        assert_eq!(
            family(&settings.theme.ui_font_family).as_deref(),
            Some("Hack")
        );
        assert_eq!(
            settings
                .terminal
                .as_ref()
                .and_then(|t| t.font_family.clone()),
            None
        );

        FontStyle::Mono.apply(&mut settings);
        assert_eq!(
            family(&settings.theme.buffer_font_family).as_deref(),
            Some("Lilex")
        );
        assert_eq!(
            family(&settings.theme.ui_font_family).as_deref(),
            Some("IBM Plex Sans")
        );
    }

    #[test]
    fn center_terminal_follows_the_project_host() {
        // Remote projects must get a shell on the remote host, not on this machine.
        assert!(!center_terminal_action().local);
    }

    #[gpui::test]
    async fn center_terminal_only_for_empty_workspace(cx: &mut TestAppContext) {
        use project::Project;
        use serde_json::json;
        use util::path;
        use workspace::{MultiWorkspace, WorkspaceHandle as _};

        let app_state = super::super::tests::init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "notes.txt": "hello" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();

        cx.read(|cx| assert!(should_open_center_terminal(workspace.read(cx), cx)));

        let file = cx.read(|cx| workspace.file_project_paths(cx))[0].clone();
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.open_path(file, None, true, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap();

        // A restored or opened file means the user already has content; don't add a terminal.
        cx.read(|cx| assert!(!should_open_center_terminal(workspace.read(cx), cx)));
    }

    #[gpui::test]
    async fn toggle_moves_focus_between_terminal_and_editor(cx: &mut TestAppContext) {
        use editor::Editor;
        use project::Project;
        use serde_json::json;
        use terminal::{
            TerminalBuilder,
            terminal_settings::{AlternateScroll, CursorShape},
        };
        use util::{path, paths::PathStyle};
        use workspace::{MultiWorkspace, SplitDirection, WorkspaceHandle as _};

        let app_state = super::super::tests::init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "notes.txt": "hello" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);

        let toggle = |cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                toggle_terminal_editor_focus(workspace, &ToggleTerminalEditorFocus, window, cx)
            })
        };
        let focused_is = |cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                let pane = workspace.active_pane().clone();
                assert!(pane.focus_handle(cx).contains_focused(window, cx));
                let item = pane.read(cx).active_item().expect("active item");
                (
                    item.downcast::<TerminalView>().is_some(),
                    item.downcast::<Editor>().is_some(),
                )
            })
        };

        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        workspace.update_in(cx, |workspace, window, cx| {
            let terminal_view = cx.new(|cx| {
                TerminalView::new(
                    terminal,
                    workspace.weak_handle(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(terminal_view), None, true, window, cx);
        });

        // No editor yet: nothing happens and nothing is opened.
        toggle(cx);
        assert_eq!(focused_is(cx), (true, false));
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.panes().len()),
            1
        );

        let file = cx.read(|cx| workspace.file_project_paths(cx))[0].clone();
        let editor_pane = workspace.update_in(cx, |workspace, window, cx| {
            let terminal_pane = workspace.active_pane().clone();
            let editor_pane =
                workspace.split_pane(terminal_pane, SplitDirection::Right, window, cx);
            let open = workspace.open_path(file, Some(editor_pane.downgrade()), true, window, cx);
            (editor_pane, open)
        });
        editor_pane.1.await.unwrap();
        assert_eq!(focused_is(cx), (false, true));

        toggle(cx);
        assert_eq!(focused_is(cx), (true, false));
        toggle(cx);
        assert_eq!(focused_is(cx), (false, true));
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.panes().len()),
            2
        );
    }

    #[gpui::test]
    async fn toggle_opens_a_terminal_when_none_is_open(cx: &mut TestAppContext) {
        // Opening a terminal starts a real shell process.
        cx.executor().allow_parking();
        use project::Project;
        use serde_json::json;
        use util::path;
        use workspace::{MultiWorkspace, WorkspaceHandle as _};

        let app_state = super::super::tests::init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "notes.txt": "hello" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        let file = cx.read(|cx| workspace.file_project_paths(cx))[0].clone();
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(file, None, true, window, cx)
            })
            .await
            .unwrap();

        workspace.update_in(cx, |workspace, window, cx| {
            toggle_terminal_editor_focus(workspace, &ToggleTerminalEditorFocus, window, cx)
        });
        cx.run_until_parked();

        let terminals = workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<TerminalView>(cx).count()
        });
        assert_eq!(terminals, 1);
    }

    #[gpui::test]
    async fn new_terminal_opens_in_the_center(cx: &mut TestAppContext) {
        // Opening a terminal starts a real shell process.
        cx.executor().allow_parking();
        use project::Project;
        use serde_json::json;
        use util::path;
        use workspace::{MultiWorkspace, WorkspaceHandle as _};

        let app_state = super::super::tests::init_test(cx);
        cx.update(terminal_view::init);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "notes.txt": "hello" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        let file = cx.read(|cx| workspace.file_project_paths(cx))[0].clone();
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(file, None, true, window, cx)
            })
            .await
            .unwrap();

        cx.dispatch_action(NewTerminal::default());
        cx.run_until_parked();

        let center_terminals = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items_of_type::<TerminalView>()
                .count()
        });
        assert_eq!(center_terminals, 1);
    }

    #[test]
    fn about_note_only_on_option_click() {
        assert_eq!(about_icon_click_note(&gpui::Modifiers::default()), None);
        assert_eq!(about_icon_click_note(&gpui::Modifiers::command()), None);
        assert_eq!(
            about_icon_click_note(&gpui::Modifiers::alt()),
            Some(photon_brand::DEDICATION)
        );
    }

    #[test]
    fn hack_font_is_bundled() {
        use gpui::AssetSource as _;
        let fonts = assets::Assets.list("fonts").expect("list fonts");
        for style in ["Regular", "Bold", "Italic", "BoldItalic"] {
            let path = format!("fonts/hack/Hack-{style}.ttf");
            assert!(fonts.iter().any(|font| font.as_ref() == path), "{path}");
        }
        // Prompt icons (starship, powerlevel10k) that Hack lacks; the terminal falls back to it.
        let symbols = "fonts/symbols-nerd-font/SymbolsNerdFontMono-Regular.ttf";
        assert!(
            fonts.iter().any(|font| font.as_ref() == symbols),
            "{symbols}"
        );
    }

    #[gpui::test]
    async fn opening_an_excalidraw_file_opens_a_web_editor_tab(cx: &mut TestAppContext) {
        use editor::Editor;
        use project::Project;
        use serde_json::json;
        use util::path;
        use workspace::MultiWorkspace;

        let app_state = super::super::tests::init_test(cx);
        cx.update(photon_web_editor::init);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/project"),
                json!({ "a.excalidraw": "{}", "a.json": "{}" }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/project").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);

        for (file, is_web_editor) in [("a.excalidraw", true), ("a.json", false)] {
            let item = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(
                        std::path::PathBuf::from(path!("/project")).join(file),
                        workspace::OpenOptions::default(),
                        window,
                        cx,
                    )
                })
                .await
                .unwrap();
            assert_eq!(
                item.downcast::<photon_web_editor::WebEditorView>()
                    .is_some(),
                is_web_editor,
                "{file}"
            );
            assert_eq!(
                item.downcast::<Editor>().is_some(),
                !is_web_editor,
                "{file}"
            );
        }
    }
}
