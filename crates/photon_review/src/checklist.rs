use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use settings::PrReviewChecklistContent;

use crate::{ChecksState, FileDiff, FileKind, ReviewThread, classify_path};

pub const TEAM_FILE: &str = ".photon/review.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecklistSource {
    Settings,
    Team,
}

impl ChecklistSource {
    fn key(self) -> &'static str {
        match self {
            ChecklistSource::Settings => "settings",
            ChecklistSource::Team => "team",
        }
    }
}

/// A question the reviewer answers by hand.
#[derive(Clone, Debug)]
pub struct ChecklistItem {
    pub key: String,
    pub text: String,
    pub checklist: String,
    /// Set when the checklist is limited to some paths; the item then also shows under the
    /// review-map steps holding a matching file.
    pub paths: Option<GlobSet>,
}

impl ChecklistItem {
    pub fn matches_any(&self, paths: &[String]) -> bool {
        self.paths
            .as_ref()
            .is_some_and(|globs| paths.iter().any(|path| globs.is_match(path)))
    }
}

#[derive(Deserialize)]
struct TeamFile {
    #[serde(default)]
    checklists: Vec<PrReviewChecklistContent>,
}

pub fn parse_team_file(json: &str) -> Result<Vec<PrReviewChecklistContent>> {
    let file: TeamFile = serde_json::from_str(json).context(format!("reading {TEAM_FILE}"))?;
    Ok(file.checklists)
}

/// The repository's own checklists at `base`, so a pull request can't change the checks it is
/// reviewed against. A missing file is the usual case and gives no error.
pub async fn read_team_checklists(
    worktree: &Path,
    base: &str,
) -> (Vec<PrReviewChecklistContent>, Option<String>) {
    let spec = format!("{base}:{TEAM_FILE}");
    match crate::checkout::git_raw(worktree, &["show", &spec]).await {
        Err(_) => (Vec::new(), None),
        Ok(text) => match parse_team_file(&text) {
            Ok(checklists) => (checklists, None),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        },
    }
}

/// The items of every checklist whose `when` rules match this pull request, and a message for
/// each checklist whose globs couldn't be read.
pub fn applicable_items(
    checklists: &[(ChecklistSource, PrReviewChecklistContent)],
    repo: &str,
    changed_paths: &[String],
) -> (Vec<ChecklistItem>, Vec<String>) {
    let mut items: Vec<ChecklistItem> = Vec::new();
    let mut errors = Vec::new();
    for (source, checklist) in checklists {
        let repos = &checklist.when.repos;
        if !repos.is_empty() && !repos.iter().any(|name| name.eq_ignore_ascii_case(repo)) {
            continue;
        }
        let paths = if checklist.when.paths.is_empty() {
            None
        } else {
            match glob_set(&checklist.when.paths) {
                Ok(globs) => Some(globs),
                Err(error) => {
                    errors.push(format!("Checklist \"{}\": {error:#}", checklist.name));
                    continue;
                }
            }
        };
        if let Some(globs) = &paths
            && !changed_paths.iter().any(|path| globs.is_match(path))
        {
            continue;
        }
        for text in &checklist.items {
            let key = format!("{}:{}:{}", source.key(), checklist.name, text);
            if text.trim().is_empty() || items.iter().any(|item| item.key == key) {
                continue;
            }
            items.push(ChecklistItem {
                key,
                text: text.clone(),
                checklist: checklist.name.clone(),
                paths: paths.clone(),
            });
        }
    }
    (items, errors)
}

fn glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).context(format!("bad path pattern {pattern:?}"))?);
    }
    Ok(builder.build()?)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorBox {
    pub text: String,
    pub checked: bool,
}

