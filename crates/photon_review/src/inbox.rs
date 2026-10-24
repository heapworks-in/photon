use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;

use anyhow::Result;
use futures::future::join_all;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Pixels, Render, SharedString, Subscription, Task, WeakEntity, Window, actions, px,
};
use settings::{Settings as _, SettingsStore};
use task::{SpawnInTerminal, TaskId};
use ui::{Tab, TintColor, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    ChecksState, GhError, GhFix, GhRunner, HostAuth, Inbox, InboxSection, PrKey, PrReviewSettings,
    PrSummary, RepoId, ReviewDb, SinceReview, Tracked, auth_status_request,
    checkout::{
        ensure_safe_segment, force_remove_worktree, parse_remote_url, review_root, worktree_path,
    },
    cleanup_checkout, gh_runner, inbox_request, open_review, parse_auth_status, parse_inbox,
    run_locally,
};

actions!(
    photon_review,
    [
        /// Toggles the pull request inbox.
        ToggleInbox,
        /// Reloads the pull request inbox from GitHub.
        RefreshInbox
    ]
);

const INBOX_PANEL_NAME: &str = "PhotonReviewInbox";
const DEFAULT_HOST: &str = "github.com";
const EMPTY_INBOX_TEXT: &str = "No open pull requests";
const EMPTY_INBOX_DETAIL: &str = "Pull requests that need your review, and your own, show up here.";
const NO_REPOSITORY_TEXT: &str = "No GitHub repository";
const NO_REPOSITORY_DETAIL: &str =
    "Open a folder with a GitHub repository to see its pull requests.";

