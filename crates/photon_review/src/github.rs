use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::RangeInclusive;

use anyhow::{Context as _, Result, anyhow};
use serde::Deserialize;

use crate::gh::{GhRequest, graphql_request, rest_request};
use crate::{Draft, Placement, place, quoted_body, review_body};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PrKey {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostAuth {
    pub host: String,
    pub login: Option<String>,
}

/// One list in the inbox: `query` is GitHub search syntax narrowing the open pull requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxSection {
    pub name: String,
    pub query: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinceReview {
    NotReviewed,
    UpToDate,
    NewCommits(usize),
    ForcePushed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrSummary {
    pub key: PrKey,
    pub node_id: String,
    pub title: String,
    pub author: String,
    pub url: String,
    pub head_sha: String,
    pub base_ref: String,
    pub checks: ChecksState,
    pub unresolved_threads: usize,
    pub last_review_commit: Option<String>,
    pub since_review: SinceReview,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inbox {
    pub sections: Vec<(String, Vec<PrSummary>)>,
    pub closed_tracked: Vec<String>,
    /// Tracked pull requests GitHub no longer resolves (deleted, or access lost).
    pub gone_tracked: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadComment {
    pub database_id: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub side: Side,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub comments: Vec<ThreadComment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrFile {
    pub path: String,
    pub viewed: bool,
    /// GitHub's `DISMISSED`: viewed, then changed by a later push.
    pub changed_since_viewed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrDetail {
    pub summary: PrSummary,
    pub state: PrState,
    pub additions: u64,
    pub deletions: u64,
    pub files: Vec<PrFile>,
    pub threads: Vec<ReviewThread>,
    /// The pull request description.
    pub body: String,
    /// The branch the pull request merges from, and the repository holding it (a fork's, for a
    /// pull request from a fork; None when that fork was deleted).
    pub head_ref: String,
    pub head_repository: Option<String>,
    pub merge: MergeStatus,
    /// General comments on the conversation, not counting review comments on lines.
    pub comment_count: usize,
    /// Who reviewed, and who is asked to, for `@` completion.
    pub reviewers: Vec<String>,
    pub requested_reviewers: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            MergeMethod::Merge => "MERGE",
            MergeMethod::Squash => "SQUASH",
            MergeMethod::Rebase => "REBASE",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MergeMethod::Merge => "Merge",
            MergeMethod::Squash => "Squash and merge",
            MergeMethod::Rebase => "Rebase and merge",
        }
    }

    /// The method as the start of a sentence, like "Squash and merge #482 into main?".
    pub fn verb(self) -> &'static str {
        match self {
            MergeMethod::Merge => "Merge",
            MergeMethod::Squash => "Squash and merge",
            MergeMethod::Rebase => "Rebase and merge",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Passed,
    Failed,
    Running,
}

impl CheckOutcome {
    pub fn label(self) -> &'static str {
        match self {
            CheckOutcome::Passed => "passed",
            CheckOutcome::Failed => "failed",
            CheckOutcome::Running => "running",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub outcome: CheckOutcome,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeStatus {
    pub is_draft: bool,
    /// GitHub's `mergeable`: MERGEABLE, CONFLICTING, or UNKNOWN while GitHub works it out.
    pub mergeable: String,
    pub merge_state: String,
    pub review_decision: Option<String>,
    /// The head commit's combined check state.
    pub checks_state: Option<String>,
    pub checks: Vec<Check>,
    /// The methods the repository allows, its default first.
    pub methods: Vec<MergeMethod>,
}

/// The one thing that matters most about whether the pull request can merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    Merged,
    Closed,
    Draft,
    Conflicts,
    ChecksFailing,
    ChangesRequested,
    ReviewRequired,
    ChecksRunning,
    Checking,
    Behind(String),
    Ready,
}

impl Readiness {
    pub fn label(&self) -> String {
        match self {
            Readiness::Merged => "Merged".into(),
            Readiness::Closed => "Closed".into(),
            Readiness::Draft => "Draft".into(),
            Readiness::Conflicts => "Conflicts".into(),
            Readiness::ChecksFailing => "Checks failing".into(),
            Readiness::ChangesRequested => "Changes requested".into(),
            Readiness::ReviewRequired => "Review required".into(),
            Readiness::ChecksRunning => "Checks running".into(),
            Readiness::Checking => "Checking…".into(),
            Readiness::Behind(base) => format!("Behind {base}"),
            Readiness::Ready => "Ready to merge".into(),
        }
    }
}

impl PrDetail {
    pub fn readiness(&self) -> Readiness {
        let merge = &self.merge;
        match self.state {
            PrState::Merged => return Readiness::Merged,
            PrState::Closed => return Readiness::Closed,
            PrState::Open => {}
        }
        let checks = merge.checks_state.as_deref();
        let decision = merge.review_decision.as_deref();
        if merge.is_draft {
            Readiness::Draft
        } else if merge.mergeable == "CONFLICTING" || merge.merge_state == "DIRTY" {
            Readiness::Conflicts
        } else if matches!(checks, Some("FAILURE" | "ERROR")) {
            Readiness::ChecksFailing
        } else if decision == Some("CHANGES_REQUESTED") {
            Readiness::ChangesRequested
        } else if decision == Some("REVIEW_REQUIRED") {
            Readiness::ReviewRequired
        } else if matches!(checks, Some("PENDING" | "EXPECTED")) {
            Readiness::ChecksRunning
        } else if merge.mergeable == "UNKNOWN" || merge.merge_state == "UNKNOWN" {
            // Saying "Ready" before GitHub has worked out conflicts could be wrong.
            Readiness::Checking
        } else if merge.merge_state == "BEHIND" {
            Readiness::Behind(self.summary.base_ref.clone())
        } else {
            Readiness::Ready
        }
    }
}

#[derive(Deserialize)]
struct RawAuthStatus {
    hosts: BTreeMap<String, Vec<RawAccount>>,
}

#[derive(Deserialize)]
struct RawAccount {
    state: String,
    #[serde(default)]
    active: bool,
    login: Option<String>,
}

pub fn auth_status_request() -> GhRequest {
    GhRequest {
        args: ["auth", "status", "--json", "hosts"]
            .map(String::from)
            .to_vec(),
        stdin: None,
    }
}

pub fn parse_auth_status(json: &str) -> Result<Vec<HostAuth>> {
    let status: RawAuthStatus = serde_json::from_str(json).context("parsing gh auth status")?;
    Ok(status
        .hosts
        .into_iter()
        .filter_map(|(host, accounts)| {
            let account = accounts
                .iter()
                .find(|account| account.active)
                .or(accounts.first())?;
            let login = if account.state == "success" {
                account.login.clone()
            } else {
                None
            };
            Some(HostAuth { host, login })
        })
        .collect())
}

#[derive(Deserialize)]
struct Response<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<RawError>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    message: String,
}

fn parse_response<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T> {
    let response: Response<T> = serde_json::from_str(json).context("parsing GitHub response")?;
    if let Some(error) = response.errors.first() {
        return Err(anyhow!("{}", error.message));
    }
    response.data.context("GitHub response had no data")
}

/// Like `parse_response`, but keeps whatever data came back alongside errors, as GitHub returns
/// for one unreadable node. Only for reads: a mutation with any error has failed.
fn parse_partial_response<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T> {
    let response: Response<serde_json::Value> =
        serde_json::from_str(json).context("parsing GitHub response")?;
    let first_error = || {
        response
            .errors
            .first()
            .map(|error| anyhow!("{}", error.message))
    };
    let Some(data) = response.data.clone().filter(|data| !data.is_null()) else {
        return Err(first_error().unwrap_or_else(|| anyhow!("GitHub response had no data")));
    };
    match serde_json::from_value(data) {
        Ok(parsed) => {
            for error in &response.errors {
                log::warn!("GitHub returned a partial result: {}", error.message);
            }
            Ok(parsed)
        }
        Err(error) => Err(first_error()
            .unwrap_or_else(|| anyhow::Error::new(error).context("parsing GitHub response"))),
    }
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<Option<T>>,
}

#[derive(Deserialize)]
struct RawLogin {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPullRequest {
    id: String,
    number: u64,
    title: String,
    url: String,
    author: Option<RawLogin>,
    head_ref_oid: String,
    base_ref_name: String,
    repository: RawRepository,
    commits: Nodes<RawCommitNode>,
    #[serde(rename = "lastCommit")]
    last_commit: Nodes<RawLastCommit>,
    review_threads: Nodes<RawThread>,
    reviews: Nodes<RawReview>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    files: Option<Nodes<RawFile>>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    head_ref_name: Option<String>,
    #[serde(default)]
    head_repository: Option<RawRepository>,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    mergeable: Option<String>,
    #[serde(default)]
    merge_state_status: Option<String>,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    comments: Option<RawCount>,
    #[serde(default)]
    latest_reviews: Option<Nodes<RawReviewAuthor>>,
    #[serde(default)]
    review_requests: Option<Nodes<RawReviewRequest>>,
}

#[derive(Deserialize)]
struct RawReviewAuthor {
    author: Option<RawLogin>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReviewRequest {
    requested_reviewer: Option<RawTeamOrUser>,
}

/// A team asked to review has no login.
#[derive(Deserialize)]
struct RawTeamOrUser {
    #[serde(default)]
    login: Option<String>,
}

fn logins<T>(nodes: Option<Nodes<T>>, login: impl Fn(T) -> Option<String>) -> Vec<String> {
    nodes
        .map(|nodes| {
            nodes
                .nodes
                .into_iter()
                .flatten()
                .filter_map(login)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCount {
    total_count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct RawCommitNode {
    commit: RawCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
    oid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLastCommit {
    commit: RawLastCommitRollup,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLastCommitRollup {
    status_check_rollup: Option<RawRollup>,
}

#[derive(Deserialize)]
struct RawRollup {
    state: String,
    #[serde(default)]
    contexts: Option<Nodes<RawCheckContext>>,
}

/// A CheckRun (`name`, `status`, `conclusion`, `detailsUrl`) or a StatusContext (`context`,
/// `state`, `targetUrl`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCheckContext {
    name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    details_url: Option<String>,
    context: Option<String>,
    state: Option<String>,
    target_url: Option<String>,
}

impl RawCheckContext {
    fn to_check(&self) -> Option<Check> {
        if let Some(context) = &self.context {
            let outcome = match self.state.as_deref() {
                Some("SUCCESS") => CheckOutcome::Passed,
                Some("PENDING" | "EXPECTED") => CheckOutcome::Running,
                _ => CheckOutcome::Failed,
            };
            return Some(Check {
                name: context.clone(),
                outcome,
                url: self.target_url.clone(),
            });
        }
        let outcome = match (self.status.as_deref(), self.conclusion.as_deref()) {
            (Some(status), _) if status != "COMPLETED" => CheckOutcome::Running,
            (_, Some("SUCCESS" | "NEUTRAL" | "SKIPPED")) => CheckOutcome::Passed,
            _ => CheckOutcome::Failed,
        };
        Some(Check {
            name: self.name.clone()?,
            outcome,
            url: self.details_url.clone(),
        })
    }
}

#[derive(Deserialize)]
struct RawReview {
    commit: Option<RawOid>,
}

#[derive(Deserialize)]
struct RawOid {
    oid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    #[serde(default)]
    id: String,
    is_resolved: bool,
    #[serde(default)]
    is_outdated: bool,
    #[serde(default)]
    path: String,
    line: Option<u32>,
    start_line: Option<u32>,
    diff_side: Option<String>,
    comments: Option<Nodes<RawComment>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    database_id: Option<u64>,
    author: Option<RawLogin>,
    body: String,
    created_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFile {
    path: String,
    viewer_viewed_state: String,
}

// GitHub reports deleted accounts as a null author.
const GHOST_LOGIN: &str = "ghost";

fn author_login(author: Option<&RawLogin>) -> String {
    author.map_or_else(|| GHOST_LOGIN.to_string(), |author| author.login.clone())
}

fn summarize(host: &str, key: Option<&PrKey>, raw: &RawPullRequest) -> Result<PrSummary> {
    let key = match key {
        Some(key) => key.clone(),
        None => {
            let (owner, repo) = raw
                .repository
                .name_with_owner
                .split_once('/')
                .with_context(|| format!("bad repository {}", raw.repository.name_with_owner))?;
            PrKey {
                host: host.to_string(),
                owner: owner.to_string(),
                repo: repo.to_string(),
                number: raw.number,
            }
        }
    };
    let commits: Vec<&RawCommit> = raw
        .commits
        .nodes
        .iter()
        .flatten()
        .map(|node| &node.commit)
        .collect();
    let checks = match raw
        .last_commit
        .nodes
        .iter()
        .flatten()
        .next_back()
        .and_then(|node| node.commit.status_check_rollup.as_ref())
        .map(|rollup| rollup.state.as_str())
    {
        Some("SUCCESS") => ChecksState::Passing,
        Some("FAILURE" | "ERROR") => ChecksState::Failing,
        Some("PENDING" | "EXPECTED") => ChecksState::Pending,
        _ => ChecksState::Unknown,
    };
    let last_review_commit = raw
        .reviews
        .nodes
        .iter()
        .flatten()
        .next_back()
        .and_then(|review| review.commit.as_ref())
        .map(|commit| commit.oid.clone());
    let since_review = match &last_review_commit {
        None => SinceReview::NotReviewed,
        Some(oid) if *oid == raw.head_ref_oid => SinceReview::UpToDate,
        Some(oid) => match commits.iter().position(|commit| commit.oid == *oid) {
            Some(index) => SinceReview::NewCommits(commits.len() - 1 - index),
            None => SinceReview::ForcePushed,
        },
    };
    Ok(PrSummary {
        key,
        node_id: raw.id.clone(),
        title: raw.title.clone(),
        author: author_login(raw.author.as_ref()),
        url: raw.url.clone(),
        head_sha: raw.head_ref_oid.clone(),
        base_ref: raw.base_ref_name.clone(),
        checks,
        unresolved_threads: raw
            .review_threads
            .nodes
            .iter()
            .flatten()
            .filter(|thread| !thread.is_resolved)
            .count(),
        last_review_commit,
        since_review,
    })
}

const PR_SUMMARY_FIELDS: &str = "id number title url author { login } headRefOid baseRefName \
    repository { nameWithOwner } \
    commits(last: 100) { nodes { commit { oid } } } \
    lastCommit: commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } \
    reviews(last: 1, author: $login, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) \
    { nodes { commit { oid } } }";

/// `repos` are `owner/repo` names; the search covers only those repositories.
pub fn inbox_request(
    host: &str,
    login: &str,
    repos: &[String],
    sections: &[InboxSection],
    tracked_node_ids: &[String],
) -> GhRequest {
    let repos = repos
        .iter()
        .map(|repo| format!("repo:{repo}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut variables = serde_json::json!({ "login": login, "tracked": tracked_node_ids });
    let mut parameters = String::new();
    let mut searches = String::new();
    // Queries come from settings, so they travel as variables rather than inside the query text.
    for (index, section) in sections.iter().enumerate() {
        parameters.push_str(&format!(", $s{index}: String!"));
        searches.push_str(&format!(
            "s{index}: search(query: $s{index}, type: ISSUE, first: 50) \
             {{ nodes {{ ... on PullRequest {{ {PR_SUMMARY_FIELDS} \
             reviewThreads(first: 100) {{ nodes {{ isResolved }} }} }} }} }} "
        ));
        variables[format!("s{index}")] = serde_json::Value::String(
            format!(
                "is:pr is:open archived:false {repos} {}",
                section.query.trim()
            )
            .trim_end()
            .to_string(),
        );
    }
    let query = format!(
        "query($login: String!, $tracked: [ID!]!{parameters}) {{ {searches}\
         nodes(ids: $tracked) {{ ... on PullRequest {{ id state }} }} }}"
    );
    graphql_request(host, &query, variables)
}

#[derive(Deserialize)]
struct RawInbox {
    #[serde(default)]
    nodes: Vec<Option<RawTracked>>,
    #[serde(flatten)]
    searches: HashMap<String, Option<Nodes<RawPullRequest>>>,
}

#[derive(Deserialize)]
struct RawTracked {
    id: Option<String>,
    state: Option<String>,
}

/// `sections` and `tracked_node_ids` are what `inbox_request` asked about, in order.
pub fn parse_inbox(
    host: &str,
    json: &str,
    sections: &[InboxSection],
    tracked_node_ids: &[String],
) -> Result<Inbox> {
    let mut raw: RawInbox = parse_partial_response(json)?;
    let mut seen = HashSet::new();
    let mut rows_by_section = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        let nodes = raw.searches.remove(&format!("s{index}")).flatten();
        let mut rows = Vec::new();
        for pull_request in nodes.iter().flat_map(|nodes| nodes.nodes.iter().flatten()) {
            if seen.insert(pull_request.id.clone()) {
                rows.push(summarize(host, None, pull_request)?);
            }
        }
        rows_by_section.push((section.name.clone(), rows));
    }
    let sections = rows_by_section;
    // `nodes(ids:)` answers in request order, with null for an id it can't resolve.
    let gone_tracked = if raw.nodes.len() == tracked_node_ids.len() {
        raw.nodes
            .iter()
            .zip(tracked_node_ids)
            .filter(|(node, _)| node.is_none())
            .map(|(_, id)| id.clone())
            .collect()
    } else {
        Vec::new()
    };
    let closed_tracked = raw
        .nodes
        .into_iter()
        .flatten()
        .filter(|node| matches!(node.state.as_deref(), Some("MERGED" | "CLOSED")))
        .filter_map(|node| node.id)
        .collect();
    Ok(Inbox {
        sections,
        closed_tracked,
        gone_tracked,
    })
}

pub fn detail_request(key: &PrKey, login: &str) -> GhRequest {
    let query = format!(
        "query($owner: String!, $name: String!, $number: Int!, $login: String!) {{ \
         repository(owner: $owner, name: $name) {{ \
         mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed viewerDefaultMergeMethod \
         pullRequest(number: $number) {{ \
         {PR_SUMMARY_FIELDS} state additions deletions body headRefName \
         headRepository {{ nameWithOwner }} \
         isDraft mergeable mergeStateStatus reviewDecision comments {{ totalCount }} \
         latestReviews(first: 50) {{ nodes {{ author {{ login }} }} }} \
         reviewRequests(first: 50) {{ nodes {{ requestedReviewer {{ ... on User {{ login }} }} }} }} \
         lastCommit: commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ \
         contexts(first: 50) {{ nodes {{ \
         ... on CheckRun {{ name status conclusion detailsUrl }} \
         ... on StatusContext {{ context state targetUrl }} }} }} }} }} }} }} \
         files(first: 100) {{ nodes {{ path viewerViewedState }} }} \
         reviewThreads(first: 100) {{ nodes {{ id isResolved isOutdated path line startLine diffSide \
         comments(first: 100) {{ nodes {{ databaseId author {{ login }} body createdAt }} }} }} }} \
         }} }} }}"
    );
    graphql_request(
        &key.host,
        &query,
        serde_json::json!({
            "owner": key.owner,
            "name": key.repo,
            "number": key.number,
            "login": login,
        }),
    )
}

#[derive(Deserialize)]
struct RawDetail {
    repository: Option<RawDetailRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDetailRepository {
    pull_request: Option<RawPullRequest>,
    #[serde(default)]
    merge_commit_allowed: bool,
    #[serde(default)]
    squash_merge_allowed: bool,
    #[serde(default)]
    rebase_merge_allowed: bool,
    #[serde(default)]
    viewer_default_merge_method: Option<String>,
}

pub fn parse_detail(key: &PrKey, json: &str) -> Result<PrDetail> {
    let raw: RawDetail = parse_response(json)?;
    let mut repository = raw.repository.context("pull request not found")?;
    let pull_request = repository
        .pull_request
        .take()
        .context("pull request not found")?;
    let summary = summarize(&key.host, Some(key), &pull_request)?;
    let state = match pull_request.state.as_deref() {
        Some("MERGED") => PrState::Merged,
        Some("CLOSED") => PrState::Closed,
        _ => PrState::Open,
    };
    let merge = merge_status(&repository, &pull_request);
    let files = pull_request
        .files
        .map(|files| {
            files
                .nodes
                .into_iter()
                .flatten()
                .map(|file| PrFile {
                    path: file.path,
                    viewed: file.viewer_viewed_state == "VIEWED",
                    changed_since_viewed: file.viewer_viewed_state == "DISMISSED",
                })
                .collect()
        })
        .unwrap_or_default();
    let threads = pull_request
        .review_threads
        .nodes
        .into_iter()
        .flatten()
        .map(|thread| ReviewThread {
            id: thread.id,
            path: thread.path,
            line: thread.line,
            start_line: thread.start_line,
            side: if thread.diff_side.as_deref() == Some("LEFT") {
                Side::Left
            } else {
                Side::Right
            },
            is_resolved: thread.is_resolved,
            is_outdated: thread.is_outdated,
            comments: thread
                .comments
                .map(|comments| {
                    comments
                        .nodes
                        .into_iter()
                        .flatten()
                        .filter_map(|comment| {
                            Some(ThreadComment {
                                database_id: comment.database_id?,
                                author: author_login(comment.author.as_ref()),
                                body: comment.body,
                                created_at: comment.created_at,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    Ok(PrDetail {
        summary,
        state,
        additions: pull_request.additions,
        deletions: pull_request.deletions,
        files,
        threads,
        body: pull_request.body.unwrap_or_default(),
        head_ref: pull_request.head_ref_name.unwrap_or_default(),
        head_repository: pull_request
            .head_repository
            .map(|repository| repository.name_with_owner),
        merge,
        comment_count: pull_request
            .comments
            .map_or(0, |comments| comments.total_count),
        reviewers: logins(pull_request.latest_reviews, |review| {
            review.author.map(|author| author.login)
        }),
        requested_reviewers: logins(pull_request.review_requests, |request| {
            request.requested_reviewer?.login
        }),
    })
}

fn merge_status(repository: &RawDetailRepository, pull_request: &RawPullRequest) -> MergeStatus {
    let rollup = pull_request
        .last_commit
        .nodes
        .iter()
        .flatten()
        .next_back()
        .and_then(|node| node.commit.status_check_rollup.as_ref());
    let mut methods: Vec<MergeMethod> = [
        (MergeMethod::Merge, repository.merge_commit_allowed),
        (MergeMethod::Squash, repository.squash_merge_allowed),
        (MergeMethod::Rebase, repository.rebase_merge_allowed),
    ]
    .into_iter()
    .filter_map(|(method, allowed)| allowed.then_some(method))
    .collect();
    let default = repository.viewer_default_merge_method.as_deref();
    if let Some(index) = methods
        .iter()
        .position(|method| Some(method.as_str()) == default)
    {
        let method = methods.remove(index);
        methods.insert(0, method);
    }
    MergeStatus {
        is_draft: pull_request.is_draft,
        mergeable: pull_request.mergeable.clone().unwrap_or_default(),
        merge_state: pull_request.merge_state_status.clone().unwrap_or_default(),
        review_decision: pull_request.review_decision.clone(),
        checks_state: rollup.map(|rollup| rollup.state.clone()),
        checks: rollup
            .and_then(|rollup| rollup.contexts.as_ref())
            .map(|contexts| {
                contexts
                    .nodes
                    .iter()
                    .flatten()
                    .filter_map(RawCheckContext::to_check)
                    .collect()
            })
            .unwrap_or_default(),
        methods,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Left => "LEFT",
        Side::Right => "RIGHT",
    }
}

/// Whether Submit sends this draft. Outdated drafts stay local until the user re-places them or
/// posts them as general comments; a draft not yet re-anchored to this head would land on the
/// wrong lines, so it waits too. A draft that may already be on GitHub waits for the user to
/// include it again.
pub fn is_sendable(draft: &Draft, head_sha: &str) -> bool {
    !draft.body.trim().is_empty()
        && draft.post_state == crate::PostState::Unposted
        && (draft.general || (!draft.outdated && draft.commit == head_sha))
}

pub fn review_request(
    key: &PrKey,
    head_sha: &str,
    event: ReviewEvent,
    summary: &str,
    checklist: Option<&str>,
    drafts: &[Draft],
    commentable: &HashMap<String, Vec<RangeInclusive<u32>>>,
    language_for_path: &dyn Fn(&str) -> Option<String>,
) -> GhRequest {
    let mut comments = Vec::new();
    let mut body_drafts = Vec::new();
    for draft in drafts.iter().filter(|draft| is_sendable(draft, head_sha)) {
        // The quote links to the commit the lines were read at, which is the head unless the
        // draft is an outdated one posted as a general comment.
        let quoted = || {
            quoted_body(
                draft,
                key,
                &draft.commit,
                language_for_path(&draft.path).as_deref(),
            )
        };
        match place(draft, commentable) {
            Placement::Line {
                path,
                side,
                start_line,
                line,
            } => {
                let mut comment = serde_json::json!({
                    "path": path,
                    "body": draft.body,
                    "line": line,
                    "side": side_name(side),
                });
                if let Some(start_line) = start_line {
                    comment["start_line"] = start_line.into();
                    comment["start_side"] = side_name(side).into();
                }
                comments.push(comment);
            }
            Placement::NearestLine { path, line } => comments.push(serde_json::json!({
                "path": path,
                "line": line,
                "side": "RIGHT",
                "body": quoted(),
            })),
            Placement::ReviewBody { .. } => body_drafts.push((draft, quoted())),
        }
    }
    // The checklist closes the review, after any comments that couldn't sit on a line.
    let mut body = review_body(summary, &body_drafts);
    if let Some(checklist) = checklist {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(checklist);
    }
    rest_request(
        &key.host,
        "POST",
        &format!(
            "repos/{}/{}/pulls/{}/reviews",
            key.owner, key.repo, key.number
        ),
        Some(serde_json::json!({
            "commit_id": head_sha,
            "event": event.as_str(),
            "body": body,
            "comments": comments,
        })),
    )
}

pub fn reply_request(key: &PrKey, comment_database_id: u64, body: &str) -> GhRequest {
    rest_request(
        &key.host,
        "POST",
        &format!(
            "repos/{}/{}/pulls/{}/comments/{comment_database_id}/replies",
            key.owner, key.repo, key.number
        ),
        Some(serde_json::json!({ "body": body })),
    )
}

pub fn resolve_request(host: &str, thread_id: &str, resolved: bool) -> GhRequest {
    let mutation = if resolved {
        "resolveReviewThread"
    } else {
        "unresolveReviewThread"
    };
    graphql_request(
        host,
        &format!(
            "mutation($threadId: ID!) {{ {mutation}(input: {{threadId: $threadId}}) \
             {{ thread {{ isResolved }} }} }}"
        ),
        serde_json::json!({ "threadId": thread_id }),
    )
}

pub fn viewed_request(host: &str, pr_node_id: &str, path: &str, viewed: bool) -> GhRequest {
    let mutation = if viewed {
        "markFileAsViewed"
    } else {
        "unmarkFileAsViewed"
    };
    graphql_request(
        host,
        &format!(
            "mutation($pullRequestId: ID!, $path: String!) {{ \
             {mutation}(input: {{pullRequestId: $pullRequestId, path: $path}}) \
             {{ clientMutationId }} }}"
        ),
        serde_json::json!({ "pullRequestId": pr_node_id, "path": path }),
    )
}

pub fn parse_mutation(json: &str) -> Result<()> {
    parse_response::<serde_json::Value>(json).map(|_| ())
}

#[derive(Deserialize)]
struct RawRestComment {
    id: u64,
    user: Option<RawLogin>,
    body: String,
    created_at: String,
}

pub fn parse_reply(json: &str) -> Result<ThreadComment> {
    let comment: RawRestComment = serde_json::from_str(json).context("parsing GitHub reply")?;
    Ok(ThreadComment {
        database_id: comment.id,
        author: author_login(comment.user.as_ref()),
        body: comment.body,
        created_at: comment.created_at,
    })
}

/// Merges only if the head is still the one the user reviewed.
pub fn merge_request(
    host: &str,
    pr_node_id: &str,
    method: MergeMethod,
    expected_head: &str,
) -> GhRequest {
    graphql_request(
        host,
        "mutation($pullRequestId: ID!, $mergeMethod: PullRequestMergeMethod!, \
         $expectedHeadOid: GitObjectID!) { mergePullRequest(input: {pullRequestId: \
         $pullRequestId, mergeMethod: $mergeMethod, expectedHeadOid: $expectedHeadOid}) \
         { pullRequest { state } } }",
        serde_json::json!({
            "pullRequestId": pr_node_id,
            "mergeMethod": method.as_str(),
            "expectedHeadOid": expected_head,
        }),
    )
}

pub const HEAD_MOVED_MESSAGE: &str =
    "The pull request changed since you loaded it. Refresh to review the new commits.";

/// GitHub's refusal, in plain words where GitHub's own are cryptic.
pub fn merge_error_message(message: &str) -> String {
    if message.to_lowercase().contains("head branch was modified") {
        HEAD_MOVED_MESSAGE.to_string()
    } else {
        format!("Couldn't merge: {message}")
    }
}

pub fn issue_comment_request(key: &PrKey, body: &str) -> GhRequest {
    rest_request(
        &key.host,
        "POST",
        &format!(
            "repos/{}/{}/issues/{}/comments",
            key.owner, key.repo, key.number
        ),
        Some(serde_json::json!({ "body": body })),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineEvent {
    Comment(ThreadComment),
    Review {
        author: String,
        verb: &'static str,
        body: String,
        submitted_at: String,
    },
    Commit {
        short_sha: String,
        headline: String,
    },
}

pub fn timeline_request(key: &PrKey) -> GhRequest {
    // ponytail: the last 100 events only; page with `before` if long conversations need more.
    graphql_request(
        &key.host,
        "query($owner: String!, $name: String!, $number: Int!) { \
         repository(owner: $owner, name: $name) { pullRequest(number: $number) { \
         timelineItems(last: 100, itemTypes: [ISSUE_COMMENT, PULL_REQUEST_REVIEW, \
         PULL_REQUEST_COMMIT]) { nodes { __typename \
         ... on IssueComment { databaseId author { login } body createdAt } \
         ... on PullRequestReview { author { login } state body submittedAt } \
         ... on PullRequestCommit { commit { abbreviatedOid messageHeadline } } } } } } }",
        serde_json::json!({ "owner": key.owner, "name": key.repo, "number": key.number }),
    )
}

#[derive(Deserialize)]
struct RawTimeline {
    repository: Option<RawTimelineRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTimelineRepository {
    pull_request: Option<RawTimelinePullRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTimelinePullRequest {
    timeline_items: Nodes<RawTimelineItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTimelineItem {
    #[serde(rename = "__typename")]
    typename: String,
    database_id: Option<u64>,
    author: Option<RawLogin>,
    body: Option<String>,
    created_at: Option<String>,
    state: Option<String>,
    submitted_at: Option<String>,
    commit: Option<RawTimelineCommit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTimelineCommit {
    abbreviated_oid: String,
    message_headline: String,
}

pub fn parse_timeline(json: &str) -> Result<Vec<TimelineEvent>> {
    let raw: RawTimeline = parse_partial_response(json)?;
    let items = raw
        .repository
        .and_then(|repository| repository.pull_request)
        .context("pull request not found")?
        .timeline_items
        .nodes;
    Ok(items
        .into_iter()
        .flatten()
        .filter_map(|item| match item.typename.as_str() {
            "IssueComment" => Some(TimelineEvent::Comment(ThreadComment {
                database_id: item.database_id?,
                author: author_login(item.author.as_ref()),
                body: item.body.unwrap_or_default(),
                created_at: item.created_at.unwrap_or_default(),
            })),
            "PullRequestReview" => {
                let verb = match item.state.as_deref() {
                    Some("APPROVED") => "approved",
                    Some("CHANGES_REQUESTED") => "requested changes",
                    Some("COMMENTED") => "commented",
                    Some("DISMISSED") => "reviewed (dismissed)",
                    // Pending reviews are the viewer's unsent drafts on github.com.
                    _ => return None,
                };
                Some(TimelineEvent::Review {
                    author: author_login(item.author.as_ref()),
                    verb,
                    body: item.body.unwrap_or_default(),
                    submitted_at: item.submitted_at.unwrap_or_default(),
                })
            }
            "PullRequestCommit" => item.commit.map(|commit| TimelineEvent::Commit {
                short_sha: commit.abbreviated_oid,
                headline: commit.message_headline,
            }),
            _ => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INBOX: &str = include_str!("../test_fixtures/inbox.json");
    const DETAIL: &str = include_str!("../test_fixtures/detail.json");

    fn key() -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "web".into(),
            number: 482,
        }
    }

    fn find(inbox: &Inbox, number: u64) -> &PrSummary {
        inbox
            .sections
            .iter()
            .flat_map(|(_, rows)| rows)
            .find(|row| row.key.number == number)
            .expect("row")
    }

    #[test]
    fn auth_status_lists_hosts() {
        let mut hosts =
            parse_auth_status(include_str!("../test_fixtures/auth_status.json")).expect("parse");
        hosts.sort_by(|a, b| a.host.cmp(&b.host));
        assert_eq!(
            hosts,
            [
                HostAuth {
                    host: "ghe.corp".into(),
                    login: None
                },
                HostAuth {
                    host: "github.com".into(),
                    login: Some("alice".into())
                },
            ]
        );
        assert_eq!(
            auth_status_request().args,
            ["auth", "status", "--json", "hosts"]
        );
    }

    #[test]
    fn auth_status_logged_out_is_empty() {
        let hosts = parse_auth_status(include_str!("../test_fixtures/auth_status_logged_out.json"))
            .expect("parse");
        assert!(hosts.is_empty());
    }

    #[test]
    fn inbox_groups_rows_and_counts() {
        let inbox = parse_inbox("github.com", INBOX, &sections(), &[]).expect("parse");
        let names: Vec<_> = inbox
            .sections
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["Review requested", "Yours", "Mentioned"]);
        let row = find(&inbox, 482);
        assert_eq!(row.title, "Add retry to uploader");
        assert_eq!(row.author, "alice");
        assert_eq!(row.key, key());
        assert_eq!(row.checks, ChecksState::Passing);
        assert_eq!(row.unresolved_threads, 2);
        assert_eq!(row.head_sha, "a1");
        assert_eq!(row.base_ref, "main");
        assert_eq!(find(&inbox, 483).checks, ChecksState::Pending);
        assert_eq!(find(&inbox, 484).checks, ChecksState::Failing);
        assert_eq!(find(&inbox, 485).checks, ChecksState::Unknown);
        assert_eq!(find(&inbox, 485).key.repo, "api");
    }

    #[test]
    fn inbox_dedupes_across_groups() {
        let inbox = parse_inbox("github.com", INBOX, &sections(), &[]).expect("parse");
        let numbers = |index: usize| -> Vec<u64> {
            inbox.sections[index]
                .1
                .iter()
                .map(|row| row.key.number)
                .collect()
        };
        assert_eq!(numbers(0), [482, 483, 484, 485]);
        assert_eq!(numbers(1), [490]);
        assert_eq!(numbers(2), [500]);
    }

    #[test]
    fn since_review_counts() {
        let inbox = parse_inbox("github.com", INBOX, &sections(), &[]).expect("parse");
        assert_eq!(find(&inbox, 482).since_review, SinceReview::NotReviewed);
        assert_eq!(find(&inbox, 483).since_review, SinceReview::UpToDate);
        assert_eq!(find(&inbox, 484).since_review, SinceReview::NewCommits(3));
        assert_eq!(find(&inbox, 484).last_review_commit.as_deref(), Some("c2"));
        assert_eq!(find(&inbox, 485).since_review, SinceReview::ForcePushed);
    }

    #[test]
    fn inbox_reports_closed_tracked() {
        let inbox = parse_inbox("github.com", INBOX, &sections(), &[]).expect("parse");
        assert_eq!(inbox.closed_tracked, ["PR_x"]);
    }

    #[test]
    fn detail_threads_files_and_viewed() {
        let detail = parse_detail(&key(), DETAIL).expect("parse");
        assert_eq!(detail.state, PrState::Open);
        assert_eq!((detail.additions, detail.deletions), (120, 14));
        assert!(detail.body.ends_with("- [ ] Tested on staging"));
        assert_eq!(detail.head_ref, "retry-uploads");
        assert_eq!(detail.head_repository.as_deref(), Some("maya/web"));
        assert_eq!(detail.summary.since_review, SinceReview::NewCommits(1));
        assert_eq!(detail.summary.unresolved_threads, 1);
        assert_eq!(
            detail.files,
            [
                PrFile {
                    path: "src/upload.rs".into(),
                    viewed: true,
                    changed_since_viewed: false,
                },
                PrFile {
                    path: "src/retry.rs".into(),
                    viewed: false,
                    changed_since_viewed: false,
                },
            ]
        );
        let dismissed = DETAIL.replacen("\"VIEWED\"", "\"DISMISSED\"", 1);
        let file = &parse_detail(&key(), &dismissed).expect("parse").files[0];
        assert!(file.changed_since_viewed && !file.viewed);
        let thread = &detail.threads[0];
        assert_eq!(thread.id, "PRRT_1");
        assert_eq!((thread.line, thread.start_line), (Some(42), Some(40)));
        assert_eq!(thread.side, Side::Right);
        assert!(!thread.is_resolved && !thread.is_outdated);
        // The fixture's third comment has a null databaseId and is skipped.
        assert_eq!(thread.comments.len(), 2);
        assert_eq!(thread.comments[0].database_id, 1001);
        assert_eq!(thread.comments[0].author, "bob");
        assert_eq!(thread.comments[1].author, "ghost");
        let outdated = &detail.threads[1];
        assert_eq!(outdated.line, None);
        assert_eq!(outdated.side, Side::Left);
        assert!(outdated.is_resolved && outdated.is_outdated);
    }

    #[test]
    fn inbox_partial_errors_keep_the_data() {
        let mut partial: serde_json::Value = serde_json::from_str(INBOX).expect("fixture");
        partial["data"]["s2"] = serde_json::Value::Null;
        partial["errors"] = serde_json::json!([{ "message": "Could not resolve PR_gone" }]);
        let tracked = ["PR_x".to_string(), "PR_y".into(), "PR_gone".into()];
        let inbox = parse_inbox("github.com", &partial.to_string(), &sections(), &tracked)
            .expect("partial");
        assert!(!inbox.sections[0].1.is_empty());
        assert!(inbox.sections[2].1.is_empty());
        assert_eq!(inbox.closed_tracked, ["PR_x"]);
        assert_eq!(inbox.gone_tracked, ["PR_gone"]);
        let failed_mutation =
            r#"{"data":{"addPullRequestReview":null},"errors":[{"message":"no"}]}"#;
        assert!(parse_mutation(failed_mutation).is_err());
    }

    #[test]
    fn graphql_errors_are_errors() {
        let json = include_str!("../test_fixtures/graphql_error.json");
        let message = parse_inbox("github.com", json, &sections(), &[])
            .expect_err("error")
            .to_string();
        assert!(
            message.contains("Could not resolve to a Repository"),
            "{message}"
        );
        assert!(parse_detail(&key(), json).is_err());
        let partial = r#"{"data":{"repository":null},"errors":[{"message":"boom"}]}"#;
        assert_eq!(
            parse_detail(&key(), partial)
                .expect_err("error")
                .to_string(),
            "boom"
        );
    }

    #[test]
    fn inbox_request_shape() {
        let tracked = vec!["PR_x".to_string()];
        let repos = vec!["acme/app".to_string(), "acme/web".to_string()];
        let mut sections = sections();
        sections.push(InboxSection {
            name: "Other open".into(),
            query: "".into(),
        });
        let request = inbox_request("ghe.corp", "alice", &repos, &sections, &tracked);
        let expected = graphql_request("ghe.corp", "", serde_json::json!({}));
        assert_eq!(request.args, expected.args);
        let body: serde_json::Value =
            serde_json::from_str(&request.stdin.expect("stdin")).expect("json");
        let scope = "is:pr is:open archived:false repo:acme/app repo:acme/web";
        assert_eq!(
            body["variables"],
            serde_json::json!({
                "login": "alice",
                "tracked": ["PR_x"],
                "s0": format!("{scope} review-requested:@me"),
                "s1": format!("{scope} author:@me"),
                "s2": format!("{scope} mentions:@me"),
                "s3": scope,
            })
        );
        let query = body["query"].as_str().expect("query");
        assert!(query.contains("states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]"));
        assert!(query.contains("lastCommit: commits(last: 1)"));
        assert!(query.contains("$s3: String!"));
        assert!(query.contains("s3: search(query: $s3, type: ISSUE, first: 50)"));
    }

    #[test]
    fn section_query_quotes_stay_out_of_the_query_text() {
        let sections = [InboxSection {
            name: "Tricky".into(),
            query: r#"label:"needs review" } evil"#.into(),
        }];
        let request = inbox_request("github.com", "alice", &["a/b".into()], &sections, &[]);
        let body: serde_json::Value =
            serde_json::from_str(&request.stdin.expect("stdin")).expect("json");
        assert!(!body["query"].as_str().expect("query").contains("evil"));
        assert!(
            body["variables"]["s0"]
                .as_str()
                .expect("s0")
                .ends_with("evil")
        );
    }

    fn sections() -> Vec<InboxSection> {
        [
            ("Review requested", "review-requested:@me"),
            ("Yours", "author:@me"),
            ("Mentioned", "mentions:@me"),
        ]
        .map(|(name, query)| InboxSection {
            name: name.into(),
            query: query.into(),
        })
        .to_vec()
    }

    fn draft(path: &str, start_line: u32, end_line: u32, body: &str) -> crate::Draft {
        crate::Draft {
            id: None,
            key: key(),
            path: path.into(),
            side: Side::Right,
            start_line,
            end_line,
            commit: "head".into(),
            anchor_text: "    audit(user);".into(),
            body: body.into(),
            outdated: false,
            general: false,
            post_state: crate::PostState::Unposted,
            from_agent: false,
        }
    }

    fn stdin_json(request: &GhRequest) -> serde_json::Value {
        serde_json::from_str(request.stdin.as_deref().expect("stdin")).expect("json")
    }

    #[test]
    fn review_request_shape() {
        let commentable =
            std::collections::HashMap::from_iter([("src/upload.rs".to_string(), vec![40..=43])]);
        let mut left = draft("src/upload.rs", 10, 11, "Why drop this?");
        left.side = Side::Left;
        let mut outdated = draft("src/upload.rs", 41, 41, "Stale");
        outdated.outdated = true;
        let mut general = draft("src/upload.rs", 41, 41, "Still true");
        general.outdated = true;
        general.general = true;
        let drafts = vec![
            draft("src/upload.rs", 42, 42, "Retry forever?"),
            draft("src/upload.rs", 47, 47, "Unchanged here"),
            draft("src/audit.rs", 3, 3, "Audit this"),
            left,
            outdated,
            general,
            draft("src/upload.rs", 41, 41, "  "),
            crate::Draft {
                post_state: crate::PostState::Sending,
                from_agent: false,
                ..draft("src/upload.rs", 41, 41, "Maybe posted")
            },
        ];
        let request = review_request(
            &key(),
            "head",
            ReviewEvent::RequestChanges,
            "Needs work.",
            Some("### Checklist (1/1)\n- [x] Done\n"),
            &drafts,
            &commentable,
            &|path| path.ends_with(".rs").then(|| "rust".to_string()),
        );
        assert_eq!(
            request.args,
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
        let body = stdin_json(&request);
        assert_eq!(body["commit_id"], "head");
        assert_eq!(body["event"], "REQUEST_CHANGES");
        let unchanged = crate::quoted_body(&drafts[1], &key(), "head", Some("rust"));
        assert_eq!(
            body["comments"],
            serde_json::json!([
                {"path": "src/upload.rs", "body": "Retry forever?", "line": 42, "side": "RIGHT"},
                {"path": "src/upload.rs", "line": 43, "side": "RIGHT", "body": unchanged},
                {"path": "src/upload.rs", "body": "Why drop this?", "line": 11, "side": "LEFT",
                 "start_line": 10, "start_side": "LEFT"},
            ])
        );
        let review_body = body["body"].as_str().expect("body");
        assert!(review_body.starts_with("Needs work.\n\n### src/audit.rs\n\n"));
        assert!(review_body.contains("Audit this"));
        assert!(review_body.contains("Still true"));
        assert!(!review_body.contains("Stale"));
        assert!(
            review_body.ends_with("\n\n### Checklist (1/1)\n- [x] Done\n"),
            "{review_body}"
        );
        assert!(review_body.find("Still true") < review_body.find("### Checklist"));
        assert!(!body.to_string().contains("Maybe posted"));
        assert_eq!(ReviewEvent::Comment.as_str(), "COMMENT");
        assert_eq!(ReviewEvent::Approve.as_str(), "APPROVE");
    }

    #[test]
    fn reply_resolve_and_viewed_requests() {
        let reply = reply_request(&key(), 1001, "Agreed.");
        assert_eq!(
            reply.args,
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "repos/acme/web/pulls/482/comments/1001/replies",
                "--input",
                "-"
            ]
        );
        assert_eq!(stdin_json(&reply), serde_json::json!({"body": "Agreed."}));

        for (resolved, mutation) in [
            (true, "resolveReviewThread"),
            (false, "unresolveReviewThread"),
        ] {
            let request = resolve_request("ghe.corp", "PRRT_1", resolved);
            assert_eq!(
                request.args,
                graphql_request("ghe.corp", "", serde_json::json!({})).args
            );
            let body = stdin_json(&request);
            assert_eq!(body["variables"], serde_json::json!({"threadId": "PRRT_1"}));
            let query = body["query"].as_str().expect("query");
            assert!(
                query.contains(&format!("{mutation}(input: {{threadId: $threadId}})")),
                "{query}"
            );
        }

        for (viewed, mutation) in [(true, "markFileAsViewed"), (false, "unmarkFileAsViewed")] {
            let body = stdin_json(&viewed_request(
                "github.com",
                "PR_482",
                "src/a b.rs",
                viewed,
            ));
            assert_eq!(
                body["variables"],
                serde_json::json!({"pullRequestId": "PR_482", "path": "src/a b.rs"})
            );
            let query = body["query"].as_str().expect("query");
            assert!(
                query.contains(&format!(
                    "{mutation}(input: {{pullRequestId: $pullRequestId, path: $path}})"
                )),
                "{query}"
            );
        }
    }

    #[test]
    fn mutation_and_reply_responses() {
        assert!(parse_mutation(r#"{"data":{"resolveReviewThread":{}}}"#).is_ok());
        assert_eq!(
            parse_mutation(r#"{"data":null,"errors":[{"message":"no access"}]}"#)
                .expect_err("error")
                .to_string(),
            "no access"
        );
        let comment = parse_reply(
            r#"{"id":2001,"user":{"login":"carol"},"body":"Agreed.","created_at":"2026-10-02T09:00:00Z"}"#,
        )
        .expect("reply");
        assert_eq!(
            comment,
            ThreadComment {
                database_id: 2001,
                author: "carol".into(),
                body: "Agreed.".into(),
                created_at: "2026-10-02T09:00:00Z".into(),
            }
        );
    }

    #[test]
    fn detail_request_shape() {
        let body: serde_json::Value =
            serde_json::from_str(&detail_request(&key(), "alice").stdin.expect("stdin"))
                .expect("json");
        assert_eq!(
            body["variables"],
            serde_json::json!({"owner": "acme", "name": "web", "number": 482, "login": "alice"})
        );
        let query = body["query"].as_str().expect("query");
        for field in [
            "viewerDefaultMergeMethod",
            "isDraft mergeable mergeStateStatus reviewDecision comments { totalCount }",
            "... on CheckRun { name status conclusion detailsUrl }",
            "latestReviews(first: 50) { nodes { author { login } } }",
            "requestedReviewer { ... on User { login } }",
        ] {
            assert!(query.contains(field), "{field} in {query}");
        }
        assert_eq!(query.matches('{').count(), query.matches('}').count());
    }

    fn detail_with(change: impl FnOnce(&mut serde_json::Value)) -> PrDetail {
        let mut json: serde_json::Value = serde_json::from_str(DETAIL).expect("fixture");
        change(&mut json["data"]["repository"]["pullRequest"]);
        parse_detail(&key(), &json.to_string()).expect("parse")
    }

    #[test]
    fn detail_reads_reviewers_and_requested_users() {
        let detail = detail_with(|pull_request| {
            pull_request["latestReviews"] =
                serde_json::json!({"nodes": [{"author": {"login": "maya"}}, {"author": null}]});
            pull_request["reviewRequests"] = serde_json::json!({"nodes": [
                {"requestedReviewer": {"login": "marcus"}},
                {"requestedReviewer": {}},
            ]});
        });
        assert_eq!(detail.reviewers, ["maya"]);
        assert_eq!(detail.requested_reviewers, ["marcus"]);
        assert!(
            parse_detail(&key(), DETAIL)
                .expect("parse")
                .reviewers
                .is_empty()
        );
    }

    #[test]
    fn detail_reads_merge_status_checks_and_comment_count() {
        let detail = parse_detail(&key(), DETAIL).expect("parse");
        assert_eq!(detail.comment_count, 2);
        let merge = &detail.merge;
        assert!(!merge.is_draft);
        assert_eq!(merge.mergeable, "MERGEABLE");
        assert_eq!(merge.merge_state, "CLEAN");
        assert_eq!(merge.review_decision.as_deref(), Some("APPROVED"));
        assert_eq!(merge.checks_state.as_deref(), Some("SUCCESS"));
        // The default method comes first; rebasing isn't allowed.
        assert_eq!(merge.methods, [MergeMethod::Squash, MergeMethod::Merge]);
        assert_eq!(
            merge.checks,
            [
                Check {
                    name: "build".into(),
                    outcome: CheckOutcome::Passed,
                    url: Some("https://github.com/acme/web/runs/1".into()),
                },
                Check {
                    name: "lint".into(),
                    outcome: CheckOutcome::Running,
                    url: Some("https://github.com/acme/web/runs/2".into()),
                },
                Check {
                    name: "ci/deploy".into(),
                    outcome: CheckOutcome::Failed,
                    url: None,
                },
            ]
        );
        assert_eq!(detail.readiness(), Readiness::Ready);
    }

    #[test]
    fn readiness_says_the_one_thing_that_matters_most() {
        let cases: Vec<(serde_json::Value, Readiness)> = vec![
            (
                serde_json::json!({"state": "MERGED", "isDraft": true}),
                Readiness::Merged,
            ),
            (serde_json::json!({"state": "CLOSED"}), Readiness::Closed),
            (
                serde_json::json!({"isDraft": true, "mergeable": "CONFLICTING"}),
                Readiness::Draft,
            ),
            (
                serde_json::json!({"mergeable": "CONFLICTING"}),
                Readiness::Conflicts,
            ),
            (
                serde_json::json!({"mergeStateStatus": "DIRTY"}),
                Readiness::Conflicts,
            ),
            (
                serde_json::json!({"rollup": "FAILURE", "reviewDecision": "CHANGES_REQUESTED"}),
                Readiness::ChecksFailing,
            ),
            (
                serde_json::json!({"rollup": "ERROR"}),
                Readiness::ChecksFailing,
            ),
            (
                serde_json::json!({"reviewDecision": "CHANGES_REQUESTED"}),
                Readiness::ChangesRequested,
            ),
            (
                serde_json::json!({"reviewDecision": "REVIEW_REQUIRED", "rollup": "PENDING"}),
                Readiness::ReviewRequired,
            ),
            (
                serde_json::json!({"rollup": "EXPECTED"}),
                Readiness::ChecksRunning,
            ),
            (
                serde_json::json!({"mergeable": "UNKNOWN", "mergeStateStatus": "UNKNOWN"}),
                Readiness::Checking,
            ),
            (
                serde_json::json!({"mergeStateStatus": "BEHIND"}),
                Readiness::Behind("main".into()),
            ),
            (
                serde_json::json!({"mergeStateStatus": "UNSTABLE"}),
                Readiness::Ready,
            ),
            (
                serde_json::json!({"mergeStateStatus": "HAS_HOOKS"}),
                Readiness::Ready,
            ),
        ];
        for (change, expected) in cases {
            let detail = detail_with(|pull_request| {
                for (field, value) in change.as_object().expect("object") {
                    if field == "rollup" {
                        pull_request["lastCommit"]["nodes"][0]["commit"]["statusCheckRollup"]["state"] =
                            value.clone();
                    } else {
                        pull_request[field] = value.clone();
                    }
                }
            });
            assert_eq!(detail.readiness(), expected, "{change}");
        }
        assert_eq!(Readiness::Behind("main".into()).label(), "Behind main");
    }

    #[test]
    fn merge_request_sends_the_method_and_expected_head() {
        let request = merge_request("github.com", "PR_482", MergeMethod::Rebase, "a2");
        let body = stdin_json(&request);
        assert_eq!(
            body["variables"],
            serde_json::json!({
                "pullRequestId": "PR_482",
                "mergeMethod": "REBASE",
                "expectedHeadOid": "a2",
            })
        );
        assert!(
            body["query"]
                .as_str()
                .expect("query")
                .contains("expectedHeadOid: $expectedHeadOid")
        );
        assert_eq!(
            merge_error_message("Head branch was modified. Review and try the merge again."),
            HEAD_MOVED_MESSAGE
        );
        assert_eq!(
            merge_error_message("Required status check \"ci\" is failing"),
            "Couldn't merge: Required status check \"ci\" is failing"
        );
    }

    #[test]
    fn timeline_lists_comments_reviews_and_commits_in_order() {
        let events = parse_timeline(include_str!("../test_fixtures/timeline.json")).expect("parse");
        assert_eq!(
            events,
            [
                TimelineEvent::Commit {
                    short_sha: "a1b2c3d".into(),
                    headline: "Retry uploads".into(),
                },
                TimelineEvent::Comment(ThreadComment {
                    database_id: 3001,
                    author: "bob".into(),
                    body: "Looks **good** overall.".into(),
                    created_at: "2026-10-02T09:00:00Z".into(),
                }),
                TimelineEvent::Review {
                    author: "carol".into(),
                    verb: "requested changes",
                    body: "Please add a test.".into(),
                    submitted_at: "2026-10-03T09:00:00Z".into(),
                },
                TimelineEvent::Commit {
                    short_sha: "e4f5a6b".into(),
                    headline: "Add retry test".into(),
                },
                TimelineEvent::Review {
                    author: "ghost".into(),
                    verb: "approved",
                    body: String::new(),
                    submitted_at: "2026-10-04T09:00:00Z".into(),
                },
            ]
        );
        assert_eq!(
            stdin_json(&timeline_request(&key()))["variables"],
            serde_json::json!({"owner": "acme", "name": "web", "number": 482})
        );
        let comment = issue_comment_request(&key(), "Thanks!");
        assert_eq!(comment.args[5], "repos/acme/web/issues/482/comments");
        assert_eq!(stdin_json(&comment), serde_json::json!({"body": "Thanks!"}));
    }
}
