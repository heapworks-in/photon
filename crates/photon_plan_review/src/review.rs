use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use db::kvp::KeyValueStore;
use db::sqlez::statement::Statement;
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Global, KeyDownEvent,
    KeybindingKeystroke, Keystroke, ScrollHandle, SharedString, Stateful, Subscription, Task,
    WeakEntity, Window, actions, px,
};
use language::{Buffer, BufferEvent};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownStyle, WrapButtonVisibility,
};
use project::{
    Project, ProjectPath,
    search::{SearchQuery, SearchResult},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use ui::{Checkbox, Indicator, TintColor, ToggleState, Tooltip, VisibleOnHover as _, prelude::*};
use util::{ResultExt as _, paths::PathMatcher, rel_path::RelPath};
use workspace::{
    Item, ItemId, Toast, Workspace, WorkspaceId, item::SerializableItem,
    notifications::NotificationId,
};

use crate::{
    FileState, Finding, Plan, PlanDraft, PlanReviewDb, Repository, Section, check_plan, file_state,
    last_segment, parse_plan, task_label, unresolved_identifiers,
};

actions!(
    plan,
    [
        /// Reviews the open plan: its tasks as steps, checked against the repository.
        Review,
        /// Shows the next task of the plan review.
        NextTask,
        /// Shows the previous task of the plan review.
        PreviousTask,
        /// Asks the agent about the selected task, in a new thread.
        AskAgent,
        /// Saves the comment being written on the plan.
        SaveComment,
    ]
);

const REPARSE_DEBOUNCE: Duration = Duration::from_millis(300);
const QUOTE_LENGTH: usize = 200;
const QUOTED_LINES: usize = 6;
const MAP_WIDTH: Pixels = px(300.);

/// A message for a new agent thread. `send` submits it; otherwise it waits for the user's words.
pub struct AgentMessage {
    pub title: String,
    pub prompt: String,
    pub send: bool,
}

/// What the app plugs in, so this crate needs no agent crates.
#[derive(Clone, Default)]
pub struct PlanReviewHooks {
    #[allow(clippy::type_complexity)]
    pub send_to_agent:
        Option<Rc<dyn Fn(&mut Workspace, AgentMessage, &mut Window, &mut Context<Workspace>)>>,
}

impl Global for PlanReviewHooks {}

pub fn set_hooks(hooks: PlanReviewHooks, cx: &mut App) {
    cx.set_global(hooks);
}

fn agent_hook(
    cx: &App,
) -> Option<Rc<dyn Fn(&mut Workspace, AgentMessage, &mut Window, &mut Context<Workspace>)>> {
    cx.try_global::<PlanReviewHooks>()
        .and_then(|hooks| hooks.send_to_agent.clone())
}

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<PlanReviewItem>(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Review, window, cx| {
            review_active_plan(workspace, window, cx)
        });
    })
    .detach();
}

struct PlanReviewNotice;

fn show_notice(workspace: &mut Workspace, message: &'static str, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<PlanReviewNotice>(), message).autohide(),
        cx,
    );
}

fn review_active_plan(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project_path = workspace.active_item(cx).and_then(|item| {
        item.project_path(cx).or_else(|| {
            let editor = item.act_as::<Editor>(cx)?;
            let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
            let file = buffer.read(cx).file()?;
            Some(ProjectPath {
                worktree_id: file.worktree_id(cx),
                path: file.path().clone(),
            })
        })
    });
    let Some(project_path) = project_path else {
        show_notice(workspace, "Open a plan to review it.", cx);
        return;
    };
    open_plan_review(workspace, project_path, window, cx).detach_and_log_err(cx);
}

