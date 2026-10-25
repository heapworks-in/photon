use std::sync::Arc;

use editor::Editor;
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Task, WeakEntity, Window, actions,
};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{
    GhError, GhRunner, PrKey, PrSummary, auth_status_request, detail_request, gh_runner,
    inbox::failed, open_review, parse_auth_status, parse_detail,
};

actions!(
    photon_review,
    [
        /// Opens the review for a pull request from its URL.
        OpenPullRequestFromUrl
    ]
);

const NOT_A_PULL_REQUEST_URL: &str = "That isn't a pull request URL";

pub(crate) fn init_open_url(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenPullRequestFromUrl, window, cx| {
            let handle = workspace.weak_handle();
            workspace.toggle_modal(window, cx, |window, cx| {
                OpenPullRequestModal::new(handle, window, cx)
            });
        });
    })
    .detach();
}

/// `https://<host>/<owner>/<repo>/pull/<number>`, with anything after the number ignored.
pub fn parse_pull_request_url(url: &str) -> Option<PrKey> {
    let rest = url.trim().strip_prefix("https://")?;
    let rest = rest.split(['?', '#']).next()?;
    let mut segments = rest.split('/');
    let (host, owner, repo, pull, number) = (
        segments.next()?,
        segments.next()?,
        segments.next()?,
        segments.next()?,
        segments.next()?,
    );
    // These go to gh as arguments, so only plain host and name characters get through.
    let is_name = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|char| char.is_ascii_alphanumeric() || matches!(char, '-' | '_' | '.'))
    };
    if !is_name(host) || !is_name(owner) || !is_name(repo) || pull != "pull" {
        return None;
    }
    if !number.chars().all(|char| char.is_ascii_digit()) {
        return None;
    }
    Some(PrKey {
        host: host.to_lowercase(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number: number.parse().ok()?,
    })
}

/// The same summary the inbox shows, so the review opens exactly as it would from the panel.
async fn fetch_summary(runner: Arc<dyn GhRunner>, key: PrKey) -> Result<PrSummary, GhError> {
    let hosts = parse_auth_status(&runner.run(auth_status_request()).await?).map_err(failed)?;
    let login = hosts
        .into_iter()
        .find(|auth| auth.host.to_lowercase() == key.host)
        .and_then(|auth| auth.login)
        .ok_or_else(|| GhError::LoggedOut {
            host: Some(key.host.clone()),
        })?;
    let output = runner.run(detail_request(&key, &login)).await?;
    Ok(parse_detail(&key, &output).map_err(failed)?.summary)
}

pub struct OpenPullRequestModal {
    workspace: WeakEntity<Workspace>,
    editor: Entity<Editor>,
    error: Option<SharedString>,
    opening: Option<Task<()>>,
}

impl EventEmitter<DismissEvent> for OpenPullRequestModal {}
impl ModalView for OpenPullRequestModal {}

impl Focusable for OpenPullRequestModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl OpenPullRequestModal {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Paste a pull request URL", window, cx);
            editor
        });
        Self {
            workspace,
            editor,
            error: None,
            opening: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = parse_pull_request_url(&self.editor.read(cx).text(cx)) else {
            self.error = Some(NOT_A_PULL_REQUEST_URL.into());
            cx.notify();
            return;
        };
        self.error = None;
        let runner = gh_runner(cx);
        let workspace = self.workspace.clone();
        self.opening = Some(cx.spawn_in(window, async move |this, cx| {
            let summary = cx.background_spawn(fetch_summary(runner, key)).await;
            this.update_in(cx, |this, window, cx| match summary {
                Ok(summary) => {
                    workspace
                        .update(cx, |workspace, cx| {
                            open_review(summary, workspace, window, cx).detach_and_log_err(cx)
                        })
                        .log_err();
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.error = Some(error.to_string().into());
                    this.opening = None;
                    cx.notify();
                }
            })
            .log_err();
        }));
        cx.notify();
    }
}

