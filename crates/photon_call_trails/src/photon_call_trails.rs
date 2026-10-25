use call_hierarchy::{Call, CallHierarchyMode, CallHierarchyView};
use collections::HashMap;
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, KeyContext, MouseDownEvent, Pixels, Point, Render,
    ScrollStrategy, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, actions,
    anchored, deferred, px, uniform_list,
};
use language::{Buffer, ToPoint as _};
use project::{CallHierarchyItem, Project};
use ui::{
    ContextMenu, Disclosure, ListItem, ToggleButtonGroup, ToggleButtonGroupStyle,
    ToggleButtonSimple, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    photon_call_trails,
    [
        /// Shows the call hierarchy of the symbol under the cursor in a panel.
        ShowInPanel,
        /// Toggles focus on the call hierarchy panel.
        ToggleFocus,
    ]
);

const PANEL_NAME: &str = "Call Hierarchy";
const NO_FUNCTION: &str = "Put the cursor on a function to see its callers.";
const UNSUPPORTED: &str = "This file's language server doesn't support call hierarchy.";

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
        None => panel.show_message(NO_FUNCTION, cx),
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
    mode: CallHierarchyMode,
    content: Content,
    selected: Option<usize>,
    root_task: Option<Task<()>>,
    child_tasks: HashMap<usize, Task<()>>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
}