/// Opens the review of the plan at `project_path`, or shows the one already open.
pub fn open_plan_review(
    workspace: &mut Workspace,
    project_path: ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<PlanReviewItem>>> {
    let existing = workspace
        .items_of_type::<PlanReviewItem>(cx)
        .find(|item| item.read(cx).project_path == project_path);
    if let Some(item) = existing {
        workspace.activate_item(&item, true, true, window, cx);
        return Task::ready(Ok(item));
    }
    let project = workspace.project().clone();
    let open = project.update(cx, |project, cx| project.open_buffer(project_path, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let buffer = open.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            if crate::plan_task_count(&buffer.read(cx).text()).is_none() {
                show_notice(workspace, "This file has no tasks to review.", cx);
                return Err(anyhow!("not a plan"));
            }
            let item = cx.new(|cx| PlanReviewItem::new(project, buffer, window, cx));
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
            Ok(item)
        })?
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    NotReviewed,
    Approved,
    ChangesRequested,
}

impl PlanStatus {
    fn label(self) -> &'static str {
        match self {
            PlanStatus::NotReviewed => "Not reviewed",
            PlanStatus::Approved => "Approved",
            PlanStatus::ChangesRequested => "Changes requested",
        }
    }

    fn color(self) -> Color {
        match self {
            PlanStatus::NotReviewed => Color::Muted,
            PlanStatus::Approved => Color::Success,
            PlanStatus::ChangesRequested => Color::Warning,
        }
    }

    fn to_db(self) -> &'static str {
        match self {
            PlanStatus::NotReviewed => "not_reviewed",
            PlanStatus::Approved => "approved",
            PlanStatus::ChangesRequested => "changes_requested",
        }
    }

    fn from_db(value: &str) -> Self {
        match value {
            "approved" => PlanStatus::Approved,
            "changes_requested" => PlanStatus::ChangesRequested,
            _ => PlanStatus::NotReviewed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    RequestChanges,
}

impl Verdict {
    fn title(self) -> &'static str {
        match self {
            Verdict::Approve => "Approve plan",
            Verdict::RequestChanges => "Request changes",
        }
    }

    fn status(self) -> PlanStatus {
        match self {
            Verdict::Approve => PlanStatus::Approved,
            Verdict::RequestChanges => PlanStatus::ChangesRequested,
        }
    }

    fn instruction(self) -> &'static str {
        match self {
            Verdict::Approve => {
                "I reviewed your plan and approved it. Go ahead, taking my comments into account."
            }
            Verdict::RequestChanges => {
                "I reviewed your plan and I'm requesting changes. Revise the plan to address each \
                 comment, then tell me what changed."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    Agent,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TickState {
    NotRead,
    Read,
    Changed,
}

struct DraftEntry {
    local_id: usize,
    draft: PlanDraft,
}

struct Composer {
    editor: Entity<Editor>,
    paragraph: usize,
    /// The draft being edited, by local id.
    editing: Option<usize>,
}

pub struct PlanReviewItem {
    project: Entity<Project>,
    workspace: Option<WeakEntity<Workspace>>,
    buffer: Entity<Buffer>,
    project_path: ProjectPath,
    abs_path: PathBuf,
    display_path: SharedString,
    source: String,
    plan: Plan,
    /// 0 is the overview, then the tasks.
    selected: usize,
    selected_paragraph: Option<usize>,
    paragraphs: Vec<Entity<Markdown>>,
    ticks: HashMap<String, String>,
    drafts: Vec<DraftEntry>,
    next_local_id: usize,
    dismissed: HashSet<String>,
    /// Identifiers the project search found definitions for; `None` until it has run.
    found: Option<HashSet<String>>,
    searched: HashSet<String>,
    spec: Option<(String, String)>,
    findings: Vec<Finding>,
    status: PlanStatus,
    composer: Option<Composer>,
    summary_editor: Entity<Editor>,
    destination: Destination,
    sending: bool,
    send_error: Option<SharedString>,
    note: Option<SharedString>,
    focus_handle: FocusHandle,
    document_scroll: ScrollHandle,
    database: PlanReviewDb,
    _reparse: Task<()>,
    _search: Task<()>,
    _spec: Task<()>,
    _subscriptions: Vec<Subscription>,
}

fn fingerprint(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn quote_of(text: &str) -> String {
    text.chars().take(QUOTE_LENGTH).collect()
}

fn contains_word(text: &str, word: &str) -> bool {
    let is_identifier = |character: char| character.is_alphanumeric() || character == '_';
    text.match_indices(word).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + word.len()..].chars().next();
        !before.is_some_and(is_identifier) && !after.is_some_and(is_identifier)
    })
}

impl PlanReviewItem {
    pub fn new(
        project: Entity<Project>,
        buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = buffer.read(cx).text();
        let (project_path, abs_path) = match buffer.read(cx).file() {
            Some(file) => {
                let project_path = ProjectPath {
                    worktree_id: file.worktree_id(cx),
                    path: file.path().clone(),
                };
                let abs_path = project
                    .read(cx)
                    .absolute_path(&project_path, cx)
                    .unwrap_or_else(|| file.path().as_std_path().to_path_buf());
                (project_path, abs_path)
            }
            None => (
                ProjectPath {
                    worktree_id: project::WorktreeId::from_usize(0),
                    path: RelPath::empty_arc(),
                },
                PathBuf::new(),
            ),
        };
        let path_style = project.read(cx).path_style(cx);
        let display_path = SharedString::from(project_path.path.display(path_style).to_string());
        let plan = parse_plan(&source).unwrap_or_else(|| Plan {
            title: None,
            kind: crate::PlanKind::Checklist,
            overview: Section::default(),
            tasks: Vec::new(),
            spec_path: None,
        });
        let database = PlanReviewDb::global(cx);
        let plan_key = abs_path.to_string_lossy().to_string();
        let ticks = database.ticks(&plan_key).log_err().unwrap_or_default();
        let dismissed = database.dismissed(&plan_key).log_err().unwrap_or_default();
        let status = database
            .status(&plan_key)
            .log_err()
            .flatten()
            .map_or(PlanStatus::NotReviewed, |status| {
                PlanStatus::from_db(&status)
            });
        let stored = database.drafts(&plan_key).log_err().unwrap_or_default();
        let next_local_id = stored.len();
        let drafts = stored
            .into_iter()
            .enumerate()
            .map(|(local_id, draft)| DraftEntry { local_id, draft })
            .collect();
        let summary_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 6, window, cx);
            editor.set_placeholder_text("Summary for the agent (optional)", window, cx);
            editor
        });
        let subscriptions = vec![cx.subscribe(&buffer, |this, _, event: &BufferEvent, cx| {
            if matches!(
                event,
                BufferEvent::Edited { .. } | BufferEvent::Reloaded | BufferEvent::Saved
            ) {
                this.schedule_reparse(cx);
            }
        })];
        let destination = if agent_hook(cx).is_some() {
            Destination::Agent
        } else {
            Destination::File
        };
        let mut this = Self {
            project,
            workspace: None,
            buffer,
            project_path,
            abs_path,
            display_path,
            source,
            plan,
            selected: 0,
            selected_paragraph: None,
            paragraphs: Vec::new(),
            ticks,
            drafts,
            next_local_id,
            dismissed,
            found: None,
            searched: HashSet::new(),
            spec: None,
            findings: Vec::new(),
            status,
            composer: None,
            summary_editor,
            destination,
            sending: false,
            send_error: None,
            note: None,
            focus_handle: cx.focus_handle(),
            document_scroll: ScrollHandle::new(),
            database,
            _reparse: Task::ready(()),
            _search: Task::ready(()),
            _spec: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.rebuild_paragraphs(cx);
        this.refresh_findings(cx);
        this.load_spec(cx);
        this.search_identifiers(cx);
        this
    }

    fn plan_key(&self) -> String {
        self.abs_path.to_string_lossy().to_string()
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn status(&self) -> PlanStatus {
        self.status
    }

    pub fn findings(&self) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|finding| !self.dismissed.contains(&finding.key))
            .collect()
    }

    pub fn drafts(&self) -> Vec<&PlanDraft> {
        self.drafts.iter().map(|entry| &entry.draft).collect()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    fn section(&self, index: usize) -> Option<&Section> {
        match index {
            0 => Some(&self.plan.overview),
            index => self.plan.tasks.get(index - 1),
        }
    }

    fn section_text(&self, section: &Section) -> &str {
        self.source.get(section.range.clone()).unwrap_or_default()
    }

    fn block_text(&self, section: &Section, paragraph: usize) -> &str {
        section
            .blocks
            .get(paragraph)
            .and_then(|range| self.source.get(range.clone()))
            .unwrap_or_default()
    }

    fn section_heading(&self, index: usize) -> String {
        match index {
            0 => "Overview".into(),
            index => {
                let title = self
                    .plan
                    .tasks
                    .get(index - 1)
                    .map_or("", |task| task.title.as_str());
                format!("{}: {title}", task_label(&self.plan, index - 1))
            }
        }
    }

    fn title(&self) -> String {
        self.plan.title.clone().unwrap_or_else(|| {
            self.project_path
                .path
                .file_stem()
                .unwrap_or("Plan")
                .to_string()
        })
    }

    fn schedule_reparse(&mut self, cx: &mut Context<Self>) {
        self._reparse = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REPARSE_DEBOUNCE).await;
            this.update(cx, |this, cx| this.reparse(cx)).log_err();
        });
    }

    fn reparse(&mut self, cx: &mut Context<Self>) {
        let source = self.buffer.read(cx).text();
        if source == self.source {
            return;
        }
        // ponytail: a file that stops reading as a plan keeps showing its last good version.
        let Some(plan) = parse_plan(&source) else {
            return;
        };
        let selected_key = self
            .section(self.selected)
            .map(|section| section.key.clone());
        self.plan = plan;
        self.source = source;
        self.selected = selected_key
            .and_then(|key| self.plan.sections().position(|section| section.key == key))
            .unwrap_or(0);
        self.selected_paragraph = None;
        self.composer = None;
        self.rebuild_paragraphs(cx);
        self.refresh_findings(cx);
        self.search_identifiers(cx);
        cx.notify();
    }

    fn rebuild_paragraphs(&mut self, cx: &mut Context<Self>) {
        let languages = self.project.read(cx).languages().clone();
        let Some(section) = self.section(self.selected) else {
            self.paragraphs.clear();
            return;
        };
        let texts: Vec<SharedString> = section
            .blocks
            .iter()
            .map(|range| {
                SharedString::from(
                    self.source
                        .get(range.clone())
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect();
        self.paragraphs = texts
            .into_iter()
            .map(|text| {
                let languages = languages.clone();
                cx.new(|cx| Markdown::new(text, Some(languages), None, cx))
            })
            .collect();
    }

    fn file_exists(&self, path: &str, cx: &App) -> bool {
        let style = self.project.read(cx).path_style(cx);
        let Ok(path) = RelPath::new(Path::new(path), style) else {
            return false;
        };
        let project_path = ProjectPath {
            worktree_id: self.project_path.worktree_id,
            path: path.into_owned().into(),
        };
        self.project
            .read(cx)
            .entry_for_path(&project_path, cx)
            .is_some()
    }

    fn refresh_findings(&mut self, cx: &mut Context<Self>) {
        let exists = |path: &str| self.file_exists(path, cx);
        let findings = check_plan(
            &self.plan,
            &self.source,
            &Repository {
                exists: &exists,
                found: self.found.as_ref(),
            },
            self.spec
                .as_ref()
                .map(|(path, text)| (path.as_str(), text.as_str())),
        );
        self.findings = findings;
    }

    /// Reads the spec the plan names, or a `spec.md`/`requirements.md` next to it.
    fn load_spec(&mut self, cx: &mut Context<Self>) {
        let style = self.project.read(cx).path_style(cx);
        let directory = self
            .project_path
            .path
            .parent()
            .map(|parent| parent.as_unix_str().to_string())
            .unwrap_or_default();
        let in_directory = |name: &str| {
            if directory.is_empty() {
                name.to_string()
            } else {
                format!("{directory}/{name}")
            }
        };
        let mut candidates = Vec::new();
        if let Some(spec) = &self.plan.spec_path {
            let spec = spec.trim_start_matches("./").trim_start_matches('/');
            candidates.push(spec.to_string());
            candidates.push(in_directory(spec));
        }
        for sibling in ["spec.md", "requirements.md"] {
            candidates.push(in_directory(sibling));
        }
        let Some(spec_path) = candidates
            .into_iter()
            .find(|candidate| self.file_exists(candidate, cx))
        else {
            return;
        };
        let Ok(relative) = RelPath::new(Path::new(&spec_path), style) else {
            return;
        };
        let Some(abs_path) = self.project.read(cx).absolute_path(
            &ProjectPath {
                worktree_id: self.project_path.worktree_id,
                path: relative.into_owned().into(),
            },
            cx,
        ) else {
            return;
        };
        let fs = self.project.read(cx).fs().clone();
        self._spec = cx.spawn(async move |this, cx| {
            let Some(text) = fs.load(&abs_path).await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.spec = Some((spec_path, text));
                this.refresh_findings(cx);
                cx.notify();
            })
            .log_err();
        });
    }

    /// One project search for every identifier no task defines, run once per name.
    fn search_identifiers(&mut self, cx: &mut Context<Self>) {
        let names: Vec<String> = unresolved_identifiers(&self.plan)
            .into_iter()
            .filter(|name| !self.searched.contains(name))
            .collect();
        if names.is_empty() {
            if self.found.is_none() {
                self.found = Some(HashSet::new());
                self.refresh_findings(cx);
            }
            return;
        }
        let pattern = names
            .iter()
            .map(|name| format!("(?:{})", photon_navigation::definition_pattern(name)))
            .collect::<Vec<_>>()
            .join("|");
        let style = self.project.read(cx).path_style(cx);
        // Plans and specs quote code they only propose, so Markdown doesn't count as a definition.
        let Some(exclude) = PathMatcher::new(["**/*.md", "**/*.mdx"], style).log_err() else {
            return;
        };
        let Some(query) = SearchQuery::regex(
            pattern,
            false,
            true,
            false,
            false,
            PathMatcher::default(),
            exclude,
            false,
            None,
        )
        .log_err() else {
            return;
        };
        let search = self
            .project
            .update(cx, |project, cx| project.search(query, cx));
        self._search = cx.spawn(async move |this, cx| {
            // Move the whole result in: holding only its receiver would drop the search.
            let search = search;
            let mut found = HashSet::new();
            while let Ok(result) = search.rx.recv().await {
                let SearchResult::Buffer { buffer, ranges } = result else {
                    continue;
                };
                buffer.read_with(cx, |buffer, _| {
                    for range in &ranges {
                        let text: String = buffer.text_for_range(range.clone()).collect();
                        for name in &names {
                            if contains_word(&text, name) {
                                found.insert(name.clone());
                            }
                        }
                    }
                });
            }
            this.update(cx, |this, cx| {
                this.searched.extend(names);
                this.found.get_or_insert_default().extend(found);
                this.refresh_findings(cx);
                cx.notify();
            })
            .log_err();
        });
    }

    fn tick_state(&self, section: &Section) -> TickState {
        match self.ticks.get(&section.key) {
            None => TickState::NotRead,
            Some(mark) if *mark == fingerprint(self.section_text(section)) => TickState::Read,
            Some(_) => TickState::Changed,
        }
    }

    pub fn set_ticked(&mut self, index: usize, ticked: bool, cx: &mut Context<Self>) {
        let Some(section) = self.section(index) else {
            return;
        };
        let key = section.key.clone();
        let mark = ticked.then(|| fingerprint(self.section_text(section)));
        match &mark {
            Some(mark) => self.ticks.insert(key.clone(), mark.clone()),
            None => self.ticks.remove(&key),
        };
        let database = self.database.clone();
        let plan_key = self.plan_key();
        cx.background_spawn(async move { database.set_tick(plan_key, key, mark).await })
            .detach_and_log_err(cx);
        cx.notify();
    }

    fn tasks_read(&self) -> usize {
        self.plan
            .tasks
            .iter()
            .filter(|task| self.tick_state(task) == TickState::Read)
            .count()
    }

    pub fn progress_text(&self) -> String {
        let gaps = self.findings().len();
        format!(
            "{} of {} tasks read · {gaps} gap{}",
            self.tasks_read(),
            self.plan.tasks.len(),
            if gaps == 1 { "" } else { "s" }
        )
    }

    pub fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let index = index.min(self.plan.tasks.len());
        if index != self.selected {
            self.selected = index;
            self.selected_paragraph = None;
            self.composer = None;
            self.rebuild_paragraphs(cx);
            self.document_scroll.set_offset(gpui::point(px(0.), px(0.)));
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// The selected section's first paragraph containing `needle`.
    pub fn paragraph_containing(&self, needle: &str) -> Option<usize> {
        let section = self.section(self.selected)?;
        (0..section.blocks.len())
            .find(|paragraph| self.block_text(section, *paragraph).contains(needle))
    }

    fn draft_paragraph(&self, section: &Section, draft: &PlanDraft) -> usize {
        let last = section.blocks.len().saturating_sub(1);
        (0..section.blocks.len())
            .find(|paragraph| quote_of(self.block_text(section, *paragraph)) == draft.quote)
            .unwrap_or(draft.paragraph.min(last))
    }

    fn open_composer(
        &mut self,
        paragraph: usize,
        editing: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body = editing
            .and_then(|local_id| self.drafts.iter().find(|entry| entry.local_id == local_id))
            .map(|entry| entry.draft.body.clone())
            .unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 8, window, cx);
            editor.set_placeholder_text("Comment on this paragraph", window, cx);
            editor.set_text(body, window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.selected_paragraph = Some(paragraph);
        self.composer = Some(Composer {
            editor,
            paragraph,
            editing,
        });
        cx.notify();
    }

    fn close_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn save_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        let body = composer.editor.read(cx).text(cx).trim().to_string();
        window.focus(&self.focus_handle, cx);
        cx.notify();
        if body.is_empty() {
            return;
        }
        if let Some(local_id) = composer.editing {
            if let Some(entry) = self
                .drafts
                .iter_mut()
                .find(|entry| entry.local_id == local_id)
            {
                entry.draft.body = body;
                self.save_draft(local_id, cx);
            }
            return;
        }
        let Some(section) = self.section(self.selected) else {
            return;
        };
        let draft = PlanDraft {
            id: None,
            task_key: section.key.clone(),
            paragraph: composer.paragraph,
            quote: quote_of(self.block_text(section, composer.paragraph)),
            body,
            finding: None,
        };
        self.push_draft(draft, cx);
    }

    fn push_draft(&mut self, draft: PlanDraft, cx: &mut Context<Self>) {
        let local_id = self.next_local_id;
        self.next_local_id += 1;
        self.drafts.push(DraftEntry { local_id, draft });
        self.save_draft(local_id, cx);
        cx.notify();
    }

    fn save_draft(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.drafts.iter().find(|entry| entry.local_id == local_id) else {
            return;
        };
        let draft = entry.draft.clone();
        let database = self.database.clone();
        let plan_key = self.plan_key();
        cx.spawn(async move |this, cx| {
            let id = database.save_draft(plan_key, draft).await?;
            let kept = this.update(cx, |this, _| {
                match this
                    .drafts
                    .iter_mut()
                    .find(|entry| entry.local_id == local_id)
                {
                    Some(entry) => {
                        entry.draft.id = Some(id);
                        true
                    }
                    None => false,
                }
            })?;
            // Deleted while it was being saved.
            if !kept {
                database.delete_drafts(vec![id]).await?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn delete_draft(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self
            .drafts
            .iter()
            .position(|entry| entry.local_id == local_id)
        else {
            return;
        };
        let entry = self.drafts.remove(index);
        if let Some(id) = entry.draft.id {
            let database = self.database.clone();
            cx.background_spawn(async move { database.delete_drafts(vec![id]).await })
                .detach_and_log_err(cx);
        }
        cx.notify();
    }

    fn dismiss_finding(&mut self, key: String, cx: &mut Context<Self>) {
        self.dismissed.insert(key.clone());
        let database = self.database.clone();
        let plan_key = self.plan_key();
        cx.background_spawn(async move { database.dismiss(plan_key, key).await })
            .detach_and_log_err(cx);
        cx.notify();
    }

    fn add_finding_to_review(
        &mut self,
        finding: &Finding,
        paragraph: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(section) = self.section(self.selected) else {
            return;
        };
        let draft = PlanDraft {
            id: None,
            task_key: section.key.clone(),
            paragraph,
            quote: quote_of(self.block_text(section, paragraph)),
            body: format!("{}. {}.", finding.title, finding.evidence),
            finding: Some(finding.key.clone()),
        };
        self.push_draft(draft, cx);
    }

    fn findings_added(&self) -> usize {
        self.drafts
            .iter()
            .filter(|entry| entry.draft.finding.is_some())
            .count()
    }

    fn counts_text(&self) -> String {
        let comments = self.drafts.len() - self.findings_added();
        let gaps = self.findings_added();
        format!(
            "{comments} comment{} · {gaps} gap{} added to the review · {} of {} tasks read",
            if comments == 1 { "" } else { "s" },
            if gaps == 1 { "" } else { "s" },
            self.tasks_read(),
            self.plan.tasks.len()
        )
    }

    pub fn review_file_path(&self) -> PathBuf {
        self.abs_path.with_extension("review.md")
    }

    fn review_file_display(&self) -> String {
        Path::new(self.display_path.as_ref())
            .with_extension("review.md")
            .to_string_lossy()
            .to_string()
    }

    /// The review as Markdown: verdict, summary, and each comment under the paragraph it's on.
    pub fn review_text(&self, verdict: Option<Verdict>, summary: &str) -> String {
        let mut text = format!(
            "# Plan review: {}\n\nPlan: `{}`\n",
            self.title(),
            self.display_path
        );
        if let Some(verdict) = verdict {
            text.push_str(&format!("Verdict: {}\n", verdict.status().label()));
        }
        if !summary.trim().is_empty() {
            text.push_str(&format!("\n{}\n", summary.trim()));
        }
        if self.drafts.is_empty() {
            return text;
        }
        text.push_str("\n## Comments\n");
        let mut placed = HashSet::new();
        for (index, section) in self.plan.sections().enumerate() {
            let drafts: Vec<&DraftEntry> = self
                .drafts
                .iter()
                .filter(|entry| entry.draft.task_key == section.key)
                .collect();
            if drafts.is_empty() {
                continue;
            }
            text.push_str(&format!("\n### {}\n", self.section_heading(index)));
            for entry in drafts {
                placed.insert(entry.local_id);
                let paragraph = self.draft_paragraph(section, &entry.draft);
                let quoted = match self.block_text(section, paragraph) {
                    "" => entry.draft.quote.as_str(),
                    current => current,
                };
                text.push_str(&format!("\n{}\n\n{}\n", quote(quoted), entry.draft.body));
            }
        }
        // Comments on tasks the plan no longer has.
        for entry in self
            .drafts
            .iter()
            .filter(|entry| !placed.contains(&entry.local_id))
        {
            text.push_str(&format!(
                "\n{}\n\n{}\n",
                quote(&entry.draft.quote),
                entry.draft.body
            ));
        }
        text
    }

    fn send_to_agent(
        &self,
        message: AgentMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let hook = agent_hook(cx).context("No agent is set up.")?;
        let workspace = self
            .workspace
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .context("The review's window is closed.")?;
        workspace.update(cx, |workspace, cx| hook(workspace, message, window, cx));
        Ok(())
    }

    pub fn send_verdict(
        &mut self,
        verdict: Verdict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let summary = self.summary_editor.read(cx).text(cx);
        let review = self.review_text(Some(verdict), &summary);
        let sent = match self.destination {
            Destination::Agent => Task::ready(self.send_to_agent(
                AgentMessage {
                    title: format!("Plan review: {}", self.title()),
                    prompt: format!(
                        "{}\n\nThe plan is {}.\n\n{review}",
                        verdict.instruction(),
                        self.abs_path.display()
                    ),
                    send: true,
                },
                window,
                cx,
            )),
            Destination::File => {
                let fs = self.project.read(cx).fs().clone();
                let path = self.review_file_path();
                cx.background_spawn(async move { fs.atomic_write(path, review).await })
            }
        };
        self.sending = true;
        self.send_error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = sent.await;
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match &result {
                    Ok(()) => this.finish_verdict(verdict, window, cx),
                    Err(error) => this.send_error = Some(error.to_string().into()),
                }
                cx.notify();
            })?;
            result
        })
    }

    fn finish_verdict(&mut self, verdict: Verdict, window: &mut Window, cx: &mut Context<Self>) {
        self.status = verdict.status();
        let ids: Vec<i64> = self
            .drafts
            .drain(..)
            .filter_map(|entry| entry.draft.id)
            .collect();
        self.summary_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        let database = self.database.clone();
        let plan_key = self.plan_key();
        let status = self.status.to_db().to_string();
        cx.background_spawn(async move {
            database.delete_drafts(ids).await?;
            database.set_status(plan_key, status).await
        })
        .detach_and_log_err(cx);
    }

    fn facts_text(&self, index: usize, section: &Section, cx: &App) -> String {
        let mut lines = Vec::new();
        let files: Vec<String> = section
            .files
            .iter()
            .map(|file| match self.file_state(index, file, cx) {
                FileState::Exists => file.path.clone(),
                FileState::New => format!("{} (new)", file.path),
                FileState::NotFound => format!("{} (not found)", file.path),
            })
            .collect();
        if !files.is_empty() {
            lines.push(format!("- Files: {}", files.join(", ")));
        }
        if !section.produces.is_empty() {
            lines.push(format!("- Produces: {}", section.produces.join("; ")));
        }
        if !section.uses.is_empty() {
            let uses: Vec<String> = section
                .uses
                .iter()
                .map(|identifier| match self.use_mark(identifier) {
                    Some((mark, _)) => format!("{identifier} ({mark})"),
                    None => identifier.clone(),
                })
                .collect();
            lines.push(format!("- Uses: {}", uses.join(", ")));
        }
        lines.push(format!(
            "- Tests: {}",
            if section.tests.is_empty() {
                "none named".to_string()
            } else {
                section.tests.join(", ")
            }
        ));
        for finding in self.findings() {
            if index > 0 && finding.task == Some(index - 1) {
                lines.push(format!("- Gap: {}. {}", finding.title, finding.evidence));
            }
        }
        lines.join("\n")
    }

    fn ask_agent(&mut self, _: &AskAgent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(section) = self.section(self.selected) else {
            return;
        };
        let heading = self.section_heading(self.selected);
        let prompt = format!(
            "About {heading} of the plan at {}:\n\n{}\n\nWhat Photon checked:\n{}\n\n",
            self.abs_path.display(),
            self.section_text(section),
            self.facts_text(self.selected, section, cx)
        );
        let message = AgentMessage {
            title: format!("Plan: {heading}"),
            prompt,
            send: false,
        };
        if let Err(error) = self.send_to_agent(message, window, cx) {
            self.note = Some(error.to_string().into());
            cx.notify();
        }
    }

    fn send_comments_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let summary = self.summary_editor.read(cx).text(cx);
        let message = AgentMessage {
            title: format!("Plan review: {}", self.title()),
            prompt: format!(
                "Here are my comments so far on your plan at {}. No verdict yet.\n\n{}",
                self.abs_path.display(),
                self.review_text(None, &summary)
            ),
            send: true,
        };
        if let Err(error) = self.send_to_agent(message, window, cx) {
            self.note = Some(error.to_string().into());
            cx.notify();
        }
    }

    fn open_path(&self, path: ProjectPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace
                .open_path(path, None, true, window, cx)
                .detach_and_log_err(cx)
        });
    }

    fn spec_project_path(&self, cx: &App) -> Option<ProjectPath> {
        let (path, _) = self.spec.as_ref()?;
        let style = self.project.read(cx).path_style(cx);
        let path = RelPath::new(Path::new(path), style).ok()?;
        Some(ProjectPath {
            worktree_id: self.project_path.worktree_id,
            path: path.into_owned().into(),
        })
    }

    fn file_state(&self, index: usize, file: &crate::FileRef, cx: &App) -> FileState {
        if index == 0 {
            return if self.file_exists(&file.path, cx) {
                FileState::Exists
            } else {
                FileState::NotFound
            };
        }
        let exists = |path: &str| self.file_exists(path, cx);
        file_state(&self.plan, index - 1, file, &exists)
    }

    /// "task 2" when another task defines it, "undefined" when nothing does.
    fn use_mark(&self, identifier: &str) -> Option<(String, Color)> {
        if let Some(definer) = self.plan.definer(identifier) {
            return Some((task_label(&self.plan, definer).to_lowercase(), Color::Muted));
        }
        let found = self.found.as_ref()?;
        (!found.contains(last_segment(identifier))).then(|| ("undefined".to_string(), Color::Error))
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.key != "c" || keystroke.modifiers.modified() {
            return;
        }
        let typing = self.composer.as_ref().is_some_and(|composer| {
            composer
                .editor
                .focus_handle(cx)
                .contains_focused(window, cx)
        }) || self
            .summary_editor
            .focus_handle(cx)
            .contains_focused(window, cx);
        if typing {
            return;
        }
        let paragraph = self.selected_paragraph.unwrap_or(0);
        self.open_composer(paragraph, None, window, cx);
        cx.stop_propagation();
    }
}

