// A synchronous runner: it shells out to git to build its fixture repository.
#![allow(clippy::disallowed_methods)]
// The GitHub fixtures are deep `json!` literals.
#![recursion_limit = "512"]

//! Renders the screenshots on Photon's website with the app itself, offscreen, the way
//! `zed_visual_test_runner` does: real Metal rendering, no Screen Recording permission.
//!
//!   cargo run -p zed --bin photon_screenshots --features photon-screenshots [-- OUTPUT_DIR]
//!
//! Every scene opens a window on a fixture repository (a small billing service with one pull
//! request), drives it like a user would, and saves `<scene>.webp` (2x) and `<scene>-1x.webp`
//! to OUTPUT_DIR (default `docs/photon/site/img/app`). Nothing touches the network or the user's
//! settings: GitHub answers come from a fake `gh`, language servers are fakes, and the agent is
//! a stub.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("The screenshot runner needs macOS (Metal).");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
#[path = "photon_screenshots/fixture.rs"]
mod fixture;

#[cfg(target_os = "macos")]
fn main() {
    // SAFETY: nothing else runs yet. Keeps the app away from the user's config and data.
    unsafe {
        std::env::set_var("ZED_STATELESS", "1");
    }
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn)
        .init();
    let output = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("docs/photon/site/img/app"));
    let only = std::env::var("PHOTON_SCENES").ok();
    match std::panic::catch_unwind(|| mac::run(output, only)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("Screenshots failed: {error:#}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("Screenshots panicked");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::{
        any::Any,
        cell::RefCell,
        collections::VecDeque,
        future::Future,
        path::{Path, PathBuf},
        rc::Rc,
        sync::Arc,
        time::Duration,
    };

    use acp_thread::{AgentConnection, StubAgentConnection};
    use agent_client_protocol::schema::v1 as acp_v1;
    use agent_servers::{AgentServer, AgentServerDelegate};
    use anyhow::{Context as _, Result, anyhow};
    use assets::Assets;
    use editor::Editor;
    use futures::channel::mpsc::UnboundedReceiver;
    use gpui::{
        App, AppContext as _, AsyncWindowContext, BorrowAppContext as _, Bounds, Entity,
        Focusable as _, Modifiers, Pixels, Task, VisualTestAppContext, WeakEntity, Window,
        WindowBounds,
        WindowHandle, WindowOptions, point, px, size,
    };
    use language::{
        BinaryStatus, FakeLspAdapter, Language, LanguageConfig, LanguageMatcher, LanguageServerName,
    };
    use lsp::FakeLanguageServer;
    use photon_review::{
        Checkout, FakeGh, GhRequest, PrKey, ReviewDb, ReviewItem, build_loaded_file, parse_detail,
        parse_unified_diff, set_gh_runner, set_review_agent,
    };
    use project::{AgentId, Project, ProjectPath};
    use serde_json::json;
    use settings::{Settings as _, SettingsStore};
    use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
    use util::ResultExt as _;
    use workspace::{AppState, MultiWorkspace, Panel, Workspace};

    use crate::fixture;

    const WINDOW_WIDTH: f32 = 1440.;
    const WINDOW_HEIGHT: f32 = 900.;
    const REVIEWER: &str = "maya";
    const AUTHOR: &str = "priya";
    const OWNER: &str = "orbit-labs";
    const REPO: &str = "billing";
    const PR_NUMBER: u64 = 214;
    const BRANCH: &str = "retry-card-charges";
    /// Below this, a WebP copy is kept; cwebp's quality setting is lowered until it fits.
    const MAX_IMAGE_BYTES: u64 = 400 * 1024;

    pub fn run(output: PathBuf, only: Option<String>) -> Result<()> {
        let temp = tempfile::tempdir()?.keep().canonicalize()?;
        let home = temp.join("home");
        write_files(&home, &owned(fixture::terminal_home()))?;
        for entry in std::fs::read_dir(home.join("bin"))? {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(entry?.path(), std::fs::Permissions::from_mode(0o755))?;
        }
        // SAFETY: no other thread runs yet. Terminals start zsh in the fixture home, so neither
        // the user's dotfiles nor their name or paths reach a screenshot; block headers show
        // `~/code/billing`.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("SHELL", "/bin/zsh");
            std::env::remove_var("ZDOTDIR");
            std::env::remove_var("TERM_PROGRAM");
        }
        let repo = Repo::create(&home.join("code"))?;
        let mut shooter = Shooter::new(output, repo)?;
        shooter.set_theme(false)?;
        let wanted = |scene: &str| {
            only.as_deref()
                .is_none_or(|only| only.split(',').any(|name| name == scene))
        };
        type Scene = fn(&mut Shooter) -> Result<()>;
        let scenes: [(&str, Scene); 15] = [
            ("hero", |shooter| shooter.hero("hero", false)),
            ("hero-light", |shooter| shooter.hero("hero-light", true)),
            ("pr-review", Shooter::pr_review),
            ("pr-merge", Shooter::pr_merge),
            ("pr-conversation", Shooter::pr_conversation),
            ("pr-unreviewed", Shooter::pr_unreviewed),
            ("markdown", Shooter::markdown),
            ("call-hierarchy", Shooter::call_hierarchy),
            ("language-server-help", Shooter::language_server_help),
            ("agent", Shooter::agent),
            ("inbox", Shooter::inbox),
            ("terminal-blocks", Shooter::terminal_blocks),
            ("terminal-tests", Shooter::terminal_tests),
            ("terminal-table", Shooter::terminal_table),
            ("terminal-git", Shooter::terminal_git),
        ];
        let mut failed = Vec::new();
        for (name, scene) in scenes {
            if !wanted(name) {
                continue;
            }
            println!("scene {name}");
            if let Err(error) = scene(&mut shooter) {
                eprintln!("  {name} failed: {error:#}");
                failed.push(name);
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("scenes failed: {}", failed.join(", ")))
        }
    }

    // ---- Fixture repository -------------------------------------------------------------

    struct Repo {
        root: PathBuf,
        base_sha: String,
        first_sha: String,
        head_sha: String,
    }

    impl Repo {
        fn create(parent: &Path) -> Result<Self> {
            let root = parent.join(REPO);
            std::fs::create_dir_all(&root)?;
            write_files(&root, &owned(fixture::base_files()))?;
            git(&root, &["init", "--quiet", "--initial-branch=main"], None)?;
            git(
                &root,
                &[
                    "remote",
                    "add",
                    "origin",
                    &format!("git@github.com:{OWNER}/{REPO}.git"),
                ],
                None,
            )?;
            let base_sha = commit(
                &root,
                "devon",
                "Charge invoices through the card gateway",
                1,
            )?;
            git(&root, &["checkout", "--quiet", "-b", BRANCH], None)?;
            write_files(&root, &owned(fixture::first_push()))?;
            let first_sha = commit(&root, AUTHOR, "Retry card charges with backoff", 2)?;
            write_files(&root, &owned(fixture::second_push()))?;
            let head_sha = commit(
                &root,
                AUTHOR,
                "Add jitter and answer 503 while the gateway is down",
                3,
            )?;
            Ok(Self {
                root,
                base_sha,
                first_sha,
                head_sha,
            })
        }
    }

    fn owned(files: Vec<(&'static str, &'static str)>) -> Vec<(&'static str, String)> {
        files
            .into_iter()
            .map(|(path, text)| (path, text.to_string()))
            .collect()
    }

    fn write_files(root: &Path, files: &[(&str, String)]) -> Result<()> {
        for (path, text) in files {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text)?;
        }
        Ok(())
    }

    fn commit(root: &Path, author: &str, message: &str, day: u32) -> Result<String> {
        git(root, &["add", "-A"], None)?;
        git(
            root,
            &["commit", "--quiet", "-m", message],
            Some((author, day)),
        )?;
        Ok(git(root, &["rev-parse", "HEAD"], None)?.trim().to_string())
    }

    /// Runs git without the user's configuration, so nothing personal ends up in the fixture.
    fn git(root: &Path, args: &[&str], author: Option<(&str, u32)>) -> Result<String> {
        let (name, day) = author.unwrap_or(("devon", 1));
        let email = format!("{name}@example.com");
        let date = format!("2026-10-{day:02}T10:00:00Z");
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", &email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", &email)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The 1-based line of the first line containing `needle`.
    fn line_of(text: &str, needle: &str) -> Result<u32> {
        text.lines()
            .position(|line| line.contains(needle))
            .map(|index| index as u32 + 1)
            .with_context(|| format!("no line contains {needle:?}"))
    }

    // ---- Fake Rust language server -------------------------------------------------------

    struct Function {
        name: &'static str,
        uri: lsp::Uri,
        /// 0-based rows.
        start: u32,
        end: u32,
        text: String,
    }

    /// The fixture's functions as the fake language server reports them, found in `root`'s files.
    fn functions(root: &Path) -> Result<Vec<Function>> {
        let mut functions = Vec::new();
        for (name, path) in fixture::FUNCTIONS {
            let abs_path = root.join(path);
            let text = std::fs::read_to_string(&abs_path)?;
            let lines: Vec<&str> = text.lines().collect();
            let Some(start) = lines.iter().position(|line| {
                line.contains(&format!("fn {name}(")) || line.contains(&format!("fn {name}<"))
            }) else {
                continue;
            };
            let indent = lines[start].len() - lines[start].trim_start().len();
            let closing = format!("{}}}", " ".repeat(indent));
            let end = lines
                .iter()
                .skip(start + 1)
                .position(|line| *line == closing)
                .map_or(start, |offset| start + 1 + offset);
            functions.push(Function {
                name,
                uri: lsp::Uri::from_file_path(&abs_path)
                    .map_err(|()| anyhow!("no uri for {}", abs_path.display()))?,
                start: start as u32,
                end: end as u32,
                text: lines[start..=end].join("\n"),
            });
        }
        Ok(functions)
    }

    fn lsp_item(function: &Function) -> lsp::CallHierarchyItem {
        let name_column = function
            .text
            .find(&format!("fn {}", function.name))
            .map_or(0, |column| column as u32 + 3);
        lsp::CallHierarchyItem {
            name: function.name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: function.uri.clone(),
            range: lsp::Range::new(
                lsp::Position::new(function.start, 0),
                lsp::Position::new(function.end, 1),
            ),
            selection_range: lsp::Range::new(
                lsp::Position::new(function.start, name_column),
                lsp::Position::new(function.start, name_column + function.name.len() as u32),
            ),
            data: None,
        }
    }

    /// Where in `caller` it calls `callee`: the first whole-word mention after the signature.
    fn call_site(caller: &Function, callee: &str) -> lsp::Range {
        for (offset, line) in caller.text.lines().enumerate().skip(1) {
            let mut search = 0;
            while let Some(found) = line[search..].find(callee) {
                let column = search + found;
                let before = line[..column].chars().last();
                let after = line[column + callee.len()..].chars().next();
                let is_word = |character: Option<char>| {
                    character
                        .is_some_and(|character| character.is_alphanumeric() || character == '_')
                };
                if !is_word(before) && !is_word(after) {
                    let row = caller.start + offset as u32;
                    return lsp::Range::new(
                        lsp::Position::new(row, column as u32),
                        lsp::Position::new(row, (column + callee.len()) as u32),
                    );
                }
                search = column + callee.len();
            }
        }
        lsp::Range::new(
            lsp::Position::new(caller.start, 0),
            lsp::Position::new(caller.start, 0),
        )
    }

    fn answer_call_hierarchy(server: &mut FakeLanguageServer, functions: &Arc<Vec<Function>>) {
        server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            let functions = functions.clone();
            move |params, _| {
                let position = params.text_document_position_params;
                let item = functions
                    .iter()
                    .filter(|function| {
                        function.uri == position.text_document.uri
                            && function.start <= position.position.line
                            && position.position.line <= function.end
                    })
                    .min_by_key(|function| function.end - function.start)
                    .map(lsp_item);
                async move { Ok(item.map(|item| vec![item])) }
            }
        });
        server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
            let functions = functions.clone();
            move |params, _| {
                let calls = fixture::CALLS
                    .iter()
                    .filter(|(_, callee)| *callee == params.item.name)
                    .filter_map(|(caller, callee)| {
                        let caller = functions.iter().find(|function| function.name == *caller)?;
                        Some(lsp::CallHierarchyIncomingCall {
                            from: lsp_item(caller),
                            from_ranges: vec![call_site(caller, callee)],
                        })
                    })
                    .collect();
                async move { Ok(Some(calls)) }
            }
        });
        server.set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
            let functions = functions.clone();
            move |params, _| {
                let caller = functions
                    .iter()
                    .find(|function| function.name == params.item.name);
                let calls = caller
                    .map(|caller| {
                        fixture::CALLS
                            .iter()
                            .filter(|(from, _)| *from == caller.name)
                            .filter_map(|(_, callee)| {
                                let to =
                                    functions.iter().find(|function| function.name == *callee)?;
                                Some(lsp::CallHierarchyOutgoingCall {
                                    to: lsp_item(to),
                                    from_ranges: vec![call_site(caller, callee)],
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                async move { Ok(Some(calls)) }
            }
        });
    }

    // ---- GitHub fixtures -----------------------------------------------------------------

    fn pr_key() -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: OWNER.into(),
            repo: REPO.into(),
            number: PR_NUMBER,
        }
    }

    fn pr_url() -> String {
        format!("https://github.com/{OWNER}/{REPO}/pull/{PR_NUMBER}")
    }

    fn check(name: &str, id: u32) -> serde_json::Value {
        json!({
            "name": name,
            "conclusion": "SUCCESS",
            "status": "COMPLETED",
            "detailsUrl": format!("https://github.com/{OWNER}/{REPO}/actions/runs/{id}"),
        })
    }

    fn comment(id: u64, author: &str, body: &str, day: u32) -> serde_json::Value {
        json!({
            "databaseId": id,
            "author": { "login": author },
            "body": body,
            "createdAt": format!("2026-10-{day:02}T14:00:00Z"),
        })
    }

    /// `viewed` is GitHub's viewed state for each changed file: VIEWED, UNVIEWED or DISMISSED.
    fn detail_json(repo: &Repo, viewed: &[(&str, &str)]) -> Result<String> {
        let retry_head = std::fs::read_to_string(repo.root.join("src/retry.rs"))?;
        let payments_head = std::fs::read_to_string(repo.root.join("src/payments.rs"))?;
        let jitter_line = line_of(&retry_head, "let jitter")?;
        let exponential_line = line_of(&retry_head, "let exponential")?;
        let sum_line = line_of(&retry_head, "capped + Duration::from_millis(jitter)")?;
        let retryable_line = line_of(&payments_head, "|| error.is_connect()")?;
        Ok(json!({ "data": { "repository": {
            "squashMergeAllowed": true,
            "mergeCommitAllowed": true,
            "rebaseMergeAllowed": true,
            "viewerDefaultMergeMethod": "SQUASH",
            "pullRequest": {
                "id": "PR_kwDOBilling214",
                "isDraft": false,
                "mergeable": "MERGEABLE",
                "mergeStateStatus": "CLEAN",
                "reviewDecision": "APPROVED",
                "comments": { "totalCount": 2 },
                "number": PR_NUMBER,
                "title": "Retry declined card charges with backoff",
                "headRefName": BRANCH,
                "headRepository": { "nameWithOwner": format!("{OWNER}/{REPO}") },
                "body": "Card charges fail for a few seconds whenever the gateway deploys, and today \
                    those invoices wait for the nightly job.\n\n\
                    This retries timeouts and 5xx responses with exponential backoff and jitter. \
                    Declines are never retried, and the idempotency key stays the same on every \
                    attempt, so a retry can't charge a customer twice.\n\n\
                    - [x] Unit tests for the backoff\n\
                    - [x] Tried against the sandbox gateway with injected timeouts\n\
                    - [ ] Dashboard for retry counts",
                "url": pr_url(),
                "author": { "login": AUTHOR },
                "headRefOid": repo.head_sha,
                "baseRefName": "main",
                "repository": { "nameWithOwner": format!("{OWNER}/{REPO}") },
                "commits": { "nodes": [
                    { "commit": { "oid": repo.first_sha } },
                    { "commit": { "oid": repo.head_sha } },
                ] },
                "reviewThreads": { "nodes": [
                    {
                        "id": "PRRT_jitter",
                        "isResolved": false,
                        "isOutdated": false,
                        "path": "src/retry.rs",
                        "line": jitter_line,
                        "startLine": null,
                        "diffSide": "RIGHT",
                        "comments": { "nodes": [
                            comment(9101, "devon", "Jitter is added after the cap, so `max` isn't really the maximum: the last wait can reach 6.25 s. Cap after adding it?", 4),
                            comment(9102, AUTHOR, "That's on purpose: the spread above the cap keeps the nightly job from retrying in lockstep. I'll say so in the doc comment.", 4),
                        ] },
                    },
                    {
                        "id": "PRRT_overflow",
                        "isResolved": true,
                        "isOutdated": false,
                        "path": "src/retry.rs",
                        "line": exponential_line,
                        "startLine": null,
                        "diffSide": "RIGHT",
                        "comments": { "nodes": [
                            comment(9105, "maya", "Can `saturating_pow` overflow before the multiply?", 4),
                            comment(9106, AUTHOR, "No: both saturate, so a huge attempt count just waits `max`.", 4),
                        ] },
                    },
                    {
                        "id": "PRRT_ceiling",
                        "isResolved": false,
                        "isOutdated": false,
                        "path": "src/retry.rs",
                        "line": sum_line,
                        "startLine": null,
                        "diffSide": "RIGHT",
                        "comments": { "nodes": [
                            comment(9107, "maya", "If `max` should be a real ceiling, cap after adding the jitter:\n\n```suggestion\n        (capped + Duration::from_millis(jitter)).min(self.max)\n```", 5),
                        ] },
                    },
                    {
                        "id": "PRRT_retryable",
                        "isResolved": true,
                        "isOutdated": false,
                        "path": "src/payments.rs",
                        "line": retryable_line,
                        "startLine": null,
                        "diffSide": "RIGHT",
                        "comments": { "nodes": [
                            comment(9103, REVIEWER, "Should a 429 from the gateway count as retryable too?", 3),
                            comment(9104, AUTHOR, "They send 503 when rate limiting, so this covers it.", 3),
                        ] },
                    },
                ] },
                "reviews": { "nodes": [] },
                "state": "OPEN",
                "additions": 94,
                "deletions": 9,
                "files": { "nodes": viewed.iter().map(|(path, state)| json!({
                    "path": path,
                    "viewerViewedState": state,
                })).collect::<Vec<_>>() },
                "lastCommit": { "nodes": [ { "commit": { "statusCheckRollup": {
                    "state": "SUCCESS",
                    "contexts": { "nodes": [
                        check("build", 8101),
                        check("test", 8102),
                        check("clippy", 8103),
                        check("sandbox-charges", 8104),
                    ] },
                } } } ] },
            },
        } } })
        .to_string())
    }

    fn timeline_json(repo: &Repo) -> String {
        json!({ "data": { "repository": { "pullRequest": { "timelineItems": { "nodes": [
            { "__typename": "PullRequestCommit", "commit": {
                "abbreviatedOid": &repo.first_sha[..7],
                "messageHeadline": "Retry card charges with backoff",
            } },
            { "__typename": "IssueComment", "databaseId": 7001, "author": { "login": "devon" },
              "body": "Nice. Can we see how often this fires before we lower the nightly job's frequency?",
              "createdAt": "2026-10-03T09:12:00Z" },
            { "__typename": "PullRequestReview", "author": { "login": REVIEWER },
              "state": "COMMENTED",
              "body": "Looks right to me. One question about rate limits inline.",
              "submittedAt": "2026-10-03T11:40:00Z" },
            { "__typename": "PullRequestCommit", "commit": {
                "abbreviatedOid": &repo.head_sha[..7],
                "messageHeadline": "Add jitter and answer 503 while the gateway is down",
            } },
            { "__typename": "IssueComment", "databaseId": 7002, "author": { "login": AUTHOR },
              "body": "Added jitter and a 503 for the API. I'll add the retry dashboard in a follow-up.",
              "createdAt": "2026-10-04T08:30:00Z" },
            { "__typename": "PullRequestReview", "author": { "login": "devon" },
              "state": "APPROVED", "body": "",
              "submittedAt": "2026-10-04T10:05:00Z" },
        ] } } } } })
        .to_string()
    }

    fn is_timeline(request: &GhRequest) -> bool {
        request
            .stdin
            .as_deref()
            .is_some_and(|stdin| stdin.contains("timelineItems"))
    }

    fn inbox_pr(
        number: u64,
        title: &str,
        author: &str,
        checks: &str,
        open_threads: usize,
        commits: &[&str],
        reviewed_at: Option<&str>,
    ) -> serde_json::Value {
        let head = commits.last().copied().unwrap_or("0000000");
        json!({
            "id": format!("PR_{number}"),
            "number": number,
            "title": title,
            "url": format!("https://github.com/{OWNER}/{REPO}/pull/{number}"),
            "author": { "login": author },
            "headRefOid": head,
            "baseRefName": "main",
            "repository": { "nameWithOwner": format!("{OWNER}/{REPO}") },
            "commits": { "nodes": commits.iter().map(|oid| json!({ "commit": { "oid": oid } })).collect::<Vec<_>>() },
            "reviewThreads": { "nodes": (0..open_threads).map(|_| json!({ "isResolved": false })).collect::<Vec<_>>() },
            "reviews": { "nodes": reviewed_at.map(|oid| vec![json!({ "commit": { "oid": oid } })]).unwrap_or_default() },
            "lastCommit": { "nodes": [ { "commit": { "statusCheckRollup": { "state": checks } } } ] },
        })
    }

    fn inbox_json() -> String {
        json!({ "data": {
            "s0": { "nodes": [
                inbox_pr(214, "Retry declined card charges with backoff", AUTHOR, "SUCCESS", 1, &["a1c4e2f", "b7d9031"], Some("a1c4e2f")),
                inbox_pr(219, "Store invoice PDFs in object storage", "devon", "PENDING", 0, &["c2e8a10"], None),
                inbox_pr(221, "Fix rounding in multi-currency totals", "sam", "FAILURE", 2, &["d41f7b3", "e09a6c2"], None),
            ] },
            "s1": { "nodes": [
                inbox_pr(217, "Add a ledger export for finance", REVIEWER, "SUCCESS", 0, &["f3b2d18"], None),
            ] },
            "s2": { "nodes": [
                inbox_pr(205, "Webhook signature verification", "lee", "SUCCESS", 3, &["0a9e4c1"], None),
            ] },
            "s3": { "nodes": [
                inbox_pr(223, "Bump sqlx to 0.8.3", "lee", "SUCCESS", 0, &["1b7c5d9"], None),
                inbox_pr(198, "Draft: tax rules per region", "sam", "PENDING", 0, &["2c8d6e0"], None),
            ] },
            "nodes": [],
        } })
        .to_string()
    }

    /// The pre-review's findings, then the check that keeps them.
    struct ScriptedReviewAgent {
        replies: RefCell<VecDeque<String>>,
    }

    impl photon_review::ReviewAgent for ScriptedReviewAgent {
        fn review(
            &self,
            _: WeakEntity<Workspace>,
            _: String,
            _: PathBuf,
            _: &mut Window,
            _: &mut App,
        ) -> Task<Result<String>> {
            Task::ready(
                self.replies
                    .borrow_mut()
                    .pop_front()
                    .context("the agent has nothing more to say"),
            )
        }
    }

    struct StubAgentServer {
        connection: StubAgentConnection,
    }

    impl AgentServer for StubAgentServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            "Agent".into()
        }

        fn connect(
            &self,
            _: AgentServerDelegate,
            _: Entity<Project>,
            _: &mut App,
        ) -> Task<Result<Rc<dyn AgentConnection>>> {
            Task::ready(Ok(Rc::new(self.connection.clone())))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    // ---- The runner ----------------------------------------------------------------------

    struct Win {
        handle: WindowHandle<MultiWorkspace>,
        workspace: Entity<Workspace>,
        width: f32,
        height: f32,
    }

    /// Part of a close-up: an element drawn with a debug selector, or a rectangle in window
    /// coordinates.
    #[derive(Clone, Copy)]
    enum Area {
        Selector(&'static str),
        Rect(f32, f32, f32, f32),
    }

    /// Space kept around a close-up's areas, so it still reads as part of the app.
    const CROP_PADDING: f32 = 16.;
    /// Narrower close-ups would show the UI larger than life on the site.
    const MIN_CROP_WIDTH: f32 = 560.;

    /// Pads `(x, y, width, height)` and grows it to between 3:2 and 16:10 around its centre,
    /// kept inside the window.
    fn fit_crop(
        (x, y, width, height): (f32, f32, f32, f32),
        window_width: f32,
        window_height: f32,
    ) -> (f32, f32, f32, f32) {
        let (mut x, mut y) = (x - CROP_PADDING, y - CROP_PADDING);
        let mut width = width + CROP_PADDING * 2.;
        let mut height = height + CROP_PADDING * 2.;
        if width < MIN_CROP_WIDTH {
            x -= (MIN_CROP_WIDTH - width) / 2.;
            width = MIN_CROP_WIDTH;
        }
        if width / height < 1.5 {
            let grown = height * 1.5;
            x -= (grown - width) / 2.;
            width = grown;
        } else if width / height > 1.6 {
            let grown = width / 1.6;
            y -= (grown - height) / 2.;
            height = grown;
        }
        let width = width.min(window_width);
        let height = height.min(window_height);
        (
            x.clamp(0., window_width - width),
            y.clamp(0., window_height - height),
            width,
            height,
        )
    }

    struct Shooter {
        cx: VisualTestAppContext,
        app_state: Arc<AppState>,
        output: PathBuf,
        repo: Repo,
        _fake_servers: Vec<UnboundedReceiver<FakeLanguageServer>>,
    }

    impl Shooter {
        fn new(output: PathBuf, repo: Repo) -> Result<Self> {
            std::fs::create_dir_all(&output)?;
            let mut cx = VisualTestAppContext::with_asset_source(
                gpui_platform::current_platform(false),
                Arc::new(Assets),
            );
            cx.update(|cx| Assets.load_fonts(cx))?;
            cx.update(|cx| settings::init(cx));
            let app_state = cx.update(init_app_state);
            cx.update(|cx| init_subsystems(&app_state, cx));
            cx.run_until_parked();

            let functions = Arc::new(functions(&repo.root)?);
            let fake_servers = cx.update(|_| {
                let languages = &app_state.languages;
                languages.add(language::rust_lang());
                languages.add(language::markdown_lang());
                languages.add(Arc::new(Language::new(
                    LanguageConfig {
                        name: "Java".into(),
                        matcher: Arc::new(LanguageMatcher {
                            path_suffixes: vec!["java".into()],
                            ..LanguageMatcher::default()
                        }),
                        ..LanguageConfig::default()
                    },
                    None,
                )));
                vec![
                    languages.register_fake_lsp(
                        "Rust",
                        FakeLspAdapter {
                            name: "rust-analyzer",
                            capabilities: lsp::ServerCapabilities {
                                call_hierarchy_provider: Some(
                                    lsp::CallHierarchyServerCapability::Simple(true),
                                ),
                                ..lsp::ServerCapabilities::default()
                            },
                            initializer: Some(Box::new(move |server| {
                                answer_call_hierarchy(server, &functions)
                            })),
                            ..FakeLspAdapter::default()
                        },
                    ),
                    languages.register_fake_lsp(
                        "Java",
                        FakeLspAdapter {
                            name: "jdtls",
                            ..FakeLspAdapter::default()
                        },
                    ),
                ]
            });
            Ok(Self {
                cx,
                app_state,
                output,
                repo,
                _fake_servers: fake_servers,
            })
        }

        /// Runs foreground and background work, giving real file system and git work time to
        /// finish, and moves the clock past debounces and animations.
        fn settle(&mut self) {
            self.cx.background_executor.allow_parking();
            for _ in 0..30 {
                self.cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(15));
                self.cx.advance_clock(Duration::from_millis(100));
            }
            self.cx.run_until_parked();
            self.cx.background_executor.forbid_parking();
        }

        fn block<T>(&mut self, future: impl Future<Output = T>) -> T {
            self.cx.background_executor.allow_parking();
            let result = self.cx.foreground_executor.block_test(future);
            self.cx.background_executor.forbid_parking();
            result
        }

        fn set_theme(&mut self, light: bool) -> Result<()> {
            let mode = if light { "light" } else { "dark" };
            // The system shell would start through `login`, which resets HOME to the user's own
            // and loads their dotfiles; a named program starts zsh directly in the fixture home.
            let home = serde_json::to_string(&std::env::var("HOME")?)?;
            let json = format!(
                r#"{{
                    "theme": {{ "mode": "{mode}", "light": "One Light", "dark": "Photon Black" }},
                    "terminal": {{ "shell": {{ "program": "/bin/zsh" }}, "env": {{ "HOME": {home} }} }}
                }}"#
            );
            self.cx.update(|cx| {
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    store.set_user_settings(&json, cx).result().map(|_| ())
                })
            })?;
            self.settle();
            Ok(())
        }

        fn project(&mut self, root: &Path) -> Result<Entity<Project>> {
            let app_state = self.app_state.clone();
            let project = self.cx.update(|cx| {
                Project::local(
                    app_state.client.clone(),
                    app_state.node_runtime.clone(),
                    app_state.user_store.clone(),
                    app_state.languages.clone(),
                    app_state.fs.clone(),
                    None,
                    project::LocalProjectFlags {
                        init_worktree_trust: false,
                        ..Default::default()
                    },
                    cx,
                )
            });
            let add = project.update(&mut self.cx, |project, cx| {
                project.find_or_create_worktree(root, true, cx)
            });
            self.block(add)?;
            self.settle();
            Ok(project)
        }

        fn open_window(&mut self, root: &Path) -> Result<Win> {
            self.open_window_sized(root, WINDOW_WIDTH, WINDOW_HEIGHT)
        }

        fn open_window_sized(&mut self, root: &Path, width: f32, height: f32) -> Result<Win> {
            let project = self.project(root)?;
            let app_state = self.app_state.clone();
            let bounds = Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(width), px(height)),
            };
            let handle: WindowHandle<MultiWorkspace> = self.cx.update(|cx| {
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        focus: false,
                        show: false,
                        ..Default::default()
                    },
                    |window, cx| {
                        let workspace = cx.new(|cx| {
                            Workspace::new(None, project.clone(), app_state.clone(), window, cx)
                        });
                        cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
                    },
                )
            })?;
            let workspace = handle.update(&mut self.cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })?;
            let win = Win {
                handle,
                workspace,
                width,
                height,
            };
            self.update(&win, |workspace, window, cx| {
                add_status_items(workspace, window, cx)
            })?;
            self.settle();
            Ok(win)
        }

        fn update<R>(
            &mut self,
            win: &Win,
            update: impl FnOnce(&mut Workspace, &mut Window, &mut gpui::Context<Workspace>) -> R,
        ) -> Result<R> {
            let workspace = win.workspace.clone();
            win.handle.update(&mut self.cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| update(workspace, window, cx))
            })
        }

        fn add_panel<P: Panel, F: Future<Output = Result<Entity<P>>>>(
            &mut self,
            win: &Win,
            load: impl FnOnce(WeakEntity<Workspace>, AsyncWindowContext) -> F,
        ) -> Result<Entity<P>> {
            let workspace = win.workspace.downgrade();
            let async_window = win
                .handle
                .update(&mut self.cx, |_, window, cx| window.to_async(cx))?;
            let panel = self.block(load(workspace, async_window))?;
            self.update(win, |workspace, window, cx| {
                workspace.add_panel(panel.clone(), window, cx)
            })?;
            self.settle();
            Ok(panel)
        }

        fn open_file(&mut self, win: &Win, path: &str) -> Result<Entity<Editor>> {
            let open = self.update(win, |workspace, window, cx| {
                let worktree_id = workspace
                    .project()
                    .read(cx)
                    .worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).id())
                    .context("no worktree")?;
                let path: Arc<util::rel_path::RelPath> = util::rel_path::rel_path(path).into();
                anyhow::Ok(workspace.open_path(
                    ProjectPath::from((worktree_id, path)),
                    None,
                    true,
                    window,
                    cx,
                ))
            })??;
            let item = self.block(open)?;
            self.settle();
            item.downcast::<Editor>().context("not an editor")
        }

        /// Puts the cursor on the first line containing `needle` and focuses the editor.
        fn place_cursor(&mut self, win: &Win, editor: &Entity<Editor>, needle: &str) -> Result<()> {
            let text = editor.read_with(&self.cx, |editor, cx| editor.text(cx));
            let row = line_of(&text, needle)? - 1;
            let column = text
                .lines()
                .nth(row as usize)
                .and_then(|line| line.find(needle))
                .unwrap_or(0) as u32;
            let editor = editor.clone();
            win.handle.update(&mut self.cx, |_, window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.change_selections(
                        editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                        window,
                        cx,
                        |selections| {
                            let point = language::Point::new(row, column);
                            selections.select_ranges([point..point]);
                        },
                    );
                });
                window.focus(&editor.focus_handle(cx), cx);
            })?;
            self.settle();
            Ok(())
        }

        fn click(&mut self, win: &Win, selector: &str) -> Result<()> {
            self.refresh(win)?;
            let bounds = self
                .cx
                .debug_bounds(win.handle.into(), selector)
                .with_context(|| format!("{selector} isn't drawn"))?;
            self.cx
                .simulate_click(win.handle.into(), bounds.center(), Modifiers::none());
            self.settle();
            Ok(())
        }

        fn refresh(&mut self, win: &Win) -> Result<()> {
            self.cx
                .update_window(win.handle.into(), |_, window, _| window.refresh())?;
            self.cx.run_until_parked();
            Ok(())
        }

        fn dispatch(&mut self, win: &Win, action: impl gpui::Action) {
            self.cx.dispatch_action(win.handle.into(), action);
            self.settle();
        }

        /// Pumps the app in real time until `done`: shells and canned commands run outside it.
        fn wait_for(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) -> Result<()> {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !done(self) {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {what}"
                );
                self.cx.background_executor.allow_parking();
                self.cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(50));
                self.cx.advance_clock(Duration::from_millis(50));
                self.cx.run_until_parked();
                self.cx.background_executor.forbid_parking();
            }
            Ok(())
        }

        fn wait_for_prompt(&mut self, view: &Entity<TerminalView>) -> Result<()> {
            let terminal = view.read_with(&self.cx, |view, _| view.terminal().clone());
            self.wait_for("the shell's prompt", |this| {
                terminal.read_with(&this.cx, |terminal, _| {
                    terminal.input_target() == terminal::InputTarget::BlockInput
                })
            })
        }

        /// A zsh in the repository, in the active center pane.
        fn open_center_terminal(&mut self, win: &Win) -> Result<Entity<TerminalView>> {
            let project = win
                .workspace
                .read_with(&self.cx, |workspace, _| workspace.project().clone());
            let root = self.repo.root.clone();
            let create = project.update(&mut self.cx, |project, cx| {
                project.create_terminal_shell(Some(root), cx)
            });
            let terminal = self.block(create)?;
            let view = self.update(win, |workspace, window, cx| {
                let view = cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        workspace.weak_handle(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                });
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
                view
            })?;
            self.wait_for_prompt(&view)?;
            Ok(view)
        }

        /// The bottom terminal panel's first terminal, which opens in the repository.
        fn open_dock_terminal(&mut self, win: &Win) -> Result<Entity<TerminalView>> {
            let panel = self.add_panel(&win, TerminalPanel::load)?;
            self.update(win, |workspace, window, cx| {
                workspace.open_panel::<TerminalPanel>(window, cx)
            })?;
            let mut view = None;
            self.wait_for("the terminal panel's terminal", |this| {
                view = panel.read_with(&this.cx, |panel, cx| {
                    panel.panes().first().and_then(|pane| {
                        pane.read(cx)
                            .active_item()
                            .and_then(|item| item.downcast::<TerminalView>())
                    })
                });
                view.is_some()
            })?;
            let view = view.context("no terminal in the panel")?;
            self.wait_for_prompt(&view)?;
            Ok(view)
        }

        /// Runs `command` at the prompt and waits for its block to finish and the next prompt.
        fn run_in_terminal(&mut self, view: &Entity<TerminalView>, command: &str) -> Result<()> {
            let terminal = view.read_with(&self.cx, |view, _| view.terminal().clone());
            let finished = |terminal: &terminal::Terminal| {
                terminal.blocks().map_or(0, |blocks| {
                    blocks
                        .blocks()
                        .filter(|block| block.end_line.is_some())
                        .count()
                })
            };
            let before = terminal.read_with(&self.cx, |terminal, _| finished(terminal));
            terminal.update(&mut self.cx, |terminal, cx| terminal.run_command(command, cx));
            self.wait_for(command, |this| {
                terminal.read_with(&this.cx, |terminal, _| {
                    finished(terminal) > before
                        && terminal.input_target() == terminal::InputTarget::BlockInput
                })
            })?;
            // Smart blocks read the finished output off the UI thread.
            self.settle();
            Ok(())
        }

        /// Focuses the terminal and dispatches one of its actions by name, as a key binding would.
        fn terminal_action(
            &mut self,
            win: &Win,
            view: &Entity<TerminalView>,
            name: &str,
        ) -> Result<()> {
            win.handle.update(&mut self.cx, |_, window, cx| {
                window.focus(&view.focus_handle(cx), cx);
                let action = cx.build_action(name, None)?;
                window.dispatch_action(action, cx);
                anyhow::Ok(())
            })??;
            self.settle();
            Ok(())
        }

        fn shoot(&mut self, win: &Win, name: &str) -> Result<()> {
            self.shoot_crops(win, Some(name), &[])
        }

        /// Saves the whole window as `name` (if any) and each close-up as its own image.
        fn shoot_crops(
            &mut self,
            win: &Win,
            name: Option<&str>,
            crops: &[(&str, &[Area])],
        ) -> Result<()> {
            self.settle();
            self.refresh(win)?;
            let image = self.cx.capture_screenshot(win.handle.into())?;
            // PHOTON_FULL=<dir> keeps every whole-window capture, for placing close-ups.
            if let Ok(directory) = std::env::var("PHOTON_FULL") {
                let stem = name.or(crops.first().map(|(name, _)| *name)).unwrap_or("scene");
                image.save(Path::new(&directory).join(format!("{stem}-full.png")))?;
            }
            if let Some(name) = name {
                self.save(&image, name)?;
            }
            let scale = image.width() as f32 / win.width;
            for (crop_name, areas) in crops {
                let mut union: Option<Bounds<Pixels>> = None;
                for area in *areas {
                    let bounds = match area {
                        Area::Selector(selector) => self
                            .cx
                            .debug_bounds(win.handle.into(), selector)
                            .with_context(|| format!("{selector} isn't drawn"))?,
                        Area::Rect(x, y, width, height) => Bounds {
                            origin: point(px(*x), px(*y)),
                            size: size(px(*width), px(*height)),
                        },
                    };
                    union = Some(union.map_or(bounds, |union| union.union(&bounds)));
                }
                let union = union.context("a close-up needs an area")?;
                let (x, y, width, height) = fit_crop(
                    (
                        f32::from(union.origin.x),
                        f32::from(union.origin.y),
                        f32::from(union.size.width),
                        f32::from(union.size.height),
                    ),
                    win.width,
                    win.height,
                );
                let to_pixels = |value: f32| (value * scale).round() as u32;
                let cropped = image::imageops::crop_imm(
                    &image,
                    to_pixels(x),
                    to_pixels(y),
                    to_pixels(width),
                    to_pixels(height),
                )
                .to_image();
                self.save(&cropped, crop_name)?;
            }
            Ok(())
        }

        fn save(&self, image: &image::RgbaImage, name: &str) -> Result<()> {
            let png = self.output.join(format!("{name}.png"));
            image.save(&png)?;
            save_webp(&png, &self.output, name, image.width() / 2)?;
            println!("  saved {name} ({}x{})", image.width(), image.height());
            Ok(())
        }

        fn close(&mut self, win: Win) {
            let project = win
                .workspace
                .read_with(&self.cx, |workspace, _| workspace.project().clone());
            project.update(&mut self.cx, |project, cx| {
                let ids: Vec<_> = project
                    .worktrees(cx)
                    .map(|worktree| worktree.read(cx).id())
                    .collect();
                for id in ids {
                    project.remove_worktree(id, cx);
                }
            });
            self.cx
                .update_window(win.handle.into(), |_, window, _| window.remove_window())
                .log_err();
            self.settle();
        }

        // ---- Scenes --------------------------------------------------------------------

        fn hero(&mut self, name: &str, light: bool) -> Result<()> {
            self.set_theme(light)?;
            let root = self.repo.root.clone();
            let win = self.open_window(&root)?;
            self.add_panel(&win, project_panel::ProjectPanel::load)?;
            self.update(&win, |workspace, window, cx| {
                workspace.open_panel::<project_panel::ProjectPanel>(window, cx)
            })?;
            let editor = self.open_file(&win, "src/payments.rs")?;
            let terminal = self.open_dock_terminal(&win)?;
            self.run_in_terminal(&terminal, "cargo build")?;
            self.run_in_terminal(&terminal, "kubectl get pods")?;
            self.place_cursor(&win, &editor, "retry_with_backoff(self.backoff")?;
            let crop = format!("{name}-crop");
            let crops: &[(&str, &[Area])] = if light {
                &[]
            } else {
                &[(crop.as_str(), &[Area::Rect(240., 40., 1200., 840.)])]
            };
            let result = self.shoot_crops(&win, Some(name), crops);
            self.close(win);
            self.set_theme(false)?;
            result
        }

        fn terminal_window(&mut self, width: f32, height: f32) -> Result<(Win, Entity<TerminalView>)> {
            let root = self.repo.root.clone();
            let win = self.open_window_sized(&root, width, height)?;
            let view = self.open_center_terminal(&win)?;
            Ok((win, view))
        }

        fn terminal_blocks(&mut self) -> Result<()> {
            let (win, view) = self.terminal_window(1000., 625.)?;
            for command in ["git log --oneline -3", "cargo build", "cargo clippy"] {
                self.run_in_terminal(&view, command)?;
            }
            let result = self.shoot_crops(
                &win,
                None,
                &[("cu-terminal-blocks", &[Area::Rect(0., 0., 1000., 625.)])],
            );
            self.close(win);
            result
        }

        fn terminal_tests(&mut self) -> Result<()> {
            let (win, view) = self.terminal_window(1150., 766.)?;
            self.run_in_terminal(&view, "cargo test")?;
            self.terminal_action(&win, &view, "photon_terminal::SelectPreviousBlock")?;
            let result = self.shoot_crops(
                &win,
                None,
                &[("cu-terminal-tests", &[Area::Rect(0., 0., 1150., 766.)])],
            );
            self.close(win);
            result
        }

        fn terminal_table(&mut self) -> Result<()> {
            let root = self.repo.root.clone();
            let win = self.open_window_sized(&root, 1100., 720.)?;
            let view = self.open_dock_terminal(&win)?;
            self.run_in_terminal(&view, "kubectl get pods")?;
            self.terminal_action(&win, &view, "photon_terminal::SelectPreviousBlock")?;
            self.terminal_action(&win, &view, "photon_terminal::OpenBlockAsTable")?;
            let result = self.shoot_crops(
                &win,
                None,
                &[("cu-terminal-table", &[Area::Rect(0., 0., 1100., 720.)])],
            );
            self.close(win);
            result
        }

        fn terminal_git(&mut self) -> Result<()> {
            let root = self.repo.root.clone();
            let retry = std::fs::read_to_string(root.join("src/retry.rs"))?;
            write_files(
                &root,
                &[
                    (
                        "src/retry.rs",
                        retry.replace(
                            "        capped + Duration::from_millis(jitter)",
                            "        (capped + Duration::from_millis(jitter)).min(self.max)",
                        ),
                    ),
                    (
                        "Cargo.toml",
                        fixture::CARGO_TOML.replace("version = \"0.4.0\"", "version = \"0.5.0\""),
                    ),
                    ("docs/runbook.md", fixture::RUNBOOK.to_string()),
                    (
                        "src/api.rs",
                        fixture::API_RS_HEAD.replace("try again shortly", "try again in a minute"),
                    ),
                ],
            )?;
            git(&root, &["add", "Cargo.toml"], None)?;
            let result = (|| {
                let (win, view) = self.terminal_window(1000., 625.)?;
                self.run_in_terminal(&view, "git status")?;
                self.terminal_action(&win, &view, "photon_terminal::SelectPreviousBlock")?;
                let result = self.shoot_crops(
                    &win,
                    None,
                    &[("cu-terminal-git", &[Area::Rect(0., 0., 1000., 625.)])],
                );
                self.close(win);
                result
            })();
            git(&root, &["reset", "--hard", "--quiet"], None)?;
            git(&root, &["clean", "-fdq"], None)?;
            result
        }

        /// Opens the pull request's review tab the way `open_review` places it, from the
        /// fixture's real diff.
        fn open_review(&mut self, viewed: &[(&str, &str)]) -> Result<(Win, Entity<ReviewItem>)> {
            let fake = FakeGh::new();
            fake.respond(is_timeline, Ok(timeline_json(&self.repo)));
            self.cx.update(|cx| set_gh_runner(fake, cx));
            let root = self.repo.root.clone();
            let win = self.open_window(&root)?;
            let detail = parse_detail(&pr_key(), &detail_json(&self.repo, viewed)?)?;
            let patch = git(
                &root,
                &[
                    "diff",
                    &format!("{}..{}", self.repo.base_sha, self.repo.head_sha),
                ],
                None,
            )?;
            let project = win
                .workspace
                .read_with(&self.cx, |workspace, _| workspace.project().clone());
            let mut files = Vec::new();
            for diff in parse_unified_diff(&patch)? {
                let base = match diff.status {
                    photon_review::FileStatus::Added => None,
                    _ => Some(git(
                        &root,
                        &["show", &format!("{}:{}", self.repo.base_sha, diff.path)],
                        None,
                    )?),
                };
                let head_sha = self.repo.head_sha.clone();
                let load = self.cx.update(|cx| {
                    build_loaded_file(
                        &project,
                        root.join(&diff.path),
                        &root,
                        &head_sha,
                        base,
                        diff,
                        cx,
                    )
                });
                files.push(self.block(load)?);
            }
            let database = self.block(ReviewDb::open_test_db(unique_name("review")));
            let checkout = Checkout {
                repo_dir: root.clone(),
                worktree: root.clone(),
                head_sha: self.repo.head_sha.clone(),
                merge_base: self.repo.base_sha.clone(),
                head_changed: false,
            };
            let item = self.update(&win, |workspace, window, cx| {
                let item = cx.new(|cx| {
                    ReviewItem::new(
                        project.clone(),
                        detail,
                        REVIEWER.into(),
                        checkout,
                        files,
                        database,
                        window,
                        cx,
                    )
                });
                workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
                item
            })?;
            self.settle();
            Ok((win, item))
        }

        fn select_step_with(
            &mut self,
            win: &Win,
            item: &Entity<ReviewItem>,
            path: &str,
        ) -> Result<()> {
            let index = item
                .read_with(&self.cx, |item, _| {
                    item.steps()
                        .iter()
                        .position(|step| step.files.iter().any(|file| file == path))
                })
                .with_context(|| format!("no step shows {path}"))?;
            let item = item.clone();
            win.handle.update(&mut self.cx, |_, window, cx| {
                item.update(cx, |item, cx| item.select_step(index, window, cx))
            })?;
            self.settle();
            Ok(())
        }

        fn all_unviewed() -> Vec<(&'static str, &'static str)> {
            vec![
                ("src/api.rs", "UNVIEWED"),
                ("src/main.rs", "VIEWED"),
                ("src/payments.rs", "UNVIEWED"),
                ("src/retry.rs", "UNVIEWED"),
            ]
        }

        fn pr_review(&mut self) -> Result<()> {
            let (win, item) = self.open_review(&Self::all_unviewed())?;
            let retry = std::fs::read_to_string(self.repo.root.join("src/retry.rs"))?;
            let finding_line = line_of(&retry, "pub fn delay(")?;
            let findings = json!([{
                "path": "src/retry.rs",
                "line": finding_line,
                "severity": "should fix",
                "category": "reliability",
                "body": "With these defaults the four attempts can wait about 8 s in total, and `charge_invoice` awaits all of them while it holds the HTTP request. The load balancer cuts requests at 5 s, so the client sees a 504 even when the last attempt succeeds. Give the API handler a shorter budget and keep this one for the nightly job.",
            }]);
            let agent = Rc::new(ScriptedReviewAgent {
                replies: RefCell::new(VecDeque::from([
                    format!("```json\n{findings}\n```"),
                    "```json\n[{\"finding\": 1, \"verdict\": \"keep\", \"reason\": \"The handler awaits the whole retry loop.\"}]\n```".to_string(),
                ])),
            });
            self.cx.update(|cx| set_review_agent(agent, cx));
            {
                let item = item.clone();
                win.handle.update(&mut self.cx, |_, window, cx| {
                    item.update(cx, |item, cx| item.start_agent_review(window, cx))
                })?;
            }
            self.settle();
            self.select_step_with(&win, &item, "src/retry.rs")?;
            // Brings the draft and the thread below it into view.
            let editor = item.read_with(&self.cx, |item, _| item.editor().clone());
            win.handle.update(&mut self.cx, |_, window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.set_scroll_position(gpui::point(0., 22.), window, cx)
                })
            })?;
            self.settle();
            let result = self.shoot(&win, "pr-review");
            self.close(win);
            result
        }

        fn pr_merge(&mut self) -> Result<()> {
            let (win, item) = self.open_review(&Self::all_unviewed())?;
            self.select_step_with(&win, &item, "src/retry.rs")?;
            self.click(&win, "photon-review-readiness")?;
            let result = self.shoot(&win, "pr-merge");
            self.close(win);
            result
        }

        fn pr_conversation(&mut self) -> Result<()> {
            let (win, _item) = self.open_review(&Self::all_unviewed())?;
            self.click(&win, "photon-review-conversation")?;
            let result = self.shoot(&win, "pr-conversation");
            self.close(win);
            result
        }

        fn pr_unreviewed(&mut self) -> Result<()> {
            let (win, item) = self.open_review(&[
                ("src/api.rs", "UNVIEWED"),
                ("src/main.rs", "VIEWED"),
                ("src/payments.rs", "VIEWED"),
                ("src/retry.rs", "DISMISSED"),
            ])?;
            self.select_step_with(&win, &item, "src/retry.rs")?;
            self.click(&win, "photon-review-view")?;
            // Highlights the entry the scene is about, the last one in the menu.
            self.dispatch(&win, menu::SelectLast);
            let result = self.shoot(&win, "pr-unreviewed");
            self.close(win);
            result
        }

        fn markdown(&mut self) -> Result<()> {
            let root = self.repo.root.clone();
            let win = self.open_window(&root)?;
            self.add_panel(&win, project_panel::ProjectPanel::load)?;
            self.update(&win, |workspace, window, cx| {
                workspace.open_panel::<project_panel::ProjectPanel>(window, cx)
            })?;
            // Markdown files open in their preview.
            self.open_file(&win, "docs/architecture.md")?;
            let result = self.shoot(&win, "markdown");
            self.close(win);
            result
        }

        fn call_hierarchy(&mut self) -> Result<()> {
            let root = self.repo.root.clone();
            let win = self.open_window(&root)?;
            self.add_panel(&win, photon_call_trails::CallHierarchyPanel::load)?;
            let editor = self.open_file(&win, "src/payments.rs")?;
            self.place_cursor(&win, &editor, "charge(&self")?;
            self.dispatch(&win, photon_call_trails::ShowInPanel);
            // Expands each caller in turn: the first under the root, then its own first caller.
            for action in ["next", "child", "next", "child", "next", "next", "child"] {
                match action {
                    "next" => self.dispatch(&win, menu::SelectNext),
                    _ => self.dispatch(&win, menu::SelectChild),
                }
            }
            let result = self.shoot(&win, "call-hierarchy");
            self.close(win);
            result
        }

        fn language_server_help(&mut self) -> Result<()> {
            let root = self.repo.root.clone();
            let win = self.open_window(&root)?;
            self.add_panel(&win, project_panel::ProjectPanel::load)?;
            self.update(&win, |workspace, window, cx| {
                workspace.open_panel::<project_panel::ProjectPanel>(window, cx)
            })?;
            let editor = self.open_file(
                &win,
                "clients/java/src/main/java/dev/billing/client/BillingClient.java",
            )?;
            self.place_cursor(&win, &editor, "public ChargeResult charge")?;
            self.cx.update(|_| {
                self.app_state.languages.update_lsp_binary_status(
                    LanguageServerName::new_static("jdtls"),
                    BinaryStatus::Failed {
                        error: fixture::JAVA_ERROR.to_string(),
                    },
                )
            });
            self.settle();
            self.click(&win, "photon-set-up-language-server")?;
            let result = self.shoot(&win, "language-server-help");
            self.close(win);
            result
        }

        fn agent(&mut self) -> Result<()> {
            let parent = self
                .repo
                .root
                .parent()
                .context("no parent")?
                .with_file_name("agent");
            let repo = Repo::create(&parent)?;
            let win = self.open_window(&repo.root)?;
            self.open_file(&win, "src/retry.rs")?;
            let panel = self.add_panel(&win, agent_ui::AgentPanel::load)?;
            self.update(&win, |workspace, window, cx| {
                workspace.open_panel::<agent_ui::AgentPanel>(window, cx)
            })?;
            let connection = StubAgentConnection::new()
                .without_rewind()
                .without_model_selector();
            let server: Rc<dyn AgentServer> = Rc::new(StubAgentServer {
                connection: connection.clone(),
            });
            win.handle.update(&mut self.cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.open_external_thread_with_server(server, window, cx)
                })
            })?;
            self.settle();
            let thread = panel
                .read_with(&self.cx, |panel, cx| panel.active_agent_thread(cx))
                .context("no thread")?;
            let end_turn = connection.defer_next_prompt_response();
            let send = thread.update(&mut self.cx, |thread, cx| {
                thread.send(
                    vec!["Cap the backoff after adding jitter so `max` is really the maximum, and add a test for it.".into()],
                    cx,
                )
            });
            self.settle();
            let title = thread.update(&mut self.cx, |thread, cx| {
                thread.set_title("Cap the backoff after jitter".into(), cx)
            });
            self.block(title)?;
            // The agent reads, then edits files during its turn.
            let retry_path = repo.root.join("src/retry.rs");
            let architecture_path = repo.root.join("docs/architecture.md");
            let retry = fixture::RETRY_RS
                .replace(
                    "        capped + Duration::from_millis(jitter)",
                    "        // Jitter spreads retries from many invoices; the cap still bounds the wait.\n        (capped + Duration::from_millis(jitter)).min(self.max)",
                )
                .replace(
                    "        assert!(backoff.delay(10) <= Duration::from_millis(6250));",
                    "        assert!(backoff.delay(10) <= backoff.max);",
                );
            std::fs::write(&retry_path, &retry)?;
            let architecture = fixture::ARCHITECTURE.replace(
                "with up to 25% jitter:",
                "with up to 25% jitter, never past 5 s:",
            );
            std::fs::write(&architecture_path, &architecture)?;
            let updates = vec![
                acp_v1::SessionUpdate::ToolCall(
                    acp_v1::ToolCall::new("read-retry", "Read src/retry.rs")
                        .kind(acp_v1::ToolKind::Read)
                        .status(acp_v1::ToolCallStatus::Completed)
                        .locations(vec![acp_v1::ToolCallLocation::new(&retry_path)]),
                ),
                acp_v1::SessionUpdate::ToolCall(
                    acp_v1::ToolCall::new("edit-retry", "Edit src/retry.rs")
                        .kind(acp_v1::ToolKind::Edit)
                        .status(acp_v1::ToolCallStatus::Completed)
                        .locations(vec![acp_v1::ToolCallLocation::new(&retry_path)])
                        .content(vec![acp_v1::ToolCallContent::Diff(
                            acp_v1::Diff::new(&retry_path, retry).old_text(fixture::RETRY_RS.to_string()),
                        )]),
                ),
                acp_v1::SessionUpdate::ToolCall(
                    acp_v1::ToolCall::new("edit-architecture", "Edit docs/architecture.md")
                        .kind(acp_v1::ToolKind::Edit)
                        .status(acp_v1::ToolCallStatus::Completed)
                        .locations(vec![acp_v1::ToolCallLocation::new(&architecture_path)])
                        .content(vec![acp_v1::ToolCallContent::Diff(
                            acp_v1::Diff::new(&architecture_path, architecture)
                                .old_text(fixture::ARCHITECTURE.to_string()),
                        )]),
                ),
                acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                    "Jitter is now added before the cap, so `max` is the longest any retry waits. \
                     The comment says why the jitter is there, the test checks that no delay passes \
                     `max`, and the architecture doc mentions the limit.\n\n`cargo test retry` passes."
                        .into(),
                )),
            ];
            // One at a time, as an agent streams them, so the view keeps up with each entry.
            for update in updates {
                thread.update(&mut self.cx, |thread, cx| {
                    thread.handle_session_update(update, cx)
                })?;
                self.cx.run_until_parked();
            }
            self.settle();
            end_turn
                .send(Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::EndTurn)))
                .map_err(|_| anyhow!("the turn already ended"))?;
            self.block(send)?;
            self.settle();
            let result = self.shoot(&win, "agent");
            self.close(win);
            result
        }

        fn inbox(&mut self) -> Result<()> {
            let fake = FakeGh::new();
            fake.respond(
                |request| request.args.first().is_some_and(|arg| arg == "auth"),
                Ok(json!({ "hosts": { "github.com": [
                    { "state": "success", "active": true, "login": REVIEWER }
                ] } })
                .to_string()),
            );
            fake.respond(
                |request| request.args.first().is_some_and(|arg| arg == "api"),
                Ok(inbox_json()),
            );
            self.cx.update(|cx| set_gh_runner(fake, cx));
            let root = self.repo.root.clone();
            let win = self.open_window(&root)?;
            let panel = self.add_panel(&win, photon_review::InboxPanel::load)?;
            self.update(&win, |workspace, window, cx| {
                workspace.open_panel::<photon_review::InboxPanel>(window, cx)
            })?;
            panel.update(&mut self.cx, |panel, cx| panel.refresh(cx));
            self.settle();
            self.open_file(&win, "src/payments.rs")?;
            let result = self.shoot(&win, "inbox");
            self.close(win);
            result
        }
    }

    fn unique_name(prefix: &str) -> &'static str {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "photon_screenshots_{prefix}_{}",
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        name.leak()
    }

    /// Writes `<name>.webp` at the captured 2x size and `<name>-1x.webp` `width_1x` wide, each
    /// under `MAX_IMAGE_BYTES`, and removes the PNG. Without cwebp the PNG stays.
    fn save_webp(png: &Path, output: &Path, name: &str, width_1x: u32) -> Result<()> {
        if std::process::Command::new("cwebp")
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("  cwebp not found; kept {}", png.display());
            return Ok(());
        }
        let width_1x = width_1x.to_string();
        for (suffix, resize) in [("", None), ("-1x", Some((width_1x.as_str(), "0")))] {
            let webp = output.join(format!("{name}{suffix}.webp"));
            for quality in [90, 85, 80, 75, 70, 60] {
                let mut command = std::process::Command::new("cwebp");
                command.args(["-quiet", "-q", &quality.to_string(), "-m", "6"]);
                if let Some((width, height)) = resize {
                    command.args(["-resize", width, height]);
                }
                let status = command.arg(png).arg("-o").arg(&webp).status()?;
                anyhow::ensure!(status.success(), "cwebp failed on {}", png.display());
                if std::fs::metadata(&webp)?.len() <= MAX_IMAGE_BYTES {
                    break;
                }
            }
        }
        std::fs::remove_file(png)?;
        Ok(())
    }

    fn init_app_state(cx: &mut App) -> Arc<AppState> {
        use fs::Fs;
        use node_runtime::NodeRuntime;
        use session::Session;

        // Fresh databases, so nothing from the user's own Photon shows up.
        cx.set_global(db::AppDatabase::test_new());
        let fs: Arc<dyn Fs> = fs::RealFs::new(None, cx.background_executor().clone());
        <dyn Fs>::set_global(fs.clone(), cx);
        let languages = Arc::new(language::LanguageRegistry::test(
            cx.background_executor().clone(),
        ));
        let clock = Arc::new(clock::FakeSystemClock::new());
        let http_client = http_client::FakeHttpClient::with_404_response();
        let client = client::Client::new(clock, http_client, cx);
        let session = cx.new(|cx| session::AppSession::new(Session::test(), cx));
        let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
        let workspace_store = cx.new(|cx| workspace::WorkspaceStore::new(client.clone(), cx));
        theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);
        client::init(&client, cx);
        let app_state = Arc::new(AppState {
            client,
            fs,
            languages,
            user_store,
            workspace_store,
            node_runtime: NodeRuntime::unavailable(),
            build_window_options: |_, _| Default::default(),
            session,
        });
        AppState::set_global(app_state.clone(), cx);
        app_state
    }

    fn init_subsystems(app_state: &Arc<AppState>, cx: &mut App) {
        gpui_tokio::init(cx);
        client::init(&app_state.client, cx);
        audio::init(cx);
        workspace::init(app_state.clone(), cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        command_palette::init(cx);
        editor::init(cx);
        call::init(app_state.client.clone(), app_state.user_store.clone(), cx);
        title_bar::init(cx);
        project_panel::init(cx);
        terminal_view::init(cx);
        search::init(cx);
        lsp_locations::init(cx);
        call_hierarchy::init(cx);
        cx.set_global(workspace::PaneSearchBarCallbacks {
            setup_search_bar: |languages, toolbar, window, cx| {
                let search_bar = cx.new(|cx| search::BufferSearchBar::new(languages, window, cx));
                toolbar.update(cx, |toolbar, cx| toolbar.add_item(search_bar, window, cx));
            },
            wrap_div_with_search_actions: search::buffer_search::register_pane_search_actions,
        });
        prompt_store::init(cx);
        let prompt_builder = prompt_store::PromptBuilder::load(app_state.fs.clone(), false, cx);
        language_model::init(cx);
        client::RefreshLlmTokenListener::register(
            app_state.client.clone(),
            app_state.user_store.clone(),
            cx,
        );
        language_models::init(app_state.user_store.clone(), app_state.client.clone(), cx);
        git_ui::init(cx);
        project::AgentRegistryStore::init_global(
            cx,
            app_state.fs.clone(),
            app_state.client.http_client(),
        );
        agent_ui::init(
            app_state.fs.clone(),
            prompt_builder,
            app_state.languages.clone(),
            false,
            false,
            cx,
        );
        app_state
            .languages
            .set_theme(theme::GlobalTheme::theme(cx).clone());
        cx.observe_global::<theme::GlobalTheme>({
            let languages = app_state.languages.clone();
            move |cx| languages.set_theme(theme::GlobalTheme::theme(cx).clone())
        })
        .detach();
        markdown_preview::init(cx);
        photon_terminal::init(cx);
        photon_call_trails::init(cx);
        photon_review::init(cx);
        photon_navigation::init(cx);
        agent_settings::AgentSettings::override_global(
            agent_settings::AgentSettings {
                notify_when_agent_waiting: settings::NotifyWhenAgentWaiting::Never,
                play_sound_when_agent_done: settings::PlaySoundWhenAgentDone::Never,
                ..agent_settings::AgentSettings::get_global(cx).clone()
            },
            cx,
        );
        // Menus show the same shortcuts as the app.
        if let Some(bindings) = settings::KeymapFile::load_asset_allow_partial_failure(
            settings::DEFAULT_KEYMAP_PATH,
            cx,
        )
        .log_err()
        {
            cx.bind_keys(bindings);
        }
    }

    /// The status bar items the scenes show, as the app adds them.
    fn add_status_items(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) {
        let set_up_language_server =
            cx.new(|cx| photon_navigation::SetUpLanguageServer::new(workspace, cx));
        let active_file_name = cx.new(|_| workspace::active_file_name::ActiveFileName::new());
        let cursor_position =
            cx.new(|_| go_to_line::cursor_position::CursorPosition::new(workspace));
        let language = cx.new(|_| language_selector::ActiveBufferLanguage::new(workspace));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_left_item(set_up_language_server, window, cx);
            status_bar.add_left_item(active_file_name, window, cx);
            status_bar.add_right_item(language, window, cx);
            status_bar.add_right_item(cursor_position, window, cx);
        });
    }
}
