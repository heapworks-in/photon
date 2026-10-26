use std::collections::{HashMap, HashSet};

use editor::Editor;
use gpui::{Anchor, App, Context, Entity, IntoElement, Render, Subscription, WeakEntity, Window};
use language::{Buffer, LanguageServerName, PLAIN_TEXT};
use project::{Project, lsp_store::LspStoreEvent};
use ui::{ContextMenu, CopyButton, PopoverMenu, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

use crate::language_server_fixes::known_fix;

/// Status bar item shown while the active file's language server failed or is missing, with help
/// to get one running.
pub struct SetUpLanguageServer {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    active_buffer: Option<Entity<Buffer>>,
    /// The latest error of every server whose download or start failed, project-wide.
    failures: HashMap<LanguageServerName, String>,
    problem: Option<Problem>,
    _lsp_store_subscription: Subscription,
}

#[derive(Clone, Debug, PartialEq)]
enum Problem {
    Failed {
        server: LanguageServerName,
        error: String,
        language: String,
    },
    Missing {
        extension_id: String,
        language: String,
    },
}

impl SetUpLanguageServer {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let lsp_store = project.read(cx).lsp_store();
        let _lsp_store_subscription = cx.subscribe(&lsp_store, |this, _, event, cx| match event {
            LspStoreEvent::LanguageServerUpdate {
                name: Some(name),
                message:
                    proto::update_language_server::Variant::StatusUpdate(proto::StatusUpdate {
                        message,
                        status: Some(proto::status_update::Status::Binary(status)),
                    }),
                ..
            } => {
                if *status == proto::ServerBinaryStatus::Failed as i32 {
                    this.failures
                        .insert(name.clone(), message.clone().unwrap_or_default());
                } else {
                    this.failures.remove(name);
                }
                this.refresh(cx);
            }
            LspStoreEvent::LanguageServerAdded(_, name, _) => {
                this.failures.remove(name);
                this.refresh(cx);
            }
            LspStoreEvent::LanguageServerRemoved(_) | LspStoreEvent::LanguageDetected { .. } => {
                this.refresh(cx)
            }
            _ => {}
        });
        Self {
            workspace: workspace.weak_handle(),
            project,
            active_buffer: None,
            failures: HashMap::default(),
            problem: None,
            _lsp_store_subscription,
        }
    }

    /// The extension "Set up" looks for, while the item offers to set one up.
    pub fn extension_id(&self) -> Option<&str> {
        match &self.problem {
            Some(Problem::Missing { extension_id, .. }) => Some(extension_id),
            _ => None,
        }
    }

    /// The server whose failure the item explains.
    pub fn failed_server(&self) -> Option<&LanguageServerName> {
        match &self.problem {
            Some(Problem::Failed { server, .. }) => Some(server),
            _ => None,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let problem = self
            .active_buffer
            .clone()
            .and_then(|buffer| self.problem_for(&buffer, cx));
        if problem != self.problem {
            self.problem = problem;
            cx.notify();
        }
    }

    fn problem_for(&self, buffer: &Entity<Buffer>, cx: &mut App) -> Option<Problem> {
        // ponytail: remote servers aren't visible here, so remote projects never show the item.
        if !self.project.read(cx).is_local() {
            return None;
        }
        let language = buffer
            .read(cx)
            .language()
            .filter(|language| **language != *PLAIN_TEXT)
            .cloned();
        if let Some(language) = &language {
            let failure = self
                .project
                .read(cx)
                .languages()
                .lsp_adapters(&language.name())
                .into_iter()
                .find_map(|adapter| {
                    self.failures
                        .get(&adapter.name)
                        .map(|error| (adapter.name.clone(), error.clone()))
                });
            if let Some((server, error)) = failure {
                return Some(Problem::Failed {
                    server,
                    error,
                    language: language.name().to_string(),
                });
            }
        }
        let extension_id = extension_to_set_up(&self.project, buffer, cx)?;
        Some(Problem::Missing {
            language: language
                .map(|language| language.name().to_string())
                .unwrap_or_else(|| extension_id.clone()),
            extension_id,
        })
    }

    fn restart(this: &WeakEntity<Self>, cx: &mut App) {
        let Ok((lsp_store, Some(buffer))) = this.read_with(cx, |this, cx| {
            (
                this.project.read(cx).lsp_store(),
                this.active_buffer.clone(),
            )
        }) else {
            return;
        };
        lsp_store.update(cx, |lsp_store, cx| {
            lsp_store.restart_language_servers_for_buffers(vec![buffer], HashSet::default(), cx)
        });
    }

    fn help_menu(
        this: WeakEntity<Self>,
        problem: Problem,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        ContextMenu::build(window, cx, move |menu, _, _| {
            let (title, error, language) = match &problem {
                Problem::Failed {
                    server,
                    error,
                    language,
                } => (
                    format!("{server} failed to start"),
                    Some(error.as_str()),
                    language,
                ),
                Problem::Missing { language, .. } => {
                    (format!("No language server for {language}"), None, language)
                }
            };
            let fix = known_fix(language, error);
            let restart = {
                let this = this.clone();
                move |_: &mut Window, cx: &mut App| Self::restart(&this, cx)
            };
            let menu = menu
                .header(title)
                .when_some(error.map(first_lines), |menu, lines| {
                    menu.custom_row(move |_, _| text_row(lines.clone(), Color::Muted))
                })
                .when_some(fix, |menu, fix| {
                    menu.custom_row(move |_, _| text_row(fix.hint.into(), Color::Default))
                        .custom_row(move |_, cx| {
                            copy_row("photon-copy-fix-command", fix.command, cx)
                        })
                        .when_some(fix.settings, |menu, settings| {
                            menu.custom_row(move |_, cx| {
                                copy_row("photon-copy-fix-settings", settings, cx)
                            })
                        })
                })
                .separator();
            match problem.clone() {
                Problem::Failed { server, error, .. } => {
                    let this = this.clone();
                    menu.entry("Restart", None, restart).entry(
                        "Show Log",
                        None,
                        move |window, cx| {
                            if let Ok(workspace) =
                                this.read_with(cx, |this, _| this.workspace.clone())
                            {
                                show_error(workspace, &server, &error, window, cx);
                            }
                        },
                    )
                }
                Problem::Missing { extension_id, .. } => menu
                    .entry("Install Extension", None, move |window, cx| {
                        window.dispatch_action(
                            Box::new(zed_actions::Extensions {
                                category_filter: None,
                                id: Some(extension_id.clone()),
                            }),
                            cx,
                        );
                    })
                    .entry("Detect & Start", None, restart),
            }
        })
    }
}

fn first_lines(error: &str) -> SharedString {
    error
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("\n")
        .into()
}

fn text_row(text: SharedString, color: Color) -> AnyElement {
    div()
        .max_w(rems(28.))
        .child(Label::new(text).size(LabelSize::Small).color(color))
        .into_any_element()
}

fn copy_row(selector: &'static str, text: &'static str, cx: &App) -> AnyElement {
    h_flex()
        .max_w(rems(28.))
        .gap_1()
        .justify_between()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Label::new(text).size(LabelSize::Small).buffer_font(cx)),
        )
        .child(
            div()
                .flex_none()
                .debug_selector(move || selector.into())
                .child(CopyButton::new(selector, text)),
        )
        .into_any_element()
}

