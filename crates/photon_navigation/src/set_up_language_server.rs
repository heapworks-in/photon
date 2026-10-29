use std::collections::{HashMap, HashSet};

use editor::Editor;
use gpui::{Anchor, App, Context, Entity, IntoElement, Render, Subscription, WeakEntity, Window};
use language::{Buffer, LanguageServerName, PLAIN_TEXT};
use project::{Project, lsp_store::LspStoreEvent};
use ui::PopoverMenuHandle;
use ui::{ContextMenu, CopyButton, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

use crate::{
    java_setup::{self, JavaSetup, MIN_JAVA, NeedsJava},
    language_server_fixes::known_fix,
};

/// Status bar item shown while the active file's language server failed or is missing, with help
/// to get one running.
pub struct SetUpLanguageServer {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    active_buffer: Option<Entity<Buffer>>,
    /// The latest error of every server whose download or start failed, project-wide.
    failures: HashMap<LanguageServerName, String>,
    problem: Option<Problem>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    java_notice_shown: bool,
    _subscriptions: [Subscription; 2],
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
    NeedsJava {
        needs: NeedsJava,
        installing: bool,
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
        let java_setup_subscription = cx.observe_global::<JavaSetup>(Self::refresh);
        Self {
            workspace: workspace.weak_handle(),
            project,
            active_buffer: None,
            failures: HashMap::default(),
            problem: None,
            menu_handle: PopoverMenuHandle::default(),
            java_notice_shown: false,
            _subscriptions: [_lsp_store_subscription, java_setup_subscription],
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

    pub fn needs_java(&self) -> Option<NeedsJava> {
        match &self.problem {
            Some(Problem::NeedsJava { needs, .. }) => Some(needs.clone()),
            _ => None,
        }
    }

    /// Opens the help popover for the active file's problem; false when there is none.
    pub fn show_help(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.problem.is_none() {
            return false;
        }
        self.menu_handle.show(window, cx);
        true
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
        match &self.problem {
            Some(Problem::NeedsJava { needs, .. }) if !self.java_notice_shown => {
                self.java_notice_shown = true;
                let needs = needs.clone();
                let workspace = self.workspace.clone();
                // The active item changes while the workspace is being updated.
                cx.defer(move |cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            java_setup::show_needs_java(workspace, &needs, cx)
                        })
                        .log_err();
                });
            }
            _ if self
                .active_buffer
                .as_ref()
                .is_some_and(|buffer| is_java(buffer, cx)) =>
            {
                java_setup::check_once(self.workspace.clone(), self.project.clone(), cx)
            }
            _ => {}
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
        if let Some(language) = &language
            && language.name().as_ref() == "Java"
            && let Some(needs) = JavaSetup::needs_java(cx)
        {
            return Some(Problem::NeedsJava {
                needs,
                installing: JavaSetup::installing(cx),
            });
        }
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
        if let Problem::NeedsJava { needs, .. } = &problem {
            let needs = needs.clone();
            let workspace = this.read_with(cx, |this, _| this.workspace.clone()).ok();
            return ContextMenu::build(window, cx, move |menu, _, _| {
                needs_java_menu(menu, &needs, workspace.clone())
            });
        }
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
                Problem::NeedsJava { .. } => return menu,
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
                Problem::NeedsJava { .. } => menu,
            }
        })
    }
}

fn needs_java_menu(
    menu: ContextMenu,
    needs: &NeedsJava,
    workspace: Option<WeakEntity<Workspace>>,
) -> ContextMenu {
    let has = match needs.current {
        Some(current) => format!("This Mac has Java {current}."),
        None => "This Mac has no Java.".to_string(),
    };
    let body: SharedString = format!(
        "Go to definition, errors and completions for Java need Java {MIN_JAVA} or newer. {has}"
    )
    .into();
    menu.header(format!("Java code intelligence needs Java {MIN_JAVA}"))
        .custom_row(move |_, _| text_row(body.clone(), Color::Muted))
        .separator()
        .entry(
            java_setup::install_label(needs.has_brew),
            None,
            move |window, cx| {
                if let Some(workspace) = workspace.clone() {
                    java_setup::install_java(workspace, window, cx);
                }
            },
        )
}