pub(crate) fn init_inbox(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleInbox, window, cx| {
            let Some(panel) = workspace.panel::<InboxPanel>(cx) else {
                return;
            };
            panel.update(cx, |panel, _| panel.user_activated = true);
            if workspace.toggle_panel_focus::<InboxPanel>(window, cx) {
                panel.update(cx, |panel, cx| {
                    if matches!(panel.state, InboxState::Idle) {
                        panel.refresh(cx);
                    }
                });
            }
        });
        workspace.register_action(|workspace, _: &RefreshInbox, _, cx| {
            if let Some(panel) = workspace.panel::<InboxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.refresh(cx));
            }
        });
    })
    .detach();
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostInbox {
    pub host: String,
    pub result: Result<Inbox, GhError>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InboxState {
    Idle,
    Loading,
    /// The window has no repository with a GitHub remote, so there is nothing to ask GitHub.
    NoRepository,
    /// Every repository is on a host that isn't GitHub, like gitlab.com.
    NotGitHub(Vec<String>),
    Loaded(Vec<HostInbox>),
    Failed(GhError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowMeta {
    pub title: String,
    pub meta: String,
    pub badges: Vec<String>,
    pub primary_action: &'static str,
}

fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

pub fn row_meta(summary: &PrSummary, has_drafts: bool) -> RowMeta {
    let key = &summary.key;
    let repository = if key.host == DEFAULT_HOST {
        format!("{}/{}", key.owner, key.repo)
    } else {
        format!("{}/{}/{}", key.host, key.owner, key.repo)
    };
    let mut badges = Vec::new();
    match summary.checks {
        ChecksState::Passing => badges.push("checks passing".to_string()),
        ChecksState::Failing => badges.push("checks failing".to_string()),
        ChecksState::Pending => badges.push("checks pending".to_string()),
        ChecksState::Unknown => {}
    }
    if summary.unresolved_threads > 0 {
        let count = summary.unresolved_threads;
        badges.push(format!(
            "{count} unresolved thread{}",
            if count == 1 { "" } else { "s" }
        ));
    }
    match summary.since_review {
        SinceReview::NewCommits(count) => {
            badges.push(format!("{} since your review", plural(count, "new commit")))
        }
        SinceReview::ForcePushed => badges.push("force-pushed since your review".to_string()),
        SinceReview::NotReviewed | SinceReview::UpToDate => {}
    }
    RowMeta {
        title: summary.title.clone(),
        meta: format!("#{} · {} · {repository}", key.number, summary.author),
        badges,
        primary_action: if summary.last_review_commit.is_some() || has_drafts {
            "Continue review"
        } else {
            "Start review"
        },
    }
}

fn section_label(name: &str, count: usize) -> String {
    format!("{name} · {count}")
}

fn host_line(hosts: &[HostInbox]) -> String {
    hosts
        .iter()
        .map(|host| host.host.as_str())
        .collect::<Vec<_>>()
        .join(" + ")
}

fn badge_color(badge: &str) -> Color {
    if badge.starts_with("checks failing") {
        Color::Error
    } else if badge.starts_with("checks passing") {
        Color::Success
    } else if badge.starts_with("checks pending") {
        Color::Warning
    } else if badge.contains("since your review") {
        Color::Accent
    } else {
        Color::Muted
    }
}

pub struct InboxPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    state: InboxState,
    refreshing: bool,
    in_flight_cleanups: InFlightCleanups,
    // Dock restoration calls `set_active(true)` at startup; nothing may load until the user acts.
    user_activated: bool,
    with_drafts: HashSet<PrKey>,
    detected: Vec<RepoId>,
    load_task: Task<()>,
    _settings_subscription: Subscription,
}

impl InboxPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            cx.update(|_, cx| {
                cx.new(|cx| {
                    let mut sections = PrReviewSettings::get_global(cx).sections.clone();
                    let settings_subscription =
                        cx.observe_global::<SettingsStore>(move |this: &mut Self, cx| {
                            let current = &PrReviewSettings::get_global(cx).sections;
                            if *current != sections {
                                sections = current.clone();
                                if this.user_activated {
                                    this.refresh(cx);
                                }
                            }
                        });
                    Self {
                        workspace,
                        focus_handle: cx.focus_handle(),
                        state: InboxState::Idle,
                        refreshing: false,
                        in_flight_cleanups: Arc::default(),
                        user_activated: false,
                        with_drafts: HashSet::default(),
                        detected: Vec::new(),
                        load_task: Task::ready(()),
                        _settings_subscription: settings_subscription,
                    }
                })
            })
        })
    }

    pub fn state(&self) -> &InboxState {
        &self.state
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let runner = gh_runner(cx);
        let database = ReviewDb::global(cx);
        self.user_activated = true;
        self.refreshing = true;
        let in_flight = self.in_flight_cleanups.clone();
        if matches!(self.state, InboxState::Idle) {
            self.state = InboxState::Loading;
        }
        self.load_task = cx.spawn(async move |this, cx| {
            // Read here, not before spawning: refresh usually runs inside the workspace's own update.
            let Ok((repos, sections)) = this.update(cx, |this, cx| {
                let sections = PrReviewSettings::get_global(cx).sections.clone();
                (window_repos(&this.workspace, cx), sections)
            }) else {
                return;
            };
            let (state, closed, with_drafts, repos) = cx
                .background_spawn({
                    let database = database.clone();
                    async move {
                        let repos = resolve_ssh_aliases(&runner, repos).await;
                        let (state, closed) =
                            load_inbox(runner, database.clone(), repos.clone(), sections).await;
                        let with_drafts = prs_with_drafts(&state, &database);
                        (state, closed, with_drafts, repos)
                    }
                })
                .await;
            // Detached so a newer refresh never cancels a half-finished cleanup.
            cx.background_spawn(cleanup_closed(closed, database, review_root(), in_flight))
                .detach();
            this.update(cx, |this, cx| {
                this.state = state;
                this.with_drafts = with_drafts;
                this.detected = repos;
                this.refreshing = false;
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    pub fn detected_line(&self) -> String {
        if self.detected.is_empty() {
            return String::new();
        }
        let repos: Vec<String> = self
            .detected
            .iter()
            .map(|repo| format!("{}/{}/{}", repo.host, repo.owner, repo.repo))
            .collect();
        format!("Found {}", repos.join(", "))
    }

    /// Runs the fix in a terminal, since it may prompt, and reloads once it ends.
    pub fn run_fix(&mut self, fix: GhFix, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(workspace), Some((command, args))) =
            (self.workspace.upgrade(), fix.args.split_first())
        else {
            return;
        };
        let line = fix.args.join(" ");
        let spawn = SpawnInTerminal {
            id: TaskId(format!("photon-review-fix: {line}")),
            label: line.clone(),
            full_label: line.clone(),
            command_label: line,
            command: Some(command.clone()),
            args: args.to_vec(),
            use_new_terminal: true,
            show_command: true,
            show_summary: true,
            ..SpawnInTerminal::default()
        };
        let running = workspace.update(cx, |workspace, cx| {
            workspace.spawn_in_terminal(spawn, window, cx)
        });
        cx.spawn(async move |this, cx| {
            running.await;
            this.update(cx, |this, cx| this.refresh(cx)).log_err();
        })
        .detach();
    }

    fn render_fix(&self, fix: GhFix, cx: &mut Context<Self>) -> impl IntoElement {
        crate::button(
            SharedString::from(format!("photon-review-fix-{}", fix.label)),
            fix.label.clone(),
        )
        .style(ButtonStyle::Tinted(TintColor::Accent))
        .on_click(cx.listener(move |this, _, window, cx| this.run_fix(fix.clone(), window, cx)))
    }
}

/// `git@work-gh:org/repo` names an SSH alias; gh only knows the host it stands for.
async fn resolve_ssh_aliases(runner: &Arc<dyn GhRunner>, repos: Vec<RepoId>) -> Vec<RepoId> {
    let mut resolved = Vec::new();
    for mut repo in repos {
        if let Some(host) = runner.ssh_hostname(&repo.host).await {
            repo.host = host;
        }
        if !resolved.contains(&repo) {
            resolved.push(repo);
        }
    }
    resolved
}

// ponytail: well-known non-GitHub hosts only; a self-hosted GitLab shows as "not logged in".
fn is_github_host(host: &str) -> bool {
    !["gitlab", "bitbucket", "codeberg", "azure.com", "sr.ht"]
        .iter()
        .any(|other| host.contains(other))
}

/// GitHub repositories behind the `origin` and `upstream` remotes of the window's projects.
fn window_repos(workspace: &WeakEntity<Workspace>, cx: &App) -> Vec<RepoId> {
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    // Names go into a search query; anything outside GitHub's name characters is dropped.
    let searchable = |part: &str| {
        part.chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '-' | '_' | '.'))
    };
    let mut repos = Vec::new();
    for repository in workspace
        .read(cx)
        .project()
        .read(cx)
        .repositories(cx)
        .values()
    {
        let repository = repository.read(cx);
        for url in [
            &repository.remote_origin_url,
            &repository.remote_upstream_url,
        ]
        .into_iter()
        .flatten()
        {
            if let Some(repo) = parse_remote_url(url)
                && searchable(&repo.owner)
                && searchable(&repo.repo)
                && !repos.contains(&repo)
            {
                repos.push(repo);
            }
        }
    }
    repos
}

fn failed(error: anyhow::Error) -> GhError {
    GhError::Failed {
        message: format!("{error:#}"),
    }
}

fn prs_with_drafts(state: &InboxState, database: &ReviewDb) -> HashSet<PrKey> {
    let InboxState::Loaded(hosts) = state else {
        return HashSet::default();
    };
    hosts
        .iter()
        .filter_map(|host| host.result.as_ref().ok())
        .flat_map(|inbox| inbox.sections.iter())
        .flat_map(|(_, summaries)| summaries.iter())
        .filter(|summary| {
            database
                .drafts(&summary.key)
                .log_err()
                .is_some_and(|drafts| !drafts.is_empty())
        })
        .map(|summary| summary.key.clone())
        .collect()
}

async fn load_inbox(
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
    repos: Vec<RepoId>,
    sections: Vec<InboxSection>,
) -> (InboxState, Vec<Tracked>) {
    if repos.is_empty() {
        return (InboxState::NoRepository, Vec::new());
    }
    let (repos, elsewhere): (Vec<_>, Vec<_>) = repos
        .into_iter()
        .partition(|repo| is_github_host(&repo.host));
    if repos.is_empty() {
        let mut hosts: Vec<String> = elsewhere.into_iter().map(|repo| repo.host).collect();
        hosts.sort();
        hosts.dedup();
        return (InboxState::NotGitHub(hosts), Vec::new());
    }
    let hosts = match runner.run(auth_status_request()).await {
        Ok(output) => match parse_auth_status(&output) {
            Ok(hosts) => hosts,
            Err(error) => return (InboxState::Failed(failed(error)), Vec::new()),
        },
        Err(error) => return (InboxState::Failed(error), Vec::new()),
    };
    let mut hosts: Vec<_> = hosts
        .into_iter()
        .filter(|auth| {
            repos
                .iter()
                .any(|repo| repo.host == auth.host.to_lowercase())
        })
        .collect();
    if hosts.is_empty() {
        let host = repos.first().map(|repo| repo.host.clone());
        return (InboxState::Failed(GhError::LoggedOut { host }), Vec::new());
    }
    // A host gh has never logged in to is missing from `gh auth status`; list it as logged out.
    for repo in &repos {
        if !hosts
            .iter()
            .any(|auth: &HostAuth| auth.host.to_lowercase() == repo.host)
        {
            hosts.push(HostAuth {
                host: repo.host.clone(),
                login: None,
            });
        }
    }

    let tracked = database.tracked().log_err().unwrap_or_default();
    let results = join_all(hosts.into_iter().map(|auth| {
        let runner = runner.clone();
        let sections = sections.clone();
        let host_repos: Vec<String> = repos
            .iter()
            .filter(|repo| repo.host == auth.host.to_lowercase())
            .map(|repo| format!("{}/{}", repo.owner, repo.repo))
            .collect();
        let node_ids: Vec<String> = tracked
            .iter()
            .filter(|tracked| tracked.key.host == auth.host)
            .map(|tracked| tracked.node_id.clone())
            .collect();
        async move {
            let result = match auth.login {
                None => Err(GhError::LoggedOut {
                    host: Some(auth.host.clone()),
                }),
                Some(login) => match runner
                    .run(inbox_request(
                        &auth.host,
                        &login,
                        &host_repos,
                        &sections,
                        &node_ids,
                    ))
                    .await
                {
                    Ok(output) => {
                        parse_inbox(&auth.host, &output, &sections, &node_ids).map_err(failed)
                    }
                    Err(error) => Err(error),
                },
            };
            HostInbox {
                host: auth.host,
                result,
            }
        }
    }))
    .await;

    let mut closed = Vec::new();
    for host in &results {
        let Ok(inbox) = &host.result else { continue };
        // Drafts are kept: they live apart from the tracked row.
        for gone in tracked.iter().filter(|tracked| {
            tracked.key.host == host.host && inbox.gone_tracked.contains(&tracked.node_id)
        }) {
            database.untrack(gone.key.clone()).await.log_err();
        }
        closed.extend(
            tracked
                .iter()
                .filter(|tracked| {
                    tracked.key.host == host.host && inbox.closed_tracked.contains(&tracked.node_id)
                })
                .cloned(),
        );
    }
    (InboxState::Loaded(results), closed)
}

type InFlightCleanups = Arc<Mutex<HashSet<PrKey>>>;

async fn cleanup_closed(
    closed: Vec<Tracked>,
    database: ReviewDb,
    review_root: PathBuf,
    in_flight: InFlightCleanups,
) {
    for pull_request in closed {
        let key = pull_request.key.clone();
        if !in_flight.lock().insert(key.clone()) {
            continue;
        }
        cleanup_one(pull_request, &database, &review_root).await;
        in_flight.lock().remove(&key);
    }
}

async fn cleanup_one(pull_request: Tracked, database: &ReviewDb, review_root: &Path) {
    let Tracked {
        key,
        repo_dir,
        worktree,
        ..
    } = pull_request;
    // Only the exact path Photon would have created may be removed; a stored path is untrusted
    // because it could point at the user's own worktree or, via `..`, anywhere on disk.
    let is_photon_worktree = ensure_safe_segment("host", &key.host).is_ok()
        && ensure_safe_segment("owner", &key.owner).is_ok()
        && ensure_safe_segment("repo", &key.repo).is_ok()
        && worktree == worktree_path(review_root, &key);
    if !is_photon_worktree {
        Err::<(), _>(anyhow::anyhow!(
            "not removing {}: not Photon's worktree for {}/{}#{}",
            worktree.display(),
            key.owner,
            key.repo,
            key.number
        ))
        .log_err();
        database.untrack(key).await.log_err();
        return;
    }
    let mut cleaned = cleanup_checkout(&repo_dir, &key, &worktree).await;
    if cleaned.is_err() && worktree.exists() {
        // Never `worktree prune`: it would touch the user's other worktrees.
        let removed = match force_remove_worktree(&repo_dir, &worktree).await {
            Ok(()) => Ok(()),
            Err(error) => {
                Err::<(), _>(error).log_err();
                std::fs::remove_dir_all(&worktree).map_err(anyhow::Error::from)
            }
        };
        cleaned = match removed {
            Ok(()) => cleanup_checkout(&repo_dir, &key, &worktree).await,
            Err(error) => Err(error),
        };
    }
    if cleaned.log_err().is_some() {
        database.untrack(key).await.log_err();
    }
}

impl EventEmitter<PanelEvent> for InboxPanel {}

impl Focusable for InboxPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for InboxPanel {
    fn persistent_name() -> &'static str {
        INBOX_PANEL_NAME
    }

    fn panel_key() -> &'static str {
        INBOX_PANEL_NAME
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::PullRequest)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Pull requests")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleInbox)
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if active {
            if self.user_activated {
                self.refresh(cx);
            }
        }
    }
}

