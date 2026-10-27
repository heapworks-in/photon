use std::collections::{HashMap, HashSet};
use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use buffer_diff::BufferDiff;
use editor::{
    Anchor, Editor, EditorEvent, ExcerptRange, MultiBuffer, MultiBufferSnapshot, PathKey,
    RowHighlightOptions, SelectionEffects,
    display_map::{
        BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock,
    },
    scroll::Autoscroll,
};
use futures::future::{FutureExt as _, Shared, join_all, try_join_all};
use gpui::{
    Action as _, AnyWindowHandle, App, AsyncApp, AsyncWindowContext, BackgroundExecutor, Context,
    DismissEvent, DragMoveEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Global,
    MouseButton, Pixels, PromptLevel, Render, ScrollHandle, SharedString, Stateful, Subscription,
    Task, WeakEntity, Window, actions, px, rgb,
};
use language::{Buffer, Capability, Point};
use multi_buffer::{ToPoint as _, build_excerpt_ranges};
use parking_lot::Mutex;
use project::{
    Project,
    lsp_store::OpenLspBufferHandle,
    trusted_worktrees::{TrustedWorktrees, TrustedWorktreesEvent},
};
use ui::{
    Checkbox, Disclosure, Indicator, TintColor, ToggleState, Tooltip, VisibleOnHover as _,
    prelude::*,
};
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{
    AppState, Item, OpenOptions, Toast, ToggleWorktreeSecurity, Workspace, WorkspaceDb,
    notifications::NotificationId, open_paths,
};

use settings::{PrReviewChecklistContent, Settings as _, SettingsStore};

use crate::{
    AgentComment, DismissReason, Dismissal, PromptInput, Verdict, agent_config_paths,
    parse_agent_comments, parse_verdicts, review_agent, review_prompt, verify_prompt,
};
use crate::{
    Answer, AutoCheck, AutoInputs, AutoResult, ChecklistItem, ChecklistSource, LocalRun,
    LocalRunsVersion, PrReviewSettings, Progress, applicable_items, checklist_note_slot, evaluate,
    parse_author_boxes, read_team_checklists, review_block,
};
use crate::{
    CallerMark, ChangedFunction, Checkout, ContextCache, ContextError, ContextExcerpt, Draft,
    FetchedCheckout, FileAnalysis, FileDiff, FileKind, FileStatus, FunctionContext, GhError,
    GhRunner, LoadedFile, MapFile, MapFunction, PostState, PrDetail, PrKey, PrSummary, Reanchor,
    RepoId, ReviewDb, ReviewEvent, ReviewThread, SUMMARY_SLOT, Side, SinceReviewPlan, Step,
    ToggleChangesSinceReview, Tracked, analyse_loaded_file, auth_status_request, base_text,
    build_loaded_file, build_review_map, call_edges, caller_touched, candidate_repos,
    classify_caller, classify_path, close_tab_before_head_moves, commentable_lines,
    context::buffer_path, context_excerpts, deleted_lines, detail_request, fetch_checkout,
    fetch_url, find_clone, finish_checkout, functions, gh_runner, git_diff, is_sendable,
    landing_note, merge_error_message, merge_request, parse_auth_status, parse_detail,
    parse_mutation, parse_reply, place, plan_since_review, reanchor, reply_request, reply_slot,
    resolve_request, review_request, review_root, since_review_files, viewed_request,
};
use crate::{CheckOutcome, MergeMethod, PrState, Readiness, remove_from_inbox};
use crate::{Move, ViewFile, is_generated, open_feedback, push_command, simplified_bases};
use crate::{TimelineEvent, issue_comment_request, parse_timeline, timeline_request};
use crate::{ViewedMark, diff_fingerprint};
use crate::{
    apply_suggestion, commit_file_request, file_at_commit_request, parse_file_contents,
    parse_suggestion, suggestion_block,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};

actions!(
    photon_review,
    [
        /// Shows the next step of the pull request review map.
        NextStep,
        /// Shows the previous step of the pull request review map.
        PreviousStep,
        /// Starts a draft review comment on the selected lines.
        AddComment
    ]
);

const LEFT_COMMENT_NOTE: &str = "Switch to the full diff to comment on removed lines";
const SIMPLIFIED_LEFT_COMMENT_NOTE: &str =
    "Show whitespace and moved code to comment on removed lines";
const VIEWED_DISABLED_TOOLTIP: &str = "Switch to the full diff to mark things viewed";
const HUNK_CONTEXT_LINES: u32 = 3;
const COMPOSER_MAX_LINES: u32 = 4;
const DEFAULT_MAP_WIDTH: Pixels = px(320.);
const MIN_MAP_WIDTH: Pixels = px(200.);
const MIN_CODE_WIDTH: Pixels = px(320.);
const MAP_RESIZE_HANDLE_WIDTH: Pixels = px(6.);
const TRUST_REASON: &str = "Trust this worktree to find callers";
const UNCHANGED_BACKGROUND: u32 = 0x07090b;
const INTERRUPTED_NOTE: &str =
    "May already be posted: a submit was interrupted. Include it again or delete it.";
const EARLIER_VERSION_NOTE: &str = "An earlier version of this comment was posted. Include this \
     version too, or delete it.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextState {
    Finding,
    Ready,
    Unavailable(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewedState {
    NotViewed,
    Viewed,
    ChangedSinceViewed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImpactRow {
    pub location: String,
    pub mark: CallerMark,
}

struct UnchangedContext;
struct NoteToast;
struct AgentReviewToast;

/// The database side of one draft. Writes run one at a time; a newer body replaces a pending
/// one, so the newest text always lands last without a timer.
#[derive(Default)]
struct DraftSync {
    pending: Option<PendingWrite>,
    id: Option<i64>,
    in_flight: bool,
    error: Option<SharedString>,
    /// Woken when the queue runs dry, so Submit can wait for its marks to land.
    idle_waiters: Vec<futures::channel::oneshot::Sender<()>>,
    /// The draft as last queued, or `None` once discarded, for settling a submit that finishes
    /// after its review closed.
    latest: Option<Draft>,
}

/// An empty body is never stored, so emptying a draft removes its row until text comes back.
enum PendingWrite {
    Save(Draft),
    DeleteRow,
}

enum SyncJob {
    Save(Draft),
    Delete(i64),
}

impl DraftSync {
    fn next_job(&mut self) -> Option<SyncJob> {
        match self.pending.take() {
            Some(PendingWrite::Save(mut draft)) => {
                draft.id = self.id;
                return Some(SyncJob::Save(draft));
            }
            Some(PendingWrite::DeleteRow) => {
                if let Some(id) = self.id.take() {
                    return Some(SyncJob::Delete(id));
                }
            }
            None => {}
        }
        self.in_flight = false;
        for waiter in self.idle_waiters.drain(..) {
            waiter.send(()).ok();
        }
        None
    }
}

/// The database side of one composer's unsent text: the summary or a thread's reply. As with
/// `DraftSync`, one writer drains it, so the newest text always lands last.
#[derive(Default)]
struct TextSync {
    pending: Option<String>,
    in_flight: bool,
    latest: String,
}

fn queue_text_write(
    sync: &Arc<Mutex<TextSync>>,
    database: &ReviewDb,
    key: &PrKey,
    slot: &str,
    body: String,
    executor: &BackgroundExecutor,
) {
    {
        let mut state = sync.lock();
        state.latest = body.clone();
        state.pending = Some(body);
        if std::mem::replace(&mut state.in_flight, true) {
            return;
        }
    }
    let sync = sync.clone();
    let database = database.clone();
    let key = key.clone();
    let slot = slot.to_string();
    // Detached so closing the review never cancels the write of what the user typed.
    executor
        .spawn(async move {
            loop {
                let body = {
                    let mut state = sync.lock();
                    let Some(body) = state.pending.take() else {
                        state.in_flight = false;
                        return;
                    };
                    body
                };
                database
                    .save_composer_text(key.clone(), slot.clone(), body)
                    .await
                    .log_err();
            }
        })
        .detach();
}

/// Clears a sent composer's stored text unless it was edited after sending.
fn clear_sent_text(
    sync: &Arc<Mutex<TextSync>>,
    sent: &str,
    database: &ReviewDb,
    key: &PrKey,
    slot: &str,
    executor: &BackgroundExecutor,
) {
    let unchanged = sync.lock().latest == sent;
    if unchanged {
        queue_text_write(sync, database, key, slot, String::new(), executor);
    }
}

struct DraftView {
    local_id: usize,
    composer: Option<Entity<Editor>>,
    sync: Arc<Mutex<DraftSync>>,
    _composer_subscription: Option<Subscription>,
}

struct SelectedLines {
    path: String,
    side: Side,
    start_line: u32,
    end_line: u32,
    anchor_text: String,
}

const ALL_ANSWERS: [Answer; 3] = [Answer::Yes, Answer::No, Answer::NotApplicable];

/// The general conversation, built the first time it's opened.
struct Conversation {
    description: Entity<Markdown>,
    /// None until the timeline is fetched, and again once a refresh makes it stale.
    events: Option<Vec<(TimelineEvent, Option<Entity<Markdown>>)>>,
    fetching: bool,
    error: Option<SharedString>,
    composer: Entity<Editor>,
    posting: bool,
    post_error: Option<SharedString>,
    _fetch_task: Task<()>,
    _composer_subscription: Subscription,
}

fn markdown_body(body: &str, cx: &mut App) -> Option<Entity<Markdown>> {
    (!body.trim().is_empty())
        .then(|| cx.new(|cx| Markdown::new(body.to_string().into(), None, None, cx)))
}

fn timeline_entry(event: TimelineEvent, cx: &mut App) -> (TimelineEvent, Option<Entity<Markdown>>) {
    let body = match &event {
        TimelineEvent::Comment(comment) => markdown_body(&comment.body, cx),
        TimelineEvent::Review { body, .. } => markdown_body(body, cx),
        TimelineEvent::Commit { .. } => None,
    };
    (event, body)
}

pub struct ReviewItem {
    project: Entity<Project>,
    detail: PrDetail,
    checkout: Checkout,
    files: Vec<LoadedFile>,
    analyses: Vec<Option<FileAnalysis>>,
    functions: Vec<ChangedFunction>,
    contexts: Vec<Option<FunctionContext>>,
    changed_rows: HashMap<String, Vec<Range<u32>>>,
    steps: Vec<Step>,
    selected: usize,
    viewed_steps: HashMap<String, ViewedMark>,
    /// Ticked steps whose files' diff changed since they were ticked.
    stale_steps: HashSet<String>,
    /// The head each file was marked viewed at on GitHub from here, by path.
    viewed_heads: HashMap<String, String>,
    context_state: ContextState,
    editor: Entity<Editor>,
    multibuffer: Entity<MultiBuffer>,
    context_cache: ContextCache,
    shown_paths: Vec<PathKey>,
    shown_context: Vec<ContextExcerpt>,
    expanded_context: HashSet<(EntityId, Point)>,
    label_blocks: HashSet<CustomBlockId>,
    /// Each changed function's callers, by function index, worked out once the call data
    /// arrives.
    callers: Vec<Vec<ImpactRow>>,
    database: ReviewDb,
    commentable: HashMap<String, Vec<RangeInclusive<u32>>>,
    drafts: Vec<Draft>,
    draft_views: Vec<DraftView>,
    next_draft_id: usize,
    reply_editors: HashMap<String, Entity<Editor>>,
    text_syncs: HashMap<String, Arc<Mutex<TextSync>>>,
    comment_blocks: HashSet<CustomBlockId>,
    login: String,
    summary_editor: Entity<Editor>,
    submitting: bool,
    submit_error: Option<SharedString>,
    merging: bool,
    merge_error: Option<SharedString>,
    new_commits: Option<NewCommits>,
    conversation_open: bool,
    conversation: Option<Conversation>,
    thread_errors: HashMap<String, SharedString>,
    file_errors: HashMap<String, SharedString>,
    submit_note: Option<SharedString>,
    resolving: HashSet<String>,
    /// Threads whose suggestion is being committed, and what came of the last commit.
    committing_suggestions: HashSet<String>,
    /// Threads the author is marking done (reply "Done", then resolve).
    finishing_threads: HashSet<String>,
    /// The agent pre-review: running, and what came of the last one.
    agent_reviewing: bool,
    agent_review_note: Option<(SharedString, Color)>,
    /// Findings the agent's check couldn't confirm, with its reason, until the next pre-review.
    dropped_findings: Vec<(AgentComment, String)>,
    dropped_open: bool,
    _agent_review_task: Task<()>,
    workspace: Option<WeakEntity<Workspace>>,
    suggestion_notes: HashMap<String, SharedString>,
    marking_viewed: HashSet<String>,
    last_review: Option<String>,
    full_files: Option<Vec<LoadedFile>>,
    /// Whether `full_files` is set aside for "Only what you haven't reviewed" rather than for the
    /// changes since your review.
    unreviewed_view: bool,
    hide_whitespace: bool,
    hide_moves: bool,
    hide_generated: bool,
    checklist_open: bool,
    photon_checked_open: bool,
    author_boxes_open: bool,
    /// Whether the View menu was opened, so a newly offered entry in it can be pointed out.
    view_menu_seen: bool,
    show_hidden: bool,
    verdict: ReviewEvent,
    map_scroll: ScrollHandle,
    map_width: Pixels,
    /// Each file's real base text, kept while the diff shows a simplified one.
    original_bases: HashMap<EntityId, Option<String>>,
    moves: Vec<Move>,
    _diff_view_task: Task<()>,
    since_review_note: Option<SharedString>,
    loading_since_review: bool,
    checklist_items: Vec<ChecklistItem>,
    auto_results: Vec<(AutoCheck, AutoResult)>,
    checklist_errors: Vec<String>,
    team_checklists: Vec<PrReviewChecklistContent>,
    team_error: Option<String>,
    checklist_answers: HashMap<String, Answer>,
    local_run: Option<LocalRun>,
    note_editors: HashMap<String, Entity<Editor>>,
    checklist_write: Shared<Task<()>>,
    _since_review_lsp_handles: Vec<OpenLspBufferHandle>,
    _since_review_task: Task<()>,
    _reanchor_task: Task<()>,
    _lsp_handles: Vec<OpenLspBufferHandle>,
    _subscriptions: Vec<Subscription>,
    _context_task: Task<()>,
}

/// Loading the author's new commits into an open review, and what came of the last try.
#[derive(Clone, Debug, PartialEq)]
pub enum NewCommits {
    Loading,
    Failed {
        message: SharedString,
        summary: PrSummary,
    },
}

pub(crate) struct PreparedReview {
    pub login: String,
    pub checkout: Checkout,
    pub detail: PrDetail,
    pub files: Vec<(FileDiff, Option<String>)>,
    pub team_checklists: (Vec<PrReviewChecklistContent>, Option<String>),
}

#[derive(Default)]
struct OpeningReviews(Arc<Mutex<HashSet<PrKey>>>);

impl Global for OpeningReviews {}

/// Frees the pull request for another `open_review` when its open finishes or is dropped.
struct OpeningGuard {
    opening: Arc<Mutex<HashSet<PrKey>>>,
    key: PrKey,
}

impl Drop for OpeningGuard {
    fn drop(&mut self) {
        self.opening.lock().remove(&self.key);
    }
}

/// Errors are shown as a toast in the window the user clicked from. A second call for a pull
/// request that is still opening does nothing.
pub fn open_review(
    summary: PrSummary,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let opening = cx.default_global::<OpeningReviews>().0.clone();
    if !opening.lock().insert(summary.key.clone()) {
        return Task::ready(Ok(()));
    }
    let guard = OpeningGuard {
        opening,
        key: summary.key.clone(),
    };
    let app_state = workspace.app_state().clone();
    let runner = gh_runner(cx);
    let database = ReviewDb::global(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = open_review_tab(summary, app_state, runner, database, cx).await;
        drop(guard);
        if let Err(error) = &result {
            let message = format!("{error:#}");
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<ReviewItem>(), message),
                        cx,
                    )
                })
                .log_err();
        }
        result
    })
}

/// The worktree path is reused for every head of a pull request, so trust given to an earlier
/// head must not carry over to code the user hasn't seen. Runs before the worktree is opened.
pub(crate) fn forget_stale_trust(checkout: &Checkout, cx: &mut App) {
    if !checkout.head_changed {
        return;
    }
    let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
        return;
    };
    trusted_worktrees.update(cx, |trusted_worktrees, cx| {
        trusted_worktrees.forget_trust(&checkout.worktree, cx);
        // Saved now, as Zed only saves trust when something is trusted.
        trusted_worktrees.schedule_serialization(cx, |trusted_paths, cx| {
            let database = WorkspaceDb::global(cx);
            cx.background_spawn(async move {
                database
                    .save_trusted_worktrees(trusted_paths)
                    .await
                    .log_err();
            })
        });
    });
}

/// Trusting a folder that contains Photon's review worktrees must not trust a pull request's code.
pub(crate) fn refuse_parent_trust(root: PathBuf, cx: &mut App) {
    if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
        trusted_worktrees.update(cx, |trusted_worktrees, _| {
            trusted_worktrees.refuse_parent_trust_under(root)
        });
    }
}

fn find_open_review(
    app_state: &AppState,
    key: &PrKey,
    cx: &App,
) -> Option<(AnyWindowHandle, Entity<Workspace>, Entity<ReviewItem>)> {
    app_state
        .workspace_store
        .read(cx)
        .workspaces_with_windows()
        .find_map(|(window, workspace)| {
            let workspace = workspace.upgrade()?;
            let item = workspace
                .read(cx)
                .items_of_type::<ReviewItem>(cx)
                .find(|item| item.read(cx).detail.summary.key == *key)?;
            Some((window, workspace, item))
        })
}

/// Opens a PR checkout as a tab in the current window.
pub(crate) async fn open_checkout(
    worktree: &Path,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> Result<workspace::OpenResult> {
    cx.update(|cx| {
        open_paths(
            &[worktree.to_path_buf()],
            app_state,
            OpenOptions::default(),
            cx,
        )
    })
    .await
}

/// Brings an already open review's project to the front: its window, and its tab in that window.
pub(crate) fn show_tab(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    window.activate_window();
    if let Some(multi_workspace) = window.root::<workspace::MultiWorkspace>().flatten() {
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx)
        });
    }
}

async fn open_review_tab(
    summary: PrSummary,
    app_state: Arc<AppState>,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    // Not read up front: the caller is usually mid-update on a workspace this would read.
    let candidates = cx.update(|_, cx| candidate_repos(cx))?;
    let source = CheckoutSource {
        fetch_url: fetch_url(&repo_id(&summary)),
        root: review_root(),
        candidates,
    };
    open_review_from(summary, source, app_state, runner, database, cx).await
}

/// Where a review's checkout comes from. Tests point it at a local repository.
pub(crate) struct CheckoutSource {
    pub fetch_url: String,
    pub root: PathBuf,
    pub candidates: Task<Vec<PathBuf>>,
}

pub(crate) async fn open_review_from(
    summary: PrSummary,
    source: CheckoutSource,
    app_state: Arc<AppState>,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let open = cx.update(|_, cx| find_open_review(&app_state, &summary.key, cx))?;
    let mut open_item = None;
    if let Some((window, workspace, item)) = open {
        // A failed load is tried again even when the inbox still has the tab's head.
        let up_to_date = item.read_with(cx, |item, _| {
            item.checkout.head_sha == summary.head_sha
                && !matches!(item.new_commits, Some(NewCommits::Failed { .. }))
        });
        window.update(cx, |_, window_context, cx| {
            show_tab(&workspace, window_context, cx);
            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&item, true, true, window_context, cx)
            });
            if !up_to_date {
                item.update(cx, |item, cx| {
                    item.set_new_commits(Some(NewCommits::Loading), cx)
                });
            }
        })?;
        if up_to_date {
            return Ok(());
        }
        open_item = Some((window, workspace.downgrade(), item.downgrade()));
    }

    let result = load_review(summary.clone(), source, app_state, runner, database, cx).await;
    // A replaced review is gone; one still open shows what went wrong, with Try again.
    let Some((window, workspace, item)) = open_item else {
        return result;
    };
    let (Some(workspace), Some(item)) = (workspace.upgrade(), item.upgrade()) else {
        return result;
    };
    let Err(error) = result else {
        item.update(cx, |item, cx| item.set_new_commits(None, cx));
        return Ok(());
    };
    // A save prompt may have shown another item of the tab.
    window.update(cx, |_, window_context, cx| {
        show_tab(&workspace, window_context, cx);
        workspace.update(cx, |workspace, cx| {
            workspace.activate_item(&item, true, true, window_context, cx)
        });
        item.update(cx, |item, cx| {
            item.set_new_commits(
                Some(NewCommits::Failed {
                    message: format!("{error:#}").into(),
                    summary,
                }),
                cx,
            )
        })
    })?;
    Ok(())
}

async fn load_review(
    summary: PrSummary,
    source: CheckoutSource,
    app_state: Arc<AppState>,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let CheckoutSource {
        fetch_url,
        root,
        candidates,
    } = source;
    let candidates = candidates.await;
    let (login, fetched) = cx
        .background_spawn({
            let summary = summary.clone();
            let runner = runner.clone();
            let root = root.clone();
            async move { fetch_review(&summary, fetch_url, root, candidates, runner).await }
        })
        .await?;
    close_tab_before_head_moves(&summary.key, &root, &fetched.head_sha, &app_state, cx).await?;
    let prepared = cx
        .background_spawn({
            let database = database.clone();
            async move { finish_review(&summary, login, fetched, runner, database).await }
        })
        .await?;
    cx.update(|_, cx| forget_stale_trust(&prepared.checkout, cx))?;
    let opened = open_checkout(&prepared.checkout.worktree, app_state, cx).await?;
    place_review(
        opened.workspace,
        opened.window.into(),
        prepared,
        database,
        cx,
    )
    .await
}

/// Adds the review to the workspace's active pane, or replaces an open review of the same pull
/// request in place, since that one describes an older head.
async fn place_review(
    workspace: Entity<Workspace>,
    window: AnyWindowHandle,
    prepared: PreparedReview,
    database: ReviewDb,
    cx: &mut AsyncApp,
) -> Result<()> {
    let PreparedReview {
        login,
        checkout,
        detail,
        files,
        team_checklists: (team_checklists, team_error),
    } = prepared;
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
    let loads = cx.update(|cx| {
        files
            .into_iter()
            .map(|(diff, base)| {
                let abs_path = checkout.worktree.join(&diff.path);
                build_loaded_file(
                    &project,
                    abs_path,
                    &checkout.worktree,
                    &checkout.head_sha,
                    base,
                    diff,
                    cx,
                )
            })
            .collect::<Vec<_>>()
    });
    let files = try_join_all(loads).await?;
    window.update(cx, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let key = detail.summary.key.clone();
            let existing = workspace
                .items_of_type::<ReviewItem>(cx)
                .find(|item| item.read(cx).detail.summary.key == key);
            let item = cx.new(|cx| {
                let mut item = ReviewItem::new(
                    project, detail, login, checkout, files, database, window, cx,
                );
                item.set_team_checklists(team_checklists, team_error, cx);
                if item.opens_on_conversation() {
                    item.open_conversation(window, cx);
                }
                item
            });
            let pane = existing.as_ref().and_then(|existing| {
                workspace
                    .panes()
                    .iter()
                    .find(|pane| pane.read(cx).index_for_item(existing).is_some())
                    .cloned()
            });
            match (existing, pane) {
                (Some(existing), Some(pane)) => pane.update(cx, |pane, cx| {
                    let index = pane.index_for_item(&existing);
                    pane.remove_item(existing.entity_id(), false, false, window, cx);
                    pane.add_item(Box::new(item), true, true, index, window, cx);
                }),
                _ => workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx),
            }
        })
    })?;
    Ok(())
}

pub(crate) fn repo_id(summary: &PrSummary) -> RepoId {
    RepoId {
        host: summary.key.host.to_lowercase(),
        owner: summary.key.owner.to_lowercase(),
        repo: summary.key.repo.to_lowercase(),
    }
}

/// Everything `open_review` does before it opens a window: the login check, the checkout, the
/// detail and the diff. The user's clone only gains Photon's refs and worktree entry (see `prepare_checkout`).
#[cfg(test)]
pub(crate) async fn prepare_review(
    summary: &PrSummary,
    fetch_url: String,
    root: PathBuf,
    candidates: Vec<PathBuf>,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
) -> Result<PreparedReview> {
    let (login, fetched) =
        fetch_review(summary, fetch_url, root, candidates, runner.clone()).await?;
    finish_review(summary, login, fetched, runner, database).await
}

/// The login check and the fetch: everything before the worktree moves. Returns the login.
pub(crate) async fn fetch_review(
    summary: &PrSummary,
    fetch_url: String,
    root: PathBuf,
    candidates: Vec<PathBuf>,
    runner: Arc<dyn GhRunner>,
) -> Result<(String, FetchedCheckout)> {
    let key = &summary.key;
    let hosts = parse_auth_status(&runner.run(auth_status_request()).await?)?;
    let login = hosts
        .into_iter()
        .find(|auth| auth.host.eq_ignore_ascii_case(&key.host))
        .and_then(|auth| auth.login)
        .ok_or_else(|| GhError::LoggedOut {
            host: Some(key.host.clone()),
        })?;

    let clone = find_clone(candidates, &repo_id(summary)).await;
    let fetched = fetch_checkout(key, &summary.base_ref, &fetch_url, clone, &root).await?;
    Ok((login, fetched))
}

pub(crate) async fn finish_review(
    summary: &PrSummary,
    login: String,
    fetched: FetchedCheckout,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
) -> Result<PreparedReview> {
    let key = &summary.key;
    let checkout = finish_checkout(fetched).await?;
    database
        .track(Tracked {
            key: key.clone(),
            node_id: summary.node_id.clone(),
            repo_dir: checkout.repo_dir.clone(),
            worktree: checkout.worktree.clone(),
        })
        .await?;
    let detail = parse_detail(key, &runner.run(detail_request(key, &login)).await?)?;

    let diffs = git_diff(&checkout.worktree, &checkout.merge_base, &checkout.head_sha).await?;
    let mut files = Vec::with_capacity(diffs.len());
    for diff in diffs {
        let base = base_text(&checkout.worktree, &checkout.merge_base, &diff).await?;
        files.push((diff, base));
    }
    let team_checklists = read_team_checklists(&checkout.worktree, &checkout.merge_base).await;
    Ok(PreparedReview {
        login,
        checkout,
        detail,
        files,
        team_checklists,
    })
}

fn status_label(diff: &FileDiff) -> &'static str {
    if diff.submodule {
        return "submodule";
    }
    if diff.symlink {
        return "symlink";
    }
    if diff.binary {
        return "binary file";
    }
    match diff.status {
        FileStatus::Added => "new file",
        FileStatus::Modified => "changed",
        FileStatus::Deleted => "deleted",
        FileStatus::Renamed { .. } => "renamed",
    }
}

fn unavailable_reason(error: ContextError) -> &'static str {
    match error {
        ContextError::NoCallHierarchy => "No language server offers call hierarchy for this code.",
        ContextError::TimedOut => "The language server didn't answer within 30 seconds.",
    }
}

fn mark_tag(mark: CallerMark) -> (&'static str, Color) {
    match mark {
        CallerMark::InThisPr => ("in this PR", Color::Accent),
        CallerMark::Unchanged => ("unchanged", Color::Muted),
        CallerMark::NotUpdated => ("not updated", Color::Error),
        CallerMark::Check => ("check", Color::Warning),
    }
}

fn buffer_for<'a>(files: &'a [LoadedFile], path: &str) -> Option<&'a Entity<Buffer>> {
    files
        .iter()
        .find(|file| file.diff.path == path)
        .and_then(|file| file.contents.as_ref())
        .map(|(buffer, _)| buffer)
}

fn path_key(sort_prefix: usize, buffer: &Entity<Buffer>, path: &str, cx: &App) -> Option<PathKey> {
    let path = match buffer.read(cx).file() {
        Some(file) => file.path().clone(),
        None => RelPath::from_unix_str(path).log_err()?.into_arc(),
    };
    Some(PathKey::with_sort_prefix(sort_prefix as u64, path))
}

fn label_renderer(
    label: SharedString,
    collapsed: bool,
    excerpt_index: usize,
    item: WeakEntity<ReviewItem>,
) -> RenderBlock {
    Arc::new(move |block: &mut BlockContext| {
        let item = item.clone();
        h_flex()
            .id(("photon-review-context", excerpt_index))
            .w_full()
            .pl(block.margins.gutter.full_width())
            .child(
                Label::new(label.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(collapsed, |this| {
                this.cursor_pointer().on_click(move |_, _, cx| {
                    item.update(cx, |item, cx| item.expand_context(excerpt_index, cx))
                        .log_err();
                })
            })
            .into_any_element()
    })
}

/// The number of open comments, shown on a step or file row; nothing when there are none.
fn comment_badge(selector: String, count: usize) -> Option<AnyElement> {
    (count > 0).then(|| {
        h_flex()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector)
            .flex_none()
            .gap_0p5()
            .child(
                Icon::new(IconName::Chat)
                    .size(IconSize::XSmall)
                    .color(Color::Accent),
            )
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Accent),
            )
            .tooltip(Tooltip::text(if count == 1 {
                "1 open comment".to_string()
            } else {
                format!("{count} open comments")
            }))
            .into_any_element()
    })
}

fn comment_card(id: ElementId, block: &BlockContext) -> Stateful<Div> {
    let colors = block.app.theme().colors();
    v_flex()
        .id(id)
        .ml(block.margins.gutter.full_width())
        .mr_4()
        .my_1()
        .p_2()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.panel_background)
}

/// Deleting an agent draft asks why in one small menu, so later pre-reviews learn from it.
fn dismiss_menu(
    item: WeakEntity<ReviewItem>,
    local_id: usize,
    group: SharedString,
) -> impl IntoElement {
    ui::PopoverMenu::new(("photon-review-dismiss", local_id))
        .trigger_with_tooltip(
            IconButton::new("delete", IconName::Trash)
                .icon_size(IconSize::Small)
                .visible_on_hover(group),
            Tooltip::text("Dismiss"),
        )
        .anchor(gpui::Anchor::TopRight)
        .menu(move |window, cx| {
            let item = item.clone();
            Some(ui::ContextMenu::build(window, cx, move |menu, _, _| {
                DismissReason::ALL.into_iter().fold(
                    menu.header("Dismiss because"),
                    |menu, reason| {
                        let item = item.clone();
                        menu.entry(reason.label(), None, move |_, cx| {
                            item.update(cx, |item, cx| {
                                item.dismiss_agent_draft(local_id, reason, cx)
                            })
                            .log_err();
                        })
                    },
                )
            }))
        })
}

fn draft_renderer(
    item: WeakEntity<ReviewItem>,
    local_id: usize,
    line: u32,
    body: SharedString,
    note: Option<SharedString>,
    posted_note: Option<&'static str>,
    composer: Option<Entity<Editor>>,
    sync: Arc<Mutex<DraftSync>>,
    can_suggest: bool,
    from_agent: bool,
) -> RenderBlock {
    Arc::new(move |block: &mut BlockContext| {
        let error = sync.lock().error.clone();
        let suggest_item = item.clone();
        let include_item = item.clone();
        let add_item = item.clone();
        let cancel_item = item.clone();
        let retry_item = item.clone();
        let edit_item = item.clone();
        let delete_item = item.clone();
        let focused_border = block.app.theme().colors().border_focused;
        let group = SharedString::from(format!("photon-review-draft-{local_id}"));
        comment_card(("photon-review-draft", local_id).into(), block)
            .group(group.clone())
            .when(composer.is_some(), |this| this.border_color(focused_border))
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new("you").size(LabelSize::Small))
                    .child(
                        Label::new(format!("· on line {line} · draft"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(composer.is_none(), |this| {
                        this.child(
                            h_flex()
                                .ml_auto()
                                .child(
                                    div()
                                        .debug_selector(move || {
                                            format!("photon-review-draft-edit-{local_id}")
                                        })
                                        .child(
                                            IconButton::new("edit", IconName::Pencil)
                                                .icon_size(IconSize::Small)
                                                .visible_on_hover(group.clone())
                                                .tooltip(Tooltip::text("Edit"))
                                                .on_click(move |_, window, cx| {
                                                    edit_item
                                                        .update(cx, |item, cx| {
                                                            item.edit_draft(local_id, window, cx)
                                                        })
                                                        .log_err();
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .debug_selector(move || {
                                            format!("photon-review-draft-delete-{local_id}")
                                        })
                                        .map(|this| {
                                            if from_agent {
                                                this.child(dismiss_menu(
                                                    delete_item.clone(),
                                                    local_id,
                                                    group.clone(),
                                                ))
                                            } else {
                                                this.child(
                                                    IconButton::new("delete", IconName::Trash)
                                                        .icon_size(IconSize::Small)
                                                        .visible_on_hover(group.clone())
                                                        .tooltip(Tooltip::text("Delete"))
                                                        .on_click(move |_, window, cx| {
                                                            delete_item
                                                                .update(cx, |item, cx| {
                                                                    item.cancel_draft(
                                                                        local_id, window, cx,
                                                                    )
                                                                })
                                                                .log_err();
                                                        }),
                                                )
                                            }
                                        }),
                                ),
                        )
                    }),
            )
            .map(|this| match &composer {
                Some(editor) => this.child(editor.clone()),
                None => this.child(Label::new(body.clone()).size(LabelSize::Small)),
            })
            .children(
                note.clone()
                    .map(|note| Label::new(note).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .when_some(posted_note, |this, posted_note| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new(posted_note)
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(
                            crate::button("include", "Include again")
                                .style(ButtonStyle::Outlined)
                                .on_click(move |_, _, cx| {
                                    include_item
                                        .update(cx, |item, cx| item.include_again(local_id, cx))
                                        .log_err();
                                }),
                        ),
                )
            })
            .children(error.map(|error| {
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    )
                    .child(
                        crate::button("retry", "Retry")
                            .style(ButtonStyle::Outlined)
                            .on_click(move |_, _, cx| {
                                retry_item
                                    .update(cx, |item, cx| item.queue_save(local_id, cx))
                                    .log_err();
                            }),
                    )
            }))
            .when(composer.is_some(), |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            crate::button("add", "Add to review")
                                .style(ButtonStyle::Tinted(TintColor::Accent))
                                .on_click(move |_, _, cx| {
                                    add_item
                                        .update(cx, |item, cx| item.collapse_draft(local_id, cx))
                                        .log_err();
                                }),
                        )
                        .when(can_suggest, |this| {
                            this.child(
                                crate::button("suggest", "Suggest change")
                                    .style(ButtonStyle::Subtle)
                                    .tooltip(Tooltip::text(
                                        "Adds the lines as a suggestion the author can commit",
                                    ))
                                    .on_click(move |_, window, cx| {
                                        suggest_item
                                            .update(cx, |item, cx| {
                                                item.suggest_change(local_id, window, cx)
                                            })
                                            .log_err();
                                    }),
                            )
                        })
                        .child(
                            crate::button("cancel", "Cancel")
                                .style(ButtonStyle::Subtle)
                                .on_click(move |_, window, cx| {
                                    cancel_item
                                        .update(cx, |item, cx| {
                                            item.cancel_draft(local_id, window, cx)
                                        })
                                        .log_err();
                                }),
                        ),
                )
            })
            .into_any_element()
    })
}

#[derive(Clone)]
struct SuggestionState {
    committable: bool,
    committing: bool,
    note: Option<SharedString>,
}

fn thread_renderer(
    item: WeakEntity<ReviewItem>,
    thread: ReviewThread,
    reply: Option<Entity<Editor>>,
    error: Option<SharedString>,
    resolve_pending: bool,
    suggestion: SuggestionState,
) -> RenderBlock {
    Arc::new(move |block: &mut BlockContext| {
        let id = SharedString::from(format!("photon-review-thread-{}", thread.id));
        let reply_item = item.clone();
        let resolve_item = item.clone();
        let reply_thread = thread.id.clone();
        let resolve_thread = thread.id.clone();
        let resolved = thread.is_resolved;
        comment_card(id.into(), block)
            .children(thread.comments.iter().enumerate().map(|(index, comment)| {
                let commit_item = item.clone();
                let commit_thread = thread.id.clone();
                let has_suggestion =
                    suggestion.committable && parse_suggestion(&comment.body).is_some();
                v_flex()
                    .child(Label::new(comment.author.clone()).size(LabelSize::Small))
                    .child(Label::new(comment.body.clone()).size(LabelSize::Small))
                    .when(has_suggestion, |this| {
                        this.child(
                            h_flex().pt_1().child(
                                crate::button(("commit-suggestion", index), "Commit suggestion")
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .disabled(suggestion.committing)
                                    .tooltip(Tooltip::text(
                                        "Commits this change to the pull request's branch on GitHub",
                                    ))
                                    .on_click(move |_, _, cx| {
                                        commit_item
                                            .update(cx, |item, cx| {
                                                item.commit_suggestion(&commit_thread, index, cx)
                                                    .detach_and_log_err(cx)
                                            })
                                            .log_err();
                                    }),
                            ),
                        )
                    })
            }))
            .children(suggestion.note.clone().map(|note| {
                Label::new(note)
                    .size(LabelSize::XSmall)
                    .color(Color::Success)
            }))
            .children(error.clone().map(|error| {
                Label::new(error)
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
            }))
            .child(
                h_flex()
                    .gap_1()
                    .children(reply.clone().map(|editor| div().flex_1().child(editor)))
                    .child(
                        crate::button("reply", "Reply")
                            .style(if reply.is_some() {
                                ButtonStyle::Tinted(TintColor::Accent)
                            } else {
                                ButtonStyle::Outlined
                            })
                            .on_click(move |_, _, cx| {
                                reply_item
                                    .update(cx, |item, cx| {
                                        item.click_reply(&reply_thread, cx).detach_and_log_err(cx)
                                    })
                                    .log_err();
                            }),
                    )
                    .child(
                        crate::button("resolve", if resolved { "Unresolve" } else { "Resolve" })
                            .style(ButtonStyle::Subtle)
                            .disabled(resolve_pending)
                            .on_click(move |_, _, cx| {
                                resolve_item
                                    .update(cx, |item, cx| {
                                        item.set_thread_resolved(&resolve_thread, !resolved, cx)
                                            .detach_and_log_err(cx)
                                    })
                                    .log_err();
                            }),
                    ),
            )
            .into_any_element()
    })
}

/// Returns false when a writer is already draining this draft's queue; it will pick up the
/// pending write.
fn claim_sync(sync: &Mutex<DraftSync>) -> bool {
    !std::mem::replace(&mut sync.lock().in_flight, true)
}

/// Runs the draft's queued writes now, unless a writer is already running them.
async fn flush_sync(
    this: &WeakEntity<ReviewItem>,
    local_id: usize,
    sync: &Arc<Mutex<DraftSync>>,
    database: &ReviewDb,
    cx: &mut AsyncApp,
) {
    if claim_sync(sync) {
        drain_sync(this.clone(), local_id, sync.clone(), database.clone(), cx).await;
    }
}

/// Queues the draft's last known text with a new post state, for when the review has closed.
fn requeue_latest(sync: &Mutex<DraftSync>, post_state: PostState) {
    let mut state = sync.lock();
    let latest = state
        .latest
        .clone()
        .filter(|draft| !draft.body.trim().is_empty());
    if let Some(mut latest) = latest {
        latest.post_state = post_state;
        state.pending = Some(PendingWrite::Save(latest));
    }
}

/// Runs a draft's queued writes in order. It works whether or not the review is still open, so a
/// delete queued after the review closes still follows any save in flight.
async fn drain_sync(
    this: WeakEntity<ReviewItem>,
    local_id: usize,
    sync: Arc<Mutex<DraftSync>>,
    database: ReviewDb,
    cx: &mut AsyncApp,
) {
    loop {
        let job = sync.lock().next_job();
        match job {
            None => return,
            Some(SyncJob::Delete(id)) => match database.delete_draft(id).await {
                Ok(()) => {
                    this.update(cx, |this, _| {
                        if let Some(index) = this.draft_index(local_id) {
                            this.drafts[index].id = None;
                        }
                    })
                    .ok();
                }
                Err(error) => {
                    log::error!("deleting a review draft failed: {error:#}");
                    let mut state = sync.lock();
                    if state.id.is_none() {
                        state.id = Some(id);
                    }
                    state.error = Some(format!("Couldn't save this draft: {error:#}").into());
                }
            },
            Some(SyncJob::Save(draft)) => {
                let result = database.save_draft(draft).await;
                let saved_id = {
                    let mut state = sync.lock();
                    match result {
                        Ok(id) => {
                            state.id = Some(id);
                            state.error = None;
                            Some(id)
                        }
                        Err(error) => {
                            log::error!("saving a review draft failed: {error:#}");
                            state.error =
                                Some(format!("Couldn't save this draft: {error:#}").into());
                            None
                        }
                    }
                };
                // The review may be closed by now; the write itself already happened.
                this.update(cx, |this, cx| {
                    if let (Some(id), Some(index)) = (saved_id, this.draft_index(local_id)) {
                        this.drafts[index].id = Some(id);
                    }
                    // Draft cards read the save error when the editor redraws them.
                    this.editor.update(cx, |_, cx| cx.notify());
                })
                .ok();
            }
        }
    }
}

/// Call hierarchy needs every head buffer open in its language server.
fn index_files(
    project: &Entity<Project>,
    files: &[LoadedFile],
    cx: &mut App,
) -> (Vec<OpenLspBufferHandle>, HashMap<String, Vec<Range<u32>>>) {
    let mut lsp_handles = Vec::new();
    let mut changed_rows = HashMap::new();
    for file in files {
        let Some((buffer, diff)) = &file.contents else {
            continue;
        };
        if buffer.read(cx).file().is_some() {
            lsp_handles.push(project.update(cx, |project, cx| {
                project.register_buffer_with_language_servers(buffer, cx)
            }));
        }
        let rows =
            functions::changed_rows(&diff.read(cx).snapshot(cx), &buffer.read(cx).snapshot());
        changed_rows.insert(file.diff.path.clone(), rows);
    }
    (lsp_handles, changed_rows)
}

impl ReviewItem {
    pub fn new(
        project: Entity<Project>,
        detail: PrDetail,
        login: String,
        checkout: Checkout,
        files: Vec<LoadedFile>,
        database: ReviewDb,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);
            // Commenting with the mouse: select lines, then right-click.
            editor.set_custom_context_menu(|editor, _, window, cx| {
                let focus = editor.focus_handle(cx);
                Some(ui::ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.context(focus)
                        .action("Add comment", AddComment.boxed_clone())
                        .separator()
                        .action("Copy", editor::actions::Copy.boxed_clone())
                }))
            });
            editor
        });
        let (lsp_handles, changed_rows) = index_files(&project, &files, cx);
        let mut subscriptions = Vec::new();
        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
            subscriptions.push(cx.subscribe(
                &trusted_worktrees,
                |this, _, event: &TrustedWorktreesEvent, cx| {
                    let TrustedWorktreesEvent::Trusted(worktree_store, _) = event else {
                        return;
                    };
                    let waiting =
                        this.context_state == ContextState::Unavailable(TRUST_REASON.into());
                    if waiting
                        && *worktree_store == this.project.read(cx).worktree_store().downgrade()
                    {
                        this._context_task = this.load_context(cx);
                    }
                },
            ));
        }
        let commentable = files
            .iter()
            .map(|file| (file.diff.path.clone(), commentable_lines(&file.diff)))
            .collect();
        let composer_texts = database
            .composer_texts(&detail.summary.key)
            .log_err()
            .unwrap_or_default();
        let summary_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, COMPOSER_MAX_LINES as usize, window, cx);
            editor.set_placeholder_text("Leave a summary (optional)", window, cx);
            if let Some(text) = composer_texts.get(SUMMARY_SLOT) {
                editor.set_text(text.as_str(), window, cx);
            }
            editor
        });
        // Comment is enabled by summary text, so the actions area redraws as it is typed.
        subscriptions.push(cx.subscribe(
            &summary_editor,
            |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    this.queue_text(SUMMARY_SLOT, text, cx);
                    cx.notify();
                }
            },
        ));
        let text_syncs = composer_texts
            .iter()
            .map(|(slot, text)| {
                let sync = TextSync {
                    latest: text.clone(),
                    ..TextSync::default()
                };
                (slot.clone(), Arc::new(Mutex::new(sync)))
            })
            .collect();
        let stored_drafts = database
            .drafts(&detail.summary.key)
            .log_err()
            .unwrap_or_default();
        let viewed_steps = database
            .viewed_steps(&detail.summary.key)
            .log_err()
            .unwrap_or_default();
        let viewed_heads = database
            .viewed_files(&detail.summary.key)
            .log_err()
            .unwrap_or_default();
        subscriptions.push(cx.observe_global::<SettingsStore>(|this, cx| {
            this.rebuild_checklist(cx);
        }));
        subscriptions.push(cx.observe_global::<LocalRunsVersion>(|this, cx| {
            this.local_run = this
                .database
                .local_run(&this.detail.summary.key)
                .log_err()
                .flatten();
            this.refresh_auto_results(cx);
            cx.notify();
        }));
        let checklist_answers = database
            .checklist_answers(&detail.summary.key)
            .log_err()
            .unwrap_or_default();
        let local_run = database.local_run(&detail.summary.key).log_err().flatten();
        // GitHub's record wins: it includes reviews submitted on github.com.
        let last_review = detail.summary.last_review_commit.clone().or_else(|| {
            database
                .last_review(&detail.summary.key)
                .log_err()
                .flatten()
        });
        let mut this = Self {
            project,
            detail,
            checkout,
            files,
            analyses: Vec::new(),
            functions: Vec::new(),
            contexts: Vec::new(),
            changed_rows,
            steps: Vec::new(),
            selected: 0,
            viewed_steps,
            stale_steps: HashSet::new(),
            viewed_heads,
            context_state: ContextState::Finding,
            editor,
            multibuffer,
            context_cache: ContextCache::default(),
            shown_paths: Vec::new(),
            shown_context: Vec::new(),
            expanded_context: HashSet::new(),
            label_blocks: HashSet::new(),
            callers: Vec::new(),
            database,
            commentable,
            drafts: Vec::new(),
            draft_views: Vec::new(),
            next_draft_id: 0,
            reply_editors: HashMap::new(),
            text_syncs,
            comment_blocks: HashSet::new(),
            login,
            summary_editor,
            submitting: false,
            submit_error: None,
            merging: false,
            merge_error: None,
            new_commits: None,
            conversation_open: false,
            conversation: None,
            thread_errors: HashMap::new(),
            file_errors: HashMap::new(),
            submit_note: None,
            resolving: HashSet::new(),
            committing_suggestions: HashSet::new(),
            finishing_threads: HashSet::new(),
            agent_reviewing: false,
            agent_review_note: None,
            dropped_findings: Vec::new(),
            dropped_open: false,
            _agent_review_task: Task::ready(()),
            workspace: None,
            suggestion_notes: HashMap::new(),
            marking_viewed: HashSet::new(),
            last_review,
            full_files: None,
            unreviewed_view: false,
            hide_whitespace: false,
            hide_moves: false,
            hide_generated: false,
            checklist_open: false,
            photon_checked_open: false,
            author_boxes_open: false,
            view_menu_seen: false,
            show_hidden: false,
            verdict: ReviewEvent::Comment,
            map_scroll: ScrollHandle::new(),
            map_width: DEFAULT_MAP_WIDTH,
            original_bases: HashMap::new(),
            moves: Vec::new(),
            _diff_view_task: Task::ready(()),
            since_review_note: None,
            loading_since_review: false,
            checklist_items: Vec::new(),
            auto_results: Vec::new(),
            checklist_errors: Vec::new(),
            team_checklists: Vec::new(),
            team_error: None,
            checklist_answers,
            local_run,
            note_editors: HashMap::new(),
            checklist_write: Task::ready(()).shared(),
            _since_review_lsp_handles: Vec::new(),
            _since_review_task: Task::ready(()),
            _reanchor_task: Task::ready(()),
            _lsp_handles: lsp_handles,
            _subscriptions: subscriptions,
            _context_task: Task::ready(()),
        };
        for draft in stored_drafts {
            this.push_draft(draft);
        }
        this.add_reply_editors(&composer_texts, window, cx);
        let unanswered: Vec<String> = this
            .checklist_answers
            .iter()
            .filter(|(_, answer)| **answer == Answer::No)
            .map(|(key, _)| key.clone())
            .collect();
        for key in unanswered {
            this.ensure_note_editor(&key, window, cx);
        }
        this.rebuild_checklist(cx);
        this.rebuild_map(&[], cx);
        this._context_task = this.load_context(cx);
        this._reanchor_task = this.reanchor_drafts(cx);
        this
    }

    /// A first look at a PR reads its description before the code: nothing viewed, no review
    /// of yours, no drafts, and there is a description to read.
    pub fn opens_on_conversation(&self) -> bool {
        !self.detail.body.trim().is_empty()
            && self.last_review.is_none()
            && self.drafts.is_empty()
            && self.viewed_steps.is_empty()
            && self.viewed_heads.is_empty()
            && !self.detail.files.iter().any(|file| file.viewed)
    }

    pub fn changes_since_review_offered(&self) -> bool {
        self.last_review
            .as_ref()
            .is_some_and(|reviewed| *reviewed != self.checkout.head_sha)
    }

    pub fn showing_changes_since_review(&self) -> bool {
        self.full_files.is_some() && !self.unreviewed_view
    }

    /// Offered once GitHub has one of this review's files viewed, or viewed and since changed.
    pub fn unreviewed_offered(&self) -> bool {
        self.unreviewed_view
            || self.all_files().iter().any(|file| {
                self.detail.files.iter().any(|state| {
                    state.path == file.diff.path && (state.viewed || state.changed_since_viewed)
                })
            })
    }

    pub fn showing_unreviewed(&self) -> bool {
        self.unreviewed_view && self.full_files.is_some()
    }

    fn cancel_subset_load(&mut self, cx: &mut Context<Self>) {
        self.loading_since_review = false;
        self._since_review_task = Task::ready(());
        self.dismiss_toast::<NoteToast>(cx);
        cx.notify();
    }

    /// Shows only what you haven't reviewed: files GitHub doesn't have viewed, in full, and files
    /// changed since you viewed them as the diff from the head you viewed them at.
    pub fn toggle_unreviewed(&mut self, cx: &mut Context<Self>) {
        self.since_review_note = None;
        if self.loading_since_review {
            self.cancel_subset_load(cx);
            return;
        }
        if let Some(full_files) = self.full_files.take() {
            self._since_review_task = Task::ready(());
            self.set_files(full_files, cx);
            if std::mem::take(&mut self.unreviewed_view) {
                return;
            }
        }
        // A file shown in full keeps its entities, so nothing is loaded twice.
        let share = |file: &LoadedFile| LoadedFile {
            diff: file.diff.clone(),
            contents: file.contents.clone(),
            note: file.note.clone(),
        };
        let head = self.checkout.head_sha.clone();
        let mut picks = Vec::new();
        let mut unknown = 0;
        for file in &self.files {
            let state = self
                .detail
                .files
                .iter()
                .find(|state| state.path == file.diff.path);
            match state {
                Some(state) if state.viewed => {}
                Some(state) if state.changed_since_viewed => {
                    match self
                        .viewed_heads
                        .get(&file.diff.path)
                        .filter(|viewed_at| **viewed_at != head)
                    {
                        Some(from) => picks.push((share(file), Some(from.clone()))),
                        None => {
                            unknown += 1;
                            picks.push((share(file), None));
                        }
                    }
                }
                _ => picks.push((share(file), None)),
            }
        }
        if picks.is_empty() {
            self.set_note("You've viewed every file.".into(), cx);
            return;
        }
        let mut froms: Vec<String> = picks.iter().filter_map(|(_, from)| from.clone()).collect();
        froms.sort();
        froms.dedup();
        if froms.is_empty() {
            self.show_unreviewed(
                picks.into_iter().map(|(file, _)| file).collect(),
                unknown,
                cx,
            );
            return;
        }
        let checkout = self.checkout.clone();
        let key = self.detail.summary.key.clone();
        let url = fetch_url(&repo_id(&self.detail.summary));
        let project = self.project.clone();
        self.loading_since_review = true;
        let item = cx.weak_entity();
        self.show_toast(
            Toast::new(
                NotificationId::unique::<NoteToast>(),
                "Loading what you haven't reviewed…",
            )
            .on_click("Cancel", move |_, cx| {
                item.update(cx, |item, cx| {
                    if item.loading_since_review {
                        item.toggle_unreviewed(cx);
                    }
                })
                .log_err();
            }),
            cx,
        );
        self._since_review_task = cx.spawn(async move |this, cx| {
            let since: HashMap<String, Vec<(FileDiff, Option<String>)>> = cx
                .background_spawn({
                    let checkout = checkout.clone();
                    async move {
                        let mut since = HashMap::new();
                        for from in froms {
                            let plan = plan_since_review(&checkout, &key, &url, &from).await;
                            if plan.message().is_some() {
                                continue;
                            }
                            match since_review_files(&checkout, &plan).await {
                                Ok(files) => {
                                    since.insert(from, files);
                                }
                                Err(error) => log::warn!("diff since a viewed head: {error:#}"),
                            }
                        }
                        since
                    }
                })
                .await;
            let mut unavailable = unknown;
            let loads = cx.update(|cx| {
                let mut loads = Vec::new();
                for (file, from) in picks {
                    let Some(from) = from else {
                        loads.push(Task::ready(Ok(file)));
                        continue;
                    };
                    let Some(files) = since.get(&from) else {
                        unavailable += 1;
                        loads.push(Task::ready(Ok(file)));
                        continue;
                    };
                    // Not in the diff since then: only the base moved under it.
                    let Some((diff, base)) = files
                        .iter()
                        .find(|(diff, _)| diff.path == file.diff.path)
                        .cloned()
                    else {
                        continue;
                    };
                    loads.push(build_loaded_file(
                        &project,
                        checkout.worktree.join(&diff.path),
                        &checkout.worktree,
                        &checkout.head_sha,
                        base,
                        diff,
                        cx,
                    ));
                }
                loads
            });
            let loaded = try_join_all(loads).await;
            this.update(cx, |this, cx| {
                this.loading_since_review = false;
                match loaded {
                    Ok(files) if files.is_empty() => {
                        this.set_note("Nothing has changed since you viewed it.".into(), cx)
                    }
                    Ok(files) => {
                        this.dismiss_toast::<NoteToast>(cx);
                        this.show_unreviewed(files, unavailable, cx);
                    }
                    Err(error) => this.set_note(format!("{error:#}").into(), cx),
                }
            })
            .log_err();
        });
        cx.notify();
    }

    /// `unavailable` files changed since viewed are shown in full: the head they were viewed at
    /// isn't known or couldn't be fetched.
    fn show_unreviewed(
        &mut self,
        files: Vec<LoadedFile>,
        unavailable: usize,
        cx: &mut Context<Self>,
    ) {
        let full = std::mem::take(&mut self.files);
        self.full_files = Some(full);
        self.unreviewed_view = true;
        self.set_files(files, cx);
        if unavailable > 0 {
            self.set_note(
                format!(
                    "Showing the full diff of {unavailable} file{} changed since you viewed {}: \
                     the commit you viewed {} at isn't available.",
                    if unavailable == 1 { "" } else { "s" },
                    if unavailable == 1 { "it" } else { "them" },
                    if unavailable == 1 { "it" } else { "them" },
                )
                .into(),
                cx,
            );
        }
        cx.notify();
    }

    pub fn toggle_changes_since_review(
        &mut self,
        _: &ToggleChangesSinceReview,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.since_review_note = None;
        if self.loading_since_review {
            self.cancel_subset_load(cx);
            return;
        }
        if let Some(full_files) = self.full_files.take() {
            self._since_review_task = Task::ready(());
            self.set_files(full_files, cx);
            if !std::mem::take(&mut self.unreviewed_view) {
                return;
            }
        }
        let Some(reviewed) = self
            .last_review
            .clone()
            .filter(|_| self.changes_since_review_offered())
        else {
            return;
        };
        let checkout = self.checkout.clone();
        let key = self.detail.summary.key.clone();
        let url = fetch_url(&repo_id(&self.detail.summary));
        let project = self.project.clone();
        self.loading_since_review = true;
        let item = cx.weak_entity();
        self.show_toast(
            Toast::new(
                NotificationId::unique::<NoteToast>(),
                "Loading the changes since your review…",
            )
            .on_click("Cancel", move |window, cx| {
                item.update(cx, |item, cx| {
                    if item.loading_since_review {
                        item.toggle_changes_since_review(&ToggleChangesSinceReview, window, cx);
                    }
                })
                .log_err();
            }),
            cx,
        );
        self._since_review_task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn({
                    let checkout = checkout.clone();
                    async move {
                        let plan = plan_since_review(&checkout, &key, &url, &reviewed).await;
                        let files = match &plan {
                            SinceReviewPlan::Unavailable { .. } => Vec::new(),
                            _ => since_review_files(&checkout, &plan).await?,
                        };
                        anyhow::Ok((plan, files))
                    }
                })
                .await;
            let loaded = match outcome {
                Ok((plan, files)) => match plan.message() {
                    Some(message) => Err(message.to_string()),
                    None => {
                        let loads = cx.update(|cx| {
                            files
                                .into_iter()
                                .map(|(diff, base)| {
                                    let abs_path = checkout.worktree.join(&diff.path);
                                    build_loaded_file(
                                        &project,
                                        abs_path,
                                        &checkout.worktree,
                                        &checkout.head_sha,
                                        base,
                                        diff,
                                        cx,
                                    )
                                })
                                .collect::<Vec<_>>()
                        });
                        try_join_all(loads)
                            .await
                            .map_err(|error| format!("{error:#}"))
                    }
                },
                Err(error) => Err(format!("{error:#}")),
            };
            this.update(cx, |this, cx| {
                this.loading_since_review = false;
                match loaded {
                    Ok(files) => {
                        this.dismiss_toast::<NoteToast>(cx);
                        let full = std::mem::take(&mut this.files);
                        this.full_files = Some(full);
                        this.set_files(files, cx);
                    }
                    Err(message) => this.set_note(message.into(), cx),
                }
            })
            .log_err();
        });
        cx.notify();
    }

    fn set_files(&mut self, files: Vec<LoadedFile>, cx: &mut Context<Self>) {
        self.files = files;
        self.reindex_files(cx);
        self.apply_diff_view(cx);
    }

    fn diff_simplified(&self) -> bool {
        self.hide_whitespace || self.hide_moves
    }

    fn toggle_hide_generated(&mut self, cx: &mut Context<Self>) {
        self.hide_generated = !self.hide_generated;
        self.reindex_files(cx);
        cx.notify();
    }

    fn toggle_diff_view(&mut self, whitespace: bool, cx: &mut Context<Self>) {
        if whitespace {
            self.hide_whitespace = !self.hide_whitespace;
        } else {
            self.hide_moves = !self.hide_moves;
        }
        self.apply_diff_view(cx);
        cx.notify();
    }

    /// Gives every shown file's diff the base text the toggles call for, then rebuilds the map
    /// from the diff that results.
    fn apply_diff_view(&mut self, cx: &mut Context<Self>) {
        let (hide_whitespace, hide_moves) = (self.hide_whitespace, self.hide_moves);
        let mut entries = Vec::new();
        for file in &self.files {
            let Some((buffer, diff)) = &file.contents else {
                continue;
            };
            if !hide_whitespace
                && !hide_moves
                && !self.original_bases.contains_key(&diff.entity_id())
            {
                continue;
            }
            let original = self
                .original_bases
                .entry(diff.entity_id())
                .or_insert_with(|| diff.read(cx).base_text_string(cx))
                .clone();
            let snapshot = buffer.read(cx).snapshot().text;
            entries.push((file.diff.path.clone(), original, snapshot, diff.clone()));
        }
        if entries.is_empty() {
            self.moves.clear();
            return;
        }
        self._diff_view_task = cx.spawn(async move |this, cx| {
            let (entries, bases, moves) = cx
                .background_spawn(async move {
                    let texts: Vec<String> = entries
                        .iter()
                        .map(|(_, _, snapshot, _)| snapshot.text())
                        .collect();
                    let view = if hide_whitespace || hide_moves {
                        let files: Vec<ViewFile> = entries
                            .iter()
                            .zip(&texts)
                            .map(|((path, original, _, _), new)| ViewFile {
                                path,
                                old: original.as_deref(),
                                new,
                            })
                            .collect();
                        let view = simplified_bases(&files, hide_whitespace, hide_moves);
                        (view.bases, view.moves)
                    } else {
                        let bases = entries.iter().map(|entry| entry.1.clone()).collect();
                        (bases, Vec::new())
                    };
                    (entries, view.0, view.1)
                })
                .await;
            for ((_, _, snapshot, diff), base) in entries.into_iter().zip(bases) {
                diff.update(cx, |diff, cx| {
                    diff.set_base_text(base.map(Into::into), snapshot, cx)
                })
                .await;
            }
            this.update(cx, |this, cx| {
                this.moves = moves;
                this.reindex_files(cx);
            })
            .log_err();
        });
    }

    fn reindex_files(&mut self, cx: &mut Context<Self>) {
        let (lsp_handles, changed_rows) = index_files(&self.project, &self.files, cx);
        // The full set's handles live in `_lsp_handles`; only the subset's are replaced here.
        self._since_review_lsp_handles = if self.full_files.is_some() {
            lsp_handles
        } else {
            Vec::new()
        };
        self.changed_rows = changed_rows;
        self.analyses = Vec::new();
        self.functions = Vec::new();
        self.contexts = Vec::new();
        self.callers = Vec::new();
        self.steps = Vec::new();
        self.selected = 0;
        self.expanded_context.clear();
        self.rebuild_map(&[], cx);
        self._context_task = self.load_context(cx);
    }

    pub fn drafts(&self) -> &[Draft] {
        &self.drafts
    }

    /// Gives each thread without one a reply box, keeping what was typed in existing ones.
    /// `stored` is unsent text from the database, by slot.
    fn add_reply_editors(
        &mut self,
        stored: &HashMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_ids: Vec<String> = self
            .detail
            .threads
            .iter()
            .map(|thread| thread.id.clone())
            .filter(|thread_id| !self.reply_editors.contains_key(thread_id))
            .collect();
        for thread_id in thread_ids {
            let slot = reply_slot(&thread_id);
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Reply…", window, cx);
                if let Some(text) = stored.get(&slot) {
                    editor.set_text(text.as_str(), window, cx);
                }
                editor
            });
            self._subscriptions.push(cx.subscribe(
                &editor,
                move |this, editor, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        let text = editor.read(cx).text(cx);
                        this.queue_text(&slot, text, cx);
                    }
                },
            ));
            self.reply_editors.insert(thread_id, editor);
        }
    }

    fn text_sync(&mut self, slot: &str) -> Arc<Mutex<TextSync>> {
        self.text_syncs.entry(slot.to_string()).or_default().clone()
    }

    fn queue_text(&mut self, slot: &str, body: String, cx: &mut Context<Self>) {
        let sync = self.text_sync(slot);
        queue_text_write(
            &sync,
            &self.database,
            &self.detail.summary.key,
            slot,
            body,
            cx.background_executor(),
        );
    }

    fn sendable_drafts(&self) -> usize {
        self.drafts
            .iter()
            .filter(|draft| is_sendable(draft, &self.checkout.head_sha))
            .count()
    }

    /// Drafts with text that Submit leaves out because they are outdated or not yet re-anchored.
    pub fn left_out_drafts(&self) -> usize {
        self.drafts
            .iter()
            .filter(|draft| {
                !draft.body.trim().is_empty()
                    && draft.post_state == PostState::Unposted
                    && !is_sendable(draft, &self.checkout.head_sha)
            })
            .count()
    }

    /// Why a draft may already be on GitHub. While a submit runs, its own drafts are marked, so
    /// nothing is said until it ends.
    pub fn posted_note(&self, draft: &Draft) -> Option<&'static str> {
        if self.submitting {
            return None;
        }
        match draft.post_state {
            PostState::Unposted => None,
            PostState::Sending => Some(INTERRUPTED_NOTE),
            PostState::EarlierVersionPosted => Some(EARLIER_VERSION_NOTE),
        }
    }

    pub fn left_out_note(&self) -> Option<String> {
        let mut parts = Vec::new();
        let left_out = self.left_out_drafts();
        if left_out > 0 {
            let plural = if left_out == 1 { "" } else { "s" };
            parts.push(format!(
                "{left_out} draft{plural} not included (outdated or not yet re-placed)"
            ));
        }
        let maybe_posted = self
            .drafts
            .iter()
            .filter(|draft| self.posted_note(draft).is_some())
            .count();
        if maybe_posted > 0 {
            parts.push(format!("{maybe_posted} may already be posted"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// Makes a draft that may already be posted sendable again, at the user's word.
    pub fn include_again(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        self.drafts[index].post_state = PostState::Unposted;
        self.queue_save(local_id, cx);
        self.show_comments(cx);
        cx.notify();
    }

    pub fn can_comment(&self, cx: &App) -> bool {
        self.sendable_drafts() > 0
            || !self.summary_editor.read(cx).text(cx).trim().is_empty()
            || self.checklist_block(cx).is_some()
    }

    fn buffer_for_path(&self, path: &str, cx: &App) -> Option<Entity<Buffer>> {
        self.multibuffer
            .read(cx)
            .all_buffers_iter()
            .find(|buffer| buffer_path(buffer, cx).as_deref() == Some(path))
            .or_else(|| buffer_for(&self.files, path).cloned())
    }

    fn draft_languages(&self, cx: &App) -> HashMap<String, String> {
        self.drafts
            .iter()
            .filter_map(|draft| {
                let buffer = self.buffer_for_path(&draft.path, cx)?;
                let language = buffer.read(cx).language()?.code_fence_block_name();
                Some((draft.path.clone(), language.to_string()))
            })
            .collect()
    }

    /// Posts every sendable draft as one GitHub review. The sent drafts are marked `sending` in
    /// the database first, so after a crash they come back as "may already be posted" rather than
    /// sendable. A failure keeps every draft and shows GitHub's message until the next submit.
    pub fn submit(
        &mut self,
        event: ReviewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.submitting {
            return Task::ready(Err(anyhow!("This review is already being submitted.")));
        }
        let key = self.detail.summary.key.clone();
        let head_sha = self.checkout.head_sha.clone();
        let summary = self.summary_editor.read(cx).text(cx);
        let checklist = self.checklist_block(cx);
        let summary_sync = self.text_sync(SUMMARY_SLOT);
        let commentable = self.commentable.clone();
        let languages = self.draft_languages(cx);
        let marked: Vec<(usize, Arc<Mutex<DraftSync>>)> = self
            .drafts
            .iter()
            .zip(&self.draft_views)
            .filter(|(draft, _)| is_sendable(draft, &head_sha))
            .map(|(_, view)| (view.local_id, view.sync.clone()))
            .collect();
        // Marked through each draft's own queue, so the mark lands after any save in flight,
        // a first insert included, and the request waits until every queue has run dry.
        let mut marks = Vec::new();
        for (local_id, sync) in &marked {
            if let Some(index) = self.draft_index(*local_id) {
                self.drafts[index].post_state = PostState::Sending;
                self.queue_save(*local_id, cx);
            }
            let mut state = sync.lock();
            if state.in_flight {
                let (waiter, idle) = futures::channel::oneshot::channel();
                state.idle_waiters.push(waiter);
                marks.push(idle);
            }
        }
        self.submitting = true;
        self.submit_error = None;
        self.submit_note = None;
        cx.notify();
        let runner = gh_runner(cx);
        let database = self.database.clone();
        cx.spawn_in(window, async move |this, cx| {
            for idle in marks {
                idle.await.ok();
            }
            // Built after the wait from each draft's latest version, which works even if the
            // review closed: a draft removed or emptied meanwhile is left out, and one edited
            // meanwhile goes with its newer text.
            let mut sent = Vec::new();
            let mut drafts = Vec::new();
            let mut unmarked = false;
            for (local_id, sync) in &marked {
                let state = sync.lock();
                let Some(latest) = state
                    .latest
                    .as_ref()
                    .filter(|draft| !draft.body.trim().is_empty())
                else {
                    continue;
                };
                unmarked |= state.error.is_some() || state.id.is_none();
                drafts.push(Draft {
                    post_state: PostState::Unposted,
                    from_agent: false,
                    ..latest.clone()
                });
                sent.push((*local_id, sync.clone(), latest.body.clone()));
            }
            let request = review_request(
                &key,
                &head_sha,
                event,
                summary.trim(),
                checklist.as_deref(),
                &drafts,
                &commentable,
                &|path| languages.get(path).cloned(),
            );
            let posted = if unmarked {
                Err(anyhow!(
                    "Couldn't save the drafts before posting, so nothing was sent."
                ))
            } else {
                runner.run(request).await.map_err(anyhow::Error::from)
            };

            if let Err(error) = posted {
                let review_open = this
                    .update(cx, |this, cx| {
                        this.submitting = false;
                        this.submit_error = Some(format!("{error:#}").into());
                        for (local_id, _) in &marked {
                            if let Some(index) = this.draft_index(*local_id) {
                                this.drafts[index].post_state = PostState::Unposted;
                                this.queue_save(*local_id, cx);
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                // Nothing reached GitHub, so the marks are cleared even if the review closed.
                if !review_open {
                    for (local_id, sync) in &marked {
                        requeue_latest(sync, PostState::Unposted);
                        flush_sync(&this, *local_id, sync, &database, cx).await;
                    }
                }
                return Err(error);
            }

            let finished = this.update_in(cx, |this, window, cx| {
                this.submitting = false;
                // Emptied while waiting, so left out of the review.
                for (local_id, _) in &marked {
                    let left_out = sent.iter().all(|(sent_id, _, _)| sent_id != local_id);
                    if let Some(index) = this.draft_index(*local_id)
                        && left_out
                    {
                        this.drafts[index].post_state = PostState::Unposted;
                    }
                }
                let mut posted = Vec::new();
                for (local_id, sync, body) in &sent {
                    let Some(index) = this.draft_index(*local_id) else {
                        // Discarded while posting; its own delete is already queued.
                        continue;
                    };
                    if this.drafts[index].body == *body {
                        this.drafts.remove(index);
                        this.draft_views.remove(index);
                        posted.push((*local_id, sync.clone()));
                    } else {
                        // Edited while posting: the earlier text is on GitHub, the newer stays.
                        this.drafts[index].post_state = PostState::EarlierVersionPosted;
                        this.queue_save(*local_id, cx);
                    }
                }
                if this.summary_editor.read(cx).text(cx) == summary {
                    this.summary_editor
                        .update(cx, |editor, cx| editor.set_text("", window, cx));
                }
                this.show_comments(cx);
                cx.notify();
                posted
            });
            let review_open = finished.is_ok();
            if !review_open {
                clear_sent_text(
                    &summary_sync,
                    &summary,
                    &database,
                    &key,
                    SUMMARY_SLOT,
                    cx.background_executor(),
                );
            }
            let posted = match finished {
                Ok(posted) => posted,
                // The review closed while posting: each draft's latest queued version says
                // whether it was edited after it was sent.
                Err(_) => sent
                    .iter()
                    .filter(|(_, sync, body)| {
                        let latest_body = sync.lock().latest.as_ref().map(|d| d.body.clone());
                        let edited = latest_body
                            .is_some_and(|latest| latest != *body && !latest.trim().is_empty());
                        if edited {
                            requeue_latest(sync, PostState::EarlierVersionPosted);
                        }
                        !edited
                    })
                    .map(|(local_id, sync, _)| (*local_id, sync.clone()))
                    .collect(),
            };
            for (_, sync) in &posted {
                sync.lock().pending = Some(PendingWrite::DeleteRow);
            }
            for (local_id, sync, _) in &sent {
                flush_sync(&this, *local_id, sync, &database, cx).await;
            }

            let mut problem = match database.set_last_review(key, head_sha).await {
                Ok(()) => None,
                Err(error) => {
                    log::error!("recording the last review failed: {error:#}");
                    Some(format!("{error:#}"))
                }
            };
            if !review_open {
                return Ok(());
            }
            match this.update_in(cx, |this, window, cx| this.refresh_detail(window, cx)) {
                Ok(refresh) => {
                    if let Err(error) = refresh.await {
                        problem.get_or_insert(format!("{error:#}"));
                    }
                }
                Err(_) => return Ok(()),
            }
            if let Some(problem) = problem {
                this.update(cx, |this, cx| {
                    this.submit_note =
                        Some(format!("Review posted. Couldn't refresh: {problem}").into());
                    cx.notify();
                })
                .ok();
            }
            Ok(())
        })
    }

    fn refresh_detail(&self, window: &mut Window, cx: &mut Context<Self>) -> Task<Result<()>> {
        let key = self.detail.summary.key.clone();
        let request = detail_request(&key, &self.login);
        let runner = gh_runner(cx);
        cx.spawn_in(window, async move |this, cx| {
            let detail = parse_detail(&key, &runner.run(request).await?)?;
            this.update_in(cx, |this, window, cx| {
                if let Some(commit) = &detail.summary.last_review_commit {
                    this.last_review = Some(commit.clone());
                }
                this.detail = detail;
                this.refresh_conversation(cx);
                this.refresh_auto_results(cx);
                this.add_reply_editors(&HashMap::new(), window, cx);
                this.show_comments(cx);
                cx.notify();
            })
        })
    }

    /// The Reply button: posts what is typed in the thread's reply box.
    pub fn click_reply(&mut self, thread_id: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(editor) = self.reply_editors.get(thread_id) else {
            return Task::ready(Err(anyhow!("No reply box for this thread.")));
        };
        let body = editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return Task::ready(Ok(()));
        }
        self.reply(thread_id, body, cx)
    }

    pub fn reply(
        &mut self,
        thread_id: &str,
        body: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(first_comment) = self
            .detail
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.comments.first())
        else {
            return Task::ready(Err(anyhow!("This thread has no comment to reply to.")));
        };
        let key = self.detail.summary.key.clone();
        let request = reply_request(&key, first_comment.database_id, &body);
        let runner = gh_runner(cx);
        let thread_id = thread_id.to_string();
        let slot = reply_slot(&thread_id);
        let reply_sync = self.text_sync(&slot);
        let database = self.database.clone();
        cx.spawn(async move |this, cx| {
            let result = match runner.run(request).await {
                Ok(output) => parse_reply(&output),
                Err(error) => Err(error.into()),
            };
            // The review may have closed meanwhile; the stored reply still goes once it's posted.
            if result.is_ok() {
                clear_sent_text(
                    &reply_sync,
                    &body,
                    &database,
                    &key,
                    &slot,
                    cx.background_executor(),
                );
            }
            this.update(cx, |this, cx| {
                match &result {
                    Ok(comment) => {
                        this.thread_errors.remove(&thread_id);
                        if let Some(thread) = this
                            .detail
                            .threads
                            .iter_mut()
                            .find(|thread| thread.id == thread_id)
                        {
                            thread.comments.push(comment.clone());
                        }
                        let buffer = this
                            .reply_editors
                            .get(&thread_id)
                            .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton());
                        if let Some(buffer) = buffer
                            && buffer.read(cx).text() == body
                        {
                            buffer.update(cx, |buffer, cx| buffer.set_text("", cx));
                        }
                    }
                    Err(error) => {
                        this.thread_errors
                            .insert(thread_id.clone(), format!("{error:#}").into());
                    }
                }
                this.show_comments(cx);
                cx.notify();
            })?;
            result.map(|_| ())
        })
    }

    /// The viewer wrote this pull request: the tab works through its feedback instead of
    /// reviewing it, since GitHub rejects approving or requesting changes on your own PR.
    pub fn is_own_pr(&self) -> bool {
        self.login.eq_ignore_ascii_case(&self.detail.summary.author)
    }

    /// Selects the review-map step holding the thread's file and scrolls to its line.
    pub fn show_thread(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self
            .detail
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .cloned()
        else {
            return;
        };
        if let Some(index) = self
            .steps
            .iter()
            .position(|step| step.files.contains(&thread.path))
        {
            self.select_step(index, window, cx);
        }
        let Some(line) = thread.line else {
            return;
        };
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let Some(anchor) = self.line_anchor(&thread.path, thread.side, line, &snapshot, cx) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_anchor_ranges([anchor..anchor]),
            );
        });
        window.focus(&self.editor.focus_handle(cx), cx);
    }

    /// Opens the thread's file from the review checkout in an editor, at the thread's line, so the
    /// author can make the change.
    pub fn edit_thread_file(
        &mut self,
        thread_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self
            .detail
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
        else {
            return;
        };
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        else {
            return;
        };
        // GitHub paths are repository-relative; anything else isn't opened.
        if Path::new(&thread.path)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return;
        }
        let path = self.checkout.worktree.join(&thread.path);
        let row = thread.line.unwrap_or(1).saturating_sub(1);
        let opening = workspace.update(cx, |workspace, cx| {
            workspace.open_abs_path(path, OpenOptions::default(), window, cx)
        });
        cx.spawn_in(window, async move |_, cx| {
            let item = opening.await?;
            if let Some(editor) = item.downcast::<Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    editor.go_to_singleton_buffer_point(Point::new(row, 0), window, cx)
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Replies "Done" to the thread and resolves it.
    pub fn mark_thread_done(
        &mut self,
        thread_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.resolving.contains(thread_id) {
            return Task::ready(Err(anyhow!("This thread is already being resolved.")));
        }
        if !self.finishing_threads.insert(thread_id.to_string()) {
            return Task::ready(Ok(()));
        }
        cx.notify();
        // A retry after a failed resolve doesn't post a second "Done".
        let already_replied = self
            .detail
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.comments.last())
            .is_some_and(|comment| {
                comment.body.trim() == "Done" && comment.author.eq_ignore_ascii_case(&self.login)
            });
        let reply = if already_replied {
            Task::ready(Ok(()))
        } else {
            self.reply(thread_id, "Done".to_string(), cx)
        };
        let thread_id = thread_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = async {
                reply.await?;
                this.update(cx, |this, cx| {
                    this.set_thread_resolved(&thread_id, true, cx)
                })?
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                this.finishing_threads.remove(&thread_id);
                if let Err(error) = &result {
                    this.thread_errors
                        .insert(thread_id.clone(), format!("{error:#}").into());
                    this.show_comments(cx);
                }
                cx.notify();
            })?;
            result
        })
    }

    pub fn push_command(&self) -> Option<String> {
        let head_repository = self.detail.head_repository.as_ref()?;
        (!self.detail.head_ref.is_empty()).then(|| {
            push_command(
                &self.detail.summary.key.host,
                head_repository,
                &self.detail.head_ref,
            )
        })
    }

    /// Opens a terminal in the review checkout with the commit-and-push command typed in, not run.
    pub fn open_push_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(command), Some(workspace)) = (
            self.push_command(),
            self.workspace
                .as_ref()
                .and_then(|workspace| workspace.upgrade()),
        ) else {
            return;
        };
        let cwd = self.checkout.worktree.clone();
        let creating = workspace.update(cx, |workspace, cx| {
            terminal_view::terminal_panel::TerminalPanel::add_center_terminal(
                workspace,
                window,
                cx,
                move |project, cx| project.create_terminal_shell(Some(cwd), cx),
            )
        });
        cx.spawn_in(window, async move |_, cx| {
            let terminal = creating.await?;
            workspace.update_in(cx, |workspace, window, cx| {
                let view = workspace
                    .items_of_type::<terminal_view::TerminalView>(cx)
                    .find(|view| view.read(cx).terminal().downgrade() == terminal);
                if let Some(view) = view {
                    view.update(cx, |view, cx| {
                        view.prefill_block_input(&command, window, cx)
                    });
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Asks the user's agent to review the pull request; its findings become draft comments the
    /// reviewer reads, edits or deletes before anything is posted.
    ///
    /// The agent works in the PR's checkout, which the PR's author controls, so it needs the
    /// checkout trusted first, and a PR that changes agent instruction or settings files asks
    /// again. Clicking while it runs cancels it.
    pub fn start_agent_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_reviewing {
            self.agent_reviewing = false;
            self._agent_review_task = Task::ready(());
            self.set_agent_note("Agent pre-review cancelled.".into(), Color::Muted, cx);
            return;
        }
        if review_agent(cx).is_none() || self.workspace.is_none() {
            return;
        }
        if self.worktree_restricted(cx) {
            self.set_agent_note(
                "The agent works in this PR's checkout: trust it first, and only if you trust the author."
                    .into(),
                Color::Warning,
                cx,
            );
            window.dispatch_action(ToggleWorktreeSecurity.boxed_clone(), cx);
            return;
        }
        let config_files: Vec<String> =
            agent_config_paths(self.all_files().iter().map(|file| file.diff.path.as_str()))
                .into_iter()
                .map(str::to_string)
                .collect();
        if config_files.is_empty() {
            self.run_agent_review(window, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "This PR changes files that steer coding agents",
            Some(&format!(
                "Your agent reads these from the checkout and may follow them:\n\n{}\n\nRead them before letting the agent work here.",
                config_files.join("\n")
            )),
            &["Start anyway", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                this.update_in(cx, |this, window, cx| this.run_agent_review(window, cx))?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn run_agent_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(agent), Some(workspace)) = (review_agent(cx), self.workspace.clone()) else {
            return;
        };
        let paths: Vec<String> = self
            .all_files()
            .iter()
            .map(|file| file.diff.path.clone())
            .collect();
        let thoroughness = PrReviewSettings::get_global(cx).thoroughness;
        let dismissals = self
            .database
            .dismissals(&self.detail.summary.key)
            .log_err()
            .unwrap_or_default();
        let prompt = review_prompt(&PromptInput {
            number: self.detail.summary.key.number,
            title: &self.detail.summary.title,
            description: &self.detail.body,
            steps: &self.steps,
            files: self.all_files().iter().map(|file| &file.diff).collect(),
            checklist: self
                .checklist_items
                .iter()
                .filter(|item| item.paths.is_none() || item.matches_any(&paths))
                .map(|item| item.text.as_str())
                .collect(),
            thoroughness,
            dismissals: &dismissals,
        });
        let work_dir = self.checkout.worktree.clone();
        let reply = agent.review(workspace.clone(), prompt, work_dir.clone(), window, cx);
        self.agent_reviewing = true;
        self.dropped_findings.clear();
        self.set_agent_note(
            "The agent is reviewing; follow it in the agent panel.".into(),
            Color::Muted,
            cx,
        );
        let number = self.detail.summary.key.number;
        let title = self.detail.summary.title.clone();
        self._agent_review_task = cx.spawn_in(window, async move |this, cx| {
            let findings = reply.await.and_then(|reply| parse_agent_comments(&reply));
            let findings: Vec<AgentComment> = match findings {
                Ok(findings) => findings
                    .into_iter()
                    .filter(|finding| finding.wanted_at(thoroughness))
                    .collect(),
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.agent_reviewing = false;
                        this.set_agent_note(format!("{error:#}").into(), Color::Error, cx);
                    })
                    .log_err();
                    return;
                }
            };
            if findings.is_empty() {
                this.update(cx, |this, cx| {
                    this.agent_reviewing = false;
                    this.set_agent_note(
                        "The agent found nothing to comment on.".into(),
                        Color::Muted,
                        cx,
                    );
                })
                .log_err();
                return;
            }
            let count = findings.len();
            let Some(check) = this
                .update_in(cx, |this, window, cx| {
                    this.set_agent_note(
                        format!(
                            "The agent is checking its {count} finding{} against the code.",
                            if count == 1 { "" } else { "s" }
                        )
                        .into(),
                        Color::Muted,
                        cx,
                    );
                    agent.review(
                        workspace,
                        verify_prompt(number, &title, &findings),
                        work_dir,
                        window,
                        cx,
                    )
                })
                .log_err()
            else {
                return;
            };
            let verdicts = check
                .await
                .and_then(|reply| parse_verdicts(&reply, findings.len()));
            this.update(cx, |this, cx| {
                this.add_checked_findings(findings, verdicts, cx)
            })
            .log_err();
        });
    }

    /// Kept findings become drafts; dropped ones are listed in the map. When the check itself
    /// failed, every finding is added and the note says they weren't checked.
    fn add_checked_findings(
        &mut self,
        findings: Vec<AgentComment>,
        verdicts: Result<Vec<Verdict>>,
        cx: &mut Context<Self>,
    ) {
        self.agent_reviewing = false;
        let total = findings.len();
        let plural = |count: usize| if count == 1 { "" } else { "s" };
        let (kept, note, color) = match verdicts {
            Ok(verdicts) => {
                let mut kept = Vec::new();
                for (finding, verdict) in findings.into_iter().zip(verdicts) {
                    if verdict.keep {
                        kept.push(finding);
                    } else {
                        self.dropped_findings.push((finding, verdict.reason));
                    }
                }
                let dropped = total - kept.len();
                let summary = if dropped == 0 {
                    format!(
                        "Kept all {total} finding{} after checking them against the code.",
                        plural(total)
                    )
                } else {
                    format!(
                        "Kept {} of {total} finding{}; dropped {dropped} it couldn't confirm.",
                        kept.len(),
                        plural(total)
                    )
                };
                let note = if kept.is_empty() {
                    summary
                } else {
                    format!("{summary} Read each draft before you submit.")
                };
                (kept, note, Color::Accent)
            }
            Err(error) => (
                findings,
                format!(
                    "Couldn't check the findings ({error:#}), so all {total} are added unchecked. \
                     Read each draft before you submit."
                ),
                Color::Warning,
            ),
        };
        let added = !kept.is_empty();
        for comment in kept {
            let draft = self.agent_draft(comment, cx);
            let local_id = self.push_draft(draft);
            self.queue_save(local_id, cx);
        }
        if added {
            self.show_comments(cx);
        }
        self.set_agent_note(note.into(), color, cx);
    }

    /// Deletes an agent draft and remembers why, so later pre-reviews of the repository don't
    /// repeat it.
    pub fn dismiss_agent_draft(
        &mut self,
        local_id: usize,
        reason: DismissReason,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        let draft = &self.drafts[index];
        let dismissal = Dismissal {
            path: draft.path.clone(),
            body: draft.body.clone(),
            reason,
        };
        let database = self.database.clone();
        let key = self.detail.summary.key.clone();
        cx.background_spawn(async move { database.add_dismissal(key, dismissal).await })
            .detach_and_log_err(cx);
        self.discard_draft(local_id, cx);
    }

    /// The findings the agent's check dropped, folded to one line.
    fn render_dropped_findings(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let count = self.dropped_findings.len();
        if count == 0 {
            return None;
        }
        let colors = cx.theme().colors();
        let section = v_flex().pb_2().gap_1().child(self.render_fold_line(
            "photon-review-dropped",
            self.dropped_open,
            format!(
                "Dropped {count} finding{} the agent couldn't confirm",
                if count == 1 { "" } else { "s" }
            ),
            |this| &mut this.dropped_open,
            cx,
        ));
        if !self.dropped_open {
            return Some(section.into_any_element());
        }
        Some(
            section
                .child(
                    v_flex()
                        .debug_selector(|| "photon-review-dropped-list".into())
                        .gap_1()
                        .children(self.dropped_findings.iter().map(|(finding, reason)| {
                            v_flex()
                                .gap_0p5()
                                .p_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(colors.border_variant)
                                .child(
                                    Label::new(match finding.line {
                                        Some(line) => format!("{}:{line}", finding.path),
                                        None => finding.path.clone(),
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                                )
                                .child(Label::new(finding.draft_text()).size(LabelSize::Small))
                                .when(!reason.is_empty(), |this| {
                                    this.child(
                                        Label::new(format!("Why dropped: {reason}"))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                })
                        })),
                )
                .into_any_element(),
        )
    }

    pub fn agent_review_note(&self) -> Option<&(SharedString, Color)> {
        self.agent_review_note.as_ref()
    }

    /// The agent's progress and result go to a toast, so nothing in the tab changes width while
    /// it runs. While it runs, the toast offers to cancel it.
    fn set_agent_note(&mut self, note: SharedString, color: Color, cx: &mut Context<Self>) {
        self.agent_review_note = Some((note.clone(), color));
        let mut toast = Toast::new(
            NotificationId::unique::<AgentReviewToast>(),
            note.to_string(),
        );
        if self.agent_reviewing {
            let item = cx.weak_entity();
            toast = toast.on_click("Cancel", move |window, cx| {
                item.update(cx, |item, cx| {
                    if item.agent_reviewing {
                        item.start_agent_review(window, cx);
                    }
                })
                .log_err();
            });
        } else if !matches!(color, Color::Error | Color::Warning) {
            toast = toast.autohide();
        }
        self.show_toast(toast, cx);
        cx.notify();
    }

    fn set_note(&mut self, note: SharedString, cx: &mut Context<Self>) {
        self.since_review_note = Some(note.clone());
        self.show_toast(
            Toast::new(NotificationId::unique::<NoteToast>(), note.to_string()),
            cx,
        );
        cx.notify();
    }

    fn show_toast(&self, toast: Toast, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) {
            workspace.update(cx, |workspace, cx| workspace.show_toast(toast, cx));
        }
    }

    fn dismiss_toast<T: 'static>(&self, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) {
            workspace.update(cx, |workspace, cx| {
                workspace.dismiss_toast(&NotificationId::unique::<T>(), cx)
            });
        }
    }

    fn run_locally(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let summary = self.detail.summary.clone();
        workspace.update(cx, |workspace, cx| {
            crate::run_locally(summary, workspace, window, cx).detach_and_log_err(cx)
        });
    }

    /// Asks first, then merges the head this review shows, so newer commits never go in unseen.
    pub fn merge(&mut self, method: MergeMethod, window: &mut Window, cx: &mut Context<Self>) {
        if self.merging {
            return;
        }
        let summary = &self.detail.summary;
        let number = summary.key.number;
        let message = format!("{} #{number} into {}?", method.verb(), summary.base_ref);
        let unsent = self
            .drafts
            .iter()
            .filter(|draft| !draft.body.trim().is_empty())
            .count();
        let detail = (unsent > 0).then(|| {
            format!(
                "Your {unsent} unsent comment{} won't be posted.",
                if unsent == 1 { "" } else { "s" }
            )
        });
        let answer = window.prompt(
            PromptLevel::Info,
            &message,
            detail.as_deref(),
            &["Merge", "Cancel"],
            cx,
        );
        let request = merge_request(
            &summary.key.host,
            &summary.node_id,
            method,
            &self.checkout.head_sha,
        );
        let runner = gh_runner(cx);
        // Detached: a merge under way finishes even if the review closes.
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return Ok(());
            }
            this.update(cx, |this, cx| {
                this.merging = true;
                this.merge_error = None;
                cx.notify();
            })?;
            let result = match runner.run(request).await {
                Ok(output) => parse_mutation(&output),
                Err(error) => Err(error.into()),
            };
            this.update_in(cx, |this, window, cx| {
                this.merging = false;
                match result {
                    Ok(()) => this.merged(window, cx),
                    Err(error) => {
                        this.merge_error = Some(merge_error_message(&format!("{error:#}")).into());
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn merged(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.detail.state = PrState::Merged;
        let key = self.detail.summary.key.clone();
        self.show_toast(
            Toast::new(
                NotificationId::unique::<NoteToast>(),
                format!("Merged #{}", key.number),
            )
            .autohide(),
            cx,
        );
        if let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) {
            workspace.update(cx, |workspace, cx| remove_from_inbox(workspace, &key, cx));
        }
        self.refresh_detail(window, cx).detach_and_log_err(cx);
    }

    /// After a submit from the popover: what came of it, as a toast.
    fn report_submitted(&mut self, cx: &mut Context<Self>) {
        let message = self
            .submit_note
            .clone()
            .unwrap_or_else(|| "Review posted.".into());
        self.show_toast(
            Toast::new(NotificationId::unique::<NoteToast>(), message.to_string()).autohide(),
            cx,
        );
    }

    /// A draft on the comment's line of the head, or a general draft naming the place when the
    /// line isn't in the review.
    fn agent_draft(&self, comment: AgentComment, cx: &App) -> Draft {
        let deleted = self
            .all_files()
            .iter()
            .any(|file| file.diff.path == comment.path && file.diff.status == FileStatus::Deleted);
        let line_text = comment.line.filter(|_| !deleted).and_then(|line| {
            let buffer = self.buffer_for_path(&comment.path, cx)?.read(cx).snapshot();
            let row = line.checked_sub(1)?;
            (row <= buffer.max_point().row).then(|| {
                buffer
                    .text_for_range(Point::new(row, 0)..Point::new(row, buffer.line_len(row)))
                    .collect::<String>()
            })
        });
        let (line, anchor_text, general, body) = match (comment.line, line_text) {
            (Some(line), Some(text)) => (line, text, false, comment.draft_text()),
            (line, _) => {
                let place = match line {
                    Some(line) => format!("{}:{line}", comment.path),
                    None => comment.path.clone(),
                };
                (
                    1,
                    String::new(),
                    true,
                    format!("`{place}`: {}", comment.draft_text()),
                )
            }
        };
        Draft {
            id: None,
            key: self.detail.summary.key.clone(),
            path: comment.path,
            side: Side::Right,
            start_line: line,
            end_line: line,
            commit: self.checkout.head_sha.clone(),
            anchor_text,
            body,
            outdated: false,
            general,
            post_state: PostState::Unposted,
            from_agent: true,
        }
    }

    /// Adds a suggestion block holding the draft's lines, ready to edit into the change.
    pub fn suggest_change(&mut self, local_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        let draft = &self.drafts[index];
        if draft.side != Side::Right || draft.general {
            return;
        }
        let block = suggestion_block(&draft.anchor_text);
        let Some(composer) = self.draft_views[index].composer.clone() else {
            return;
        };
        composer.update(cx, |editor, cx| {
            let body = editor.text(cx);
            let text = if body.trim().is_empty() {
                block
            } else {
                format!("{}\n\n{block}", body.trim_end())
            };
            editor.set_text(text, window, cx);
            editor.move_to_end(&Default::default(), window, cx);
        });
    }

    /// Whether a comment's suggestion can be committed from here: the thread sits on the head's
    /// lines and Photon knows the branch to commit to.
    fn suggestion_committable(&self, thread: &ReviewThread) -> bool {
        !thread.is_outdated
            && thread.side == Side::Right
            && thread.line.is_some()
            && self.detail.head_repository.is_some()
            && !self.detail.head_ref.is_empty()
    }

    /// Commits the suggestion in a thread's comment to the pull request's branch, as
    /// github.com's "Commit suggestion" does. Nothing in the local checkout changes.
    pub fn commit_suggestion(
        &mut self,
        thread_id: &str,
        comment_index: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.committing_suggestions.contains(thread_id) {
            return Task::ready(Ok(()));
        }
        let Some(thread) = self
            .detail
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
        else {
            return Task::ready(Err(anyhow!("Unknown review thread {thread_id}.")));
        };
        let (Some(comment), Some(end), Some(head_repository)) = (
            thread.comments.get(comment_index),
            thread.line,
            self.detail.head_repository.clone(),
        ) else {
            return Task::ready(Err(anyhow!("This suggestion can't be committed.")));
        };
        let Some(replacement) = parse_suggestion(&comment.body) else {
            return Task::ready(Err(anyhow!("This comment has no single suggestion.")));
        };
        if !self.suggestion_committable(thread) {
            return Task::ready(Err(anyhow!("This suggestion can't be committed.")));
        }
        let start = thread.start_line.unwrap_or(end);
        let path = thread.path.clone();
        let branch = self.detail.head_ref.clone();
        let commit = self.checkout.head_sha.clone();
        let message = format!("Apply suggestion from @{} in code review", comment.author);
        let key = self.detail.summary.key.clone();
        let runner = gh_runner(cx);
        let thread_id = thread_id.to_string();
        self.committing_suggestions.insert(thread_id.clone());
        self.suggestion_notes.remove(&thread_id);
        self.thread_errors.remove(&thread_id);
        self.show_comments(cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = async {
                let contents = runner
                    .run(file_at_commit_request(
                        &key,
                        &head_repository,
                        &path,
                        &commit,
                    ))
                    .await?;
                let (text, blob_sha) = parse_file_contents(&contents)?;
                let updated = apply_suggestion(&text, start, end, &replacement)?;
                runner
                    .run(commit_file_request(
                        &key,
                        &head_repository,
                        &branch,
                        &path,
                        &updated,
                        &blob_sha,
                        &message,
                    ))
                    .await?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.committing_suggestions.remove(&thread_id);
                match &result {
                    Ok(()) => {
                        this.suggestion_notes.insert(
                            thread_id.clone(),
                            format!("Committed to {branch}. Reopen the review to see it.").into(),
                        );
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        // GitHub's 409: the branch's file isn't the one this review read.
                        let message =
                            if message.contains("409") || message.contains("does not match") {
                                "The file changed on the branch since this review opened. \
                                 Reopen the review and try again."
                                    .to_string()
                            } else {
                                message
                            };
                        this.thread_errors.insert(thread_id.clone(), message.into());
                    }
                }
                this.show_comments(cx);
                cx.notify();
            })?;
            result
        })
    }

    /// Flips the thread at once, as GitHub does, and flips it back if `gh` fails. A click while
    /// the thread's last request is pending sends nothing.
    pub fn set_thread_resolved(
        &mut self,
        thread_id: &str,
        resolved: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.resolving.contains(thread_id) {
            return Task::ready(Ok(()));
        }
        let Some(thread) = self
            .detail
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        else {
            return Task::ready(Err(anyhow!("Unknown review thread {thread_id}.")));
        };
        thread.is_resolved = resolved;
        self.resolving.insert(thread_id.to_string());
        self.thread_errors.remove(thread_id);
        self.show_comments(cx);
        cx.notify();
        let request = resolve_request(&self.detail.summary.key.host, thread_id, resolved);
        let runner = gh_runner(cx);
        let thread_id = thread_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = match runner.run(request).await {
                Ok(output) => parse_mutation(&output),
                Err(error) => Err(error.into()),
            };
            this.update(cx, |this, cx| {
                this.resolving.remove(&thread_id);
                if let Err(error) = &result {
                    // A refresh may have brought GitHub's own state in the meantime.
                    if let Some(thread) = this
                        .detail
                        .threads
                        .iter_mut()
                        .find(|thread| thread.id == thread_id && thread.is_resolved == resolved)
                    {
                        thread.is_resolved = !resolved;
                    }
                    this.thread_errors
                        .insert(thread_id.clone(), format!("{error:#}").into());
                }
                this.show_comments(cx);
                cx.notify();
            })?;
            result
        })
    }

    /// Syncs GitHub's Viewed checkbox for a file: flips at once and flips back if `gh` fails. A
    /// click while the file's last request is pending sends nothing.
    pub fn set_file_viewed(
        &mut self,
        path: &str,
        viewed: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.full_files.is_some() || self.marking_viewed.contains(path) {
            return Task::ready(Ok(()));
        }
        self.set_local_file_viewed(path, viewed);
        self.marking_viewed.insert(path.to_string());
        self.file_errors.remove(path);
        cx.notify();
        let request = viewed_request(
            &self.detail.summary.key.host,
            &self.detail.summary.node_id,
            path,
            viewed,
        );
        let runner = gh_runner(cx);
        let path = path.to_string();
        cx.spawn(async move |this, cx| {
            let result = match runner.run(request).await {
                Ok(output) => parse_mutation(&output),
                Err(error) => Err(error.into()),
            };
            this.update(cx, |this, cx| {
                this.marking_viewed.remove(&path);
                match &result {
                    Ok(()) => this.record_viewed_head(&path, viewed, cx),
                    Err(error) => {
                        if this.file_viewed(&path) == viewed {
                            this.set_local_file_viewed(&path, !viewed);
                        }
                        this.file_errors
                            .insert(path.clone(), format!("{error:#}").into());
                    }
                }
                cx.notify();
            })?;
            result
        })
    }

    fn set_local_file_viewed(&mut self, path: &str, viewed: bool) {
        match self.detail.files.iter_mut().find(|file| file.path == path) {
            Some(file) => {
                file.viewed = viewed;
                file.changed_since_viewed = false;
            }
            None => self.detail.files.push(crate::PrFile {
                path: path.to_string(),
                viewed,
                changed_since_viewed: false,
            }),
        }
    }

    /// Remembers the head a file was marked viewed at, to show what changed since.
    fn record_viewed_head(&mut self, path: &str, viewed: bool, cx: &mut Context<Self>) {
        let head = viewed.then(|| self.checkout.head_sha.clone());
        match &head {
            Some(head) => self.viewed_heads.insert(path.to_string(), head.clone()),
            None => self.viewed_heads.remove(path),
        };
        let database = self.database.clone();
        let key = self.detail.summary.key.clone();
        let path = path.to_string();
        cx.background_spawn(async move { database.set_file_viewed_head(key, path, head).await })
            .detach_and_log_err(cx);
    }

    fn file_viewed(&self, path: &str) -> bool {
        self.detail
            .files
            .iter()
            .any(|file| file.path == path && file.viewed)
    }

    fn step_viewed(&self, step: &Step) -> bool {
        self.step_state(step) == ViewedState::Viewed
    }

    /// A step is viewed when ticked here, or when GitHub has all its files viewed. A tick from an
    /// older head counts as changed once the step's diff differs or GitHub says a file changed.
    fn step_state(&self, step: &Step) -> ViewedState {
        let file = |path: &String| self.detail.files.iter().find(|file| file.path == *path);
        if let Some(mark) = self.viewed_steps.get(&step.key) {
            let ticked_at_head = mark.head.as_deref() == Some(self.checkout.head_sha.as_str());
            let changed = !ticked_at_head
                && (self.stale_steps.contains(&step.key)
                    || step
                        .files
                        .iter()
                        .any(|path| file(path).is_some_and(|file| file.changed_since_viewed)));
            return if changed {
                ViewedState::ChangedSinceViewed
            } else {
                ViewedState::Viewed
            };
        }
        if step.files.is_empty() {
            return ViewedState::NotViewed;
        }
        if step
            .files
            .iter()
            .all(|path| file(path).is_some_and(|file| file.viewed))
        {
            ViewedState::Viewed
        } else if step
            .files
            .iter()
            .all(|path| file(path).is_some_and(|file| file.viewed || file.changed_since_viewed))
        {
            ViewedState::ChangedSinceViewed
        } else {
            ViewedState::NotViewed
        }
    }

    fn step_fingerprint(&self, step: &Step) -> String {
        let files = self.all_files();
        diff_fingerprint(step.files.iter().filter_map(|path| {
            files
                .iter()
                .find(|file| file.diff.path == *path)
                .map(|file| &file.diff)
        }))
    }

    /// Ticks a step. A file is marked viewed on GitHub once every step showing it is ticked;
    /// unticking a step unmarks its files.
    pub fn toggle_step_viewed(&mut self, index: usize, viewed: bool, cx: &mut Context<Self>) {
        let Some(step) = self.steps.get(index).cloned() else {
            return;
        };
        self.set_step_viewed(&step.key, viewed, cx);
        for path in &step.files {
            let file_done = self
                .steps
                .iter()
                .filter(|step| step.files.contains(path))
                .all(|step| self.step_viewed(step));
            let sync = if viewed { file_done } else { true };
            if sync && self.file_viewed(path) != viewed {
                self.set_file_viewed(path, viewed, cx)
                    .detach_and_log_err(cx);
            }
        }
    }

    /// Steps exist only in Photon, so their viewed state stays local.
    pub fn set_step_viewed(&mut self, step_key: &str, viewed: bool, cx: &mut Context<Self>) {
        if self.full_files.is_some() {
            return;
        }
        self.stale_steps.remove(step_key);
        let mark = viewed.then(|| ViewedMark {
            head: Some(self.checkout.head_sha.clone()),
            fingerprint: self
                .steps
                .iter()
                .find(|step| step.key == step_key)
                .map(|step| self.step_fingerprint(step)),
        });
        match &mark {
            Some(mark) => self.viewed_steps.insert(step_key.to_string(), mark.clone()),
            None => self.viewed_steps.remove(step_key),
        };
        cx.notify();
        let database = self.database.clone();
        let key = self.detail.summary.key.clone();
        let step_key = step_key.to_string();
        cx.background_spawn(async move { database.set_step_viewed(key, step_key, mark).await })
            .detach_and_log_err(cx);
    }

    fn draft_index(&self, local_id: usize) -> Option<usize> {
        self.draft_views
            .iter()
            .position(|view| view.local_id == local_id)
    }

    fn draft_note(&self, draft: &Draft) -> Option<String> {
        landing_note(&place(draft, &self.commentable))
    }

    fn push_draft(&mut self, draft: Draft) -> usize {
        let local_id = self.next_draft_id;
        self.next_draft_id += 1;
        let sync = Arc::new(Mutex::new(DraftSync {
            id: draft.id,
            latest: Some(draft.clone()),
            ..DraftSync::default()
        }));
        self.drafts.push(draft);
        self.draft_views.push(DraftView {
            local_id,
            composer: None,
            sync,
            _composer_subscription: None,
        });
        local_id
    }

    /// Shows the draft as an editable composer holding its current body.
    fn open_composer(&mut self, local_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        if self.draft_views[index].composer.is_some() {
            return;
        }
        let body = self.drafts[index].body.clone();
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, COMPOSER_MAX_LINES as usize, window, cx);
            editor.set_placeholder_text("Leave a comment", window, cx);
            editor.set_text(body, window, cx);
            editor
        });
        let subscription = cx.subscribe(&composer, move |this, editor, event: &EditorEvent, cx| {
            if !matches!(event, EditorEvent::BufferEdited) {
                return;
            }
            let body = editor.read(cx).text(cx);
            if let Some(index) = this.draft_index(local_id) {
                this.drafts[index].body = body;
                this.queue_save(local_id, cx);
                // The draft count and Comment's enabled state depend on the body.
                cx.notify();
            }
        });
        let view = &mut self.draft_views[index];
        view.composer = Some(composer.clone());
        view._composer_subscription = Some(subscription);
        self.show_comments(cx);
        window.focus(&composer.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn edit_draft(&mut self, local_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(local_id, window, cx);
    }

    fn queue_save(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        let draft = &self.drafts[index];
        let write = if draft.body.trim().is_empty() {
            PendingWrite::DeleteRow
        } else {
            PendingWrite::Save(draft.clone())
        };
        let sync = self.draft_views[index].sync.clone();
        {
            let mut state = sync.lock();
            state.pending = Some(write);
            state.latest = Some(draft.clone());
        }
        self.start_sync(local_id, sync, cx);
    }

    fn start_sync(&self, local_id: usize, sync: Arc<Mutex<DraftSync>>, cx: &mut Context<Self>) {
        if !claim_sync(&sync) {
            return;
        }
        let database = self.database.clone();
        // Detached so closing the review never cancels the write of what the user typed.
        cx.spawn(async move |this, cx| drain_sync(this, local_id, sync, database, cx).await)
            .detach();
    }

    fn discard_draft(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        self.drafts.remove(index);
        let view = self.draft_views.remove(index);
        {
            let mut state = view.sync.lock();
            state.pending = Some(PendingWrite::DeleteRow);
            state.latest = None;
        }
        self.start_sync(local_id, view.sync, cx);
        self.show_comments(cx);
        cx.notify();
    }

    pub fn cancel_draft(&mut self, local_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        if self.drafts[index].body.trim().is_empty() {
            self.discard_draft(local_id, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Discard this draft?",
            None,
            &["Discard", "Keep"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() == Some(0) {
                this.update(cx, |this, cx| this.discard_draft(local_id, cx))
                    .log_err();
            }
        })
        .detach();
    }

    fn collapse_draft(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        if self.drafts[index].body.trim().is_empty() {
            return;
        }
        // Saving again here retries a write that failed while typing.
        self.queue_save(local_id, cx);
        let view = &mut self.draft_views[index];
        view.composer = None;
        view._composer_subscription = None;
        self.show_comments(cx);
        cx.notify();
    }

    /// The newest selection's lines in the buffer under it: the head buffer of any excerpt
    /// (`Side::Right`), or the base text of deleted rows (`Side::Left`).
    fn selected_lines(&self, cx: &App) -> Option<SelectedLines> {
        let selection = *self.editor.read(cx).selections.newest_anchor();
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let start = selection.start.to_point(&snapshot);
        let mut end = selection.end.to_point(&snapshot);
        // A selection ending at the start of a line doesn't include that line.
        if end.row > start.row && end.column == 0 {
            end = Point::new(end.row - 1, 0);
        }
        let (buffer, start_point) = snapshot.point_to_buffer_point(Point::new(start.row, 0))?;
        let end_row = snapshot
            .point_to_buffer_point(Point::new(end.row, 0))
            .filter(|(end_buffer, _)| end_buffer.remote_id() == buffer.remote_id())
            .map_or(start_point.row, |(_, point)| point.row.max(start_point.row));
        let anchor_text = (start_point.row..=end_row)
            .map(|row| {
                buffer
                    .text_for_range(Point::new(row, 0)..Point::new(row, buffer.line_len(row)))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let buffer_id = buffer.remote_id();
        let base_file = self.files.iter().find(|file| {
            file.contents.as_ref().is_some_and(|(_, diff)| {
                diff.read(cx).base_text_buffer().read(cx).remote_id() == buffer_id
            })
        });
        let (path, side) = match base_file {
            Some(file) => (file.diff.path.clone(), Side::Left),
            None => {
                let buffer = self.multibuffer.read(cx).buffer(buffer_id)?;
                let path = buffer_path(&buffer, cx).or_else(|| {
                    self.files
                        .iter()
                        .find(|file| file.contents.as_ref().is_some_and(|(b, _)| *b == buffer))
                        .map(|file| file.diff.path.clone())
                })?;
                (path, Side::Right)
            }
        };
        Some(SelectedLines {
            path,
            side,
            start_line: start_point.row + 1,
            end_line: end_row + 1,
            anchor_text,
        })
    }

    fn add_comment(&mut self, _: &AddComment, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lines) = self.selected_lines(cx) else {
            return;
        };
        // The base text here is the reviewed commit's, so its line numbers are not GitHub's.
        if self.full_files.is_some() && lines.side == Side::Left {
            self.set_note(LEFT_COMMENT_NOTE.into(), cx);
            return;
        }
        // The simplified base's line numbers are not the old file's.
        if self.diff_simplified() && lines.side == Side::Left {
            self.set_note(SIMPLIFIED_LEFT_COMMENT_NOTE.into(), cx);
            return;
        }
        let draft = Draft {
            id: None,
            key: self.detail.summary.key.clone(),
            path: lines.path,
            side: lines.side,
            start_line: lines.start_line,
            end_line: lines.end_line,
            commit: self.checkout.head_sha.clone(),
            anchor_text: lines.anchor_text,
            body: String::new(),
            outdated: false,
            general: false,
            post_state: crate::PostState::Unposted,
            from_agent: false,
        };
        let local_id = self.push_draft(draft);
        self.open_composer(local_id, window, cx);
    }

    /// Moves a draft to the current selection, which also makes an outdated draft current again.
    pub fn replace_draft(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let (Some(lines), Some(index)) = (self.selected_lines(cx), self.draft_index(local_id))
        else {
            return;
        };
        let draft = &mut self.drafts[index];
        draft.path = lines.path;
        draft.side = lines.side;
        draft.start_line = lines.start_line;
        draft.end_line = lines.end_line;
        draft.anchor_text = lines.anchor_text;
        draft.commit = self.checkout.head_sha.clone();
        draft.outdated = false;
        draft.general = false;
        self.queue_save(local_id, cx);
        self.show_comments(cx);
        cx.notify();
    }

    fn post_as_general(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        self.drafts[index].general = true;
        self.queue_save(local_id, cx);
        cx.notify();
    }

    /// Drafts written on an older head move to the same text at this head, or become outdated.
    fn reanchor_drafts(&self, cx: &mut Context<Self>) -> Task<()> {
        let head = &self.checkout.head_sha;
        let fs = self.project.read(cx).fs().clone();
        let mut jobs = Vec::new();
        for (draft, view) in self.drafts.iter().zip(&self.draft_views) {
            if draft.outdated || draft.commit == *head {
                continue;
            }
            let file = self.files.iter().find(|file| file.diff.path == draft.path);
            let contents = file.and_then(|file| file.contents.as_ref());
            // `Ok(None)` means the file is gone; an error leaves the draft for the next open.
            let text: Task<Result<Option<String>>> = match (draft.side, contents) {
                (Side::Left, contents) => Task::ready(Ok(
                    contents.and_then(|(_, diff)| diff.read(cx).base_text_string(cx))
                )),
                (Side::Right, Some((buffer, _))) => Task::ready(Ok(Some(buffer.read(cx).text()))),
                (Side::Right, None) if file.is_some() => Task::ready(Ok(None)),
                (Side::Right, None) => {
                    let path = self.checkout.worktree.join(&draft.path);
                    let fs = fs.clone();
                    cx.background_spawn(async move {
                        match fs.metadata(&path).await? {
                            None => Ok(None),
                            Some(_) => fs.load(&path).await.map(Some),
                        }
                    })
                }
            };
            jobs.push((
                view.local_id,
                draft.path.clone(),
                draft.anchor_text.clone(),
                draft.start_line,
                text,
            ));
        }
        cx.spawn(async move |this, cx| {
            for (local_id, path, anchor_text, start_line, text) in jobs {
                let text = match text.await {
                    Ok(text) => text,
                    Err(error) => {
                        log::warn!("can't read {path} to re-anchor a review draft: {error:#}");
                        continue;
                    }
                };
                let result = reanchor(&anchor_text, start_line, text.as_deref());
                this.update(cx, |this, cx| this.apply_reanchor(local_id, result, cx))
                    .log_err();
            }
        })
    }

    fn apply_reanchor(&mut self, local_id: usize, result: Reanchor, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        let result = match result {
            Reanchor::Placed {
                start_line,
                end_line,
            } if self.drafts[index].side == Side::Left
                && !self.on_deleted_rows(&self.drafts[index].path, start_line, end_line) =>
            {
                Reanchor::Outdated
            }
            result => result,
        };
        let draft = &mut self.drafts[index];
        match result {
            Reanchor::Placed {
                start_line,
                end_line,
            } => {
                draft.start_line = start_line;
                draft.end_line = end_line;
                draft.commit = self.checkout.head_sha.clone();
            }
            Reanchor::Outdated => draft.outdated = true,
        }
        self.queue_save(local_id, cx);
        self.show_comments(cx);
        cx.notify();
    }

    fn on_deleted_rows(&self, path: &str, start_line: u32, end_line: u32) -> bool {
        self.files
            .iter()
            .find(|file| file.diff.path == path)
            .is_some_and(|file| {
                deleted_lines(&file.diff)
                    .iter()
                    .any(|rows| rows.contains(&start_line) && rows.contains(&end_line))
            })
    }

    /// The multibuffer position at the end of a 1-based line, when that line is shown.
    fn line_anchor(
        &self,
        path: &str,
        side: Side,
        line: u32,
        snapshot: &MultiBufferSnapshot,
        cx: &App,
    ) -> Option<Anchor> {
        let row = line.checked_sub(1)?;
        match side {
            Side::Right => {
                let buffer = self.buffer_for_path(path, cx)?.read(cx).snapshot();
                if row > buffer.max_point().row {
                    return None;
                }
                snapshot
                    .anchor_in_excerpt(buffer.anchor_before(Point::new(row, buffer.line_len(row))))
            }
            Side::Left => {
                if self.diff_simplified() {
                    return None;
                }
                let file = self.files.iter().find(|file| file.diff.path == path)?;
                let (buffer, diff) = file.contents.as_ref()?;
                let buffer = buffer.read(cx).snapshot();
                let diff = diff.read(cx).snapshot(cx);
                let base = diff.base_text();
                if row > base.max_point().row {
                    return None;
                }
                let base_point = Point::new(row, base.line_len(row));
                let point = diff.base_text_point_to_buffer_point(base_point, &buffer);
                let anchor = snapshot.anchor_in_excerpt(buffer.anchor_before(point))?;
                Some(anchor.with_diff_base_anchor(base.anchor_before(base_point)))
            }
        }
    }

    fn show_comments(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let item = cx.weak_entity();
        let mut blocks = Vec::new();
        for (draft, view) in self.drafts.iter().zip(&self.draft_views) {
            if draft.outdated || draft.general {
                continue;
            }
            let Some(anchor) =
                self.line_anchor(&draft.path, draft.side, draft.end_line, &snapshot, cx)
            else {
                continue;
            };
            let note = self.draft_note(draft);
            let posted_note = self.posted_note(draft);
            let body_rows = draft.body.lines().count().max(1) as u32;
            let height = match view.composer {
                Some(_) => COMPOSER_MAX_LINES + 4,
                None => 2 + body_rows,
            } + if note.is_some() { 2 } else { 0 }
                + if posted_note.is_some() { 2 } else { 0 };
            blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(height),
                style: BlockStyle::Sticky,
                render: draft_renderer(
                    item.clone(),
                    view.local_id,
                    draft.end_line,
                    draft.body.clone().into(),
                    note.map(SharedString::from),
                    posted_note,
                    view.composer.clone(),
                    view.sync.clone(),
                    draft.side == Side::Right && !draft.general,
                    draft.from_agent,
                ),
                priority: 0,
            });
        }
        for thread in &self.detail.threads {
            let Some(line) = thread.line else {
                continue;
            };
            let Some(anchor) = self.line_anchor(&thread.path, thread.side, line, &snapshot, cx)
            else {
                continue;
            };
            let body_rows: usize = thread
                .comments
                .iter()
                .map(|comment| 1 + comment.body.lines().count().max(1))
                .sum();
            let error = self.thread_errors.get(&thread.id).cloned();
            let pending = self.resolving.contains(&thread.id);
            let suggestion = SuggestionState {
                committable: self.suggestion_committable(thread),
                committing: self.committing_suggestions.contains(&thread.id),
                note: self.suggestion_notes.get(&thread.id).cloned(),
            };
            let suggestion_rows = if suggestion.committable {
                thread
                    .comments
                    .iter()
                    .filter(|comment| parse_suggestion(&comment.body).is_some())
                    .count()
                    * 2
            } else {
                0
            };
            let extra_rows = suggestion_rows as u32 + u32::from(suggestion.note.is_some());
            blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(body_rows as u32 + 3 + u32::from(error.is_some()) + extra_rows),
                style: BlockStyle::Sticky,
                render: thread_renderer(
                    item.clone(),
                    thread.clone(),
                    self.reply_editors.get(&thread.id).cloned(),
                    error,
                    pending,
                    suggestion,
                ),
                priority: 0,
            });
        }
        let stale = std::mem::take(&mut self.comment_blocks);
        self.comment_blocks = self.editor.update(cx, |editor, cx| {
            editor.remove_blocks(stale.into_iter().collect(), None, cx);
            editor.insert_blocks(blocks, None, cx).into_iter().collect()
        });
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Notes on the selected step's files, such as one read from git instead of the checkout.
    fn step_notes(&self) -> Vec<SharedString> {
        let Some(step) = self.steps.get(self.selected) else {
            return Vec::new();
        };
        self.files
            .iter()
            .filter(|file| step.files.contains(&file.diff.path))
            .filter_map(|file| file.note.clone())
            .collect()
    }

    pub fn selected_step(&self) -> usize {
        self.selected
    }

    pub fn new_commits(&self) -> Option<&NewCommits> {
        self.new_commits.as_ref()
    }

    fn set_new_commits(&mut self, new_commits: Option<NewCommits>, cx: &mut Context<Self>) {
        self.new_commits = new_commits;
        cx.notify();
    }

    fn retry_new_commits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(NewCommits::Failed { summary, .. }) = self.new_commits.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            open_review(summary, workspace, window, cx).detach_and_log_err(cx)
        });
    }

    fn render_new_commits(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let new_commits = self.new_commits.as_ref()?;
        let row = h_flex()
            .debug_selector(|| "photon-review-new-commits".into())
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border);
        Some(
            match new_commits {
                NewCommits::Loading => row.child(
                    Label::new("Loading new commits…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
                NewCommits::Failed { message, .. } => row
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(message.clone())
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "photon-review-new-commits-retry".into())
                            .child(
                                Button::new("photon-review-new-commits-retry", "Try again")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.retry_new_commits(window, cx)
                                    })),
                            ),
                    ),
            }
            .into_any_element(),
        )
    }

    pub fn select_step(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.steps.len() || (index == self.selected && !self.conversation_open) {
            return;
        }
        self.conversation_open = false;
        self.selected = index;
        self.show_step(cx);
        cx.notify();
    }

    pub fn open_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.conversation_open = true;
        if self.conversation.is_none() {
            let description =
                cx.new(|cx| Markdown::new(self.detail.body.clone().into(), None, None, cx));
            let composer = cx.new(|cx| {
                let mut editor = Editor::auto_height(3, 12, window, cx);
                editor.set_placeholder_text("Leave a comment", window, cx);
                editor
            });
            // Comment is enabled by text, so the composer's row redraws as it is typed.
            let subscription = cx.subscribe(&composer, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            });
            self.conversation = Some(Conversation {
                description,
                events: None,
                fetching: false,
                error: None,
                composer,
                posting: false,
                post_error: None,
                _fetch_task: Task::ready(()),
                _composer_subscription: subscription,
            });
        }
        if self
            .conversation
            .as_ref()
            .is_some_and(|conversation| conversation.events.is_none())
        {
            self.fetch_conversation(cx);
        }
        cx.notify();
    }

    fn fetch_conversation(&mut self, cx: &mut Context<Self>) {
        let request = timeline_request(&self.detail.summary.key);
        let runner = gh_runner(cx);
        let Some(conversation) = self.conversation.as_mut() else {
            return;
        };
        conversation.fetching = true;
        conversation._fetch_task = cx.spawn(async move |this, cx| {
            let result = match runner.run(request).await {
                Ok(output) => parse_timeline(&output),
                Err(error) => Err(error.into()),
            };
            this.update(cx, |this, cx| {
                let Some(conversation) = this.conversation.as_mut() else {
                    return;
                };
                conversation.fetching = false;
                match result {
                    Ok(events) => {
                        conversation.error = None;
                        conversation.events = Some(
                            events
                                .into_iter()
                                .map(|event| timeline_entry(event, cx))
                                .collect(),
                        );
                    }
                    Err(error) => conversation.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        });
    }

    /// After the detail refreshes: a showing timeline is fetched again, a hidden one when next
    /// opened.
    fn refresh_conversation(&mut self, cx: &mut Context<Self>) {
        let body = self.detail.body.clone();
        let open = self.conversation_open;
        let Some(conversation) = self.conversation.as_mut() else {
            return;
        };
        conversation
            .description
            .update(cx, |description, cx| description.reset(body.into(), cx));
        conversation.events = None;
        if open {
            self.fetch_conversation(cx);
        }
    }

    /// The Comment button: posts the composer's text as a general comment.
    pub fn post_comment(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(conversation) = self.conversation.as_mut() else {
            return Task::ready(Ok(()));
        };
        let body = conversation.composer.read(cx).text(cx);
        if body.trim().is_empty() || conversation.posting {
            return Task::ready(Ok(()));
        }
        conversation.posting = true;
        conversation.post_error = None;
        cx.notify();
        let request = issue_comment_request(&self.detail.summary.key, &body);
        let runner = gh_runner(cx);
        cx.spawn(async move |this, cx| {
            let result = match runner.run(request).await {
                Ok(output) => parse_reply(&output),
                Err(error) => Err(error.into()),
            };
            this.update(cx, |this, cx| {
                let Some(conversation) = this.conversation.as_mut() else {
                    return;
                };
                conversation.posting = false;
                match &result {
                    Ok(comment) => {
                        let entry = timeline_entry(TimelineEvent::Comment(comment.clone()), cx);
                        if let Some(events) = conversation.events.as_mut() {
                            events.push(entry);
                        }
                        // Text typed while posting stays for the next comment.
                        let buffer = conversation
                            .composer
                            .read(cx)
                            .buffer()
                            .read(cx)
                            .as_singleton();
                        if let Some(buffer) = buffer
                            && buffer.read(cx).text() == body
                        {
                            buffer.update(cx, |buffer, cx| buffer.set_text("", cx));
                        }
                        this.detail.comment_count += 1;
                    }
                    Err(error) => conversation.post_error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })?;
            result.map(|_| ())
        })
    }

    pub fn context_state(&self) -> &ContextState {
        &self.context_state
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn expand_context(&mut self, excerpt_index: usize, cx: &mut Context<Self>) {
        let Some(excerpt) = self.shown_context.get(excerpt_index) else {
            return;
        };
        self.expanded_context
            .insert((excerpt.buffer.entity_id(), excerpt.range.start));
        self.show_step(cx);
        cx.notify();
    }

    fn load_context(&mut self, cx: &mut Context<Self>) -> Task<()> {
        self.context_state = ContextState::Finding;
        cx.notify();
        let analyses: Vec<_> = self
            .files
            .iter()
            .map(|file| analyse_loaded_file(file, cx))
            .collect();
        cx.spawn(async move |this, cx| {
            let analyses = join_all(analyses).await;
            let Some(fetches) = this
                .update(cx, |this, cx| {
                    this.set_analyses(analyses, cx);
                    // Language servers don't start in a restricted worktree, and a PR's code is
                    // never trusted automatically, so the fetches (and their 30 s cap) wait for
                    // the user to trust it.
                    if this.worktree_restricted(cx) {
                        this.context_state = ContextState::Unavailable(TRUST_REASON.into());
                        cx.notify();
                        return None;
                    }
                    Some(this.fetch_contexts(cx))
                })
                .log_err()
                .flatten()
            else {
                return;
            };
            let results = join_all(fetches).await;
            this.update(cx, |this, cx| this.set_contexts(results, cx))
                .log_err();
        })
    }

    fn worktree_restricted(&self, cx: &mut Context<Self>) -> bool {
        let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
            return false;
        };
        let worktree_store = self.project.read(cx).worktree_store();
        let worktree_ids: Vec<_> = self
            .project
            .read(cx)
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).id())
            .collect();
        trusted_worktrees.update(cx, |trusted_worktrees, cx| {
            worktree_ids
                .into_iter()
                .any(|id| !trusted_worktrees.can_trust(&worktree_store, id, cx))
        })
    }

    fn set_analyses(&mut self, analyses: Vec<Option<FileAnalysis>>, cx: &mut Context<Self>) {
        self.functions = analyses
            .iter()
            .flatten()
            .flat_map(|analysis| match analysis {
                FileAnalysis::Functions { functions, .. } => functions.clone(),
                FileAnalysis::Unanalysable => Vec::new(),
            })
            .collect();
        self.contexts = vec![None; self.functions.len()];
        self.callers = Vec::new();
        self.analyses = analyses;
        self.rebuild_map(&[], cx);
    }

    fn fetch_contexts(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<Task<Result<FunctionContext, ContextError>>> {
        let mut fetches = Vec::with_capacity(self.functions.len());
        for function in &self.functions {
            let fetch = match buffer_for(&self.files, &function.path) {
                Some(buffer) => self.context_cache.fetch(
                    &self.checkout.head_sha,
                    self.project.clone(),
                    buffer.clone(),
                    function,
                    cx,
                ),
                None => Task::ready(Err(ContextError::NoCallHierarchy)),
            };
            fetches.push(fetch);
        }
        fetches
    }

    fn set_contexts(
        &mut self,
        results: Vec<Result<FunctionContext, ContextError>>,
        cx: &mut Context<Self>,
    ) {
        let failure = if results.iter().any(Result::is_ok) {
            None
        } else {
            results
                .iter()
                .find_map(|result| result.as_ref().err().copied())
        };
        self.context_state = match failure {
            Some(error) => ContextState::Unavailable(unavailable_reason(error).into()),
            None => ContextState::Ready,
        };
        self.contexts = results.into_iter().map(Result::ok).collect();
        let edges = call_edges(&self.functions, &self.contexts, cx);
        self.compute_callers(cx);
        self.rebuild_map(&edges, cx);
    }

    /// A generated file, or one whose every change is whitespace or moved code, while those are
    /// hidden.
    fn hidden_by_view(&self, file: &LoadedFile) -> bool {
        (self.hide_generated && is_generated(&file.diff.path))
            || self.diff_simplified()
                && file.contents.is_some()
                && self
                    .changed_rows
                    .get(&file.diff.path)
                    .is_none_or(|rows| rows.is_empty())
    }

    fn map_inputs(&self) -> (Vec<MapFile>, Vec<MapFunction>) {
        let files = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| !self.hidden_by_view(file))
            .map(|(index, file)| {
                let mut kind = classify_path(&file.diff.path);
                let unanalysable = file.contents.is_none()
                    || matches!(
                        self.analyses.get(index),
                        Some(None | Some(FileAnalysis::Unanalysable))
                    );
                if kind == FileKind::Code && unanalysable {
                    kind = FileKind::Unanalysable;
                }
                MapFile {
                    path: file.diff.path.clone(),
                    kind,
                    status_label: status_label(&file.diff),
                }
            })
            .collect();
        let functions = self
            .functions
            .iter()
            .map(|function| MapFunction {
                path: function.path.clone(),
                name: function.name.clone(),
                first_row: function.range.start.row,
            })
            .collect();
        (files, functions)
    }

    fn rebuild_map(&mut self, edges: &[(usize, usize)], cx: &mut Context<Self>) {
        let (files, functions) = self.map_inputs();
        let selected_key = self.steps.get(self.selected).map(|step| step.key.clone());
        self.steps = build_review_map(&files, &functions, edges);
        // Changed functions in hidden files would otherwise still make steps.
        let shown: HashSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
        self.steps
            .retain(|step| step.files.iter().any(|path| shown.contains(path.as_str())));
        self.selected = selected_key
            .and_then(|key| self.steps.iter().position(|step| step.key == key))
            .unwrap_or_else(|| self.selected.min(self.steps.len().saturating_sub(1)));
        // Worked out here rather than per frame; a subset's steps say nothing about the full diff.
        if self.full_files.is_none() {
            self.stale_steps = self
                .steps
                .iter()
                .filter(|step| {
                    self.viewed_steps
                        .get(&step.key)
                        .and_then(|mark| mark.fingerprint.as_ref())
                        .is_some_and(|print| *print != self.step_fingerprint(step))
                })
                .map(|step| step.key.clone())
                .collect();
        }
        self.show_step(cx);
        cx.notify();
    }

    /// The file's changed rows, minus rows that belong only to functions of other steps.
    fn step_rows(&self, step: &Step, path: &str) -> Vec<Range<u32>> {
        let Some(rows) = self.changed_rows.get(path) else {
            return Vec::new();
        };
        rows.iter()
            .filter(|rows| {
                let mut owners = self
                    .functions
                    .iter()
                    .enumerate()
                    .filter(|(_, function)| {
                        function.path == path
                            && function.range.start.row < rows.end
                            && rows.start <= function.range.end.row
                    })
                    .map(|(index, _)| index)
                    .peekable();
                owners.peek().is_none() || owners.any(|index| step.functions.contains(&index))
            })
            .cloned()
            .collect()
    }

    fn show_step(&mut self, cx: &mut Context<Self>) {
        let step = self.steps.get(self.selected).cloned();
        let mut entries: Vec<(PathKey, Entity<Buffer>, Vec<ExcerptRange<Point>>)> = Vec::new();
        let mut shown_context: Vec<ContextExcerpt> = Vec::new();
        if let Some(step) = &step {
            for path in &step.files {
                let Some(buffer) = buffer_for(&self.files, path) else {
                    continue;
                };
                let Some(key) = path_key(entries.len(), buffer, path, cx) else {
                    continue;
                };
                let snapshot = buffer.read(cx).snapshot();
                let max_row = snapshot.max_point().row;
                let ranges = self.step_rows(step, path).into_iter().map(|rows| {
                    let start = rows.start.min(max_row);
                    let end = rows.end.saturating_sub(1).clamp(start, max_row);
                    Point::new(start, 0)..Point::new(end, snapshot.line_len(end))
                });
                let ranges = build_excerpt_ranges(ranges, HUNK_CONTEXT_LINES, &snapshot);
                entries.push((key, buffer.clone(), ranges));
            }

            for &index in &step.functions {
                let (Some(function), Some(Some(context))) =
                    (self.functions.get(index), self.contexts.get(index))
                else {
                    continue;
                };
                for mut excerpt in context_excerpts(function, context, &self.changed_rows, cx) {
                    let duplicate = shown_context.iter().any(|shown| {
                        shown.buffer == excerpt.buffer && shown.range.start == excerpt.range.start
                    });
                    if duplicate {
                        continue;
                    }
                    let expanded = self
                        .expanded_context
                        .contains(&(excerpt.buffer.entity_id(), excerpt.range.start));
                    if expanded {
                        let snapshot = excerpt.buffer.read(cx).snapshot();
                        let last_row = (excerpt.range.start.row + excerpt.full_rows - 1)
                            .min(snapshot.max_point().row);
                        excerpt.range.end = Point::new(last_row, snapshot.line_len(last_row));
                    }
                    shown_context.push(excerpt);
                }
            }
        }

        // A buffer can sit under only one path key, so context inside an already shown file
        // joins that file's ranges.
        for excerpt in &shown_context {
            let range = ExcerptRange {
                context: excerpt.range.clone(),
                primary: excerpt.range.clone(),
            };
            if let Some(entry) = entries.iter_mut().find(|entry| entry.1 == excerpt.buffer) {
                entry.2.push(range);
                continue;
            }
            let Some(path) = buffer_path(&excerpt.buffer, cx) else {
                continue;
            };
            let Some(key) = path_key(entries.len(), &excerpt.buffer, &path, cx) else {
                continue;
            };
            entries.push((key, excerpt.buffer.clone(), vec![range]));
        }

        let keys: Vec<PathKey> = entries.iter().map(|entry| entry.0.clone()).collect();
        let stale: Vec<PathKey> = self
            .shown_paths
            .iter()
            .filter(|key| !keys.contains(key))
            .cloned()
            .collect();
        let diffs: Vec<Entity<BufferDiff>> = self
            .files
            .iter()
            .filter_map(|file| file.contents.as_ref())
            .filter(|(buffer, _)| entries.iter().any(|entry| &entry.1 == buffer))
            .map(|(_, diff)| diff.clone())
            .collect();
        self.multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.remove_excerpts_for_paths(stale, cx);
            for (key, buffer, ranges) in entries {
                let snapshot = buffer.read(cx).snapshot();
                multibuffer.set_excerpt_ranges_for_path(key, buffer, &snapshot, ranges, cx);
            }
            for diff in diffs {
                multibuffer.add_diff(diff, cx);
            }
        });
        self.shown_paths = keys;

        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let item = cx.weak_entity();
        let mut blocks = Vec::new();
        let mut highlights = Vec::new();
        for (index, excerpt) in shown_context.iter().enumerate() {
            let buffer = excerpt.buffer.read(cx).snapshot();
            let Some(start) = snapshot.anchor_in_excerpt(buffer.anchor_before(excerpt.range.start))
            else {
                continue;
            };
            let end = snapshot
                .anchor_in_excerpt(buffer.anchor_after(excerpt.range.end))
                .unwrap_or(start);
            let collapsed = excerpt.range.end.row - excerpt.range.start.row + 1 < excerpt.full_rows;
            let label = if collapsed {
                format!(
                    "{} · collapsed · {} lines",
                    excerpt.label, excerpt.full_rows
                )
            } else {
                excerpt.label.clone()
            };
            blocks.push(BlockProperties {
                placement: BlockPlacement::Above(start),
                height: Some(1),
                style: BlockStyle::Flex,
                render: label_renderer(label.into(), collapsed, index, item.clone()),
                priority: 0,
            });
            highlights.push(start..end);
        }
        let stale_blocks = std::mem::take(&mut self.label_blocks);
        self.label_blocks = self.editor.update(cx, |editor, cx| {
            editor.remove_blocks(stale_blocks.into_iter().collect(), None, cx);
            editor.clear_row_highlights::<UnchangedContext>();
            for range in highlights {
                editor.highlight_rows::<UnchangedContext>(
                    range,
                    |_| rgb(UNCHANGED_BACKGROUND).into(),
                    RowHighlightOptions::default(),
                    cx,
                );
            }
            editor.insert_blocks(blocks, None, cx).into_iter().collect()
        });
        self.shown_context = shown_context;
        self.show_comments(cx);
    }

    fn compute_callers(&mut self, cx: &App) {
        self.callers = self
            .functions
            .iter()
            .zip(&self.contexts)
            .map(|(function, context)| {
                let Some(context) = context else {
                    return Vec::new();
                };
                context
                    .callers
                    .iter()
                    .map(|call| {
                        let path = buffer_path(&call.from.buffer, cx).unwrap_or_default();
                        let snapshot = call.from.buffer.read(cx).snapshot();
                        let row = call
                            .from_ranges
                            .iter()
                            .map(|location| {
                                language::ToPoint::to_point(
                                    &location.range.start,
                                    &location.buffer.read(cx).snapshot(),
                                )
                                .row
                            })
                            .min()
                            .unwrap_or_else(|| {
                                language::ToPoint::to_point(&call.from.range.start, &snapshot).row
                            });
                        ImpactRow {
                            location: format!("{path}:{}", row + 1),
                            mark: classify_caller(
                                Some(function.change),
                                caller_touched(call, &self.changed_rows, cx),
                            ),
                        }
                    })
                    .collect()
            })
            .collect();
    }

    /// Callers of the changed function at `function_index`; empty until the call data arrives.
    pub fn callers(&self, function_index: usize) -> &[ImpactRow] {
        self.callers
            .get(function_index)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// What's left to review.
    fn progress_text(&self) -> String {
        let states: Vec<ViewedState> = self
            .steps
            .iter()
            .map(|step| self.step_state(step))
            .collect();
        let left = states
            .iter()
            .filter(|state| **state != ViewedState::Viewed)
            .count();
        let changed = states
            .iter()
            .filter(|state| **state == ViewedState::ChangedSinceViewed)
            .count();
        if left == 0 && !states.is_empty() {
            return "All steps viewed".to_string();
        }
        let mut text = format!("{left} of {} steps left to review", states.len());
        if changed > 0 {
            text.push_str(&format!(" · {changed} changed since viewed"));
        }
        text
    }

    fn all_files(&self) -> &[LoadedFile] {
        self.full_files.as_deref().unwrap_or(&self.files)
    }

    pub fn checklist_items(&self) -> &[ChecklistItem] {
        &self.checklist_items
    }

    pub fn checklist_errors(&self) -> &[String] {
        &self.checklist_errors
    }

    pub fn checklist_answer(&self, item_key: &str) -> Option<Answer> {
        self.checklist_answers.get(item_key).copied()
    }

    fn rebuild_checklist(&mut self, cx: &mut Context<Self>) {
        let checklists: Vec<_> = PrReviewSettings::get_global(cx)
            .checklists
            .iter()
            .map(|checklist| (ChecklistSource::Settings, checklist.clone()))
            .chain(
                self.team_checklists
                    .iter()
                    .map(|checklist| (ChecklistSource::Team, checklist.clone())),
            )
            .collect();
        let key = &self.detail.summary.key;
        let changed: Vec<String> = self
            .all_files()
            .iter()
            .map(|file| file.diff.path.clone())
            .collect();
        let (items, mut errors) = applicable_items(
            &checklists,
            &format!("{}/{}", key.owner, key.repo),
            &changed,
        );
        errors.extend(
            PrReviewSettings::get_global(cx)
                .unknown_auto_checks
                .iter()
                .map(|name| format!("Unknown check \"{name}\" in pr_review.auto_checks")),
        );
        self.checklist_items = items;
        self.checklist_errors = errors;
        self.refresh_auto_results(cx);
        cx.notify();
    }

    pub fn set_team_checklists(
        &mut self,
        checklists: Vec<PrReviewChecklistContent>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.team_checklists = checklists;
        self.team_error = error;
        self.rebuild_checklist(cx);
    }

    pub fn auto_results(&self) -> &[(AutoCheck, AutoResult)] {
        &self.auto_results
    }

    /// Re-run after anything the checks read changes: files, PR detail, the local run, settings.
    fn refresh_auto_results(&mut self, cx: &App) {
        let inputs = AutoInputs {
            checks: self.detail.summary.checks,
            threads: &self.detail.threads,
            files: self.all_files().iter().map(|file| &file.diff).collect(),
            head_sha: &self.checkout.head_sha,
            local_run: self.local_run.as_ref(),
        };
        self.auto_results = PrReviewSettings::get_global(cx)
            .auto_checks
            .iter()
            .map(|check| (*check, evaluate(*check, &inputs)))
            .collect();
    }

    fn checklist_progress(&self) -> Progress {
        crate::progress(
            &self.auto_results,
            &self.checklist_items,
            &self.checklist_answers,
        )
    }

    fn checklist_block(&self, cx: &App) -> Option<String> {
        review_block(
            &self.auto_results,
            &self.checklist_items,
            &self.checklist_answers,
            &self.checklist_notes(cx),
        )
    }

    fn checklist_notes(&self, cx: &App) -> HashMap<String, String> {
        self.note_editors
            .iter()
            .map(|(key, editor)| (key.clone(), editor.read(cx).text(cx)))
            .collect()
    }

    /// Clicking the answer already given clears it.
    pub fn answer_checklist_item(
        &mut self,
        item_key: &str,
        answer: Answer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = (self.checklist_answers.get(item_key) != Some(&answer)).then_some(answer);
        self.set_checklist_answer(item_key, answer, window, cx);
    }

    pub fn set_checklist_answer(
        &mut self,
        item_key: &str,
        answer: Option<Answer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match answer {
            Some(answer) => {
                self.checklist_answers.insert(item_key.to_string(), answer);
            }
            None => {
                self.checklist_answers.remove(item_key);
            }
        }
        if answer == Some(Answer::No) {
            self.ensure_note_editor(item_key, window, cx);
        }
        let database = self.database.clone();
        let key = self.detail.summary.key.clone();
        let item_key = item_key.to_string();
        // Chained, so quick clicks are stored in the order they were made, and detached, so
        // closing the review doesn't drop the last one.
        let previous = self.checklist_write.clone();
        let write = cx
            .background_spawn(async move {
                previous.await;
                database
                    .set_checklist_answer(key, item_key, answer)
                    .await
                    .log_err();
            })
            .shared();
        cx.background_spawn(write.clone()).detach();
        self.checklist_write = write;
        cx.notify();
    }

    fn ensure_note_editor(&mut self, item_key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.note_editors.contains_key(item_key) {
            return;
        }
        let slot = checklist_note_slot(item_key);
        let text = self
            .text_syncs
            .get(&slot)
            .map(|sync| sync.lock().latest.clone())
            .unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 4, window, cx);
            editor.set_placeholder_text("Why not? (goes in your review)", window, cx);
            editor.set_text(text, window, cx);
            editor
        });
        self._subscriptions.push(cx.subscribe(
            &editor,
            move |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    this.queue_text(&slot, text, cx);
                }
            },
        ));
        self.note_editors.insert(item_key.to_string(), editor);
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let summary = &self.detail.summary;
        let files = self.all_files().len();
        let title = SharedString::from(summary.title.clone());
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
                        div()
                            .id("photon-review-title")
                            .debug_selector(|| "photon-review-title".into())
                            .min_w_0()
                            .tooltip(Tooltip::text(title.clone()))
                            .child(
                                Label::new(title)
                                    .size(LabelSize::Large)
                                    .weight(gpui::FontWeight::SEMIBOLD)
                                    .line_clamp(3),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(
                                Label::new(format!("#{}", summary.key.number))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(format!("{} → {}", summary.author, summary.base_ref))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                ui::DiffStat::new(
                                    "photon-review-diff-stat",
                                    usize::try_from(self.detail.additions).unwrap_or(usize::MAX),
                                    usize::try_from(self.detail.deletions).unwrap_or(usize::MAX),
                                )
                                .tooltip(format!(
                                    "{files} file{} changed",
                                    if files == 1 { "" } else { "s" }
                                )),
                            )
                            .child(self.render_readiness()),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(self.render_view_menu(cx))
                    .child(self.render_more_menu(cx))
                    .children(self.render_merge(cx))
                    .child(self.render_submit_controls(cx)),
            )
    }

    /// A manual item, or an auto check that didn't pass. Ticking means Yes; the other answers
    /// `choices` allows sit in the item's menu.
    fn render_check_row(
        &self,
        item_key: &str,
        text: &str,
        evidence: Option<(&str, Color)>,
        choices: &[Answer],
        cx: &Context<Self>,
    ) -> AnyElement {
        let answer = self
            .checklist_answers
            .get(item_key)
            .copied()
            .filter(|answer| choices.contains(answer));
        let text_color = match answer {
            Some(Answer::Yes | Answer::NotApplicable) => Color::Muted,
            Some(Answer::No) => Color::Error,
            None => Color::Default,
        };
        let (allows_no, allows_not_applicable) = (
            choices.contains(&Answer::No),
            choices.contains(&Answer::NotApplicable),
        );
        let item = cx.weak_entity();
        let menu_key = item_key.to_string();
        let menu =
            ui::PopoverMenu::new(SharedString::from(format!("photon-check-menu-{item_key}")))
                .trigger_with_tooltip(
                    IconButton::new(
                        SharedString::from(format!("photon-check-more-{item_key}")),
                        IconName::Ellipsis,
                    )
                    .icon_size(IconSize::Small),
                    Tooltip::text("Other answers"),
                )
                .anchor(gpui::Anchor::TopRight)
                .menu(move |window, cx| {
                    let item = item.clone();
                    let key = menu_key.clone();
                    Some(ui::ContextMenu::build(window, cx, move |menu, _, _| {
                        let set = |value: Option<Answer>| {
                            let item = item.clone();
                            let key = key.clone();
                            move |window: &mut Window, cx: &mut App| {
                                item.update(cx, |item, cx| {
                                    item.set_checklist_answer(&key, value, window, cx)
                                })
                                .log_err();
                            }
                        };
                        menu.when(allows_no, |menu| {
                            menu.toggleable_entry(
                                "No, add a note",
                                answer == Some(Answer::No),
                                IconPosition::Start,
                                None,
                                set(Some(Answer::No)),
                            )
                        })
                        .when(allows_not_applicable, |menu| {
                            menu.toggleable_entry(
                                "Doesn't apply",
                                answer == Some(Answer::NotApplicable),
                                IconPosition::Start,
                                None,
                                set(Some(Answer::NotApplicable)),
                            )
                        })
                        .when(answer.is_some(), |menu| {
                            menu.separator().entry("Clear answer", None, set(None))
                        })
                    }))
                });
        let box_key = item_key.to_string();
        let box_selector = format!("photon-check-box-{item_key}");
        let menu_selector = format!("photon-check-menu-{item_key}");
        h_flex()
            .items_start()
            .gap_1p5()
            .py_0p5()
            .child(
                div().debug_selector(move || box_selector).child(
                    Checkbox::new(
                        SharedString::from(format!("photon-check-{item_key}")),
                        (answer == Some(Answer::Yes)).into(),
                    )
                    .disabled(!choices.contains(&Answer::Yes))
                    .on_click(cx.listener(
                        move |this, _: &ToggleState, window, cx| {
                            this.answer_checklist_item(&box_key, Answer::Yes, window, cx)
                        },
                    )),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new(text.to_string())
                            .size(LabelSize::Small)
                            .color(text_color),
                    )
                    .children(evidence.map(|(evidence, color)| {
                        Label::new(evidence.to_string())
                            .size(LabelSize::XSmall)
                            .color(color)
                    }))
                    .when(answer == Some(Answer::NotApplicable), |row| {
                        row.child(
                            Label::new("Doesn't apply")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when(answer == Some(Answer::No), |row| {
                        row.children(self.note_editors.get(item_key).cloned())
                    }),
            )
            .child(div().debug_selector(move || menu_selector).child(menu))
            .into_any_element()
    }

    /// What the diff shows. New view controls go in this menu rather than the header.
    fn render_view_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let item = cx.weak_entity();
        let (hide_whitespace, hide_moves, hide_generated) =
            (self.hide_whitespace, self.hide_moves, self.hide_generated);
        let since_offered = self.changes_since_review_offered();
        let since_on = self.showing_changes_since_review()
            || (self.loading_since_review && !self.unreviewed_view);
        let unreviewed_offered = self.unreviewed_offered();
        let unreviewed_on = self.showing_unreviewed();
        let anything_hidden =
            !self.moves.is_empty() || self.files.iter().any(|file| self.hidden_by_view(file));
        let show_hidden = self.show_hidden;
        let hidden = [hide_whitespace, hide_moves, hide_generated]
            .into_iter()
            .filter(|hidden| *hidden)
            .count();
        let label = if self.showing_changes_since_review() {
            "View · since review".to_string()
        } else if unreviewed_on {
            "View · not reviewed".to_string()
        } else {
            match hidden {
                0 => "View".to_string(),
                count => format!("View · {count} hidden"),
            }
        };
        let point_out_since = since_offered && !self.view_menu_seen;
        let menu = ui::PopoverMenu::new("photon-review-view")
            .trigger_with_tooltip(
                crate::button("photon-review-view-trigger", label)
                    .style(ButtonStyle::Outlined)
                    .toggle_state(hidden > 0 || since_on || unreviewed_on)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                Tooltip::text(if point_out_since {
                    "New: show only the changes since your review"
                } else {
                    "Choose what the diff shows"
                }),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let item = item.clone();
                item.update(cx, |item, cx| {
                    if !item.view_menu_seen {
                        item.view_menu_seen = true;
                        cx.notify();
                    }
                })
                .log_err();
                Some(ui::ContextMenu::build(window, cx, move |menu, _, _| {
                    let toggle = |whitespace: bool| {
                        let item = item.clone();
                        move |_: &mut Window, cx: &mut App| {
                            item.update(cx, |item, cx| item.toggle_diff_view(whitespace, cx))
                                .log_err();
                        }
                    };
                    menu.header("Hide in diff")
                        .toggleable_entry(
                            "Whitespace changes",
                            hide_whitespace,
                            IconPosition::Start,
                            None,
                            toggle(true),
                        )
                        .toggleable_entry(
                            "Moved code",
                            hide_moves,
                            IconPosition::Start,
                            None,
                            toggle(false),
                        )
                        .toggleable_entry(
                            "Generated and lock files",
                            hide_generated,
                            IconPosition::Start,
                            None,
                            {
                                let item = item.clone();
                                move |_, cx| {
                                    item.update(cx, |item, cx| item.toggle_hide_generated(cx))
                                        .log_err();
                                }
                            },
                        )
                        .when(since_offered || unreviewed_offered, |menu| {
                            menu.separator().header("Show")
                        })
                        .when(unreviewed_offered, |menu| {
                            let item = item.clone();
                            menu.toggleable_entry(
                                "Only what you haven't reviewed",
                                unreviewed_on,
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    item.update(cx, |item, cx| item.toggle_unreviewed(cx))
                                        .log_err();
                                },
                            )
                        })
                        .when(since_offered, |menu| {
                            let item = item.clone();
                            menu.toggleable_entry(
                                "Only changes since your review",
                                since_on,
                                IconPosition::Start,
                                Some(ToggleChangesSinceReview.boxed_clone()),
                                move |window, cx| {
                                    item.update(cx, |item, cx| {
                                        item.toggle_changes_since_review(
                                            &ToggleChangesSinceReview,
                                            window,
                                            cx,
                                        )
                                    })
                                    .log_err();
                                },
                            )
                        })
                        .when(anything_hidden, |menu| {
                            let item = item.clone();
                            menu.separator().toggleable_entry(
                                "Show what's hidden",
                                show_hidden,
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    item.update(cx, |item, cx| {
                                        item.show_hidden = !item.show_hidden;
                                        cx.notify();
                                    })
                                    .log_err();
                                },
                            )
                        })
                }))
            });
        div()
            .relative()
            .debug_selector(|| "photon-review-view".into())
            .child(menu)
            .when(point_out_since, |this| {
                this.child(
                    div()
                        .debug_selector(|| "photon-review-view-dot".into())
                        .absolute()
                        .top_0()
                        .right_0()
                        .child(Indicator::dot().color(Color::Accent)),
                )
            })
    }

    /// Occasional actions, out of the header's way.
    fn render_more_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let item = cx.weak_entity();
        let agent_offered = review_agent(cx).is_some() && !self.is_own_pr();
        let agent_label = if self.agent_reviewing {
            "Cancel agent pre-review"
        } else {
            "Pre-review with agent"
        };
        let url = self.detail.summary.url.clone();
        let menu = ui::PopoverMenu::new("photon-review-more")
            .trigger_with_tooltip(
                IconButton::new("photon-review-more-trigger", IconName::Ellipsis)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::Small),
                Tooltip::text("More actions"),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let item = item.clone();
                let url = url.clone();
                Some(ui::ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.when(agent_offered, |menu| {
                        let item = item.clone();
                        menu.entry(agent_label, None, move |window, cx| {
                            item.update(cx, |item, cx| item.start_agent_review(window, cx))
                                .log_err();
                        })
                    })
                    .entry("Run locally", None, {
                        let item = item.clone();
                        move |window, cx| {
                            item.update(cx, |item, cx| item.run_locally(window, cx))
                                .log_err();
                        }
                    })
                    .entry(
                        "Add your own checklist…",
                        Some(zed_actions::OpenSettingsFile.boxed_clone()),
                        |window, cx| {
                            window.dispatch_action(zed_actions::OpenSettingsFile.boxed_clone(), cx)
                        },
                    )
                    .separator()
                    .entry("Open on GitHub", None, move |_, cx| cx.open_url(&url))
                }))
            });
        div()
            .debug_selector(|| "photon-review-more".into())
            .child(menu)
    }

    /// What matters most about merging, opening the checks, review decision and mergeability.
    fn render_readiness(&self) -> impl IntoElement {
        let readiness = self.detail.readiness();
        let color = match readiness {
            Readiness::Merged => Color::Accent,
            Readiness::Conflicts | Readiness::ChecksFailing => Color::Error,
            Readiness::ChangesRequested | Readiness::ReviewRequired | Readiness::Behind(_) => {
                Color::Warning
            }
            Readiness::Ready => Color::Success,
            Readiness::Closed
            | Readiness::Draft
            | Readiness::ChecksRunning
            | Readiness::Checking => Color::Muted,
        };
        let merge = self.detail.merge.clone();
        let base = self.detail.summary.base_ref.clone();
        let menu = ui::PopoverMenu::new("photon-review-readiness")
            .trigger(
                ui::ButtonLike::new("photon-review-readiness-trigger").child(
                    ui::Chip::new(readiness.label())
                        .label_color(color)
                        .label_size(LabelSize::Small),
                ),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let merge = merge.clone();
                let base = base.clone();
                Some(ui::ContextMenu::build(window, cx, move |mut menu, _, _| {
                    if !merge.checks.is_empty() {
                        menu = menu.header("Checks");
                    }
                    for check in &merge.checks {
                        let (icon, color) = match check.outcome {
                            CheckOutcome::Passed => (IconName::Check, Color::Success),
                            CheckOutcome::Failed => (IconName::XCircle, Color::Error),
                            CheckOutcome::Running => (IconName::Circle, Color::Warning),
                        };
                        let url = check.url.clone();
                        menu = menu.item(
                            ui::ContextMenuEntry::new(format!(
                                "{}: {}",
                                check.name,
                                check.outcome.label()
                            ))
                            .icon(icon)
                            .icon_color(color)
                            .disabled(url.is_none())
                            .handler(move |_, cx| {
                                if let Some(url) = &url {
                                    cx.open_url(url);
                                }
                            }),
                        );
                    }
                    let review = match merge.review_decision.as_deref() {
                        Some("APPROVED") => "Approved",
                        Some("CHANGES_REQUESTED") => "Changes requested",
                        Some("REVIEW_REQUIRED") => "Review required",
                        _ => "No review required",
                    };
                    let mergeability = match merge.mergeable.as_str() {
                        "CONFLICTING" => format!("Conflicts with {base}"),
                        "MERGEABLE" if merge.merge_state == "BEHIND" => {
                            format!("No conflicts, behind {base}")
                        }
                        "MERGEABLE" => "No conflicts".to_string(),
                        _ => "Checking…".to_string(),
                    };
                    menu.header("Review")
                        .label(review)
                        .header("Merge")
                        .label(mergeability)
                }))
            });
        div()
            .debug_selector(|| "photon-review-readiness".into())
            .child(menu)
    }

    /// Merge with the repository's default method, the other allowed ones in the menu. Blocks
    /// other than drafts and conflicts are GitHub's to report when the merge is tried.
    fn render_merge(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.detail.state != PrState::Open {
            return None;
        }
        let (primary, others) = self.detail.merge.methods.split_first()?;
        let primary = *primary;
        let blocked = match self.detail.readiness() {
            Readiness::Draft => {
                Some("A draft can't be merged. Mark it ready for review first.".to_string())
            }
            Readiness::Conflicts => Some(format!(
                "Resolve the conflicts with {} first",
                self.detail.summary.base_ref
            )),
            _ => None,
        };
        let disabled = blocked.is_some() || self.merging;
        let button = crate::button("photon-review-merge-button", primary.label())
            .disabled(disabled)
            .when_some(blocked, |button, reason| {
                button.tooltip(Tooltip::text(reason))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.merge(primary, window, cx)));
        let control = if others.is_empty() {
            button.style(ButtonStyle::Outlined).into_any_element()
        } else {
            let item = cx.weak_entity();
            let others = others.to_vec();
            let menu = ui::PopoverMenu::new("photon-review-merge-methods")
                .trigger_with_tooltip(
                    IconButton::new("photon-review-merge-methods-trigger", IconName::ChevronDown)
                        .icon_size(IconSize::XSmall)
                        .disabled(disabled),
                    Tooltip::text("Other ways to merge"),
                )
                .anchor(gpui::Anchor::TopRight)
                .menu(move |window, cx| {
                    let item = item.clone();
                    let others = others.clone();
                    Some(ui::ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for method in &others {
                            let item = item.clone();
                            let method = *method;
                            menu = menu.entry(method.label(), None, move |window, cx| {
                                item.update(cx, |item, cx| item.merge(method, window, cx))
                                    .log_err();
                            });
                        }
                        menu
                    }))
                });
            ui::SplitButton::new(
                button,
                div()
                    .debug_selector(|| "photon-review-merge-methods".into())
                    .child(menu)
                    .into_any_element(),
            )
            .style(ui::SplitButtonStyle::Outlined)
            .into_any_element()
        };
        Some(
            div()
                .debug_selector(|| "photon-review-merge".into())
                .child(control)
                .into_any_element(),
        )
    }

    /// The one primary action: Submit review, or Push changes… on your own PR, where Submit
    /// review stays as a quiet button since the author can still post comments.
    fn render_submit_controls(&self, cx: &Context<Self>) -> impl IntoElement {
        let item = cx.weak_entity();
        let push = self.is_own_pr() && self.push_command().is_some();
        let submit = ui::PopoverMenu::new("photon-review-submit")
            .trigger(
                crate::button("photon-review-submit-trigger", "Submit review").style(if push {
                    ButtonStyle::Outlined
                } else {
                    ButtonStyle::Tinted(TintColor::Accent)
                }),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let item = item.upgrade()?;
                Some(cx.new(|cx| SubmitPopover::new(item, window, cx)))
            });
        h_flex()
            .gap_1()
            .child(
                div()
                    .debug_selector(|| "photon-review-submit".into())
                    .child(submit),
            )
            .when(push, |this| {
                this.child(
                    div().debug_selector(|| "photon-review-push".into()).child(
                        crate::button("photon-review-push-button", "Push changes…")
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .tooltip(Tooltip::text(
                                "Opens a terminal with git add, commit and push typed in, for you to run",
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_push_terminal(window, cx)
                            })),
                    ),
                )
            })
    }

    /// The verdict the Submit popover posts. Your own PR can only get comments.
    fn verdict(&self) -> ReviewEvent {
        if self.is_own_pr() {
            ReviewEvent::Comment
        } else {
            self.verdict
        }
    }

    /// The strip above the diff listing what the View menu hides, shown on request from that
    /// menu: hidden files leave no step, so there is no place in the diff to fold them into.
    fn render_hidden_by_view(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.show_hidden {
            return None;
        }
        let hidden_files: Vec<&str> = self
            .files
            .iter()
            .filter(|file| self.hidden_by_view(file))
            .map(|file| file.diff.path.as_str())
            .collect();
        if hidden_files.is_empty() && self.moves.is_empty() {
            return None;
        }
        let row = |text: String| {
            Label::new(text)
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .truncate()
        };
        Some(
            v_flex()
                .debug_selector(|| "photon-review-hidden".into())
                .px_2()
                .py_1()
                .gap_0p5()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Label::new("Hidden from the diff")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .children(self.moves.iter().map(|moved| {
                    row(format!(
                        "Moved {} lines: {}:{} → {}:{}",
                        moved.lines, moved.from_path, moved.from_line, moved.to_path, moved.to_line
                    ))
                }))
                .children(hidden_files.into_iter().map(|path| {
                    row(if self.hide_generated && is_generated(path) {
                        format!("{path}: generated")
                    } else {
                        format!("{path}: only whitespace or moved code")
                    })
                }))
                .into_any_element(),
        )
    }

    /// A one-line summary that unfolds what it sums up.
    fn render_fold_line(
        &self,
        id: &'static str,
        open: bool,
        text: String,
        flag: fn(&mut Self) -> &mut bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(id)
            .debug_selector(move || id.to_string())
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let open = flag(this);
                *open = !*open;
                cx.notify();
            }))
            .child(Disclosure::new(
                SharedString::from(format!("{id}-disclosure")),
                open,
            ))
            .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
    }

    fn render_checklist(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let auto = &self.auto_results;
        let boxes = parse_author_boxes(&self.detail.body);
        let errors: Vec<&String> = self
            .checklist_errors
            .iter()
            .chain(self.team_error.as_ref())
            .collect();
        if auto.is_empty()
            && self.checklist_items.is_empty()
            && boxes.is_empty()
            && errors.is_empty()
        {
            return None;
        }
        let progress = self.checklist_progress();
        let section = v_flex()
            .id("photon-review-checklist")
            .pt_2()
            .gap_1()
            .child(
                h_flex()
                    .id("photon-review-checklist-toggle")
                    .debug_selector(|| "photon-review-checklist-toggle".into())
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.checklist_open = !this.checklist_open;
                        cx.notify();
                    }))
                    .child(Disclosure::new(
                        "photon-review-checklist-disclosure",
                        self.checklist_open,
                    ))
                    .child(
                        Label::new(format!("Checklist · {}", progress.text()))
                            .size(LabelSize::Small),
                    ),
            )
            .children(errors.into_iter().map(|error| {
                Label::new(error.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
            }));
        if !self.checklist_open {
            return Some(section.into_any_element());
        }
        let mut body = v_flex()
            .debug_selector(|| "photon-review-checklist-body".into())
            .pl_1()
            .gap_1();
        let (passed, open): (Vec<_>, Vec<_>) = auto.iter().partition(|(_, result)| result.passed());
        for (check, result) in open {
            // Against evidence of a failure, the reviewer can only say it doesn't apply.
            let (color, choices): (Color, &[Answer]) = match result {
                AutoResult::Failed(_) => (Color::Warning, &[Answer::NotApplicable]),
                _ => (Color::Muted, &ALL_ANSWERS),
            };
            body = body.child(self.render_check_row(
                &check.key(),
                check.title(),
                Some((result.evidence(), color)),
                choices,
                cx,
            ));
        }
        for item in &self.checklist_items {
            body = body.child(self.render_check_row(
                &item.key,
                &item.text,
                Some((&item.checklist, Color::Muted)),
                &ALL_ANSWERS,
                cx,
            ));
        }
        if !passed.is_empty() {
            body = body.child(self.render_fold_line(
                "photon-review-photon-checked",
                self.photon_checked_open,
                format!("✓ {} checked by Photon", passed.len()),
                |this| &mut this.photon_checked_open,
                cx,
            ));
            if self.photon_checked_open {
                for (check, result) in passed {
                    body = body.child(
                        v_flex()
                            .pl_5()
                            .child(Label::new(check.title()).size(LabelSize::Small))
                            .child(
                                Label::new(result.evidence().to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    );
                }
            }
        }
        if !boxes.is_empty() {
            let ticked = boxes.iter().filter(|author_box| author_box.checked).count();
            body = body.child(self.render_fold_line(
                "photon-review-author-boxes",
                self.author_boxes_open,
                format!("The author ticked {ticked} of {}", boxes.len()),
                |this| &mut this.author_boxes_open,
                cx,
            ));
            if self.author_boxes_open {
                for author_box in boxes {
                    body = body.child(
                        div().pl_5().child(
                            Label::new(format!(
                                "{} {}",
                                if author_box.checked { "☑" } else { "☐" },
                                author_box.text
                            ))
                            .size(LabelSize::Small)
                            .color(if author_box.checked {
                                Color::Default
                            } else {
                                Color::Muted
                            }),
                        ),
                    );
                }
            }
        }
        Some(section.child(body).into_any_element())
    }

    /// A changed function in the selected step. Once its callers are known, clicking it lists
    /// them, and a caller that wasn't updated for its change is called out with a tag.
    fn render_function_row(
        &self,
        function_index: usize,
        name: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let not_updated = self
            .callers(function_index)
            .iter()
            .filter(|row| row.mark == CallerMark::NotUpdated)
            .count();
        let known = self
            .contexts
            .get(function_index)
            .is_some_and(Option::is_some);
        let label = format!("{name}()");
        let trigger = if known {
            let item = cx.weak_entity();
            let title = label.clone();
            ui::PopoverMenu::new(("photon-review-callers", function_index))
                .trigger_with_tooltip(
                    Button::new(("photon-review-function", function_index), label)
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .label_size(LabelSize::Small)
                        .color(Color::Accent),
                    Tooltip::text("Changed in this PR. Click to see its callers"),
                )
                .menu(move |window, cx| {
                    let rows = item
                        .read_with(cx, |item, _| item.callers(function_index).to_vec())
                        .ok()?;
                    let title = title.clone();
                    Some(ui::ContextMenu::build(window, cx, move |menu, _, _| {
                        let mut menu = menu.header(format!("Callers of {title}"));
                        if rows.is_empty() {
                            return menu.custom_row(|_, _| {
                                div()
                                    .debug_selector(|| "photon-review-no-callers".into())
                                    .child(
                                        Label::new("No callers found")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .into_any_element()
                            });
                        }
                        for (index, row) in rows.iter().enumerate() {
                            let (tag, color) = mark_tag(row.mark);
                            let location = row.location.clone();
                            let explanation = mark_explanation(row.mark);
                            menu = menu.custom_row(move |_, _| {
                                let selector = format!("photon-review-caller-{location}-{tag}");
                                h_flex()
                                    .id(("photon-review-caller", index))
                                    .debug_selector(move || selector)
                                    .w_full()
                                    .justify_between()
                                    .gap_4()
                                    .tooltip(Tooltip::text(explanation))
                                    .child(Label::new(location.clone()).size(LabelSize::Small))
                                    .child(Label::new(tag).size(LabelSize::XSmall).color(color))
                                    .into_any_element()
                            });
                        }
                        menu
                    }))
                })
                .into_any_element()
        } else {
            Label::new(label)
                .size(LabelSize::Small)
                .color(Color::Accent)
                .into_any_element()
        };
        let selector = format!("photon-review-function-{name}");
        let tag_selector = format!("photon-review-not-updated-{name}");
        h_flex()
            .pl_6()
            .gap_1p5()
            .child(div().size_1p5().rounded_full().bg(Color::Accent.color(cx)))
            .child(div().debug_selector(move || selector).child(trigger))
            .when(not_updated > 0, |row| {
                row.child(
                    div()
                        .id(("photon-review-not-updated", function_index))
                        .debug_selector(move || tag_selector)
                        .tooltip(Tooltip::text(mark_explanation(CallerMark::NotUpdated)))
                        .child(
                            Label::new(format!("{not_updated} not updated"))
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        ),
                )
            })
            .into_any_element()
    }

    /// `width` is measured from the review's left edge; the code keeps at least
    /// `MIN_CODE_WIDTH`.
    fn set_map_width(&mut self, width: Pixels, available: Pixels, cx: &mut Context<Self>) {
        let widest = (available - MIN_CODE_WIDTH).max(MIN_MAP_WIDTH);
        self.map_width = width.clamp(MIN_MAP_WIDTH, widest);
        cx.notify();
    }

    pub fn map_width(&self) -> Pixels {
        self.map_width
    }

    fn render_map_resize_handle(&self, cx: &Context<Self>) -> impl IntoElement {
        let item = cx.weak_entity();
        // Inside the map's edge: the code, drawn after the map, would cover anything past it.
        div()
            .id("photon-review-map-resize")
            .debug_selector(|| "photon-review-map-resize".into())
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(MAP_RESIZE_HANDLE_WIDTH)
            .cursor_col_resize()
            .on_drag(DraggedReviewMap, |dragged, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| dragged.clone())
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, move |event, _, cx| {
                if event.click_count == 2 {
                    item.update(cx, |item, cx| {
                        item.map_width = DEFAULT_MAP_WIDTH;
                        cx.notify();
                    })
                    .log_err();
                    cx.stop_propagation();
                }
            })
    }

    /// Unresolved threads and drafts on these files, wherever they sit in the review.
    fn comment_count(&self, paths: &[String]) -> usize {
        let threads = self
            .detail
            .threads
            .iter()
            .filter(|thread| !thread.is_resolved && paths.contains(&thread.path))
            .count();
        let drafts = self
            .drafts
            .iter()
            .filter(|draft| !draft.general && paths.contains(&draft.path))
            .count();
        threads + drafts
    }

    fn render_map(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let mut map = v_flex()
            .id("photon-review-map")
            .track_scroll(&self.map_scroll)
            .w(self.map_width)
            .flex_none()
            .h_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(colors.border)
            .child(
                div()
                    .debug_selector(|| "photon-review-progress".into())
                    .child(
                        Label::new(self.progress_text())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            // Always the map's second child: a step's checklist link scrolls to it.
            .child(div().children(self.render_checklist(cx)));
        map = map.children(self.render_feedback(cx));
        map = map.children(self.render_outdated_drafts(cx));
        map = map.children(self.render_dropped_findings(cx));
        map = map.children(self.render_unplaced_threads(cx));
        let comments = self.detail.comment_count;
        map = map.child(
            h_flex()
                .id("photon-review-conversation")
                .debug_selector(|| "photon-review-conversation".into())
                .gap_2()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .when(self.conversation_open, |this| {
                    this.bg(colors.element_selected)
                })
                .on_click(cx.listener(|this, _, window, cx| this.open_conversation(window, cx)))
                .child(
                    Icon::new(IconName::Chat)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(div().flex_1().child(Label::new("Conversation")))
                .when(comments > 0, |this| {
                    this.child(
                        Label::new(comments.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                }),
        );
        for (index, step) in self.steps.iter().enumerate() {
            let selected = index == self.selected && !self.conversation_open;
            let state = self.step_state(step);
            let viewed = state == ViewedState::Viewed;
            map = map.child(
                h_flex()
                    .id(("photon-review-step", index))
                    .gap_2()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(colors.element_selected))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.select_step(index, window, cx)),
                    )
                    .child(
                        // Ticking a step mustn't also select it.
                        div()
                            .debug_selector(move || format!("photon-review-step-viewed-{index}"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Checkbox::new(("photon-review-step-viewed", index), viewed.into())
                                    .disabled(self.full_files.is_some())
                                    .tooltip(Tooltip::text(if self.full_files.is_some() {
                                        VIEWED_DISABLED_TOOLTIP
                                    } else {
                                        "Viewed: also marks the files viewed on GitHub once all \
                                         their steps are viewed"
                                    }))
                                    .on_click(cx.listener(
                                        move |this, state: &ToggleState, _, cx| {
                                            this.toggle_step_viewed(
                                                index,
                                                *state == ToggleState::Selected,
                                                cx,
                                            )
                                        },
                                    )),
                            ),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .child(
                                Label::new(step.title.clone())
                                    .truncate()
                                    .when(viewed, |label| label.color(Color::Muted)),
                            )
                            .when(state == ViewedState::ChangedSinceViewed, |this| {
                                this.child(
                                    div()
                                        .debug_selector(move || {
                                            format!("photon-review-step-changed-{index}")
                                        })
                                        .child(
                                            Label::new("Changed since you viewed it")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Accent),
                                        ),
                                )
                            })
                            .when(selected, |this| {
                                this.child(
                                    Label::new(step.subtitle.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            }),
                    )
                    .children(comment_badge(
                        format!("photon-review-step-comments-{index}"),
                        self.comment_count(&step.files),
                    )),
            );
            if !selected {
                continue;
            }
            for path in &step.files {
                let Some(file) = self.files.iter().find(|file| file.diff.path == *path) else {
                    continue;
                };
                let (added, removed) = file.diff.line_counts();
                let (icon, color) = match file.diff.status {
                    FileStatus::Added => (IconName::SquarePlus, Color::VersionControlAdded),
                    FileStatus::Deleted => (IconName::SquareMinus, Color::VersionControlDeleted),
                    FileStatus::Modified | FileStatus::Renamed { .. } => {
                        (IconName::SquareDot, Color::VersionControlModified)
                    }
                };
                let selector = format!("photon-review-file-{icon:?}:{path}");
                map = map.child(
                    v_flex()
                        .pl_6()
                        .child(
                            h_flex()
                                .debug_selector(move || selector)
                                .gap_1p5()
                                .child(Icon::new(icon).size(IconSize::Small).color(color))
                                .child(div().min_w_0().flex_1().child(
                                    Label::new(path.clone()).size(LabelSize::Small).truncate(),
                                ))
                                .children(comment_badge(
                                    format!("photon-review-file-comments-{path}"),
                                    self.comment_count(std::slice::from_ref(path)),
                                ))
                                .child(
                                    ui::DiffStat::new(
                                        SharedString::from(format!("photon-review-stat-{path}")),
                                        added,
                                        removed,
                                    )
                                    .label_size(LabelSize::XSmall)
                                    .tooltip(status_label(&file.diff)),
                                ),
                        )
                        .children(self.file_errors.get(path).map(|error| {
                            Label::new(error.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Error)
                        })),
                );
            }
            for &function_index in &step.functions {
                if let Some(function) = self.functions.get(function_index) {
                    map = map.child(self.render_function_row(function_index, &function.name, cx));
                }
            }
            let mut neighbours: Vec<&str> = Vec::new();
            for excerpt in &self.shown_context {
                if !neighbours.contains(&excerpt.name.as_str()) {
                    neighbours.push(&excerpt.name);
                }
            }
            for name in neighbours {
                map = map.child(
                    h_flex()
                        .id(SharedString::from(format!(
                            "photon-review-neighbour-{name}"
                        )))
                        .pl_6()
                        .gap_1p5()
                        .tooltip(Tooltip::text("Unchanged, pulled in by the call graph"))
                        .child(
                            div()
                                .size_1p5()
                                .rounded_full()
                                .bg(Color::Disabled.color(cx)),
                        )
                        .child(
                            Label::new(format!("{name}()"))
                                .size(LabelSize::Small)
                                .color(Color::Disabled),
                        ),
                );
            }
            let checklist_items = self
                .checklist_items
                .iter()
                .filter(|item| item.matches_any(&step.files))
                .count();
            if checklist_items > 0 {
                map = map.child(
                    div()
                        .pl_6()
                        .debug_selector(|| "photon-review-step-checklist".into())
                        .child(
                            Button::new(
                                "photon-review-step-checklist",
                                if checklist_items == 1 {
                                    "1 checklist item applies to these files".to_string()
                                } else {
                                    format!(
                                        "{checklist_items} checklist items apply to these files"
                                    )
                                },
                            )
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::XSmall)
                            .color(Color::Accent)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.checklist_open = true;
                                this.map_scroll.scroll_to_top_of_item(1);
                                cx.notify();
                            })),
                        ),
                );
            }
        }
        map
    }

    /// Open threads the code can't show: outdated ones, and ones on a whole file.
    fn render_unplaced_threads(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let threads: Vec<_> = self
            .detail
            .threads
            .iter()
            .filter(|thread| !thread.is_resolved && thread.line.is_none())
            .collect();
        if threads.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let count = threads.len();
        Some(
            v_flex()
                .debug_selector(|| "photon-review-unplaced-threads".into())
                .gap_1()
                .pb_2()
                .child(
                    Label::new(if count == 1 {
                        "1 comment not on a line of this diff".to_string()
                    } else {
                        format!("{count} comments not on a line of this diff")
                    })
                    .size(LabelSize::Small)
                    .color(Color::Accent),
                )
                .children(threads.into_iter().enumerate().map(|(index, thread)| {
                    let item = cx.weak_entity();
                    let thread_id = thread.id.clone();
                    let pending = self.resolving.contains(&thread.id);
                    v_flex()
                        .id(("photon-review-unplaced", index))
                        .gap_0p5()
                        .p_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border_variant)
                        .child(
                            Label::new(format!(
                                "{} · {}",
                                thread.path,
                                if thread.is_outdated {
                                    "outdated"
                                } else {
                                    "whole file"
                                }
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                        )
                        .children(thread.comments.iter().map(|comment| {
                            v_flex()
                                .child(
                                    Label::new(comment.author.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(comment.body.clone()).size(LabelSize::Small))
                        }))
                        .child(
                            h_flex().child(
                                crate::button(("photon-review-unplaced-resolve", index), "Resolve")
                                    .style(ButtonStyle::Subtle)
                                    .disabled(pending)
                                    .on_click(move |_, _, cx| {
                                        item.update(cx, |item, cx| {
                                            item.set_thread_resolved(&thread_id, true, cx)
                                                .detach_and_log_err(cx)
                                        })
                                        .log_err();
                                    }),
                            ),
                        )
                }))
                .into_any_element(),
        )
    }

    fn render_outdated_drafts(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let outdated: Vec<_> = self
            .drafts
            .iter()
            .zip(&self.draft_views)
            .filter(|(draft, _)| draft.outdated)
            .collect();
        if outdated.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let count = outdated.len();
        Some(
            v_flex()
                .gap_1()
                .pb_2()
                .child(
                    Label::new(format!(
                        "{count} outdated draft{}",
                        if count == 1 { "" } else { "s" }
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Warning),
                )
                .children(outdated.into_iter().map(|(draft, view)| {
                    let local_id = view.local_id;
                    let posted_note = self.posted_note(draft);
                    let (may_be_posted, general) = (posted_note.is_some(), draft.general);
                    let item = cx.weak_entity();
                    let menu = ui::PopoverMenu::new(("photon-review-outdated-menu", local_id))
                        .trigger_with_tooltip(
                            IconButton::new(
                                ("photon-review-outdated-more", local_id),
                                IconName::Ellipsis,
                            )
                            .icon_size(IconSize::Small),
                            Tooltip::text("What to do with this draft"),
                        )
                        .anchor(gpui::Anchor::TopRight)
                        .menu(move |window, cx| {
                            let item = item.clone();
                            Some(ui::ContextMenu::build(window, cx, move |menu, _, _| {
                                let replace = item.clone();
                                let include = item.clone();
                                let general_item = item.clone();
                                let delete = item.clone();
                                menu.entry("Place on the selected lines", None, move |_, cx| {
                                    replace
                                        .update(cx, |item, cx| item.replace_draft(local_id, cx))
                                        .log_err();
                                })
                                // A draft that may already be posted is included again before
                                // anything else can send it.
                                .when(may_be_posted, |menu| {
                                    menu.entry("Include again", None, move |_, cx| {
                                        include
                                            .update(cx, |item, cx| item.include_again(local_id, cx))
                                            .log_err();
                                    })
                                })
                                .when(!may_be_posted && !general, |menu| {
                                    menu.entry("Post as a general comment", None, move |_, cx| {
                                        general_item
                                            .update(cx, |item, cx| {
                                                item.post_as_general(local_id, cx)
                                            })
                                            .log_err();
                                    })
                                })
                                .separator()
                                .entry(
                                    "Delete draft",
                                    None,
                                    move |window, cx| {
                                        delete
                                            .update(cx, |item, cx| {
                                                item.cancel_draft(local_id, window, cx)
                                            })
                                            .log_err();
                                    },
                                )
                            }))
                        });
                    v_flex()
                        .id(("photon-review-outdated", local_id))
                        .gap_0p5()
                        .p_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border_variant)
                        .child(
                            h_flex()
                                .justify_between()
                                .gap_1()
                                .child(
                                    Label::new(format!("{}:{}", draft.path, draft.start_line))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                                .child(
                                    div()
                                        .debug_selector(move || {
                                            format!("photon-review-outdated-menu-{local_id}")
                                        })
                                        .child(menu),
                                ),
                        )
                        .child(
                            Label::new(draft.anchor_text.clone())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx),
                        )
                        .child(Label::new(draft.body.clone()).size(LabelSize::Small))
                        .when(general && !may_be_posted, |this| {
                            this.child(
                                Label::new("posts as a general comment")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                        .children(posted_note.map(|note| {
                            Label::new(note)
                                .size(LabelSize::XSmall)
                                .color(Color::Warning)
                        }))
                }))
                .into_any_element(),
        )
    }

    fn render_code(&self, cx: &Context<Self>) -> impl IntoElement {
        let status = match &self.context_state {
            ContextState::Finding | ContextState::Ready => None,
            ContextState::Unavailable(reason) => Some(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .child(
                        Label::new("no call information")
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(reason.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(reason.as_ref() == TRUST_REASON, |this| {
                        this.child(
                            crate::button("photon-review-trust", "Trust worktree…")
                                .style(ButtonStyle::Tinted(TintColor::Accent))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(ToggleWorktreeSecurity.boxed_clone(), cx)
                                }),
                        )
                    }),
            ),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .children(status)
            .children(self.step_notes().into_iter().map(|note| {
                div()
                    .px_2()
                    .py_1()
                    .child(Label::new(note).size(LabelSize::Small).color(Color::Muted))
            }))
            .children(self.render_hidden_by_view(cx))
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }

    /// The description, then comments, reviews and pushed commits in order, then a composer.
    fn render_conversation(
        &self,
        conversation: &Conversation,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let style = MarkdownStyle::themed(MarkdownFont::Editor, window, cx);
        let colors = cx.theme().colors();
        let date = |time: &str| time.get(..10).unwrap_or(time).to_string();
        let byline = |author: String, time: String| {
            h_flex()
                .gap_2()
                .child(Label::new(author).size(LabelSize::Small))
                .child(Label::new(time).size(LabelSize::XSmall).color(Color::Muted))
        };
        let description = v_flex()
            .debug_selector(|| "photon-review-description".into())
            .gap_2()
            .pb_4()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(self.detail.summary.author.clone())
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(Label::new("opened this pull request").color(Color::Muted)),
            )
            .child(if self.detail.body.trim().is_empty() {
                Label::new("No description provided.")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            } else {
                MarkdownElement::new(conversation.description.clone(), style.clone())
                    .into_any_element()
            });
        let events =
            conversation
                .events
                .iter()
                .flatten()
                .enumerate()
                .map(|(index, (event, body))| {
                    let row = match event {
                        TimelineEvent::Comment(comment) => v_flex()
                            .child(byline(comment.author.clone(), date(&comment.created_at))),
                        TimelineEvent::Review {
                            author,
                            verb,
                            submitted_at,
                            ..
                        } => v_flex().child(byline(format!("{author} {verb}"), date(submitted_at))),
                        TimelineEvent::Commit {
                            short_sha,
                            headline,
                        } => v_flex().child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::GitCommit)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(short_sha.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .buffer_font(cx),
                                )
                                .child(
                                    Label::new(headline.clone())
                                        .size(LabelSize::Small)
                                        .truncate(),
                                ),
                        ),
                    };
                    row.debug_selector(move || format!("photon-review-timeline-{index}"))
                        .children(
                            body.clone()
                                .map(|body| MarkdownElement::new(body, style.clone())),
                        )
                });
        let status = if let Some(error) = &conversation.error {
            Some(
                Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            )
        } else if conversation.events.is_none() {
            Some(
                Label::new("Loading conversation…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        } else {
            None
        };
        let has_text = !conversation.composer.read(cx).text(cx).trim().is_empty();
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                v_flex()
                    .id("photon-review-conversation-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .py_4()
                    .child(
                        // A comfortable line length for prose, however wide the pane.
                        v_flex()
                            .w_full()
                            .max_w(px(760.))
                            .gap_3()
                            .child(description)
                            .child(
                                Label::new("Activity")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .children(events)
                            .children(status),
                    ),
            )
            .child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .p_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.border)
                            .child(conversation.composer.clone()),
                    )
                    .children(conversation.post_error.clone().map(|error| {
                        div()
                            .debug_selector(|| "photon-review-comment-error".into())
                            .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    }))
                    .child(
                        h_flex().justify_end().child(
                            div()
                                .debug_selector(|| "photon-review-comment".into())
                                .child(
                                    crate::button("photon-review-comment", "Comment")
                                        .style(ButtonStyle::Tinted(TintColor::Accent))
                                        .disabled(!has_text || conversation.posting)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.post_comment(cx).detach_and_log_err(cx)
                                        })),
                                ),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_feedback(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.is_own_pr() {
            return None;
        }
        let open = open_feedback(&self.detail.threads);
        if open.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let mut section = v_flex().id("photon-review-feedback").pb_2().gap_1().child(
            Label::new(format!("Feedback · {} open", open.len()))
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
        for thread in open {
            let first = thread.comments.first();
            let summary = first
                .map(|comment| {
                    let line = comment.body.lines().next().unwrap_or_default();
                    format!("{}: {line}", comment.author)
                })
                .unwrap_or_default();
            let place = match thread.line {
                Some(line) => format!("{}:{line}", thread.path),
                None => format!("{} · outdated", thread.path),
            };
            let (show_id, edit_id, done_id) =
                (thread.id.clone(), thread.id.clone(), thread.id.clone());
            let finishing = self.finishing_threads.contains(&thread.id);
            let element_id =
                |name: &str| SharedString::from(format!("photon-feedback-{name}-{}", thread.id));
            let group = element_id("group");
            let (row_selector, edit_selector, done_selector) = (
                element_id("row").to_string(),
                element_id("edit").to_string(),
                element_id("done").to_string(),
            );
            section = section.child(
                h_flex()
                    .id(element_id("row"))
                    .group(group.clone())
                    .debug_selector(move || row_selector)
                    .gap_1()
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .tooltip(Tooltip::text("Show it in the diff"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.show_thread(&show_id, window, cx)
                    }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(summary).size(LabelSize::Small).truncate())
                            .child(
                                Label::new(place)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .child(
                        div().debug_selector(move || edit_selector).child(
                            IconButton::new(element_id("edit"), IconName::Pencil)
                                .icon_size(IconSize::Small)
                                .visible_on_hover(group.clone())
                                .tooltip(Tooltip::text("Edit the file"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_thread_file(&edit_id, window, cx)
                                })),
                        ),
                    )
                    .child(
                        div().debug_selector(move || done_selector).child(
                            IconButton::new(element_id("done"), IconName::Check)
                                .icon_size(IconSize::Small)
                                .visible_on_hover(group)
                                .disabled(finishing)
                                .tooltip(Tooltip::text("Reply \"Done\" and resolve the thread"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.mark_thread_done(&done_id, cx).detach_and_log_err(cx)
                                })),
                        ),
                    ),
            );
        }
        Some(section.into_any_element())
    }
}

fn mark_explanation(mark: CallerMark) -> &'static str {
    match mark {
        CallerMark::InThisPr => "This caller changed in this PR too",
        CallerMark::Unchanged => "This caller didn't change, and the change doesn't affect it",
        CallerMark::NotUpdated => "The signature changed and this caller wasn't updated",
        CallerMark::Check => "The body changed and this caller wasn't touched, so check it",
    }
}

impl Render for ReviewItem {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PhotonReview")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|this, _: &NextStep, window, cx| {
                this.select_step(this.selected + 1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousStep, window, cx| {
                this.select_step(this.selected.saturating_sub(1), window, cx)
            }))
            .on_action(cx.listener(Self::add_comment))
            .on_action(cx.listener(Self::toggle_changes_since_review))
            .child(self.render_header(cx))
            .children(self.merge_error.clone().map(|error| {
                div()
                    .debug_selector(|| "photon-review-merge-error".into())
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            }))
            .children(self.render_new_commits(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .on_drag_move(cx.listener(
                        |this, event: &DragMoveEvent<DraggedReviewMap>, _, cx| {
                            this.set_map_width(
                                event.event.position.x - event.bounds.left(),
                                event.bounds.size.width,
                                cx,
                            );
                        },
                    ))
                    .child(
                        div()
                            .relative()
                            .h_full()
                            .flex_none()
                            .child(self.render_map(cx))
                            .child(self.render_map_resize_handle(cx)),
                    )
                    .map(|this| match &self.conversation {
                        Some(conversation) if self.conversation_open => {
                            this.child(self.render_conversation(conversation, window, cx))
                        }
                        _ => this.child(self.render_code(cx).into_any_element()),
                    }),
            )
    }
}

#[derive(Clone)]
struct DraggedReviewMap;

impl Render for DraggedReviewMap {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// The Submit review popover: verdict, summary, and what will and won't be sent.
struct SubmitPopover {
    item: WeakEntity<ReviewItem>,
    focus_handle: FocusHandle,
    _observe_item: Subscription,
}

impl SubmitPopover {
    fn new(item: Entity<ReviewItem>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = item.read(cx).summary_editor.focus_handle(cx);
        Self {
            _observe_item: cx.observe_in(&item, window, |_, _, _, cx| cx.notify()),
            item: item.downgrade(),
            focus_handle,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let item = self.item.clone();
        let Ok(submitting) = item.update(cx, |item, cx| {
            let verdict = item.verdict();
            item.submit(verdict, window, cx)
        }) else {
            return;
        };
        // Detached, so closing the popover never cancels a submit under way. A failure stays in
        // the popover, which stays open.
        cx.spawn_in(window, async move |this, cx| {
            submitting.await?;
            item.update(cx, |item, cx| item.report_submitted(cx))?;
            this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

impl EventEmitter<DismissEvent> for SubmitPopover {}

impl Focusable for SubmitPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SubmitPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(item) = self.item.upgrade() else {
            return div().into_any_element();
        };
        let item = item.read(cx);
        let verdict = item.verdict();
        let choices: &[(ReviewEvent, &'static str, &'static str)] = if item.is_own_pr() {
            &[]
        } else {
            &[
                (
                    ReviewEvent::Comment,
                    "Comment",
                    "Post your comments without a verdict",
                ),
                (ReviewEvent::Approve, "Approve", "Lets the author merge"),
                (
                    ReviewEvent::RequestChanges,
                    "Request changes",
                    "Blocks merging until you approve",
                ),
            ]
        };
        let submit_label = match verdict {
            ReviewEvent::Comment => "Comment",
            ReviewEvent::Approve => "Approve",
            ReviewEvent::RequestChanges => "Request changes",
        };
        let can_submit =
            !item.submitting && (verdict != ReviewEvent::Comment || item.can_comment(cx));
        let drafts = item.drafts.len();
        let checklist = item.checklist_progress();
        v_flex()
            .w(px(380.))
            .p_2()
            .gap_2()
            .elevation_2(cx)
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .child(
                div()
                    .p_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(item.summary_editor.clone()),
            )
            .when(!choices.is_empty(), |this| {
                this.child(v_flex().gap_1().children(choices.iter().map(
                    |&(event, label, description)| {
                        div()
                            .debug_selector(move || format!("photon-review-verdict-{label}"))
                            .child(
                                ui::ChoiceCard::radio(
                                    SharedString::from(format!(
                                        "photon-review-verdict-{}",
                                        event.as_str()
                                    )),
                                    label,
                                    verdict == event,
                                )
                                .description(description)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.item
                                            .update(cx, |item, cx| {
                                                item.verdict = event;
                                                cx.notify();
                                            })
                                            .log_err();
                                    },
                                )),
                            )
                    },
                )))
            })
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new(format!(
                            "{drafts} draft comment{} · {}",
                            if drafts == 1 { "" } else { "s" },
                            item.progress_text()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .when(checklist.total > 0, |this| {
                        this.child(
                            Label::new(format!("Checklist {}", checklist.text()))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .children(item.left_out_note().map(|note| {
                        Label::new(note)
                            .size(LabelSize::Small)
                            .color(Color::Warning)
                    }))
                    .children(
                        item.submit_error.clone().map(|error| {
                            Label::new(error).size(LabelSize::Small).color(Color::Error)
                        }),
                    ),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        crate::button("photon-review-submit-cancel", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        div()
                            .debug_selector(|| "photon-review-submit-confirm".into())
                            .child(
                                crate::button("photon-review-submit-confirm", submit_label)
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .disabled(!can_submit)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl EventEmitter<()> for ReviewItem {}

impl Focusable for ReviewItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Item for ReviewItem {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        let summary = &self.detail.summary;
        format!("#{} {}", summary.key.number, summary.title).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest))
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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "synchronous fixture setup")]
mod tests {
    use super::*;
    use crate::{
        FakeGh, FileStatus, PrKey, ReviewDb, build_loaded_file, keep_changes_message, parse_detail,
    };
    use base64::Engine as _;
    use editor::MultiBufferOffset;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use language::FakeLspAdapter;
    use project::Project;
    use project::trusted_worktrees::PathTrust;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use util::path;
    use workspace::MultiWorkspace;

    const DETAIL: &str = include_str!("../test_fixtures/detail.json");
    const AUTH: &str =
        r#"{"hosts":{"github.com":[{"state":"success","active":true,"login":"alice"}]}}"#;

    struct GraphFunction {
        name: &'static str,
        path: &'static str,
        start: u32,
        end: u32,
    }

    struct GraphCall {
        caller: &'static str,
        callee: &'static str,
        row: u32,
    }

    struct Graph {
        functions: Vec<GraphFunction>,
        calls: Vec<GraphCall>,
    }

    struct PrFile {
        path: &'static str,
        base: Option<&'static str>,
        status: FileStatus,
        hunks: Vec<crate::Hunk>,
    }

    fn function(name: &'static str, path: &'static str, start: u32, end: u32) -> GraphFunction {
        GraphFunction {
            name,
            path,
            start,
            end,
        }
    }

    fn call(caller: &'static str, callee: &'static str, row: u32) -> GraphCall {
        GraphCall {
            caller,
            callee,
            row,
        }
    }

    fn modified(path: &'static str, base: &'static str) -> PrFile {
        PrFile {
            path,
            base: Some(base),
            status: FileStatus::Modified,
            hunks: Vec::new(),
        }
    }

    fn uri(path: &str) -> lsp::Uri {
        lsp::Uri::from_file_path(format!("{}/{path}", path!("/repo"))).expect("uri")
    }

    fn lsp_item(function: &GraphFunction) -> lsp::CallHierarchyItem {
        lsp::CallHierarchyItem {
            name: function.name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: uri(function.path),
            range: lsp::Range::new(
                lsp::Position::new(function.start, 0),
                lsp::Position::new(function.end, 1),
            ),
            selection_range: lsp::Range::new(
                lsp::Position::new(function.start, 3),
                lsp::Position::new(function.start, 3 + function.name.len() as u32),
            ),
            data: None,
        }
    }

    fn call_range(row: u32) -> lsp::Range {
        lsp::Range::new(lsp::Position::new(row, 4), lsp::Position::new(row, 10))
    }

    type Gate = futures::future::Shared<futures::channel::oneshot::Receiver<()>>;

    fn answer_call_hierarchy(
        server: &mut lsp::FakeLanguageServer,
        graph: &Arc<Graph>,
        gate: Gate,
        prepares: &Arc<AtomicUsize>,
    ) {
        server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            let graph = graph.clone();
            let prepares = prepares.clone();
            move |params, _| {
                prepares.fetch_add(1, Ordering::SeqCst);
                let position = params.text_document_position_params;
                let item = graph
                    .functions
                    .iter()
                    .find(|function| {
                        uri(function.path) == position.text_document.uri
                            && function.start <= position.position.line
                            && position.position.line <= function.end
                    })
                    .map(lsp_item);
                let gate = gate.clone();
                async move {
                    gate.await.ok();
                    Ok(item.map(|item| vec![item]))
                }
            }
        });
        server.set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
            let graph = graph.clone();
            move |params, _| {
                let calls = graph
                    .calls
                    .iter()
                    .filter(|call| call.caller == params.item.name)
                    .filter_map(|call| {
                        let callee = graph.functions.iter().find(|f| f.name == call.callee)?;
                        Some(lsp::CallHierarchyOutgoingCall {
                            to: lsp_item(callee),
                            from_ranges: vec![call_range(call.row)],
                        })
                    })
                    .collect();
                async move { Ok(Some(calls)) }
            }
        });
        server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
            let graph = graph.clone();
            move |params, _| {
                let calls = graph
                    .calls
                    .iter()
                    .filter(|call| call.callee == params.item.name)
                    .filter_map(|call| {
                        let caller = graph.functions.iter().find(|f| f.name == call.caller)?;
                        Some(lsp::CallHierarchyIncomingCall {
                            from: lsp_item(caller),
                            from_ranges: vec![call_range(call.row)],
                        })
                    })
                    .collect();
                async move { Ok(Some(calls)) }
            }
        });
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    fn key() -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "web".into(),
            number: 482,
        }
    }

    fn checkout() -> Checkout {
        Checkout {
            repo_dir: PathBuf::from(path!("/repo")),
            worktree: PathBuf::from(path!("/repo")),
            head_sha: "head".into(),
            merge_base: "base".into(),
            head_changed: false,
        }
    }

    /// Builds a review over `tree`, where `files` are the PR's changed files. `call_hierarchy`
    /// false gives a server without the capability; `gate` holds every prepare request until it
    /// resolves.
    async fn open_item(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
        files: Vec<PrFile>,
        graph: Graph,
        call_hierarchy: bool,
        gate: Option<Gate>,
    ) -> (Entity<ReviewItem>, VisualTestContext) {
        let (item, cx, _) =
            open_item_with(cx, tree, files, graph, call_hierarchy, gate, false, None).await;
        (item, cx)
    }

    static DATABASE_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn test_database(cx: &mut TestAppContext) -> ReviewDb {
        let name = format!(
            "review_item_{}",
            DATABASE_COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        cx.foreground_executor()
            .block_test(ReviewDb::open_test_db(name.leak()))
    }

    /// `restricted` opens the project with worktree trust enforced and the worktree untrusted.
    /// Also returns the number of call-hierarchy prepare requests the server received.
    async fn open_item_with(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
        files: Vec<PrFile>,
        graph: Graph,
        call_hierarchy: bool,
        gate: Option<Gate>,
        restricted: bool,
        database: Option<ReviewDb>,
    ) -> (Entity<ReviewItem>, VisualTestContext, Arc<AtomicUsize>) {
        let detail = parse_detail(&key(), DETAIL).expect("detail");
        open_item_with_detail(
            cx,
            tree,
            files,
            graph,
            call_hierarchy,
            gate,
            restricted,
            database,
            detail,
        )
        .await
    }

    async fn open_item_with_detail(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
        files: Vec<PrFile>,
        graph: Graph,
        call_hierarchy: bool,
        gate: Option<Gate>,
        restricted: bool,
        database: Option<ReviewDb>,
        detail: PrDetail,
    ) -> (Entity<ReviewItem>, VisualTestContext, Arc<AtomicUsize>) {
        init_test(cx);
        let database = database.unwrap_or_else(|| test_database(cx));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/repo"), tree).await;
        let project = if restricted {
            cx.update(|cx| project::trusted_worktrees::init(Default::default(), cx));
            Project::test_with_worktree_trust(fs, [path!("/repo").as_ref()], cx).await
        } else {
            Project::test(fs, [path!("/repo").as_ref()], cx).await
        };
        let prepares = Arc::new(AtomicUsize::new(0));
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        registry.add(language::rust_lang());
        let graph = Arc::new(graph);
        let gate = gate.unwrap_or_else(|| {
            let (release, gate) = futures::channel::oneshot::channel();
            release.send(()).expect("open gate");
            gate.shared()
        });
        registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    call_hierarchy_provider: call_hierarchy
                        .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..lsp::ServerCapabilities::default()
                },
                initializer: Some(Box::new({
                    let prepares = prepares.clone();
                    move |server| answer_call_hierarchy(server, &graph, gate.clone(), &prepares)
                })),
                ..FakeLspAdapter::default()
            },
        );
        let mut loaded = Vec::new();
        for file in files {
            let diff = FileDiff {
                path: file.path.to_string(),
                status: file.status,
                binary: false,
                submodule: false,
                symlink: false,
                hunks: file.hunks,
            };
            let task = cx.update(|cx| {
                build_loaded_file(
                    &project,
                    PathBuf::from(path!("/repo")).join(file.path),
                    Path::new(path!("/repo")),
                    "head",
                    file.base.map(str::to_string),
                    diff,
                    cx,
                )
            });
            loaded.push(task.await.expect("loaded file"));
        }
        let window = cx.add_window(|window, cx| {
            ReviewItem::new(
                project.clone(),
                detail,
                "alice".into(),
                checkout(),
                loaded,
                database,
                window,
                cx,
            )
        });
        let item = window.root(cx).expect("item");
        let cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        (item, cx, prepares)
    }

    /// Lets the language server start and every call-hierarchy retry run.
    fn settle(cx: &mut VisualTestContext) {
        for _ in 0..5 {
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
        }
    }

    fn text(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> String {
        item.read_with(cx, |item, cx| item.editor().read(cx).text(cx))
    }

    fn step_keys(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> Vec<String> {
        item.read_with(cx, |item, _| {
            item.steps().iter().map(|step| step.key.clone()).collect()
        })
    }

    const LOGIN_BASE: &str = "\
fn login(user: &str, password: &str) -> bool {
    verify_password(user, password)
}
";
    const LOGIN_HEAD: &str = "\
fn login(user: &str, password: &str) -> bool {
    let user = user.trim();
    audit(user);
    verify_password(user, password)
}
";
    const PASSWORD: &str = "\
fn verify_password(user: &str, password: &str) -> bool {
    user == password
}
";

    fn login_graph() -> Graph {
        Graph {
            functions: vec![
                function("login", "src/login.rs", 0, 4),
                function("verify_password", "src/password.rs", 0, 2),
            ],
            calls: vec![call("login", "verify_password", 3)],
        }
    }

    fn login_tree() -> serde_json::Value {
        json!({ "src": { "login.rs": LOGIN_HEAD, "password.rs": PASSWORD } })
    }

    #[gpui::test]
    async fn workspace_shows_changed_hunk_then_unchanged_callee(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(
                gpui::AvailableSpace::Definite(px(1200.)),
                gpui::AvailableSpace::Definite(px(800.)),
            ),
            |_, _| item.clone().into_any_element(),
        );
        let text = text(&item, &mut cx);
        let hunk = text.find("let user = user.trim();").expect("hunk shown");
        let callee = text.find("user == password").expect("callee shown");
        assert!(hunk < callee, "{text}");
        item.read_with(&cx, |item, _| {
            assert_eq!(item.context_state(), &ContextState::Ready);
            let labels: Vec<_> = item
                .shown_context
                .iter()
                .map(|excerpt| excerpt.label.as_str())
                .collect();
            assert_eq!(
                labels,
                vec!["unchanged · src/password.rs · verify_password() · called on line 4"]
            );
            assert_eq!(item.label_blocks.len(), 1);
            let titles: Vec<_> = item.steps().iter().map(|step| step.title.clone()).collect();
            assert_eq!(titles, vec!["login.rs · login()"]);
        });
    }

    #[gpui::test]
    async fn diff_shows_before_callers_arrive(cx: &mut TestAppContext) {
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            Some(gate.shared()),
        )
        .await;
        settle(&mut cx);
        assert!(text(&item, &mut cx).contains("let user = user.trim();"));
        assert!(!text(&item, &mut cx).contains("user == password"));
        item.read_with(&cx, |item, _| {
            assert_eq!(item.context_state(), &ContextState::Finding)
        });

        release.send(()).expect("release");
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.context_state(), &ContextState::Ready)
        });
        assert!(text(&item, &mut cx).contains("user == password"));
    }

    #[gpui::test]
    async fn no_call_hierarchy_is_unavailable(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            false,
            None,
        )
        .await;
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert!(matches!(item.context_state(), ContextState::Unavailable(_)));
        });
        assert!(text(&item, &mut cx).contains("let user = user.trim();"));
    }

    const A_BASE: &str = "fn caller() {\n    callee();\n}\n";
    const A_HEAD: &str = "fn caller() {\n    callee();\n    callee();\n}\n";
    const B_BASE: &str = "fn callee() {\n    let a = 1;\n}\n";
    const B_HEAD: &str = "fn callee() {\n    let b = 2;\n}\n";

    fn two_file_graph() -> Graph {
        Graph {
            functions: vec![
                function("caller", "src/a.rs", 0, 3),
                function("callee", "src/b.rs", 0, 2),
            ],
            calls: vec![call("caller", "callee", 1), call("caller", "callee", 2)],
        }
    }

    #[gpui::test]
    async fn selecting_step_replaces_excerpts(cx: &mut TestAppContext) {
        let graph = Graph {
            functions: two_file_graph().functions,
            calls: Vec::new(),
        };
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "a.rs": A_HEAD, "b.rs": B_HEAD } }),
            vec![modified("src/a.rs", A_BASE), modified("src/b.rs", B_BASE)],
            graph,
            true,
            None,
        )
        .await;
        settle(&mut cx);
        assert_eq!(
            step_keys(&item, &mut cx),
            vec!["src/a.rs::caller", "src/b.rs::callee"]
        );
        assert!(text(&item, &mut cx).contains("fn caller"));
        assert!(!text(&item, &mut cx).contains("let b = 2"));

        item.update_in(&mut cx, |item, window, cx| item.select_step(1, window, cx));
        cx.run_until_parked();
        assert!(text(&item, &mut cx).contains("let b = 2"));
        assert!(!text(&item, &mut cx).contains("fn caller"));

        item.update_in(&mut cx, |item, window, cx| {
            window.focus(&item.editor().focus_handle(cx), cx)
        });
        cx.dispatch_action(PreviousStep);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(item.selected_step(), 0));
        assert!(text(&item, &mut cx).contains("fn caller"));
        cx.dispatch_action(NextStep);
        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(item.selected_step(), 1));
    }

    #[gpui::test]
    async fn map_reorders_when_edges_arrive(cx: &mut TestAppContext) {
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "a.rs": A_HEAD, "b.rs": B_HEAD } }),
            vec![modified("src/a.rs", A_BASE), modified("src/b.rs", B_BASE)],
            two_file_graph(),
            true,
            Some(gate.shared()),
        )
        .await;
        settle(&mut cx);
        assert_eq!(
            step_keys(&item, &mut cx),
            vec!["src/a.rs::caller", "src/b.rs::callee"]
        );
        item.update_in(&mut cx, |item, window, cx| item.select_step(1, window, cx));

        release.send(()).expect("release");
        settle(&mut cx);
        assert_eq!(
            step_keys(&item, &mut cx),
            vec!["src/b.rs::callee", "src/a.rs::caller"]
        );
        item.read_with(&cx, |item, _| {
            assert_eq!(item.selected_step(), 0);
            assert_eq!(item.context_state(), &ContextState::Ready);
        });
        assert!(text(&item, &mut cx).contains("let b = 2"));
    }

    #[gpui::test]
    async fn impact_marks_callers(cx: &mut TestAppContext) {
        let graph = Graph {
            functions: vec![
                function("target", "src/target.rs", 0, 2),
                function("uses_new", "src/a.rs", 0, 2),
                function("old_user", "src/c.rs", 0, 2),
            ],
            calls: vec![call("uses_new", "target", 1), call("old_user", "target", 1)],
        };
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": {
                "target.rs": "fn target(a: u32, b: u32) -> u32 {\n    a + b\n}\n",
                "a.rs": "fn uses_new() -> u32 {\n    target(1, 2)\n}\n",
                "c.rs": "fn old_user() -> u32 {\n    target(1)\n}\n",
            } }),
            vec![
                modified("src/target.rs", "fn target(a: u32) -> u32 {\n    a\n}\n"),
                modified("src/a.rs", "fn uses_new() -> u32 {\n    target(1)\n}\n"),
            ],
            graph,
            true,
            None,
        )
        .await;
        settle(&mut cx);
        assert_eq!(
            step_keys(&item, &mut cx),
            vec!["src/target.rs::target", "src/a.rs::uses_new"]
        );
        // The selected step keeps its key when edges arrive, so it moved to the second step.
        item.update_in(&mut cx, |item, window, cx| item.select_step(0, window, cx));
        item.read_with(&cx, |item, _| {
            let target = item
                .functions
                .iter()
                .position(|function| function.name == "target")
                .expect("target");
            assert_eq!(
                item.callers(target),
                &[
                    ImpactRow {
                        location: "src/a.rs:2".into(),
                        mark: CallerMark::InThisPr,
                    },
                    ImpactRow {
                        location: "src/c.rs:2".into(),
                        mark: CallerMark::NotUpdated,
                    },
                ]
            );
        });
        assert!(drawn("photon-review-not-updated-target", &mut cx));
        assert!(!drawn(
            "photon-review-caller-src/c.rs:2-not updated",
            &mut cx
        ));
        click("photon-review-function-target", &mut cx);
        assert!(drawn("photon-review-caller-src/a.rs:2-in this PR", &mut cx));
        assert!(drawn(
            "photon-review-caller-src/c.rs:2-not updated",
            &mut cx
        ));
    }

    #[gpui::test]
    async fn function_rows_wait_for_callers_without_a_note(cx: &mut TestAppContext) {
        let (release, gate) = futures::channel::oneshot::channel();
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![login_with_hunk()],
            login_graph(),
            true,
            Some(gate.shared()),
        )
        .await;
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.context_state(), &ContextState::Finding)
        });
        assert!(drawn("photon-review-function-login", &mut cx));
        click("photon-review-function-login", &mut cx);
        assert!(!drawn("photon-review-no-callers", &mut cx));
        release.send(()).expect("release");
        settle(&mut cx);
        click("photon-review-function-login", &mut cx);
        assert!(drawn("photon-review-no-callers", &mut cx));
    }

    #[gpui::test]
    async fn long_unchanged_excerpt_collapses_and_expands(cx: &mut TestAppContext) {
        let long: String = std::iter::once("fn long() {\n".to_string())
            .chain((1..=10).map(|row| format!("    let row{row} = {row};\n")))
            .chain(std::iter::once("}\n".to_string()))
            .collect();
        let graph = Graph {
            functions: vec![
                function("main", "src/main.rs", 0, 3),
                function("long", "src/long.rs", 0, 11),
            ],
            calls: vec![call("main", "long", 2)],
        };
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": {
                "main.rs": "fn main() {\n    let a = 2;\n    long();\n}\n",
                "long.rs": long,
            } }),
            vec![modified(
                "src/main.rs",
                "fn main() {\n    let a = 1;\n    long();\n}\n",
            )],
            graph,
            true,
            None,
        )
        .await;
        settle(&mut cx);
        let rows = |item: &ReviewItem| {
            item.shown_context
                .iter()
                .map(|excerpt| excerpt.range.end.row - excerpt.range.start.row + 1)
                .collect::<Vec<_>>()
        };
        item.read_with(&cx, |item, _| {
            assert_eq!(rows(item), vec![8]);
            assert!(
                item.shown_context[0]
                    .label
                    .starts_with("unchanged · src/long.rs · long() · called on line 3"),
            );
        });
        assert!(text(&item, &mut cx).contains("let row7 = 7;"));
        assert!(!text(&item, &mut cx).contains("let row8 = 8;"));

        item.update(&mut cx, |item, cx| item.expand_context(0, cx));
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(rows(item), vec![12]));
        assert!(text(&item, &mut cx).contains("let row10 = 10;"));
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct GitFixture {
        _temp: tempfile::TempDir,
        base: PathBuf,
        upstream: PathBuf,
        seed: PathBuf,
        user: PathBuf,
        upstream_url: String,
        pr_sha: String,
    }

    impl GitFixture {
        /// An upstream with `main` and PR #482, and a user clone whose remote names
        /// github.com/acme/web and whose work tree has an uncommitted edit.
        fn new() -> Option<Self> {
            if Command::new("git").arg("--version").output().is_err() {
                return None;
            }
            let temp = tempfile::tempdir().expect("tempdir");
            let base = temp.path().canonicalize().expect("canonical");
            let upstream = base.join("upstream.git");
            let seed = base.join("seed");
            let user = base.join("user");
            std::fs::create_dir_all(&seed).expect("seed dir");
            run(
                &base,
                &["init", "-q", "--bare", "-b", "main", "upstream.git"],
            );
            run(&seed, &["init", "-q", "-b", "main"]);
            std::fs::write(seed.join("a.rs"), "fn a() {}\n").expect("write");
            run(&seed, &["add", "."]);
            run(&seed, &["commit", "-q", "-m", "base"]);
            run(&seed, &["checkout", "-q", "-b", "pr"]);
            let upstream_url = format!("file://{}", upstream.display());
            run(&base, &["clone", "-q", &upstream_url, "user"]);
            let mut fixture = Self {
                _temp: temp,
                base,
                upstream,
                seed,
                user,
                upstream_url,
                pr_sha: String::new(),
            };
            fixture.push_pr_head("fn a() {\n    1;\n}\n");
            run(&fixture.user, &["fetch", "-q", "origin"]);
            run(
                &fixture.user,
                &["remote", "set-url", "origin", "git@github.com:acme/web.git"],
            );
            std::fs::write(fixture.user.join("a.rs"), "dirty\n").expect("write");
            Some(fixture)
        }

        fn push_pr_head(&mut self, contents: &str) {
            std::fs::write(self.seed.join("a.rs"), contents).expect("write");
            run(&self.seed, &["commit", "-q", "-am", "pr"]);
            self.pr_sha = run(&self.seed, &["rev-parse", "HEAD"]);
            run(
                &self.seed,
                &["push", "-q", "-f", &self.upstream_url, "main", "pr"],
            );
            run(
                &self.upstream,
                &["update-ref", "refs/pull/482/head", &self.pr_sha],
            );
        }

        async fn prepare(&self, fake: &Arc<FakeGh>, database: &ReviewDb) -> Result<PreparedReview> {
            fake.respond(|request| request.args[0] == "auth", Ok(AUTH.into()));
            fake.respond(
                |request| {
                    request
                        .stdin
                        .as_deref()
                        .is_some_and(|stdin| stdin.contains("pullRequest"))
                },
                Ok(DETAIL.into()),
            );
            let summary = parse_detail(&key(), DETAIL).expect("detail").summary;
            prepare_review(
                &summary,
                self.upstream_url.clone(),
                self.base.join("root"),
                vec![self.user.clone()],
                fake.clone(),
                database.clone(),
            )
            .await
        }
    }

    #[gpui::test]
    async fn open_review_leaves_user_clone_untouched(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = GitFixture::new() else {
            return;
        };
        let user = &fixture.user;
        let user_state = || {
            vec![
                run(user, &["symbolic-ref", "HEAD"]),
                run(user, &["status", "--porcelain=v1", "-uall"]),
                run(user, &["stash", "list"]),
                format!("{:?}", std::fs::read(user.join("a.rs")).ok()),
            ]
        };
        let before = user_state();

        let fake = FakeGh::new();
        let database = cx.foreground_executor().block_test(ReviewDb::open_test_db(
            "open_review_leaves_user_clone_untouched",
        ));
        let prepared = fixture.prepare(&fake, &database).await.expect("prepared");

        assert_eq!(user_state(), before);
        assert_eq!(prepared.checkout.head_sha, fixture.pr_sha);
        assert_eq!(prepared.checkout.repo_dir, user.join(".git"));
        assert!(
            prepared
                .checkout
                .worktree
                .starts_with(fixture.base.join("root"))
        );
        assert_eq!(prepared.detail.summary.key, key());
        let paths: Vec<_> = prepared
            .files
            .iter()
            .map(|(diff, base)| (diff.path.as_str(), base.as_deref()))
            .collect();
        assert_eq!(paths, vec![("a.rs", Some("fn a() {}\n"))]);
        let tracked = database.tracked().expect("tracked");
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].worktree, prepared.checkout.worktree);
    }

    #[gpui::test]
    async fn logged_out_fails_before_checkout(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let fake = FakeGh::new();
        fake.respond(
            |request| request.args[0] == "auth",
            Ok(include_str!("../test_fixtures/auth_status_logged_out.json").into()),
        );
        let database = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("logged_out_fails_before_checkout"));
        let summary = parse_detail(&key(), DETAIL).expect("detail").summary;
        let error = prepare_review(
            &summary,
            "file:///nowhere".into(),
            root.clone(),
            Vec::new(),
            fake.clone(),
            database.clone(),
        )
        .await
        .err()
        .expect("logged out");
        assert!(error.to_string().contains("Not logged in"), "{error:#}");
        assert_eq!(fake.requests().len(), 1);
        assert!(!root.exists());
        assert!(database.tracked().expect("tracked").is_empty());
    }

    #[gpui::test]
    async fn open_review_from_inside_a_workspace_update_does_not_panic(cx: &mut TestAppContext) {
        init_test(cx);
        let fake = FakeGh::new();
        fake.respond(
            |request| request.args[0] == "auth",
            Ok(include_str!("../test_fixtures/auth_status_logged_out.json").into()),
        );
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            crate::set_gh_runner(fake.clone(), cx);
        });
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        cx.update(|cx| AppState::set_global(workspace.read(cx).app_state().clone(), cx));
        let summary = parse_detail(&key(), DETAIL).expect("detail").summary;

        // The inbox's Review button calls open_review while the workspace is being updated.
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let task = workspace.update_in(&mut visual, |workspace, window, cx| {
            open_review(summary, workspace, window, cx)
        });
        let error = task.await.err().expect("logged out");
        assert!(error.to_string().contains("Not logged in"), "{error:#}");
    }

    #[gpui::test]
    async fn photon_review_opens_as_tab_in_current_window(cx: &mut TestAppContext) {
        init_test(cx);
        let app_state = cx.update(AppState::test);
        cx.update(|cx| AppState::set_global(app_state.clone(), cx));
        let fs = app_state.fs.as_fake();
        fs.insert_tree(path!("/repo"), json!({ "a.rs": "" })).await;
        fs.insert_tree(path!("/reviews/pr-1"), json!({ "a.rs": "" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/repo").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        cx.run_until_parked();

        let opened = open_checkout(
            Path::new(path!("/reviews/pr-1")),
            app_state.clone(),
            &mut cx.to_async(),
        )
        .await
        .expect("opened");
        cx.run_until_parked();

        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        assert_eq!(opened.window, window);
        window
            .read_with(cx, |multi_workspace, cx| {
                assert_eq!(multi_workspace.workspaces().count(), 2);
                assert_eq!(multi_workspace.workspace(), &opened.workspace);
                let roots = opened.workspace.read(cx).root_paths(cx);
                assert_eq!(
                    roots.first().map(|root| root.as_ref()),
                    Some(Path::new(path!("/reviews/pr-1")))
                );
            })
            .expect("window");
    }

    #[gpui::test]
    async fn photon_show_tab_activates_background_review(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let review_project = Project::test(fs.clone(), [], cx).await;
        let other_project = Project::test(fs, [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(review_project, window, cx));
        let review_workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("window");
        window
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(other_project, window, cx);
            })
            .expect("window");

        // The callers hold an untyped handle, which leaves the window's root free to update.
        let any_window: AnyWindowHandle = window.into();
        any_window
            .update(cx, |_, window_context, cx| {
                show_tab(&review_workspace, window_context, cx)
            })
            .expect("window");
        cx.run_until_parked();

        window
            .read_with(cx, |multi_workspace, _| {
                assert_eq!(multi_workspace.workspace(), &review_workspace)
            })
            .expect("window");
    }

    #[gpui::test]
    async fn reopening_after_new_head_replaces_the_review(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let Some(mut fixture) = GitFixture::new() else {
            return;
        };
        init_test(cx);
        let fake = FakeGh::new();
        let database = cx.foreground_executor().block_test(ReviewDb::open_test_db(
            "reopening_after_new_head_replaces_the_review",
        ));
        let prepared = fixture.prepare(&fake, &database).await.expect("first");
        let worktree = prepared.checkout.worktree.clone();
        let fs = fs::RealFs::new(None, cx.executor());
        let project = Project::test(fs, [worktree.as_path()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        place_review(
            workspace.clone(),
            window.into(),
            prepared,
            database.clone(),
            &mut cx.to_async(),
        )
        .await
        .expect("placed");

        let first_head = fixture.pr_sha.clone();
        fixture.push_pr_head("fn a() {\n    2;\n}\n");
        assert_ne!(fixture.pr_sha, first_head);
        let prepared = fixture.prepare(&fake, &database).await.expect("second");
        assert_eq!(prepared.checkout.head_sha, fixture.pr_sha);
        place_review(
            workspace.clone(),
            window.into(),
            prepared,
            database.clone(),
            &mut cx.to_async(),
        )
        .await
        .expect("replaced");

        let items = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ReviewItem>(cx)
                .collect::<Vec<_>>()
        });
        assert_eq!(items.len(), 1);
        let (head, text) = items[0].read_with(cx, |item, cx| {
            (
                item.checkout.head_sha.clone(),
                item.editor().read(cx).text(cx),
            )
        });
        assert_eq!(head, fixture.pr_sha);
        assert!(text.contains("    2;"), "{text}");
        assert!(!text.contains("    1;"), "{text}");
    }

    /// A window whose one tab is the user's clone, set up so `open_review_from` runs for real
    /// against the fixture's local upstream.
    struct ReloadSetup {
        fixture: GitFixture,
        app_state: Arc<AppState>,
        fake: Arc<FakeGh>,
        database: ReviewDb,
        window: gpui::WindowHandle<MultiWorkspace>,
        user_workspace: Entity<Workspace>,
        /// The head the inbox shows, when it lags behind GitHub.
        /// What GitHub answers for the PR.
        detail: String,
        inbox_head: Option<String>,
        cx: VisualTestContext,
    }

    async fn reload_setup(name: &'static str, cx: &mut TestAppContext) -> Option<ReloadSetup> {
        cx.executor().allow_parking();
        let fixture = GitFixture::new()?;
        init_test(cx);
        let test_state = cx.update(AppState::test);
        let app_state = Arc::new(AppState {
            languages: test_state.languages.clone(),
            client: test_state.client.clone(),
            user_store: test_state.user_store.clone(),
            workspace_store: test_state.workspace_store.clone(),
            fs: fs::RealFs::new(None, cx.executor()),
            build_window_options: test_state.build_window_options,
            node_runtime: test_state.node_runtime.clone(),
            session: test_state.session.clone(),
        });
        let fake = FakeGh::new();
        cx.update(|cx| {
            AppState::set_global(app_state.clone(), cx);
            cx.set_global(db::AppDatabase::test_new());
            crate::set_gh_runner(fake.clone(), cx);
        });
        init_trust(path!("/nowhere"), cx);
        let database = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db(name));
        let project = Project::test(app_state.fs.clone(), [fixture.user.as_path()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let user_workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("window");
        let cx = VisualTestContext::from_window(window.into(), cx);
        Some(ReloadSetup {
            fixture,
            app_state,
            fake,
            database,
            window,
            user_workspace,
            inbox_head: None,
            detail: DETAIL.to_string(),
            cx,
        })
    }

    impl ReloadSetup {
        fn summary(&self) -> PrSummary {
            let mut summary = parse_detail(&key(), &self.detail).expect("detail").summary;
            summary.head_sha = self
                .inbox_head
                .clone()
                .unwrap_or_else(|| self.fixture.pr_sha.clone());
            summary
        }

        fn worktree(&self) -> PathBuf {
            crate::worktree_path(&self.fixture.base.join("root"), &key())
        }

        /// Opens the PR as the inbox does; the fetch waits for `go` when one is given.
        fn open(
            &mut self,
            go: Option<futures::channel::oneshot::Receiver<()>>,
        ) -> Task<Result<()>> {
            self.fake
                .respond(|request| request.args[0] == "auth", Ok(AUTH.into()));
            self.fake.respond(
                |request| {
                    request
                        .stdin
                        .as_deref()
                        .is_some_and(|stdin| stdin.contains("pullRequest"))
                },
                Ok(self.detail.clone()),
            );
            let summary = self.summary();
            let fetch_url = self.fixture.upstream_url.clone();
            let root = self.fixture.base.join("root");
            let user = self.fixture.user.clone();
            let app_state = self.app_state.clone();
            let runner: Arc<dyn GhRunner> = self.fake.clone();
            let database = self.database.clone();
            self.user_workspace
                .update_in(&mut self.cx, |_, window, cx| {
                    let candidates = cx.background_spawn(async move {
                        if let Some(go) = go {
                            go.await.ok();
                        }
                        vec![user]
                    });
                    let source = CheckoutSource {
                        fetch_url,
                        root,
                        candidates,
                    };
                    cx.spawn_in(window, async move |_, cx| {
                        open_review_from(summary, source, app_state, runner, database, cx).await
                    })
                })
        }

        fn reviews(&mut self) -> Vec<Entity<ReviewItem>> {
            self.window
                .read_with(&self.cx, |multi_workspace, cx| {
                    multi_workspace
                        .workspaces()
                        .flat_map(|workspace| {
                            workspace
                                .read(cx)
                                .items_of_type::<ReviewItem>(cx)
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
                .expect("window")
        }

        fn review(&mut self) -> Entity<ReviewItem> {
            let reviews = self.reviews();
            assert_eq!(reviews.len(), 1, "one review of the PR");
            reviews[0].clone()
        }

        fn tab_roots(&mut self) -> Vec<PathBuf> {
            self.window
                .read_with(&self.cx, |multi_workspace, cx| {
                    multi_workspace
                        .workspaces()
                        .flat_map(|workspace| workspace.read(cx).root_paths(cx))
                        .map(|root| root.to_path_buf())
                        .collect()
                })
                .expect("window")
        }

        fn pr_workspace(&mut self) -> Entity<Workspace> {
            let worktree = self.worktree();
            self.window
                .read_with(&self.cx, |multi_workspace, cx| {
                    multi_workspace
                        .workspaces()
                        .find(|workspace| {
                            workspace
                                .read(cx)
                                .root_paths(cx)
                                .iter()
                                .any(|root| root.as_ref() == worktree)
                        })
                        .cloned()
                })
                .expect("window")
                .expect("the PR's tab")
        }

        async fn open_first_head(&mut self) -> Entity<ReviewItem> {
            self.open(None).await.expect("opened");
            self.cx.run_until_parked();
            let review = self.review();
            let head = review.read_with(&self.cx, |item, _| item.checkout.head_sha.clone());
            assert_eq!(head, self.fixture.pr_sha);
            review
        }
    }

    #[gpui::test]
    async fn new_commits_reopen_the_prs_tab(cx: &mut TestAppContext) {
        let Some(mut setup) = reload_setup("new_commits_reopen_the_prs_tab", cx).await else {
            return;
        };
        let first = setup.open_first_head().await;
        let worktree = setup.worktree();
        assert_eq!(
            setup.tab_roots(),
            vec![setup.fixture.user.clone(), worktree.clone()]
        );
        let old_head = setup.fixture.pr_sha.clone();
        let old_project = setup
            .pr_workspace()
            .read_with(&setup.cx, |workspace, _| workspace.project().clone());
        cx.update(|cx| {
            let worktree_store = old_project.read(cx).worktree_store();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| {
                    trusted.trust(
                        &worktree_store,
                        HashSet::from_iter([PathTrust::AbsPath(worktree.clone())]),
                        cx,
                    )
                })
        });
        assert!(worktree_trusted(&old_project, cx));
        drop(old_project);

        setup.fixture.push_pr_head("fn a() {\n    2;\n}\n");
        let (go, wait) = futures::channel::oneshot::channel();
        let opening = setup.open(Some(wait));
        setup.cx.run_until_parked();
        // The open review says what is happening before any git work.
        assert_eq!(
            first.read_with(&setup.cx, |item, _| item.new_commits().cloned()),
            Some(NewCommits::Loading)
        );
        assert!(drawn("photon-review-new-commits", &mut setup.cx));
        go.send(()).ok();
        opening.await.expect("loaded");
        setup.cx.run_until_parked();

        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        assert_eq!(
            setup.tab_roots(),
            vec![setup.fixture.user.clone(), worktree.clone()]
        );
        let review = setup.review();
        assert_ne!(
            review,
            first,
            "{:?}",
            first.read_with(&setup.cx, |item, _| item.new_commits().cloned())
        );
        let (head, text, new_commits) = review.read_with(&setup.cx, |item, cx| {
            (
                item.checkout.head_sha.clone(),
                item.editor().read(cx).text(cx),
                item.new_commits().cloned(),
            )
        });
        assert_ne!(head, old_head);
        assert_eq!(head, setup.fixture.pr_sha);
        assert!(text.contains("    2;"), "{text}");
        assert_eq!(new_commits, None);
        let project = setup
            .pr_workspace()
            .read_with(&setup.cx, |workspace, _| workspace.project().clone());
        assert!(
            !worktree_trusted(&project, cx),
            "trust given to the old head stays with it"
        );
    }

    #[gpui::test]
    async fn keeping_unsaved_changes_keeps_the_old_head(cx: &mut TestAppContext) {
        let Some(mut setup) = reload_setup("keeping_unsaved_changes_keeps_the_old_head", cx).await
        else {
            return;
        };
        let first = setup.open_first_head().await;
        let old_head = setup.fixture.pr_sha.clone();
        let unsaved = setup
            .cx
            .new(|cx| workspace::item::test::TestItem::new(cx).with_dirty(true));
        setup
            .pr_workspace()
            .update_in(&mut setup.cx, |workspace, window, cx| {
                workspace.add_item_to_active_pane(
                    Box::new(unsaved.clone()),
                    None,
                    false,
                    window,
                    cx,
                )
            });

        setup.fixture.push_pr_head("fn a() {\n    2;\n}\n");
        let opening = setup.open(None);
        // The fetch is real git work, so its end is waited for rather than parked on.
        for _ in 0..1000 {
            setup.cx.run_until_parked();
            if setup.cx.has_pending_prompt() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            setup.cx.has_pending_prompt(),
            "unsaved changes are asked about"
        );
        setup.cx.simulate_prompt_answer("Cancel");
        opening.await.expect("handled in the review");
        setup.cx.run_until_parked();

        assert_eq!(setup.review(), first);
        let (head, new_commits) = first.read_with(&setup.cx, |item, _| {
            (item.checkout.head_sha.clone(), item.new_commits().cloned())
        });
        assert_eq!(head, old_head);
        let message = keep_changes_message(&key());
        assert_eq!(
            new_commits,
            Some(NewCommits::Failed {
                message: message.clone().into(),
                summary: setup.summary(),
            })
        );
        assert!(drawn("photon-review-new-commits-retry", &mut setup.cx));
        assert_eq!(
            run(&setup.worktree(), &["rev-parse", "HEAD"]),
            old_head,
            "the open project's code never moves"
        );

        // Trying again once the changes are dealt with loads the new head.
        unsaved.update(&mut setup.cx, |item, _| item.is_dirty = false);
        setup.open(None).await.expect("loaded");
        setup.cx.run_until_parked();
        let head = setup
            .review()
            .read_with(&setup.cx, |item, _| item.checkout.head_sha.clone());
        assert_eq!(head, setup.fixture.pr_sha);
    }

    #[gpui::test]
    async fn a_failed_load_shows_in_the_open_review(cx: &mut TestAppContext) {
        let Some(mut setup) = reload_setup("a_failed_load_shows_in_the_open_review", cx).await
        else {
            return;
        };
        let first = setup.open_first_head().await;
        let old_head = setup.fixture.pr_sha.clone();
        setup.fixture.push_pr_head("fn a() {\n    2;\n}\n");
        let upstream_url = std::mem::replace(
            &mut setup.fixture.upstream_url,
            format!("file://{}", setup.fixture.base.join("gone.git").display()),
        );
        setup.open(None).await.expect("handled in the review");
        setup.cx.run_until_parked();
        let new_commits = first.read_with(&setup.cx, |item, _| item.new_commits().cloned());
        assert!(
            matches!(new_commits, Some(NewCommits::Failed { .. })),
            "{new_commits:?}"
        );
        assert!(drawn("photon-review-new-commits-retry", &mut setup.cx));

        // The inbox may still have the tab's head; a click tries again anyway.
        setup.fixture.upstream_url = upstream_url;
        setup.inbox_head = Some(old_head);
        setup.open(None).await.expect("loaded");
        setup.cx.run_until_parked();
        let head = setup
            .review()
            .read_with(&setup.cx, |item, _| item.checkout.head_sha.clone());
        assert_eq!(head, setup.fixture.pr_sha);
    }

    /// The fixture PR as it looks to someone who never reviewed it or viewed a file.
    fn unseen_detail() -> String {
        let detail = DETAIL
            .replace("\"VIEWED\"", "\"UNVIEWED\"")
            .replace(
                "\"reviews\": {\n     \"nodes\": [\n      {\n       \"commit\": {\n        \"oid\": \"a1\"\n       }\n      }\n     ]\n    }",
                "\"reviews\": {\n     \"nodes\": []\n    }",
            );
        let summary = parse_detail(&key(), &detail).expect("detail").summary;
        assert_eq!(
            summary.last_review_commit, None,
            "the fixture changed shape"
        );
        detail
    }

    #[gpui::test]
    async fn a_first_open_starts_on_the_description(cx: &mut TestAppContext) {
        let Some(mut setup) = reload_setup("a_first_open_starts_on_the_description", cx).await
        else {
            return;
        };
        setup.detail = unseen_detail();
        setup.open(None).await.expect("opened");
        let review = setup.review();
        assert!(review.read_with(&setup.cx, |item, _| item.conversation_open));
        assert!(drawn("photon-review-description", &mut setup.cx));
    }

    #[gpui::test]
    async fn a_later_open_keeps_its_usual_step(cx: &mut TestAppContext) {
        let Some(mut setup) = reload_setup("a_later_open_keeps_its_usual_step", cx).await else {
            return;
        };
        // GitHub has a review of yours and a viewed file.
        setup.open_first_head().await;
        let review = setup.review();
        assert!(!review.read_with(&setup.cx, |item, _| item.conversation_open));
        assert!(!drawn("photon-review-description", &mut setup.cx));
        // The title has a line of its own, taller than the details row below it.
        let title = setup
            .cx
            .debug_bounds("photon-review-title")
            .expect("title drawn");
        let stat = setup
            .cx
            .debug_bounds("photon-review-readiness")
            .expect("readiness drawn");
        assert!(title.bottom() <= stat.top(), "{title:?} {stat:?}");
    }

    fn worktree_trusted(project: &Entity<Project>, cx: &mut TestAppContext) -> bool {
        cx.update(|cx| {
            let worktree_store = project.read(cx).worktree_store();
            let Some(worktree) = project.read(cx).worktrees(cx).next() else {
                return false;
            };
            let id = worktree.read(cx).id();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| trusted.can_trust(&worktree_store, id, cx))
        })
    }

    fn init_trust(saved: &str, cx: &mut TestAppContext) {
        let mut saved_paths = project::trusted_worktrees::DbTrustedPaths::default();
        saved_paths
            .entry(None)
            .or_default()
            .insert(PathBuf::from(saved));
        cx.update(|cx| project::trusted_worktrees::init(saved_paths, cx));
    }

    #[gpui::test]
    async fn trust_does_not_survive_a_new_head(cx: &mut TestAppContext) {
        init_test(cx);
        init_trust(path!("/reviews/pr"), cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/reviews/pr"), json!({ "a.rs": "" }))
            .await;
        let mut checkout = Checkout {
            worktree: PathBuf::from(path!("/reviews/pr")),
            head_changed: false,
            ..checkout()
        };

        cx.update(|cx| forget_stale_trust(&checkout, cx));
        let project =
            Project::test_with_worktree_trust(fs.clone(), [path!("/reviews/pr").as_ref()], cx)
                .await;
        assert!(
            worktree_trusted(&project, cx),
            "the same head stays trusted"
        );

        checkout.head_changed = true;
        cx.update(|cx| forget_stale_trust(&checkout, cx));
        assert!(!worktree_trusted(&project, cx));
        let reopened =
            Project::test_with_worktree_trust(fs, [path!("/reviews/pr").as_ref()], cx).await;
        assert!(!worktree_trusted(&reopened, cx));
    }

    #[gpui::test]
    async fn pr_worktree_is_never_trusted_through_a_parent(cx: &mut TestAppContext) {
        init_test(cx);
        init_trust(path!("/reviews"), cx);
        cx.update(|cx| refuse_parent_trust(PathBuf::from(path!("/reviews/pr_review")), cx));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/reviews/pr_review/pr"), json!({ "a.rs": "" }))
            .await;
        let project =
            Project::test_with_worktree_trust(fs, [path!("/reviews/pr_review/pr").as_ref()], cx)
                .await;
        assert!(!worktree_trusted(&project, cx));

        cx.update(|cx| {
            let worktree_store = project.read(cx).worktree_store();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| {
                    trusted.trust(
                        &worktree_store,
                        [PathTrust::AbsPath(PathBuf::from(path!("/reviews")))]
                            .into_iter()
                            .collect(),
                        cx,
                    )
                });
        });
        assert!(!worktree_trusted(&project, cx));

        cx.update(|cx| {
            let worktree_store = project.read(cx).worktree_store();
            let worktree_ids = project
                .read(cx)
                .worktrees(cx)
                .map(|worktree| PathTrust::Worktree(worktree.read(cx).id()))
                .collect();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| {
                    trusted.trust(&worktree_store, worktree_ids, cx)
                });
        });
        assert!(
            worktree_trusted(&project, cx),
            "trusting the worktree itself works"
        );
    }

    // A file no language server handles (YAML here) still shows its diff.
    #[gpui::test]
    async fn files_without_a_language_server_still_show_their_diff(cx: &mut TestAppContext) {
        const CI_BASE: &str = "name: ci\non: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n";
        const CI_HEAD: &str = "name: ci\non: push\njobs:\n  test:\n    runs-on: macos-15\n";
        let (item, mut cx) = open_item(
            cx,
            json!({ ".github": { "workflows": { "ci.yml": CI_HEAD } } }),
            vec![modified(".github/workflows/ci.yml", CI_BASE)],
            Graph {
                functions: Vec::new(),
                calls: Vec::new(),
            },
            false,
            None,
        )
        .await;
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert!(!item.steps().is_empty(), "the file gets a step")
        });
        let shown = text(&item, &mut cx);
        assert!(shown.contains("runs-on: macos-15"), "diff shown: {shown:?}");
    }

    const APP_BASE: &str = "server:\n  port: 8080\n";
    const APP_HEAD: &str = "server:\n  port: 9090\n";

    #[gpui::test]
    async fn missing_file_in_checkout_is_read_from_git(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path();
        run(repo, &["init", "-q"]);
        std::fs::write(repo.join("application.yml"), APP_BASE).expect("write");
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", "base"]);
        let base = run(repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("application.yml"), APP_HEAD).expect("write");
        run(repo, &["commit", "-q", "-am", "head"]);
        let head = run(repo, &["rev-parse", "HEAD"]);
        let diff = git_diff(repo, &base, &head)
            .await
            .expect("diff")
            .pop()
            .expect("one file");

        init_test(cx);
        let database = test_database(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/repo"), json!({})).await;
        let project = Project::test(fs, [path!("/repo").as_ref()], cx).await;
        let loaded = cx
            .update(|cx| {
                build_loaded_file(
                    &project,
                    PathBuf::from(path!("/repo/application.yml")),
                    repo,
                    &head,
                    Some(APP_BASE.into()),
                    diff,
                    cx,
                )
            })
            .await
            .expect("loaded");
        let detail = parse_detail(&key(), DETAIL).expect("detail");
        let window = cx.add_window(|window, cx| {
            ReviewItem::new(
                project.clone(),
                detail,
                "alice".into(),
                checkout(),
                vec![loaded],
                database,
                window,
                cx,
            )
        });
        let item = window.root(cx).expect("item");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        settle(&mut cx);
        let shown = text(&item, &mut cx);
        assert!(shown.contains("port: 9090"), "head shown: {shown:?}");
        assert!(
            shown.contains("port: 8080"),
            "base shown as deleted: {shown:?}"
        );
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.step_notes(),
                vec![SharedString::from(
                    "application.yml · Shown from git; the file isn't in the checkout"
                )]
            )
        });
    }

    #[gpui::test]
    async fn unreadable_file_shows_a_message_not_a_blank_editor(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (item, mut cx) = open_item(
            cx,
            json!({}),
            vec![modified("config/application.yml", APP_BASE)],
            Graph {
                functions: Vec::new(),
                calls: Vec::new(),
            },
            false,
            None,
        )
        .await;
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert!(item.files[0].contents.is_none(), "no blank buffer");
            assert!(!item.steps().is_empty(), "the file still gets a step");
            assert_eq!(
                item.step_notes(),
                vec![SharedString::from(
                    "Couldn't read application.yml from the checkout"
                )]
            );
        });
    }

    #[gpui::test]
    async fn untrusted_worktree_waits_for_trust(cx: &mut TestAppContext) {
        let (item, mut cx, prepares) = open_item_with(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
            true,
            None,
        )
        .await;
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.context_state(),
                &ContextState::Unavailable(TRUST_REASON.into())
            )
        });
        assert_eq!(prepares.load(Ordering::SeqCst), 0);
        assert!(text(&item, &mut cx).contains("let user = user.trim();"));

        cx.update(|_, cx| {
            let project = item.read(cx).project.clone();
            let worktree_store = project.read(cx).worktree_store();
            let worktree_ids: Vec<_> = project
                .read(cx)
                .worktrees(cx)
                .map(|worktree| PathTrust::Worktree(worktree.read(cx).id()))
                .collect();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| {
                    trusted.trust(&worktree_store, worktree_ids.into_iter().collect(), cx)
                });
        });
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.context_state(), &ContextState::Ready)
        });
        assert!(prepares.load(Ordering::SeqCst) > 0);
        assert!(text(&item, &mut cx).contains("user == password"));
    }

    fn select_text(item: &Entity<ReviewItem>, needle: &str, cx: &mut VisualTestContext) {
        let offset = text(item, cx).find(needle).expect("text shown");
        item.update_in(cx, |item, window, cx| {
            window.focus(&item.editor().focus_handle(cx), cx);
            item.editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)])
                })
            })
        });
        cx.run_until_parked();
    }

    fn composer(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> Entity<Editor> {
        item.read_with(cx, |item, _| {
            item.draft_views
                .last()
                .and_then(|view| view.composer.clone())
                .expect("composer")
        })
    }

    fn type_text(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) {
        editor.update_in(cx, |editor, window, cx| editor.insert(text, window, cx));
    }

    fn database(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> ReviewDb {
        item.read_with(cx, |item, _| item.database.clone())
    }

    fn stored_draft(path: &str, start_line: u32, anchor_text: &str) -> Draft {
        Draft {
            id: None,
            key: key(),
            path: path.into(),
            side: crate::Side::Right,
            start_line,
            end_line: start_line,
            commit: "old".into(),
            anchor_text: anchor_text.into(),
            body: format!("about {anchor_text}"),
            outdated: false,
            general: false,
            post_state: crate::PostState::Unposted,
            from_agent: false,
        }
    }

    #[gpui::test]
    async fn draft_survives_reload(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let composer = composer(&item, &mut cx);
        type_text(&composer, "Why audit ", &mut cx);
        type_text(&composer, "here?", &mut cx);
        cx.run_until_parked();

        let database = database(&item, &mut cx);
        let saved = database.drafts(&key()).expect("drafts");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].body, "Why audit here?");
        assert_eq!(saved[0].path, "src/login.rs");
        assert_eq!((saved[0].start_line, saved[0].end_line), (3, 3));
        assert_eq!(saved[0].anchor_text, "    audit(user);");
        assert_eq!(saved[0].commit, "head");

        let (project, detail, files) = item.read_with(&cx, |item, _| {
            let files: Vec<LoadedFile> = item
                .files
                .iter()
                .map(|file| LoadedFile {
                    diff: file.diff.clone(),
                    contents: file.contents.clone(),
                    note: file.note.clone(),
                })
                .collect();
            (item.project.clone(), item.detail.clone(), files)
        });
        cx.update(|window, _| window.remove_window());
        drop(item);
        cx.run_until_parked();

        let window = cx.add_window(|window, cx| {
            ReviewItem::new(
                project,
                detail,
                "alice".into(),
                checkout(),
                files,
                database,
                window,
                cx,
            )
        });
        let reopened = window.root(&mut cx).expect("item");
        cx.run_until_parked();
        reopened.read_with(&cx, |item, _| {
            let bodies: Vec<_> = item
                .drafts()
                .iter()
                .map(|draft| draft.body.as_str())
                .collect();
            assert_eq!(bodies, vec!["Why audit here?"]);
            assert_eq!(item.comment_blocks.len(), 1);
        });
    }

    #[gpui::test]
    async fn comment_on_unchanged_excerpt_creates_draft(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        select_text(&item, "user == password", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            let draft = item.drafts().last().expect("draft").clone();
            assert_eq!(draft.path, "src/password.rs");
            assert_eq!(draft.side, crate::Side::Right);
            assert_eq!((draft.start_line, draft.end_line), (2, 2));
            assert_eq!(draft.anchor_text, "    user == password");
            assert_eq!(draft.commit, "head");
            assert_eq!(
                item.draft_note(&draft).as_deref(),
                Some(
                    "Unchanged file: GitHub can't take this comment directly, so Photon adds it \
                     to the review summary under src/password.rs, with a quote."
                )
            );
            assert_eq!(item.comment_blocks.len(), 1);
        });
    }

    #[gpui::test]
    async fn comment_on_deleted_row_uses_base_side(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "b.rs": B_HEAD } }),
            vec![modified("src/b.rs", B_BASE)],
            Graph {
                functions: vec![function("callee", "src/b.rs", 0, 2)],
                calls: Vec::new(),
            },
            true,
            None,
        )
        .await;
        settle(&mut cx);
        select_text(&item, "let a = 1;", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            let draft = item.drafts().last().expect("draft");
            assert_eq!(draft.path, "src/b.rs");
            assert_eq!(draft.side, crate::Side::Left);
            assert_eq!((draft.start_line, draft.end_line), (2, 2));
            assert_eq!(draft.anchor_text, "    let a = 1;");
            assert_eq!(item.comment_blocks.len(), 1);
        });
    }

    #[gpui::test]
    async fn hiding_whitespace_drops_reformatted_files_and_base_comments(cx: &mut TestAppContext) {
        const C_BASE: &str = "fn other() {\n  let c = 3;\n}\n";
        const C_HEAD: &str = "fn other() {\n    let c = 3;\n}\n";
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "b.rs": B_HEAD, "c.rs": C_HEAD } }),
            vec![modified("src/b.rs", B_BASE), modified("src/c.rs", C_BASE)],
            Graph {
                functions: vec![
                    function("callee", "src/b.rs", 0, 2),
                    function("other", "src/c.rs", 0, 2),
                ],
                calls: Vec::new(),
            },
            true,
            None,
        )
        .await;
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["src/b.rs", "src/c.rs"]);

        item.update(&mut cx, |item, cx| item.toggle_diff_view(true, cx));
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["src/b.rs"]);
        item.update(&mut cx, |item, _| {
            assert!(item.hidden_by_view(&item.files[1]));
            item.selected = 0;
        });
        item.update_in(&mut cx, |item, _, cx| item.show_step(cx));
        cx.run_until_parked();
        select_text(&item, "let a = 1;", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert!(item.drafts().is_empty());
            assert_eq!(
                item.since_review_note.as_deref(),
                Some(SIMPLIFIED_LEFT_COMMENT_NOTE)
            );
        });

        item.update(&mut cx, |item, cx| item.toggle_diff_view(true, cx));
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["src/b.rs", "src/c.rs"]);
        let base = item.read_with(&cx, |item, cx| {
            item.files[1]
                .contents
                .as_ref()
                .and_then(|(_, diff)| diff.read(cx).base_text_string(cx))
        });
        assert_eq!(base.as_deref(), Some(C_BASE));
    }

    fn step_files(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> Vec<String> {
        item.read_with(cx, |item, _| {
            let mut files: Vec<String> = item
                .steps()
                .iter()
                .flat_map(|step| step.files.clone())
                .collect();
            files.sort();
            files.dedup();
            files
        })
    }

    #[gpui::test]
    async fn hiding_moved_code_drops_files_that_only_moved_it(cx: &mut TestAppContext) {
        const MOVED: &str = "fn helper() {\n    let total = 1;\n    total\n}\n";
        let a_base = format!("fn keep() {{}}\n{MOVED}");
        let c_base = "fn other() {}\n";
        let c_head = format!("fn other() {{}}\n{MOVED}");
        let a_base: &'static str = a_base.leak();
        let c_head: &'static str = c_head.leak();
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "a.rs": "fn keep() {}\n", "b.rs": B_HEAD, "c.rs": c_head } }),
            vec![
                modified("src/a.rs", a_base),
                modified("src/b.rs", B_BASE),
                modified("src/c.rs", c_base),
            ],
            Graph {
                functions: vec![
                    function("callee", "src/b.rs", 0, 2),
                    function("helper", "src/c.rs", 1, 4),
                ],
                calls: Vec::new(),
            },
            true,
            None,
        )
        .await;
        settle(&mut cx);
        assert_eq!(
            step_files(&item, &mut cx),
            ["src/a.rs", "src/b.rs", "src/c.rs"]
        );

        item.update(&mut cx, |item, cx| item.toggle_diff_view(false, cx));
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["src/b.rs"]);
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.moves,
                [Move {
                    from_path: "src/a.rs".into(),
                    from_line: 2,
                    to_path: "src/c.rs".into(),
                    to_line: 2,
                    lines: 4,
                }]
            );
        });

        item.update(&mut cx, |item, cx| item.toggle_diff_view(false, cx));
        settle(&mut cx);
        assert_eq!(
            step_files(&item, &mut cx),
            ["src/a.rs", "src/b.rs", "src/c.rs"]
        );
        item.read_with(&cx, |item, _| assert!(item.moves.is_empty()));
    }

    #[gpui::test]
    async fn hiding_generated_files_drops_lock_files_from_the_map(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            json!({ "Cargo.lock": "version = 4\n", "src": { "b.rs": B_HEAD } }),
            vec![
                modified("Cargo.lock", "version = 3\n"),
                modified("src/b.rs", B_BASE),
            ],
            Graph {
                functions: vec![function("callee", "src/b.rs", 0, 2)],
                calls: Vec::new(),
            },
            true,
            None,
        )
        .await;
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["Cargo.lock", "src/b.rs"]);
        item.update(&mut cx, |item, cx| item.toggle_hide_generated(cx));
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["src/b.rs"]);
        item.read_with(&cx, |item, _| {
            assert!(item.hidden_by_view(&item.files[0]));
            assert!(!item.hidden_by_view(&item.files[1]));
        });
        item.update(&mut cx, |item, cx| item.toggle_hide_generated(cx));
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["Cargo.lock", "src/b.rs"]);
    }

    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    fn drawn(selector: &'static str, cx: &mut VisualTestContext) -> bool {
        cx.run_until_parked();
        cx.debug_bounds(selector).is_some()
    }

    const C_REFORMATTED_BASE: &str = "fn other() {\n  let c = 3;\n}\n";
    const C_REFORMATTED_HEAD: &str = "fn other() {\n    let c = 3;\n}\n";

    async fn open_reformatted(cx: &mut TestAppContext) -> (Entity<ReviewItem>, VisualTestContext) {
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "b.rs": B_HEAD, "c.rs": C_REFORMATTED_HEAD } }),
            vec![
                modified("src/b.rs", B_BASE),
                modified("src/c.rs", C_REFORMATTED_BASE),
            ],
            Graph {
                functions: vec![
                    function("callee", "src/b.rs", 0, 2),
                    function("other", "src/c.rs", 0, 2),
                ],
                calls: Vec::new(),
            },
            true,
            None,
        )
        .await;
        settle(&mut cx);
        (item, cx)
    }

    #[gpui::test]
    async fn the_view_menu_hides_whitespace_when_clicked(cx: &mut TestAppContext) {
        let (item, mut cx) = open_reformatted(cx).await;
        assert!(!drawn("photon-review-hidden", &mut cx));

        click("photon-review-view", &mut cx);
        click("MENU_ITEM-Whitespace changes", &mut cx);
        settle(&mut cx);
        item.read_with(&cx, |item, _| assert!(item.hide_whitespace));
        assert_eq!(step_files(&item, &mut cx), ["src/b.rs"]);
        assert!(!drawn("photon-review-hidden", &mut cx));
        click("photon-review-view", &mut cx);
        click("MENU_ITEM-Show what's hidden", &mut cx);
        assert!(drawn("photon-review-hidden", &mut cx));

        click("photon-review-view", &mut cx);
        click("MENU_ITEM-Whitespace changes", &mut cx);
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["src/b.rs", "src/c.rs"]);
        assert!(!drawn("photon-review-hidden", &mut cx));

        click("photon-review-view", &mut cx);
        click("MENU_ITEM-Moved code", &mut cx);
        item.read_with(&cx, |item, _| assert!(item.hide_moves));
        click("photon-review-view", &mut cx);
        click("MENU_ITEM-Generated and lock files", &mut cx);
        item.read_with(&cx, |item, _| assert!(item.hide_generated));
    }

    #[gpui::test]
    async fn show_whats_hidden_is_offered_only_while_something_is_hidden(cx: &mut TestAppContext) {
        let (_item, mut cx) = open_reformatted(cx).await;
        click("photon-review-view", &mut cx);
        assert!(drawn("MENU_ITEM-Whitespace changes", &mut cx));
        assert!(!drawn("MENU_ITEM-Show what's hidden", &mut cx));
        click("MENU_ITEM-Whitespace changes", &mut cx);
        settle(&mut cx);
        click("photon-review-view", &mut cx);
        assert!(drawn("MENU_ITEM-Show what's hidden", &mut cx));
    }

    #[gpui::test]
    async fn clicking_the_checklist_heading_folds_and_unfolds_it(cx: &mut TestAppContext) {
        let (_item, mut cx) = open_reformatted(cx).await;
        assert!(drawn("photon-review-checklist-toggle", &mut cx));
        assert!(!drawn("photon-review-checklist-body", &mut cx));
        click("photon-review-checklist-toggle", &mut cx);
        assert!(drawn("photon-review-checklist-body", &mut cx));
        click("photon-review-checklist-toggle", &mut cx);
        assert!(!drawn("photon-review-checklist-body", &mut cx));
        assert!(drawn("photon-review-checklist-toggle", &mut cx));
    }

    #[gpui::test]
    async fn add_your_own_checklist_opens_the_settings_file(cx: &mut TestAppContext) {
        let (_item, mut cx) = open_reformatted(cx).await;
        let opened = std::rc::Rc::new(std::cell::Cell::new(0));
        cx.update(|_, cx| {
            let opened = opened.clone();
            cx.on_action(move |_: &zed_actions::OpenSettingsFile, _| opened.set(opened.get() + 1));
        });
        click("photon-review-more", &mut cx);
        click("MENU_ITEM-Add your own checklist…", &mut cx);
        assert_eq!(opened.get(), 1);
    }

    #[gpui::test]
    async fn clicking_a_step_checkbox_ticks_it_without_selecting_it(cx: &mut TestAppContext) {
        let (item, mut cx) = open_reformatted(cx).await;
        let fake = install_fake(&mut cx);
        let marks_viewed = |request: &crate::GhRequest| {
            request
                .stdin
                .as_deref()
                .is_some_and(|stdin| stdin.contains("FileAsViewed("))
        };
        fake.respond(marks_viewed, Ok(r#"{"data":{"x":{}}}"#.to_string()));
        let (selected, second) =
            item.read_with(&cx, |item, _| (item.selected, item.steps[1].key.clone()));
        click("photon-review-step-viewed-1", &mut cx);
        item.read_with(&cx, |item, _| {
            assert!(item.viewed_steps.contains_key(&second));
            assert_eq!(item.selected, selected);
            assert_eq!(item.progress_text(), "1 of 2 steps left to review");
        });
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|request| marks_viewed(request))
                .count(),
            1,
            "the step's only file is marked viewed on GitHub"
        );
        click("photon-review-step-viewed-1", &mut cx);
        item.read_with(&cx, |item, _| {
            assert!(!item.viewed_steps.contains_key(&second))
        });
    }

    #[gpui::test]
    async fn the_selected_step_lists_its_files_with_status(cx: &mut TestAppContext) {
        let added = PrFile {
            path: "src/new.rs",
            base: None,
            status: FileStatus::Added,
            hunks: vec![crate::Hunk {
                old_start: 0,
                old_len: 0,
                new_start: 1,
                new_len: 2,
                body: "+fn fresh() {\n+}\n".into(),
            }],
        };
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "b.rs": B_HEAD, "new.rs": "fn fresh() {\n}\n" } }),
            vec![modified("src/b.rs", B_BASE), added],
            Graph {
                functions: vec![
                    function("callee", "src/b.rs", 0, 2),
                    function("fresh", "src/new.rs", 0, 1),
                ],
                calls: Vec::new(),
            },
            true,
            None,
        )
        .await;
        settle(&mut cx);
        let step_of = |path: &str, cx: &mut VisualTestContext| {
            item.read_with(cx, |item, _| {
                item.steps
                    .iter()
                    .position(|step| step.files.iter().any(|file| file == path))
                    .expect("step")
            })
        };
        let new_step = step_of("src/new.rs", &mut cx);
        item.update_in(&mut cx, |item, window, cx| {
            item.select_step(new_step, window, cx)
        });
        assert!(drawn("photon-review-file-SquarePlus:src/new.rs", &mut cx));
        let b_step = step_of("src/b.rs", &mut cx);
        item.update_in(&mut cx, |item, window, cx| {
            item.select_step(b_step, window, cx)
        });
        assert!(drawn("photon-review-file-SquareDot:src/b.rs", &mut cx));
        assert!(!drawn("photon-review-file-SquarePlus:src/new.rs", &mut cx));
        item.read_with(&cx, |item, _| {
            let new_file = item
                .files
                .iter()
                .find(|file| file.diff.path == "src/new.rs");
            assert_eq!(new_file.expect("new file").diff.line_counts(), (2, 0));
        });
    }

    #[gpui::test]
    async fn reanchored_draft_saved_with_new_commit(cx: &mut TestAppContext) {
        let database = test_database(cx);
        for draft in [
            stored_draft("src/login.rs", 2, "    verify_password(user, password)"),
            stored_draft("src/login.rs", 2, "    old_line();"),
            stored_draft("src/password.rs", 5, "    user == password"),
        ] {
            database.save_draft(draft).await.expect("save");
        }
        let (item, mut cx, _) = open_item_with(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
            false,
            Some(database.clone()),
        )
        .await;
        settle(&mut cx);
        let summary = |drafts: &[Draft]| {
            drafts
                .iter()
                .map(|draft| {
                    (
                        draft.start_line,
                        draft.end_line,
                        draft.commit.clone(),
                        draft.outdated,
                    )
                })
                .collect::<Vec<_>>()
        };
        let expected = vec![
            (4, 4, "head".to_string(), false),
            (2, 2, "old".to_string(), true),
            (2, 2, "head".to_string(), false),
        ];
        assert_eq!(summary(&database.drafts(&key()).expect("drafts")), expected);
        item.read_with(&cx, |item, _| {
            assert_eq!(summary(item.drafts()), expected);
            assert_eq!(item.drafts()[1].anchor_text, "    old_line();");
        });

        // The outdated draft can be placed again on the current selection.
        select_text(&item, "audit(user);", &mut cx);
        item.update(&mut cx, |item, cx| item.replace_draft(1, cx));
        cx.run_until_parked();
        let replaced = database.drafts(&key()).expect("drafts")[1].clone();
        assert_eq!((replaced.start_line, replaced.outdated), (3, false));
        assert_eq!(replaced.anchor_text, "    audit(user);");
        assert_eq!(replaced.body, "about     old_line();");
    }

    #[gpui::test]
    async fn cancel_nonempty_draft_asks_first(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        let database = database(&item, &mut cx);
        let local_id = |item: &Entity<ReviewItem>, cx: &mut VisualTestContext| {
            item.read_with(cx, |item, _| {
                item.draft_views.last().expect("view").local_id
            })
        };

        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let empty = local_id(&item, &mut cx);
        item.update_in(&mut cx, |item, window, cx| {
            item.cancel_draft(empty, window, cx)
        });
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        item.read_with(&cx, |item, _| assert!(item.drafts().is_empty()));

        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let typed = local_id(&item, &mut cx);
        type_text(&composer(&item, &mut cx), "Keep me", &mut cx);
        cx.run_until_parked();
        item.update_in(&mut cx, |item, window, cx| {
            item.cancel_draft(typed, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Keep");
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(item.drafts().len(), 1));
        assert_eq!(database.drafts(&key()).expect("drafts").len(), 1);

        item.update_in(&mut cx, |item, window, cx| {
            item.cancel_draft(typed, window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Discard");
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert!(item.drafts().is_empty());
            assert!(item.comment_blocks.is_empty());
        });
        assert!(database.drafts(&key()).expect("drafts").is_empty());
    }

    async fn open_with_drafts(
        cx: &mut TestAppContext,
        drafts: Vec<Draft>,
    ) -> (Entity<ReviewItem>, VisualTestContext, ReviewDb) {
        let database = test_database(cx);
        for draft in drafts {
            database.save_draft(draft).await.expect("save");
        }
        let (item, mut cx, _) = open_item_with(
            cx,
            login_tree(),
            vec![login_with_hunk()],
            login_graph(),
            true,
            None,
            false,
            Some(database.clone()),
        )
        .await;
        settle(&mut cx);
        (item, cx, database)
    }

    /// Lines 1–4 of the head are in the diff; line 5 (`}`) is unchanged.
    fn login_with_hunk() -> PrFile {
        PrFile {
            hunks: vec![crate::Hunk {
                old_start: 1,
                old_len: 2,
                new_start: 1,
                new_len: 4,
                body: " fn login(user: &str, password: &str) -> bool {\n\
                       +    let user = user.trim();\n\
                       +    audit(user);\n     verify_password(user, password)\n"
                    .into(),
            }],
            ..modified("src/login.rs", LOGIN_BASE)
        }
    }

    fn current_draft(path: &str, start_line: u32, anchor_text: &str) -> Draft {
        Draft {
            commit: "head".into(),
            ..stored_draft(path, start_line, anchor_text)
        }
    }

    fn bodies(database: &ReviewDb) -> Vec<String> {
        database
            .drafts(&key())
            .expect("drafts")
            .into_iter()
            .map(|draft| draft.body)
            .collect()
    }

    #[gpui::test]
    async fn edit_after_reload(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        item.update_in(&mut cx, |item, window, cx| item.edit_draft(0, window, cx));
        cx.run_until_parked();
        let composer = composer(&item, &mut cx);
        assert_eq!(
            composer.read_with(&cx, |editor, cx| editor.text(cx)),
            "about     audit(user);"
        );
        type_text(&composer, " Really?", &mut cx);
        cx.run_until_parked();
        assert_eq!(bodies(&database), vec!["about     audit(user); Really?"]);
        item.update(&mut cx, |item, cx| item.collapse_draft(0, cx));
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert!(item.draft_views[0].composer.is_none());
            assert_eq!(item.drafts()[0].body, "about     audit(user); Really?");
        });
    }

    #[gpui::test]
    async fn suggest_change_adds_the_lines_as_a_suggestion(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        item.update_in(&mut cx, |item, window, cx| {
            item.edit_draft(0, window, cx);
            item.suggest_change(0, window, cx);
        });
        cx.run_until_parked();
        let composer = composer(&item, &mut cx);
        assert_eq!(
            composer.read_with(&cx, |editor, cx| editor.text(cx)),
            "about     audit(user);\n\n```suggestion\n    audit(user);\n```"
        );
    }

    #[gpui::test]
    async fn open_comments_are_counted_on_their_step_and_file(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        assert!(!drawn("photon-review-step-comments-0", &mut cx));
        assert!(!drawn("photon-review-file-comments-src/login.rs", &mut cx));
        item.update(&mut cx, |item, cx| {
            let mut resolved = suggestion_thread(false);
            resolved.id = "T_resolved".into();
            resolved.is_resolved = true;
            item.detail.threads.push(suggestion_thread(false));
            item.detail.threads.push(resolved);
            cx.notify();
        });
        assert!(drawn("photon-review-step-comments-0", &mut cx));
        assert!(drawn("photon-review-file-comments-src/login.rs", &mut cx));
        item.read_with(&cx, |item, _| {
            assert_eq!(item.comment_count(&["src/login.rs".into()]), 1);
            assert_eq!(item.comment_count(&["src/other.rs".into()]), 0);
        });
    }

    #[gpui::test]
    async fn drafts_count_as_open_comments(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        assert!(drawn("photon-review-step-comments-0", &mut cx));
        item.read_with(&cx, |item, _| {
            assert_eq!(item.comment_count(&["src/login.rs".into()]), 1)
        });
    }

    #[gpui::test]
    async fn comments_without_a_line_are_listed_in_the_map(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        assert!(!drawn("photon-review-unplaced-threads", &mut cx));
        item.update(&mut cx, |item, cx| {
            let mut outdated = suggestion_thread(true);
            outdated.line = None;
            item.detail.threads.push(outdated);
            cx.notify();
        });
        assert!(drawn("photon-review-unplaced-threads", &mut cx));
        item.update(&mut cx, |item, cx| {
            if let Some(thread) = item.detail.threads.last_mut() {
                thread.is_resolved = true;
            }
            cx.notify();
        });
        assert!(!drawn("photon-review-unplaced-threads", &mut cx));
    }

    fn drag_map_edge(by: Pixels, cx: &mut VisualTestContext) {
        let start = cx
            .debug_bounds("photon-review-map-resize")
            .expect("resize handle is drawn")
            .center();
        let end = gpui::point(start.x + by, start.y);
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(
            gpui::point(start.x + by / 2., start.y),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_move(end, MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn dragging_the_map_edge_resizes_the_steps(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let width = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.debug_bounds("photon-review-progress")
                .expect("map is drawn");
            item.read_with(cx, |item, _| item.map_width())
        };
        assert_eq!(width(&mut cx), DEFAULT_MAP_WIDTH);

        drag_map_edge(px(150.), &mut cx);
        let wider = width(&mut cx);
        assert!(
            (wider - (DEFAULT_MAP_WIDTH + px(150.))).abs() <= px(4.),
            "{wider:?}"
        );

        drag_map_edge(px(-1000.), &mut cx);
        assert_eq!(
            width(&mut cx),
            MIN_MAP_WIDTH,
            "never narrower than the minimum"
        );

        let handle = cx
            .debug_bounds("photon-review-map-resize")
            .expect("handle")
            .center();
        for click_count in [1, 2] {
            cx.simulate_event(gpui::MouseDownEvent {
                position: handle,
                button: MouseButton::Left,
                modifiers: gpui::Modifiers::none(),
                click_count,
                first_mouse: false,
            });
            cx.simulate_event(gpui::MouseUpEvent {
                position: handle,
                button: MouseButton::Left,
                modifiers: gpui::Modifiers::none(),
                click_count,
            });
        }
        assert_eq!(width(&mut cx), DEFAULT_MAP_WIDTH, "double-click resets");
    }

    fn suggestion_thread(outdated: bool) -> ReviewThread {
        ReviewThread {
            id: "T_suggest".into(),
            path: "src/login.rs".into(),
            line: Some(3),
            start_line: Some(2),
            side: Side::Right,
            is_resolved: false,
            is_outdated: outdated,
            comments: vec![crate::ThreadComment {
                database_id: 9,
                author: "priya".into(),
                body: "Tighter:\n```suggestion\n    audit(&user);\n```".into(),
                created_at: "2026-10-24T00:00:00Z".into(),
            }],
        }
    }

    #[gpui::test]
    async fn commit_suggestion_commits_the_change_on_the_branch(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        item.update(&mut cx, |item, cx| {
            item.detail.threads.push(suggestion_thread(false));
            item.show_comments(cx);
        });
        let fake = install_fake(&mut cx);
        let file = "fn login(user: User) {\n    check(user);\n    audit(user);\n}\n";
        fake.respond(
            |request| request.args.iter().any(|arg| arg.ends_with("?ref=head")),
            Ok(json!({
                "sha": "blob1",
                "encoding": "base64",
                "content": base64::engine::general_purpose::STANDARD.encode(file),
            })
            .to_string()),
        );
        fake.respond(
            |request| request.args.iter().any(|arg| arg == "PUT"),
            Ok("{}".into()),
        );
        let task = item.update(&mut cx, |item, cx| {
            item.commit_suggestion("T_suggest", 0, cx)
        });
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("committed");

        let put = fake
            .requests()
            .into_iter()
            .find(|request| request.args.iter().any(|arg| arg == "PUT"))
            .expect("commit sent");
        assert!(
            put.args
                .contains(&"repos/maya/web/contents/src/login.rs".to_string())
        );
        let body = request_json(&put);
        assert_eq!(body["branch"], "retry-uploads");
        assert_eq!(body["sha"], "blob1");
        assert_eq!(
            body["message"],
            "Apply suggestion from @priya in code review"
        );
        let content = base64::engine::general_purpose::STANDARD
            .decode(body["content"].as_str().expect("content"))
            .expect("base64");
        assert_eq!(
            String::from_utf8(content).expect("utf8"),
            "fn login(user: User) {\n    audit(&user);\n}\n"
        );
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.suggestion_notes
                    .get("T_suggest")
                    .map(|note| note.as_ref()),
                Some("Committed to retry-uploads. Reopen the review to see it.")
            );
        });
    }

    #[gpui::test]
    async fn outdated_or_failed_suggestions_are_not_committed(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        item.update(&mut cx, |item, _| {
            item.detail.threads.push(suggestion_thread(true))
        });
        let fake = install_fake(&mut cx);
        let outdated = item.update(&mut cx, |item, cx| {
            item.commit_suggestion("T_suggest", 0, cx)
        });
        assert!(outdated.now_or_never().expect("ready").is_err());
        assert!(fake.requests().is_empty());

        item.update(&mut cx, |item, _| {
            item.detail.threads[0..].iter_mut().for_each(|thread| {
                if thread.id == "T_suggest" {
                    thread.is_outdated = false;
                }
            })
        });
        fake.respond(
            |request| request.args.iter().any(|arg| arg.ends_with("?ref=head")),
            Err(crate::GhError::Failed {
                message: "gh: Not Found (HTTP 404)".into(),
            }),
        );
        let task = item.update(&mut cx, |item, cx| {
            item.commit_suggestion("T_suggest", 0, cx)
        });
        cx.run_until_parked();
        assert!(task.now_or_never().expect("finished").is_err());
        item.read_with(&cx, |item, _| {
            assert!(item.thread_errors["T_suggest"].contains("404"));
            assert!(!item.committing_suggestions.contains("T_suggest"));
        });
    }

    #[gpui::test]
    async fn delete_after_reload(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        item.update_in(&mut cx, |item, window, cx| item.cancel_draft(0, window, cx));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Discard");
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert!(item.drafts().is_empty());
            assert!(item.comment_blocks.is_empty());
        });
        assert!(bodies(&database).is_empty());
    }

    #[gpui::test]
    async fn empty_body_leaves_no_row(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        let database = database(&item, &mut cx);
        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let composer = composer(&item, &mut cx);
        type_text(&composer, "x", &mut cx);
        cx.run_until_parked();
        assert_eq!(bodies(&database), vec!["x"]);

        composer.update_in(&mut cx, |editor, window, cx| {
            editor.set_text("  ", window, cx)
        });
        cx.run_until_parked();
        assert!(bodies(&database).is_empty());
        item.read_with(&cx, |item, _| assert_eq!(item.drafts().len(), 1));

        type_text(&composer, "y", &mut cx);
        cx.run_until_parked();
        assert_eq!(bodies(&database), vec!["  y"]);
    }

    #[gpui::test]
    async fn failed_save_is_retried_when_added(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        let database = database(&item, &mut cx);
        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        crate::store::FAILING_SAVES.set(1);
        type_text(&composer(&item, &mut cx), "Keep me", &mut cx);
        cx.run_until_parked();
        assert!(bodies(&database).is_empty());
        item.read_with(&cx, |item, _| {
            assert!(item.draft_views[0].sync.lock().error.is_some())
        });

        item.update(&mut cx, |item, cx| item.collapse_draft(0, cx));
        cx.run_until_parked();
        assert_eq!(bodies(&database), vec!["Keep me"]);
        item.read_with(&cx, |item, _| {
            assert!(item.draft_views[0].composer.is_none());
            assert!(item.draft_views[0].sync.lock().error.is_none());
        });
    }

    #[gpui::test]
    async fn left_draft_reanchored_outside_diff_is_outdated(cx: &mut TestAppContext) {
        let database = test_database(cx);
        let left = |start_line, anchor_text: &str| Draft {
            side: crate::Side::Left,
            ..stored_draft("src/b.rs", start_line, anchor_text)
        };
        for draft in [left(5, "    let a = 1;"), left(1, "fn callee() {")] {
            database.save_draft(draft).await.expect("save");
        }
        let (_item, mut cx, _) = open_item_with(
            cx,
            json!({ "src": { "b.rs": B_HEAD } }),
            vec![PrFile {
                hunks: vec![crate::Hunk {
                    old_start: 1,
                    old_len: 3,
                    new_start: 1,
                    new_len: 3,
                    body: " fn callee() {\n-    let a = 1;\n+    let b = 2;\n }\n".into(),
                }],
                ..modified("src/b.rs", B_BASE)
            }],
            Graph {
                functions: vec![function("callee", "src/b.rs", 0, 2)],
                calls: Vec::new(),
            },
            true,
            None,
            false,
            Some(database.clone()),
        )
        .await;
        settle(&mut cx);
        let drafts: Vec<_> = database
            .drafts(&key())
            .expect("drafts")
            .into_iter()
            .map(|draft| (draft.start_line, draft.commit, draft.outdated))
            .collect();
        assert_eq!(
            drafts,
            vec![(2, "head".to_string(), false), (1, "old".to_string(), true)]
        );
    }

    #[gpui::test]
    async fn unreadable_file_leaves_draft_untouched(cx: &mut TestAppContext) {
        let (_item, _cx, database) = open_with_drafts(
            cx,
            vec![
                stored_draft("src", 1, "fn a() {}"),
                stored_draft("src/gone.rs", 1, "fn a() {}"),
            ],
        )
        .await;
        let drafts: Vec<_> = database
            .drafts(&key())
            .expect("drafts")
            .into_iter()
            .map(|draft| (draft.path, draft.commit, draft.outdated))
            .collect();
        assert_eq!(
            drafts,
            vec![
                ("src".to_string(), "old".to_string(), false),
                ("src/gone.rs".to_string(), "old".to_string(), true),
            ]
        );
    }

    fn install_fake(cx: &mut VisualTestContext) -> Arc<FakeGh> {
        let fake = FakeGh::new();
        cx.update(|_, cx| crate::set_gh_runner(fake.clone(), cx));
        fake
    }

    fn is_review_post(request: &crate::GhRequest) -> bool {
        request
            .args
            .iter()
            .any(|argument| argument.ends_with("/reviews"))
    }

    fn is_detail(request: &crate::GhRequest) -> bool {
        request
            .stdin
            .as_deref()
            .is_some_and(|stdin| stdin.contains("pullRequest(number"))
    }

    fn request_json(request: &crate::GhRequest) -> serde_json::Value {
        serde_json::from_str(request.stdin.as_deref().expect("stdin")).expect("json")
    }

    fn submit_drafts() -> Vec<Draft> {
        vec![
            current_draft("src/login.rs", 3, "    audit(user);"),
            current_draft("src/login.rs", 5, "}"),
            current_draft("src/password.rs", 2, "    user == password"),
        ]
    }

    fn submit(
        item: &Entity<ReviewItem>,
        event: crate::ReviewEvent,
        cx: &mut VisualTestContext,
    ) -> Result<()> {
        let task = item.update_in(cx, |item, window, cx| item.submit(event, window, cx));
        cx.run_until_parked();
        task.now_or_never().expect("submit finished")
    }

    fn set_checklists(cx: &mut VisualTestContext, checklists: serde_json::Value, auto: &[&str]) {
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let pr_review = settings.project.pr_review.get_or_insert_default();
                    pr_review.checklists =
                        Some(serde_json::from_value(checklists).expect("checklists"));
                    pr_review.auto_checks =
                        Some(auto.iter().map(|name| name.to_string()).collect());
                });
            });
        });
        cx.run_until_parked();
    }

    fn checklist_keys(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> Vec<String> {
        item.read_with(cx, |item, _| {
            item.checklist_items()
                .iter()
                .map(|item| item.key.clone())
                .collect()
        })
    }

    #[gpui::test]
    async fn checklist_answers_and_notes_go_into_the_review(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        set_checklists(
            &mut cx,
            json!([
                {"name": "Login", "when": {"paths": ["src/login.rs"]}, "items": ["Rate limited", "Audited"]},
                {"name": "Docs", "when": {"paths": ["docs/**"]}, "items": ["Spelling"]}
            ]),
            &[],
        );
        let keys = checklist_keys(&item, &mut cx);
        assert_eq!(
            keys,
            ["settings:Login:Rate limited", "settings:Login:Audited"]
        );
        item.update_in(&mut cx, |item, window, cx| {
            item.answer_checklist_item(&keys[0], Answer::No, window, cx);
            item.answer_checklist_item(&keys[1], Answer::Yes, window, cx);
            // Clicking the given answer again clears it.
            item.answer_checklist_item(&keys[1], Answer::Yes, window, cx);
            assert_eq!(item.checklist_answer(&keys[1]), None);
            item.answer_checklist_item(&keys[1], Answer::NotApplicable, window, cx);
        });
        let note = item.read_with(&cx, |item, _| item.note_editors[&keys[0]].clone());
        note.update_in(&mut cx, |editor, window, cx| {
            editor.set_text("no limit on retries", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            database.checklist_answers(&key()).expect("answers"),
            HashMap::from_iter([
                (keys[0].clone(), Answer::No),
                (keys[1].clone(), Answer::NotApplicable)
            ])
        );
        assert_eq!(
            database
                .composer_texts(&key())
                .expect("texts")
                .get(&checklist_note_slot(&keys[0]))
                .map(String::as_str),
            Some("no limit on retries")
        );

        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        submit(&item, crate::ReviewEvent::Approve, &mut cx).expect("submitted");
        let post = fake
            .requests()
            .into_iter()
            .find(is_review_post)
            .expect("review posted");
        assert_eq!(
            request_json(&post)["body"],
            "### Checklist (1/2)\n- [ ] Rate limited: **No**, no limit on retries\n- N/A Audited\n"
        );
    }

    #[gpui::test]
    async fn unknown_auto_check_names_are_reported(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        set_checklists(&mut cx, json!([]), &["checks_pass", "checks_pas"]);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.auto_results().len(), 1);
            assert_eq!(
                item.checklist_errors(),
                ["Unknown check \"checks_pas\" in pr_review.auto_checks"]
            );
        });
    }

    #[gpui::test]
    async fn team_checklists_and_local_runs_feed_the_checklist(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        set_checklists(&mut cx, json!([]), &["ran_locally"]);
        item.update(&mut cx, |item, cx| {
            item.set_team_checklists(
                vec![
                    serde_json::from_value(json!({"name": "Team", "items": ["Owner approved"]}))
                        .expect("team"),
                ],
                None,
                cx,
            )
        });
        assert_eq!(checklist_keys(&item, &mut cx), ["team:Team:Owner approved"]);
        let ran_locally =
            |cx: &mut VisualTestContext| item.read_with(cx, |item, _| item.auto_results().to_vec());
        assert!(matches!(
            ran_locally(&mut cx)[..],
            [(AutoCheck::RanLocally, AutoResult::Unknown(_))]
        ));

        database
            .set_local_run(
                key(),
                LocalRun {
                    head_sha: "head".into(),
                    command: "cargo test".into(),
                    exit_code: 0,
                },
            )
            .await
            .expect("record");
        cx.update(|_, cx| cx.default_global::<LocalRunsVersion>().0 += 1);
        cx.run_until_parked();
        assert_eq!(
            ran_locally(&mut cx),
            [(
                AutoCheck::RanLocally,
                AutoResult::Passed("\"cargo test\" exited 0".into())
            )]
        );
    }

    #[gpui::test]
    async fn submit_sends_one_review_with_every_draft(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, submit_drafts()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        submit(&item, crate::ReviewEvent::RequestChanges, &mut cx).expect("submitted");

        let posts: Vec<_> = fake.requests().into_iter().filter(is_review_post).collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(
            posts[0].args,
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "repos/acme/web/pulls/482/reviews",
                "--input",
                "-"
            ]
        );
        let body = request_json(&posts[0]);
        assert_eq!(body["commit_id"], "head");
        assert_eq!(body["event"], "REQUEST_CHANGES");
        let comments = body["comments"].as_array().expect("comments");
        assert_eq!(comments.len(), 2);
        assert_eq!(
            comments[0],
            json!({"path": "src/login.rs", "body": "about     audit(user);", "line": 3, "side": "RIGHT"})
        );
        assert_eq!(comments[1]["line"], 4);
        assert_eq!(comments[1]["side"], "RIGHT");
        assert!(
            comments[1]["body"]
                .as_str()
                .expect("quote")
                .starts_with(
                    "Unchanged code, [src/login.rs line 5](https://github.com/acme/web/blob/head/src/login.rs#L5):\n\n```rust\n}\n```"
                ),
            "{}",
            comments[1]["body"]
        );
        let review_body = body["body"].as_str().expect("body");
        assert!(
            review_body.starts_with("### src/password.rs\n\n"),
            "{review_body}"
        );
        assert!(review_body.contains("about     user == password"));
    }

    #[gpui::test]
    async fn failed_submit_keeps_drafts_and_shows_error(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, submit_drafts()).await;
        let fake = install_fake(&mut cx);
        let message = "gh: Validation Failed (HTTP 422): line must be part of the diff";
        fake.respond(
            is_review_post,
            Err(GhError::Failed {
                message: message.into(),
            }),
        );
        let before = item.read_with(&cx, |item, _| item.drafts().to_vec());
        let stored = database.drafts(&key()).expect("drafts");

        let error = submit(&item, crate::ReviewEvent::Comment, &mut cx).expect_err("failed");
        assert_eq!(error.to_string(), message);
        assert_eq!(fake.requests().len(), 1);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.drafts(), before.as_slice());
            assert_eq!(item.submit_error.as_deref(), Some(message));
            assert!(!item.submitting);
        });
        assert_eq!(database.drafts(&key()).expect("drafts"), stored);
        assert_eq!(database.last_review(&key()).expect("last review"), None);
    }

    #[gpui::test]
    async fn submit_success_clears_drafts_and_records_last_review(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, submit_drafts()).await;
        database
            .track(Tracked {
                key: key(),
                node_id: "PR_482".into(),
                repo_dir: PathBuf::from(path!("/repo")),
                worktree: PathBuf::from(path!("/repo")),
            })
            .await
            .expect("track");
        let fake = install_fake(&mut cx);
        // GitHub only answers if every sent row was marked before the request went out.
        let marked = database.clone();
        fake.respond(
            move |request| {
                is_review_post(request)
                    && marked.drafts(&key()).is_ok_and(|drafts| {
                        drafts
                            .iter()
                            .all(|draft| draft.post_state == PostState::Sending)
                    })
            },
            Ok("{}".into()),
        );
        fake.respond(is_detail, Ok(DETAIL.into()));
        item.update_in(&mut cx, |item, window, cx| {
            item.summary_editor
                .update(cx, |editor, cx| editor.set_text("Looks good.", window, cx))
        });
        submit(&item, crate::ReviewEvent::Approve, &mut cx).expect("submitted");

        assert!(database.drafts(&key()).expect("drafts").is_empty());
        assert_eq!(
            database
                .last_review(&key())
                .expect("last review")
                .as_deref(),
            Some("head")
        );
        let requests = fake.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(request_json(&requests[0])["event"], "APPROVE");
        assert!(is_detail(&requests[1]));
        item.read_with(&cx, |item, cx| {
            assert!(item.drafts().is_empty());
            assert!(item.submit_error.is_none());
            assert!(item.summary_editor.read(cx).text(cx).is_empty());
        });
    }

    fn reopen(
        database: &ReviewDb,
        cx: &mut TestAppContext,
    ) -> impl std::future::Future<Output = (Entity<ReviewItem>, VisualTestContext, Arc<AtomicUsize>)>
    {
        open_item_with(
            cx,
            login_tree(),
            vec![login_with_hunk()],
            login_graph(),
            true,
            None,
            false,
            Some(database.clone()),
        )
    }

    fn reply_editor(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> Entity<Editor> {
        item.read_with(cx, |item, _| {
            item.reply_editors
                .get("PRRT_1")
                .cloned()
                .expect("reply editor")
        })
    }

    #[gpui::test]
    async fn summary_and_reply_text_survive_a_reload(cx: &mut TestAppContext) {
        let (item, mut item_cx, database) = open_with_drafts(cx, Vec::new()).await;
        item.update_in(&mut item_cx, |item, window, cx| {
            item.summary_editor
                .update(cx, |editor, cx| editor.set_text("Mostly fine.", window, cx))
        });
        let editor = reply_editor(&item, &mut item_cx);
        type_text(&editor, "Agreed.", &mut item_cx);
        item_cx.run_until_parked();
        drop(item);

        let (item, mut item_cx, _) = reopen(&database, cx).await;
        item.read_with(&item_cx, |item, cx| {
            assert_eq!(item.summary_editor.read(cx).text(cx), "Mostly fine.");
        });
        let editor = reply_editor(&item, &mut item_cx);
        assert_eq!(
            editor.read_with(&item_cx, |editor, cx| editor.text(cx)),
            "Agreed."
        );

        let fake = install_fake(&mut item_cx);
        fake.respond(
            |request| {
                request
                    .args
                    .iter()
                    .any(|argument| argument.ends_with("/replies"))
            },
            Ok(r#"{"id":2001,"user":{"login":"alice"},"body":"Agreed.","created_at":"t"}"#.into()),
        );
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        let task = item.update(&mut item_cx, |item, cx| item.click_reply("PRRT_1", cx));
        item_cx.run_until_parked();
        task.now_or_never().expect("finished").expect("replied");
        submit(&item, crate::ReviewEvent::Comment, &mut item_cx).expect("submitted");
        item_cx.run_until_parked();
        assert!(database.composer_texts(&key()).expect("texts").is_empty());
    }

    #[gpui::test]
    async fn outdated_drafts_stay_local_on_submit(cx: &mut TestAppContext) {
        let outdated = Draft {
            outdated: true,
            ..stored_draft("src/login.rs", 2, "    gone();")
        };
        let (item, mut cx, database) = open_with_drafts(cx, vec![outdated.clone()]).await;
        item.read_with(&cx, |item, cx| {
            assert_eq!(item.left_out_drafts(), 1);
            assert_eq!(
                item.left_out_note().as_deref(),
                Some("1 draft not included (outdated or not yet re-placed)")
            );
            assert!(!item.can_comment(cx));
        });
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        item.update(&mut cx, |item, cx| {
            let draft = current_draft("src/login.rs", 3, "    audit(user);");
            item.push_draft(draft);
            item.queue_save(1, cx);
        });
        cx.run_until_parked();
        item.read_with(&cx, |item, cx| assert!(item.can_comment(cx)));
        submit(&item, crate::ReviewEvent::Comment, &mut cx).expect("submitted");

        let posts: Vec<_> = fake.requests().into_iter().filter(is_review_post).collect();
        let body = request_json(&posts[0]);
        assert_eq!(body["comments"].as_array().expect("comments").len(), 1);
        assert!(!body.to_string().contains("gone()"));
        let remaining: Vec<_> = database
            .drafts(&key())
            .expect("drafts")
            .into_iter()
            .map(|draft| (draft.anchor_text, draft.outdated))
            .collect();
        assert_eq!(remaining, vec![("    gone();".to_string(), true)]);
        item.read_with(&cx, |item, _| assert_eq!(item.drafts().len(), 1));
    }

    #[gpui::test]
    async fn reply_posts_only_on_click(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(
            |request| {
                request
                    .args
                    .iter()
                    .any(|argument| argument.ends_with("/replies"))
            },
            Ok(r#"{"id":2001,"user":{"login":"alice"},"body":"Agreed.","created_at":"t"}"#.into()),
        );
        let editor = item.read_with(&cx, |item, _| {
            item.reply_editors
                .get("PRRT_1")
                .cloned()
                .expect("reply editor")
        });
        type_text(&editor, "Agreed.", &mut cx);
        cx.run_until_parked();
        assert!(fake.requests().is_empty());

        let task = item.update(&mut cx, |item, cx| item.click_reply("PRRT_1", cx));
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("replied");
        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .args
                .contains(&"repos/acme/web/pulls/482/comments/1001/replies".to_string())
        );
        assert_eq!(request_json(&requests[0]), json!({"body": "Agreed."}));
        item.read_with(&cx, |item, _| {
            let thread = &item.detail.threads[0];
            let last = thread.comments.last().expect("comment");
            assert_eq!((last.database_id, last.body.as_str()), (2001, "Agreed."));
        });
        assert!(
            editor
                .read_with(&cx, |editor, cx| editor.text(cx))
                .is_empty()
        );
    }

    struct FakeAgent {
        replies: std::cell::RefCell<std::collections::VecDeque<Result<String>>>,
        prompts: std::cell::RefCell<Vec<(String, PathBuf)>>,
    }

    impl crate::ReviewAgent for FakeAgent {
        fn review(
            &self,
            _workspace: WeakEntity<Workspace>,
            prompt: String,
            work_dir: PathBuf,
            _window: &mut Window,
            _cx: &mut App,
        ) -> Task<Result<String>> {
            self.prompts.borrow_mut().push((prompt, work_dir));
            Task::ready(
                self.replies
                    .borrow_mut()
                    .pop_front()
                    .unwrap_or_else(|| Err(anyhow!("no reply"))),
            )
        }
    }

    fn install_agent(reply: Result<String>, cx: &mut VisualTestContext) -> std::rc::Rc<FakeAgent> {
        install_agent_replies(vec![reply], cx)
    }

    /// The agent answers each prompt with the next reply: the review, then the check.
    fn install_agent_replies(
        replies: Vec<Result<String>>,
        cx: &mut VisualTestContext,
    ) -> std::rc::Rc<FakeAgent> {
        let agent = std::rc::Rc::new(FakeAgent {
            replies: std::cell::RefCell::new(replies.into()),
            prompts: Default::default(),
        });
        cx.update(|_, cx| crate::set_review_agent(agent.clone(), cx));
        agent
    }

    #[gpui::test]
    async fn agent_findings_become_drafts(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        let agent = install_agent_replies(
            vec![
                Ok("Here you go:\n```json\n[\n {\"path\": \"src/login.rs\", \"line\": 3, \"body\": \"Audit before the check?\"},\n {\"path\": \"src/other.rs\", \"line\": 7, \"body\": \"Not in this PR.\"}\n]\n```".into()),
                Ok("```json\n[{\"finding\": 1, \"verdict\": \"keep\"}, {\"finding\": 2, \"verdict\": \"keep\"}]\n```".into()),
            ],
            &mut cx,
        );
        item.update_in(&mut cx, |item, window, cx| {
            item.workspace = Some(WeakEntity::new_invalid());
            item.start_agent_review(window, cx);
        });
        cx.run_until_parked();

        let prompts = agent.prompts.borrow();
        assert_eq!(prompts.len(), 2, "the review, then the check");
        assert!(prompts[0].0.contains("--- a/src/login.rs"));
        assert_eq!(prompts[0].1, PathBuf::from(path!("/repo")));
        assert_eq!(prompts[1].1, PathBuf::from(path!("/repo")));
        item.read_with(&cx, |item, _| {
            let drafts = item.drafts();
            assert_eq!(drafts.len(), 2);
            assert_eq!(
                (
                    drafts[0].path.as_str(),
                    drafts[0].end_line,
                    drafts[0].general
                ),
                ("src/login.rs", 3, false)
            );
            assert_eq!(drafts[0].anchor_text, "    audit(user);");
            assert_eq!(drafts[0].body, "Audit before the check?");
            assert!(drafts[1].general);
            assert_eq!(drafts[1].body, "`src/other.rs:7`: Not in this PR.");
            assert!(!item.agent_reviewing);
            assert!(
                item.agent_review_note()
                    .is_some_and(|(note, _)| note.starts_with("Kept all 2 findings"))
            );
        });
        assert_eq!(bodies(&database).len(), 2);
    }

    const THREE_FINDINGS: &str = "```json\n[\n {\"path\": \"src/login.rs\", \"line\": 3, \"severity\": \"must fix\", \"category\": \"security\", \"body\": \"Audit before the check?\"},\n {\"path\": \"src/login.rs\", \"line\": 2, \"severity\": \"should fix\", \"category\": \"logic\", \"body\": \"Trim drops tabs.\"},\n {\"path\": \"src/login.rs\", \"line\": 4, \"severity\": \"must fix\", \"category\": \"logic\", \"body\": \"Wrong order.\"}\n]\n```";

    fn start_pre_review(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) {
        set_login(item, false, cx);
        item.update(cx, |item, _| {
            item.workspace = Some(WeakEntity::new_invalid())
        });
        click("photon-review-more", cx);
        click("MENU_ITEM-Pre-review with agent", cx);
    }

    #[gpui::test]
    async fn the_check_keeps_only_confirmed_findings_and_lists_the_rest(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        let agent = install_agent_replies(
            vec![
                Ok(THREE_FINDINGS.into()),
                Ok("```json\n[{\"finding\": 1, \"verdict\": \"keep\", \"reason\": \"Seen.\"}, {\"finding\": 2, \"verdict\": \"drop\", \"reason\": \"Tabs are trimmed too.\"}]\n```".into()),
            ],
            &mut cx,
        );
        start_pre_review(&item, &mut cx);

        let check = agent.prompts.borrow()[1].0.clone();
        assert!(
            check.contains("1. src/login.rs:3 (must fix, security)\nAudit before the check?"),
            "{check}"
        );
        assert!(check.contains("3. src/login.rs:4 (must fix, logic)\nWrong order."));
        item.read_with(&cx, |item, _| {
            assert_eq!(item.drafts().len(), 1);
            assert!(item.drafts()[0].from_agent);
            assert_eq!(
                item.agent_review_note().map(|(note, _)| note.as_ref()),
                Some(
                    "Kept 1 of 3 findings; dropped 2 it couldn't confirm. Read each draft \
                     before you submit."
                )
            );
        });
        assert_eq!(bodies(&database).len(), 1, "dropped findings aren't drafts");
        assert!(drawn("photon-review-dropped", &mut cx));
        assert!(!drawn("photon-review-dropped-list", &mut cx));
        click("photon-review-dropped", &mut cx);
        assert!(drawn("photon-review-dropped-list", &mut cx));
        item.read_with(&cx, |item, _| {
            let reasons: Vec<&str> = item
                .dropped_findings
                .iter()
                .map(|(_, reason)| reason.as_str())
                .collect();
            assert_eq!(
                reasons,
                ["Tabs are trimmed too.", "The check didn't mention it."]
            );
        });
    }

    #[gpui::test]
    async fn a_failed_check_adds_every_finding_with_a_warning(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        install_agent_replies(
            vec![Ok(THREE_FINDINGS.into()), Ok("I looked; all fine.".into())],
            &mut cx,
        );
        start_pre_review(&item, &mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.drafts().len(), 3);
            let (note, color) = item.agent_review_note().expect("note");
            assert!(note.starts_with("Couldn't check the findings"), "{note}");
            assert_eq!(*color, Color::Warning);
        });
    }

    #[gpui::test]
    async fn quiet_asks_for_and_keeps_only_must_fix_correctness(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .project
                        .pr_review
                        .get_or_insert_default()
                        .agent_review
                        .get_or_insert_default()
                        .thoroughness = Some(settings::ReviewThoroughness::Quiet);
                });
            });
        });
        let agent = install_agent_replies(
            vec![
                Ok(THREE_FINDINGS.into()),
                Ok("```json\n[{\"finding\": 1, \"verdict\": \"keep\"}, {\"finding\": 2, \"verdict\": \"keep\"}]\n```".into()),
            ],
            &mut cx,
        );
        start_pre_review(&item, &mut cx);
        let prompts = agent.prompts.borrow();
        assert!(prompts[0].0.contains("Report only must-fix problems"));
        assert!(
            !prompts[1].0.contains("Trim drops tabs."),
            "{}",
            prompts[1].0
        );
        item.read_with(&cx, |item, _| {
            let bodies: Vec<&str> = item
                .drafts()
                .iter()
                .map(|draft| draft.body.as_str())
                .collect();
            assert_eq!(
                bodies,
                [
                    "Security (must fix): Audit before the check?",
                    "Logic (must fix): Wrong order."
                ]
            );
        });
    }

    #[gpui::test]
    async fn dismissing_an_agent_draft_asks_why_and_feeds_the_next_review(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![Draft {
                body: "**Logic** (must fix): Audit before the check?".into(),
                from_agent: true,
                ..current_draft("src/login.rs", 3, "    audit(user);")
            }],
        )
        .await;
        hover("photon-review-draft-delete-0", &mut cx);
        click("photon-review-draft-delete-0", &mut cx);
        assert!(drawn("MENU_ITEM-Intentional", &mut cx));
        assert!(drawn("MENU_ITEM-Not worth it", &mut cx));
        click("MENU_ITEM-Wrong", &mut cx);
        item.read_with(&cx, |item, _| assert!(item.drafts().is_empty()));
        assert!(bodies(&database).is_empty());
        assert_eq!(
            database.dismissals(&key()).expect("dismissals"),
            [Dismissal {
                path: "src/login.rs".into(),
                body: "**Logic** (must fix): Audit before the check?".into(),
                reason: DismissReason::Wrong,
            }]
        );

        let agent = install_agent(Ok("```json\n[]\n```".into()), &mut cx);
        start_pre_review(&item, &mut cx);
        let prompt = agent.prompts.borrow()[0].0.clone();
        assert!(
            prompt.contains("Past dismissals in this repository — don't repeat these.",),
            "{prompt}"
        );
        assert!(prompt.contains(
            "```text\n- src/login.rs [Wrong]: **Logic** (must fix): Audit before the check?\n```"
        ));
    }

    #[gpui::test]
    async fn agent_failure_is_shown_and_adds_nothing(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        install_agent(Ok("Looks fine to me.".into()), &mut cx);
        item.update_in(&mut cx, |item, window, cx| {
            item.workspace = Some(WeakEntity::new_invalid());
            item.start_agent_review(window, cx);
        });
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert!(item.drafts().is_empty());
            let (note, color) = item.agent_review_note().expect("note");
            assert!(note.contains("no ```json block"), "{note}");
            assert_eq!(*color, Color::Error);
        });
    }

    #[gpui::test]
    async fn done_replies_then_resolves(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(
            |request| {
                request
                    .args
                    .iter()
                    .any(|argument| argument.ends_with("/replies"))
            },
            Ok(r#"{"id":2002,"user":{"login":"maya"},"body":"Done","created_at":"t"}"#.into()),
        );
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| stdin.contains("resolveReviewThread("))
            },
            Ok(r#"{"data":{"x":{}}}"#.to_string()),
        );
        let task = item.update(&mut cx, |item, cx| item.mark_thread_done("PRRT_1", cx));
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("done");
        let requests = fake.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(request_json(&requests[0]), json!({"body": "Done"}));
        assert!(
            requests[1]
                .stdin
                .as_deref()
                .is_some_and(|stdin| stdin.contains("resolveReviewThread("))
        );
        item.read_with(&cx, |item, _| {
            let thread = &item.detail.threads[0];
            assert!(thread.is_resolved);
            assert_eq!(
                thread.comments.last().map(|comment| comment.body.as_str()),
                Some("Done")
            );
            assert!(item.finishing_threads.is_empty());
        });
    }

    #[gpui::test]
    async fn done_retry_does_not_reply_twice(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        item.update(&mut cx, |item, _| {
            let login = item.login.clone();
            item.detail.threads[0].comments.push(crate::ThreadComment {
                database_id: 77,
                author: login,
                body: "Done".into(),
                created_at: "t".into(),
            });
        });
        let fake = install_fake(&mut cx);
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| stdin.contains("resolveReviewThread("))
            },
            Ok(r#"{"data":{"x":{}}}"#.to_string()),
        );
        let task = item.update(&mut cx, |item, cx| item.mark_thread_done("PRRT_1", cx));
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("resolved");
        let requests = fake.requests();
        assert_eq!(requests.len(), 1, "only the resolve");
    }

    #[gpui::test]
    async fn failed_done_keeps_the_thread_open(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(
            |request| {
                request
                    .args
                    .iter()
                    .any(|argument| argument.ends_with("/replies"))
            },
            Err(crate::GhError::Failed {
                message: "gh: Forbidden (HTTP 403)".into(),
            }),
        );
        let task = item.update(&mut cx, |item, cx| item.mark_thread_done("PRRT_1", cx));
        cx.run_until_parked();
        assert!(task.now_or_never().expect("finished").is_err());
        assert_eq!(fake.requests().len(), 1);
        item.read_with(&cx, |item, _| {
            assert!(!item.detail.threads[0].is_resolved);
            assert!(item.thread_errors["PRRT_1"].contains("403"));
            assert!(item.finishing_threads.is_empty());
        });
    }

    #[gpui::test]
    async fn own_pr_is_recognised_and_shows_threads(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        item.update(&mut cx, |item, _| {
            item.login = "someone-else".into();
            assert!(!item.is_own_pr());
            item.login = item.detail.summary.author.to_uppercase();
            assert!(item.is_own_pr());
            assert_eq!(
                item.push_command().as_deref(),
                Some(
                    "git add -A && git commit -m 'Address review feedback' && git push 'https://github.com/maya/web.git' HEAD:'refs/heads/retry-uploads'"
                )
            );
        });
        item.update(&mut cx, |item, _| {
            item.detail.threads.push(suggestion_thread(false))
        });
        item.update_in(&mut cx, |item, window, cx| {
            item.show_thread("T_suggest", window, cx)
        });
        cx.run_until_parked();
        item.read_with(&cx, |item, cx| {
            assert!(
                item.steps()[item.selected_step()]
                    .files
                    .contains(&"src/login.rs".to_string())
            );
            let snapshot = item.multibuffer.read(cx).snapshot(cx);
            let cursor = item.editor.read(cx).selections.newest_anchor().head();
            let (_, point) = snapshot
                .point_to_buffer_point(cursor.to_point(&snapshot))
                .expect("cursor in a buffer");
            assert_eq!(point.row, 2, "cursor on line 3");
        });
    }

    #[gpui::test]
    async fn resolve_and_viewed_apply_immediately(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        let ok = || Ok(r#"{"data":{"x":{}}}"#.to_string());
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| stdin.contains("resolveReviewThread("))
            },
            ok(),
        );
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| stdin.contains("markFileAsViewed("))
            },
            ok(),
        );
        let resolved = |item: &Entity<ReviewItem>, cx: &mut VisualTestContext| {
            item.read_with(cx, |item, _| item.detail.threads[0].is_resolved)
        };
        let viewed = |item: &Entity<ReviewItem>, path: &str, cx: &mut VisualTestContext| {
            item.read_with(cx, |item, _| {
                item.detail
                    .files
                    .iter()
                    .any(|file| file.path == path && file.viewed)
            })
        };

        let task = item.update(&mut cx, |item, cx| {
            item.set_thread_resolved("PRRT_1", true, cx)
        });
        assert!(resolved(&item, &mut cx));
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("resolved");
        assert!(resolved(&item, &mut cx));
        assert_eq!(fake.requests().len(), 1);

        // No response is queued for unresolve, so FakeGh fails it.
        let task = item.update(&mut cx, |item, cx| {
            item.set_thread_resolved("PRRT_1", false, cx)
        });
        assert!(!resolved(&item, &mut cx));
        cx.run_until_parked();
        assert!(task.now_or_never().expect("finished").is_err());
        assert!(resolved(&item, &mut cx));
        item.read_with(&cx, |item, _| {
            assert!(item.thread_errors.contains_key("PRRT_1"))
        });
        assert_eq!(fake.requests().len(), 2);

        let task = item.update(&mut cx, |item, cx| {
            item.set_file_viewed("src/retry.rs", true, cx)
        });
        assert!(viewed(&item, "src/retry.rs", &mut cx));
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("viewed");
        assert!(viewed(&item, "src/retry.rs", &mut cx));
        let request = request_json(&fake.requests()[2]);
        assert_eq!(
            request["variables"],
            json!({"pullRequestId": "PR_482", "path": "src/retry.rs"})
        );

        let task = item.update(&mut cx, |item, cx| {
            item.set_file_viewed("src/upload.rs", false, cx)
        });
        assert!(!viewed(&item, "src/upload.rs", &mut cx));
        cx.run_until_parked();
        assert!(task.now_or_never().expect("finished").is_err());
        assert!(viewed(&item, "src/upload.rs", &mut cx));
        item.read_with(&cx, |item, _| {
            assert!(item.file_errors.contains_key("src/upload.rs"))
        });
        assert_eq!(fake.requests().len(), 4);
    }

    #[gpui::test]
    async fn a_file_is_viewed_on_github_once_all_its_steps_are(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| stdin.contains("markFileAsViewed("))
            },
            Ok(r#"{"data":{"x":{}}}"#.to_string()),
        );
        let step = |key: &str, path: &str| Step {
            key: key.into(),
            title: key.into(),
            subtitle: String::new(),
            files: vec![path.into()],
            functions: Vec::new(),
        };
        item.update(&mut cx, |item, _| {
            item.steps = vec![
                step("first", "src/retry.rs"),
                step("second", "src/retry.rs"),
                step("viewed on github", "src/upload.rs"),
            ];
        });
        let marks = |fake: &FakeGh| {
            fake.requests()
                .iter()
                .filter(|request| {
                    request
                        .stdin
                        .as_deref()
                        .is_some_and(|stdin| stdin.contains("FileAs"))
                })
                .count()
        };

        item.read_with(&cx, |item, _| {
            assert!(!item.step_viewed(&item.steps[0]));
            assert!(
                item.step_viewed(&item.steps[2]),
                "GitHub already has its file viewed"
            );
        });
        item.update(&mut cx, |item, cx| item.toggle_step_viewed(0, true, cx));
        cx.run_until_parked();
        assert_eq!(marks(&fake), 0, "another step still shows the file");

        item.update(&mut cx, |item, cx| item.toggle_step_viewed(1, true, cx));
        cx.run_until_parked();
        assert_eq!(marks(&fake), 1);
        item.read_with(&cx, |item, _| {
            assert!(item.file_viewed("src/retry.rs"));
            assert_eq!(item.progress_text(), "All steps viewed");
        });

        item.update(&mut cx, |item, cx| item.toggle_step_viewed(0, false, cx));
        cx.run_until_parked();
        assert_eq!(marks(&fake), 2, "unticking unmarks the file");
    }

    #[gpui::test]
    async fn no_requests_without_user_action(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, submit_drafts()).await;
        let fake = install_fake(&mut cx);
        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        type_text(&composer(&item, &mut cx), "Typed, not sent", &mut cx);
        let step = step_keys(&item, &mut cx)[0].clone();
        item.update(&mut cx, |item, cx| item.set_step_viewed(&step, true, cx));
        cx.run_until_parked();
        assert!(fake.requests().is_empty());
        assert!(
            database
                .viewed_steps(&key())
                .expect("steps")
                .contains_key(&step)
        );
        assert_eq!(database.drafts(&key()).expect("drafts").len(), 4);
    }

    #[gpui::test]
    async fn review_closed_mid_submit_leaves_no_row(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        // The draft's first save hasn't run when Submit is clicked, so it has no row id yet.
        let task = item.update_in(&mut cx, |item, window, cx| {
            let local_id = item.push_draft(current_draft("src/login.rs", 3, "    audit(user);"));
            item.queue_save(local_id, cx);
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.update(|window, _| window.remove_window());
        drop(item);
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        assert_eq!(
            fake.requests().iter().filter(|r| is_review_post(r)).count(),
            1
        );
        assert!(database.drafts(&key()).expect("drafts").is_empty());
    }

    #[gpui::test]
    async fn sending_row_reopens_marked_and_is_not_sent(cx: &mut TestAppContext) {
        let maybe_posted = Draft {
            post_state: crate::PostState::Sending,
            from_agent: false,
            ..current_draft("src/login.rs", 3, "    audit(user);")
        };
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![maybe_posted, current_draft("src/login.rs", 5, "}")],
        )
        .await;
        item.read_with(&cx, |item, _| {
            assert_eq!(item.drafts()[0].post_state, PostState::Sending);
            assert_eq!(item.sendable_drafts(), 1);
            assert_eq!(item.left_out_drafts(), 0);
        });
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        submit(&item, crate::ReviewEvent::Comment, &mut cx).expect("submitted");
        let posts: Vec<_> = fake.requests().into_iter().filter(is_review_post).collect();
        let comments = request_json(&posts[0])["comments"].clone();
        assert_eq!(comments.as_array().expect("comments").len(), 1);
        assert_eq!(comments[0]["line"], 4);
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].post_state, PostState::Sending);

        item.update(&mut cx, |item, cx| item.include_again(0, cx));
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(item.sendable_drafts(), 1));
        assert_eq!(
            database.drafts(&key()).expect("drafts")[0].post_state,
            PostState::Unposted
        );
    }

    #[gpui::test]
    async fn second_click_while_pending_sends_nothing(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        let ok = || Ok(r#"{"data":{"x":{}}}"#.to_string());
        fake.respond(|_| true, ok());
        fake.respond(|_| true, ok());
        item.update(&mut cx, |item, cx| {
            item.set_thread_resolved("PRRT_1", true, cx).detach();
            item.set_thread_resolved("PRRT_1", false, cx).detach();
            item.set_file_viewed("src/retry.rs", true, cx).detach();
            item.set_file_viewed("src/retry.rs", false, cx).detach();
        });
        cx.run_until_parked();
        assert_eq!(fake.requests().len(), 2);
        item.read_with(&cx, |item, _| {
            assert!(item.detail.threads[0].is_resolved);
            assert!(item.file_viewed("src/retry.rs"));
            assert!(item.resolving.is_empty() && item.marking_viewed.is_empty());
        });
    }

    #[gpui::test]
    async fn failed_refresh_after_post_shows_note(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, submit_drafts()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        let task = item.update_in(&mut cx, |item, window, cx| {
            item.summary_editor
                .update(cx, |editor, cx| editor.set_text("Sent", window, cx));
            let task = item.submit(crate::ReviewEvent::Comment, window, cx);
            item.summary_editor
                .update(cx, |editor, cx| editor.set_text("Typed after", window, cx));
            task
        });
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        item.read_with(&cx, |item, cx| {
            assert_eq!(
                item.submit_note.as_deref(),
                Some("Review posted. Couldn't refresh: FakeGh: unexpected request")
            );
            assert!(item.submit_error.is_none());
            assert!(item.drafts().is_empty());
            assert_eq!(item.summary_editor.read(cx).text(cx), "Typed after");
        });
    }

    #[gpui::test]
    async fn sending_general_draft_can_be_included_or_deleted(cx: &mut TestAppContext) {
        let stuck = Draft {
            outdated: true,
            general: true,
            post_state: PostState::Sending,
            from_agent: false,
            ..stored_draft("src/login.rs", 2, "    gone();")
        };
        let (item, mut cx, database) = open_with_drafts(cx, vec![stuck.clone(), stuck]).await;
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(
                gpui::AvailableSpace::Definite(px(1200.)),
                gpui::AvailableSpace::Definite(px(800.)),
            ),
            |_, _| item.clone().into_any_element(),
        );
        item.read_with(&cx, |item, _| {
            assert_eq!(item.posted_note(&item.drafts()[0]), Some(INTERRUPTED_NOTE));
            assert_eq!(item.sendable_drafts(), 0);
            assert_eq!(
                item.left_out_note().as_deref(),
                Some("2 may already be posted")
            );
        });

        item.update(&mut cx, |item, cx| item.include_again(0, cx));
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert_eq!(item.posted_note(&item.drafts()[0]), None);
            assert_eq!(item.sendable_drafts(), 1);
            assert_eq!(
                item.left_out_note().as_deref(),
                Some("1 may already be posted")
            );
        });

        item.update_in(&mut cx, |item, window, cx| item.cancel_draft(1, window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Discard");
        cx.run_until_parked();
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].post_state, PostState::Unposted);
    }

    #[gpui::test]
    async fn draft_inserted_during_submit_is_marked_before_post(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        let marked = database.clone();
        fake.respond(
            move |request| {
                is_review_post(request)
                    && marked.drafts(&key()).is_ok_and(|drafts| {
                        drafts.len() == 1 && drafts[0].post_state == PostState::Sending
                    })
            },
            Ok("{}".into()),
        );
        fake.respond(is_detail, Ok(DETAIL.into()));
        let task = item.update_in(&mut cx, |item, window, cx| {
            let local_id = item.push_draft(current_draft("src/login.rs", 3, "    audit(user);"));
            item.queue_save(local_id, cx);
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        assert!(database.drafts(&key()).expect("drafts").is_empty());
    }

    #[gpui::test]
    async fn draft_edited_during_submit_says_earlier_version_posted(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        let fake = FakeGh::new();
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        cx.update(|_, cx| {
            crate::set_gh_runner(
                Arc::new(GatedGh {
                    fake: fake.clone(),
                    gate: gate.shared(),
                }),
                cx,
            )
        });
        let task = item.update_in(&mut cx, |item, window, cx| {
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.run_until_parked();
        item.update(&mut cx, |item, cx| {
            item.drafts[0].body = "Edited while posting".into();
            item.queue_save(0, cx);
        });
        release.send(()).expect("release");
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.posted_note(&item.drafts()[0]),
                Some(EARLIER_VERSION_NOTE)
            );
            assert_eq!(item.sendable_drafts(), 0);
        });
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored[0].body, "Edited while posting");
        assert_eq!(stored[0].post_state, PostState::EarlierVersionPosted);
    }

    #[gpui::test]
    async fn failed_mark_sends_nothing(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        crate::store::FAILING_SAVES.set(1);
        let task = item.update_in(&mut cx, |item, window, cx| {
            let local_id = item.push_draft(current_draft("src/login.rs", 3, "    audit(user);"));
            item.queue_save(local_id, cx);
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.run_until_parked();
        assert!(task.now_or_never().expect("finished").is_err());
        assert!(fake.requests().is_empty());
        item.read_with(&cx, |item, _| {
            assert!(item.submit_error.is_some());
            assert_eq!(item.sendable_drafts(), 1);
        });
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].post_state, PostState::Unposted);
    }

    /// Answers like the fake, but only once the gate opens.
    struct GatedGh {
        fake: Arc<FakeGh>,
        gate: Gate,
    }

    impl GhRunner for GatedGh {
        fn run(
            &self,
            request: crate::GhRequest,
        ) -> futures::future::BoxFuture<'static, Result<String, GhError>> {
            let answer = self.fake.run(request);
            let gate = self.gate.clone();
            Box::pin(async move {
                gate.await.ok();
                answer.await
            })
        }
    }

    #[gpui::test]
    async fn edit_during_post_survives_closing_the_review(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        let fake = FakeGh::new();
        fake.respond(is_review_post, Ok("{}".into()));
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        cx.update(|_, cx| {
            crate::set_gh_runner(
                Arc::new(GatedGh {
                    fake: fake.clone(),
                    gate: gate.shared(),
                }),
                cx,
            )
        });
        let task = item.update_in(&mut cx, |item, window, cx| {
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(fake.requests().len(), 1);
        item.update(&mut cx, |item, cx| {
            item.drafts[0].body = "Edited while posting".into();
            item.queue_save(0, cx);
        });
        cx.run_until_parked();
        cx.update(|window, _| window.remove_window());
        drop(item);
        release.send(()).expect("release");
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].body, "Edited while posting");
        assert_eq!(stored[0].post_state, PostState::EarlierVersionPosted);
    }

    #[gpui::test]
    async fn drafts_removed_or_emptied_while_waiting_are_left_out(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![
                current_draft("src/login.rs", 3, "    audit(user);"),
                current_draft("src/login.rs", 5, "}"),
                current_draft("src/login.rs", 2, "    let user = user.trim();"),
            ],
        )
        .await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        let task = item.update_in(&mut cx, |item, window, cx| {
            let task = item.submit(crate::ReviewEvent::Comment, window, cx);
            item.discard_draft(0, cx);
            if let Some(index) = item.draft_index(1) {
                item.drafts[index].body = String::new();
            }
            item.queue_save(1, cx);
            task
        });
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        let posts: Vec<_> = fake.requests().into_iter().filter(is_review_post).collect();
        let comments = request_json(&posts[0])["comments"].clone();
        assert_eq!(comments.as_array().expect("comments").len(), 1);
        assert_eq!(comments[0]["line"], 2);
        assert!(database.drafts(&key()).expect("drafts").is_empty());
        item.read_with(&cx, |item, _| {
            assert!(item.submit_error.is_none());
            let left: Vec<_> = item
                .drafts()
                .iter()
                .map(|draft| (draft.body.as_str(), draft.post_state))
                .collect();
            assert_eq!(left, vec![("", PostState::Unposted)]);
        });
    }

    #[gpui::test]
    async fn since_review_mode_blocks_base_comments_and_viewed(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "b.rs": B_HEAD } }),
            vec![modified("src/b.rs", B_BASE)],
            Graph {
                functions: vec![function("callee", "src/b.rs", 0, 2)],
                calls: Vec::new(),
            },
            true,
            None,
        )
        .await;
        settle(&mut cx);
        item.update(&mut cx, |item, _| item.full_files = Some(Vec::new()));
        select_text(&item, "let a = 1;", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let step = step_keys(&item, &mut cx)[0].clone();
        item.update(&mut cx, |item, cx| item.set_step_viewed(&step, true, cx));
        item.read_with(&cx, |item, _| {
            assert!(item.drafts().is_empty());
            assert_eq!(item.since_review_note.as_deref(), Some(LEFT_COMMENT_NOTE));
            assert!(item.viewed_steps.is_empty());
        });
        select_text(&item, "let b = 2;", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.drafts().last().map(|draft| draft.side),
                Some(Side::Right)
            );
        });
    }

    #[gpui::test]
    async fn second_click_while_loading_cancels(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![login_with_hunk()],
            login_graph(),
            true,
            None,
        )
        .await;
        item.update_in(&mut cx, |item, window, cx| {
            item.loading_since_review = true;
            item.toggle_changes_since_review(&ToggleChangesSinceReview, window, cx);
            assert!(!item.loading_since_review);
            assert!(!item.showing_changes_since_review());
        });
    }

    #[gpui::test]
    async fn changes_since_review_toggle_offered_only_when_pushed(cx: &mut TestAppContext) {
        for (on_github, last_review, offered) in [
            (None, None, false),
            (None, Some("head"), false),
            (None, Some("older"), true),
            (Some("older"), None, true),
            (Some("older"), Some("head"), true),
            (Some("head"), Some("older"), false),
        ] {
            let database = test_database(cx);
            database
                .track(Tracked {
                    key: key(),
                    node_id: "PR_482".into(),
                    repo_dir: PathBuf::from(path!("/repo")),
                    worktree: PathBuf::from(path!("/repo")),
                })
                .await
                .expect("track");
            if let Some(commit) = last_review {
                database
                    .set_last_review(key(), commit.into())
                    .await
                    .expect("set last review");
            }
            let mut detail = parse_detail(&key(), DETAIL).expect("detail");
            detail.summary.last_review_commit = on_github.map(str::to_string);
            let (item, cx, _) = open_item_with_detail(
                cx,
                login_tree(),
                vec![login_with_hunk()],
                login_graph(),
                true,
                None,
                false,
                Some(database),
                detail,
            )
            .await;
            assert_eq!(
                item.read_with(&cx, |item, _| item.changes_since_review_offered()),
                offered,
                "github {on_github:?}, database {last_review:?}"
            );
        }
    }

    fn reformatted_files() -> (serde_json::Value, Vec<PrFile>, Graph) {
        (
            json!({ "src": { "b.rs": B_HEAD, "c.rs": C_REFORMATTED_HEAD } }),
            vec![
                modified("src/b.rs", B_BASE),
                modified("src/c.rs", C_REFORMATTED_BASE),
            ],
            Graph {
                functions: vec![
                    function("callee", "src/b.rs", 0, 2),
                    function("other", "src/c.rs", 0, 2),
                ],
                calls: Vec::new(),
            },
        )
    }

    async fn open_reformatted_with(
        cx: &mut TestAppContext,
        database: ReviewDb,
        files: Vec<crate::PrFile>,
    ) -> (Entity<ReviewItem>, VisualTestContext) {
        let (tree, changed, graph) = reformatted_files();
        let mut detail = parse_detail(&key(), DETAIL).expect("detail");
        detail.files = files;
        let (item, mut cx, _) = open_item_with_detail(
            cx,
            tree,
            changed,
            graph,
            true,
            None,
            false,
            Some(database),
            detail,
        )
        .await;
        settle(&mut cx);
        (item, cx)
    }

    fn github_file(path: &str, viewed: bool, changed_since_viewed: bool) -> crate::PrFile {
        crate::PrFile {
            path: path.into(),
            viewed,
            changed_since_viewed,
        }
    }

    #[gpui::test]
    async fn a_step_whose_diff_changed_since_it_was_ticked_is_left_to_review(
        cx: &mut TestAppContext,
    ) {
        let database = test_database(cx);
        let (first, first_cx) = open_reformatted_with(cx, database.clone(), Vec::new()).await;
        let (keys, prints) = first.read_with(&first_cx, |item, _| {
            let keys: Vec<String> = item.steps.iter().map(|step| step.key.clone()).collect();
            let prints: Vec<String> = item
                .steps
                .iter()
                .map(|step| item.step_fingerprint(step))
                .collect();
            (keys, prints)
        });
        assert_eq!(keys.len(), 2);
        let older = |fingerprint: &str| ViewedMark {
            head: Some("older".into()),
            fingerprint: Some(fingerprint.into()),
        };
        database
            .set_step_viewed(key(), keys[0].clone(), Some(older("before the push")))
            .await
            .expect("mark");
        database
            .set_step_viewed(key(), keys[1].clone(), Some(older(&prints[1])))
            .await
            .expect("mark");

        let (item, mut cx) = open_reformatted_with(cx, database.clone(), Vec::new()).await;
        assert!(drawn("photon-review-step-changed-0", &mut cx));
        assert!(
            !drawn("photon-review-step-changed-1", &mut cx),
            "an unchanged diff stays viewed after a push"
        );
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.progress_text(),
                "1 of 2 steps left to review · 1 changed since viewed"
            );
        });

        let fake = install_fake(&mut cx);
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| stdin.contains("FileAsViewed("))
            },
            Ok(r#"{"data":{"x":{}}}"#.to_string()),
        );
        click("photon-review-step-viewed-0", &mut cx);
        assert!(!drawn("photon-review-step-changed-0", &mut cx));
        item.read_with(&cx, |item, _| {
            assert_eq!(item.progress_text(), "All steps viewed");
        });
        let stored = database.viewed_steps(&key()).expect("steps");
        assert_eq!(stored[&keys[0]].head.as_deref(), Some("head"));
        assert_eq!(stored[&keys[0]].fingerprint.as_ref(), Some(&prints[0]));
        let viewed_at = database.viewed_files(&key()).expect("files");
        assert_eq!(viewed_at.get("src/b.rs").map(String::as_str), Some("head"));
    }

    #[gpui::test]
    async fn only_what_you_havent_reviewed_hides_viewed_files(cx: &mut TestAppContext) {
        let database = test_database(cx);
        let (_item, mut nothing_viewed) = open_reformatted_with(cx, database, Vec::new()).await;
        click("photon-review-view", &mut nothing_viewed);
        assert!(drawn("MENU_ITEM-Whitespace changes", &mut nothing_viewed));
        assert!(!drawn(
            "MENU_ITEM-Only what you haven't reviewed",
            &mut nothing_viewed
        ));

        let database = test_database(cx);
        let (item, mut cx) = open_reformatted_with(
            cx,
            database,
            vec![
                github_file("src/b.rs", true, false),
                github_file("src/c.rs", false, true),
            ],
        )
        .await;
        item.read_with(&cx, |item, _| {
            let states: Vec<ViewedState> = item
                .steps
                .iter()
                .map(|step| item.step_state(step))
                .collect();
            assert_eq!(
                states,
                [ViewedState::Viewed, ViewedState::ChangedSinceViewed],
                "GitHub's DISMISSED means changed since viewed"
            );
        });
        click("photon-review-view", &mut cx);
        click("MENU_ITEM-Only what you haven't reviewed", &mut cx);
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["src/c.rs"]);
        item.read_with(&cx, |item, _| {
            assert!(item.showing_unreviewed());
            assert!(!item.showing_changes_since_review());
            assert_eq!(
                item.since_review_note.as_deref(),
                Some(
                    "Showing the full diff of 1 file changed since you viewed it: the commit \
                     you viewed it at isn't available."
                )
            );
        });

        click("photon-review-view", &mut cx);
        click("MENU_ITEM-Only what you haven't reviewed", &mut cx);
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["src/b.rs", "src/c.rs"]);
        item.read_with(&cx, |item, _| assert!(!item.showing_unreviewed()));
    }

    #[gpui::test]
    async fn a_file_changed_since_viewed_at_an_unfetchable_head_is_shown_in_full(
        cx: &mut TestAppContext,
    ) {
        let database = test_database(cx);
        database
            // Not a commit id, so it's refused before any git command runs.
            .set_file_viewed_head(key(), "src/c.rs".into(), Some("older".into()))
            .await
            .expect("head");
        let (item, mut cx) = open_reformatted_with(
            cx,
            database,
            vec![
                github_file("src/b.rs", true, false),
                github_file("src/c.rs", false, true),
            ],
        )
        .await;
        click("photon-review-view", &mut cx);
        click("MENU_ITEM-Only what you haven't reviewed", &mut cx);
        settle(&mut cx);
        assert_eq!(step_files(&item, &mut cx), ["src/c.rs"]);
        item.read_with(&cx, |item, _| {
            assert!(!item.loading_since_review);
            assert!(
                item.since_review_note
                    .as_deref()
                    .is_some_and(|note| note.starts_with("Showing the full diff of 1 file")),
                "{:?}",
                item.since_review_note
            );
        });
    }

    fn hover(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        cx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();
    }

    fn set_login(item: &Entity<ReviewItem>, own_pr: bool, cx: &mut VisualTestContext) {
        item.update(cx, |item, cx| {
            item.login = if own_pr {
                item.detail.summary.author.clone()
            } else {
                "someone-else".into()
            };
            cx.notify();
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn the_header_has_one_primary_action(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        set_login(&item, false, &mut cx);
        assert!(drawn("photon-review-view", &mut cx));
        assert!(drawn("photon-review-more", &mut cx));
        assert!(drawn("photon-review-submit", &mut cx));
        assert!(!drawn("photon-review-push", &mut cx));

        // The author can still post comments, so Submit review stays beside Push changes….
        set_login(&item, true, &mut cx);
        assert!(drawn("photon-review-push", &mut cx));
        assert!(drawn("photon-review-submit", &mut cx));
    }

    #[gpui::test]
    async fn changes_since_review_is_a_view_menu_entry_with_a_dot_until_seen(
        cx: &mut TestAppContext,
    ) {
        let (item, mut not_offered) = open_reformatted(cx).await;
        item.update(&mut not_offered, |item, cx| {
            item.last_review = None;
            cx.notify();
        });
        assert!(!drawn("photon-review-view-dot", &mut not_offered));
        click("photon-review-view", &mut not_offered);
        assert!(drawn("MENU_ITEM-Whitespace changes", &mut not_offered));
        assert!(!drawn(
            "MENU_ITEM-Only changes since your review",
            &mut not_offered
        ));

        let mut detail = parse_detail(&key(), DETAIL).expect("detail");
        detail.summary.last_review_commit = Some("older".into());
        let (item, mut cx, _) = open_item_with_detail(
            cx,
            login_tree(),
            vec![login_with_hunk()],
            login_graph(),
            true,
            None,
            false,
            None,
            detail,
        )
        .await;
        assert!(drawn("photon-review-view-dot", &mut cx));
        click("photon-review-view", &mut cx);
        assert!(!drawn("photon-review-view-dot", &mut cx));
        click("MENU_ITEM-Only changes since your review", &mut cx);
        item.read_with(&cx, |item, _| {
            assert!(!item.loading_since_review);
            assert!(
                item.since_review_note
                    .as_deref()
                    .is_some_and(|note| note.starts_with("Couldn't fetch")),
                "{:?}",
                item.since_review_note
            );
        });
    }

    #[gpui::test]
    async fn the_more_menu_holds_the_occasional_actions(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        set_login(&item, false, &mut cx);
        item.update(&mut cx, |item, _| {
            item.workspace = Some(WeakEntity::new_invalid())
        });
        install_agent(Ok("Nothing:\n```json\n[]\n```".into()), &mut cx);
        click("photon-review-more", &mut cx);
        assert!(drawn("MENU_ITEM-Run locally", &mut cx));
        assert!(drawn("MENU_ITEM-Add your own checklist…", &mut cx));
        click("MENU_ITEM-Pre-review with agent", &mut cx);
        item.read_with(&cx, |item, _| {
            assert!(!item.agent_reviewing);
            assert!(
                item.agent_review_note()
                    .is_some_and(|(note, _)| note.contains("found nothing")),
                "{:?}",
                item.agent_review_note()
            );
        });

        click("photon-review-more", &mut cx);
        click("MENU_ITEM-Open on GitHub", &mut cx);
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/acme/web/pull/482")
        );

        set_login(&item, true, &mut cx);
        click("photon-review-more", &mut cx);
        assert!(drawn("MENU_ITEM-Run locally", &mut cx));
        assert!(!drawn("MENU_ITEM-Pre-review with agent", &mut cx));
    }

    #[gpui::test]
    async fn progress_is_steps_only_at_the_top_of_the_map(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        set_checklists(
            &mut cx,
            json!([{"name": "Login", "items": ["Audited"]}]),
            &[],
        );
        assert!(drawn("photon-review-progress", &mut cx));
        item.read_with(&cx, |item, _| {
            let progress = item.progress_text();
            assert!(progress.ends_with("steps left to review"), "{progress}");
        });
    }

    #[gpui::test]
    async fn a_checklist_item_is_one_checkbox_and_a_menu(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        set_checklists(
            &mut cx,
            json!([{"name": "Login", "items": ["Audited"]}]),
            &["tests_changed"],
        );
        let key = "settings:Login:Audited";
        let answer =
            |cx: &mut VisualTestContext| item.read_with(cx, |item, _| item.checklist_answer(key));
        click("photon-review-checklist-toggle", &mut cx);

        click("photon-check-box-settings:Login:Audited", &mut cx);
        assert_eq!(answer(&mut cx), Some(Answer::Yes));
        click("photon-check-box-settings:Login:Audited", &mut cx);
        assert_eq!(answer(&mut cx), None);

        click("photon-check-menu-settings:Login:Audited", &mut cx);
        assert!(!drawn("MENU_ITEM-Clear answer", &mut cx));
        click("MENU_ITEM-No, add a note", &mut cx);
        assert_eq!(answer(&mut cx), Some(Answer::No));
        item.read_with(&cx, |item, _| assert!(item.note_editors.contains_key(key)));
        click("photon-check-menu-settings:Login:Audited", &mut cx);
        click("MENU_ITEM-Doesn't apply", &mut cx);
        assert_eq!(answer(&mut cx), Some(Answer::NotApplicable));
        click("photon-check-menu-settings:Login:Audited", &mut cx);
        click("MENU_ITEM-Clear answer", &mut cx);
        assert_eq!(answer(&mut cx), None);

        // Against a failed auto check, the reviewer can only say it doesn't apply.
        let auto_key = "auto:tests_changed";
        click("photon-check-box-auto:tests_changed", &mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.checklist_answer(auto_key), None)
        });
        click("photon-check-menu-auto:tests_changed", &mut cx);
        assert!(!drawn("MENU_ITEM-No, add a note", &mut cx));
        click("MENU_ITEM-Doesn't apply", &mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.checklist_answer(auto_key), Some(Answer::NotApplicable))
        });
    }

    #[gpui::test]
    async fn photon_checks_and_author_boxes_fold_to_one_line(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        set_checklists(&mut cx, json!([]), &["checks_pass"]);
        click("photon-review-checklist-toggle", &mut cx);
        assert!(drawn("photon-review-photon-checked", &mut cx));
        assert!(drawn("photon-review-author-boxes", &mut cx));
        item.read_with(&cx, |item, _| {
            assert!(!item.photon_checked_open && !item.author_boxes_open)
        });
        click("photon-review-photon-checked", &mut cx);
        click("photon-review-author-boxes", &mut cx);
        item.read_with(&cx, |item, _| {
            assert!(item.photon_checked_open && item.author_boxes_open)
        });
    }

    #[gpui::test]
    async fn path_checklist_items_are_one_link_under_their_step(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        set_checklists(
            &mut cx,
            json!([{"name": "Login", "when": {"paths": ["src/login.rs"]}, "items": ["Rate limited", "Audited"]}]),
            &[],
        );
        assert!(!drawn("photon-review-checklist-body", &mut cx));
        assert!(!drawn("photon-check-box-settings:Login:Audited", &mut cx));
        click("photon-review-step-checklist", &mut cx);
        item.read_with(&cx, |item, _| assert!(item.checklist_open));
        assert!(drawn("photon-check-box-settings:Login:Audited", &mut cx));
    }

    #[gpui::test]
    async fn feedback_rows_keep_edit_and_done_behind_hover(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        set_login(&item, true, &mut cx);
        let fake = install_fake(&mut cx);
        fake.respond(
            |request| {
                request
                    .args
                    .iter()
                    .any(|argument| argument.ends_with("/replies"))
            },
            Ok(r#"{"id":2002,"user":{"login":"maya"},"body":"Done","created_at":"t"}"#.into()),
        );
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| stdin.contains("resolveReviewThread("))
            },
            Ok(r#"{"data":{"x":{}}}"#.to_string()),
        );
        assert!(drawn("photon-feedback-row-PRRT_1", &mut cx));
        hover("photon-feedback-row-PRRT_1", &mut cx);
        click("photon-feedback-done-PRRT_1", &mut cx);
        assert_eq!(fake.requests().len(), 2);
        item.read_with(&cx, |item, _| assert!(item.detail.threads[0].is_resolved));
    }

    #[gpui::test]
    async fn saved_drafts_show_edit_and_delete_on_hover(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(
            cx,
            vec![Draft {
                body: "Why audit here?".into(),
                ..current_draft("src/login.rs", 3, "    audit(user);")
            }],
        )
        .await;
        hover("photon-review-draft-edit-0", &mut cx);
        click("photon-review-draft-edit-0", &mut cx);
        item.read_with(&cx, |item, _| {
            assert!(item.draft_views[0].composer.is_some())
        });
    }

    #[gpui::test]
    async fn an_outdated_draft_has_one_menu(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(
            cx,
            vec![Draft {
                body: "Why audit here?".into(),
                ..current_draft("src/login.rs", 3, "    audit(user);")
            }],
        )
        .await;
        item.update(&mut cx, |item, cx| {
            item.drafts[0].outdated = true;
            cx.notify();
        });
        click("photon-review-outdated-menu-0", &mut cx);
        assert!(drawn("MENU_ITEM-Place on the selected lines", &mut cx));
        assert!(drawn("MENU_ITEM-Delete draft", &mut cx));
        click("MENU_ITEM-Post as a general comment", &mut cx);
        item.read_with(&cx, |item, _| assert!(item.drafts[0].general));
    }

    #[gpui::test]
    async fn the_submit_popover_posts_the_chosen_verdict(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, submit_drafts()).await;
        set_login(&item, false, &mut cx);
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        click("photon-review-submit", &mut cx);
        assert!(drawn("photon-review-verdict-Comment", &mut cx));
        assert!(drawn("photon-review-verdict-Request changes", &mut cx));
        click("photon-review-verdict-Approve", &mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.verdict(), ReviewEvent::Approve)
        });
        click("photon-review-submit-confirm", &mut cx);
        let posts: Vec<_> = fake.requests().into_iter().filter(is_review_post).collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(request_json(&posts[0])["event"], "APPROVE");
        assert!(!drawn("photon-review-submit-confirm", &mut cx));
    }

    #[gpui::test]
    async fn the_submit_popover_offers_only_comment_on_your_own_pr(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, submit_drafts()).await;
        set_login(&item, true, &mut cx);
        item.update(&mut cx, |item, _| item.verdict = ReviewEvent::Approve);
        click("photon-review-submit", &mut cx);
        assert!(drawn("photon-review-submit-confirm", &mut cx));
        assert!(!drawn("photon-review-verdict-Approve", &mut cx));
        item.read_with(&cx, |item, _| {
            assert_eq!(item.verdict(), ReviewEvent::Comment)
        });
    }

    #[gpui::test]
    async fn a_failed_submit_keeps_the_popover_open(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, submit_drafts()).await;
        set_login(&item, false, &mut cx);
        let fake = install_fake(&mut cx);
        fake.respond(
            is_review_post,
            Err(GhError::Failed {
                message: "gh: HTTP 422".into(),
            }),
        );
        click("photon-review-submit", &mut cx);
        click("photon-review-submit-confirm", &mut cx);
        assert!(drawn("photon-review-submit-confirm", &mut cx));
        item.read_with(&cx, |item, _| {
            assert_eq!(item.submit_error.as_deref(), Some("gh: HTTP 422"))
        });
    }

    fn is_merge(request: &crate::GhRequest) -> bool {
        request
            .stdin
            .as_deref()
            .is_some_and(|stdin| stdin.contains("mergePullRequest"))
    }

    const MERGED: &str = r#"{"data":{"mergePullRequest":{"pullRequest":{"state":"MERGED"}}}}"#;

    fn merged_detail() -> String {
        DETAIL.replace(r#""state": "OPEN""#, r#""state": "MERGED""#)
    }

    fn readiness(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> String {
        item.read_with(cx, |item, _| item.detail.readiness().label())
    }

    #[gpui::test]
    async fn the_readiness_pill_lists_checks_and_opens_one(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        assert_eq!(readiness(&item, &mut cx), "Ready to merge");
        click("photon-review-readiness", &mut cx);
        assert!(drawn("MENU_ITEM-build: passed", &mut cx));
        assert!(drawn("MENU_ITEM-lint: running", &mut cx));
        assert!(drawn("MENU_ITEM-ci/deploy: failed", &mut cx));
        click("MENU_ITEM-lint: running", &mut cx);
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/acme/web/runs/2")
        );
    }

    #[gpui::test]
    async fn the_pill_follows_each_refresh(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(
            is_detail,
            Ok(DETAIL.replace(r#""mergeable": "MERGEABLE""#, r#""mergeable": "UNKNOWN""#)),
        );
        let refresh = item.update_in(&mut cx, |item, window, cx| item.refresh_detail(window, cx));
        cx.run_until_parked();
        refresh.now_or_never().expect("refreshed").expect("refresh");
        assert_eq!(readiness(&item, &mut cx), "Checking…");
        assert!(drawn("photon-review-readiness", &mut cx));
        fake.respond(is_detail, Ok(DETAIL.into()));
        let refresh = item.update_in(&mut cx, |item, window, cx| item.refresh_detail(window, cx));
        cx.run_until_parked();
        refresh.now_or_never().expect("refreshed").expect("refresh");
        assert_eq!(readiness(&item, &mut cx), "Ready to merge");
    }

    #[gpui::test]
    async fn merge_asks_then_merges_the_reviewed_head(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, submit_drafts()).await;
        let fake = install_fake(&mut cx);

        click("photon-review-merge", &mut cx);
        assert_eq!(
            cx.pending_prompt(),
            Some((
                "Squash and merge #482 into main?".to_string(),
                "Your 3 unsent comments won't be posted.".to_string()
            ))
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(fake.requests().is_empty());

        fake.respond(is_merge, Ok(MERGED.into()));
        fake.respond(is_detail, Ok(merged_detail()));
        click("photon-review-merge", &mut cx);
        cx.simulate_prompt_answer("Merge");
        cx.run_until_parked();
        let merges: Vec<_> = fake.requests().into_iter().filter(is_merge).collect();
        assert_eq!(merges.len(), 1);
        assert_eq!(
            request_json(&merges[0])["variables"],
            json!({
                "pullRequestId": "PR_482",
                "mergeMethod": "SQUASH",
                "expectedHeadOid": "head",
            })
        );
        assert_eq!(readiness(&item, &mut cx), "Merged");
        assert!(!drawn("photon-review-merge", &mut cx));
        assert!(!drawn("photon-review-merge-error", &mut cx));
    }

    #[gpui::test]
    async fn the_merge_menu_offers_only_the_other_allowed_methods(cx: &mut TestAppContext) {
        let (_item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        click("photon-review-merge-methods", &mut cx);
        assert!(drawn("MENU_ITEM-Merge", &mut cx));
        assert!(!drawn("MENU_ITEM-Squash and Merge", &mut cx));
        assert!(!drawn("MENU_ITEM-Rebase and Merge", &mut cx));
        fake.respond(is_merge, Ok(MERGED.into()));
        fake.respond(is_detail, Ok(merged_detail()));
        click("MENU_ITEM-Merge", &mut cx);
        let (message, _) = cx.pending_prompt().expect("prompt");
        assert_eq!(message, "Merge #482 into main?");
        cx.simulate_prompt_answer("Merge");
        cx.run_until_parked();
        let merges: Vec<_> = fake.requests().into_iter().filter(is_merge).collect();
        assert_eq!(
            request_json(&merges[0])["variables"]["mergeMethod"],
            "MERGE"
        );
    }

    #[gpui::test]
    async fn a_moved_head_is_explained_and_nothing_merges(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(
            is_merge,
            Ok(r#"{"data":null,"errors":[{"message":"Head branch was modified. Review and try the merge again."}]}"#.into()),
        );
        click("photon-review-merge", &mut cx);
        cx.simulate_prompt_answer("Merge");
        cx.run_until_parked();
        assert!(drawn("photon-review-merge-error", &mut cx));
        item.read_with(&cx, |item, _| {
            assert_eq!(item.merge_error.as_deref(), Some(crate::HEAD_MOVED_MESSAGE));
            assert_eq!(item.detail.state, PrState::Open);
        });
        assert!(drawn("photon-review-merge", &mut cx));
    }

    #[gpui::test]
    async fn drafts_and_conflicts_cannot_be_merged(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        install_fake(&mut cx);
        for block in ["draft", "conflicts"] {
            item.update(&mut cx, |item, cx| {
                item.detail.merge.is_draft = block == "draft";
                item.detail.merge.mergeable = if block == "conflicts" {
                    "CONFLICTING".into()
                } else {
                    "MERGEABLE".into()
                };
                cx.notify();
            });
            click("photon-review-merge", &mut cx);
            assert!(!cx.has_pending_prompt(), "{block}");
        }
        assert_eq!(readiness(&item, &mut cx), "Conflicts");
    }

    const TIMELINE: &str = include_str!("../test_fixtures/timeline.json");

    fn is_timeline(request: &crate::GhRequest) -> bool {
        request
            .stdin
            .as_deref()
            .is_some_and(|stdin| stdin.contains("timelineItems"))
    }

    fn is_comment_post(request: &crate::GhRequest) -> bool {
        request
            .args
            .iter()
            .any(|argument| argument.ends_with("/issues/482/comments"))
    }

    fn timeline_requests(fake: &FakeGh) -> usize {
        fake.requests().iter().filter(|r| is_timeline(r)).count()
    }

    fn conversation_composer(
        item: &Entity<ReviewItem>,
        cx: &mut VisualTestContext,
    ) -> Entity<Editor> {
        item.read_with(cx, |item, _| {
            item.conversation
                .as_ref()
                .expect("conversation")
                .composer
                .clone()
        })
    }

    #[gpui::test]
    async fn the_conversation_is_fetched_when_first_opened_and_shown_in_order(
        cx: &mut TestAppContext,
    ) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        assert!(drawn("photon-review-conversation", &mut cx));
        assert!(!drawn("photon-review-description", &mut cx));
        assert_eq!(timeline_requests(&fake), 0);

        fake.respond(is_timeline, Ok(TIMELINE.into()));
        click("photon-review-conversation", &mut cx);
        assert_eq!(timeline_requests(&fake), 1);
        let selectors = [
            "photon-review-description",
            "photon-review-timeline-0",
            "photon-review-timeline-1",
            "photon-review-timeline-2",
            "photon-review-timeline-3",
            "photon-review-timeline-4",
        ];
        let tops: Vec<Pixels> = selectors
            .iter()
            .map(|selector| {
                cx.debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} is drawn"))
                    .top()
            })
            .collect();
        assert!(tops.windows(2).all(|pair| pair[0] < pair[1]), "{tops:?}");
        assert!(!drawn("photon-review-timeline-5", &mut cx));
        item.read_with(&cx, |item, _| {
            let events = item.conversation.as_ref().and_then(|c| c.events.as_ref());
            let kinds: Vec<&str> = events
                .expect("events")
                .iter()
                .map(|(event, body)| match event {
                    TimelineEvent::Comment(_) => "comment",
                    TimelineEvent::Review { .. } if body.is_some() => "review with body",
                    TimelineEvent::Review { .. } => "review",
                    TimelineEvent::Commit { .. } => "commit",
                })
                .collect();
            assert_eq!(
                kinds,
                ["commit", "comment", "review with body", "commit", "review"]
            );
        });

        // Back to a step shows the code; coming back reuses what was fetched.
        item.update_in(&mut cx, |item, window, cx| item.select_step(0, window, cx));
        assert!(!drawn("photon-review-description", &mut cx));
        click("photon-review-conversation", &mut cx);
        assert!(drawn("photon-review-timeline-4", &mut cx));
        assert_eq!(timeline_requests(&fake), 1);
    }

    #[gpui::test]
    async fn a_refresh_fetches_the_conversation_again_only_while_it_shows(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        let refresh = |item: &Entity<ReviewItem>, cx: &mut VisualTestContext| {
            let task = item.update_in(cx, |item, window, cx| item.refresh_detail(window, cx));
            cx.run_until_parked();
            task.now_or_never().expect("refreshed").expect("refresh");
        };
        fake.respond(is_detail, Ok(DETAIL.into()));
        refresh(&item, &mut cx);
        assert_eq!(timeline_requests(&fake), 0);

        fake.respond(is_timeline, Ok(TIMELINE.into()));
        click("photon-review-conversation", &mut cx);
        fake.respond(is_detail, Ok(DETAIL.into()));
        fake.respond(is_timeline, Ok(TIMELINE.into()));
        refresh(&item, &mut cx);
        assert_eq!(timeline_requests(&fake), 2);

        item.update_in(&mut cx, |item, window, cx| item.select_step(0, window, cx));
        fake.respond(is_detail, Ok(DETAIL.into()));
        refresh(&item, &mut cx);
        assert_eq!(timeline_requests(&fake), 2);
        fake.respond(is_timeline, Ok(TIMELINE.into()));
        click("photon-review-conversation", &mut cx);
        assert_eq!(timeline_requests(&fake), 3);
    }

    #[gpui::test]
    async fn a_comment_posts_and_is_appended(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_timeline, Ok(TIMELINE.into()));
        click("photon-review-conversation", &mut cx);
        fake.respond(
            is_comment_post,
            Ok(r#"{"id":3002,"user":{"login":"alice"},"body":"Ship it","created_at":"2026-10-05T09:00:00Z"}"#.into()),
        );
        let composer = conversation_composer(&item, &mut cx);
        type_text(&composer, "Ship it", &mut cx);
        click("photon-review-comment", &mut cx);
        let posts: Vec<_> = fake
            .requests()
            .into_iter()
            .filter(is_comment_post)
            .collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(request_json(&posts[0]), json!({"body": "Ship it"}));
        assert!(drawn("photon-review-timeline-5", &mut cx));
        item.read_with(&cx, |item, cx| {
            let conversation = item.conversation.as_ref().expect("conversation");
            let last = conversation
                .events
                .as_ref()
                .and_then(|events| events.last())
                .map(|(event, _)| event.clone());
            let Some(TimelineEvent::Comment(comment)) = last else {
                panic!("expected the new comment last, got {last:?}");
            };
            assert_eq!(
                (comment.author.as_str(), comment.body.as_str()),
                ("alice", "Ship it")
            );
            assert_eq!(conversation.composer.read(cx).text(cx), "");
            assert_eq!(item.detail.comment_count, 3);
        });
        assert!(!drawn("photon-review-comment-error", &mut cx));
    }

    #[gpui::test]
    async fn a_failed_comment_keeps_the_text(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_timeline, Ok(TIMELINE.into()));
        click("photon-review-conversation", &mut cx);
        fake.respond(
            is_comment_post,
            Err(GhError::Failed {
                message: "gh: HTTP 403".into(),
            }),
        );
        let composer = conversation_composer(&item, &mut cx);
        type_text(&composer, "Ship it", &mut cx);
        click("photon-review-comment", &mut cx);
        assert!(drawn("photon-review-comment-error", &mut cx));
        assert!(!drawn("photon-review-timeline-5", &mut cx));
        composer.read_with(&cx, |editor, cx| assert_eq!(editor.text(cx), "Ship it"));
    }
}
