use call_hierarchy::{Call, CallHierarchyMode, CallHierarchyView};
use collections::HashMap;
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{
    Action, App, AsyncWindowContext, Bounds, ClickEvent, ClipboardItem, Context, DismissEvent,
    Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Global, KeyContext, MouseDownEvent,
    Pixels, Point, Render, ScrollHandle, ScrollStrategy, StyledText, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, anchored, deferred, point, px, size,
    uniform_list,
};
use language::{
    Buffer, LanguageServerName, PLAIN_TEXT, PointUtf16, ToPoint as _, ToPointUtf16 as _,
};
use project::{
    CallHierarchyItem, Project, ProjectPath, Symbol,
    lsp_store::{LspStoreEvent, SymbolLocation},
};
use smallvec::SmallVec;
use std::{ops::Range, rc::Rc};
use ui::{
    Chip, ContextMenu, Disclosure, IndentGuideColors, KeyBinding, ListItem, RenderedIndentGuide,
    ToggleButtonGroup, ToggleButtonGroupStyle, ToggleButtonSimple, Tooltip, prelude::*,
    rems_from_px,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

mod map;

actions!(
    photon_call_trails,
    [
        /// Shows the call hierarchy of the symbol under the cursor in a panel.
        ShowInPanel,
        /// Toggles focus on the call hierarchy panel.
        ToggleFocus,
        /// Shows or hides the selected call's call site.
        ToggleCallSite,
        /// Makes the selected function the root of the call hierarchy.
        MakeRoot,
    ]
);

const PANEL_NAME: &str = "Call Hierarchy";
const INDENT: Pixels = px(12.);

/// What the app plugs into the panel, so this crate needs neither the review nor the agent crates.
#[derive(Clone, Default)]
pub struct CallTrailHooks {
    /// The number of an open pull request review that changes these rows of the file at this
    /// worktree-relative path.
    #[allow(clippy::type_complexity)]
    pub changed_in_review: Option<Rc<dyn Fn(&Workspace, &str, Range<u32>, &App) -> Option<u64>>>,
    /// Asks the user's agent to explain a trail, given as a prompt.
    #[allow(clippy::type_complexity)]
    pub explain: Option<Rc<dyn Fn(&mut Workspace, String, &mut Window, &mut Context<Workspace>)>>,
}

impl Global for CallTrailHooks {}

pub fn set_hooks(hooks: CallTrailHooks, cx: &mut App) {
    cx.set_global(hooks);
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<CallHierarchyPanel>(window, cx);
        });
        workspace.register_action(show_in_panel);
        workspace.register_action(keep_open);
    })
    .detach();
}

/// What to say when the language server finds no function at the cursor. Errors in the file usually
/// mean the server isn't reading the project's sources, so they're the likely reason.
fn not_found_note(language: &str, errors: usize) -> (SharedString, SharedString) {
    let body = match errors {
        0 => "Put the cursor on a function's name.".to_string(),
        1 => format!(
            "{language} reports 1 error in this file, so it may not see this project's sources."
        ),
        errors => format!(
            "{language} reports {errors} errors in this file, so it may not see this project's sources."
        ),
    };
    ("No function at the cursor".into(), body.into())
}

fn show_in_panel(
    workspace: &mut Workspace,
    _: &ShowInPanel,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.focus_panel::<CallHierarchyPanel>(window, cx) else {
        return;
    };
    let editor = workspace.active_item_as::<Editor>(cx);
    panel.update(cx, |panel, cx| match editor {
        Some(editor) => panel.show_symbol_under_cursor(editor, window, cx),
        None => panel.show_empty(cx),
    });
}

fn keep_open(
    workspace: &mut Workspace,
    _: &call_hierarchy::KeepOpen,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((root, mode)) = workspace
        .active_modal::<CallHierarchyView>(cx)
        .and_then(|popup| popup.read(cx).root_and_mode(cx))
    else {
        return;
    };
    workspace.hide_modal(window, cx);
    if let Some(panel) = workspace.focus_panel::<CallHierarchyPanel>(window, cx) {
        panel.update(cx, |panel, cx| panel.set_root(root, mode, window, cx));
    }
}

pub struct CallHierarchyPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    map_scroll_handle: ScrollHandle,
    mode: CallHierarchyMode,
    view: TrailView,
    content: Content,
    root_header: RootHeader,
    selected: Option<usize>,
    /// The rows last drawn, for the indent guides.
    rows: Vec<Row>,
    root_task: Option<Task<()>>,
    child_tasks: HashMap<usize, Task<()>>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// The active editor's buffer, whose language server the panel reports on.
    active_buffer: Option<WeakEntity<Buffer>>,
    /// Why the last trace found nothing, shown above the start screen until the next trace.
    not_found: Option<(SharedString, SharedString)>,
    server_problem: Option<ServerProblem>,
    /// Servers that are downloading, starting or failed, project-wide.
    server_statuses: HashMap<LanguageServerName, ServerStatus>,
    /// The last start error of each failed server, to match a known fix.
    server_errors: HashMap<LanguageServerName, String>,
    /// The function the user asked for while its language server wasn't ready; traced once it is.
    pending_trace: Option<(WeakEntity<Buffer>, language::Anchor)>,
    /// Most recent first.
    recent: Vec<StartPoint>,
    /// `None` until first looked up.
    start_points: Option<Vec<StartPoint>>,
    start_points_task: Option<Task<()>>,
    active: bool,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerStatus {
    Busy,
    Failed,
}

/// Why the active file's language server can't give call trails.
#[derive(Clone, Debug, PartialEq)]
enum ServerProblem {
    Starting {
        server: SharedString,
        language: SharedString,
    },
    Failed {
        server: SharedString,
        /// A known fix's one sentence, like "jdtls needs Java 21 or newer."
        hint: Option<&'static str>,
    },
    Missing {
        language: SharedString,
    },
    Unsupported {
        server: SharedString,
        language: SharedString,
    },
}

impl ServerProblem {
    fn title(&self) -> String {
        match self {
            ServerProblem::Starting { language, .. } => {
                format!("No call hierarchy for {language} yet")
            }
            ServerProblem::Failed { server, .. } => format!("{server} failed to start"),
            ServerProblem::Missing { language } => format!("No language server for {language}"),
            ServerProblem::Unsupported { language, .. } => {
                format!("No call hierarchy for {language}")
            }
        }
    }

    fn body(&self) -> String {
        match self {
            ServerProblem::Starting { server, .. } => {
                format!("{server} is starting… This panel fills in as soon as it's ready.")
            }
            ServerProblem::Failed { hint, .. } => hint
                .unwrap_or("The call hierarchy needs it running.")
                .to_string(),
            ServerProblem::Missing { .. } => {
                "The call hierarchy needs one to find callers.".to_string()
            }
            ServerProblem::Unsupported { server, .. } => {
                format!("{server} can't find a function's callers and calls.")
            }
        }
    }
}

/// A function to trace from the empty panel.
#[derive(Clone, Debug, PartialEq)]
struct StartPoint {
    name: SharedString,
    tag: Option<Tag>,
    path: ProjectPath,
    file_name: SharedString,
    /// Where the function's declaration starts; its name is at or after this.
    position: PointUtf16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrailView {
    List,
    Map,
}

#[derive(Default)]
struct RootHeader {
    title: SharedString,
    signature: Option<SharedString>,
    changed_in_pr: Option<u64>,
}

enum Content {
    /// Nothing traced: where to start, or why the language server can't help.
    Empty,
    LoadingRoot,
    /// `nodes[0]` is the root; the rows show its descendants.
    Tree(Vec<Node>),
}

/// What kind of function a call is, when it's cheap to tell from its declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    EntryPoint,
    HttpHandler,
    Test,
}

impl Tag {
    fn label(self) -> &'static str {
        match self {
            Tag::EntryPoint => "entry point",
            Tag::HttpHandler => "HTTP handler",
            Tag::Test => "test",
        }
    }

    fn color(self) -> Color {
        match self {
            Tag::EntryPoint | Tag::HttpHandler => Color::Warning,
            Tag::Test => Color::Success,
        }
    }
}

struct Node {
    call: Call,
    parent: Option<usize>,
    depth: usize,
    /// `None` until first expanded and fetched.
    children: Option<Vec<usize>>,
    expanded: bool,
    recursive: bool,
    tag: Option<Tag>,
    /// The row of the first call site, in `call.target`'s buffer.
    call_row: u32,
    show_call_site: bool,
}