impl InboxPanel {
    fn render_note(text: impl Into<SharedString>, color: Color) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .child(Label::new(text).color(color).size(LabelSize::Small))
    }

    fn render_section_label(text: impl Into<SharedString>) -> impl IntoElement {
        div()
            .px_2()
            .pt_3()
            .pb_1()
            .child(Label::new(text).color(Color::Muted).size(LabelSize::Small))
    }

    fn render_empty_state(
        icon: IconName,
        title: impl Into<SharedString>,
        detail: Option<(SharedString, Color)>,
        button_label: Option<&'static str>,
        extra: Option<AnyElement>,
    ) -> impl IntoElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .px_6()
            .py_8()
            .child(Icon::new(icon).size(IconSize::Medium).color(Color::Muted))
            .child(Label::new(title))
            .children(detail.map(|(detail, color)| {
                div()
                    .text_center()
                    .child(Label::new(detail).color(color).size(LabelSize::Small))
            }))
            .children(button_label.map(|label| {
                crate::button("photon-review-load", label)
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(RefreshInbox.boxed_clone(), cx)
                    })
            }))
            .children(extra)
    }

    fn row(&self, summary: &PrSummary) -> RowMeta {
        row_meta(summary, self.with_drafts.contains(&summary.key))
    }

    fn render_row(&self, summary: &PrSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let row = self.row(summary);
        let key = &summary.key;
        let id = format!("{}/{}/{}#{}", key.host, key.owner, key.repo, key.number);
        let primary_summary = summary.clone();
        let workspace = self.workspace.clone();
        let local_summary = summary.clone();
        let local_workspace = workspace.clone();
        v_flex()
            .id(SharedString::from(id.clone()))
            .gap_1()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(row.title))
            .child(
                Label::new(row.meta)
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .children(row.badges.iter().map(|badge| {
                        Label::new(badge.clone())
                            .color(badge_color(badge))
                            .size(LabelSize::XSmall)
                    })),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        crate::button(SharedString::from(format!("{id}-run")), "Run locally")
                            .style(ButtonStyle::Outlined)
                            .on_click(move |_, window, cx| {
                                local_workspace
                                    .update(cx, |workspace, cx| {
                                        run_locally(local_summary.clone(), workspace, window, cx)
                                            .detach_and_log_err(cx)
                                    })
                                    .log_err();
                            }),
                    )
                    .child(
                        crate::button(
                            SharedString::from(format!("{id}-review")),
                            row.primary_action,
                        )
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .on_click(move |_, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    open_review(primary_summary.clone(), workspace, window, cx)
                                        .detach_and_log_err(cx)
                                })
                                .log_err();
                        }),
                    ),
            )
    }
}