fn is_java(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == "Java")
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
        let (label, button, tooltip, color) = match &problem {
            Problem::Failed { .. } => (
                "Language server failed",
                "Help",
                "Why the language server failed",
                Color::Muted,
            ),
            Problem::Missing { .. } => (
                "No language server",
                "Set Up",
                "Set up a language server",
                Color::Muted,
            ),
            Problem::NeedsJava {
                installing: true, ..
            } => {
                return h_flex().child(
                    Label::new(format!("Installing Java {MIN_JAVA}…"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            Problem::NeedsJava { .. } => (
                "Needs Java 21",
                "Set Up",
                "Set up Java for Java code intelligence",
                Color::Warning,
            ),
        };
        let this = cx.weak_entity();
        h_flex()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(color))
            .child(
                div()
                    .debug_selector(|| "photon-set-up-language-server".into())
                    .child(
                        PopoverMenu::new("photon-language-server-help")
                            .with_handle(self.menu_handle.clone())
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
    use crate::java_setup::{
        InstallFailedNotice, InstalledNotice, JavaInstall, JavaScan, NeedsJavaNotice,
        UsingJavaNotice,
    };
    use futures::{StreamExt as _, channel::mpsc::UnboundedReceiver};
    use gpui::Task;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use language::{BinaryStatus, FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
    use lsp::FakeLanguageServer;
    use project::FakeFs;
    use project::project_settings::ProjectSettings;
    use serde_json::json;
    use settings::{Settings as _, SettingsStore};
    use std::{
        cell::{Cell, RefCell},
        os::unix::process::ExitStatusExt as _,
        process::ExitStatus,
        rc::Rc,
        sync::Arc,
    };
    use task::SpawnInTerminal;
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace};
    use workspace::{TerminalProvider, notifications::NotificationId};

    const OLD_JAVA_ERROR: &str = "failed to initialize jdtls: Server reset the connection\n\
        -- stderr --\n\
        This version of the Java Runtime only recognizes class file versions up to 61.0";

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            crate::register_language_server_help(cx);
        });
    }

    struct FakeJava {
        scan: Rc<RefCell<JavaScan>>,
        detections: Rc<Cell<usize>>,
    }

    fn fake_java(scan: JavaScan, cx: &mut TestAppContext) -> FakeJava {
        let scan = Rc::new(RefCell::new(scan));
        let detections = Rc::new(Cell::new(0));
        let detect: java_setup::Detect = Rc::new({
            let scan = scan.clone();
            let detections = detections.clone();
            move |_, _| {
                detections.set(detections.get() + 1);
                Task::ready(scan.borrow().clone())
            }
        });
        cx.update(|cx| cx.set_global(JavaSetup::new(detect)));
        FakeJava { scan, detections }
    }

    fn old_java_only() -> JavaScan {
        JavaScan {
            current: Some(17),
            installs: vec![JavaInstall {
                major: 17,
                home: "/jdk17".into(),
            }],
            has_brew: true,
        }
    }

    fn with_java(major: u32, home: &str) -> JavaScan {
        let mut scan = old_java_only();
        scan.installs.push(JavaInstall {
            major,
            home: home.into(),
        });
        scan
    }

    fn java_home(cx: &VisualTestContext) -> Option<String> {
        cx.read(|cx| {
            ProjectSettings::get_global(cx)
                .lsp
                .get(&LanguageServerName::new_static("jdtls"))
                .and_then(|settings| settings.settings.as_ref())
                .and_then(|settings| settings.get("java_home"))
                .and_then(|home| home.as_str())
                .map(str::to_string)
        })
    }

    fn has_notice<T: 'static>(workspace: &Entity<Workspace>, cx: &VisualTestContext) -> bool {
        workspace.read_with(cx, |workspace, _| {
            workspace.has_notification(&NotificationId::unique::<T>())
        })
    }

    struct FakeTerminal {
        spawned: Rc<RefCell<Vec<SpawnInTerminal>>>,
        exit_code: i32,
    }

    impl TerminalProvider for FakeTerminal {
        fn spawn(
            &self,
            task: SpawnInTerminal,
            _: &mut Window,
            _: &mut App,
        ) -> Task<Option<anyhow::Result<ExitStatus>>> {
            self.spawned.borrow_mut().push(task);
            Task::ready(Some(Ok(ExitStatus::from_raw(self.exit_code << 8))))
        }
    }

    fn fake_terminal(
        exit_code: i32,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Rc<RefCell<Vec<SpawnInTerminal>>> {
        let spawned = Rc::new(RefCell::new(Vec::new()));
        workspace.update(cx, |workspace, _| {
            workspace.set_terminal_provider(FakeTerminal {
                spawned: spawned.clone(),
                exit_code,
            })
        });
        spawned
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

    #[gpui::test]
    async fn a_newer_java_is_used_and_announced(cx: &mut TestAppContext) {
        init_test(cx);
        let java = fake_java(with_java(27, "/jdk27"), cx);
        let mut opened = open("Foo.java", true, cx).await;
        assert_eq!(java.detections.get(), 1);
        assert_eq!(java_home(&opened.cx).as_deref(), Some("/jdk27"));
        assert!(started_again(&mut opened), "jdtls restarts on the new Java");
        assert!(has_notice::<UsingJavaNotice>(&opened.workspace, &opened.cx));
        assert!(!has_notice::<NeedsJavaNotice>(
            &opened.workspace,
            &opened.cx
        ));
    }

    #[gpui::test]
    async fn a_chosen_java_home_is_never_overwritten(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    java_setup::set_java_home(settings, "/mine", true)
                })
            })
        });
        let java = fake_java(with_java(27, "/jdk27"), cx);
        let opened = open("Foo.java", true, cx).await;
        assert_eq!(java.detections.get(), 0);
        assert_eq!(java_home(&opened.cx).as_deref(), Some("/mine"));
        assert!(!has_notice::<UsingJavaNotice>(
            &opened.workspace,
            &opened.cx
        ));
    }

    #[gpui::test]
    async fn the_java_notice_shows_once_per_project(cx: &mut TestAppContext) {
        init_test(cx);
        let java = fake_java(old_java_only(), cx);
        let Opened {
            workspace,
            project,
            mut cx,
            ..
        } = open("Foo.java", true, cx).await;
        assert!(has_notice::<NeedsJavaNotice>(&workspace, &cx));
        assert_eq!(java_home(&cx), None);

        // "Not now" dismisses the notice; opening another Java file doesn't bring it back.
        workspace.update(&mut cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<NeedsJavaNotice>(), cx)
        });
        let worktree_id = project.read_with(&cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id()
        });
        project
            .update(&mut cx, |project, cx| {
                project.create_entry((worktree_id, rel_path("Bar.java")), false, cx)
            })
            .await
            .expect("file created");
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path("Bar.java")), None, true, window, cx)
            })
            .await
            .expect("file opens");
        cx.run_until_parked();
        assert!(!has_notice::<NeedsJavaNotice>(&workspace, &cx));
        assert_eq!(java.detections.get(), 1, "detection runs once per app run");
    }

    #[gpui::test]
    async fn not_now_leaves_the_status_item(cx: &mut TestAppContext) {
        init_test(cx);
        fake_java(old_java_only(), cx);
        let Opened {
            workspace,
            item,
            mut cx,
            ..
        } = open("Foo.java", true, cx).await;
        workspace.update(&mut cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<NeedsJavaNotice>(), cx)
        });
        cx.run_until_parked();
        assert!(item.read_with(&cx, |item, _| item.needs_java().is_some()));
        click("photon-set-up-language-server", &mut cx);
        assert!(cx.debug_bounds("MENU_ITEM-Install Java 21").is_some());
    }

    #[gpui::test]
    async fn without_homebrew_the_button_downloads_java(cx: &mut TestAppContext) {
        init_test(cx);
        let mut scan = old_java_only();
        scan.has_brew = false;
        fake_java(scan, cx);
        let Opened {
            workspace, mut cx, ..
        } = open("Foo.java", true, cx).await;
        let spawned = fake_terminal(0, &workspace, &mut cx);
        click("photon-set-up-language-server", &mut cx);
        assert!(cx.debug_bounds("MENU_ITEM-Install Java 21").is_none());
        click("MENU_ITEM-Download Java 21", &mut cx);
        assert!(spawned.borrow().is_empty());
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://adoptium.net/temurin/releases/?version=21")
        );
    }

    #[gpui::test]
    async fn install_runs_in_a_terminal_tab_and_reports_failure(cx: &mut TestAppContext) {
        init_test(cx);
        fake_java(old_java_only(), cx);
        let Opened {
            workspace,
            item,
            mut cx,
            ..
        } = open("Foo.java", true, cx).await;
        let spawned = fake_terminal(1, &workspace, &mut cx);
        click("photon-set-up-language-server", &mut cx);
        click("MENU_ITEM-Install Java 21", &mut cx);

        let spawned = spawned.borrow();
        let task = spawned.first().expect("the install runs in a terminal");
        assert_eq!(task.label, "Install Java 21");
        assert_eq!(task.command.as_deref(), Some("brew"));
        assert_eq!(task.args, ["install", "openjdk@21"]);
        assert!(has_notice::<InstallFailedNotice>(&workspace, &cx));
        assert!(!has_notice::<InstalledNotice>(&workspace, &cx));
        assert_eq!(java_home(&cx), None);
        assert!(item.read_with(&cx, |item, _| item.needs_java().is_some()));
    }

    #[gpui::test]
    async fn a_successful_install_uses_the_new_java(cx: &mut TestAppContext) {
        init_test(cx);
        let java = fake_java(old_java_only(), cx);
        let mut opened = open("Foo.java", true, cx).await;
        let spawned = fake_terminal(0, &opened.workspace, &mut opened.cx);
        *java.scan.borrow_mut() = with_java(21, "/opt/homebrew/opt/openjdk@21/home");

        click("photon-set-up-language-server", &mut opened.cx);
        click("MENU_ITEM-Install Java 21", &mut opened.cx);

        assert_eq!(spawned.borrow().len(), 1);
        assert_eq!(
            java_home(&opened.cx).as_deref(),
            Some("/opt/homebrew/opt/openjdk@21/home")
        );
        assert!(started_again(&mut opened), "jdtls restarts on the new Java");
        assert!(has_notice::<InstalledNotice>(&opened.workspace, &opened.cx));
        opened
            .item
            .read_with(&opened.cx, |item, _| assert_eq!(item.needs_java(), None));
    }

    #[gpui::test]
    async fn show_language_server_help_opens_the_help(cx: &mut TestAppContext) {
        init_test(cx);
        let mut opened = open("Foo.java", true, cx).await;
        fail_jdtls(&mut opened);
        opened.cx.dispatch_action(crate::ShowLanguageServerHelp);
        opened.cx.run_until_parked();
        assert!(opened.cx.debug_bounds("MENU_ITEM-Restart").is_some());
    }

    #[gpui::test]
    async fn show_language_server_help_shows_the_java_notice(cx: &mut TestAppContext) {
        init_test(cx);
        fake_java(old_java_only(), cx);
        let Opened {
            workspace, mut cx, ..
        } = open("Foo.java", true, cx).await;
        workspace.update(&mut cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<NeedsJavaNotice>(), cx)
        });
        cx.dispatch_action(crate::ShowLanguageServerHelp);
        cx.run_until_parked();
        assert!(has_notice::<NeedsJavaNotice>(&workspace, &cx));
    }
}