impl Node {
    fn new(call: Call, parent: Option<usize>, depth: usize, recursive: bool, cx: &App) -> Self {
        let tag = tag_for(&call.item, cx);
        let call_row = call
            .target
            .range
            .start
            .to_point(&call.target.buffer.read(cx).snapshot())
            .row;
        Self {
            call,
            parent,
            depth,
            children: None,
            expanded: false,
            recursive,
            tag,
            call_row,
            show_call_site: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    Node(usize),
    Loading {
        depth: usize,
    },
    /// One line of a call's call site: the call line and a line of context on each side.
    CallSite {
        node_ix: usize,
        row: u32,
    },
}

impl CallHierarchyPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                cx.new(|cx| Self::new(workspace, window, cx))
            })
        })
    }

    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let lsp_store = project.read(cx).lsp_store();
        let mut subscriptions = vec![
            cx.subscribe_in(&lsp_store, window, Self::on_lsp_store_event),
            cx.subscribe(&project, |this, _, event, cx| {
                if matches!(
                    event,
                    project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
                ) {
                    this.start_points = None;
                    this.start_points_task = None;
                    if this.active {
                        this.load_start_points(cx);
                    }
                    cx.notify();
                }
            }),
        ];
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.subscribe_in(
                &workspace_entity,
                window,
                |this, workspace, event, window, cx| {
                    if matches!(event, workspace::Event::ActiveItemChanged) {
                        let buffer = active_buffer(workspace.read(cx), cx);
                        if buffer != this.active_buffer {
                            this.active_buffer = buffer;
                            this.pending_trace = None;
                            this.refresh_server_problem(window, cx);
                        }
                    }
                },
            ));
        }
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            map_scroll_handle: ScrollHandle::new(),
            mode: CallHierarchyMode::Incoming,
            view: TrailView::List,
            content: Content::Empty,
            not_found: None,
            root_header: RootHeader::default(),
            selected: None,
            rows: Vec::new(),
            root_task: None,
            child_tasks: HashMap::default(),
            context_menu: None,
            active_buffer: active_buffer(workspace, cx),
            server_problem: None,
            server_statuses: HashMap::default(),
            server_errors: HashMap::default(),
            pending_trace: None,
            recent: Vec::new(),
            start_points: None,
            start_points_task: None,
            active: false,
            _subscriptions: subscriptions,
        };
        this.refresh_server_problem(window, cx);
        this
    }

    fn on_lsp_store_event(
        &mut self,
        _: &Entity<project::LspStore>,
        event: &LspStoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            LspStoreEvent::LanguageServerUpdate {
                name: Some(name),
                message:
                    proto::update_language_server::Variant::StatusUpdate(proto::StatusUpdate {
                        status: Some(proto::status_update::Status::Binary(status)),
                        message,
                    }),
                ..
            } => {
                let busy = [
                    proto::ServerBinaryStatus::CheckingForUpdate,
                    proto::ServerBinaryStatus::Downloading,
                    proto::ServerBinaryStatus::Starting,
                ];
                let status = if *status == proto::ServerBinaryStatus::Failed as i32 {
                    Some(ServerStatus::Failed)
                } else if busy.iter().any(|busy| *status == *busy as i32) {
                    Some(ServerStatus::Busy)
                } else {
                    None
                };
                match status {
                    Some(status) => self.server_statuses.insert(name.clone(), status),
                    None => self.server_statuses.remove(name),
                };
                match (status, message) {
                    (Some(ServerStatus::Failed), Some(message)) => {
                        self.server_errors.insert(name.clone(), message.clone())
                    }
                    _ => self.server_errors.remove(name),
                };
                self.refresh_server_problem(window, cx);
            }
            LspStoreEvent::LanguageServerAdded(_, name, _) => {
                self.server_statuses.remove(name);
                // Servers that weren't running had no symbols to offer.
                if self
                    .start_points
                    .as_ref()
                    .is_some_and(|start_points| start_points.is_empty())
                {
                    self.start_points = None;
                    if self.active {
                        self.load_start_points(cx);
                    }
                }
                self.refresh_server_problem(window, cx);
            }
            LspStoreEvent::LanguageServerRemoved(_) | LspStoreEvent::LanguageDetected { .. } => {
                self.refresh_server_problem(window, cx)
            }
            _ => {}
        }
    }

    fn refresh_server_problem(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let problem = self
            .active_buffer
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .and_then(|buffer| self.server_problem_for(&buffer, cx));
        if problem != self.server_problem {
            self.server_problem = problem;
            cx.notify();
        }
        if self.server_problem.is_none()
            && let Some((buffer, position)) = self.pending_trace.take()
            && let Some(buffer) = buffer.upgrade()
        {
            self.trace(buffer, position, window, cx);
        }
    }

    /// Remote projects check servers on the host, so they never report a problem here.
    fn server_problem_for(&self, buffer: &Entity<Buffer>, cx: &mut App) -> Option<ServerProblem> {
        if !self.project.read(cx).is_local() {
            return None;
        }
        let lsp_store = self.project.read(cx).lsp_store();
        let (server_ids, running) = lsp_store.update(cx, |lsp_store, cx| {
            buffer.update(cx, |buffer, cx| {
                let server_ids = lsp_store.language_servers_for_local_buffer(buffer, cx);
                let running = server_ids
                    .iter()
                    .filter_map(|server_id| {
                        let (_, server) =
                            lsp_store.language_server_for_local_buffer(buffer, *server_id, cx)?;
                        let supported = lsp_store
                            .lsp_server_capabilities
                            .get(server_id)
                            .is_some_and(|capabilities| {
                                match &capabilities.call_hierarchy_provider {
                                    Some(lsp::CallHierarchyServerCapability::Simple(supported)) => {
                                        *supported
                                    }
                                    Some(lsp::CallHierarchyServerCapability::Options(_)) => true,
                                    None => false,
                                }
                            });
                        Some((SharedString::from(server.name().0.to_string()), supported))
                    })
                    .collect::<Vec<_>>();
                (server_ids, running)
            })
        });
        let language = buffer
            .read(cx)
            .language()
            .filter(|language| **language != *PLAIN_TEXT)
            .cloned();
        let Some(language) = language else {
            let suggestion = extension_suggest::suggest_extension(buffer.read(cx).file()?.path())?;
            let mut name = suggestion.extension_id.to_string();
            if let Some(first) = name.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            return Some(ServerProblem::Missing {
                language: name.into(),
            });
        };
        let language_name = SharedString::from(language.name().to_string());
        if running.iter().any(|(_, supported)| *supported) {
            return None;
        }
        if let Some((server, _)) = running.into_iter().next() {
            return Some(ServerProblem::Unsupported {
                server,
                language: language_name,
            });
        }
        let adapters = self
            .project
            .read(cx)
            .languages()
            .lsp_adapters(&language.name());
        let status = |status: ServerStatus| {
            adapters
                .iter()
                .find(|adapter| self.server_statuses.get(&adapter.name) == Some(&status))
                .map(|adapter| SharedString::from(adapter.name.0.to_string()))
        };
        if let Some(adapter) = adapters
            .iter()
            .find(|adapter| self.server_statuses.get(&adapter.name) == Some(&ServerStatus::Failed))
        {
            let error = self.server_errors.get(&adapter.name).map(String::as_str);
            return Some(ServerProblem::Failed {
                server: SharedString::from(adapter.name.0.to_string()),
                hint: photon_navigation::known_fix(&language_name, error).map(|fix| fix.hint),
            });
        }
        if let Some(server) = status(ServerStatus::Busy).or_else(|| {
            (!server_ids.is_empty())
                .then(|| adapters.first())
                .flatten()
                .map(|adapter| SharedString::from(adapter.name.0.to_string()))
        }) {
            return Some(ServerProblem::Starting {
                server,
                language: language_name,
            });
        }
        (!adapters.is_empty()).then_some(ServerProblem::Missing {
            language: language_name,
        })
    }

    fn show_empty(&mut self, cx: &mut Context<Self>) {
        self.root_task = None;
        self.child_tasks.clear();
        self.selected = None;
        self.content = Content::Empty;
        cx.notify();
    }

    fn show_symbol_under_cursor(
        &mut self,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = editor.update(cx, |editor, cx| {
            let head = editor.selections.newest_anchor().head();
            editor.buffer().read(cx).text_anchor_for_position(head, cx)
        });
        let Some((buffer, position)) = position else {
            self.show_empty(cx);
            return;
        };
        self.trace(buffer, position, window, cx);
    }

    /// Traces the function at `position`, or, while its language server isn't ready, waits for it.
    fn trace(
        &mut self,
        buffer: Entity<Buffer>,
        position: language::Anchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.server_problem_for(&buffer, cx).is_some() {
            self.show_empty(cx);
            self.pending_trace = Some((buffer.downgrade(), position));
            return;
        }
        self.pending_trace = None;
        let prepare = self.project.update(cx, |project, cx| {
            project.prepare_call_hierarchy(&buffer, position, cx)
        });
        self.start_loading_root(cx);
        let project = self.project.clone();
        let (language, errors) = buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let errors = snapshot
                .diagnostics_in_range::<usize, usize>(0..snapshot.len(), false)
                .filter(|entry| entry.diagnostic.severity == lsp::DiagnosticSeverity::ERROR)
                .count();
            let language = buffer.language().map_or_else(
                || "The language server".to_string(),
                |language| language.name().to_string(),
            );
            (language, errors)
        });
        self.root_task = Some(cx.spawn_in(window, async move |this, cx| {
            let note = match prepare.await {
                Ok(items) => match items.unwrap_or_default().into_iter().next() {
                    Some(item) => Err(item),
                    None => Ok(not_found_note(&language, errors)),
                },
                Err(error) => {
                    log::error!("failed to prepare call hierarchy: {error:#}");
                    Ok((
                        format!("{language} couldn't trace this function").into(),
                        format!("{error:#}").into(),
                    ))
                }
            };
            let item = match note {
                Err(item) => item,
                Ok(note) => {
                    this.update(cx, |this, cx| {
                        this.show_empty(cx);
                        this.not_found = Some(note);
                    })
                    .ok();
                    return;
                }
            };
            let root = call_hierarchy::call_for_item(item, &project, cx).await;
            this.update_in(cx, |this, window, cx| this.show_root(root, window, cx))
                .ok();
        }));
    }

    fn trace_start_point(
        &mut self,
        start_point: StartPoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self
            .project
            .update(cx, |project, cx| project.open_buffer(start_point.path, cx));
        self.start_loading_root(cx);
        self.root_task = Some(cx.spawn_in(window, async move |this, cx| {
            let buffer = match open.await {
                Ok(buffer) => buffer,
                Err(error) => {
                    log::error!("failed to open {}: {error:#}", start_point.name);
                    this.update(cx, |this, cx| this.show_empty(cx)).ok();
                    return;
                }
            };
            this.update_in(cx, |this, window, cx| {
                let snapshot = buffer.read(cx).snapshot();
                let start = language::Unclipped(start_point.position).to_point(&snapshot);
                let position =
                    snapshot.anchor_after(name_position(&snapshot, start, &start_point.name));
                this.trace(buffer, position, window, cx)
            })
            .ok();
        }));
    }

    fn load_start_points(&mut self, cx: &mut Context<Self>) {
        if self.start_points.is_some() || self.start_points_task.is_some() {
            return;
        }
        let symbols = self
            .project
            .update(cx, |project, cx| project.symbols("", cx));
        self.start_points_task = Some(cx.spawn(async move |this, cx| {
            let symbols = symbols.await.log_err().unwrap_or_default();
            this.update(cx, |this, cx| {
                this.start_points = Some(start_points(&this.project, symbols, cx));
                this.start_points_task = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn remember(&mut self, item: &CallHierarchyItem, tag: Option<Tag>, cx: &App) {
        let Some(file) = item.buffer.read(cx).file() else {
            return;
        };
        let snapshot = item.buffer.read(cx).snapshot();
        let start_point = StartPoint {
            name: item.name.clone().into(),
            tag,
            path: ProjectPath {
                worktree_id: file.worktree_id(cx),
                path: file.path().clone(),
            },
            file_name: file.file_name(cx).to_string().into(),
            position: item.selection_range.start.to_point_utf16(&snapshot),
        };
        self.recent
            .retain(|recent| recent.name != start_point.name || recent.path != start_point.path);
        self.recent.insert(0, start_point);
        self.recent.truncate(5);
    }

    pub fn set_root(
        &mut self,
        item: CallHierarchyItem,
        mode: CallHierarchyMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = mode;
        self.start_loading_root(cx);
        let project = self.project.clone();
        self.root_task = Some(cx.spawn_in(window, async move |this, cx| {
            let root = call_hierarchy::call_for_item(item, &project, cx).await;
            this.update_in(cx, |this, window, cx| this.show_root(root, window, cx))
                .ok();
        }));
    }

    fn start_loading_root(&mut self, cx: &mut Context<Self>) {
        self.not_found = None;
        self.child_tasks.clear();
        self.selected = None;
        self.content = Content::LoadingRoot;
        cx.notify();
    }

    /// Never called while the workspace is being updated: the review hook reads it.
    fn show_root(&mut self, root: Call, window: &mut Window, cx: &mut Context<Self>) {
        self.root_task = None;
        self.child_tasks.clear();
        self.selected = None;
        self.root_header = self.root_header(&root, cx);
        let root = Node::new(root, None, 0, false, cx);
        self.remember(&root.call.item, root.tag, cx);
        self.content = Content::Tree(vec![root]);
        self.expand(0, window, cx);
    }

    fn root_header(&self, root: &Call, cx: &App) -> RootHeader {
        let item = &root.item;
        let snapshot = item.buffer.read(cx).snapshot();
        let owner = snapshot
            .symbols_containing(item.selection_range.start, None)
            .iter()
            .rev()
            .find_map(|symbol| owner_name(&symbol.text).map(str::to_string));
        let title = match owner {
            Some(owner) => format!("{owner}::{}", item.name),
            None => item.name.clone(),
        };
        let signature = item
            .detail
            .clone()
            .filter(|detail| !detail.trim().is_empty())
            .unwrap_or_else(|| {
                let row = item.selection_range.start.to_point(&snapshot).row;
                signature(&snapshot, row)
            });
        let changed_in_pr = (|| {
            let hook = cx
                .try_global::<CallTrailHooks>()?
                .changed_in_review
                .clone()?;
            let workspace = self.workspace.upgrade()?;
            let path = item
                .buffer
                .read(cx)
                .file()?
                .path()
                .as_unix_str()
                .to_string();
            let rows = item.range.start.to_point(&snapshot).row
                ..item.range.end.to_point(&snapshot).row + 1;
            hook(workspace.read(cx), &path, rows, cx)
        })();
        RootHeader {
            title: title.into(),
            signature: (!signature.is_empty()).then(|| signature.into()),
            changed_in_pr,
        }
    }

    fn set_mode(&mut self, mode: CallHierarchyMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.make_root(0, window, cx);
    }

    fn set_view(&mut self, view: TrailView, cx: &mut Context<Self>) {
        self.view = view;
        cx.notify();
    }

    fn make_root(&mut self, node_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(node) = self.nodes().get(node_ix) {
            let root = node.call.clone();
            self.show_root(root, window, cx);
        }
    }

    fn make_selected_root(&mut self, _: &MakeRoot, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(node_ix) = self.selected {
            self.make_root(node_ix, window, cx);
        }
    }

    fn nodes(&self) -> &[Node] {
        match &self.content {
            Content::Tree(nodes) => nodes,
            Content::Empty | Content::LoadingRoot => &[],
        }
    }

    /// Recursive calls, and callers of entry points and tests, aren't worth a fetch; nor is a
    /// call already known to have none.
    fn can_expand(&self, node: &Node) -> bool {
        let leaf_by_kind = self.mode == CallHierarchyMode::Incoming
            && matches!(node.tag, Some(Tag::EntryPoint | Tag::Test));
        !node.recursive
            && !(node.parent.is_some() && leaf_by_kind)
            && node
                .children
                .as_ref()
                .is_none_or(|children| !children.is_empty())
    }

    fn expand(&mut self, node_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .nodes()
            .get(node_ix)
            .is_some_and(|node| self.can_expand(node))
        {
            return;
        }
        let mode = self.mode;
        let Content::Tree(nodes) = &mut self.content else {
            return;
        };
        let Some(node) = nodes.get_mut(node_ix) else {
            return;
        };
        node.expanded = true;
        cx.notify();
        if node.children.is_some() || self.child_tasks.contains_key(&node_ix) {
            return;
        }
        let item = node.call.item.clone();
        let project = self.project.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let calls = call_hierarchy::fetch_calls(&item, &project, mode, cx).await;
            this.update(cx, |this, cx| this.add_children(node_ix, calls, cx))
                .ok();
        });
        self.child_tasks.insert(node_ix, task);
    }

    fn add_children(&mut self, parent_ix: usize, calls: Vec<Call>, cx: &mut Context<Self>) {
        self.child_tasks.remove(&parent_ix);
        let Content::Tree(nodes) = &mut self.content else {
            return;
        };
        let Some(depth) = nodes.get(parent_ix).map(|parent| parent.depth + 1) else {
            return;
        };
        let mut children = Vec::with_capacity(calls.len());
        for call in calls {
            let mut ancestor = Some(parent_ix);
            let mut recursive = false;
            while let Some(ancestor_node) = ancestor.and_then(|ix| nodes.get(ix)) {
                if same_item(&ancestor_node.call.item, &call.item) {
                    recursive = true;
                    break;
                }
                ancestor = ancestor_node.parent;
            }
            children.push(nodes.len());
            nodes.push(Node::new(call, Some(parent_ix), depth, recursive, cx));
        }
        if let Some(parent) = nodes.get_mut(parent_ix) {
            parent.children = Some(children);
        }
        cx.notify();
    }

    fn collapse(&mut self, node_ix: usize, cx: &mut Context<Self>) {
        if let Content::Tree(nodes) = &mut self.content
            && let Some(node) = nodes.get_mut(node_ix)
        {
            node.expanded = false;
            cx.notify();
        }
    }

    fn toggle_expanded(&mut self, node_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.nodes().get(node_ix).is_some_and(|node| node.expanded) {
            self.collapse(node_ix, cx);
        } else {
            self.expand(node_ix, window, cx);
        }
    }

    fn toggle_call_site(&mut self, node_ix: usize, cx: &mut Context<Self>) {
        // The root has no call site of its own.
        if node_ix == 0 {
            return;
        }
        if let Content::Tree(nodes) = &mut self.content
            && let Some(node) = nodes.get_mut(node_ix)
        {
            node.show_call_site = !node.show_call_site;
            cx.notify();
        }
    }

    fn toggle_selected_call_site(
        &mut self,
        _: &ToggleCallSite,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(node_ix) = self.selected {
            self.toggle_call_site(node_ix, cx);
        }
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let mut rows = Vec::new();
        if !self.nodes().is_empty() {
            self.push_rows(0, &mut rows, cx);
        }
        rows
    }

    fn push_rows(&self, node_ix: usize, rows: &mut Vec<Row>, cx: &App) {
        let nodes = self.nodes();
        let Some(node) = nodes.get(node_ix) else {
            return;
        };
        let Some(children) = &node.children else {
            rows.push(Row::Loading {
                depth: node.depth + 1,
            });
            return;
        };
        for &child_ix in children {
            rows.push(Row::Node(child_ix));
            let Some(child) = nodes.get(child_ix) else {
                continue;
            };
            if child.show_call_site {
                let last_row = child.call.target.buffer.read(cx).max_point().row;
                for row in child.call_row.saturating_sub(1)..=(child.call_row + 1).min(last_row) {
                    rows.push(Row::CallSite {
                        node_ix: child_ix,
                        row,
                    });
                }
            }
            if child.expanded {
                self.push_rows(child_ix, rows, cx);
            }
        }
    }

    /// The selected call and its ancestors, ending with the root.
    fn trail(&self) -> Vec<usize> {
        let nodes = self.nodes();
        let mut trail = Vec::new();
        let mut current = self.selected.filter(|selected| *selected < nodes.len());
        while let Some(node_ix) = current {
            trail.push(node_ix);
            current = nodes.get(node_ix).and_then(|node| node.parent);
        }
        if trail.is_empty() && !nodes.is_empty() {
            trail.push(0);
        }
        trail
    }

    /// The trail in the order calls happen: outermost caller first.
    fn trail_in_call_order(&self) -> Vec<usize> {
        let mut trail = self.trail();
        if self.mode == CallHierarchyMode::Incoming {
            return trail;
        }
        trail.reverse();
        trail
    }

    fn summary(&self) -> Option<String> {
        let nodes = self.nodes();
        let children = nodes.first()?.children.as_ref()?;
        let noun = match self.mode {
            CallHierarchyMode::Incoming => ("caller", "callers"),
            CallHierarchyMode::Outgoing => ("call", "calls"),
        };
        if children.is_empty() {
            return Some(format!("No {}", noun.1));
        }
        let mut counted: Vec<&CallHierarchyItem> = Vec::new();
        let (mut entry_points, mut tests) = (0, 0);
        for node in nodes.iter().skip(1) {
            if node.recursive || counted.iter().any(|item| same_item(item, &node.call.item)) {
                continue;
            }
            counted.push(&node.call.item);
            match node.tag {
                Some(Tag::EntryPoint | Tag::HttpHandler) => entry_points += 1,
                Some(Tag::Test) => tests += 1,
                None => {}
            }
        }
        let count = |count: usize, (one, many): (&str, &str)| {
            format!("{count} {}", if count == 1 { one } else { many })
        };
        let mut parts = vec![count(children.len(), noun)];
        if entry_points > 0 {
            parts.push(count(entry_points, ("entry point", "entry points")));
        }
        if tests > 0 {
            parts.push(count(tests, ("test", "tests")));
        }
        Some(parts.join(" · "))
    }

    /// The selected trail for the user's agent: each function, and the line where it makes the
    /// next call.
    fn trail_prompt(&self, cx: &App) -> Option<String> {
        self.selected?;
        let nodes = self.nodes();
        let trail = self.trail_in_call_order();
        let mut prompt = String::from(
            "Explain this call trail briefly: what each step does and why it calls the next.\n",
        );
        for (step, node_ix) in trail.iter().enumerate() {
            let node = nodes.get(*node_ix)?;
            let item = &node.call.item;
            let snapshot = item.buffer.read(cx).snapshot();
            let path = item
                .buffer
                .read(cx)
                .file()
                .map(|file| file.path().as_unix_str().to_string())
                .unwrap_or_default();
            let row = item.selection_range.start.to_point(&snapshot).row + 1;
            prompt.push_str(&format!("\n{}. {} ({path}:{row})", step + 1, item.name));
            let Some(next) = trail.get(step + 1).and_then(|next| nodes.get(*next)) else {
                continue;
            };
            // The call site belongs to the deeper of the two nodes.
            let site = if node.depth > next.depth { node } else { next };
            let site_snapshot = site.call.target.buffer.read(cx).snapshot();
            let site_path = site
                .call
                .target
                .buffer
                .read(cx)
                .file()
                .map(|file| file.path().as_unix_str().to_string())
                .unwrap_or_default();
            let line = site_snapshot
                .text_for_range(
                    language::Point::new(site.call_row, 0)
                        ..language::Point::new(
                            site.call_row,
                            site_snapshot.line_len(site.call_row),
                        ),
                )
                .collect::<String>();
            prompt.push_str(&format!(
                "\n   calls {} at {site_path}:{}: `{}`",
                next.call.item.name,
                site.call_row + 1,
                line.trim()
            ));
        }
        Some(prompt)
    }

    fn explain_trail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(explain) = cx
            .try_global::<CallTrailHooks>()
            .and_then(|hooks| hooks.explain.clone())
        else {
            return;
        };
        let Some(prompt) = self.trail_prompt(cx) else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| explain(workspace, prompt, window, cx))
            .log_err();
    }

    fn open_call(&mut self, node_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.nodes().get(node_ix) else {
            return;
        };
        let buffer = node.call.target.buffer.clone();
        let target = node.call.target.range.start;
        self.workspace
            .update(cx, |workspace, cx| {
                let position = target.to_point(&buffer.read(cx).snapshot());
                let pane = workspace.active_pane().clone();
                let editor = workspace.open_project_item::<Editor>(
                    Some(pane),
                    buffer,
                    true,
                    true,
                    true,
                    true,
                    window,
                    cx,
                );
                editor.update(cx, |editor, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([position..position]),
                    );
                });
            })
            .log_err();
    }

    fn select(&mut self, node_ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(node_ix);
        if let Some(row_ix) = self
            .rows(cx)
            .iter()
            .position(|row| *row == Row::Node(node_ix))
        {
            self.scroll_handle
                .scroll_to_item(row_ix, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn select_by_offset(&mut self, offset: isize, cx: &mut Context<Self>) {
        let node_rows = self
            .rows(cx)
            .into_iter()
            .filter_map(|row| match row {
                Row::Node(node_ix) => Some(node_ix),
                Row::Loading { .. } | Row::CallSite { .. } => None,
            })
            .collect::<Vec<_>>();
        let current = self
            .selected
            .and_then(|selected| node_rows.iter().position(|node_ix| *node_ix == selected));
        let next = match current {
            Some(current) => current
                .saturating_add_signed(offset)
                .min(node_rows.len().saturating_sub(1)),
            None => 0,
        };
        if let Some(&node_ix) = node_rows.get(next) {
            self.select(node_ix, cx);
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select_by_offset(1, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_by_offset(-1, cx);
    }

    fn select_child(&mut self, _: &menu::SelectChild, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node_ix) = self.selected else {
            return;
        };
        let Some(node) = self.nodes().get(node_ix) else {
            return;
        };
        if !node.expanded {
            self.expand(node_ix, window, cx);
        } else if let Some(&first_child) =
            node.children.as_ref().and_then(|children| children.first())
        {
            self.select(first_child, cx);
        }
    }

    fn select_parent(&mut self, _: &menu::SelectParent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(node_ix) = self.selected else {
            return;
        };
        let Some(node) = self.nodes().get(node_ix) else {
            return;
        };
        if node.expanded && self.can_expand(node) {
            self.collapse(node_ix, cx);
        } else if let Some(parent) = node.parent.filter(|parent| *parent != 0) {
            self.select(parent, cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(node_ix) = self.selected {
            self.open_call(node_ix, window, cx);
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        node_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self
            .nodes()
            .get(node_ix)
            .map(|node| node.call.item.name.clone())
        else {
            return;
        };
        self.select(node_ix, cx);
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .action("Make Root", Box::new(MakeRoot))
                .entry("Copy Name", None, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                })
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
    }

    fn render_row(&self, row: Row, cx: &mut Context<Self>) -> AnyElement {
        match row {
            Row::Node(node_ix) => self.render_node_row(node_ix, cx),
            Row::CallSite { node_ix, row } => self.render_call_site_line(node_ix, row, cx),
            Row::Loading { depth } => ListItem::new(("call-trail-loading", depth))
                .height(row_height())
                .indent_level(depth.saturating_sub(1))
                .indent_step_size(INDENT)
                .selectable(false)
                .start_slot(div().w_4())
                .child(
                    Label::new("Loading…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        }
    }

    fn render_node_row(&self, node_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(node) = self.nodes().get(node_ix) else {
            return div().into_any_element();
        };
        let call = &node.call;
        let name = SharedString::from(call.item.name.clone());
        let file = call.item.buffer.read(cx).file().map(|file| {
            (
                SharedString::from(file.file_name(cx).to_string()),
                SharedString::from(file.full_path(cx).to_string_lossy().into_owned()),
            )
        });
        let disclosure = self.can_expand(node).then(|| {
            Disclosure::new(("call-trail-disclosure", node_ix), node.expanded).on_click(
                cx.listener(move |this, _, window, cx| this.toggle_expanded(node_ix, window, cx)),
            )
        });
        let sites_selector = name.clone();
        let call_sites = div()
            .id(("call-trail-sites", node_ix))
            .debug_selector(move || format!("call-trail-sites-{sites_selector}"))
            .px_1()
            .rounded_sm()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(
                Label::new(format!("×{}", call.site_count))
                    .size(LabelSize::XSmall)
                    .color(if node.show_call_site {
                        Color::Accent
                    } else {
                        Color::Muted
                    }),
            )
            .tooltip(Tooltip::text(if node.show_call_site {
                "Hide Call Site"
            } else {
                "Show Call Site"
            }))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.selected = Some(node_ix);
                this.toggle_call_site(node_ix, cx);
            }));
        let name_selector = name.clone();
        div()
            .debug_selector(move || format!("call-trail-{name_selector}"))
            .child(
                ListItem::new(("call-trail", node_ix))
                    .height(row_height())
                    .indent_level(node.depth.saturating_sub(1))
                    .indent_step_size(INDENT)
                    .toggle_state(self.selected == Some(node_ix))
                    .start_slot(h_flex().w_4().justify_center().children(disclosure))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .overflow_hidden()
                            .child(glyph(node.tag, cx))
                            .child(
                                div()
                                    .flex_none()
                                    .child(Label::new(name).size(LabelSize::Small).single_line()),
                            )
                            .children(file.map(|(file_name, full_path)| {
                                div()
                                    .id(("call-trail-file", node_ix))
                                    .min_w_0()
                                    .overflow_hidden()
                                    .tooltip(Tooltip::text(full_path))
                                    .child(
                                        Label::new(file_name)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                            }))
                            .children(node.tag.map(|tag| chip(tag.label(), tag.color(), cx)))
                            .when(node.recursive, |this| {
                                this.child(chip("↻ recursive", Color::Accent, cx))
                            }),
                    )
                    .end_slot(call_sites)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.selected = Some(node_ix);
                        this.open_call(node_ix, window, cx);
                    }))
                    .on_secondary_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.deploy_context_menu(event.position, node_ix, window, cx);
                        },
                    )),
            )
            .into_any_element()
    }

    fn render_call_site_line(
        &self,
        node_ix: usize,
        row: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(node) = self.nodes().get(node_ix) else {
            return div().into_any_element();
        };
        let snapshot = node.call.target.buffer.read(cx).snapshot();
        let first_row = node.call_row.saturating_sub(1);
        let last_row = (node.call_row + 1).min(snapshot.max_point().row);
        // Drops the indentation the three lines share, so the narrow panel shows the code.
        let indent = (first_row..=last_row)
            .filter(|row| !snapshot.is_line_blank(*row))
            .map(|row| snapshot.indent_size_for_line(row).len)
            .min()
            .unwrap_or(0);
        let line_len = snapshot.line_len(row);
        let code = snapshot.highlighted_text_for_range(
            language::Point::new(row, indent.min(line_len))..language::Point::new(row, line_len),
            None,
            cx.theme().syntax(),
        );
        let is_call = row == node.call_row;
        let colors = cx.theme().colors();
        let selector = node.call.item.name.clone();
        div()
            .id(SharedString::from(format!(
                "call-trail-site-{node_ix}-{row}"
            )))
            .debug_selector(move || format!("call-trail-site-{selector}-{}", row + 1))
            .h(row_height())
            .w_full()
            .pl(INDENT * node.depth as f32 + px(12.))
            .pr_2()
            .child(
                h_flex()
                    .h_full()
                    .w_full()
                    .gap_2()
                    .px_1p5()
                    .overflow_hidden()
                    .border_x_1()
                    .border_color(colors.border_variant)
                    .bg(if is_call {
                        colors.editor_active_line_background
                    } else {
                        colors.editor_background
                    })
                    .when(row == first_row, |this| this.border_t_1().rounded_t_md())
                    .when(row == last_row, |this| this.border_b_1().rounded_b_md())
                    .cursor_pointer()
                    .child(
                        div()
                            .flex_none()
                            .min_w(rems_from_px(20_f32))
                            .text_right()
                            .child(
                                Label::new((row + 1).to_string())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(if is_call { Color::Accent } else { Color::Muted }),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_buffer(cx)
                            .text_ui_xs(cx)
                            .child(StyledText::new(code.text).with_highlights(code.highlights)),
                    ),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.selected = Some(node_ix);
                this.open_call(node_ix, window, cx);
            }))
            .into_any_element()
    }

    fn render_header(&self, root: &Node, cx: &mut Context<Self>) -> AnyElement {
        let header = &self.root_header;
        let icon_button = |id: &'static str, icon: IconName, tooltip: &'static str, view| {
            div().debug_selector(move || id.into()).child(
                IconButton::new(id, icon)
                    .icon_size(IconSize::Small)
                    .toggle_state(self.view == view)
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_view(view, cx))),
            )
        };
        v_flex()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_1p5()
                    .min_w_0()
                    .child(glyph(root.tag, cx))
                    .child(
                        div().min_w_0().overflow_hidden().child(
                            Label::new(header.title.clone())
                                .weight(FontWeight::SEMIBOLD)
                                .truncate(),
                        ),
                    )
                    .children(header.changed_in_pr.map(|number| {
                        div()
                            .debug_selector(|| "call-trail-pr-chip".into())
                            .child(chip(
                                format!("changed in PR #{number}"),
                                Color::Modified,
                                cx,
                            ))
                    })),
            )
            .children(header.signature.clone().map(|signature| {
                div().pl(rems_from_px(22_f32)).min_w_0().child(
                    Label::new(signature)
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted)
                        .truncate(),
                )
            }))
            .child(
                h_flex()
                    .pt_1()
                    .gap_1()
                    .child(
                        ToggleButtonGroup::single_row(
                            "call-trail-mode",
                            [
                                ToggleButtonSimple::new(
                                    "Callers",
                                    cx.listener(|this, _, window, cx| {
                                        this.set_mode(CallHierarchyMode::Incoming, window, cx)
                                    }),
                                ),
                                ToggleButtonSimple::new(
                                    "Calls",
                                    cx.listener(|this, _, window, cx| {
                                        this.set_mode(CallHierarchyMode::Outgoing, window, cx)
                                    }),
                                ),
                            ],
                        )
                        .style(ToggleButtonGroupStyle::Outlined)
                        .auto_width()
                        .selected_index(match self.mode {
                            CallHierarchyMode::Incoming => 0,
                            CallHierarchyMode::Outgoing => 1,
                        }),
                    )
                    .child(icon_button(
                        "call-trail-view-list",
                        IconName::ListTree,
                        "List",
                        TrailView::List,
                    ))
                    .child(icon_button(
                        "call-trail-view-map",
                        IconName::GitGraph,
                        "Map",
                        TrailView::Map,
                    ))
                    .child(div().flex_1())
                    .children(self.summary().map(|summary| {
                        div().min_w_0().child(
                            Label::new(summary)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    })),
            )
            .into_any_element()
    }

    fn render_breadcrumb(&self, cx: &mut Context<Self>) -> AnyElement {
        let nodes = self.nodes();
        let trail = self.trail_in_call_order();
        let last = trail.len().saturating_sub(1);
        let mut crumbs = h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .min_w_0()
            .overflow_hidden()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new("trail")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        for (position, node_ix) in trail.iter().copied().enumerate() {
            let Some(node) = nodes.get(node_ix) else {
                continue;
            };
            let name = SharedString::from(node.call.item.name.clone());
            let is_selected = self.selected == Some(node_ix) || (node_ix == 0 && trail.len() == 1);
            let selector = name.clone();
            crumbs = crumbs.child(
                div()
                    .id(("call-trail-crumb", node_ix))
                    .debug_selector(move || format!("call-trail-crumb-{selector}"))
                    .min_w_0()
                    .overflow_hidden()
                    .when(node_ix != 0, |this| {
                        this.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| this.select(node_ix, cx)))
                    })
                    .child(
                        Label::new(name)
                            .size(LabelSize::XSmall)
                            .color(if is_selected {
                                Color::Default
                            } else {
                                Color::Muted
                            })
                            .truncate(),
                    ),
            );
            if position < last {
                crumbs = crumbs.child(Label::new("›").size(LabelSize::XSmall).color(Color::Muted));
            }
        }
        crumbs.into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let hint = |action: &dyn Action, label: &'static str, cx: &App| {
            h_flex()
                .flex_none()
                .gap_1()
                .child(
                    KeyBinding::for_action_in(action, &self.focus_handle, cx)
                        .size(rems_from_px(11_f32)),
                )
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        };
        let can_explain = cx
            .try_global::<CallTrailHooks>()
            .is_some_and(|hooks| hooks.explain.is_some());
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_3()
            .overflow_hidden()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(hint(&menu::Confirm, "open", cx))
            .when(self.view == TrailView::List, |this| {
                this.child(hint(&ToggleCallSite, "call site", cx))
            })
            .child(hint(&MakeRoot, "make root", cx))
            .child(div().flex_1())
            .when(can_explain, |this| {
                this.child(
                    div().debug_selector(|| "call-trail-explain".into()).child(
                        Button::new("call-trail-explain", "Explain This Trail…")
                            .label_size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .disabled(self.selected.is_none())
                            .tooltip(Tooltip::text("Ask your agent how this trail works"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.explain_trail(window, cx)),
                            ),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_server_problem(&self, problem: &ServerProblem, cx: &mut Context<Self>) -> AnyElement {
        let (icon, color) = match problem {
            ServerProblem::Starting { .. } => (IconName::Info, Color::Muted),
            _ => (IconName::Warning, Color::Warning),
        };
        v_flex()
            .debug_selector(|| "call-trail-server-problem".into())
            .flex_1()
            .items_center()
            .justify_center()
            .px_6()
            .gap_1()
            .child(Icon::new(icon).color(color))
            .child(
                Label::new(problem.title())
                    .weight(FontWeight::SEMIBOLD)
                    .mt_1(),
            )
            .child(
                div().text_center().child(
                    Label::new(problem.body())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                div()
                    .debug_selector(|| "call-trail-set-up-server".into())
                    .mt_2()
                    .child(
                        Button::new("call-trail-set-up-server", "Set Up Language Server")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.focus_handle.dispatch_action(
                                    &photon_navigation::ShowLanguageServerHelp,
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_start(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mut editor_context = KeyContext::default();
        editor_context.add("Editor");
        let key_binding = window
            .highest_precedence_binding_for_action_in_context(&ShowInPanel, editor_context)
            .map(|binding| {
                KeyBinding::from_keystrokes(
                    binding.keystrokes().into(),
                    KeyBinding::is_vim_mode(cx),
                )
                .size(rems_from_px(12_f32))
            });
        let start_points = self.start_points.as_deref().unwrap_or_default();
        let has_rows = !self.recent.is_empty() || !start_points.is_empty();
        let hint = |text: &'static str| Label::new(text).size(LabelSize::Small).color(Color::Muted);
        let how = h_flex()
            .flex_wrap()
            .justify_center()
            .gap_1()
            .map(|this| match key_binding {
                Some(key_binding) => this
                    .child(hint("Put the cursor on a function and press"))
                    .child(key_binding)
                    .when(has_rows, |this| this.child(hint("or pick one below."))),
                None if has_rows => {
                    this.child(hint("Put the cursor on a function, or pick one below."))
                }
                None => this.child(hint("Put the cursor on a function.")),
            });
        v_flex()
            .debug_selector(|| "call-trail-start".into())
            .id("call-trail-start")
            .flex_1()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .items_center()
                    .px_4()
                    .pt_8()
                    .pb_4()
                    .gap_1()
                    .children(self.not_found.as_ref().map(|(title, body)| {
                        v_flex()
                            .debug_selector(|| "call-trail-not-found".into())
                            .items_center()
                            .pb_4()
                            .gap_1()
                            .child(Icon::new(IconName::Warning).color(Color::Warning))
                            .child(Label::new(title.clone()).weight(FontWeight::SEMIBOLD))
                            .child(
                                div().text_center().child(
                                    Label::new(body.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                    }))
                    .child(trail_picture(cx))
                    .child(
                        Label::new("Who calls this, and what does it call?")
                            .weight(FontWeight::SEMIBOLD)
                            .mt_3(),
                    )
                    .child(how),
            )
            .children(self.render_start_points(
                "Recent",
                "this project",
                "call-trail-recent",
                &self.recent,
                cx,
            ))
            .children(self.render_start_points(
                "Start points",
                "found in this project",
                "call-trail-start-point",
                start_points,
                cx,
            ))
            .into_any_element()
    }

    fn render_start_points(
        &self,
        title: &'static str,
        source: &'static str,
        id: &'static str,
        start_points: &[StartPoint],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if start_points.is_empty() {
            return None;
        }
        let header = h_flex()
            .px_2()
            .pt_3()
            .pb_1()
            .justify_between()
            .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
            .child(
                Label::new(source)
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
            );
        let rows = start_points.iter().enumerate().map(|(ix, start_point)| {
            let selector = format!("{id}-{}", start_point.name);
            let clicked = start_point.clone();
            div().debug_selector(move || selector).child(
                ListItem::new((id, ix))
                    .height(row_height())
                    .start_slot(glyph(start_point.tag, cx))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                Label::new(start_point.name.clone())
                                    .size(LabelSize::Small)
                                    .single_line(),
                            )
                            .children(
                                start_point
                                    .tag
                                    .map(|tag| chip(tag.label(), tag.color(), cx)),
                            ),
                    )
                    .end_slot(
                        Label::new(start_point.file_name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.trace_start_point(clicked.clone(), window, cx)
                    })),
            )
        });
        Some(
            v_flex()
                .px_1()
                .child(header)
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.rows(cx);
        self.rows = rows.clone();
        uniform_list(
            "call-trails",
            rows.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                rows.get(range)
                    .unwrap_or_default()
                    .iter()
                    .map(|row| this.render_row(*row, cx))
                    .collect()
            }),
        )
        .flex_1()
        .py_1()
        .track_scroll(&self.scroll_handle)
        .with_decoration(
            ui::indent_guides(INDENT, IndentGuideColors::panel(cx))
                .with_compute_indents_fn(cx.entity(), |this, range, _, _| {
                    let nodes = this.nodes();
                    this.rows
                        .get(range)
                        .unwrap_or_default()
                        .iter()
                        .map(|row| match row {
                            Row::Node(node_ix) => nodes
                                .get(*node_ix)
                                .map_or(0, |node| node.depth.saturating_sub(1)),
                            Row::CallSite { node_ix, .. } => {
                                nodes.get(*node_ix).map_or(0, |node| node.depth)
                            }
                            Row::Loading { depth } => depth.saturating_sub(1),
                        })
                        .collect()
                })
                .with_render_fn(cx.entity(), |_, params, _, _| {
                    const LEFT_OFFSET: Pixels = px(15.);
                    params
                        .indent_guides
                        .into_iter()
                        .map(|layout| RenderedIndentGuide {
                            bounds: Bounds::new(
                                point(
                                    layout.offset.x * params.indent_size + LEFT_OFFSET,
                                    layout.offset.y * params.item_height,
                                ),
                                size(px(1.), layout.length * params.item_height),
                            ),
                            layout,
                            is_active: false,
                            hitbox: None,
                        })
                        .collect::<SmallVec<_>>()
                }),
        )
        .into_any_element()
    }
}

impl Render for CallHierarchyPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults(cx.platform_info().os);
        key_context.add("CallHierarchyPanel");
        key_context.add("menu");
        let message = matches!(self.content, Content::LoadingRoot).then_some("Loading…");
        let empty = matches!(self.content, Content::Empty).then(|| match &self.server_problem {
            Some(problem) => self.render_server_problem(problem, cx),
            None => self.render_start(window, cx),
        });
        let header = self
            .nodes()
            .first()
            .map(|root| self.render_header(root, cx));
        let has_tree = header.is_some();
        let body = has_tree.then(|| match self.view {
            TrailView::List => self.render_list(cx),
            TrailView::Map => self.render_map(window, cx),
        });
        v_flex()
            .id("call-hierarchy-panel")
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_child))
            .on_action(cx.listener(Self::select_parent))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::toggle_selected_call_site))
            .on_action(cx.listener(Self::make_selected_root))
            .children(message.map(|message| {
                div()
                    .px_2()
                    .py_1()
                    .child(Label::new(message).color(Color::Muted))
            }))
            .children(empty)
            .children(header)
            .when(has_tree, |this| this.child(self.render_breadcrumb(cx)))
            .children(body)
            .when(has_tree, |this| this.child(self.render_footer(cx)))
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Focusable for CallHierarchyPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CallHierarchyPanel {}

impl Panel for CallHierarchyPanel {
    fn persistent_name() -> &'static str {
        PANEL_NAME
    }

    fn panel_key() -> &'static str {
        PANEL_NAME
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(380.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ArrowRightLeft)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some(PANEL_NAME)
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        9
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            self.load_start_points(cx);
            // Closing dropped the root's fetch, if it was still loading.
            if self
                .nodes()
                .first()
                .is_some_and(|root| root.children.is_none())
            {
                self.expand(0, window, cx);
            }
            return;
        }
        self.root_task = None;
        self.start_points_task = None;
        if matches!(self.content, Content::LoadingRoot) {
            self.content = Content::Empty;
        }
        let pending = std::mem::take(&mut self.child_tasks);
        if let Content::Tree(nodes) = &mut self.content {
            for node_ix in pending.keys() {
                if let Some(node) = nodes.get_mut(*node_ix) {
                    node.expanded = false;
                }
            }
        }
        cx.notify();
    }
}

