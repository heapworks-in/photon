use std::sync::Arc;

use anyhow::Result;
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task};
use language::{Buffer, BufferEvent};
use project::{Project, ProjectEntryId, ProjectItem, ProjectPath};

use crate::WebEditorRegistry;

pub struct WebDocument {
    buffer: Entity<Buffer>,
    editor_id: Arc<str>,
    // `ProjectItem::is_dirty` has no `cx`, so the buffer's dirty bit is mirrored here.
    dirty: bool,
    _subscription: Subscription,
}

impl WebDocument {
    pub fn new(buffer: Entity<Buffer>, editor_id: Arc<str>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&buffer, |this, buffer, event, cx| {
            if matches!(event, BufferEvent::DirtyChanged) {
                this.dirty = buffer.read(cx).is_dirty();
            }
        });
        Self {
            dirty: buffer.read(cx).is_dirty(),
            buffer,
            editor_id,
            _subscription: subscription,
        }
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    pub fn editor_id(&self) -> &Arc<str> {
        &self.editor_id
    }

    pub fn editor_id_for_path(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &App,
    ) -> Option<Arc<str>> {
        let abs_path = project.read(cx).absolute_path(path, cx)?;
        let registry = cx.try_global::<WebEditorRegistry>()?;
        Some(registry.editor_for_path(&abs_path)?.id.clone())
    }

    pub fn open(
        project: Entity<Project>,
        path: ProjectPath,
        editor_id: Arc<str>,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(path, cx))
                .await?;
            Ok(cx.new(|cx| WebDocument::new(buffer, editor_id, cx)))
        })
    }
}

impl ProjectItem for WebDocument {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        let editor_id = Self::editor_id_for_path(project, path, cx)?;
        Some(Self::open(project.clone(), path.clone(), editor_id, cx))
    }

    fn entry_id(&self, cx: &App) -> Option<ProjectEntryId> {
        self.buffer.read(cx).entry_id(cx)
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        ProjectItem::project_path(self.buffer.read(cx), cx)
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }
}
