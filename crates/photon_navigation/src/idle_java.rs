//! Each project tab runs its own Java server (jdtls), and each one holds the project in memory:
//! ten tabs are ten servers. A tab's Java server is paused once the tab has gone unused for
//! [`PAUSE_AFTER`], and started again when the tab is shown. jdtls keeps its workspace on disk,
//! so it comes back in seconds instead of indexing the project again.

use std::time::Duration;

use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, Context, Entity, EntityId, Task, TaskExt as _, WeakEntity};
use language::LanguageServerName;
use lsp::LanguageServerSelector;
use workspace::{MultiWorkspace, MultiWorkspaceEvent, Workspace};

const PAUSE_AFTER: Duration = Duration::from_secs(10 * 60);
const JAVA_SERVER: LanguageServerName = LanguageServerName::new_static("jdtls");

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|multi_workspace: &mut MultiWorkspace, _, cx| {
        let this = cx.weak_entity();
        let shown = multi_workspace.workspace().clone();
        let pauses = cx.new(|_| JavaPauses {
            multi_workspace: this,
            shown: shown.downgrade(),
            timers: HashMap::default(),
            paused: HashSet::default(),
        });
        // Tabs that open in the background, as when Photon reopens a window, count as left now.
        for workspace in multi_workspace.workspaces() {
            if *workspace != shown {
                pauses.update(cx, |pauses, cx| pauses.left(workspace, cx));
            }
        }
        cx.subscribe_self(move |multi_workspace, event: &MultiWorkspaceEvent, cx| {
            let shown = multi_workspace.workspace().clone();
            pauses.update(cx, |pauses, cx| match event {
                // Not every switch names the tab it replaced, so the last shown tab is kept here.
                MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } => {
                    let previous = std::mem::replace(&mut pauses.shown, shown.downgrade());
                    pauses.shown(&shown, cx);
                    if let Some(previous) = previous.upgrade()
                        && previous != shown
                    {
                        pauses.left(&previous, cx);
                    }
                }
                MultiWorkspaceEvent::WorkspaceAdded(workspace) if *workspace != shown => {
                    pauses.left(workspace, cx)
                }
                MultiWorkspaceEvent::WorkspaceRemoved(id) => {
                    pauses.timers.remove(id);
                    pauses.paused.remove(id);
                }
                _ => {}
            });
        })
        .detach();
    })
    .detach();
}

struct JavaPauses {
    multi_workspace: WeakEntity<MultiWorkspace>,
    shown: WeakEntity<Workspace>,
    /// Running while a tab goes unused; dropped when it's shown again.
    timers: HashMap<EntityId, Task<()>>,
    /// Tabs whose Java server Photon paused, and so starts again when they're shown.
    paused: HashSet<EntityId>,
}

impl JavaPauses {
    fn left(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let id = workspace.entity_id();
        let workspace = workspace.downgrade();
        let timer = cx.background_executor().timer(PAUSE_AFTER);
        self.timers.insert(
            id,
            cx.spawn(async move |this, cx| {
                timer.await;
                this.update(cx, |this, cx| {
                    this.timers.remove(&id);
                    if let Some(workspace) = workspace.upgrade() {
                        this.pause(&workspace, cx);
                    }
                })
                .ok();
            }),
        );
    }

    fn shown(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let id = workspace.entity_id();
        self.timers.remove(&id);
        if !self.paused.remove(&id) {
            return;
        }
        let project = workspace.read(cx).project().clone();
        let buffers = project.read(cx).opened_buffers(cx);
        log::info!("starting the Java server of a project tab shown again");
        project.read(cx).lsp_store().update(cx, |lsp_store, cx| {
            lsp_store.restart_language_servers_for_buffers(
                buffers,
                HashSet::from_iter([LanguageServerSelector::Name(JAVA_SERVER)]),
                cx,
            )
        });
    }