fn row_height() -> Rems {
    rems_from_px(24_f32)
}

/// A small square marking what kind of function a row is.
fn glyph(tag: Option<Tag>, cx: &App) -> AnyElement {
    let (color, mark) = match tag {
        Some(Tag::Test) => (Color::Success, Some(IconName::Check)),
        Some(Tag::EntryPoint | Tag::HttpHandler) => (Color::Warning, Some(IconName::PlayFilled)),
        None => (Color::Accent, None),
    };
    div()
        .flex_none()
        .size(rems_from_px(16_f32))
        .rounded_sm()
        .flex()
        .items_center()
        .justify_center()
        .bg(color.color(cx).alpha(0.14))
        .map(|this| match mark {
            Some(icon) => this.child(Icon::new(icon).size(IconSize::XSmall).color(color)),
            None => this.child(Label::new("ƒ").size(LabelSize::XSmall).color(color)),
        })
        .into_any_element()
}

/// Two callers joining at a function that goes on to call more: what a trail looks like.
fn trail_picture(cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    let border = colors.border;
    let function = |name: &'static str, color: Color| {
        div()
            .h(px(20.))
            .px_2()
            .flex()
            .items_center()
            .rounded_sm()
            .border_1()
            .border_color(if color == Color::Accent {
                color.color(cx)
            } else {
                border
            })
            .bg(colors.editor_background)
            .child(
                Label::new(name)
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(color),
            )
    };
    h_flex()
        .child(
            v_flex()
                .items_end()
                .gap(px(10.))
                .child(function("handler", Color::Muted))
                .child(function("test", Color::Muted)),
        )
        // The bracket's top and bottom edges meet the two callers' middles.
        .child(
            div()
                .w_3()
                .h(px(31.))
                .border_y_1()
                .border_r_1()
                .rounded_r_md()
                .border_color(border),
        )
        .child(div().w_3().h(px(1.)).bg(border))
        .child(function("charge()", Color::Accent))
        .child(div().w_4().h(px(1.)).bg(border))
        .child(Label::new("…").size(LabelSize::XSmall).color(Color::Muted))
}