/// A server that failed to start never reaches the server log view (it is added once running), so
/// its error, which carries the server's stderr, is its log.
fn show_error(
    workspace: WeakEntity<Workspace>,
    server: &LanguageServerName,
    error: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let create_buffer = workspace.update(cx, |workspace, cx| {
        workspace
            .project()
            .update(cx, |project, cx| project.create_buffer(None, false, cx))
    });
    let text = format!("Language server {server}:\n\n{error}");
    window
        .spawn(cx, async move |cx| {
            let buffer = create_buffer.await?;
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, text)], None, cx);
                buffer.set_capability(language::Capability::ReadOnly, cx);
            });
            workspace.update_in(cx, |workspace, window, cx| {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::for_buffer(buffer, None, window, cx);
                    editor.set_read_only(true);
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
        })
        .detach_and_log_err(cx);
}

/// Files Zed has no language for are matched to an extension by name; files with a language are
/// only flagged when that language has servers registered but none started for the file.
fn extension_to_set_up(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut App,
) -> Option<String> {
    // ponytail: remote servers aren't visible here, so remote projects never show the item.
    if !project.read(cx).is_local() {
        return None;
    }
    let lsp_store = project.read(cx).lsp_store();
    let has_server = buffer.update(cx, |buffer, cx| {
        lsp_store.update(cx, |lsp_store, cx| {
            !lsp_store
                .language_servers_for_local_buffer(buffer, cx)
                .is_empty()
        })
    });
    if has_server {
        return None;
    }
    let buffer = buffer.read(cx);
    let suggested = extension_suggest::suggest_extension(buffer.file()?.path())
        .map(|suggestion| suggestion.extension_id.to_string());
    match buffer
        .language()
        .filter(|language| **language != *PLAIN_TEXT)
    {
        None => suggested,
        Some(language) => {
            let has_adapters = !project
                .read(cx)
                .languages()
                .lsp_adapters(&language.name())
                .is_empty();
            has_adapters.then(|| {
                suggested
                    .unwrap_or_else(|| language.name().as_ref().to_lowercase().replace(' ', "-"))
            })
        }
    }
}