fn quote(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut quoted: Vec<String> = lines
        .iter()
        .take(QUOTED_LINES)
        .map(|line| format!("> {line}").trim_end().to_string())
        .collect();
    if lines.len() > QUOTED_LINES {
        quoted.push("> …".into());
    }
    quoted.join("\n")
}

fn chip(label: impl Into<SharedString>, color: Color, cx: &App) -> AnyElement {
    let label = label.into();
    let tint = color.color(cx);
    div()
        .debug_selector({
            let label = label.clone();
            move || format!("plan-chip-{label}")
        })
        .child(
            ui::Chip::new(label)
                .label_color(color)
                .label_size(LabelSize::XSmall)
                .when(color != Color::Muted, |chip| {
                    chip.bg_color(tint.opacity(0.12))
                        .border_color(tint.opacity(0.3))
                }),
        )
        .into_any_element()
}

fn code_label(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    Label::new(text)
        .size(LabelSize::Small)
        .color(Color::Accent)
        .buffer_font(cx)
        .into_any_element()
}

fn key_hint(keys: &'static str, label: &'static str) -> AnyElement {
    let keystrokes: Vec<KeybindingKeystroke> = keys
        .split(' ')
        .filter_map(|key| Keystroke::parse(key).ok())
        .map(KeybindingKeystroke::from_keystroke)
        .collect();
    h_flex()
        .gap_1()
        .child(ui::KeyBinding::from_keystrokes(keystrokes.into(), false).size(px(11.)))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .into_any_element()
}