fn chip(label: impl Into<SharedString>, color: Color, cx: &App) -> Chip {
    let hsla = color.color(cx);
    Chip::new(label)
        .label_color(color)
        .bg_color(hsla.alpha(0.1))
        .border_color(hsla.alpha(0.25))
}

/// The type a method belongs to, from an outline item's text like `impl Gateway`,
/// `impl<T> Display for Wrapper<T>` or `class Gateway`.
fn owner_name(text: &str) -> Option<&str> {
    let rest = if let Some(rest) = text.strip_prefix("impl") {
        let rest = rest.trim_start();
        let rest = match rest.strip_prefix('<') {
            Some(generics) => generics.split_once('>').map_or("", |(_, rest)| rest),
            None => rest,
        };
        rest.rsplit(" for ").next().unwrap_or(rest)
    } else {
        ["class ", "struct ", "trait ", "interface ", "enum "]
            .iter()
            .find_map(|keyword| text.strip_prefix(keyword))?
    };
    let rest = rest.trim_start();
    let end = rest
        .find(|character: char| {
            !(character.is_alphanumeric() || character == '_' || character == ':')
        })
        .unwrap_or(rest.len());
    let name = rest.get(..end)?.trim_end_matches(':');
    (!name.is_empty()).then_some(name)
}