impl Render for SetUpLanguageServer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(problem) = self.problem.clone() else {
            return h_flex().hidden();
        };
        let (label, button, tooltip) = match &problem {
            Problem::Failed { .. } => (
                "Language server failed",
                "Help",
                "Why the language server failed",
            ),
            Problem::Missing { .. } => ("No language server", "Set up", "Set up a language server"),
        };
        let this = cx.weak_entity();
        h_flex()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .debug_selector(|| "photon-set-up-language-server".into())
                    .child(
                        PopoverMenu::new("photon-language-server-help")
                            .anchor(Anchor::BottomLeft)
                            .trigger_with_tooltip(
                                Button::new("photon-set-up-language-server", button)
                                    .label_size(LabelSize::Small),
                                Tooltip::text(tooltip),
                            )
                            .menu(move |window, cx| {
                                Some(SetUpLanguageServer::help_menu(
                                    this.clone(),
                                    problem.clone(),
                                    window,
                                    cx,
                                ))
                            }),
                    ),
            )
    }
}

impl StatusItemView for SetUpLanguageServer {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_buffer = active_pane_item
            .and_then(|item| item.downcast::<Editor>())
            .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton());
        self.refresh(cx);
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt as _, channel::mpsc::UnboundedReceiver};
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use language::{BinaryStatus, FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
    use lsp::FakeLanguageServer;
    use project::FakeFs;
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc, sync::Arc};
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace};

    const OLD_JAVA_ERROR: &str = "failed to initialize jdtls: Server reset the connection\n\
        -- stderr --\n\
        This version of the Java Runtime only recognizes class file versions up to 61.0";

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
        });
    }

    struct Opened {
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        item: Entity<SetUpLanguageServer>,
        fake_servers: Option<UnboundedReceiver<FakeLanguageServer>>,
        cx: VisualTestContext,
    }

    async fn open(file: &str, fake_server: bool, cx: &mut TestAppContext) -> Opened {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ file: "class Foo {}\n" }))
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let mut fake_servers = None;
        if fake_server {
            project.read_with(cx, |project, _| {
                let registry = project.languages();
                registry.add(Arc::new(Language::new(
                    LanguageConfig {
                        name: "Java".into(),
                        matcher: Arc::new(LanguageMatcher {
                            path_suffixes: vec!["java".into()],
                            ..LanguageMatcher::default()
                        }),
                        ..LanguageConfig::default()
                    },
                    None,
                )));
                fake_servers = Some(registry.register_fake_lsp(
                    "Java",
                    FakeLspAdapter {
                        name: "jdtls",
                        ..FakeLspAdapter::default()
                    },
                ));
            });
        }
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let item = workspace.update_in(cx, |workspace, window, cx| {
            let item = cx.new(|cx| SetUpLanguageServer::new(workspace, cx));
            workspace.status_bar().update(cx, |status_bar, cx| {
                status_bar.add_left_item(item.clone(), window, cx)
            });
            item
        });
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id()
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path(file)), None, true, window, cx)
            })
            .await
            .expect("file opens")
            .downcast::<Editor>()
            .expect("an editor");
        if let Some(fake_servers) = fake_servers.as_mut() {
            fake_servers.next().await.expect("server starts");
        }
        cx.run_until_parked();
        Opened {
            workspace,
            project,
            item,
            fake_servers,
            cx: cx.clone(),
        }
    }

    fn fail_jdtls(opened: &mut Opened) {
        opened.project.read_with(&opened.cx, |project, _| {
            project.languages().update_lsp_binary_status(
                LanguageServerName::new_static("jdtls"),
                BinaryStatus::Failed {
                    error: OLD_JAVA_ERROR.to_string(),
                },
            )
        });
        opened.cx.run_until_parked();
    }

    fn started_again(opened: &mut Opened) -> bool {
        opened.cx.run_until_parked();
        opened
            .fake_servers
            .as_mut()
            .is_some_and(|fake_servers| fake_servers.try_recv().is_ok())
    }

    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    fn copied(cx: &VisualTestContext) -> Option<String> {
        cx.read_from_clipboard().and_then(|item| item.text())
    }

    #[gpui::test]
    async fn shown_for_a_java_file_without_a_server(cx: &mut TestAppContext) {
        init_test(cx);
        let Opened { item, mut cx, .. } = open("Foo.java", false, cx).await;
        item.read_with(&cx, |item, _| assert_eq!(item.extension_id(), Some("java")));
        assert!(cx.debug_bounds("photon-set-up-language-server").is_some());
    }

    #[gpui::test]
    async fn hidden_while_a_server_runs(cx: &mut TestAppContext) {
        init_test(cx);
        let Opened { item, mut cx, .. } = open("Foo.java", true, cx).await;
        item.read_with(&cx, |item, _| {
            assert_eq!(item.extension_id(), None);
            assert_eq!(item.failed_server(), None);
        });
        assert!(cx.debug_bounds("photon-set-up-language-server").is_none());
    }

    #[gpui::test]
    async fn hidden_for_files_without_a_language(cx: &mut TestAppContext) {
        init_test(cx);
        let Opened { item, cx, .. } = open("notes.txt", false, cx).await;
        item.read_with(&cx, |item, _| assert_eq!(item.extension_id(), None));
    }

    #[gpui::test]
    async fn set_up_offers_the_extension_and_the_system_requirement(cx: &mut TestAppContext) {
        init_test(cx);
        let Opened {
            workspace, mut cx, ..
        } = open("Foo.java", false, cx).await;
        let opened = Rc::new(RefCell::new(None));
        workspace.update(&mut cx, |workspace, _| {
            let opened = opened.clone();
            workspace.register_action(move |_, action: &zed_actions::Extensions, _, _| {
                opened.replace(Some(action.clone()));
            });
        });
        cx.run_until_parked();

        click("photon-set-up-language-server", &mut cx);
        assert!(cx.debug_bounds("MENU_ITEM-Detect & Start").is_some());
        click("photon-copy-fix-command", &mut cx);
        assert_eq!(copied(&cx).as_deref(), Some("brew install openjdk@21"));
        click("MENU_ITEM-Install Extension", &mut cx);

        assert_eq!(
            opened.borrow().clone(),
            Some(zed_actions::Extensions {
                category_filter: None,
                id: Some("java".to_string()),
            })
        );
    }

    #[gpui::test]
    async fn detect_and_start_starts_the_server(cx: &mut TestAppContext) {
        init_test(cx);
        let mut opened = open("Foo.java", true, cx).await;
        opened.project.update(&mut opened.cx, |project, cx| {
            project
                .lsp_store()
                .update(cx, |lsp_store, cx| lsp_store.stop_all_language_servers(cx))
        });
        opened.cx.run_until_parked();
        opened.item.read_with(&opened.cx, |item, _| {
            assert_eq!(item.extension_id(), Some("java"))
        });

        click("photon-set-up-language-server", &mut opened.cx);
        click("MENU_ITEM-Detect & Start", &mut opened.cx);
        assert!(started_again(&mut opened), "server starts again");
        opened
            .item
            .read_with(&opened.cx, |item, _| assert_eq!(item.extension_id(), None));
    }

    #[gpui::test]
    async fn a_failed_server_explains_itself_with_the_known_fix(cx: &mut TestAppContext) {
        init_test(cx);
        let mut opened = open("Foo.java", true, cx).await;
        fail_jdtls(&mut opened);
        let Opened { item, mut cx, .. } = opened;
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.failed_server().map(|name| name.0.as_ref()),
                Some("jdtls")
            )
        });

        click("photon-set-up-language-server", &mut cx);
        assert!(cx.debug_bounds("MENU_ITEM-Restart").is_some());
        assert!(cx.debug_bounds("MENU_ITEM-Show Log").is_some());
        click("photon-copy-fix-command", &mut cx);
        assert_eq!(copied(&cx).as_deref(), Some("brew install openjdk@21"));
        click("photon-copy-fix-settings", &mut cx);
        assert!(copied(&cx).is_some_and(|text| text.contains(r#""java_home""#)));
    }

    #[gpui::test]
    async fn restart_restarts_the_failed_server(cx: &mut TestAppContext) {
        init_test(cx);
        let mut opened = open("Foo.java", true, cx).await;
        fail_jdtls(&mut opened);

        click("photon-set-up-language-server", &mut opened.cx);
        click("MENU_ITEM-Restart", &mut opened.cx);
        assert!(started_again(&mut opened), "server restarts");
        opened
            .item
            .read_with(&opened.cx, |item, _| assert_eq!(item.failed_server(), None));
    }

    #[gpui::test]
    async fn show_log_opens_the_full_error(cx: &mut TestAppContext) {
        init_test(cx);
        let mut opened = open("Foo.java", true, cx).await;
        fail_jdtls(&mut opened);
        let Opened {
            workspace, mut cx, ..
        } = opened;

        click("photon-set-up-language-server", &mut cx);
        click("MENU_ITEM-Show Log", &mut cx);
        let text = workspace.update(&mut cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("an editor")
                .read(cx)
                .text(cx)
        });
        assert!(text.starts_with("Language server jdtls:"), "{text}");
        assert!(text.contains("class file versions up to 61.0"), "{text}");
    }
}