impl PlanReviewItem {
    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut meta = format!(
            "{} · {} task{}",
            self.plan.kind.label(),
            self.plan.tasks.len(),
            if self.plan.tasks.len() == 1 { "" } else { "s" }
        );
        if self.spec.is_some() {
            meta.push_str(" · spec linked");
        }
        h_flex()
            .items_start()
            .gap_3()
            .px_3()
            .pt_2()
            .pb_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div().debug_selector(|| "plan-title".into()).child(
                            Label::new(self.title())
                                .size(LabelSize::Large)
                                .weight(gpui::FontWeight::SEMIBOLD)
                                .truncate(),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(
                                Label::new(self.display_path.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                div().debug_selector(|| "plan-meta".into()).child(
                                    Label::new(meta).size(LabelSize::Small).color(Color::Muted),
                                ),
                            )
                            .child(
                                div()
                                    .debug_selector(move || "plan-status".into())
                                    .child(chip(self.status.label(), self.status.color(), cx)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(self.render_more_menu(cx))
                    .child(self.render_verdict_button(Verdict::RequestChanges, cx))
                    .child(self.render_verdict_button(Verdict::Approve, cx)),
            )
    }

    fn render_more_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let item = cx.weak_entity();
        let source = self.project_path.clone();
        let spec = self.spec_project_path(cx);
        let agent = agent_hook(cx).is_some();
        let menu = ui::PopoverMenu::new("plan-more")
            .trigger_with_tooltip(
                IconButton::new("plan-more-trigger", IconName::Ellipsis)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::Small),
                Tooltip::text("More actions"),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let item = item.clone();
                let source = source.clone();
                let spec = spec.clone();
                Some(ui::ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.entry("Open source", None, {
                        let item = item.clone();
                        move |window, cx| {
                            item.update(cx, |item, cx| item.open_path(source.clone(), window, cx))
                                .log_err();
                        }
                    })
                    .when_some(spec, |menu, spec| {
                        let item = item.clone();
                        menu.entry("Open the spec", None, move |window, cx| {
                            item.update(cx, |item, cx| item.open_path(spec.clone(), window, cx))
                                .log_err();
                        })
                    })
                    .when(agent, |menu| {
                        let item = item.clone();
                        menu.entry("Send to agent", None, move |window, cx| {
                            item.update(cx, |item, cx| item.send_comments_to_agent(window, cx))
                                .log_err();
                        })
                    })
                }))
            });
        div().debug_selector(|| "plan-more".into()).child(menu)
    }

    fn render_verdict_button(&self, verdict: Verdict, cx: &Context<Self>) -> impl IntoElement {
        let item = cx.weak_entity();
        let (id, selector) = match verdict {
            Verdict::Approve => ("plan-approve", "plan-approve"),
            Verdict::RequestChanges => ("plan-request-changes", "plan-request-changes"),
        };
        let button = Button::new(id, verdict.title())
            .size(ButtonSize::Medium)
            .style(match verdict {
                Verdict::Approve => ButtonStyle::Tinted(TintColor::Accent),
                Verdict::RequestChanges => ButtonStyle::Outlined,
            });
        let popover = ui::PopoverMenu::new(SharedString::from(format!("{id}-popover")))
            .trigger(button)
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let item = item.upgrade()?;
                Some(cx.new(|cx| VerdictPopover::new(item, verdict, window, cx)))
            });
        div().debug_selector(move || selector.into()).child(popover)
    }

    fn render_map(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let mut map = v_flex()
            .id("plan-map")
            .w(MAP_WIDTH)
            .flex_none()
            .h_full()
            .p_2()
            .gap_0p5()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(colors.border)
            .child(
                div()
                    .debug_selector(|| "plan-progress".into())
                    .px_1()
                    .pb_1()
                    .child(
                        Label::new(self.progress_text())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );
        for (index, section) in self.plan.sections().enumerate() {
            map = map.child(self.render_step(index, section, cx));
            if index == self.selected && index > 0 {
                map = map.child(
                    h_flex()
                        .pl_7()
                        .pb_1()
                        .gap_1()
                        .flex_wrap()
                        .children(self.file_chips(index, section, cx))
                        .when(!section.tests.is_empty(), |this| {
                            let tests = section.tests.len();
                            this.child(chip(
                                format!("{tests} test{}", if tests == 1 { "" } else { "s" }),
                                Color::Info,
                                cx,
                            ))
                        }),
                );
            }
        }
        let findings = self.findings();
        if !findings.is_empty() {
            map = map.child(section_label(format!("Gaps · {}", findings.len())));
            for (position, finding) in findings.iter().enumerate() {
                let color = if finding.is_error() {
                    Color::Error
                } else {
                    Color::Warning
                };
                let target = finding.task.map_or(0, |task| task + 1);
                map = map.child(
                    map_note(
                        SharedString::from(format!("plan-gap-{position}")),
                        color,
                        finding.title.clone(),
                        finding.evidence.clone(),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.select(target, window, cx)),
                    ),
                );
            }
        }
        let decisions: Vec<(usize, &String)> = self
            .plan
            .sections()
            .enumerate()
            .flat_map(|(index, section)| {
                section
                    .decisions
                    .iter()
                    .map(move |decision| (index, decision))
            })
            .collect();
        if !decisions.is_empty() {
            map = map.child(section_label(format!("Decisions · {}", decisions.len())));
            for (position, (index, decision)) in decisions.into_iter().enumerate() {
                let place = match index {
                    0 => "Overview".to_string(),
                    index => task_label(&self.plan, index - 1),
                };
                map = map.child(
                    map_note(
                        SharedString::from(format!("plan-decision-{position}")),
                        Color::Accent,
                        decision.clone(),
                        place,
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.select(index, window, cx)),
                    ),
                );
            }
        }
        map
    }

    fn render_step(&self, index: usize, section: &Section, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let selected = index == self.selected;
        let state = self.tick_state(section);
        let subtitle = if index == 0 {
            section.labels.join(" · ")
        } else {
            let files = section.files.len();
            let tests = section.tests.len();
            let mut parts = Vec::new();
            if files > 0 {
                parts.push(format!("{files} file{}", if files == 1 { "" } else { "s" }));
            }
            if tests > 0 {
                parts.push(format!("{tests} test{}", if tests == 1 { "" } else { "s" }));
            } else if section.adds_code() && !section.names_test() {
                parts.push("no test".into());
            }
            parts.join(" · ")
        };
        h_flex()
            .id(("plan-step", index))
            .debug_selector(move || format!("plan-step-{index}"))
            .gap_2()
            .px_1()
            .py_0p5()
            .rounded_sm()
            .cursor_pointer()
            .when(selected, |this| this.bg(colors.element_selected))
            .on_click(cx.listener(move |this, _, window, cx| this.select(index, window, cx)))
            .child(
                // Ticking a step mustn't also select it.
                div()
                    .debug_selector(move || format!("plan-tick-{index}"))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Checkbox::new(("plan-tick", index), (state == TickState::Read).into())
                            .when(selected, |this| {
                                this.style(ui::ToggleStyle::Custom(colors.text_muted))
                            })
                            .tooltip(Tooltip::text("Read"))
                            .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                                this.set_ticked(index, *state == ToggleState::Selected, cx)
                            })),
                    ),
            )
            .when(index > 0, |this| {
                this.child(
                    div().w_5().flex_none().child(
                        Label::new(section.number.clone().unwrap_or_else(|| index.to_string()))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
            })
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(
                        Label::new(section.title.clone())
                            .truncate()
                            .when(state == TickState::Read, |label| label.color(Color::Muted)),
                    )
                    .when(state == TickState::Changed, |this| {
                        this.child(
                            div()
                                .debug_selector(move || format!("plan-step-changed-{index}"))
                                .child(
                                    Label::new("Changed since you read it")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                ),
                        )
                    }),
            )
            .when(!subtitle.is_empty(), |this| {
                this.child(
                    div().flex_none().child(
                        Label::new(subtitle)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
    }

    fn file_chips(&self, index: usize, section: &Section, cx: &App) -> Vec<AnyElement> {
        section
            .files
            .iter()
            .map(|file| match self.file_state(index, file, cx) {
                FileState::Exists => chip(file.path.clone(), Color::Muted, cx),
                FileState::New => chip(format!("{} · new", file.path), Color::Success, cx),
                FileState::NotFound => chip(format!("{} · not found", file.path), Color::Error, cx),
            })
            .collect()
    }

    fn render_facts(&self, index: usize, section: &Section, cx: &Context<Self>) -> AnyElement {
        let row = |label: &'static str, children: Vec<AnyElement>| {
            h_flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .w(px(80.))
                        .flex_none()
                        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                )
                .child(
                    h_flex()
                        .debug_selector(move || format!("plan-facts-{}", label.to_lowercase()))
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .flex_wrap()
                        .children(children),
                )
        };
        let uses: Vec<AnyElement> = section
            .uses
            .iter()
            .flat_map(|identifier| {
                let mut parts = vec![code_label(identifier.clone(), cx)];
                if let Some((mark, color)) = self.use_mark(identifier) {
                    parts.push(chip(mark, color, cx));
                }
                parts
            })
            .collect();
        let tests: Vec<AnyElement> = if section.tests.is_empty() {
            let text = if section.names_test() {
                "named by command only"
            } else {
                "none named"
            };
            vec![
                Label::new(text)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            ]
        } else {
            section
                .tests
                .iter()
                .map(|test| code_label(test.clone(), cx))
                .collect()
        };
        v_flex()
            .debug_selector(|| "plan-facts".into())
            .flex_none()
            .gap_1p5()
            .mb_4()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .when(!section.files.is_empty(), |this| {
                this.child(row("Files", self.file_chips(index, section, cx)))
            })
            .when(!section.produces.is_empty(), |this| {
                this.child(row(
                    "Produces",
                    section
                        .produces
                        .iter()
                        .map(|signature| code_label(signature.clone(), cx))
                        .collect(),
                ))
            })
            .when(!uses.is_empty(), |this| this.child(row("Uses", uses)))
            .child(row("Tests", tests))
            .into_any_element()
    }

    fn render_document(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let Some(section) = self.section(self.selected) else {
            return div().into_any_element();
        };
        let index = self.selected;
        let style = MarkdownStyle::themed(MarkdownFont::Preview, window, cx);
        let colors = cx.theme().colors();
        let task_findings: Vec<&Finding> = self
            .findings()
            .into_iter()
            .filter(|finding| index > 0 && finding.task == Some(index - 1))
            .filter(|finding| {
                !self
                    .drafts
                    .iter()
                    .any(|entry| entry.draft.finding.as_deref() == Some(finding.key.as_str()))
            })
            .collect();
        let finding_paragraph = |finding: &Finding| {
            finding
                .anchor
                .as_ref()
                .and_then(|anchor| {
                    (0..section.blocks.len()).find(|paragraph| {
                        self.block_text(section, *paragraph)
                            .contains(anchor.as_str())
                    })
                })
                .unwrap_or(0)
        };
        let mut document = v_flex()
            .id("plan-document")
            .track_scroll(&self.document_scroll)
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px_6()
            .py_4()
            .when(index > 0, |this| {
                this.child(self.render_facts(index, section, cx))
            });
        for (paragraph, markdown) in self.paragraphs.iter().enumerate() {
            let selected = self.selected_paragraph == Some(paragraph);
            let group = SharedString::from(format!("plan-paragraph-{paragraph}"));
            document = document.child(
                h_flex()
                    .id(("plan-paragraph", paragraph))
                    .debug_selector(move || format!("plan-paragraph-{paragraph}"))
                    .group(group.clone())
                    .flex_none()
                    .items_start()
                    .max_w(px(820.))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected_paragraph = Some(paragraph);
                        window.focus(&this.focus_handle, cx);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w_6()
                            .flex_none()
                            .debug_selector(move || format!("plan-paragraph-add-{paragraph}"))
                            .child(
                                IconButton::new(("plan-paragraph-add", paragraph), IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Accent)
                                    .tooltip(Tooltip::text("Comment on this paragraph"))
                                    .when(!selected, |button| {
                                        button.visible_on_hover(group.clone())
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_composer(paragraph, None, window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pb_2()
                            .pl_2()
                            .border_l_2()
                            .border_color(if selected {
                                colors.border_focused
                            } else {
                                gpui::transparent_black()
                            })
                            .child(
                                MarkdownElement::new(markdown.clone(), style.clone())
                                    .code_block_renderer(CodeBlockRenderer::Default {
                                        copy_button_visibility:
                                            CopyButtonVisibility::VisibleOnHover,
                                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                                        border: true,
                                    }),
                            ),
                    ),
            );
            for (position, finding) in task_findings.iter().enumerate() {
                if finding_paragraph(finding) == paragraph {
                    document =
                        document.child(self.render_finding_card(position, finding, paragraph, cx));
                }
            }
            for entry in &self.drafts {
                if entry.draft.task_key == section.key
                    && self.draft_paragraph(section, &entry.draft) == paragraph
                    && self
                        .composer
                        .as_ref()
                        .is_none_or(|composer| composer.editing != Some(entry.local_id))
                {
                    document = document.child(self.render_draft_card(entry, cx));
                }
            }
            if let Some(composer) = &self.composer
                && composer.paragraph == paragraph
            {
                document = document.child(self.render_composer(composer, cx));
            }
        }
        document.into_any_element()
    }

    /// A card under a paragraph, marked by a coloured edge on its left.
    fn card(&self, accent: Color, content: Div, cx: &App) -> Div {
        h_flex()
            .ml_8()
            .mb_3()
            .max_w(px(780.))
            .flex_none()
            .items_stretch()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .overflow_hidden()
            .child(div().w(px(2.)).flex_none().bg(accent.color(cx)))
            .child(content.flex_1().min_w_0().gap_1().px_3().py_2())
    }

    fn render_finding_card(
        &self,
        position: usize,
        finding: &Finding,
        paragraph: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let key = finding.key.clone();
        let added = finding.clone();
        let content = v_flex()
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new("Photon").size(LabelSize::XSmall))
                    .child(
                        Label::new("gap")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .debug_selector(move || format!("plan-finding-dismiss-{position}"))
                            .child(
                                Button::new(("plan-finding-dismiss", position), "Not a problem")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.dismiss_finding(key.clone(), cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("plan-finding-add-{position}"))
                            .child(
                                Button::new(("plan-finding-add", position), "Add to review")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.add_finding_to_review(&added, paragraph, cx)
                                    })),
                            ),
                    ),
            )
            .child(Label::new(finding.title.clone()).size(LabelSize::Small))
            .child(
                Label::new(finding.evidence.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        self.card(Color::Warning, content, cx)
            .debug_selector(move || format!("plan-finding-{position}"))
            .into_any_element()
    }

    fn render_draft_card(&self, entry: &DraftEntry, cx: &Context<Self>) -> AnyElement {
        let local_id = entry.local_id;
        let paragraph = entry.draft.paragraph;
        let content = v_flex()
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new("you").size(LabelSize::XSmall))
                    .child(
                        Label::new("draft")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new(("plan-draft-delete", local_id), "Delete")
                            .style(ButtonStyle::Subtle)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.delete_draft(local_id, cx)),
                            ),
                    )
                    .child(
                        Button::new(("plan-draft-edit", local_id), "Edit")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_composer(paragraph, Some(local_id), window, cx)
                            })),
                    ),
            )
            .child(Label::new(entry.draft.body.clone()).size(LabelSize::Small));
        self.card(Color::Accent, content, cx)
            .debug_selector(move || format!("plan-draft-{local_id}"))
            .into_any_element()
    }

    fn render_composer(&self, composer: &Composer, cx: &Context<Self>) -> AnyElement {
        let content = v_flex().child(composer.editor.clone()).child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("plan-composer-cancel", "Cancel")
                        .style(ButtonStyle::Subtle)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.close_composer(window, cx)),
                        ),
                )
                .child(
                    div().debug_selector(|| "plan-composer-save".into()).child(
                        Button::new("plan-composer-save", "Comment")
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_composer(window, cx)),
                            ),
                    ),
                ),
        );
        self.card(Color::Accent, content, cx)
            .debug_selector(|| "plan-composer".into())
            .key_context("PlanComposer")
            .on_action(
                cx.listener(|this, _: &SaveComment, window, cx| this.save_composer(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                    this.close_composer(window, cx)
                }),
            )
            .into_any_element()
    }

    fn render_footer(&self, cx: &Context<Self>) -> impl IntoElement {
        let comments = self.drafts.len();
        h_flex()
            .gap_4()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(key_hint("cmd-enter", "next task"))
            .child(key_hint("c", "comment on paragraph"))
            .child(key_hint("ctrl-alt-t", "ask the agent about this task"))
            .child(div().flex_1())
            .children(self.note.clone().map(|note| {
                Label::new(note)
                    .size(LabelSize::XSmall)
                    .color(Color::Warning)
            }))
            .child(
                div().debug_selector(|| "plan-footer-count".into()).child(
                    Label::new(format!(
                        "{comments} comment{} · sent with your verdict",
                        if comments == 1 { "" } else { "s" }
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
            )
    }
}

fn section_label(text: String) -> impl IntoElement {
    div()
        .px_1()
        .pt_3()
        .pb_0p5()
        .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
}

fn map_note(id: SharedString, color: Color, title: String, detail: String) -> Stateful<Div> {
    let selector = id.to_string();
    h_flex()
        .id(id)
        .debug_selector(move || selector)
        .items_start()
        .gap_2()
        .px_1()
        .py_0p5()
        .rounded_sm()
        .cursor_pointer()
        .child(div().pt_1p5().child(Indicator::dot().color(color)))
        .child(
            v_flex()
                .min_w_0()
                .flex_1()
                .child(Label::new(title).size(LabelSize::Small))
                .child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
}

impl Render for PlanReviewItem {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PlanReview")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(|this, _: &NextTask, window, cx| {
                this.select(this.selected + 1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousTask, window, cx| {
                this.select(this.selected.saturating_sub(1), window, cx)
            }))
            .on_action(cx.listener(Self::ask_agent))
            .child(self.render_header(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_map(cx))
                    .child(self.render_document(window, cx)),
            )
            .child(self.render_footer(cx))
    }
}

/// Approve plan / Request changes: summary, counts, and where the review goes.
struct VerdictPopover {
    item: WeakEntity<PlanReviewItem>,
    verdict: Verdict,
    focus_handle: FocusHandle,
    _observe_item: Subscription,
}

impl VerdictPopover {
    fn new(
        item: Entity<PlanReviewItem>,
        verdict: Verdict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = item.read(cx).summary_editor.focus_handle(cx);
        Self {
            _observe_item: cx.observe_in(&item, window, |_, _, _, cx| cx.notify()),
            item: item.downgrade(),
            verdict,
            focus_handle,
        }
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let verdict = self.verdict;
        let Ok(sending) = self
            .item
            .update(cx, |item, cx| item.send_verdict(verdict, window, cx))
        else {
            return;
        };
        // Detached, so closing the popover never cancels a write under way. A failure stays in
        // the popover, which stays open.
        cx.spawn_in(window, async move |this, cx| {
            sending.await?;
            this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

impl EventEmitter<DismissEvent> for VerdictPopover {}

impl Focusable for VerdictPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for VerdictPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(item) = self.item.upgrade() else {
            return div().into_any_element();
        };
        let item = item.read(cx);
        let agent = agent_hook(cx).is_some();
        let destination = item.destination;
        let choices = [
            (
                Destination::Agent,
                "Send to the agent",
                "Opens a new thread in the agent panel with the plan and your comments".to_string(),
            ),
            (
                Destination::File,
                "Save next to the plan",
                format!("Writes {}", item.review_file_display()),
            ),
        ];
        v_flex()
            .w(px(380.))
            .p_2()
            .gap_2()
            .elevation_2(cx)
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .child(Label::new(self.verdict.title()).weight(gpui::FontWeight::SEMIBOLD))
            .child(
                div().debug_selector(|| "plan-verdict-counts".into()).child(
                    Label::new(item.counts_text())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                div()
                    .debug_selector(|| "plan-verdict-summary".into())
                    .p_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(item.summary_editor.clone()),
            )
            .child(
                v_flex().gap_1().children(
                    choices
                        .into_iter()
                        .filter(|(choice, ..)| agent || *choice == Destination::File)
                        .map(|(choice, label, description)| {
                            let selector = match choice {
                                Destination::Agent => "plan-destination-agent",
                                Destination::File => "plan-destination-file",
                            };
                            div().debug_selector(move || selector.into()).child(
                                ui::ChoiceCard::radio(selector, label, destination == choice)
                                    .description(description)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.item
                                            .update(cx, |item, cx| {
                                                item.destination = choice;
                                                cx.notify();
                                            })
                                            .log_err();
                                    })),
                            )
                        }),
                ),
            )
            .children(
                item.send_error
                    .clone()
                    .map(|error| Label::new(error).size(LabelSize::Small).color(Color::Error)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("plan-verdict-cancel", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        div().debug_selector(|| "plan-verdict-send".into()).child(
                            Button::new("plan-verdict-send", "Send")
                                .style(ButtonStyle::Tinted(TintColor::Accent))
                                .disabled(item.sending)
                                .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                        ),
                    ),
            )
            .into_any_element()
    }
}

impl EventEmitter<()> for PlanReviewItem {}

impl Focusable for PlanReviewItem {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for PlanReviewItem {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Plan · {}", self.title()).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTodo))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.display_path.clone())
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.workspace = Some(workspace.weak_handle());
    }
}

#[derive(Serialize, Deserialize)]
struct SerializedPlanReview {
    path: PathBuf,
}

fn items_namespace(workspace_id: WorkspaceId) -> String {
    format!("photon_plan_review_items/{}", i64::from(workspace_id))
}

impl SerializableItem for PlanReviewItem {
    fn serialized_item_kind() -> &'static str {
        "PhotonPlanReview"
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
            let stored = store
                .scoped(&items_namespace(workspace_id))
                .read(&item_id.to_string())?
                .context("nothing stored for this plan review")?;
            let stored: SerializedPlanReview = serde_json::from_str(&stored)?;
            let (worktree, path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(&stored.path, false, cx)
                })
                .await?;
            let project_path = ProjectPath {
                worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
                path,
            };
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await?;
            cx.update(|window, cx| cx.new(|cx| PlanReviewItem::new(project, buffer, window, cx)))
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
        let stored = serde_json::to_string(&SerializedPlanReview {
            path: self.abs_path.clone(),
        })
        .log_err()?;
        let store = KeyValueStore::global(cx);
        Some(cx.background_spawn(async move {
            store
                .scoped(&items_namespace(workspace_id))
                .write(item_id.to_string(), stored)
                .await
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, Fs as _};
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::cell::RefCell;
    use std::sync::Arc;
    use util::path;
    use workspace::{AppState, MultiWorkspace};

    const PLAN: &str = include_str!("../test_fixtures/superpowers.md");
    const SPEC: &str = include_str!("../test_fixtures/spec.md");
    const PLAN_PATH: &str = "docs/plans/retry-charges.md";

    fn billing() -> serde_json::Value {
        json!({
            "Cargo.toml": "[package]\nname = \"billing\"\n",
            "docs": {
                "architecture.md": "# Billing\n",
                "plans": { "retry-charges.md": PLAN },
                "specs": { "2026-10-20-retry-charges-design.md": SPEC },
                // Proposed code in Markdown is not a definition.
                "notes.md": "```rust\nfn is_retryable() {}\n```\n",
            },
            "src": {
                "main.rs": "mod payments;\n",
                "payments.rs": "pub struct Gateway;\n\nimpl Gateway {\n    pub async fn charge(&self) {}\n}\n",
                "ledger.rs": "pub async fn record_payment() {}\n",
                "jobs.rs": "async fn retry_failed_invoices() {}\npub async fn failed_invoices() {}\n",
            },
        })
    }

    struct Setup {
        workspace: Entity<Workspace>,
        fs: Arc<FakeFs>,
        cx: VisualTestContext,
    }

    async fn setup(cx: &mut TestAppContext) -> Setup {
        cx.update(|cx| {
            let _state = AppState::test(cx);
            cx.set_global(db::AppDatabase::test_new());
            editor::init(cx);
            crate::init(cx);
            cx.bind_keys([
                gpui::KeyBinding::new("cmd-enter", NextTask, Some("PlanReview")),
                gpui::KeyBinding::new("ctrl-alt-t", AskAgent, Some("PlanReview")),
            ]);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/billing"), billing()).await;
        let project = Project::test(fs.clone(), [path!("/billing").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("window");
        let cx = VisualTestContext::from_window(window.into(), cx);
        Setup { workspace, fs, cx }
    }

    impl Setup {
        fn worktree_id(&mut self) -> project::WorktreeId {
            self.workspace.read_with(&self.cx, |workspace, cx| {
                workspace
                    .project()
                    .read(cx)
                    .worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).id())
                    .expect("a worktree")
            })
        }

        /// Opens the plan in an editor, then runs `plan: review`, as the command palette would.
        fn open_review(&mut self) -> Entity<PlanReviewItem> {
            let worktree_id = self.worktree_id();
            let open = self
                .workspace
                .update_in(&mut self.cx, |workspace, window, cx| {
                    workspace.open_path(
                        (
                            worktree_id,
                            RelPath::from_unix_str(PLAN_PATH).expect("path").into_arc(),
                        ),
                        None,
                        true,
                        window,
                        cx,
                    )
                });
            self.cx.run_until_parked();
            self.cx
                .foreground_executor()
                .block_test(open)
                .expect("opened");
            self.workspace
                .update_in(&mut self.cx, |workspace, window, cx| {
                    review_active_plan(workspace, window, cx)
                });
            self.cx.run_until_parked();
            self.active_review().expect("a plan review")
        }

        fn active_review(&mut self) -> Option<Entity<PlanReviewItem>> {
            self.workspace.read_with(&self.cx, |workspace, cx| {
                workspace
                    .active_item(cx)
                    .and_then(|item| item.downcast::<PlanReviewItem>())
            })
        }

        /// Opens the same plan in a new review, as after a restart.
        fn reopen(&mut self, item: &Entity<PlanReviewItem>) -> Entity<PlanReviewItem> {
            let buffer = item.read_with(&self.cx, |item, _| item.buffer.clone());
            let project = self
                .workspace
                .read_with(&self.cx, |workspace, _| workspace.project().clone());
            let reopened = self
                .workspace
                .update_in(&mut self.cx, |workspace, window, cx| {
                    let reopened = cx.new(|cx| PlanReviewItem::new(project, buffer, window, cx));
                    workspace.add_item_to_active_pane(
                        Box::new(reopened.clone()),
                        None,
                        true,
                        window,
                        cx,
                    );
                    reopened
                });
            self.cx.run_until_parked();
            reopened
        }

        fn click(&mut self, selector: &str) {
            let selector: &'static str = selector.to_string().leak();
            self.cx.run_until_parked();
            let bounds = self
                .cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"));
            self.cx
                .simulate_click(bounds.center(), gpui::Modifiers::none());
            self.cx.run_until_parked();
        }

        fn hover(&mut self, selector: &str) {
            let selector: &'static str = selector.to_string().leak();
            self.cx.run_until_parked();
            let bounds = self
                .cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"));
            self.cx
                .simulate_mouse_move(bounds.center(), None, gpui::Modifiers::none());
            self.cx.run_until_parked();
        }

        fn drawn(&mut self, selector: &str) -> bool {
            let selector: &'static str = selector.to_string().leak();
            self.cx.run_until_parked();
            self.cx.debug_bounds(selector).is_some()
        }

        fn select_task_three(&mut self, item: &Entity<PlanReviewItem>) {
            self.click("plan-step-3");
            assert_eq!(item.read_with(&self.cx, |item, _| item.selected()), 3);
        }

        /// Comments on the paragraph holding `needle`, by clicking it and pressing `c`.
        fn comment(&mut self, item: &Entity<PlanReviewItem>, needle: &str, body: &str) {
            let paragraph = item
                .read_with(&self.cx, |item, _| item.paragraph_containing(needle))
                .expect("the paragraph");
            self.click(&format!("plan-paragraph-{paragraph}"));
            self.cx.simulate_keystrokes("c");
            assert!(self.drawn("plan-composer"));
            self.cx.simulate_input(body);
            self.click("plan-composer-save");
        }
    }

    fn titles(item: &Entity<PlanReviewItem>, cx: &mut VisualTestContext) -> Vec<String> {
        item.read_with(cx, |item, _| {
            item.findings()
                .iter()
                .map(|finding| finding.title.clone())
                .collect()
        })
    }

    #[gpui::test]
    async fn a_plan_opens_as_a_review_with_tasks_facts_and_gaps(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let item = setup.open_review();
        assert_eq!(
            item.read_with(&setup.cx, |item, cx| item.tab_content_text(0, cx)),
            "Plan · Retry declined card charges with backoff"
        );
        assert_eq!(
            titles(&item, &mut setup.cx),
            [
                "Task 3 modifies src/gateway.rs, which doesn't exist",
                "Task 6 adds code with no test",
                "is_retryable is used in task 3 but defined nowhere",
                "No task covers “Refund partial captures”",
            ]
        );
        assert_eq!(
            item.read_with(&setup.cx, |item, _| item.progress_text()),
            "0 of 7 tasks read · 4 gaps"
        );
        assert!(setup.drawn("plan-gap-0"));
        assert!(setup.drawn("plan-decision-1"));

        setup.select_task_three(&item);
        assert!(setup.drawn("plan-chip-src/payments.rs"));
        assert!(setup.drawn("plan-chip-src/retry.rs · new"));
        assert!(setup.drawn("plan-chip-src/gateway.rs · not found"));
        assert!(setup.drawn("plan-chip-1 test"));
        assert!(setup.drawn("plan-facts-produces"));
        assert!(setup.drawn("plan-chip-task 2"));
        assert!(setup.drawn("plan-chip-undefined"));
        assert!(setup.drawn("plan-finding-0"), "the missing file's card");
        assert!(
            setup.drawn("plan-finding-1"),
            "the undefined identifier's card"
        );

        // A gap in the map shows its task.
        setup.click("plan-gap-1");
        assert_eq!(item.read_with(&setup.cx, |item, _| item.selected()), 6);
    }

    #[gpui::test]
    async fn the_header_names_the_kind_and_a_linked_spec(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let item = setup.open_review();
        assert!(setup.drawn("plan-meta"));
        assert!(setup.drawn("plan-chip-Not reviewed"));
        let (kind, spec) = item.read_with(&setup.cx, |item, _| {
            (
                item.plan.kind.label(),
                item.spec.as_ref().map(|(path, _)| path.clone()),
            )
        });
        assert_eq!(kind, "Superpowers plan");
        assert_eq!(
            spec.as_deref(),
            Some("docs/specs/2026-10-20-retry-charges-design.md")
        );
    }

    #[gpui::test]
    async fn findings_can_be_dismissed_or_added_to_the_review(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let item = setup.open_review();
        setup.select_task_three(&item);
        setup.click("plan-finding-dismiss-0");
        assert_eq!(
            item.read_with(&setup.cx, |item, _| item.progress_text()),
            "0 of 7 tasks read · 3 gaps"
        );
        setup.click("plan-finding-add-0");
        let drafts = item.read_with(&setup.cx, |item, _| {
            item.drafts().into_iter().cloned().collect::<Vec<_>>()
        });
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].finding.as_deref(), Some("undefined:is_retryable"));
        assert!(drafts[0].quote.starts_with("Replace the direct HTTP call"));
        assert!(
            !setup.drawn("plan-finding-0"),
            "added findings become comments"
        );

        let reopened = setup.reopen(&item);
        assert_eq!(
            reopened.read_with(&setup.cx, |item, _| item.progress_text()),
            "0 of 7 tasks read · 3 gaps",
            "the dismissal is kept"
        );
        assert_eq!(
            reopened.read_with(&setup.cx, |item, _| item.drafts().len()),
            1
        );
    }

    #[gpui::test]
    async fn comments_are_drafts_that_survive_reopening(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let item = setup.open_review();
        setup.select_task_three(&item);
        setup.comment(
            &item,
            "Replace the direct",
            "Where does is_retryable come from?",
        );
        assert!(setup.drawn("plan-draft-0"));
        assert!(!setup.drawn("plan-composer"));
        let reopened = setup.reopen(&item);
        let drafts = reopened.read_with(&setup.cx, |item, _| {
            item.drafts().into_iter().cloned().collect::<Vec<_>>()
        });
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].body, "Where does is_retryable come from?");
        assert_eq!(drafts[0].task_key, "task-3");

        // The gutter's + opens a comment too.
        setup.click("plan-step-3");
        setup.hover("plan-paragraph-1");
        setup.click("plan-paragraph-add-1");
        assert!(setup.drawn("plan-composer"));
    }

    #[gpui::test]
    async fn save_comment_keeps_the_task_and_saves_the_draft(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let item = setup.open_review();
        setup.select_task_three(&item);
        let paragraph = item
            .read_with(&setup.cx, |item, _| {
                item.paragraph_containing("Replace the direct")
            })
            .expect("the paragraph");
        setup.click(&format!("plan-paragraph-{paragraph}"));
        setup.cx.simulate_keystrokes("c");
        setup.cx.simulate_input("Why here?");
        setup.cx.dispatch_action(SaveComment);
        setup.cx.run_until_parked();
        assert!(!setup.drawn("plan-composer"));
        item.read_with(&setup.cx, |item, _| {
            assert_eq!(item.selected(), 3);
            assert_eq!(item.drafts().len(), 1);
        });
        let keymap = include_str!("../../../assets/keymaps/default-macos.json");
        assert!(keymap.contains("\"context\": \"PlanComposer > Editor\""));
        assert!(keymap.contains("\"cmd-enter\": \"plan::SaveComment\""));
    }

    #[gpui::test]
    async fn approving_writes_the_review_next_to_the_plan(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let item = setup.open_review();
        setup.select_task_three(&item);
        setup.comment(&item, "Replace the direct", "A 402 must not be retried.");
        setup.click("plan-tick-3");
        setup.click("plan-approve");
        assert!(setup.drawn("plan-verdict-counts"));
        assert!(!setup.drawn("plan-destination-agent"), "no agent is set up");
        setup.click("plan-verdict-summary");
        setup.cx.simulate_input("Looks right.");
        setup.click("plan-verdict-send");

        let review = setup
            .fs
            .load(Path::new(path!(
                "/billing/docs/plans/retry-charges.review.md"
            )))
            .await
            .expect("the review file");
        assert!(review.starts_with("# Plan review: Retry declined card charges with backoff\n"));
        assert!(review.contains("Verdict: Approved"), "{review}");
        assert!(review.contains("Looks right."), "{review}");
        assert!(
            review.contains("### Task 3: Retry in Gateway::charge"),
            "{review}"
        );
        assert!(
            review.contains("> Replace the direct HTTP call with `retry_with_backoff("),
            "{review}"
        );
        assert!(
            review.contains("\n\nA 402 must not be retried.\n"),
            "{review}"
        );
        assert_eq!(
            item.read_with(&setup.cx, |item, _| item.status()),
            PlanStatus::Approved
        );
        assert!(setup.drawn("plan-chip-Approved"));
        assert!(item.read_with(&setup.cx, |item, _| item.drafts().is_empty()));
        assert_eq!(
            setup
                .reopen(&item)
                .read_with(&setup.cx, |item, _| item.status()),
            PlanStatus::Approved
        );
    }

    fn install_agent(setup: &mut Setup) -> Rc<RefCell<Vec<AgentMessage>>> {
        let sent = Rc::new(RefCell::new(Vec::new()));
        setup.cx.update(|_, cx| {
            set_hooks(
                PlanReviewHooks {
                    send_to_agent: Some(Rc::new({
                        let sent = sent.clone();
                        move |_, message, _, _| sent.borrow_mut().push(message)
                    })),
                },
                cx,
            )
        });
        sent
    }

    #[gpui::test]
    async fn requesting_changes_sends_the_comments_to_the_agent(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let sent = install_agent(&mut setup);
        let item = setup.open_review();
        setup.select_task_three(&item);
        setup.comment(
            &item,
            "Replace the direct",
            "Where does is_retryable come from?",
        );
        setup.click("plan-request-changes");
        assert!(setup.drawn("plan-destination-agent"));
        setup.click("plan-verdict-send");

        let sent = sent.borrow();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].send);
        assert_eq!(
            sent[0].title,
            "Plan review: Retry declined card charges with backoff"
        );
        let prompt = &sent[0].prompt;
        assert!(prompt.contains("I'm requesting changes"), "{prompt}");
        assert!(prompt.contains("retry-charges.md"), "{prompt}");
        assert!(prompt.contains("Verdict: Changes requested"), "{prompt}");
        assert!(
            prompt.contains("> Replace the direct HTTP call"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Where does is_retryable come from?"),
            "{prompt}"
        );
        assert_eq!(
            item.read_with(&setup.cx, |item, _| item.status()),
            PlanStatus::ChangesRequested
        );
        assert!(setup.drawn("plan-chip-Changes requested"));
    }

    #[gpui::test]
    async fn keys_move_between_tasks_and_ask_the_agent(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let sent = install_agent(&mut setup);
        let item = setup.open_review();
        setup
            .cx
            .simulate_keystrokes("cmd-enter cmd-enter cmd-enter");
        assert_eq!(item.read_with(&setup.cx, |item, _| item.selected()), 3);
        setup.cx.simulate_keystrokes("ctrl-alt-t");
        let sent = sent.borrow();
        assert_eq!(sent.len(), 1);
        assert!(!sent[0].send, "the question is the user's to type");
        let prompt = &sent[0].prompt;
        assert!(
            prompt.contains("### Task 3: Retry in Gateway::charge"),
            "{prompt}"
        );
        assert!(
            prompt.contains(
                "- Files: src/payments.rs, src/gateway.rs (not found), src/retry.rs (new)"
            ),
            "{prompt}"
        );
        assert!(prompt.contains("is_retryable (undefined)"), "{prompt}");
    }

    #[gpui::test]
    async fn ticks_persist_until_the_task_changes(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let item = setup.open_review();
        setup.click("plan-tick-1");
        setup.click("plan-tick-2");
        assert_eq!(
            item.read_with(&setup.cx, |item, _| item.progress_text()),
            "2 of 7 tasks read · 4 gaps"
        );
        let reopened = setup.reopen(&item);
        assert_eq!(
            reopened.read_with(&setup.cx, |item, _| item.progress_text()),
            "2 of 7 tasks read · 4 gaps"
        );
        let buffer = reopened.read_with(&setup.cx, |item, _| item.buffer.clone());
        buffer.update(&mut setup.cx, |buffer, cx| {
            let text = buffer.text();
            let at = text.find("`Backoff` starts at 100 ms").expect("text");
            buffer.edit([(at..at, "Note: ")], None, cx);
        });
        setup.cx.executor().advance_clock(REPARSE_DEBOUNCE);
        setup.cx.run_until_parked();
        assert!(setup.drawn("plan-step-changed-1"));
        assert!(!setup.drawn("plan-step-changed-2"));
        assert_eq!(
            reopened.read_with(&setup.cx, |item, _| item.progress_text()),
            "1 of 7 tasks read · 4 gaps"
        );
    }

    #[gpui::test]
    async fn documents_without_tasks_do_not_open(cx: &mut TestAppContext) {
        let mut setup = setup(cx).await;
        let worktree_id = setup.worktree_id();
        let open = setup
            .workspace
            .update_in(&mut setup.cx, |workspace, window, cx| {
                open_plan_review(
                    workspace,
                    ProjectPath {
                        worktree_id,
                        path: RelPath::from_unix_str("docs/architecture.md")
                            .expect("path")
                            .into_arc(),
                    },
                    window,
                    cx,
                )
            });
        setup.cx.run_until_parked();
        assert!(setup.cx.foreground_executor().block_test(open).is_err());
        assert!(setup.active_review().is_none());
    }
}