/// The declaration from `row` up to its body, on one line.
fn signature(snapshot: &language::BufferSnapshot, row: u32) -> String {
    let mut signature = String::new();
    for row in row..=(row + 5).min(snapshot.max_point().row) {
        let line = snapshot
            .text_for_range(
                language::Point::new(row, 0)..language::Point::new(row, snapshot.line_len(row)),
            )
            .collect::<String>();
        let (line, ends) = match line.find('{') {
            Some(brace) => (line.get(..brace).unwrap_or_default().to_string(), true),
            None => (line, false),
        };
        if !signature.is_empty() && !signature.ends_with('(') {
            signature.push(' ');
        }
        signature.push_str(line.trim());
        if ends {
            break;
        }
    }
    signature
        .replace("( ", "(")
        .replace(", )", ")")
        .trim()
        .to_string()
}

/// Tags a function from its name, the attribute lines above it and its signature: cheap checks
/// on text the buffer already has.
fn tag_for(item: &CallHierarchyItem, cx: &App) -> Option<Tag> {
    let snapshot = item.buffer.read(cx).snapshot();
    let start = item.range.start.to_point(&snapshot).row;
    let name = item.selection_range.start.to_point(&snapshot);
    tag_at(&snapshot, &item.name, start, name)
}

fn tag_at(
    snapshot: &language::BufferSnapshot,
    name: &str,
    start: u32,
    name_position: language::Point,
) -> Option<Tag> {
    let name_row = name_position.row;
    let line = |row: u32| {
        snapshot
            .text_for_range(
                language::Point::new(row, 0)..language::Point::new(row, snapshot.line_len(row)),
            )
            .collect::<String>()
    };
    let mut attributes = Vec::new();
    // Language servers differ on whether a function's range includes its attributes.
    let mut row = start.min(name_row);
    for _ in 0..6 {
        let text = line(row);
        let trimmed = text.trim_start();
        if trimmed.starts_with("#[") || trimmed.starts_with('@') {
            attributes.push(trimmed.to_string());
        }
        if row == 0 {
            break;
        }
        row -= 1;
        let above = line(row);
        let above = above.trim_start();
        if !(above.starts_with("#[") || above.starts_with('@') || above.starts_with("///")) {
            break;
        }
    }
    let in_test_module = snapshot
        .symbols_containing(name_position, None)
        .iter()
        .any(|symbol| symbol.text == "mod tests" || symbol.text == "mod test");
    detect_tag(
        name,
        &attributes,
        &signature(snapshot, name_row),
        in_test_module,
    )
}

fn detect_tag(
    name: &str,
    attributes: &[String],
    signature: &str,
    in_test_module: bool,
) -> Option<Tag> {
    const ROUTE_ATTRIBUTES: &[&str] = &[
        "#[get(",
        "#[post(",
        "#[put(",
        "#[patch(",
        "#[delete(",
        "#[route(",
        "#[handler",
        "@app.",
        "@router.",
        "@Get",
        "@Post",
        "@Put",
        "@Delete",
        "@RequestMapping",
        "@GetMapping",
        "@PostMapping",
    ];
    const HANDLER_PARAMETERS: &[&str] =
        &["State(", "State<", "HttpRequest", "web::Json<", "Request<"];
    let is_test_attribute = |attribute: &String| {
        attribute.contains("test]") || attribute.contains("::test") || attribute.contains("test(")
    };
    if attributes.iter().any(is_test_attribute) || name.starts_with("test_") {
        return Some(Tag::Test);
    }
    if name == "main" {
        return Some(Tag::EntryPoint);
    }
    let parameters = signature.split("->").next().unwrap_or(signature);
    if attributes.iter().any(|attribute| {
        ROUTE_ATTRIBUTES
            .iter()
            .any(|route| attribute.starts_with(route))
    }) || HANDLER_PARAMETERS
        .iter()
        .any(|parameter| parameters.contains(parameter))
    {
        return Some(Tag::HttpHandler);
    }
    in_test_module.then_some(Tag::Test)
}

fn same_item(left: &CallHierarchyItem, right: &CallHierarchyItem) -> bool {
    left.buffer == right.buffer
        && left.selection_range == right.selection_range
        && left.name == right.name
}

fn active_buffer(workspace: &Workspace, cx: &App) -> Option<WeakEntity<Buffer>> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    Some(buffer.downgrade())
}

/// Where `name` is written, on the line of `start` or one of the next few: workspace symbols
/// start at the declaration, which may begin with attributes.
fn name_position(
    snapshot: &language::BufferSnapshot,
    start: language::Point,
    name: &str,
) -> language::Point {
    let last_row = (start.row + 5).min(snapshot.max_point().row);
    for row in start.row..=last_row {
        let line_start = if row == start.row {
            start
        } else {
            language::Point::new(row, 0)
        };
        let line = snapshot
            .text_for_range(line_start..language::Point::new(row, snapshot.line_len(row)))
            .collect::<String>();
        if let Some(column) = line.find(name) {
            return language::Point::new(row, line_start.column + column as u32);
        }
    }
    start
}