enum Content {
    Message(&'static str),
    LoadingRoot,
    /// `nodes[0]` is the root; the rows show its descendants.
    Tree(Vec<Node>),
}

struct Node {
    call: Call,
    parent: Option<usize>,
    depth: usize,
    /// `None` until first expanded and fetched.
    children: Option<Vec<usize>>,
    expanded: bool,
    recursive: bool,
}

impl Node {
    fn root(call: Call) -> Self {
        Self {
            call,
            parent: None,
            depth: 0,
            children: None,
            expanded: false,
            recursive: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    Node(usize),
    Loading { depth: usize },
    Empty { depth: usize },
}

impl CallHierarchyPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, _, cx| {
                let project = workspace.project().clone();
                let workspace = cx.entity().downgrade();
                cx.new(|cx| Self::new(workspace, project, cx))
            })
        })
    }

    fn new(workspace: WeakEntity<Workspace>, project: Entity<Project>, cx: &mut App) -> Self {
        Self {
            workspace,
            project,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            mode: CallHierarchyMode::Incoming,
            content: Content::Message(NO_FUNCTION),
            selected: None,
            root_task: None,
            child_tasks: HashMap::default(),
            context_menu: None,
        }
    }

    fn show_message(&mut self, message: &'static str, cx: &mut Context<Self>) {
        self.root_task = None;
        self.child_tasks.clear();
        self.selected = None;
        self.content = Content::Message(message);
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
            self.show_message(NO_FUNCTION, cx);
            return;
        };
        let prepare = self.project.update(cx, |project, cx| {
            project.prepare_call_hierarchy(&buffer, position, cx)
        });
        self.start_loading_root(cx);
        let project = self.project.clone();
        self.root_task = Some(cx.spawn_in(window, async move |this, cx| {
            let item = match prepare.await {
                Ok(items) => items.unwrap_or_default().into_iter().next(),
                Err(error) => {
                    log::error!("failed to prepare call hierarchy: {error:#}");
                    None
                }
            };
            let Some(item) = item else {
                this.update(cx, |this, cx| {
                    let supported = supports_call_hierarchy(&this.project, &buffer, cx);
                    this.show_message(if supported { NO_FUNCTION } else { UNSUPPORTED }, cx);
                })
                .ok();
                return;
            };
            let root = call_hierarchy::call_for_item(item, &project, cx).await;
            this.update_in(cx, |this, window, cx| this.show_root(root, window, cx))
                .ok();
        }));
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
        self.child_tasks.clear();
        self.selected = None;
        self.content = Content::LoadingRoot;
        cx.notify();
    }

    fn show_root(&mut self, root: Call, window: &mut Window, cx: &mut Context<Self>) {
        self.root_task = None;
        self.child_tasks.clear();
        self.selected = None;
        self.content = Content::Tree(vec![Node::root(root)]);
        self.expand(0, window, cx);
    }

    fn set_mode(&mut self, mode: CallHierarchyMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.make_root(0, window, cx);
    }

    fn make_root(&mut self, node_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(node) = self.nodes().get(node_ix) {
            let root = node.call.clone();
            self.show_root(root, window, cx);
        }
    }

    fn nodes(&self) -> &[Node] {
        match &self.content {
            Content::Tree(nodes) => nodes,
            Content::Message(_) | Content::LoadingRoot => &[],
        }
    }

    fn expand(&mut self, node_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self.mode;
        let Content::Tree(nodes) = &mut self.content else {
            return;
        };
        let Some(node) = nodes.get_mut(node_ix).filter(|node| !node.recursive) else {
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
            nodes.push(Node {
                call,
                parent: Some(parent_ix),
                depth,
                children: None,
                expanded: false,
                recursive,
            });
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

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if !self.nodes().is_empty() {
            self.push_rows(0, &mut rows);
        }
        rows
    }

    fn push_rows(&self, node_ix: usize, rows: &mut Vec<Row>) {
        let nodes = self.nodes();
        let Some(node) = nodes.get(node_ix) else {
            return;
        };
        match &node.children {
            None => rows.push(Row::Loading {
                depth: node.depth + 1,
            }),
            Some(children) if children.is_empty() => rows.push(Row::Empty {
                depth: node.depth + 1,
            }),
            Some(children) => {
                for &child in children {
                    rows.push(Row::Node(child));
                    if nodes.get(child).is_some_and(|child| child.expanded) {
                        self.push_rows(child, rows);
                    }
                }
            }
        }
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
            .rows()
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
            .rows()
            .into_iter()
            .filter_map(|row| match row {
                Row::Node(node_ix) => Some(node_ix),
                Row::Loading { .. } | Row::Empty { .. } => None,
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
        if node.expanded {
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
        let this = cx.entity().downgrade();
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .entry("Make Root", None, move |window, cx| {
                    this.update(cx, |this, cx| this.make_root(node_ix, window, cx))
                        .ok();
                })
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

    fn empty_label(&self) -> &'static str {
        match self.mode {
            CallHierarchyMode::Incoming => "No callers",
            CallHierarchyMode::Outgoing => "No calls",
        }
    }

    fn render_row(&self, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let (depth, node_ix) = match row {
            Row::Node(node_ix) => match self.nodes().get(node_ix) {
                Some(node) => (node.depth, Some(node_ix)),
                None => return div().into_any_element(),
            },
            Row::Loading { depth } | Row::Empty { depth } => (depth, None),
        };
        let indent = depth.saturating_sub(1);
        let Some((node_ix, node)) = node_ix.and_then(|ix| Some((ix, self.nodes().get(ix)?))) else {
            let text = match row {
                Row::Empty { .. } => self.empty_label(),
                _ => "Loading…",
            };
            return ListItem::new(("call-trail-note", depth))
                .indent_level(indent)
                .indent_step_size(px(12.))
                .selectable(false)
                .start_slot(div().w_4())
                .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
                .into_any_element();
        };

        let call = &node.call;
        let (name, detail) = call_hierarchy::render_item(call, std::iter::empty(), cx);
        let path = call.display.path.clone().map(|path| {
            let count_suffix = format!(" ({} calls)", call.site_count);
            match path.strip_suffix(&count_suffix) {
                Some(path) => SharedString::from(path.to_string()),
                None => path,
            }
        });
        let disclosure = (!node.recursive).then(|| {
            Disclosure::new(("call-trail-disclosure", node_ix), node.expanded).on_click(
                cx.listener(move |this, _, window, cx| this.toggle_expanded(node_ix, window, cx)),
            )
        });
        let name_selector = call.display.name.clone();
        div()
            .debug_selector(move || format!("call-trail-{name_selector}"))
            .child(
                ListItem::new(("call-trail", node_ix))
                    .indent_level(indent)
                    .indent_step_size(px(12.))
                    .toggle_state(self.selected == Some(node_ix))
                    .start_slot(h_flex().w_4().justify_center().children(disclosure))
                    .child(
                        h_flex()
                            .gap_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(div().flex_none().child(name))
                            .children(detail.map(|detail| div().flex_none().child(detail)))
                            .when(call.site_count > 1, |this| {
                                this.child(
                                    Label::new(format!("×{}", call.site_count))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            })
                            .when(node.recursive, |this| {
                                this.child(
                                    Label::new("↻ recursive")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            })
                            .children(path.map(|path| {
                                Label::new(path)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate()
                            })),
                    )
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

    fn render_header(&self, root: &Call, cx: &mut Context<Self>) -> AnyElement {
        let (name, detail) = call_hierarchy::render_item(root, std::iter::empty(), cx);
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .min_w_0()
                    .overflow_hidden()
                    .child(name)
                    .children(detail),
            )
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
            .into_any_element()
    }
}

impl Render for CallHierarchyPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults(cx.platform_info().os);
        key_context.add("CallHierarchyPanel");
        key_context.add("menu");
        let message = match &self.content {
            Content::Message(message) => Some(*message),
            Content::LoadingRoot => Some("Loading…"),
            Content::Tree(_) => None,
        };
        let header = self
            .nodes()
            .first()
            .map(|root| self.render_header(&root.call, cx));
        let rows = self.rows();
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
            .children(message.map(|message| {
                div()
                    .px_2()
                    .py_1()
                    .child(Label::new(message).color(Color::Muted))
            }))
            .children(header)
            .when(!rows.is_empty(), |this| {
                this.child(
                    uniform_list(
                        "call-trails",
                        rows.len(),
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            rows.get(range)
                                .unwrap_or_default()
                                .iter()
                                .map(|row| this.render_row(*row, cx))
                                .collect()
                        }),
                    )
                    .flex_1()
                    .py_1()
                    .track_scroll(&self.scroll_handle),
                )
            })
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
        px(300.)
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
        if active {
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
        if matches!(self.content, Content::LoadingRoot) {
            self.content = Content::Message(NO_FUNCTION);
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

fn same_item(left: &CallHierarchyItem, right: &CallHierarchyItem) -> bool {
    left.buffer == right.buffer
        && left.selection_range == right.selection_range
        && left.name == right.name
}

/// Remote projects check capabilities on the host, so they count as supported.
fn supports_call_hierarchy(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut App,
) -> bool {
    if !project.read(cx).is_local() {
        return true;
    }
    let lsp_store = project.read(cx).lsp_store();
    lsp_store.update(cx, |lsp_store, cx| {
        let server_ids = buffer.update(cx, |buffer, cx| {
            lsp_store.language_servers_for_local_buffer(buffer, cx)
        });
        server_ids.iter().any(|server_id| {
            lsp_store
                .lsp_server_capabilities
                .get(server_id)
                .is_some_and(|capabilities| match &capabilities.call_hierarchy_provider {
                    Some(lsp::CallHierarchyServerCapability::Simple(supported)) => *supported,
                    Some(lsp::CallHierarchyServerCapability::Options(_)) => true,
                    None => false,
                })
        })
    })
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

    const SOURCE: &str = "fn main() { helper(); helper(); }\nfn helper() { leaf(); helper(); }\nfn leaf() {}\nfn other() { leaf(); }\n";

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
    /// other calls leaf.
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
            _ => Vec::new(),
        }
    }

    async fn setup<'a>(call_hierarchy_support: bool, cx: &'a mut TestAppContext) -> Setup<'a> {
        cx.update(|cx| {
            let _state = AppState::test(cx);
            editor::init(cx);
            call_hierarchy::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/test"), json!({"src": {"main.rs": SOURCE}}))
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
        let mut fake_servers = project.read_with(cx, |project, _| {
            project.languages().register_fake_lsp(
                "Rust",
                FakeLspAdapter {
                    capabilities: lsp::ServerCapabilities {
                        call_hierarchy_provider: call_hierarchy_support
                            .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
                        ..lsp::ServerCapabilities::default()
                    },
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
            let weak_workspace = cx.entity().downgrade();
            let panel = cx.new(|cx| CallHierarchyPanel::new(weak_workspace, project.clone(), cx));
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
            self.panel.read_with(self.cx, |panel, _| {
                panel
                    .rows()
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
            self.panel.read_with(self.cx, |panel, _| {
                let selected = panel.selected?;
                panel
                    .rows()
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

        fn message(&mut self) -> Option<&'static str> {
            self.panel
                .read_with(self.cx, |panel, _| match panel.content {
                    Content::Message(message) => Some(message),
                    _ => None,
                })
        }

        /// The visible rows, two spaces of indent per level.
        fn tree(&mut self) -> Vec<String> {
            self.panel.read_with(self.cx, |panel, _| {
                let nodes = panel.nodes();
                panel
                    .rows()
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
                            text
                        }
                        Row::Loading { depth } => format!("{}Loading…", "  ".repeat(depth - 1)),
                        Row::Empty { depth } => {
                            format!("{}{}", "  ".repeat(depth - 1), panel.empty_label())
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
        assert_eq!(setup.message(), Some(NO_FUNCTION));

        setup.show_in_panel(1);
        assert_eq!(setup.root().as_deref(), Some("helper"));
        assert_eq!(setup.tree(), ["main ×2", "helper ↻"]);
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
        assert_eq!(setup.tree(), ["helper", "  main ×2", "  helper ↻", "other"]);

        setup.expand("other");
        assert_eq!(setup.take_requests(), ["callers of other"]);
        assert_eq!(
            setup.tree(),
            ["helper", "  main ×2", "  helper ↻", "other", "  No callers"]
        );

        setup.collapse("helper");
        assert_eq!(setup.tree(), ["helper", "other", "  No callers"]);
        setup.expand("helper");
        assert_eq!(setup.take_requests(), Vec::<String>::new());
        assert_eq!(
            setup.tree(),
            ["helper", "  main ×2", "  helper ↻", "other", "  No callers"]
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
        assert_eq!(setup.tree(), ["helper", "  main ×2", "  helper ↻", "other"]);
    }

    #[gpui::test]
    async fn switching_to_calls_shows_outgoing_calls(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(0);
        assert_eq!(setup.tree(), ["No callers"]);
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
    async fn recursive_calls_are_marked_and_not_expandable(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(1);
        assert_eq!(setup.tree(), ["main ×2", "helper ↻"]);
        let recursive = setup.node("helper");
        setup.take_requests();
        setup.panel.update_in(setup.cx, |panel, window, cx| {
            panel.expand(recursive, window, cx);
        });
        setup.cx.run_until_parked();
        assert_eq!(setup.take_requests(), Vec::<String>::new());
        assert_eq!(setup.tree(), ["main ×2", "helper ↻"]);
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
        setup.cx.update(|_, cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("down", menu::SelectNext, Some("CallHierarchyPanel")),
                gpui::KeyBinding::new("up", menu::SelectPrevious, Some("CallHierarchyPanel")),
                gpui::KeyBinding::new("right", menu::SelectChild, Some("CallHierarchyPanel")),
                gpui::KeyBinding::new("left", menu::SelectParent, Some("CallHierarchyPanel")),
                gpui::KeyBinding::new("enter", menu::Confirm, Some("CallHierarchyPanel")),
            ]);
        });
        setup.show_in_panel(2);
        let focused = setup.panel.update_in(setup.cx, |panel, window, _| {
            panel.focus_handle.is_focused(window)
        });
        assert!(focused, "Show in panel focuses the panel");

        setup.cx.simulate_keystrokes("down right");
        setup.cx.run_until_parked();
        assert_eq!(setup.tree(), ["helper", "  main ×2", "  helper ↻", "other"]);
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
        assert_eq!(setup.tree(), ["main ×2", "helper ↻"]);

        setup.right_click("call-trail-main");
        setup.click("MENU_ITEM-Copy Name");
        let copied = setup
            .cx
            .update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(copied.as_deref(), Some("main"));
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
    async fn explains_when_there_is_nothing_to_show(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(4);
        assert_eq!(setup.message(), Some(NO_FUNCTION));
        assert_eq!(setup.root(), None);
    }

    #[gpui::test]
    async fn explains_an_unsupported_language_server(cx: &mut TestAppContext) {
        let mut setup = setup(false, cx).await;
        setup.show_in_panel(1);
        assert_eq!(setup.message(), Some(UNSUPPORTED));
    }

    #[gpui::test]
    async fn rows_show_the_path_without_the_popup_call_count(cx: &mut TestAppContext) {
        let mut setup = setup(true, cx).await;
        setup.show_in_panel(1);
        let path = setup.panel.read_with(setup.cx, |panel, _| {
            panel.nodes()[1].call.display.path.clone()
        });
        let path = path.expect("calls have a path");
        assert!(
            path.ends_with(":1 (2 calls)"),
            "the popup's path suffix this panel replaces with ×N changed: {path}"
        );
    }
}