/// `- [ ]` and `- [x]` lines (or `*`) in the pull request description.
pub fn parse_author_boxes(body: &str) -> Vec<AuthorBox> {
    body.lines()
        .filter_map(|line| {
            let rest = line.trim_start();
            let rest = rest
                .strip_prefix("- ")
                .or_else(|| rest.strip_prefix("* "))?;
            let (checked, text) = if let Some(text) = rest.strip_prefix("[ ]") {
                (false, text)
            } else if let Some(text) = rest
                .strip_prefix("[x]")
                .or_else(|| rest.strip_prefix("[X]"))
            {
                (true, text)
            } else {
                return None;
            };
            let text = text.trim();
            (!text.is_empty()).then(|| AuthorBox {
                text: text.to_string(),
                checked,
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutoCheck {
    ChecksPass,
    ThreadsResolved,
    TestsChanged,
    RanLocally,
    NoDebugLeftovers,
}

impl AutoCheck {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "checks_pass" => AutoCheck::ChecksPass,
            "threads_resolved" => AutoCheck::ThreadsResolved,
            "tests_changed" => AutoCheck::TestsChanged,
            "ran_locally" => AutoCheck::RanLocally,
            "no_debug_leftovers" => AutoCheck::NoDebugLeftovers,
            _ => return None,
        })
    }

    pub fn key(self) -> String {
        let name = match self {
            AutoCheck::ChecksPass => "checks_pass",
            AutoCheck::ThreadsResolved => "threads_resolved",
            AutoCheck::TestsChanged => "tests_changed",
            AutoCheck::RanLocally => "ran_locally",
            AutoCheck::NoDebugLeftovers => "no_debug_leftovers",
        };
        format!("auto:{name}")
    }

    pub fn title(self) -> &'static str {
        match self {
            AutoCheck::ChecksPass => "CI checks pass",
            AutoCheck::ThreadsResolved => "Review threads resolved",
            AutoCheck::TestsChanged => "Tests changed with the code",
            AutoCheck::RanLocally => "Tests ran locally",
            AutoCheck::NoDebugLeftovers => "No TODOs or debug prints added",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoResult {
    Passed(String),
    Failed(String),
    Unknown(String),
}

impl AutoResult {
    pub fn passed(&self) -> bool {
        matches!(self, AutoResult::Passed(_))
    }

    /// Whether a reviewer's answer counts. Evidence of a failure overrides a Yes or No given
    /// earlier (say, while CI was pending); only "doesn't apply" still holds.
    pub fn accepts(&self, answer: Answer) -> bool {
        match self {
            AutoResult::Passed(_) => false,
            AutoResult::Failed(_) => answer == Answer::NotApplicable,
            AutoResult::Unknown(_) => true,
        }
    }

    pub fn evidence(&self) -> &str {
        match self {
            AutoResult::Passed(text) | AutoResult::Failed(text) | AutoResult::Unknown(text) => text,
        }
    }
}

/// The latest test command run in the PR's Run locally terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRun {
    pub head_sha: String,
    pub command: String,
    pub exit_code: i32,
}

pub struct AutoInputs<'a> {
    pub checks: ChecksState,
    pub threads: &'a [ReviewThread],
    pub files: Vec<&'a FileDiff>,
    pub head_sha: &'a str,
    pub local_run: Option<&'a LocalRun>,
}

const DEBUG_LEFTOVERS: [&str; 6] = [
    "TODO",
    "FIXME",
    "dbg!(",
    "console.log(",
    "debugger;",
    "breakpoint()",
];

pub fn evaluate(check: AutoCheck, inputs: &AutoInputs) -> AutoResult {
    match check {
        AutoCheck::ChecksPass => match inputs.checks {
            ChecksState::Passing => AutoResult::Passed("All checks passed".into()),
            ChecksState::Failing => AutoResult::Failed("Some checks failed".into()),
            ChecksState::Pending => AutoResult::Unknown("Checks are still running".into()),
            ChecksState::Unknown => AutoResult::Unknown("No checks reported".into()),
        },
        AutoCheck::ThreadsResolved => {
            let total = inputs.threads.len();
            let open = inputs
                .threads
                .iter()
                .filter(|thread| !thread.is_resolved)
                .count();
            match (total, open) {
                (0, _) => AutoResult::Passed("No review threads".into()),
                (_, 0) => AutoResult::Passed(format!("{total} of {total} resolved")),
                _ => AutoResult::Failed(format!("{open} of {total} unresolved")),
            }
        }
        AutoCheck::TestsChanged => match inputs
            .files
            .iter()
            .find(|file| classify_path(&file.path) == FileKind::Test)
        {
            Some(file) => AutoResult::Passed(format!("{} changed", file.path)),
            None => AutoResult::Failed("No test files changed".into()),
        },
        AutoCheck::RanLocally => match inputs
            .local_run
            .filter(|run| run.head_sha == inputs.head_sha)
        {
            Some(run) if run.exit_code == 0 => {
                AutoResult::Passed(format!("\"{}\" exited 0", run.command))
            }
            Some(run) => {
                AutoResult::Failed(format!("\"{}\" exited {}", run.command, run.exit_code))
            }
            None => AutoResult::Unknown("Use Run locally to run the tests".into()),
        },
        AutoCheck::NoDebugLeftovers => {
            for file in inputs.files.iter().filter(|file| !file.is_opaque()) {
                for hunk in &file.hunks {
                    let mut new_line = hunk.new_start;
                    for line in hunk.body.lines() {
                        match line.as_bytes().first() {
                            Some(b'+') => {
                                if let Some(found) = DEBUG_LEFTOVERS
                                    .iter()
                                    .find(|pattern| line.contains(*pattern))
                                {
                                    return AutoResult::Failed(format!(
                                        "{} in {}:{new_line}",
                                        found.trim_end_matches(['(', ';']),
                                        file.path
                                    ));
                                }
                                new_line += 1;
                            }
                            Some(b' ') => new_line += 1,
                            _ => {}
                        }
                    }
                }
            }
            AutoResult::Passed(format!("Checked {} files", inputs.files.len()))
        }
    }
}

const TEST_RUNNERS: [&str; 8] = [
    "pytest", "jest", "vitest", "mocha", "rspec", "phpunit", "ctest", "nextest",
];

/// Whether a command finished in the Run locally terminal counts as running the tests: it starts
/// with the PR's test command, or one of its words is `test` (`cargo test`, `npm run test:unit`)
/// or a test runner. `git checkout test-fix` and `ls tests/` don't count.
pub fn counts_as_test_run(command: &str, test_command: Option<&str>) -> bool {
    let command = command.trim();
    if let Some(test_command) = test_command.map(str::trim).filter(|test| !test.is_empty())
        && command.starts_with(test_command)
    {
        return true;
    }
    command.split_whitespace().any(|word| {
        let word = word.to_ascii_lowercase();
        let program = word.rsplit('/').next().unwrap_or(&word);
        word == "test" || word.starts_with("test:") || TEST_RUNNERS.contains(&program)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    NotApplicable,
}

impl Answer {
    pub fn to_i64(self) -> i64 {
        match self {
            Answer::Yes => 0,
            Answer::No => 1,
            Answer::NotApplicable => 2,
        }
    }

    pub fn from_i64(value: i64) -> Option<Self> {
        Some(match value {
            0 => Answer::Yes,
            1 => Answer::No,
            2 => Answer::NotApplicable,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub no: usize,
}

pub fn progress(
    auto: &[(AutoCheck, AutoResult)],
    items: &[ChecklistItem],
    answers: &HashMap<String, Answer>,
) -> Progress {
    let mut progress = Progress {
        total: auto.len() + items.len(),
        ..Progress::default()
    };
    let auto_answers = auto.iter().map(|(check, result)| {
        let answer = answers
            .get(&check.key())
            .copied()
            .filter(|answer| result.accepts(*answer));
        (result.passed(), answer)
    });
    let item_answers = items
        .iter()
        .map(|item| (false, answers.get(&item.key).copied()));
    for (passed, answer) in auto_answers.chain(item_answers) {
        match (passed, answer) {
            (true, _) | (false, Some(Answer::Yes | Answer::NotApplicable)) => progress.done += 1,
            (false, Some(Answer::No)) => progress.no += 1,
            (false, None) => {}
        }
    }
    progress
}

impl Progress {
    pub fn text(&self) -> String {
        let mut text = format!("{}/{}", self.done, self.total);
        if self.no > 0 {
            text.push_str(&format!(" · {} No", self.no));
        }
        text
    }
}

/// The checklist section appended to the review body. Left out when there is nothing a
/// person decided: no manual items and no answers.
pub fn review_block(
    auto: &[(AutoCheck, AutoResult)],
    items: &[ChecklistItem],
    answers: &HashMap<String, Answer>,
    notes: &HashMap<String, String>,
) -> Option<String> {
    let auto_answer = |check: &AutoCheck, result: &AutoResult| {
        answers
            .get(&check.key())
            .copied()
            .filter(|answer| result.accepts(*answer))
    };
    let answered_auto = auto
        .iter()
        .any(|(check, result)| auto_answer(check, result).is_some());
    if items.is_empty() && !answered_auto {
        return None;
    }
    let progress = progress(auto, items, answers);
    let mut block = format!("### Checklist ({}/{})\n", progress.done, progress.total);
    for (check, result) in auto {
        let title = check.title();
        let evidence = sentence_case(result.evidence());
        let line = match (result, auto_answer(check, result)) {
            (AutoResult::Passed(_), _) => format!("- [x] {title}: {evidence}"),
            (_, Some(Answer::Yes)) => format!("- [x] {title}: checked by the reviewer"),
            (_, Some(Answer::NotApplicable)) => format!("- N/A {title}"),
            (_, Some(Answer::No)) => no_line(title, notes.get(&check.key())),
            (_, None) => format!("- [ ] {title}: {evidence}"),
        };
        block.push_str(&line);
        block.push('\n');
    }
    for item in items {
        let text = &item.text;
        let line = match answers.get(&item.key) {
            Some(Answer::Yes) => format!("- [x] {text}"),
            Some(Answer::NotApplicable) => format!("- N/A {text}"),
            Some(Answer::No) => no_line(text, notes.get(&item.key)),
            None => format!("- [ ] {text}: not checked"),
        };
        block.push_str(&line);
        block.push('\n');
    }
    Some(block)
}

fn no_line(text: &str, note: Option<&String>) -> String {
    match note.map(|note| note.trim()) {
        Some(note) if !note.is_empty() => format!("- [ ] {text}: **No**, {note}"),
        _ => format!("- [ ] {text}: **No**"),
    }
}

/// Lowercases only the first letter, so paths and commands in evidence keep their case.
fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileStatus, Hunk, Side};
    use settings::PrReviewChecklistWhenContent;

    fn checklist(
        name: &str,
        paths: &[&str],
        repos: &[&str],
        items: &[&str],
    ) -> PrReviewChecklistContent {
        PrReviewChecklistContent {
            name: name.into(),
            when: PrReviewChecklistWhenContent {
                paths: paths.iter().map(|path| path.to_string()).collect(),
                repos: repos.iter().map(|repo| repo.to_string()).collect(),
            },
            items: items.iter().map(|item| item.to_string()).collect(),
        }
    }

    fn file(path: &str, body: &str) -> FileDiff {
        FileDiff {
            path: path.into(),
            status: FileStatus::Modified,
            binary: false,
            submodule: false,
            symlink: false,
            hunks: vec![Hunk {
                old_start: 10,
                old_len: 2,
                new_start: 10,
                new_len: 3,
                body: body.into(),
            }],
        }
    }

    fn thread(resolved: bool) -> ReviewThread {
        ReviewThread {
            id: "t".into(),
            path: "a.rs".into(),
            line: Some(1),
            start_line: None,
            side: Side::Right,
            is_resolved: resolved,
            is_outdated: false,
            comments: Vec::new(),
        }
    }

    fn inputs<'a>(files: &'a [FileDiff], threads: &'a [ReviewThread]) -> AutoInputs<'a> {
        AutoInputs {
            checks: ChecksState::Passing,
            threads,
            files: files.iter().collect(),
            head_sha: "h1",
            local_run: None,
        }
    }

    #[test]
    fn checklists_apply_by_repo_and_path() {
        let changed = vec!["migrations/0042.sql".to_string(), "src/api.rs".into()];
        let lists = [
            (
                ChecklistSource::Settings,
                checklist("Every PR", &[], &[], &["Errors handled"]),
            ),
            (
                ChecklistSource::Settings,
                checklist("Migrations", &["migrations/**"], &[], &["Reversible"]),
            ),
            (
                ChecklistSource::Settings,
                checklist("Docs", &["docs/**"], &[], &["Spelling"]),
            ),
            (
                ChecklistSource::Team,
                checklist("Payments", &[], &["Acme/Checkout"], &["Replay safe"]),
            ),
            (
                ChecklistSource::Team,
                checklist("Other repo", &[], &["acme/web"], &["Nope"]),
            ),
        ];
        let (items, errors) = applicable_items(&lists, "acme/checkout", &changed);
        assert!(errors.is_empty());
        let keys: Vec<_> = items.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "settings:Every PR:Errors handled",
                "settings:Migrations:Reversible",
                "team:Payments:Replay safe",
            ]
        );
        assert!(items[0].paths.is_none());
        assert!(items[1].matches_any(&["migrations/0042.sql".into()]));
        assert!(!items[1].matches_any(&["src/api.rs".into()]));
    }

    #[test]
    fn bad_glob_is_reported_not_fatal() {
        let lists = [
            (
                ChecklistSource::Team,
                checklist("Broken", &["src/[a"], &[], &["x"]),
            ),
            (ChecklistSource::Team, checklist("Fine", &[], &[], &["y"])),
        ];
        let (items, errors) = applicable_items(&lists, "a/b", &["src/a.rs".into()]);
        assert_eq!(items.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("Broken"), "{errors:?}");
    }

    #[test]
    fn team_file_parses() {
        let lists = parse_team_file(
            r#"{"checklists":[{"name":"API","when":{"paths":["api/**"]},"items":["Documented"]}]}"#,
        )
        .expect("parse");
        assert_eq!(lists, [checklist("API", &["api/**"], &[], &["Documented"])]);
        assert!(parse_team_file("{nope").is_err());
    }

    #[test]
    fn author_boxes_are_read_from_the_description() {
        let boxes = parse_author_boxes(
            "## Checklist\n- [x] Added tests\n* [ ] Tested on staging\n  - [X] Changelog\n- [ ]\n- not a box\n",
        );
        assert_eq!(
            boxes,
            [
                AuthorBox {
                    text: "Added tests".into(),
                    checked: true
                },
                AuthorBox {
                    text: "Tested on staging".into(),
                    checked: false
                },
                AuthorBox {
                    text: "Changelog".into(),
                    checked: true
                },
            ]
        );
    }

    #[test]
    fn auto_checks_name_their_evidence() {
        let threads = [thread(true), thread(false)];
        let files = [
            file("src/retry.rs", " a\n+let x = 1;\n+dbg!(x);\n b\n"),
            file("tests/retry_test.rs", "+#[test]\n"),
        ];
        let mut input = inputs(&files, &threads);
        assert_eq!(
            evaluate(AutoCheck::ChecksPass, &input),
            AutoResult::Passed("All checks passed".into())
        );
        assert_eq!(
            evaluate(AutoCheck::ThreadsResolved, &input),
            AutoResult::Failed("1 of 2 unresolved".into())
        );
        assert_eq!(
            evaluate(AutoCheck::TestsChanged, &input),
            AutoResult::Passed("tests/retry_test.rs changed".into())
        );
        assert_eq!(
            evaluate(AutoCheck::NoDebugLeftovers, &input),
            AutoResult::Failed("dbg! in src/retry.rs:12".into())
        );
        assert!(matches!(
            evaluate(AutoCheck::RanLocally, &input),
            AutoResult::Unknown(_)
        ));

        let run = LocalRun {
            head_sha: "h1".into(),
            command: "cargo test".into(),
            exit_code: 0,
        };
        input.local_run = Some(&run);
        assert_eq!(
            evaluate(AutoCheck::RanLocally, &input),
            AutoResult::Passed("\"cargo test\" exited 0".into())
        );
        let stale = LocalRun {
            head_sha: "old".into(),
            ..run.clone()
        };
        input.local_run = Some(&stale);
        assert!(matches!(
            evaluate(AutoCheck::RanLocally, &input),
            AutoResult::Unknown(_)
        ));
        input.checks = ChecksState::Pending;
        assert!(matches!(
            evaluate(AutoCheck::ChecksPass, &input),
            AutoResult::Unknown(_)
        ));

        let clean = [file("src/a.rs", "-dbg!(x);\n+let y = 2;\n")];
        assert!(evaluate(AutoCheck::NoDebugLeftovers, &inputs(&clean, &[])).passed());
        assert!(evaluate(AutoCheck::ThreadsResolved, &inputs(&clean, &[])).passed());
        assert!(!evaluate(AutoCheck::TestsChanged, &inputs(&clean, &[])).passed());
    }

    #[test]
    fn test_runs_are_recognised() {
        assert!(counts_as_test_run("make check-all", Some("make check")));
        assert!(counts_as_test_run("cargo test -p api", None));
        assert!(counts_as_test_run("npm run Test", Some("make check")));
        assert!(counts_as_test_run("npm run test:unit", None));
        assert!(counts_as_test_run("./node_modules/.bin/jest src", None));
        assert!(counts_as_test_run("python -m pytest -q", None));
        assert!(!counts_as_test_run("ls -la", None));
        assert!(!counts_as_test_run("git checkout test-fix", None));
        assert!(!counts_as_test_run("ls tests/", None));
        assert!(!counts_as_test_run("echo attest", None));
        assert!(!counts_as_test_run("git status", Some("  ")));
    }

    #[test]
    fn progress_counts_passed_and_answered() {
        let (items, _) = applicable_items(
            &[(
                ChecklistSource::Settings,
                checklist("A", &[], &[], &["one", "two", "three"]),
            )],
            "a/b",
            &[],
        );
        let auto = [
            (AutoCheck::ChecksPass, AutoResult::Passed("ok".into())),
            (AutoCheck::RanLocally, AutoResult::Unknown("not run".into())),
        ];
        let answers = HashMap::from_iter([
            (items[0].key.clone(), Answer::Yes),
            (items[1].key.clone(), Answer::No),
            (AutoCheck::RanLocally.key(), Answer::NotApplicable),
        ]);
        let progress = progress(&auto, &items, &answers);
        assert_eq!(
            progress,
            Progress {
                done: 3,
                total: 5,
                no: 1
            }
        );
        assert_eq!(progress.text(), "3/5 · 1 No");
    }

    #[test]
    fn review_block_lists_every_item() {
        let (items, _) = applicable_items(
            &[(
                ChecklistSource::Settings,
                checklist(
                    "A",
                    &[],
                    &[],
                    &["Replay safe", "Migration reversible", "No card data logged"],
                ),
            )],
            "a/b",
            &[],
        );
        let auto = [
            (
                AutoCheck::ChecksPass,
                AutoResult::Passed("All checks passed".into()),
            ),
            (AutoCheck::RanLocally, AutoResult::Unknown("not run".into())),
        ];
        let answers = HashMap::from_iter([
            (items[0].key.clone(), Answer::No),
            (items[1].key.clone(), Answer::NotApplicable),
        ]);
        let notes =
            HashMap::from_iter([(items[0].key.clone(), "needs an idempotency key".to_string())]);
        assert_eq!(
            review_block(&auto, &items, &answers, &notes).expect("block"),
            "### Checklist (2/5)\n\
             - [x] CI checks pass: all checks passed\n\
             - [ ] Tests ran locally: not run\n\
             - [ ] Replay safe: **No**, needs an idempotency key\n\
             - N/A Migration reversible\n\
             - [ ] No card data logged: not checked\n"
        );
    }

    #[test]
    fn failing_evidence_overrides_an_earlier_yes() {
        let pending_then_failed = [(
            AutoCheck::ChecksPass,
            AutoResult::Failed("Some checks failed".into()),
        )];
        let yes = HashMap::from_iter([(AutoCheck::ChecksPass.key(), Answer::Yes)]);
        assert_eq!(progress(&pending_then_failed, &[], &yes).done, 0);
        assert_eq!(
            review_block(&pending_then_failed, &[], &yes, &HashMap::default()),
            None
        );

        let not_applicable =
            HashMap::from_iter([(AutoCheck::ChecksPass.key(), Answer::NotApplicable)]);
        assert_eq!(progress(&pending_then_failed, &[], &not_applicable).done, 1);
        let unknown = [(
            AutoCheck::RanLocally,
            AutoResult::Unknown("Use Run locally".into()),
        )];
        let yes_run = HashMap::from_iter([(AutoCheck::RanLocally.key(), Answer::Yes)]);
        assert_eq!(
            review_block(&unknown, &[], &yes_run, &HashMap::default()).expect("block"),
            "### Checklist (1/1)\n- [x] Tests ran locally: checked by the reviewer\n"
        );
        let failed = [(
            AutoCheck::TestsChanged,
            AutoResult::Failed("dbg! in src/Api.rs:3".into()),
        )];
        let items = applicable_items(
            &[(ChecklistSource::Settings, checklist("A", &[], &[], &["x"]))],
            "a/b",
            &[],
        )
        .0;
        assert!(
            review_block(&failed, &items, &HashMap::default(), &HashMap::default())
                .expect("block")
                .contains("- [ ] Tests changed with the code: dbg! in src/Api.rs:3\n")
        );
    }

    #[test]
    fn review_block_is_left_out_when_nobody_decided_anything() {
        let auto = [(AutoCheck::ChecksPass, AutoResult::Passed("ok".into()))];
        assert_eq!(
            review_block(&auto, &[], &HashMap::default(), &HashMap::default()),
            None
        );
        let failed = [(AutoCheck::RanLocally, AutoResult::Unknown("not run".into()))];
        let answers = HashMap::from_iter([(AutoCheck::RanLocally.key(), Answer::Yes)]);
        assert!(review_block(&failed, &[], &answers, &HashMap::default()).is_some());
    }
}