    fn pause(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let still_hidden = self
            .multi_workspace
            .upgrade()
            .is_some_and(|multi_workspace| multi_workspace.read(cx).workspace() != workspace);
        let project = workspace.read(cx).project().clone();
        // A remote project's servers run on its host, which has its own memory to manage.
        if !still_hidden || !project.read(cx).is_local() {
            return;
        }
        let lsp_store = project.read(cx).lsp_store();
        let running = lsp_store
            .read(cx)
            .language_server_statuses()
            .any(|(_, status)| status.name == JAVA_SERVER);
        if !running {
            return;
        }
        log::info!("pausing the Java server of a project tab unused for {PAUSE_AFTER:?}");
        self.paused.insert(workspace.entity_id());
        lsp_store
            .update(cx, |lsp_store, cx| {
                lsp_store.stop_language_servers_for_buffers(
                    Vec::new(),
                    HashSet::from_iter([LanguageServerSelector::Name(JAVA_SERVER)]),
                    cx,
                )
            })
            .detach_and_log_err(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
    use project::{FakeFs, Project};
    use serde_json::json;
    use std::sync::Arc;
    use util::path;
    use workspace::{AppState, MultiWorkspace, Workspace};

    struct Tab {
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        servers: futures::channel::mpsc::UnboundedReceiver<lsp::FakeLanguageServer>,
        _buffer: Entity<language::Buffer>,
        _handle: project::lsp_store::OpenLspBufferHandle,
    }

    fn java() -> Arc<Language> {
        Arc::new(Language::new(
            LanguageConfig {
                name: "Java".into(),
                matcher: Arc::new(LanguageMatcher {
                    path_suffixes: vec!["java".into()],
                    ..LanguageMatcher::default()
                }),
                ..LanguageConfig::default()
            },
            None,
        ))
    }

    async fn java_project(root: &str, cx: &mut TestAppContext) -> (Entity<Project>, futures::channel::mpsc::UnboundedReceiver<lsp::FakeLanguageServer>) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(root, json!({ "App.java": "class App { void run() {} }\n" }))
            .await;
        let project = Project::test(fs, [root.as_ref()], cx).await;
        let servers = project.read_with(cx, |project, _| {
            project.languages().add(java());
            project.languages().register_fake_lsp(
                "Java",
                FakeLspAdapter {
                    name: "jdtls",
                    ..FakeLspAdapter::default()
                },
            )
        });
        (project, servers)
    }

    async fn open_java_file(
        project: &Entity<Project>,
        root: &str,
        cx: &mut VisualTestContext,
    ) -> (Entity<language::Buffer>, project::lsp_store::OpenLspBufferHandle) {
        let path = format!("{root}/App.java");
        project
            .update(cx, |project, cx| project.open_local_buffer_with_lsp(path, cx))
            .await
            .unwrap()
    }

    /// Two project tabs, each with a Java file open and its own Java server; the first is shown.
    async fn two_tabs(
        cx: &mut TestAppContext,
    ) -> (Entity<MultiWorkspace>, Tab, Tab, VisualTestContext) {
        cx.update(|cx| {
            let _state = AppState::test(cx);
            init(cx);
        });
        let (project_a, servers_a) = java_project(path!("/a"), cx).await;
        let (project_b, servers_b) = java_project(path!("/b"), cx).await;
        let (multi_workspace, cx) = cx
            .add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
        let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
            mw.test_add_workspace(project_b.clone(), window, cx)
        });
        let (buffer_a, handle_a) = open_java_file(&project_a, path!("/a"), cx).await;
        let (buffer_b, handle_b) = open_java_file(&project_b, path!("/b"), cx).await;
        show(&multi_workspace, &workspace_a, cx);
        let mut a = Tab {
            workspace: workspace_a,
            project: project_a,
            servers: servers_a,
            _buffer: buffer_a,
            _handle: handle_a,
        };
        let mut b = Tab {
            workspace: workspace_b,
            project: project_b,
            servers: servers_b,
            _buffer: buffer_b,
            _handle: handle_b,
        };
        a.servers.next().await.expect("tab a's Java server starts");
        b.servers.next().await.expect("tab b's Java server starts");
        cx.run_until_parked();
        (multi_workspace, a, b, cx.clone())
    }

    fn show(multi_workspace: &Entity<MultiWorkspace>, tab: &Entity<Workspace>, cx: &mut VisualTestContext) {
        multi_workspace.update_in(cx, |mw, window, cx| {
            let source = Some(mw.workspace().downgrade());
            mw.activate(tab.clone(), source, window, cx)
        });
        cx.run_until_parked();
    }

    fn java_server_running(tab: &Tab, cx: &mut VisualTestContext) -> bool {
        tab.project.read_with(cx, |project, cx| {
            project
                .lsp_store()
                .read(cx)
                .language_server_statuses()
                .any(|(_, status)| status.name.0 == "jdtls")
        })
    }

    /// Past `duration`, plus the few seconds a server is given to shut down.
    fn wait(duration: Duration, cx: &mut VisualTestContext) {
        cx.executor().advance_clock(duration);
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn an_unused_tab_pauses_its_java_server_and_resumes_when_shown(
        cx: &mut TestAppContext,
    ) {
        let (multi_workspace, mut a, b, mut cx) = two_tabs(cx).await;
        show(&multi_workspace, &b.workspace, &mut cx);

        wait(PAUSE_AFTER - Duration::from_secs(60), &mut cx);
        assert!(java_server_running(&a, &mut cx), "not yet");

        wait(Duration::from_secs(60), &mut cx);
        assert!(!java_server_running(&a, &mut cx), "paused once unused for long enough");
        assert!(java_server_running(&b, &mut cx), "the tab in use keeps its server");

        show(&multi_workspace, &a.workspace, &mut cx);
        assert!(a.servers.next().await.is_some(), "a new Java server starts");
        cx.run_until_parked();
        assert!(java_server_running(&a, &mut cx));
    }

    #[gpui::test]
    async fn a_tab_left_by_opening_another_project_is_paused(cx: &mut TestAppContext) {
        // Opening a project into the window shows its tab without naming the one it replaced.
        let (multi_workspace, a, b, mut cx) = two_tabs(cx).await;
        multi_workspace.update_in(&mut cx, |mw, window, cx| {
            mw.activate(b.workspace.clone(), None, window, cx)
        });
        cx.run_until_parked();

        wait(PAUSE_AFTER, &mut cx);
        assert!(!java_server_running(&a, &mut cx));
        assert!(java_server_running(&b, &mut cx));
    }

    #[gpui::test]
    async fn coming_back_in_time_keeps_the_server(cx: &mut TestAppContext) {
        let (multi_workspace, a, b, mut cx) = two_tabs(cx).await;
        show(&multi_workspace, &b.workspace, &mut cx);
        wait(PAUSE_AFTER / 2, &mut cx);
        show(&multi_workspace, &a.workspace, &mut cx);

        wait(PAUSE_AFTER, &mut cx);
        assert!(java_server_running(&a, &mut cx));
    }

    #[gpui::test]
    async fn a_server_stopped_by_hand_stays_stopped(cx: &mut TestAppContext) {
        let (multi_workspace, a, b, mut cx) = two_tabs(cx).await;
        a.project.update(&mut cx, |project, cx| {
            project.lsp_store().update(cx, |lsp_store, cx| {
                lsp_store
                    .stop_language_servers_for_buffers(
                        Vec::new(),
                        [lsp::LanguageServerSelector::Name("jdtls".into())]
                            .into_iter()
                            .collect(),
                        cx,
                    )
                    .detach()
            })
        });
        wait(Duration::ZERO, &mut cx);
        assert!(!java_server_running(&a, &mut cx));

        show(&multi_workspace, &b.workspace, &mut cx);
        wait(PAUSE_AFTER, &mut cx);
        show(&multi_workspace, &a.workspace, &mut cx);
        wait(Duration::ZERO, &mut cx);
        assert!(!java_server_running(&a, &mut cx), "Photon only resumes what it paused");
    }

    #[gpui::test]
    async fn a_tab_restored_in_the_background_is_paused_too(cx: &mut TestAppContext) {
        // Tab b was added without ever being shown, as tabs are when Photon reopens a window.
        let (_multi_workspace, a, b, mut cx) = two_tabs(cx).await;
        wait(PAUSE_AFTER, &mut cx);
        assert!(!java_server_running(&b, &mut cx));
        assert!(java_server_running(&a, &mut cx));
    }
}
