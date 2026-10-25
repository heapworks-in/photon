use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::HashSet;
use db::kvp::KeyValueStore;
use db::sqlez::statement::Statement;
use fs::MTime;
use futures::channel::oneshot;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FutureExt as _, Keystroke, Render,
    Subscription, Task, WeakEntity, Window, div,
};
use language::{Buffer, BufferEvent};
use project::{Project, ProjectItem as _, ProjectPath};
use serde::{Deserialize, Serialize};
use theme::GlobalTheme;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    ItemId, Pane, Workspace, WorkspaceId,
    item::{Item, ItemBufferKind, ItemEvent, ProjectItem, SaveOptions, SerializableItem},
};

use crate::{Appearance, FromPage, ToPage, WebDocument, forwarded_shortcuts};

const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Serialize, Deserialize)]
struct SerializedTab {
    path: PathBuf,
    unsaved_text: Option<String>,
    saved_mtime: Option<MTime>,
}

pub trait PageConnection {
    fn send(&self, message: ToPage);
}

pub enum WebEditorEvent {
    Edited,
    DirtyChanged,
    TitleChanged,
}

pub struct WebEditorView {
    document: Entity<WebDocument>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    page: Option<Rc<dyn PageConnection>>,
    // The text the page is showing, so its own changes are not echoed back as a `Load`.
    page_text: Option<String>,
    next_flush_request: u64,
    pending_flushes: Vec<(u64, oneshot::Sender<()>)>,
    error: Option<SharedString>,
    #[cfg(target_os = "macos")]
    pub(crate) native: crate::native::NativeHost,
    _subscriptions: Vec<Subscription>,
}

impl WebEditorView {
    pub fn new(
        document: Entity<WebDocument>,
        project: Entity<Project>,
        #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = document.read(cx).buffer().clone();
        let focus_handle = cx.focus_handle();
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut subscriptions = vec![
            cx.subscribe(&buffer, Self::on_buffer_event),
            cx.observe_global::<GlobalTheme>(|this, cx| {
                this.send(ToPage::Theme {
                    appearance: appearance(cx),
                })
            }),
        ];
        #[cfg(target_os = "macos")]
        subscriptions.extend(crate::native::subscribe(&focus_handle, window, cx));
        Self {
            document,
            project,
            focus_handle,
            page: None,
            page_text: None,
            next_flush_request: 0,
            pending_flushes: Vec::new(),
            error: None,
            #[cfg(target_os = "macos")]
            native: crate::native::NativeHost::new(cx),
            _subscriptions: subscriptions,
        }
    }

    pub fn document(&self) -> &Entity<WebDocument> {
        &self.document
    }

    pub fn set_page(&mut self, page: Option<Rc<dyn PageConnection>>) {
        self.page = page;
        self.page_text = None;
        // Dropping the senders lets pending saves go ahead with what the buffer has.
        self.pending_flushes.clear();
    }

    pub fn handle_page_message(
        &mut self,
        message: FromPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match message {
            FromPage::Ready { .. } => self.load_page(cx),
            FromPage::Changed { text } => {
                // After an error the page may hold a half-read scene; writing it back could destroy the file.
                if self.error.is_some() {
                    log::warn!("ignoring a change from a web editor page that reported an error");
                    return;
                }
                let buffer = self.buffer(cx);
                if buffer.read(cx).text() != text {
                    self.page_text = Some(text.clone());
                    buffer.update(cx, |buffer, cx| {
                        buffer.set_text(text, cx);
                    });
                }
            }
            FromPage::Flushed { request } => {
                if let Some(index) = self
                    .pending_flushes
                    .iter()
                    .position(|(pending, _)| *pending == request)
                {
                    let (_, sender) = self.pending_flushes.swap_remove(index);
                    // Fails only when the save already gave up waiting, which needs no handling.
                    sender.send(()).ok();
                }
            }
            FromPage::Error { message } => {
                self.error = Some(message.into());
                cx.notify();
            }
            FromPage::Shortcut { keystroke } => {
                // The page is extension code: only the forwarded shortcuts may reach Photon's keymap.
                if !forwarded_shortcuts().contains(&keystroke) {
                    log::warn!(
                        "web editor page sent a shortcut that is not forwarded: {keystroke}"
                    );
                    return;
                }
                let Some(keystroke) =
                    Keystroke::parse_with_os(&keystroke, cx.platform_info().os).log_err()
                else {
                    return;
                };
                // Deferred so the action's handlers can update this view.
                window.defer(cx, move |window, cx| {
                    window.dispatch_keystroke(keystroke, cx);
                });
            }
            FromPage::LibraryChanged { text } => {
                let Some(path) = self.library_path(cx) else {
                    return;
                };
                let fs = self.project.read(cx).fs().clone();
                cx.background_spawn(async move {
                    if let Some(directory) = path.parent() {
                        fs.create_dir(directory).await?;
                    }
                    fs.atomic_write(path, text).await
                })
                .detach_and_log_err(cx);
            }
        }
    }