/// Up to five entry points, HTTP handlers and tests among the project's function symbols. Symbols
/// in open files are tagged like trail rows; the rest only by name, to avoid opening files.
fn start_points(project: &Entity<Project>, symbols: Vec<Symbol>, cx: &App) -> Vec<StartPoint> {
    let mut start_points = Vec::new();
    for symbol in symbols {
        if !matches!(
            language::symbol_kind_to_lsp(symbol.kind),
            lsp::SymbolKind::FUNCTION | lsp::SymbolKind::METHOD
        ) {
            continue;
        }
        let SymbolLocation::InProject(path) = symbol.path else {
            continue;
        };
        let position = symbol.range.start.0;
        let tag = match project.read(cx).get_open_buffer(&path, cx) {
            Some(buffer) => {
                let snapshot = buffer.read(cx).snapshot();
                let start = language::Unclipped(position).to_point(&snapshot);
                let name = name_position(&snapshot, start, &symbol.name);
                tag_at(&snapshot, &symbol.name, start.row, name)
            }
            None => detect_tag(&symbol.name, &[], "", false),
        };
        let Some(tag) = tag else {
            continue;
        };
        let name = SharedString::from(symbol.name);
        if start_points
            .iter()
            .any(|start_point: &StartPoint| start_point.name == name && start_point.path == path)
        {
            continue;
        }
        start_points.push(StartPoint {
            name,
            tag: Some(tag),
            file_name: path.path.file_name().unwrap_or_default().to_string().into(),
            path,
            position,
        });
    }
    start_points.sort_by_key(|start_point| match start_point.tag {
        Some(Tag::EntryPoint) => 0,
        Some(Tag::HttpHandler) => 1,
        _ => 2,
    });
    start_points.truncate(5);
    start_points
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::ToPoint as _;
    use futures::StreamExt as _;
    use gpui::{TestAppContext, VisualTestContext};
    use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher, LanguageName};
    use project::FakeFs;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace};

    const SOURCE: &str = "fn main() { helper(); helper(); }\nfn helper() { leaf(); helper(); }\nfn leaf() {}\nfn other() { leaf(); }\n#[test]\nfn other_works() { other(); }\n";

    struct Setup<'a> {
        workspace: Entity<Workspace>,
        panel: Entity<CallHierarchyPanel>,
        editor: Entity<Editor>,
        requests: Arc<Mutex<Vec<String>>>,
        cx: &'a mut VisualTestContext,
    }

    fn item(name: &str, uri: &lsp::Uri, line: u32) -> lsp::CallHierarchyItem {
        lsp::CallHierarchyItem {
            name: name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: Some(format!("fn {name}()")),
            uri: uri.clone(),
            range: lsp::Range {
                start: lsp::Position { line, character: 0 },
                end: lsp::Position {
                    line,
                    character: 10,
                },
            },
            selection_range: lsp::Range {
                start: lsp::Position { line, character: 3 },
                end: lsp::Position {
                    line,
                    character: 3 + name.len() as u32,
                },
            },
            data: None,
        }
    }

    fn line_of(name: &str) -> u32 {
        match name {
            "main" => 0,
            "helper" => 1,
            "leaf" => 2,
            "other_works" => 5,
            _ => 3,
        }
    }

    fn site(line: u32, character: u32) -> lsp::Range {
        lsp::Range {
            start: lsp::Position { line, character },
            end: lsp::Position {
                line,
                character: character + 4,
            },
        }
    }

    /// Callers and callees of [`SOURCE`]: main calls helper twice, helper calls leaf and itself,
    /// other calls leaf, and the test other_works calls other.
    fn incoming(name: &str, uri: &lsp::Uri) -> Vec<lsp::CallHierarchyIncomingCall> {
        let call = |from: &str, sites: Vec<lsp::Range>| lsp::CallHierarchyIncomingCall {
            from: item(from, uri, line_of(from)),
            from_ranges: sites,
        };
        match name {
            "helper" => vec![
                call("main", vec![site(0, 12), site(0, 22)]),
                call("helper", vec![site(1, 22)]),
            ],
            "leaf" => vec![
                call("helper", vec![site(1, 14)]),
                call("other", vec![site(3, 13)]),
            ],
            "other" => vec![call("other_works", vec![site(5, 19)])],
            _ => Vec::new(),
        }
    }

    fn outgoing(name: &str, uri: &lsp::Uri) -> Vec<lsp::CallHierarchyOutgoingCall> {
        let call = |to: &str, sites: Vec<lsp::Range>| lsp::CallHierarchyOutgoingCall {
            to: item(to, uri, line_of(to)),
            from_ranges: sites,
        };
        match name {
            "main" => vec![call("helper", vec![site(0, 12), site(0, 22)])],
            "helper" => vec![
                call("leaf", vec![site(1, 14)]),
                call("helper", vec![site(1, 22)]),
            ],
            "other" => vec![call("leaf", vec![site(3, 13)])],
            "other_works" => vec![call("other", vec![site(5, 19)])],
            _ => Vec::new(),
        }
    }

    async fn setup<'a>(call_hierarchy_support: bool, cx: &'a mut TestAppContext) -> Setup<'a> {
        setup_with(call_hierarchy_support, None, cx).await
    }

    /// With `initialized`, the language server stays starting until it receives.
    async fn setup_with<'a>(
        call_hierarchy_support: bool,
        initialized: Option<futures::channel::oneshot::Receiver<()>>,
        cx: &'a mut TestAppContext,
    ) -> Setup<'a> {
        cx.update(|cx| {
            let _state = AppState::test(cx);
            editor::init(cx);
            call_hierarchy::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/test"),
            json!({"src": {"main.rs": SOURCE, "App.java": "class App {}\n"}}),
        )
        .await;
        let project = Project::test(fs, [path!("/test").as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(Arc::new(Language::new(
                LanguageConfig {
                    name: LanguageName::new("Rust"),
                    matcher: Arc::new(LanguageMatcher {
                        path_suffixes: vec!["rs".into()],
                        ..LanguageMatcher::default()
                    }),
                    ..LanguageConfig::default()
                },
                Some(tree_sitter_rust::LANGUAGE.into()),
            )))
        });
        let capabilities = lsp::ServerCapabilities {
            call_hierarchy_provider: call_hierarchy_support
                .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
            workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
            ..lsp::ServerCapabilities::default()
        };
        let initializer = initialized.map(|initialized| {
            let initialized = Arc::new(Mutex::new(Some(initialized)));
            let capabilities = capabilities.clone();
            Box::new(move |server: &mut lsp::FakeLanguageServer| {
                let initialized = initialized.clone();
                let capabilities = capabilities.clone();
                server.set_request_handler::<lsp::request::Initialize, _, _>(move |_, _| {
                    let initialized = initialized.lock().unwrap().take();
                    let capabilities = capabilities.clone();
                    async move {
                        if let Some(initialized) = initialized {
                            initialized.await.ok();
                        }
                        Ok(lsp::InitializeResult {
                            capabilities,
                            ..lsp::InitializeResult::default()
                        })
                    }
                });
            }) as Box<dyn Fn(&mut lsp::FakeLanguageServer) + Send + Sync>
        });
        let mut fake_servers = project.read_with(cx, |project, _| {
            project.languages().register_fake_lsp(
                "Rust",
                FakeLspAdapter {
                    name: "rust-analyzer",
                    capabilities,
                    initializer,
                    ..FakeLspAdapter::default()
                },
            )
        });

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let worktree_id = workspace.update(cx, |workspace, cx| {
            workspace.project().update(cx, |project, cx| {
                project.worktrees(cx).next().unwrap().read(cx).id()
            })
        });
        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(
                    (worktree_id, rel_path("src/main.rs")),
                    None,
                    true,
                    window,
                    cx,
                )
            })
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap();
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| CallHierarchyPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let fake_server = fake_servers.next().await.unwrap();
        let uri = lsp::Uri::from_file_path(path!("/test/src/main.rs")).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            let uri = uri.clone();
            move |params, _| {
                let uri = uri.clone();
                async move {
                    let line = params.text_document_position_params.position.line;
                    let name = match line {
                        0 => "main",
                        1 => "helper",
                        2 => "leaf",
                        3 => "other",
                        5 => "other_works",
                        _ => return Ok(None),
                    };
                    Ok(Some(vec![item(name, &uri, line)]))
                }
            }
        });
        fake_server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
            let uri = uri.clone();
            let requests = requests.clone();
            move |params, _| {
                let uri = uri.clone();
                requests
                    .lock()
                    .unwrap()
                    .push(format!("callers of {}", params.item.name));
                async move { Ok(Some(incoming(&params.item.name, &uri))) }
            }
        });
        fake_server.set_request_handler::<lsp::request::WorkspaceSymbolRequest, _, _>({
            let uri = uri.clone();
            move |_, _| {
                // other_works's declaration starts at its `#[test]` attribute.
                let symbols = [("main", 0), ("helper", 1), ("leaf", 2), ("other_works", 4)]
                    .into_iter()
                    .map(|(name, line)| {
                        #[allow(deprecated)]
                        lsp::SymbolInformation {
                            name: name.to_string(),
                            kind: lsp::SymbolKind::FUNCTION,
                            tags: None,
                            deprecated: None,
                            location: lsp::Location {
                                uri: uri.clone(),
                                range: site(line, 0),
                            },
                            container_name: None,
                        }
                    })
                    .collect();
                async move { Ok(Some(lsp::WorkspaceSymbolResponse::Flat(symbols))) }
            }
        });
        fake_server.set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
            let requests = requests.clone();
            move |params, _| {
                let uri = uri.clone();
                requests
                    .lock()
                    .unwrap()
                    .push(format!("calls of {}", params.item.name));
                async move { Ok(Some(outgoing(&params.item.name, &uri))) }
            }
        });
        cx.executor().run_until_parked();
        Setup {
            workspace,
            panel,
            editor,
            requests,
            cx,
        }
    }

    impl Setup<'_> {
        fn move_cursor(&mut self, line: u32, column: u32) {
            self.editor.update_in(self.cx, |editor, window, cx| {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                    let point = language::Point::new(line, column);
                    s.select_ranges([point..point])
                });
            });
        }

        fn show_in_panel(&mut self, line: u32) {
            self.move_cursor(line, 4);
            self.editor.update_in(self.cx, |editor, window, cx| {
                window.focus(&editor.focus_handle(cx), cx)
            });
            self.cx.dispatch_action(ShowInPanel);
            self.cx.run_until_parked();
        }

        fn take_requests(&self) -> Vec<String> {
            std::mem::take(&mut *self.requests.lock().unwrap())
        }

        fn node(&mut self, name: &str) -> usize {
            self.panel.read_with(self.cx, |panel, cx| {
                panel
                    .rows(cx)
                    .into_iter()
                    .find_map(|row| match row {
                        Row::Node(ix) if panel.nodes()[ix].call.item.name == name => Some(ix),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("no row for {name}"))
            })
        }

        fn expand(&mut self, name: &str) {
            let node = self.node(name);
            self.panel
                .update_in(self.cx, |panel, window, cx| panel.expand(node, window, cx));
            self.cx.run_until_parked();
        }

        fn collapse(&mut self, name: &str) {
            let node = self.node(name);
            self.panel
                .update(self.cx, |panel, cx| panel.collapse(node, cx));
            self.cx.run_until_parked();
        }

        fn selected_row(&mut self) -> Option<usize> {
            self.panel.read_with(self.cx, |panel, cx| {
                let selected = panel.selected?;
                panel
                    .rows(cx)
                    .iter()
                    .position(|row| *row == Row::Node(selected))
            })
        }

        fn root(&mut self) -> Option<String> {
            self.panel.read_with(self.cx, |panel, _| {
                panel
                    .nodes()
                    .first()
                    .map(|root| root.call.item.name.clone())
            })
        }

        fn is_empty(&mut self) -> bool {
            self.panel
                .read_with(self.cx, |panel, _| matches!(panel.content, Content::Empty))
        }

        fn not_found(&mut self) -> Option<(String, String)> {
            self.panel.read_with(self.cx, |panel, _| {
                panel
                    .not_found
                    .as_ref()
                    .map(|(title, body)| (title.to_string(), body.to_string()))
            })
        }

        /// The message the empty panel shows instead of where to start, if any.
        fn problem(&mut self) -> Option<(String, String)> {
            self.panel.read_with(self.cx, |panel, _| {
                panel
                    .server_problem
                    .as_ref()
                    .map(|problem| (problem.title(), problem.body()))
            })
        }

        fn open_panel(&mut self) {
            self.workspace.update_in(self.cx, |workspace, window, cx| {
                workspace.open_panel::<CallHierarchyPanel>(window, cx)
            });
            self.cx.run_until_parked();
        }

        fn open_file(&mut self, path: &'static str) {
            let worktree_id = self.workspace.update(self.cx, |workspace, cx| {
                workspace.project().update(cx, |project, cx| {
                    project.worktrees(cx).next().unwrap().read(cx).id()
                })
            });
            self.workspace
                .update_in(self.cx, |workspace, window, cx| {
                    workspace.open_path((worktree_id, rel_path(path)), None, true, window, cx)
                })
                .detach();
            self.cx.run_until_parked();
        }

        /// Recent functions, then start points, as `name [tag]`.
        fn start_rows(&mut self) -> (Vec<String>, Vec<String>) {
            self.panel.read_with(self.cx, |panel, _| {
                let names = |start_points: &[StartPoint]| {
                    start_points
                        .iter()
                        .map(|start_point| match start_point.tag {
                            Some(tag) => format!("{} [{}]", start_point.name, tag.label()),
                            None => start_point.name.to_string(),
                        })
                        .collect()
                };
                (
                    names(&panel.recent),
                    names(panel.start_points.as_deref().unwrap_or_default()),
                )
            })
        }

        /// The visible rows, two spaces of indent per level; call-site lines show their line
        /// number, with `>` on the call.
        fn tree(&mut self) -> Vec<String> {
            self.panel.read_with(self.cx, |panel, cx| {
                let nodes = panel.nodes();
                panel
                    .rows(cx)
                    .into_iter()
                    .map(|row| match row {
                        Row::Node(ix) => {
                            let node = &nodes[ix];
                            let mut text =
                                format!("{}{}", "  ".repeat(node.depth - 1), node.call.item.name);
                            if node.call.site_count > 1 {
                                text.push_str(&format!(" ×{}", node.call.site_count));
                            }
                            if node.recursive {
                                text.push_str(" ↻");
                            }
                            if let Some(tag) = node.tag {
                                text.push_str(&format!(" [{}]", tag.label()));
                            }
                            text
                        }
                        Row::Loading { depth } => format!("{}Loading…", "  ".repeat(depth - 1)),
                        Row::CallSite { node_ix, row } => {
                            let node = &nodes[node_ix];
                            let snapshot = node.call.target.buffer.read(cx).snapshot();
                            let line = snapshot
                                .text_for_range(
                                    language::Point::new(row, 0)
                                        ..language::Point::new(row, snapshot.line_len(row)),
                                )
                                .collect::<String>();
                            let mark = if row == node.call_row { ">" } else { "" };
                            format!("{}| {}{mark} {line}", "  ".repeat(node.depth), row + 1)
                        }
                    })
                    .collect()
            })
        }

        fn click(&mut self, selector: &'static str) {
            let bounds = self
                .cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"));
            self.cx
                .simulate_click(bounds.center(), gpui::Modifiers::none());
            self.cx.run_until_parked();
        }

        fn right_click(&mut self, selector: &'static str) {
            let bounds = self
                .cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"));
            let position = bounds.center();
            self.cx.simulate_mouse_down(
                position,
                gpui::MouseButton::Right,
                gpui::Modifiers::none(),
            );
            self.cx
                .simulate_mouse_up(position, gpui::MouseButton::Right, gpui::Modifiers::none());
            self.cx.run_until_parked();
        }

        fn select(&mut self, name: &str) {
            let node = self.node(name);
            self.panel
                .update(self.cx, |panel, cx| panel.select(node, cx));
            self.cx.run_until_parked();
        }

        fn summary(&mut self) -> Option<String> {
            self.panel.read_with(self.cx, |panel, _| panel.summary())
        }

        /// The breadcrumb's names, in the order it shows them.
        fn crumbs(&mut self) -> Vec<String> {
            self.panel.read_with(self.cx, |panel, _| {
                panel
                    .trail_in_call_order()
                    .into_iter()
                    .map(|node_ix| panel.nodes()[node_ix].call.item.name.clone())
                    .collect()
            })
        }

        fn double_click(&mut self, selector: &'static str) {
            let position = self
                .cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"))
                .center();
            for click_count in [1, 2] {
                self.cx.simulate_event(gpui::MouseDownEvent {
                    position,
                    modifiers: gpui::Modifiers::none(),
                    button: gpui::MouseButton::Left,
                    click_count,
                    first_mouse: false,
                });
                self.cx.simulate_event(gpui::MouseUpEvent {
                    position,
                    modifiers: gpui::Modifiers::none(),
                    button: gpui::MouseButton::Left,
                    click_count,
                });
            }
            self.cx.run_until_parked();
        }

        fn bind_panel_keys(&mut self) {
            self.cx.update(|_, cx| {
                cx.bind_keys([
                    gpui::KeyBinding::new("down", menu::SelectNext, Some("CallHierarchyPanel")),
                    gpui::KeyBinding::new("up", menu::SelectPrevious, Some("CallHierarchyPanel")),
                    gpui::KeyBinding::new("right", menu::SelectChild, Some("CallHierarchyPanel")),
                    gpui::KeyBinding::new("left", menu::SelectParent, Some("CallHierarchyPanel")),
                    gpui::KeyBinding::new("enter", menu::Confirm, Some("CallHierarchyPanel")),
                    gpui::KeyBinding::new("space", ToggleCallSite, Some("CallHierarchyPanel")),
                    gpui::KeyBinding::new("alt-left", MakeRoot, Some("CallHierarchyPanel")),
                ]);
            });
        }

        fn cursor(&mut self) -> language::Point {
            let editor = self
                .workspace
                .update(self.cx, |workspace, cx| {
                    workspace.active_item_as::<Editor>(cx)
                })
                .expect("an editor is active");
            editor.update(self.cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                editor.selections.newest_anchor().head().to_point(&snapshot)
            })
        }
    }

    #[gpui::test]
    async fn show_in_panel_roots_the_tree_and_lists_callers(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        assert!(setup.is_empty());

        setup.show_in_panel(1);
        assert_eq!(setup.root().as_deref(), Some("helper"));
        assert_eq!(setup.tree(), ["main ×2 [entry point]", "helper ↻"]);
        assert_eq!(setup.take_requests(), ["callers of helper"]);
    }

    #[gpui::test]
    async fn children_load_on_first_expand_and_stay_cached(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(2);
        assert_eq!(setup.tree(), ["helper", "other"]);
        assert_eq!(setup.take_requests(), ["callers of leaf"]);

        setup.expand("helper");
        assert_eq!(setup.take_requests(), ["callers of helper"]);
        assert_eq!(
            setup.tree(),
            ["helper", "  main ×2 [entry point]", "  helper ↻", "other"]
        );

        setup.expand("other");
        assert_eq!(setup.take_requests(), ["callers of other"]);
        assert_eq!(
            setup.tree(),
            [
                "helper",
                "  main ×2 [entry point]",
                "  helper ↻",
                "other",
                "  other_works [test]"
            ]
        );

        setup.collapse("helper");
        assert_eq!(setup.tree(), ["helper", "other", "  other_works [test]"]);
        setup.expand("helper");
        assert_eq!(setup.take_requests(), Vec::<String>::new());
        assert_eq!(
            setup.tree(),
            [
                "helper",
                "  main ×2 [entry point]",
                "  helper ↻",
                "other",
                "  other_works [test]"
            ]
        );
    }

    #[gpui::test]
    async fn expanding_shows_loading_until_the_server_answers(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(2);
        let node = setup.node("helper");
        setup
            .panel
            .update_in(setup.cx, |panel, window, cx| panel.expand(node, window, cx));
        assert_eq!(setup.tree(), ["helper", "  Loading…", "other"]);
        setup.cx.run_until_parked();
        assert_eq!(
            setup.tree(),
            ["helper", "  main ×2 [entry point]", "  helper ↻", "other"]
        );
    }

    #[gpui::test]
    async fn switching_to_calls_shows_outgoing_calls(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(0);
        assert_eq!(setup.tree(), Vec::<String>::new());
        assert_eq!(setup.summary().as_deref(), Some("No callers"));
        setup.take_requests();

        setup.panel.update_in(setup.cx, |panel, window, cx| {
            panel.set_mode(CallHierarchyMode::Outgoing, window, cx)
        });
        setup.cx.run_until_parked();
        assert_eq!(setup.root().as_deref(), Some("main"));
        assert_eq!(setup.take_requests(), ["calls of main"]);
        assert_eq!(setup.tree(), ["helper ×2"]);

        setup.expand("helper");
        assert_eq!(setup.tree(), ["helper ×2", "  leaf", "  helper ↻"]);
    }

    #[gpui::test]
    async fn a_call_with_no_calls_is_a_leaf_not_a_no_callers_row(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(0);
        setup.panel.update_in(setup.cx, |panel, window, cx| {
            panel.set_mode(CallHierarchyMode::Outgoing, window, cx)
        });
        setup.cx.run_until_parked();
        setup.expand("helper");
        let leaf = setup.node("leaf");
        let expandable = |setup: &mut Setup| {
            setup
                .panel
                .read_with(setup.cx, |panel, _| panel.can_expand(&panel.nodes()[leaf]))
        };
        assert!(expandable(&mut setup), "unknown until fetched");

        setup.expand("leaf");
        assert_eq!(
            setup.take_requests().last().map(String::as_str),
            Some("calls of leaf")
        );
        assert_eq!(setup.tree(), ["helper ×2", "  leaf", "  helper ↻"]);
        assert!(!expandable(&mut setup), "a known leaf has no chevron");
    }

    #[gpui::test]
    async fn entry_points_and_tests_are_leaves_without_a_fetch(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(1);
        setup.take_requests();
        setup.expand("main");
        assert_eq!(setup.take_requests(), Vec::<String>::new());
        assert_eq!(setup.tree(), ["main ×2 [entry point]", "helper ↻"]);
    }

    #[gpui::test]
    async fn recursive_calls_are_marked_and_not_expandable(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(1);
        assert_eq!(setup.tree(), ["main ×2 [entry point]", "helper ↻"]);
        let recursive = setup.node("helper");
        setup.take_requests();
        setup.panel.update_in(setup.cx, |panel, window, cx| {
            panel.expand(recursive, window, cx);
        });
        setup.cx.run_until_parked();
        assert_eq!(setup.take_requests(), Vec::<String>::new());
        assert_eq!(setup.tree(), ["main ×2 [entry point]", "helper ↻"]);
    }

    #[gpui::test]
    async fn header_summarizes_callers_entry_points_and_tests(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(1);
        assert_eq!(
            setup.summary().as_deref(),
            Some("2 callers · 1 entry point")
        );

        setup.show_in_panel(2);
        assert_eq!(setup.summary().as_deref(), Some("2 callers"));
        setup.expand("other");
        assert_eq!(setup.summary().as_deref(), Some("2 callers · 1 test"));
        setup.expand("helper");
        assert_eq!(
            setup.summary().as_deref(),
            Some("2 callers · 1 entry point · 1 test")
        );

        setup.panel.update_in(setup.cx, |panel, window, cx| {
            panel.set_mode(CallHierarchyMode::Outgoing, window, cx)
        });
        setup.cx.run_until_parked();
        assert_eq!(setup.summary().as_deref(), Some("No calls"));
    }

    #[gpui::test]
    async fn header_marks_a_root_changed_in_an_open_review(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.cx.update(|_, cx| {
            set_hooks(
                CallTrailHooks {
                    changed_in_review: Some(Rc::new(|_, path, rows, _| {
                        (path == "src/main.rs" && rows.contains(&1)).then_some(214)
                    })),
                    explain: None,
                },
                cx,
            )
        });
        setup.show_in_panel(1);
        let changed = |setup: &mut Setup| {
            setup
                .panel
                .read_with(setup.cx, |panel, _| panel.root_header.changed_in_pr)
        };
        assert_eq!(changed(&mut setup), Some(214));
        assert!(setup.cx.debug_bounds("call-trail-pr-chip").is_some());

        setup.show_in_panel(2);
        assert_eq!(changed(&mut setup), None);
        assert!(setup.cx.debug_bounds("call-trail-pr-chip").is_none());
    }

    #[gpui::test]
    async fn breadcrumb_follows_the_selection_in_call_order(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(2);
        assert_eq!(setup.crumbs(), ["leaf"]);
        setup.expand("helper");
        setup.select("main");
        assert_eq!(setup.crumbs(), ["main", "helper", "leaf"]);
        for selector in [
            "call-trail-crumb-main",
            "call-trail-crumb-helper",
            "call-trail-crumb-leaf",
        ] {
            assert!(setup.cx.debug_bounds(selector).is_some(), "{selector}");
        }

        setup.click("call-trail-crumb-helper");
        assert_eq!(setup.crumbs(), ["helper", "leaf"]);
        assert_eq!(setup.selected_row(), Some(0));

        setup.show_in_panel(0);
        setup.panel.update_in(setup.cx, |panel, window, cx| {
            panel.set_mode(CallHierarchyMode::Outgoing, window, cx)
        });
        setup.cx.run_until_parked();
        setup.expand("helper");
        setup.select("leaf");
        assert_eq!(setup.crumbs(), ["main", "helper", "leaf"]);
    }

    #[gpui::test]
    async fn call_site_shows_the_call_line_with_context(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.bind_panel_keys();
        setup.show_in_panel(2);
        setup.click("call-trail-sites-other");
        assert_eq!(
            setup.tree(),
            [
                "helper",
                "other",
                "  | 3 fn leaf() {}",
                "  | 4> fn other() { leaf(); }",
                "  | 5 #[test]",
            ]
        );

        setup.click("call-trail-site-other-4");
        assert_eq!(setup.cursor(), language::Point::new(3, 13));
    }

    #[gpui::test]
    async fn space_toggles_the_selected_call_site(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.bind_panel_keys();
        setup.show_in_panel(1);
        setup.cx.simulate_keystrokes("down space");
        assert_eq!(
            setup.tree(),
            [
                "main ×2 [entry point]",
                "  | 1> fn main() { helper(); helper(); }",
                "  | 2 fn helper() { leaf(); helper(); }",
                "helper ↻",
            ]
        );
        setup.cx.simulate_keystrokes("space");
        assert_eq!(setup.tree(), ["main ×2 [entry point]", "helper ↻"]);
    }

    #[gpui::test]
    async fn clicking_a_row_opens_the_call_site(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(2);
        setup.click("call-trail-other");
        assert_eq!(setup.cursor(), language::Point::new(3, 13));
        setup.click("call-trail-helper");
        assert_eq!(setup.cursor(), language::Point::new(1, 14));
    }

    #[gpui::test]
    async fn keyboard_moves_expands_collapses_and_opens(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.bind_panel_keys();
        setup.show_in_panel(2);
        let focused = setup.panel.update_in(setup.cx, |panel, window, _| {
            panel.focus_handle.is_focused(window)
        });
        assert!(focused, "Show in panel focuses the panel");

        setup.cx.simulate_keystrokes("down right");
        setup.cx.run_until_parked();
        assert_eq!(
            setup.tree(),
            ["helper", "  main ×2 [entry point]", "  helper ↻", "other"]
        );
        assert_eq!(setup.selected_row(), Some(0));
        setup.cx.simulate_keystrokes("right down");
        assert_eq!(setup.selected_row(), Some(2));
        setup.cx.simulate_keystrokes("up");
        assert_eq!(setup.selected_row(), Some(1));
        setup.cx.simulate_keystrokes("left");
        assert_eq!(setup.selected_row(), Some(0));
        setup.cx.simulate_keystrokes("left");
        assert_eq!(setup.tree(), ["helper", "other"]);
        setup.cx.simulate_keystrokes("down enter");
        setup.cx.run_until_parked();
        assert_eq!(setup.cursor(), language::Point::new(3, 13));
    }

    #[gpui::test]
    async fn make_root_re_roots_the_tree(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(2);
        setup.take_requests();
        setup.right_click("call-trail-helper");
        setup.click("MENU_ITEM-Make Root");
        assert_eq!(setup.root().as_deref(), Some("helper"));
        assert_eq!(setup.take_requests(), ["callers of helper"]);
        assert_eq!(setup.tree(), ["main ×2 [entry point]", "helper ↻"]);

        setup.right_click("call-trail-main");
        setup.click("MENU_ITEM-Copy Name");
        let copied = setup
            .cx
            .update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(copied.as_deref(), Some("main"));
    }

    #[gpui::test]
    async fn option_left_makes_the_selection_the_root(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.bind_panel_keys();
        setup.show_in_panel(2);
        setup.take_requests();
        setup.cx.simulate_keystrokes("down alt-left");
        setup.cx.run_until_parked();
        assert_eq!(setup.root().as_deref(), Some("helper"));
        assert_eq!(setup.take_requests(), ["callers of helper"]);
        assert_eq!(setup.crumbs(), ["helper"]);
    }

    #[gpui::test]
    async fn map_draws_the_explored_tree_and_opens_a_clicked_node(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(2);
        setup.expand("helper");
        setup.click("call-trail-view-map");
        let layout = setup.panel.read_with(setup.cx, |panel, _| {
            map::layout(panel.nodes())
                .into_iter()
                .map(|placed| {
                    let node = &panel.nodes()[placed.node_ix];
                    let parent = node
                        .parent
                        .map(|parent| panel.nodes()[parent].call.item.name.clone());
                    (
                        node.call.item.name.clone(),
                        placed.column,
                        placed.row,
                        parent,
                    )
                })
                .collect::<Vec<_>>()
        });
        let named = |name: &str, column, row, parent: Option<&str>| {
            (name.to_string(), column, row, parent.map(str::to_string))
        };
        assert_eq!(
            layout,
            [
                named("main", 2, 0., Some("helper")),
                named("helper", 2, 1., Some("helper")),
                named("helper", 1, 0.5, Some("leaf")),
                named("other", 1, 2., Some("leaf")),
                named("leaf", 0, 1.25, None),
            ]
        );
        for selector in [
            "call-trail-map-main",
            "call-trail-map-other",
            "call-trail-map-leaf",
        ] {
            assert!(setup.cx.debug_bounds(selector).is_some(), "{selector}");
        }

        setup.click("call-trail-map-other");
        assert_eq!(setup.cursor(), language::Point::new(3, 13));
        assert_eq!(setup.crumbs(), ["other", "leaf"]);

        setup.take_requests();
        setup.double_click("call-trail-map-other");
        assert_eq!(setup.take_requests(), ["callers of other"]);
        assert!(
            setup
                .cx
                .debug_bounds("call-trail-map-other_works")
                .is_some(),
            "double-click expands the node"
        );
    }

    #[gpui::test]
    async fn explain_sends_the_selected_trail_only_when_clicked(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        let prompts = Rc::new(std::cell::RefCell::new(Vec::new()));
        setup.cx.update(|_, cx| {
            set_hooks(
                CallTrailHooks {
                    changed_in_review: None,
                    explain: Some(Rc::new({
                        let prompts = prompts.clone();
                        move |_, prompt, _, _| prompts.borrow_mut().push(prompt)
                    })),
                },
                cx,
            )
        });
        setup.show_in_panel(2);
        setup.expand("helper");
        setup.select("main");
        assert!(prompts.borrow().is_empty(), "nothing runs until clicked");

        setup.click("call-trail-explain");
        let prompts = prompts.borrow();
        let prompt = prompts.first().expect("the trail was sent");
        let expected = [
            "1. main (src/main.rs:1)",
            "   calls helper at src/main.rs:1: `fn main() { helper(); helper(); }`",
            "2. helper (src/main.rs:2)",
            "   calls leaf at src/main.rs:2: `fn helper() { leaf(); helper(); }`",
            "3. leaf (src/main.rs:3)",
        ];
        for line in expected {
            assert!(prompt.contains(line), "{line:?} in {prompt}");
        }
    }

    #[gpui::test]
    async fn keep_open_moves_the_popup_hierarchy_into_the_panel(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.move_cursor(0, 4);
        setup.editor.update_in(setup.cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx)
        });
        setup.cx.dispatch_action(call_hierarchy::ShowOutgoingCalls);
        setup.cx.run_until_parked();
        setup.take_requests();

        setup.click("call-hierarchy-keep-open");
        let popup_open = setup.workspace.update(setup.cx, |workspace, cx| {
            workspace.active_modal::<CallHierarchyView>(cx).is_some()
        });
        assert!(!popup_open, "Keep Open closes the popup");
        assert_eq!(setup.root().as_deref(), Some("main"));
        let mode = setup.panel.read_with(setup.cx, |panel, _| panel.mode);
        assert_eq!(mode, CallHierarchyMode::Outgoing);
        assert_eq!(setup.tree(), ["helper ×2"]);
    }

    #[gpui::test]
    async fn closing_the_panel_drops_in_flight_fetches(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(2);
        setup.take_requests();
        let node = setup.node("helper");
        setup
            .panel
            .update_in(setup.cx, |panel, window, cx| panel.expand(node, window, cx));
        setup
            .workspace
            .update_in(setup.cx, |workspace, window, cx| {
                workspace.close_panel::<CallHierarchyPanel>(window, cx)
            });
        setup.cx.run_until_parked();
        let (expanded, children_loaded) = setup.panel.read_with(setup.cx, |panel, _| {
            let node = &panel.nodes()[node];
            (node.expanded, node.children.is_some())
        });
        assert!(!expanded && !children_loaded, "nothing lands after close");
        assert_eq!(setup.tree(), ["helper", "other"]);
    }

    #[gpui::test]
    async fn reopening_fetches_the_roots_calls_that_closing_dropped(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(0);
        setup.take_requests();
        setup.panel.update_in(setup.cx, |panel, window, cx| {
            panel.set_mode(CallHierarchyMode::Outgoing, window, cx)
        });
        setup
            .workspace
            .update_in(setup.cx, |workspace, window, cx| {
                workspace.close_panel::<CallHierarchyPanel>(window, cx)
            });
        setup.cx.run_until_parked();
        assert_eq!(setup.tree(), ["Loading…"]);

        setup
            .workspace
            .update_in(setup.cx, |workspace, window, cx| {
                workspace.open_panel::<CallHierarchyPanel>(window, cx)
            });
        setup.cx.run_until_parked();
        assert_eq!(setup.take_requests(), ["calls of main"]);
        assert_eq!(setup.tree(), ["helper ×2"]);
    }

    #[gpui::test]
    async fn the_key_works_while_the_panel_has_focus(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.move_cursor(1, 4);
        setup.panel.update_in(setup.cx, |panel, window, cx| {
            window.focus(&panel.focus_handle(cx), cx)
        });
        setup.cx.dispatch_action(ShowInPanel);
        setup.cx.run_until_parked();
        assert_eq!(setup.root().as_deref(), Some("helper"));
    }

    #[gpui::test]
    async fn explains_when_there_is_nothing_to_show(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(4);
        assert!(setup.is_empty());
        assert_eq!(setup.problem(), None);
        assert_eq!(setup.root(), None);
        assert!(setup.cx.debug_bounds("call-trail-start").is_some());
        assert_eq!(
            setup.not_found(),
            Some((
                "No function at the cursor".to_string(),
                "Put the cursor on a function's name.".to_string()
            ))
        );
        assert!(setup.cx.debug_bounds("call-trail-not-found").is_some());

        setup.show_in_panel(1);
        assert_eq!(setup.not_found(), None, "a trace clears the note");
    }

    #[gpui::test]
    async fn empty_panel_offers_recent_functions_and_start_points(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.open_panel();
        assert!(setup.cx.debug_bounds("call-trail-start").is_some());
        assert_eq!(
            setup.start_rows(),
            (
                Vec::new(),
                vec![
                    "main [entry point]".to_string(),
                    "other_works [test]".to_string()
                ]
            )
        );

        setup.show_in_panel(1);
        setup.show_in_panel(2);
        setup.show_in_panel(4);
        assert!(setup.is_empty());
        assert_eq!(setup.start_rows().0, ["leaf", "helper"]);

        setup.click("call-trail-start-point-other_works");
        assert_eq!(setup.root().as_deref(), Some("other_works"));
        assert_eq!(
            setup.start_rows().0,
            ["other_works [test]", "leaf", "helper"]
        );

        setup.show_in_panel(4);
        setup.click("call-trail-recent-helper");
        assert_eq!(setup.root().as_deref(), Some("helper"));
        assert_eq!(setup.tree(), ["main ×2 [entry point]", "helper ↻"]);
        assert_eq!(
            setup.start_rows().0,
            ["helper", "other_works [test]", "leaf"]
        );
    }

    #[gpui::test]
    async fn waits_for_a_starting_server_then_traces_the_function(cx: &mut TestAppContext) {
        let (initialized_tx, initialized_rx) = futures::channel::oneshot::channel();
        let mut setup = setup_with(true, Some(initialized_rx), cx).await;
        setup.open_panel();
        assert_eq!(
            setup.problem(),
            Some((
                "No call hierarchy for Rust yet".to_string(),
                "rust-analyzer is starting… This panel fills in as soon as it's ready.".to_string()
            ))
        );

        setup.show_in_panel(1);
        assert_eq!(setup.root(), None);
        assert!(setup.cx.debug_bounds("call-trail-server-problem").is_some());
        assert!(setup.cx.debug_bounds("call-trail-set-up-server").is_some());

        initialized_tx.send(()).unwrap();
        setup.cx.run_until_parked();
        assert_eq!(setup.problem(), None);
        assert_eq!(setup.root().as_deref(), Some("helper"));
        assert_eq!(setup.tree(), ["main ×2 [entry point]", "helper ↻"]);
    }

    #[gpui::test]
    async fn explains_a_server_that_failed_to_start(cx: &mut TestAppContext) {
        let (_initialized_tx, initialized_rx) = futures::channel::oneshot::channel();
        let mut setup = setup_with(true, Some(initialized_rx), cx).await;
        setup.open_panel();
        setup.workspace.read_with(setup.cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .languages()
                .update_lsp_binary_status(
                    LanguageServerName::new_static("rust-analyzer"),
                    language::BinaryStatus::Failed {
                        error: "could not start".to_string(),
                    },
                )
        });
        setup.cx.run_until_parked();
        assert_eq!(
            setup.problem(),
            Some((
                "rust-analyzer failed to start".to_string(),
                "The call hierarchy needs it running.".to_string()
            ))
        );
        assert!(setup.cx.debug_bounds("call-trail-set-up-server").is_some());
    }

    #[gpui::test]
    async fn explains_a_language_without_a_server(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.open_panel();
        assert_eq!(setup.problem(), None);
        setup.open_file("src/App.java");
        assert_eq!(
            setup.problem(),
            Some((
                "No language server for Java".to_string(),
                "The call hierarchy needs one to find callers.".to_string()
            ))
        );
        assert!(setup.cx.debug_bounds("call-trail-set-up-server").is_some());
    }

    #[gpui::test]
    async fn explains_an_unsupported_language_server(cx: &mut TestAppContext) {
        let mut setup = setup(false, cx).await;
        setup.open_panel();
        let unsupported = Some((
            "No call hierarchy for Rust".to_string(),
            "rust-analyzer can't find a function's callers and calls.".to_string(),
        ));
        assert_eq!(setup.problem(), unsupported);
        assert!(setup.cx.debug_bounds("call-trail-server-problem").is_some());

        setup.show_in_panel(1);
        assert_eq!(setup.problem(), unsupported);
        assert_eq!(setup.root(), None);
    }

    #[gpui::test]
    async fn set_up_language_server_opens_the_setup_help(cx: &mut TestAppContext) {
        let mut setup = setup(false, cx).await;
        let shown = Rc::new(std::cell::Cell::new(0));
        setup.workspace.update(setup.cx, |workspace, _| {
            let shown = shown.clone();
            workspace.register_action(
                move |_, _: &photon_navigation::ShowLanguageServerHelp, _, _| {
                    shown.set(shown.get() + 1)
                },
            );
        });
        setup.open_panel();
        setup.click("call-trail-set-up-server");
        assert_eq!(shown.get(), 1);
    }

    #[test]
    fn a_failed_java_server_says_what_it_needs() {
        let hint = photon_navigation::known_fix(
            "Java",
            Some("This version of the Java Runtime only recognizes class file versions up to 61.0"),
        )
        .map(|fix| fix.hint);
        let problem = ServerProblem::Failed {
            server: "jdtls".into(),
            hint,
        };
        assert_eq!(problem.title(), "jdtls failed to start");
        assert_eq!(problem.body(), "jdtls needs Java 21 or newer.");
    }

    #[test]
    fn tags_come_from_names_attributes_and_signatures() {
        let tag = |name: &str, attributes: &[&str], signature: &str| {
            let attributes = attributes.iter().map(|a| a.to_string()).collect::<Vec<_>>();
            detect_tag(name, &attributes, signature, false)
        };
        assert_eq!(tag("main", &[], "fn main()"), Some(Tag::EntryPoint));
        assert_eq!(tag("works", &["#[test]"], "fn works()"), Some(Tag::Test));
        assert_eq!(
            tag("works", &["#[tokio::test]"], "async fn works()"),
            Some(Tag::Test)
        );
        assert_eq!(tag("test_login", &[], "def test_login():"), Some(Tag::Test));
        assert_eq!(
            tag(
                "charge_invoice",
                &[],
                "pub async fn charge_invoice(State(state): State<AppState>, Path(id): Path<InvoiceId>) -> Result<Json<ChargeResponse>, (StatusCode, String)>"
            ),
            Some(Tag::HttpHandler)
        );
        assert_eq!(
            tag("index", &["#[get(\"/\")]"], "async fn index()"),
            Some(Tag::HttpHandler)
        );
        assert_eq!(
            tag(
                "charge",
                &[],
                "pub async fn charge(&self, invoice: &Invoice) -> Result<Receipt, ChargeError>"
            ),
            None
        );
        assert_eq!(
            detect_tag("helper", &[], "fn helper()", true),
            Some(Tag::Test)
        );
    }

    #[test]
    fn owner_names_come_from_impl_and_class_outlines() {
        assert_eq!(owner_name("impl Gateway"), Some("Gateway"));
        assert_eq!(
            owner_name("impl<T> Display for Wrapper<T>"),
            Some("Wrapper")
        );
        assert_eq!(owner_name("class Gateway"), Some("Gateway"));
        assert_eq!(owner_name("pub async fn charge(&self)"), None);
        assert_eq!(owner_name("mod tests"), None);
    }
}
