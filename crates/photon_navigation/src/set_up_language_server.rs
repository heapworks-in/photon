use editor::Editor;
use gpui::{App, Context, Entity, IntoElement, Render, Subscription, Window};
use language::{Buffer, PLAIN_TEXT};
use project::{Project, lsp_store::LspStoreEvent};
use ui::{Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

/// Status bar item shown while the active file has no language server, with a way to install one.
pub struct SetUpLanguageServer {
    project: Entity<Project>,
    active_buffer: Option<Entity<Buffer>>,
    extension_id: Option<String>,
    _lsp_store_subscription: Subscription,
}

impl SetUpLanguageServer {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let lsp_store = project.read(cx).lsp_store();
        let _lsp_store_subscription = cx.subscribe(&lsp_store, |this, _, event, cx| {
            if matches!(
                event,
                LspStoreEvent::LanguageServerAdded(..)
                    | LspStoreEvent::LanguageServerRemoved(_)
                    | LspStoreEvent::LanguageDetected { .. }
            ) {
                this.refresh(cx);
            }
        });
        Self {
            project,
            active_buffer: None,
            extension_id: None,
            _lsp_store_subscription,
        }
    }

    /// The extension "Set up" looks for, while the item is shown.
    pub fn extension_id(&self) -> Option<&str> {
        self.extension_id.as_deref()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let extension_id = self
            .active_buffer
            .clone()
            .and_then(|buffer| extension_to_set_up(&self.project, &buffer, cx));
        if extension_id != self.extension_id {
            self.extension_id = extension_id;
            cx.notify();
        }
    }
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
        let Some(extension_id) = self.extension_id.clone() else {
            return h_flex().hidden();
        };
        h_flex()
            .gap_1()
            .child(
                Label::new("No language server")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .debug_selector(|| "photon-set-up-language-server".into())
                    .child(
                        Button::new("photon-set-up-language-server", "Set up")
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text("Find a language server extension"))
                            .on_click(cx.listener(move |_, _, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::Extensions {
                                        category_filter: None,
                                        id: Some(extension_id.clone()),
                                    }),
                                    cx,
                                );
                            })),
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
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
    use project::FakeFs;
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc, sync::Arc};
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
        });
    }

    async fn open(
        file: &str,
        fake_server: bool,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<SetUpLanguageServer>,
        VisualTestContext,
    ) {
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
                fake_servers = Some(registry.register_fake_lsp("Java", FakeLspAdapter::default()));
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
        if let Some(mut fake_servers) = fake_servers {
            use futures::StreamExt as _;
            fake_servers.next().await.expect("server starts");
        }
        cx.run_until_parked();
        (workspace, item, cx.clone())
    }

    #[gpui::test]
    async fn shown_for_a_java_file_without_a_server(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, item, mut cx) = open("Foo.java", false, cx).await;
        item.read_with(&cx, |item, _| assert_eq!(item.extension_id(), Some("java")));
        assert!(cx.debug_bounds("photon-set-up-language-server").is_some());
    }

    #[gpui::test]
    async fn hidden_while_a_server_runs(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, item, mut cx) = open("Foo.java", true, cx).await;
        item.read_with(&cx, |item, _| assert_eq!(item.extension_id(), None));
        assert!(cx.debug_bounds("photon-set-up-language-server").is_none());
    }

    #[gpui::test]
    async fn hidden_for_files_without_a_language(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, item, cx) = open("notes.txt", false, cx).await;
        item.read_with(&cx, |item, _| assert_eq!(item.extension_id(), None));
    }

    #[gpui::test]
    async fn set_up_opens_extensions_for_the_language(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, _, mut cx) = open("Foo.java", false, cx).await;
        let opened = Rc::new(RefCell::new(None));
        workspace.update(&mut cx, |workspace, _| {
            let opened = opened.clone();
            workspace.register_action(move |_, action: &zed_actions::Extensions, _, _| {
                opened.replace(Some(action.clone()));
            });
        });
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("photon-set-up-language-server")
            .expect("item is drawn");
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            opened.borrow().clone(),
            Some(zed_actions::Extensions {
                category_filter: None,
                id: Some("java".to_string()),
            })
        );
    }
}
