//! ⌃⌥E edit requests: a short instruction typed under a selection, queued in the editor, and sent
//! to the agent together by Apply All.

use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use acp_thread::MentionUri;
use agent_client_protocol::schema::v2 as acp_v2;
use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use editor::{Editor, ToPoint as _};
use gpui::{App, Context, Entity, EntityId, Focusable as _, Global, Render, WeakEntity, Window};
use project::ProjectItem as _;
use ui::{Button, IconButton, IconName, IconSize, Tooltip, prelude::*};
use workspace::Workspace;

gpui::actions!(
    photon_agent,
    [
        /// Types an edit for your agent to make to the selection. Apply All sends every queued edit.
        QueueEditRequest,
    ]
);

/// Sends queued edit requests to the agent as one message. Set by the agent panel.
pub type EditRequestSender =
    Rc<dyn Fn(&mut Workspace, Vec<acp_v2::ContentBlock>, &mut Window, &mut Context<Workspace>)>;

struct GlobalEditRequestSender(EditRequestSender);

impl Global for GlobalEditRequestSender {}

pub fn set_edit_request_sender(sender: EditRequestSender, cx: &mut App) {
    cx.set_global(GlobalEditRequestSender(sender));
}

/// Every open request, typed or queued, in any editor.
#[derive(Default)]
struct EditRequests(Vec<WeakEntity<EditRequest>>);

impl Global for EditRequests {}

pub struct EditRequest {
    workspace: WeakEntity<Workspace>,
    editor: WeakEntity<Editor>,
    block: Option<CustomBlockId>,
    abs_path: Option<PathBuf>,
    line_range: RangeInclusive<u32>,
    selected_text: String,
    instruction_editor: Entity<Editor>,
    /// The instruction once Enter queued it.
    queued: Option<SharedString>,
}

/// Opens the instruction box under the active editor's selection.
pub fn queue_edit_request(
    workspace: &mut Workspace,
    _: &QueueEditRequest,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !cx.has_global::<GlobalEditRequestSender>() {
        return;
    }
    let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
        return;
    };
    let project = workspace.project().clone();
    let Some((end, abs_path, line_range, selected_text)) = editor.update(cx, |editor, cx| {
        let selection = *editor.selections.newest_anchor();
        let multi_buffer = editor.buffer().read(cx);
        // ponytail: one file per editor; multibuffer excerpts would need a path per range.
        let buffer = multi_buffer.as_singleton()?;
        let snapshot = multi_buffer.snapshot(cx);
        let selected_text = snapshot
            .text_for_range(selection.start..selection.end)
            .collect::<String>();
        if selected_text.is_empty() {
            return None;
        }
        let abs_path = buffer
            .read(cx)
            .project_path(cx)
            .and_then(|project_path| project.read(cx).absolute_path(&project_path, cx));
        let line_range =
            selection.start.to_point(&snapshot).row..=selection.end.to_point(&snapshot).row;
        Some((selection.end, abs_path, line_range, selected_text))
    }) else {
        return;
    };

    let request = cx.new(|cx| {
        let instruction_editor = cx.new(|cx| {
            let mut instruction_editor = Editor::single_line(window, cx);
            instruction_editor.set_placeholder_text("Describe the edit", window, cx);
            instruction_editor
        });
        EditRequest {
            workspace: workspace.weak_handle(),
            editor: editor.downgrade(),
            block: None,
            abs_path,
            line_range,
            selected_text,
            instruction_editor,
            queued: None,
        }
    });
    let block = editor.update(cx, |editor, cx| {
        let request = request.clone();
        editor
            .insert_blocks(
                [BlockProperties {
                    style: BlockStyle::Sticky,
                    placement: BlockPlacement::Below(end),
                    height: Some(2),
                    render: Arc::new(move |_| request.clone().into_any_element()),
                    priority: 0,
                }],
                None,
                cx,
            )
            .pop()
    });
    request.update(cx, |request, _| request.block = block);
    cx.default_global::<EditRequests>()
        .0
        .push(request.downgrade());
    let focus_handle = request.read(cx).instruction_editor.focus_handle(cx);
    window.focus(&focus_handle, cx);
}