impl Render for OpenPullRequestModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match (&self.error, &self.opening) {
            (Some(error), _) => Label::new(error.clone()).color(Color::Error),
            (None, Some(_)) => Label::new("Opening…").color(Color::Muted),
            (None, None) => Label::new("Press Enter to open its review.").color(Color::Muted),
        };
        v_flex()
            .key_context("OpenPullRequestModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w_96()
            .overflow_hidden()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .bg(cx.theme().colors().editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .p_2()
                    .child(status.size(LabelSize::Small)),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::MultiWorkspace;

    use super::*;
    use crate::{Checkout, FakeGh, ReviewDb, ReviewItem, set_gh_runner};

    const DETAIL: &str = include_str!("../test_fixtures/detail.json");
    const AUTH: &str =
        r#"{"hosts":{"github.com":[{"state":"success","active":true,"login":"alice"}]}}"#;

    fn key(host: &str, owner: &str, repo: &str, number: u64) -> PrKey {
        PrKey {
            host: host.into(),
            owner: owner.into(),
            repo: repo.into(),
            number,
        }
    }

    #[test]
    fn parses_pull_request_urls() {
        let web = key("github.com", "acme", "web", 482);
        for url in [
            "https://github.com/acme/web/pull/482",
            "  https://github.com/acme/web/pull/482/  ",
            "https://github.com/acme/web/pull/482/files",
            "https://github.com/acme/web/pull/482/commits",
            "https://github.com/acme/web/pull/482/commits/a1b2c3",
            "https://github.com/acme/web/pull/482#discussion_r1",
            "https://github.com/acme/web/pull/482?diff=split",
            "https://github.com/acme/web/pull/482/files?w=1#diff-1",
            "https://GitHub.com/acme/web/pull/482",
        ] {
            assert_eq!(parse_pull_request_url(url), Some(web.clone()), "{url}");
        }
        assert_eq!(
            parse_pull_request_url("https://github.acme.corp/platform/api/pull/7"),
            Some(key("github.acme.corp", "platform", "api", 7))
        );
        for url in [
            "",
            "github.com/acme/web/pull/482",
            "ftp://github.com/acme/web/pull/482",
            "https://github.com/acme/web/issues/482",
            "https://github.com/acme/web/pull",
            "https://github.com/acme/web/pull/",
            "https://github.com/acme/web/pull/abc",
            "https://github.com/acme/web/pull/-1",
            "https://github.com/acme/web/pulls",
            "https://github.com/acme/web",
            "https://github.com/acme/web/tree/main/pull/1",
            "https://github.com//web/pull/1",
            "https://user@github.com/acme/web/pull/1",
        ] {
            assert_eq!(parse_pull_request_url(url), None, "{url}");
        }
    }

    fn init_test(cx: &mut TestAppContext, fake: &Arc<FakeGh>) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            cx.set_global(db::AppDatabase::test_new());
            set_gh_runner(fake.clone(), cx);
            init_open_url(cx);
        });
    }

    async fn window(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<Project>, VisualTestContext) {
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = VisualTestContext::from_window(window.into(), cx);
        (workspace, project, cx)
    }

    fn add_review(
        workspace: &Entity<Workspace>,
        project: &Entity<Project>,
        number: u64,
        cx: &mut VisualTestContext,
    ) -> Entity<ReviewItem> {
        let pr = key("github.com", "acme", "web", number);
        let detail = parse_detail(&pr, DETAIL).expect("detail");
        workspace.update_in(cx, |workspace, window, cx| {
            let item = cx.new(|cx| {
                ReviewItem::new(
                    project.clone(),
                    detail.clone(),
                    "alice".into(),
                    Checkout {
                        repo_dir: "/repo".into(),
                        worktree: "/repo".into(),
                        head_sha: detail.summary.head_sha.clone(),
                        merge_base: "a1".into(),
                        head_changed: false,
                    },
                    Vec::new(),
                    ReviewDb::global(cx),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
            item
        })
    }

    fn open_modal(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<OpenPullRequestModal> {
        cx.dispatch_action(OpenPullRequestFromUrl);
        cx.run_until_parked();
        workspace
            .read_with(cx, |workspace, cx| workspace.active_modal(cx))
            .expect("modal open")
    }

    fn enter(modal: &Entity<OpenPullRequestModal>, url: &str, cx: &mut VisualTestContext) {
        modal.update_in(cx, |modal, window, cx| {
            modal
                .editor
                .update(cx, |editor, cx| editor.set_text(url, window, cx))
        });
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
    }

    fn modal_error(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
        workspace.read_with(cx, |workspace, cx| {
            let modal = workspace.active_modal::<OpenPullRequestModal>(cx)?;
            modal.read(cx).error.as_ref().map(|error| error.to_string())
        })
    }

    #[gpui::test]
    async fn enter_opens_the_review_for_the_url(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let (workspace, project, mut cx) = window(cx).await;
        let review = add_review(&workspace, &project, 482, &mut cx);
        add_review(&workspace, &project, 7, &mut cx);
        fake.respond(|request| request.args[0] == "auth", Ok(AUTH.into()));
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_ref()
                    .is_some_and(|stdin| stdin.contains("pullRequest") && stdin.contains("482"))
            },
            Ok(DETAIL.into()),
        );
        // Opening the review checks the login again.
        fake.respond(|request| request.args[0] == "auth", Ok(AUTH.into()));

        let modal = open_modal(&workspace, &mut cx);
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.editor.update(cx, |editor, cx| {
                editor.set_text("https://github.com/acme/web/pull/482/files", window, cx)
            })
        });
        cx.run_until_parked();
        assert!(fake.requests().is_empty(), "no network until Enter");

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();

        workspace.read_with(&cx, |workspace, cx| {
            assert!(workspace.active_modal::<OpenPullRequestModal>(cx).is_none());
            assert_eq!(workspace.items_of_type::<ReviewItem>(cx).count(), 2);
            assert_eq!(
                workspace.active_item(cx).map(|item| item.item_id()),
                Some(review.entity_id())
            );
        });
    }

    #[gpui::test]
    async fn invalid_url_shows_an_error_and_stays_open(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let (workspace, _project, mut cx) = window(cx).await;
        let modal = open_modal(&workspace, &mut cx);
        enter(&modal, "https://github.com/acme/web/issues/482", &mut cx);
        assert_eq!(
            modal_error(&workspace, &mut cx).as_deref(),
            Some(NOT_A_PULL_REQUEST_URL)
        );
        assert!(fake.requests().is_empty());
    }

    #[gpui::test]
    async fn gh_failure_shows_the_reason_and_opens_nothing(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let (workspace, _project, mut cx) = window(cx).await;
        fake.respond(
            |request| request.args[0] == "auth",
            Ok(include_str!("../test_fixtures/auth_status_logged_out.json").into()),
        );
        let modal = open_modal(&workspace, &mut cx);
        enter(&modal, "https://github.com/acme/web/pull/482", &mut cx);
        let error = modal_error(&workspace, &mut cx).expect("error shown");
        assert!(error.contains("Not logged in to github.com"), "{error}");
        workspace.read_with(&cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ReviewItem>(cx).count(), 0)
        });

        fake.respond(|request| request.args[0] == "auth", Ok(AUTH.into()));
        fake.respond(
            |request| request.stdin.is_some(),
            Ok(include_str!("../test_fixtures/graphql_error.json").into()),
        );
        enter(&modal, "https://github.com/acme/web/pull/999", &mut cx);
        let error = modal_error(&workspace, &mut cx).expect("error shown");
        assert!(!error.is_empty());
        workspace.read_with(&cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ReviewItem>(cx).count(), 0)
        });
    }
}