    fn buffer(&self, cx: &App) -> Entity<Buffer> {
        self.document.read(cx).buffer().clone()
    }

    fn send(&self, message: ToPage) {
        if let Some(page) = &self.page {
            page.send(message);
        }
    }

    fn load_page(&mut self, cx: &mut Context<Self>) {
        if self.page.is_none() {
            return;
        }
        self.send_load(self.buffer(cx).read(cx).text(), cx);
        self.send(ToPage::Theme {
            appearance: appearance(cx),
        });

        let Some(path) = self.library_path(cx) else {
            return;
        };
        let fs = self.project.read(cx).fs().clone();
        cx.spawn(async move |this, cx| {
            if !fs.is_file(&path).await {
                return Ok(());
            }
            let text = fs.load(&path).await?;
            this.update(cx, |this, _| this.send(ToPage::Library { text }))
        })
        .detach_and_log_err(cx);
    }

    fn send_load(&mut self, text: String, cx: &mut Context<Self>) {
        let file_name = self.tab_content_text(0, cx).to_string();
        self.page_text = Some(text.clone());
        if self.error.take().is_some() {
            cx.notify();
        }
        self.send(ToPage::Load {
            text,
            file_name,
            shortcuts: forwarded_shortcuts(),
        });
    }

    fn on_buffer_event(
        &mut self,
        buffer: Entity<Buffer>,
        event: &BufferEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            BufferEvent::Edited { .. } => {
                if self.page.is_some() {
                    let text = buffer.read(cx).text();
                    if self.page_text.as_ref() != Some(&text) {
                        self.send_load(text, cx);
                    }
                }
                cx.emit(WebEditorEvent::Edited);
            }
            BufferEvent::DirtyChanged => cx.emit(WebEditorEvent::DirtyChanged),
            BufferEvent::Saved | BufferEvent::FileHandleChanged | BufferEvent::Reloaded => {
                cx.emit(WebEditorEvent::TitleChanged)
            }
            _ => {}
        }
    }

    fn library_path(&self, cx: &App) -> Option<PathBuf> {
        let editor_id = self.document.read(cx).editor_id();
        // Editor ids come from extension manifests; keep them from naming paths outside the data dir.
        let mut components = Path::new(editor_id.as_ref()).components();
        if !matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        ) {
            log::warn!("web editor id {editor_id:?} is not a plain name; its library is not kept");
            return None;
        }
        Some(
            paths::data_dir()
                .join("web_editors")
                .join(editor_id.as_ref())
                .join("library.json"),
        )
    }

    fn flush_page(&mut self, cx: &mut Context<Self>) -> Task<()> {
        if self.page.is_none() {
            return Task::ready(());
        }
        self.next_flush_request += 1;
        let request = self.next_flush_request;
        let (sender, receiver) = oneshot::channel();
        self.pending_flushes.push((request, sender));
        self.send(ToPage::Flush { request });
        cx.spawn(async move |this, cx| {
            if receiver
                .with_timeout(FLUSH_TIMEOUT, cx.background_executor())
                .await
                .is_err()
            {
                log::warn!(
                    "web editor page did not flush within {FLUSH_TIMEOUT:?}; saving what it has"
                );
                this.update(cx, |this, _| {
                    this.pending_flushes
                        .retain(|(pending, _)| *pending != request)
                })
                .log_err();
            }
        })
    }

    fn abs_path(&self, cx: &App) -> Option<PathBuf> {
        let project_path = self.document.read(cx).project_path(cx)?;
        self.project.read(cx).absolute_path(&project_path, cx)
    }
}

fn appearance(cx: &App) -> Appearance {
    match cx.theme().appearance {
        theme::Appearance::Light => Appearance::Light,
        theme::Appearance::Dark => Appearance::Dark,
    }
}