impl EditRequest {
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let instruction = self.instruction_editor.read(cx).text(cx);
        let instruction = instruction.trim();
        if instruction.is_empty() {
            return;
        }
        self.queued = Some(instruction.to_string().into());
        if let Some(editor) = self.editor.upgrade() {
            window.focus(&editor.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn cancel(&mut self, _: &editor::actions::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.upgrade() {
            window.focus(&editor.focus_handle(cx), cx);
        }
        self.close(cx);
    }

    /// Removes the request's block from its editor and the request from the queue.
    fn close(&mut self, cx: &mut Context<Self>) {
        if let (Some(editor), Some(block)) = (self.editor.upgrade(), self.block.take()) {
            editor.update(cx, |editor, cx| {
                editor.remove_blocks(HashSet::from_iter([block]), None, cx)
            });
        }
        let this = cx.entity_id();
        cx.default_global::<EditRequests>()
            .0
            .retain(|request| request.entity_id() != this && request.upgrade().is_some());
    }

    fn content(&self) -> Vec<acp_v2::ContentBlock> {
        let instruction = self.queued.clone().unwrap_or_default();
        let selection = MentionUri::Selection {
            abs_path: self.abs_path.clone(),
            line_range: self.line_range.clone(),
            column: None,
        };
        vec![
            acp_v2::ContentBlock::Resource(acp_v2::EmbeddedResource::new(
                acp_v2::EmbeddedResourceResource::TextResourceContents(
                    acp_v2::TextResourceContents::new(
                        self.selected_text.clone(),
                        selection.to_uri().to_string(),
                    ),
                ),
            )),
            format!("{instruction}\n").into(),
        ]
    }
}

/// The queued requests in `workspace`, oldest first, leaving out `except` (a request being
/// updated can't be read).
fn queued_requests(
    workspace: &WeakEntity<Workspace>,
    except: Option<EntityId>,
    cx: &App,
) -> Vec<Entity<EditRequest>> {
    cx.try_global::<EditRequests>()
        .map(|requests| {
            requests
                .0
                .iter()
                .filter(|request| Some(request.entity_id()) != except)
                .filter_map(WeakEntity::upgrade)
                .filter(|request| {
                    let request = request.read(cx);
                    request.queued.is_some() && &request.workspace == workspace
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Sends every queued request in the workspace to a new agent thread, then clears them.
fn apply_all(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let Some(sender) = cx
        .try_global::<GlobalEditRequestSender>()
        .map(|sender| sender.0.clone())
    else {
        return;
    };
    let requests = queued_requests(&workspace, None, cx);
    let mut content = vec![acp_v2::ContentBlock::from("Make these edits:\n")];
    for request in &requests {
        content.extend(request.read(cx).content());
    }
    for request in requests {
        request.update(cx, |request, cx| request.close(cx));
    }
    workspace
        .update(cx, |workspace, cx| sender(workspace, content, window, cx))
        .ok();
}

impl Render for EditRequest {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row = h_flex()
            .id("photon-edit-request")
            .key_context("PhotonEditRequest")
            .w_full()
            .h_full()
            .gap_2()
            .pl_2()
            .pr_4()
            .border_l_2()
            .border_color(cx.theme().colors().border_focused)
            .bg(cx.theme().colors().editor_background)
            .child(
                Icon::new(IconName::Pencil)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            );
        let Some(instruction) = self.queued.clone() else {
            return row
                .on_action(cx.listener(Self::confirm))
                .on_action(cx.listener(Self::cancel))
                .child(div().flex_1().child(self.instruction_editor.clone()));
        };
        let queued_count = 1 + queued_requests(&self.workspace, Some(cx.entity_id()), cx).len();
        let workspace = self.workspace.clone();
        let request = cx.entity().downgrade();
        row.child(
            Label::new(instruction)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
        .child(div().flex_1())
        .child(
            div().debug_selector(|| "photon-apply-all".into()).child(
                Button::new(
                    "photon-apply-all",
                    if queued_count > 1 {
                        format!("Apply All ({queued_count})")
                    } else {
                        "Apply All".to_string()
                    },
                )
                .label_size(LabelSize::Small)
                .tooltip(Tooltip::text("Send the queued edits to your agent"))
                .on_click(move |_, window, cx| apply_all(workspace.clone(), window, cx)),
            ),
        )
        .child(
            IconButton::new("photon-remove-edit-request", IconName::Close)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Remove"))
                .on_click(move |_, _, cx| {
                    request.update(cx, |request, cx| request.close(cx)).ok();
                }),
        )
    }
}