impl Render for InboxPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = v_flex()
            .id("photon-review-inbox-body")
            .flex_1()
            .overflow_y_scroll();
        let mut host_line_text = None;
        match &self.state {
            InboxState::Idle => {
                body = body.child(Self::render_empty_state(
                    IconName::PullRequest,
                    "See what needs your review",
                    Some((
                        "Load the open pull requests for this window's repositories.".into(),
                        Color::Muted,
                    )),
                    Some("Load pull requests"),
                    None,
                ))
            }
            InboxState::Loading => {
                body = body.child(Self::render_empty_state(
                    IconName::PullRequest,
                    "Loading pull requests…",
                    None,
                    None,
                    None,
                ))
            }
            InboxState::NoRepository => {
                body = body.child(Self::render_empty_state(
                    IconName::Github,
                    NO_REPOSITORY_TEXT,
                    Some((NO_REPOSITORY_DETAIL.into(), Color::Muted)),
                    None,
                    None,
                ))
            }
            InboxState::NotGitHub(hosts) => {
                body = body.child(Self::render_empty_state(
                    IconName::Github,
                    "Not a GitHub repository",
                    Some((
                        format!(
                            "Pull requests work with GitHub and GitHub Enterprise. This window's repositories are on {}.",
                            hosts.join(", ")
                        )
                        .into(),
                        Color::Muted,
                    )),
                    None,
                    None,
                ))
            }
            InboxState::Failed(error) => {
                let fix = error.fix();
                let detected = self.detected_line();
                let extra = v_flex()
                    .items_center()
                    .gap_2()
                    .when(!detected.is_empty(), |extra| {
                        extra.child(Label::new(detected).color(Color::Muted).size(LabelSize::Small))
                    })
                    .children(fix.clone().map(|fix| self.render_fix(fix, cx)))
                    .into_any_element();
                body = body.child(Self::render_empty_state(
                    IconName::Warning,
                    "Couldn't load pull requests",
                    Some((error.to_string().into(), Color::Error)),
                    fix.is_none().then_some("Try again"),
                    Some(extra),
                ))
            }
            InboxState::Loaded(hosts) => {
                host_line_text = Some(host_line(hosts));
                let mut any_rows = false;
                for host in hosts {
                    if hosts.len() > 1 {
                        body = body.child(Self::render_section_label(host.host.clone()));
                    }
                    match &host.result {
                        Err(error) => {
                            body = body
                                .child(Self::render_note(error.to_string(), Color::Error))
                                .children(error.fix().map(|fix| {
                                    div().px_2().pb_1().child(self.render_fix(fix, cx))
                                }))
                        }
                        Ok(inbox) => {
                            for (group, rows) in &inbox.sections {
                                if rows.is_empty() {
                                    continue;
                                }
                                any_rows = true;
                                body = body.child(Self::render_section_label(section_label(
                                    group,
                                    rows.len(),
                                )));
                                for summary in rows {
                                    body = body.child(self.render_row(summary, cx));
                                }
                            }
                        }
                    }
                }
                if !any_rows && hosts.iter().all(|host| host.result.is_ok()) {
                    body = body.child(Self::render_empty_state(
                        IconName::Check,
                        EMPTY_INBOX_TEXT,
                        Some((EMPTY_INBOX_DETAIL.into(), Color::Muted)),
                        None,
                        None,
                    ));
                }
            }
        }
        v_flex()
            .key_context(INBOX_PANEL_NAME)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .h(Tab::container_height(cx))
                    .flex_none()
                    .pl_2()
                    .pr_1()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new("Pull requests"))
                    .child(
                        h_flex()
                            .gap_1()
                            .when(self.refreshing, |header| {
                                header.child(
                                    Label::new("Refreshing…")
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                )
                            })
                            .when(!self.refreshing, |header| {
                                header.children(host_line_text.map(|text| {
                                    Label::new(text).color(Color::Muted).size(LabelSize::Small)
                                }))
                            })
                            .child(
                                IconButton::new("photon-review-refresh", IconName::RotateCw)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh"))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(RefreshInbox.boxed_clone(), cx)
                                    }),
                            ),
                    ),
            )
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use gpui::{AvailableSpace, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::{MultiWorkspace, Workspace};

    use super::*;
    use crate::{FakeGh, set_gh_runner};

    const AUTH_ONE_HOST: &str =
        r#"{"hosts":{"github.com":[{"state":"success","active":true,"login":"alice"}]}}"#;
    const AUTH_TWO_HOSTS: &str = include_str!("../test_fixtures/auth_status.json");
    const INBOX: &str = include_str!("../test_fixtures/inbox.json");

    fn init_test(cx: &mut TestAppContext, fake: &Arc<FakeGh>) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            cx.set_global(db::AppDatabase::test_new());
            set_gh_runner(fake.clone(), cx);
            init_inbox(cx);
        });
    }

    fn respond_auth(fake: &FakeGh, json: &'static str) {
        fake.respond(|request| request.args[0] == "auth", Ok(json.into()));
    }

    fn respond_inbox(fake: &FakeGh, host: &'static str) {
        fake.respond(
            move |request| request.args[0] == "api" && request.args.iter().any(|arg| arg == host),
            Ok(INBOX.into()),
        );
    }

    const ORIGIN: (&str, &str) = ("origin", "git@github.com:acme/web.git");

    /// Opens a window on a repository with these remotes; no remotes means no repository.
    async fn add_panel(
        cx: &mut TestAppContext,
        remotes: &[(&str, &str)],
    ) -> (Entity<Workspace>, Entity<InboxPanel>, VisualTestContext) {
        let fs = FakeFs::new(cx.executor());
        let project = if remotes.is_empty() {
            Project::test(fs, [], cx).await
        } else {
            fs.insert_tree(Path::new("/project"), serde_json::json!({ ".git": {} }))
                .await;
            for (name, url) in remotes {
                fs.set_remote_for_repo(Path::new("/project/.git"), *name, *url);
            }
            let project = Project::test(fs, [Path::new("/project")], cx).await;
            project
                .update(cx, |project, cx| project.git_scans_complete(cx))
                .await;
            project
        };
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let weak_workspace = workspace.downgrade();
        let panel = window
            .update(cx, |_, window, cx| {
                cx.spawn_in(window, async move |_this, cx| {
                    InboxPanel::load(weak_workspace, cx.clone()).await
                })
            })
            .expect("window")
            .await
            .expect("load panel");
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        workspace.update_in(&mut visual, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        (workspace, panel, visual)
    }

    fn render(panel: &Entity<InboxPanel>, cx: &mut VisualTestContext) {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(
                AvailableSpace::Definite(px(360.)),
                AvailableSpace::Definite(px(600.)),
            ),
            |_, _| panel.clone().into_any_element(),
        );
    }

    fn set_active(panel: &Entity<InboxPanel>, active: bool, cx: &mut VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            panel.user_activated = true;
            panel.set_active(active, window, cx)
        });
        cx.run_until_parked();
    }

    fn request_summary(fake: &FakeGh) -> Vec<String> {
        fake.requests()
            .iter()
            .map(|request| {
                if request.args[0] == "auth" {
                    "auth".to_string()
                } else {
                    format!("api {}", request.args[3])
                }
            })
            .collect()
    }

    fn summary(number: u64) -> PrSummary {
        PrSummary {
            key: PrKey {
                host: "github.com".into(),
                owner: "acme".into(),
                repo: "web".into(),
                number,
            },
            node_id: format!("PR_{number}"),
            title: "Add retry".into(),
            author: "alice".into(),
            url: String::new(),
            head_sha: "a1".into(),
            base_ref: "main".into(),
            checks: ChecksState::Unknown,
            unresolved_threads: 0,
            last_review_commit: None,
            since_review: SinceReview::NotReviewed,
        }
    }

    #[gpui::test]
    async fn panel_creation_runs_no_gh(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let (_workspace, panel, cx) = add_panel(cx, &[ORIGIN]).await;
        cx.run_until_parked();
        assert!(fake.requests().is_empty());
        panel.read_with(&cx, |panel, _| {
            assert!(matches!(panel.state(), InboxState::Idle))
        });
    }

    #[gpui::test]
    async fn startup_makes_no_gh_or_git_network_calls(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        // Dock restoration activates the panel at startup without a user action.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.set_active(true, window, cx)
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(600));
        cx.run_until_parked();
        assert!(fake.requests().is_empty());
        let network_commands: Vec<_> = crate::checkout::COMMAND_LOG.with_borrow(|log| {
            log.iter()
                .filter(|argv| {
                    argv.iter().any(|arg| {
                        matches!(arg.as_str(), "clone" | "fetch" | "worktree" | "checkout")
                    })
                })
                .cloned()
                .collect()
        });
        assert!(network_commands.is_empty(), "{network_commands:?}");
    }

    #[gpui::test]
    async fn sections_setting_changes_the_lists(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.pr_review.get_or_insert_default().sections =
                        Some(vec![settings::PrReviewSectionContent {
                            name: "Everything".into(),
                            query: "".into(),
                        }]);
                });
            });
        });
        cx.run_until_parked();
        let requests = fake.requests();
        let last = requests
            .into_iter()
            .rfind(|request| request.args.iter().any(|arg| arg == "graphql"))
            .expect("an inbox request");
        let body: serde_json::Value =
            serde_json::from_str(last.stdin.as_deref().expect("stdin")).expect("json");
        assert_eq!(
            body["variables"]["s0"],
            "is:pr is:open archived:false repo:acme/web"
        );
        assert!(body["variables"].get("s1").is_none());
        panel.read_with(&cx, |panel, _| {
            let InboxState::Loaded(hosts) = panel.state() else {
                panic!("expected Loaded, got {:?}", panel.state());
            };
            let inbox = hosts[0].result.as_ref().expect("inbox");
            let names: Vec<_> = inbox
                .sections
                .iter()
                .map(|(name, _)| name.as_str())
                .collect();
            assert_eq!(names, ["Everything"]);
        });
    }

    #[gpui::test]
    async fn opening_panel_loads_groups(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        assert_eq!(request_summary(&fake), ["auth", "api github.com"]);
        panel.update_in(&mut cx, |panel, window, _| {
            let InboxState::Loaded(hosts) = panel.state() else {
                panic!("expected Loaded, got {:?}", panel.state());
            };
            assert_eq!(hosts.len(), 1);
            let inbox = hosts[0].result.as_ref().expect("inbox");
            assert_eq!(inbox.sections.len(), 4);
            assert_eq!(host_line(hosts), "github.com");
            window.refresh();
        });
        render(&panel, &mut cx);
    }

    #[gpui::test]
    async fn inbox_searches_only_this_windows_repos(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let (_workspace, panel, mut cx) = add_panel(
            cx,
            &[ORIGIN, ("upstream", "https://github.com/Upstream/Web.git")],
        )
        .await;
        set_active(&panel, true, &mut cx);
        let requests = fake.requests();
        let query = requests[1].stdin.as_deref().expect("graphql body");
        assert!(
            query.contains("archived:false repo:acme/web repo:upstream/web review-requested:@me"),
            "{query}"
        );
    }

    #[gpui::test]
    async fn window_without_a_github_repo_runs_no_gh(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let (_workspace, panel, mut cx) = add_panel(cx, &[]).await;
        set_active(&panel, true, &mut cx);
        assert!(fake.requests().is_empty());
        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.state(), &InboxState::NoRepository)
        });
        render(&panel, &mut cx);
    }

    #[gpui::test]
    async fn refresh_on_open_and_manual_only(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        for _ in 0..3 {
            respond_auth(&fake, AUTH_ONE_HOST);
            respond_inbox(&fake, "github.com");
        }
        let (workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        assert_eq!(fake.requests().len(), 2);

        cx.executor().advance_clock(Duration::from_secs(3600));
        cx.run_until_parked();
        assert_eq!(fake.requests().len(), 2);

        workspace.update_in(&mut cx, |_, window, cx| {
            window.dispatch_action(RefreshInbox.boxed_clone(), cx)
        });
        cx.run_until_parked();
        assert_eq!(fake.requests().len(), 4);

        set_active(&panel, false, &mut cx);
        assert_eq!(fake.requests().len(), 4);
        set_active(&panel, true, &mut cx);
        assert_eq!(fake.requests().len(), 6);
    }

    #[gpui::test]
    async fn gh_missing_shows_install_hint(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        fake.respond(|_| true, Err(GhError::NotInstalled));
        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        panel.read_with(&cx, |panel, _| {
            let InboxState::Failed(error) = panel.state() else {
                panic!("expected Failed");
            };
            assert_eq!(error, &GhError::NotInstalled);
            assert!(error.to_string().contains("brew install gh"));
        });
        render(&panel, &mut cx);
    }

    #[gpui::test]
    async fn logged_out_shows_login_hint(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, r#"{"hosts":{}}"#);
        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        panel.read_with(&cx, |panel, _| {
            assert!(matches!(
                panel.state(),
                InboxState::Failed(GhError::LoggedOut {
                    host: Some(host)
                }) if host == "github.com"
            ));
        });
    }

    #[gpui::test]
    async fn one_host_logged_out_still_lists_others(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_TWO_HOSTS);
        respond_inbox(&fake, "github.com");
        let (_workspace, panel, mut cx) =
            add_panel(cx, &[ORIGIN, ("upstream", "https://ghe.corp/acme/app.git")]).await;
        set_active(&panel, true, &mut cx);
        assert_eq!(request_summary(&fake), ["auth", "api github.com"]);
        panel.update_in(&mut cx, |panel, _, _| {
            let InboxState::Loaded(hosts) = panel.state() else {
                panic!("expected Loaded");
            };
            let by_host = |name: &str| hosts.iter().find(|host| host.host == name).expect("host");
            assert!(by_host("github.com").result.is_ok());
            let error = by_host("ghe.corp").result.as_ref().expect_err("logged out");
            assert_eq!(
                error,
                &GhError::LoggedOut {
                    host: Some("ghe.corp".into())
                }
            );
            assert!(
                error
                    .to_string()
                    .contains("gh auth login --hostname ghe.corp")
            );
            assert_eq!(host_line(hosts), "ghe.corp + github.com");
        });
        render(&panel, &mut cx);
    }

    #[gpui::test]
    async fn enterprise_host_without_login_is_reported(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let (_workspace, panel, mut cx) =
            add_panel(cx, &[ORIGIN, ("upstream", "https://ghe.corp/acme/app.git")]).await;
        set_active(&panel, true, &mut cx);
        panel.read_with(&cx, |panel, _| {
            let InboxState::Loaded(hosts) = panel.state() else {
                panic!("expected Loaded, got {:?}", panel.state());
            };
            let ghe = hosts
                .iter()
                .find(|host| host.host == "ghe.corp")
                .expect("ghe.corp listed");
            assert_eq!(
                ghe.result,
                Err(GhError::LoggedOut {
                    host: Some("ghe.corp".into())
                })
            );
        });
    }

    #[gpui::test]
    async fn ssh_alias_remote_uses_the_real_host(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        fake.alias("work-gh", "ghe.corp");
        respond_auth(
            &fake,
            r#"{"hosts":{"ghe.corp":[{"state":"success","active":true,"login":"alice"}]}}"#,
        );
        respond_inbox(&fake, "ghe.corp");
        let (_workspace, panel, mut cx) =
            add_panel(cx, &[("origin", "git@work-gh:acme/app.git")]).await;
        set_active(&panel, true, &mut cx);
        assert_eq!(request_summary(&fake), ["auth", "api ghe.corp"]);
        let requests = fake.requests();
        let query = requests[1].stdin.as_deref().expect("graphql body");
        assert!(query.contains("repo:acme/app"), "{query}");
    }

    #[gpui::test]
    async fn gitlab_remote_is_not_github(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let (_workspace, panel, mut cx) =
            add_panel(cx, &[("origin", "git@gitlab.com:acme/app.git")]).await;
        set_active(&panel, true, &mut cx);
        assert!(fake.requests().is_empty());
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                panel.state(),
                &InboxState::NotGitHub(vec!["gitlab.com".into()])
            )
        });
        render(&panel, &mut cx);
    }

    #[gpui::test]
    async fn failure_shows_the_detected_repository(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, r#"{"hosts":{}}"#);
        let (_workspace, panel, mut cx) =
            add_panel(cx, &[("origin", "https://ghe.corp/acme/app.git")]).await;
        set_active(&panel, true, &mut cx);
        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.detected_line(), "Found ghe.corp/acme/app")
        });
        render(&panel, &mut cx);
    }

    #[gpui::test]
    async fn fix_reloads_the_panel_when_its_command_ends(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, r#"{"hosts":{}}"#);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        panel.update_in(&mut cx, |panel, window, cx| {
            let InboxState::Failed(error) = panel.state() else {
                panic!("expected Failed");
            };
            let fix = error.fix().expect("login fix");
            panel.run_fix(fix, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(request_summary(&fake), ["auth", "auth", "api github.com"]);
        panel.read_with(&cx, |panel, _| {
            assert!(matches!(panel.state(), InboxState::Loaded(_)))
        });
    }

    #[gpui::test]
    async fn closed_tracked_pr_is_untracked(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let repo = tempfile::tempdir().expect("tempdir");
        crate::checkout::git(repo.path(), &["init", "-q"])
            .await
            .expect("git init");

        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let key = PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "web".into(),
            number: 1,
        };
        let db = cx.update(|cx| ReviewDb::global(cx));
        db.track(Tracked {
            key,
            node_id: "PR_x".into(),
            repo_dir: repo.path().to_path_buf(),
            worktree: repo.path().join("missing-worktree"),
        })
        .await
        .expect("track");
        assert_eq!(db.tracked().expect("tracked").len(), 1);

        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        for _ in 0..50 {
            if db.tracked().expect("tracked").is_empty() {
                break;
            }
            cx.executor().timer(Duration::from_millis(100)).await;
            cx.run_until_parked();
        }
        assert_eq!(db.tracked().expect("tracked"), vec![]);
        drop(panel);
    }

    #[gpui::test]
    async fn partial_error_keeps_the_inbox_and_untracks_the_missing_pr(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        let mut partial: serde_json::Value = serde_json::from_str(INBOX).expect("fixture");
        partial["data"]["s1"] = serde_json::Value::Null;
        partial["data"]["nodes"] = serde_json::json!([null]);
        partial["errors"] = serde_json::json!([
            { "message": "Could not resolve to a node with the global id of 'PR_gone'" },
            { "message": "Resource protected by organization SAML enforcement" }
        ]);
        fake.respond(|request| request.args[0] == "api", Ok(partial.to_string()));
        let key = PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "web".into(),
            number: 9,
        };
        let database = cx.update(|cx| ReviewDb::global(cx));
        database
            .track(Tracked {
                key: key.clone(),
                node_id: "PR_gone".into(),
                repo_dir: "/nonexistent/repo".into(),
                worktree: "/nonexistent/worktree".into(),
            })
            .await
            .expect("track");
        database
            .save_draft(crate::Draft {
                id: None,
                key: key.clone(),
                path: "src/a.rs".into(),
                side: crate::Side::Right,
                start_line: 1,
                end_line: 1,
                commit: "a1".into(),
                anchor_text: "fn a() {}".into(),
                body: "Keep me".into(),
                outdated: false,
                general: false,
                post_state: crate::PostState::Unposted,
            })
            .await
            .expect("save");

        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        panel.read_with(&cx, |panel, _| {
            let InboxState::Loaded(hosts) = panel.state() else {
                panic!("expected Loaded, got {:?}", panel.state());
            };
            let inbox = hosts[0]
                .result
                .as_ref()
                .expect("inbox despite partial errors");
            let counts: Vec<_> = inbox
                .sections
                .iter()
                .map(|(name, rows)| (name.as_str(), rows.len()))
                .collect();
            assert!(counts[0].1 > 0, "{counts:?}");
            assert_eq!(counts[1], ("Yours", 0));
        });
        render(&panel, &mut cx);
        assert_eq!(database.tracked().expect("tracked"), vec![]);
        assert_eq!(database.drafts(&key).expect("drafts").len(), 1);
    }

    #[gpui::test]
    async fn restored_open_dock_loads_nothing_until_user_acts(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let (workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace
                .right_dock()
                .update(cx, |dock, cx| dock.set_open(true, window, cx));
        });
        cx.run_until_parked();
        assert!(fake.requests().is_empty());
        panel.read_with(&cx, |panel, _| {
            assert!(matches!(panel.state(), InboxState::Idle))
        });
        render(&panel, &mut cx);

        workspace.update_in(&mut cx, |_, window, cx| {
            window.dispatch_action(RefreshInbox.boxed_clone(), cx)
        });
        cx.run_until_parked();
        assert_eq!(request_summary(&fake), ["auth", "api github.com"]);
    }

    #[gpui::test]
    async fn toggling_the_panel_loads(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let (workspace, _panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        workspace.update_in(&mut cx, |_, window, cx| {
            window.dispatch_action(ToggleInbox.boxed_clone(), cx)
        });
        cx.run_until_parked();
        assert_eq!(request_summary(&fake), ["auth", "api github.com"]);
    }

    #[gpui::test]
    async fn refresh_keeps_previous_list_visible(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        for _ in 0..2 {
            respond_auth(&fake, AUTH_ONE_HOST);
            respond_inbox(&fake, "github.com");
        }
        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        panel.update(&mut cx, |panel, cx| {
            panel.refresh(cx);
            assert!(panel.refreshing);
            assert!(matches!(panel.state(), InboxState::Loaded(_)));
        });
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| assert!(!panel.refreshing));
    }

    fn damaged_key(repo: &str, number: u64) -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: repo.into(),
            number,
        }
    }

    async fn track_and_clean(
        cx: &mut TestAppContext,
        entries: Vec<(PrKey, PathBuf)>,
        repo_dir: &Path,
        root: &Path,
    ) -> ReviewDb {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let database = cx.update(|cx| ReviewDb::global(cx));
        let mut closed = Vec::new();
        for (key, worktree) in entries {
            let tracked = Tracked {
                key,
                node_id: "PR_cleanup".into(),
                repo_dir: repo_dir.to_path_buf(),
                worktree,
            };
            database.track(tracked.clone()).await.expect("track");
            closed.push(tracked);
        }
        cleanup_closed(closed, database.clone(), root.to_path_buf(), Arc::default()).await;
        database
    }

    #[gpui::test]
    async fn cleanup_force_removes_damaged_worktree(cx: &mut TestAppContext) {
        use crate::checkout::git;
        cx.executor().allow_parking();
        let repo = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("tempdir");
        let key = damaged_key("damaged", 9);
        let worktree = worktree_path(root.path(), &key);
        git(repo.path(), &["init", "-q"]).await.expect("init");
        git(
            repo.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        )
        .await
        .expect("commit");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &worktree.to_string_lossy(),
            ],
        )
        .await
        .expect("worktree add");
        std::fs::remove_file(worktree.join(".git")).expect("damage worktree");
        std::fs::write(worktree.join("leftover.txt"), "x").expect("leftover");

        let database = track_and_clean(
            cx,
            vec![(key.clone(), worktree.clone())],
            repo.path(),
            root.path(),
        )
        .await;
        assert!(!worktree.exists());
        assert!(
            database
                .tracked()
                .expect("tracked")
                .iter()
                .all(|tracked| tracked.key != key)
        );
    }

    #[gpui::test]
    async fn cleanup_never_touches_paths_outside_the_photon_worktree(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        let outside = base.path().join("outside");
        let clone = root.join("clones/github.com/acme/safety");
        for dir in [&root, &outside, &clone] {
            std::fs::create_dir_all(dir).expect("mkdir");
            std::fs::write(dir.join("keep.txt"), "x").expect("write");
        }
        let entries = vec![
            (damaged_key("safety", 10), root.join("..").join("outside")),
            (damaged_key("safety", 11), root.clone()),
            (damaged_key("safety", 12), clone.clone()),
            (
                damaged_key("safety", 13),
                worktree_path(&root, &damaged_key("safety", 14)),
            ),
        ];
        let keys: Vec<PrKey> = entries.iter().map(|(key, _)| key.clone()).collect();
        let database = track_and_clean(cx, entries, base.path(), &root).await;
        for dir in [&root, &outside, &clone] {
            assert!(dir.join("keep.txt").exists(), "{dir:?} was touched");
        }
        let remaining = database.tracked().expect("tracked");
        assert!(remaining.iter().all(|tracked| !keys.contains(&tracked.key)));
    }

    #[gpui::test]
    async fn cleanup_skips_keys_already_in_flight(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let database = cx.update(|cx| ReviewDb::global(cx));
        let key = damaged_key("inflight", 20);
        let tracked = Tracked {
            key: key.clone(),
            node_id: "PR_inflight".into(),
            repo_dir: PathBuf::from("/nonexistent"),
            worktree: PathBuf::from("/nonexistent/wt"),
        };
        database.track(tracked.clone()).await.expect("track");
        let in_flight: InFlightCleanups = Arc::default();
        in_flight.lock().insert(key.clone());
        cleanup_closed(
            vec![tracked],
            database.clone(),
            PathBuf::from("/nonexistent"),
            in_flight,
        )
        .await;
        assert!(
            database
                .tracked()
                .expect("tracked")
                .iter()
                .any(|tracked| tracked.key == key)
        );
    }

    #[test]
    fn row_meta_texts() {
        let mut row = summary(482);
        let meta = row_meta(&row, false);
        assert_eq!(meta.title, "Add retry");
        assert_eq!(meta.meta, "#482 · alice · acme/web");
        assert!(meta.badges.is_empty());
        assert_eq!(meta.primary_action, "Start review");

        row.key.host = "ghe.corp".into();
        assert_eq!(
            row_meta(&row, false).meta,
            "#482 · alice · ghe.corp/acme/web"
        );
        row.key.host = "github.com".into();

        for (checks, text) in [
            (ChecksState::Passing, Some("checks passing")),
            (ChecksState::Failing, Some("checks failing")),
            (ChecksState::Pending, Some("checks pending")),
            (ChecksState::Unknown, None),
        ] {
            row.checks = checks;
            assert_eq!(
                row_meta(&row, false).badges,
                text.map(String::from).into_iter().collect::<Vec<_>>()
            );
        }

        row.checks = ChecksState::Failing;
        row.unresolved_threads = 1;
        row.since_review = SinceReview::NewCommits(1);
        assert_eq!(
            row_meta(&row, false).badges,
            [
                "checks failing",
                "1 unresolved thread",
                "1 new commit since your review"
            ]
        );
        row.unresolved_threads = 2;
        row.since_review = SinceReview::NewCommits(3);
        assert_eq!(
            row_meta(&row, false).badges,
            [
                "checks failing",
                "2 unresolved threads",
                "3 new commits since your review"
            ]
        );
        row.since_review = SinceReview::ForcePushed;
        assert_eq!(
            row_meta(&row, false).badges[2],
            "force-pushed since your review"
        );
        row.since_review = SinceReview::UpToDate;
        assert_eq!(row_meta(&row, false).badges.len(), 2);

        assert_eq!(row_meta(&row, true).primary_action, "Continue review");
        row.last_review_commit = Some("a0".into());
        assert_eq!(row_meta(&row, false).primary_action, "Continue review");
    }

    #[test]
    fn section_labels_and_copy() {
        assert_eq!(section_label("Review requested", 3), "Review requested · 3");
        assert_eq!(EMPTY_INBOX_TEXT, "No open pull requests");
    }

    #[gpui::test]
    async fn drafts_make_the_row_continue_review(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let (_workspace, panel, mut cx) = add_panel(cx, &[ORIGIN]).await;
        set_active(&panel, true, &mut cx);
        let first_row = |panel: &InboxPanel| {
            let InboxState::Loaded(hosts) = panel.state() else {
                panic!("expected Loaded");
            };
            let inbox = hosts[0].result.as_ref().expect("inbox");
            let summary = inbox.sections[0].1[0].clone();
            (summary.key.clone(), panel.row(&summary).primary_action)
        };
        let (key, action) = panel.read_with(&cx, |panel, _| first_row(panel));
        assert_eq!(action, "Start review");

        let database = cx.update(|_, cx| ReviewDb::global(cx));
        database
            .save_draft(crate::Draft {
                id: None,
                key,
                path: "src/a.rs".into(),
                side: crate::Side::Right,
                start_line: 1,
                end_line: 1,
                commit: "a1".into(),
                anchor_text: "fn a() {}".into(),
                body: "Why?".into(),
                outdated: false,
                general: false,
                post_state: crate::PostState::Unposted,
            })
            .await
            .expect("save");
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        panel.update(&mut cx, |panel, cx| panel.refresh(cx));
        cx.run_until_parked();
        let (_, action) = panel.read_with(&cx, |panel, _| first_row(panel));
        assert_eq!(action, "Continue review");
    }
}