fn has_deleted_file(buffer: &Buffer) -> bool {
    buffer
        .file()
        .is_some_and(|file| file.disk_state().is_deleted())
}

fn items_namespace(workspace_id: WorkspaceId) -> String {
    format!("web_editor_items/{}", i64::from(workspace_id))
}

impl EventEmitter<WebEditorEvent> for WebEditorView {}

impl Focusable for WebEditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WebEditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .bg(cx.theme().colors().editor_background)
            .map(|this| {
                #[cfg(target_os = "macos")]
                return this.child(crate::native::element(self, cx));
                #[cfg(not(target_os = "macos"))]
                this.p_4().child(Label::new("Web editors need macOS"))
            })
            .when_some(self.error.clone(), |this, error| {
                this.p_4().child(Label::new(error).color(Color::Error))
            })
    }
}

impl Item for WebEditorView {
    type Event = WebEditorEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            WebEditorEvent::Edited => f(ItemEvent::Edit),
            WebEditorEvent::DirtyChanged | WebEditorEvent::TitleChanged => {
                f(ItemEvent::UpdateTab);
                f(ItemEvent::UpdateBreadcrumbs);
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::native::hidden(self, window, cx);
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.document.read(cx).buffer().read(cx).file() {
            Some(file) => file.file_name(cx).to_string().into(),
            None => "untitled".into(),
        }
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.abs_path(cx)?.to_string_lossy().into_owned().into())
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        let buffer = self.document.read(cx).buffer();
        f(buffer.entity_id(), buffer.read(cx))
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let document = self.document.clone();
        let project = self.project.clone();
        Task::ready(Some(
            cx.new(|cx| WebEditorView::new(document, project, window, cx)),
        ))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffer(cx).read(cx).is_dirty()
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.buffer(cx).read(cx).has_conflict()
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        has_deleted_file(self.buffer(cx).read(cx))
    }

    fn can_save(&self, cx: &App) -> bool {
        self.document.read(cx).project_path(cx).is_some()
    }

    fn can_save_as(&self, _cx: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let flushed = self.flush_page(cx);
        let buffer = self.buffer(cx);
        cx.spawn_in(window, async move |_, cx| {
            flushed.await;
            let needs_write = buffer.read_with(cx, |buffer, _| {
                buffer.is_dirty() || has_deleted_file(buffer)
            });
            if needs_write {
                project
                    .update(cx, |project, cx| project.save_buffer(buffer, cx))
                    .await?;
            }
            Ok(())
        })
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let flushed = self.flush_page(cx);
        let buffer = self.buffer(cx);
        cx.spawn_in(window, async move |_, cx| {
            flushed.await;
            project
                .update(cx, |project, cx| project.save_buffer_as(buffer, path, cx))
                .await
        })
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffers = HashSet::from_iter([self.buffer(cx)]);
        let reload = project.update(cx, |project, cx| project.reload_buffers(buffers, true, cx));
        cx.background_spawn(async move {
            reload.await?;
            Ok(())
        })
    }
}

impl ProjectItem for WebEditorView {
    type Item = WebDocument;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, project, window, cx)
    }
}

impl SerializableItem for WebEditorView {
    fn serialized_item_kind() -> &'static str {
        "WebEditorView"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let store = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            let placeholders = vec!["?"; alive_items.len()].join(", ");
            let query = format!(
                "DELETE FROM scoped_kv_store WHERE namespace = ? AND key NOT IN ({placeholders})"
            );
            store
                .write(move |connection| {
                    let mut statement = Statement::prepare(connection, query)?;
                    let mut next_index = statement.bind(&items_namespace(workspace_id), 1)?;
                    for item_id in alive_items {
                        next_index = statement.bind(&item_id.to_string(), next_index)?;
                    }
                    statement.exec()
                })
                .await
        })
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let store = KeyValueStore::global(cx);
        window.spawn(cx, async move |cx| {
            let tab = store
                .scoped(&items_namespace(workspace_id))
                .read(&item_id.to_string())?
                .context("nothing stored for this web editor tab")?;
            let tab: SerializedTab = serde_json::from_str(&tab)?;
            let abs_path = tab.path;

            let (worktree, path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(&abs_path, false, cx)
                })
                .await?;
            let project_path = ProjectPath {
                worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
                path,
            };
            let editor_id = cx
                .update(|_, cx| WebDocument::editor_id_for_path(&project, &project_path, cx))?
                .with_context(|| format!("no web editor is installed for {abs_path:?}"))?;
            let document = cx
                .update(|_, cx| WebDocument::open(project.clone(), project_path, editor_id, cx))?
                .await?;
            if let Some(unsaved_text) = tab.unsaved_text {
                let buffer = document.read_with(cx, |document, _| document.buffer().clone());
                // Same as Editor's restore: start from the mtime the text was based on, so a file
                // changed while Photon was closed shows as a conflict instead of being overwritten.
                buffer.update(cx, |buffer, cx| {
                    if tab.saved_mtime.is_some() {
                        buffer.did_reload(
                            buffer.version(),
                            buffer.line_ending(),
                            tab.saved_mtime,
                            cx,
                        );
                    }
                    buffer.set_text(unsaved_text, cx);
                    if let Some(entry) = buffer.peek_undo_stack() {
                        buffer.forget_transaction(entry.transaction_id());
                    }
                });
            }
            cx.update(|window, cx| cx.new(|cx| WebEditorView::new(document, project, window, cx)))
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let path = self.abs_path(cx)?;
        let buffer = self.buffer(cx);
        let buffer = buffer.read(cx);
        let unsaved_text = buffer.is_dirty().then(|| buffer.text());
        let saved_mtime = buffer.saved_mtime();
        let store = KeyValueStore::global(cx);
        Some(cx.background_spawn(async move {
            let tab = serde_json::to_string(&SerializedTab {
                path,
                unsaved_text,
                saved_mtime,
            })?;
            store
                .scoped(&items_namespace(workspace_id))
                .write(item_id.to_string(), tab)
                .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, WebEditorEvent::Edited | WebEditorEvent::DirtyChanged)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::Duration;

    use fs::{FakeFs, Fs as _};
    use gpui::{Entity, KeyBinding, TestAppContext, VisualTestContext};
    use project::{Project, ProjectPath};
    use settings::SettingsStore;
    use util::rel_path::rel_path;
    use workspace::{MultiWorkspace, item::SaveOptions};

    use super::*;
    use crate::{
        Appearance, FromPage, ToPage, WebDocument, WebEditor, WebEditorRegistry, WebEditorSource,
        forwarded_shortcuts,
    };

    #[derive(Default)]
    struct FakePage {
        sent: RefCell<Vec<ToPage>>,
    }

    impl PageConnection for FakePage {
        fn send(&self, message: ToPage) {
            self.sent.borrow_mut().push(message);
        }
    }

    impl FakePage {
        fn take(&self) -> Vec<ToPage> {
            std::mem::take(&mut *self.sent.borrow_mut())
        }
    }

    const FILE: &str = "/root/a.excalidraw";

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let mut registry = WebEditorRegistry::default();
            registry.register(WebEditor {
                id: "excalidraw".into(),
                extension_id: "photon".into(),
                name: "Excalidraw".to_string(),
                file_suffixes: vec!["excalidraw".to_string()],
                entry: "index.html".to_string(),
                source: WebEditorSource::Embedded {
                    prefix: "web_editors/excalidraw".to_string(),
                },
                built_in: true,
            });
            cx.set_global(registry);
        });
    }

    async fn project_with(
        contents: &str,
        cx: &mut TestAppContext,
    ) -> (std::sync::Arc<FakeFs>, Entity<Project>) {
        init(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({ "a.excalidraw": contents, "notes.txt": "hi" }),
        )
        .await;
        let project = Project::test(fs.clone(), [Path::new("/root")], cx).await;
        (fs, project)
    }

    fn project_path(project: &Entity<Project>, name: &str, cx: &TestAppContext) -> ProjectPath {
        cx.read(|cx| {
            let worktree = project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("test project has a worktree");
            ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: rel_path(name).into(),
            }
        })
    }

    async fn open_document(
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Entity<WebDocument> {
        let path = project_path(project, "a.excalidraw", cx);
        cx.update(|cx| WebDocument::try_open(project, &path, cx))
            .expect("a.excalidraw opens as a web document")
            .await
            .expect("document opens")
    }

    fn connect(view: &Entity<WebEditorView>, cx: &mut VisualTestContext) -> Rc<FakePage> {
        let page = Rc::new(FakePage::default());
        view.update(cx, |view, _| view.set_page(Some(page.clone())));
        page
    }

    fn send(view: &Entity<WebEditorView>, message: FromPage, cx: &mut VisualTestContext) {
        view.update_in(cx, |view, window, cx| {
            view.handle_page_message(message, window, cx)
        });
        cx.run_until_parked();
    }

    fn ready() -> FromPage {
        FromPage::Ready { version: 1 }
    }

    fn changed(text: &str) -> FromPage {
        FromPage::Changed {
            text: text.to_string(),
        }
    }

    fn load(text: &str) -> ToPage {
        ToPage::Load {
            text: text.to_string(),
            file_name: "a.excalidraw".to_string(),
            shortcuts: forwarded_shortcuts(),
        }
    }

    fn theme(cx: &mut VisualTestContext) -> ToPage {
        let appearance = cx.update(|_, cx| match cx.theme().appearance {
            theme::Appearance::Light => Appearance::Light,
            theme::Appearance::Dark => Appearance::Dark,
        });
        ToPage::Theme { appearance }
    }

    async fn open_view<'a>(
        contents: &str,
        cx: &'a mut TestAppContext,
    ) -> (
        std::sync::Arc<FakeFs>,
        Entity<Project>,
        Entity<WebEditorView>,
        Rc<FakePage>,
        &'a mut VisualTestContext,
    ) {
        let (fs, project) = project_with(contents, cx).await;
        let document = open_document(&project, cx).await;
        let (view, cx) = cx.add_window_view({
            let project = project.clone();
            |window, cx| WebEditorView::new(document, project, window, cx)
        });
        let page = connect(&view, cx);
        (fs, project, view, page, cx)
    }

    async fn save(
        view: &Entity<WebEditorView>,
        project: &Entity<Project>,
        cx: &mut VisualTestContext,
    ) {
        view.update_in(cx, |view, window, cx| {
            view.save(SaveOptions::default(), project.clone(), window, cx)
        })
        .await
        .expect("save succeeds");
    }

    async fn file_text(fs: &FakeFs, path: &str) -> String {
        fs.load(Path::new(path)).await.expect("file exists")
    }

    #[gpui::test]
    async fn non_matching_paths_are_not_web_documents(cx: &mut TestAppContext) {
        let (_fs, project) = project_with("{}", cx).await;
        let path = project_path(&project, "notes.txt", cx);
        assert!(
            cx.update(|cx| WebDocument::try_open(&project, &path, cx))
                .is_none()
        );
    }

    #[gpui::test]
    async fn ready_loads_the_file_and_theme(cx: &mut TestAppContext) {
        let (_fs, _project, view, page, cx) = open_view("{\"a\":1}", cx).await;
        send(&view, ready(), cx);
        assert_eq!(page.take(), vec![load("{\"a\":1}"), theme(cx)]);
        view.read_with(cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "a.excalidraw");
        });
    }

    #[gpui::test]
    async fn changes_make_the_tab_dirty_and_save_writes_them(cx: &mut TestAppContext) {
        let (fs, project, view, page, cx) = open_view("{}", cx).await;
        send(&view, ready(), cx);
        page.take();

        send(&view, changed("{\"b\":2}"), cx);
        assert!(view.read_with(cx, |view, cx| view.is_dirty(cx)));
        assert_eq!(
            page.take(),
            vec![],
            "the page's own change is not echoed back"
        );

        let saving = view.update_in(cx, |view, window, cx| {
            view.save(SaveOptions::default(), project.clone(), window, cx)
        });
        let ToPage::Flush { request } = page.take().pop().expect("flush sent") else {
            panic!("expected a flush");
        };
        send(&view, FromPage::Flushed { request }, cx);
        saving.await.expect("save succeeds");

        assert_eq!(file_text(&fs, FILE).await, "{\"b\":2}");
        assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));
    }

    #[gpui::test]
    async fn unchanged_scene_is_never_written(cx: &mut TestAppContext) {
        let (fs, project, view, _page, cx) = open_view("{}", cx).await;
        view.update(cx, |view, _| view.set_page(None));
        send(&view, changed("{}"), cx);
        assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));

        let writes = fs.write_count_for_path(FILE);
        save(&view, &project, cx).await;
        assert_eq!(fs.write_count_for_path(FILE), writes);
    }

    #[gpui::test]
    async fn save_flushes_the_page_first(cx: &mut TestAppContext) {
        let (fs, project, view, page, cx) = open_view("{}", cx).await;
        send(&view, ready(), cx);
        send(&view, changed("first"), cx);
        page.take();

        let saving = view.update_in(cx, |view, window, cx| {
            view.save(SaveOptions::default(), project.clone(), window, cx)
        });
        cx.run_until_parked();
        let sent = page.take();
        let [ToPage::Flush { request }] = sent.as_slice() else {
            panic!("expected only a flush, got {sent:?}");
        };
        assert_eq!(
            file_text(&fs, FILE).await,
            "{}",
            "nothing written before the flush"
        );

        send(&view, changed("latest"), cx);
        send(&view, FromPage::Flushed { request: *request }, cx);
        saving.await.expect("save succeeds");
        assert_eq!(file_text(&fs, FILE).await, "latest");
    }

    #[gpui::test]
    async fn save_without_a_flush_reply_times_out_and_saves(cx: &mut TestAppContext) {
        let (fs, project, view, _page, cx) = open_view("{}", cx).await;
        send(&view, changed("pending"), cx);
        let saving = view.update_in(cx, |view, window, cx| {
            view.save(SaveOptions::default(), project.clone(), window, cx)
        });
        cx.background_executor
            .timer(Duration::from_millis(600))
            .await;
        saving.await.expect("save succeeds");
        assert_eq!(file_text(&fs, FILE).await, "pending");
    }

    #[gpui::test]
    async fn disk_change_reloads_a_clean_tab(cx: &mut TestAppContext) {
        let (fs, _project, view, page, cx) = open_view("{}", cx).await;
        send(&view, ready(), cx);
        page.take();

        fs.insert_file(FILE, b"from disk".to_vec()).await;
        cx.run_until_parked();

        assert_eq!(page.take(), vec![load("from disk")]);
        assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));
    }

    #[gpui::test]
    async fn disk_change_on_a_dirty_tab_keeps_edits(cx: &mut TestAppContext) {
        let (fs, _project, view, page, cx) = open_view("{}", cx).await;
        send(&view, ready(), cx);
        send(&view, changed("mine"), cx);
        page.take();

        fs.insert_file(FILE, b"theirs".to_vec()).await;
        cx.run_until_parked();

        assert_eq!(page.take(), vec![], "the page keeps the unsaved drawing");
        view.read_with(cx, |view, cx| {
            assert!(view.is_dirty(cx));
            assert!(view.has_conflict(cx));
            assert_eq!(view.document.read(cx).buffer().read(cx).text(), "mine");
        });
        assert_eq!(file_text(&fs, FILE).await, "theirs");
    }

    #[gpui::test]
    async fn two_tabs_share_one_document(cx: &mut TestAppContext) {
        let (_fs, project, first, first_page, cx) = open_view("{}", cx).await;
        let second = first
            .update_in(cx, |view, window, cx| view.clone_on_split(None, window, cx))
            .await
            .expect("web editor tabs split");
        let second_page = connect(&second, cx);
        send(&first, ready(), cx);
        send(&second, ready(), cx);
        first_page.take();
        second_page.take();

        send(&first, changed("from first"), cx);
        assert_eq!(first_page.take(), vec![]);
        assert_eq!(second_page.take(), vec![load("from first")]);

        send(&second, changed("from second"), cx);
        assert_eq!(first_page.take(), vec![load("from second")]);
        assert_eq!(second_page.take(), vec![]);

        cx.read(|cx| {
            assert!(first.read(cx).is_dirty(cx));
            assert!(second.read(cx).is_dirty(cx));
            assert_eq!(
                first.read(cx).document.read(cx).buffer().entity_id(),
                second.read(cx).document.read(cx).buffer().entity_id()
            );
        });
        drop(project);
    }

    async fn workspace_with_id<'a>(
        project: &Entity<Project>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Workspace>, WorkspaceId, &'a mut VisualTestContext) {
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let workspace_id = workspace
            .read_with(cx, |workspace, _| workspace.database_id())
            .expect("workspace has an id");
        (workspace, workspace_id, cx)
    }

    fn another_workspace(
        project: &Entity<Project>,
        cx: &mut VisualTestContext,
    ) -> (Entity<Workspace>, WorkspaceId) {
        let workspace =
            cx.new_window_entity(|window, cx| Workspace::test_new(project.clone(), window, cx));
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let workspace_id = workspace
            .read_with(cx, |workspace, _| workspace.database_id())
            .expect("workspace has an id");
        (workspace, workspace_id)
    }

    async fn serialize_view(
        workspace: &Entity<Workspace>,
        view: &Entity<WebEditorView>,
        item_id: ItemId,
        cx: &mut VisualTestContext,
    ) {
        workspace
            .update(cx, |workspace, cx| {
                view.update(cx, |view, cx| view.serialize(workspace, item_id, false, cx))
            })
            .expect("tab serializes")
            .await
            .expect("serialized");
    }

    async fn restore_view(
        workspace: &Entity<Workspace>,
        project: &Entity<Project>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        cx: &mut VisualTestContext,
    ) -> Entity<WebEditorView> {
        workspace
            .update_in(cx, |workspace, window, cx| {
                WebEditorView::deserialize(
                    project.clone(),
                    workspace.weak_handle(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("tab restores")
    }

    async fn dirty_view_serialized(
        text: &str,
        project: &Entity<Project>,
        workspace: &Entity<Workspace>,
        item_id: ItemId,
        cx: &mut VisualTestContext,
    ) {
        let document = open_document(project, cx).await;
        let view = cx.new_window_entity(|window, cx| {
            WebEditorView::new(document, project.clone(), window, cx)
        });
        send(&view, changed(text), cx);
        serialize_view(workspace, &view, item_id, cx).await;
    }

    #[gpui::test]
    async fn unsaved_edits_survive_a_restart(cx: &mut TestAppContext) {
        let (fs, project) = project_with("{}", cx).await;
        let (workspace, workspace_id, cx) = workspace_with_id(&project, cx).await;
        dirty_view_serialized("unsaved", &project, &workspace, 42, cx).await;

        let project = Project::test(fs.clone(), [Path::new("/root")], cx).await;
        let restored = restore_view(&workspace, &project, workspace_id, 42, cx).await;
        restored.read_with(cx, |view, cx| {
            assert!(view.is_dirty(cx));
            assert!(!view.has_conflict(cx));
            assert_eq!(view.document.read(cx).buffer().read(cx).text(), "unsaved");
        });
        assert_eq!(file_text(&fs, FILE).await, "{}");
    }

    #[gpui::test]
    async fn unsaved_text_is_kept_per_workspace(cx: &mut TestAppContext) {
        let (fs, project) = project_with("{}", cx).await;
        let (first_workspace, first_id, cx) = workspace_with_id(&project, cx).await;
        dirty_view_serialized("first window", &project, &first_workspace, 1, cx).await;

        let second_project = Project::test(fs.clone(), [Path::new("/root")], cx).await;
        let (second_workspace, second_id) = another_workspace(&second_project, cx);
        let document = open_document(&second_project, cx).await;
        let clean = cx.new_window_entity(|window, cx| {
            WebEditorView::new(document, second_project.clone(), window, cx)
        });
        serialize_view(&second_workspace, &clean, 2, cx).await;

        let project = Project::test(fs.clone(), [Path::new("/root")], cx).await;
        let restored_first = restore_view(&first_workspace, &project, first_id, 1, cx).await;
        restored_first.read_with(cx, |view, cx| {
            assert_eq!(
                view.document.read(cx).buffer().read(cx).text(),
                "first window"
            );
        });
        let project = Project::test(fs.clone(), [Path::new("/root")], cx).await;
        let restored_second = restore_view(&second_workspace, &project, second_id, 2, cx).await;
        restored_second.read_with(cx, |view, cx| {
            assert!(!view.is_dirty(cx));
            assert_eq!(view.document.read(cx).buffer().read(cx).text(), "{}");
        });
    }

    #[gpui::test]
    async fn disk_change_while_closed_conflicts_on_restore(cx: &mut TestAppContext) {
        let (fs, project) = project_with("{}", cx).await;
        let (workspace, workspace_id, cx) = workspace_with_id(&project, cx).await;
        dirty_view_serialized("unsaved", &project, &workspace, 42, cx).await;
        drop(project);

        fs.insert_file(FILE, b"changed while closed".to_vec()).await;
        cx.run_until_parked();

        let project = Project::test(fs.clone(), [Path::new("/root")], cx).await;
        let (restart_workspace, _) = another_workspace(&project, cx);
        let restored = restore_view(&restart_workspace, &project, workspace_id, 42, cx).await;
        restored.read_with(cx, |view, cx| {
            assert!(view.is_dirty(cx));
            assert!(view.has_conflict(cx));
            assert_eq!(view.document.read(cx).buffer().read(cx).text(), "unsaved");
        });

        restart_workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(restored.clone()), None, true, window, cx)
        });
        let saving = restart_workspace.update_in(cx, |workspace, window, cx| {
            workspace.save_active_item(workspace::SaveIntent::Save, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "a conflicting save asks first");
        assert_eq!(file_text(&fs, FILE).await, "changed while closed");
        cx.simulate_prompt_answer("Cancel");
        saving.await.ok();
        assert_eq!(file_text(&fs, FILE).await, "changed while closed");
    }

    #[gpui::test]
    async fn empty_file_loads_as_empty_text(cx: &mut TestAppContext) {
        let (_fs, _project, view, page, cx) = open_view("", cx).await;
        send(&view, ready(), cx);
        assert_eq!(page.take().first(), Some(&load("")));
    }

    #[gpui::test]
    async fn forwarded_shortcut_dispatches_the_action(cx: &mut TestAppContext) {
        let (fs, project) = project_with("{}", cx).await;
        cx.update(|cx| {
            cx.bind_keys([KeyBinding::new(
                "cmd-s",
                workspace::Save { save_intent: None },
                None,
            )])
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let document = open_document(&project, cx).await;
        let view = cx.new_window_entity(|window, cx| {
            WebEditorView::new(document, project.clone(), window, cx)
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx)
        });
        let page = connect(&view, cx);
        send(&view, changed("shortcut"), cx);
        page.take();

        send(
            &view,
            FromPage::Shortcut {
                keystroke: "cmd-s".to_string(),
            },
            cx,
        );
        let sent = page.take();
        let [ToPage::Flush { request }] = sent.as_slice() else {
            panic!("expected cmd-s to start a save, got {sent:?}");
        };
        send(&view, FromPage::Flushed { request: *request }, cx);
        assert_eq!(file_text(&fs, FILE).await, "shortcut");
    }

    #[gpui::test]
    async fn page_error_is_shown_and_nothing_is_written(cx: &mut TestAppContext) {
        let (fs, project, view, page, cx) = open_view("{\"keep\":true}", cx).await;
        send(&view, ready(), cx);
        send(
            &view,
            FromPage::Error {
                message: "Could not read this drawing".to_string(),
            },
            cx,
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.error.as_deref(), Some("Could not read this drawing"));
        });
        send(&view, changed(""), cx);
        assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));
        page.take();

        let writes = fs.write_count_for_path(FILE);
        let saving = view.update_in(cx, |view, window, cx| {
            view.save(SaveOptions::default(), project.clone(), window, cx)
        });
        let ToPage::Flush { request } = page.take().pop().expect("flush sent") else {
            panic!("expected a flush");
        };
        send(&view, FromPage::Flushed { request }, cx);
        saving.await.expect("save succeeds");
        assert_eq!(fs.write_count_for_path(FILE), writes);
        assert_eq!(file_text(&fs, FILE).await, "{\"keep\":true}");

        send(&view, ready(), cx);
        assert!(
            view.read_with(cx, |view, _| view.error.is_none()),
            "a new load clears the error"
        );
    }

    #[gpui::test]
    async fn libraries_persist_between_sessions(cx: &mut TestAppContext) {
        let (fs, project, view, _page, cx) = open_view("{}", cx).await;
        send(&view, ready(), cx);
        send(
            &view,
            FromPage::LibraryChanged {
                text: "[\"shape\"]".to_string(),
            },
            cx,
        );
        let library: PathBuf = paths::data_dir()
            .join("web_editors")
            .join("excalidraw")
            .join("library.json");
        assert_eq!(
            file_text(&fs, library.to_str().expect("utf-8")).await,
            "[\"shape\"]"
        );

        let document = open_document(&project, cx).await;
        let second = cx.new_window_entity(|window, cx| {
            WebEditorView::new(document, project.clone(), window, cx)
        });
        let second_page = connect(&second, cx);
        send(&second, ready(), cx);
        assert_eq!(
            second_page.take(),
            vec![
                load("{}"),
                theme(cx),
                ToPage::Library {
                    text: "[\"shape\"]".to_string()
                }
            ]
        );
    }
}
