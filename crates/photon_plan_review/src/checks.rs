use std::collections::HashSet;

use crate::{FileRef, FileRole, Plan, last_segment, requirement_headings, significant_words};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    Exists,
    /// Created by this task or an earlier one, or a test file still to be written.
    New,
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    MissingFile,
    NoTest,
    UndefinedIdentifier,
    SharedFile,
    UncoveredRequirement,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Stable across reloads, for dismissing.
    pub key: String,
    pub kind: FindingKind,
    pub task: Option<usize>,
    /// Text the finding is about; its card goes after the first paragraph that contains it.
    pub anchor: Option<String>,
    pub title: String,
    pub evidence: String,
}

impl Finding {
    pub fn is_error(&self) -> bool {
        self.kind == FindingKind::MissingFile
    }
}

/// What the repository is asked about. `found` is `None` until the project search for
/// identifiers has finished.
pub struct Repository<'a> {
    pub exists: &'a dyn Fn(&str) -> bool,
    pub found: Option<&'a HashSet<String>>,
}

pub fn file_state(
    plan: &Plan,
    task: usize,
    file: &FileRef,
    exists: &dyn Fn(&str) -> bool,
) -> FileState {
    let created = plan.tasks.iter().take(task + 1).any(|earlier| {
        earlier
            .files
            .iter()
            .any(|other| other.path == file.path && other.role == FileRole::Create)
    });
    if created {
        FileState::New
    } else if exists(&file.path) {
        FileState::Exists
    } else if file.role == FileRole::Test {
        FileState::New
    } else {
        FileState::NotFound
    }
}

/// Identifiers the tasks use that no task defines, so only the repository can.
pub fn unresolved_identifiers(plan: &Plan) -> Vec<String> {
    let mut names = Vec::new();
    for task in &plan.tasks {
        for identifier in &task.uses {
            if plan.definer(identifier).is_none() {
                let name = last_segment(identifier).to_string();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names
}

/// `source` is the plan's text; `spec` the linked spec's path and text.
pub fn check_plan(
    plan: &Plan,
    source: &str,
    repository: &Repository,
    spec: Option<(&str, &str)>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (index, task) in plan.tasks.iter().enumerate() {
        let number = task_label(plan, index);
        for file in &task.files {
            if file_state(plan, index, file, repository.exists) != FileState::NotFound {
                continue;
            }
            let verb = match file.role {
                FileRole::Modify => "modifies",
                _ => "names",
            };
            findings.push(Finding {
                key: format!("missing-file:{}:{}", task.key, file.path),
                kind: FindingKind::MissingFile,
                task: Some(index),
                anchor: Some(file.path.clone()),
                title: format!("{number} {verb} {}, which doesn't exist", file.path),
                evidence: missing_file_evidence(plan, index, &file.path),
            });
        }
        if task.adds_code() && !task.names_test() {
            findings.push(Finding {
                key: format!("no-test:{}", task.key),
                kind: FindingKind::NoTest,
                task: Some(index),
                anchor: None,
                title: format!("{number} adds code with no test"),
                evidence: if task.summary.is_empty() {
                    task.title.clone()
                } else {
                    task.summary.clone()
                },
            });
        }
    }

    if let Some(found) = repository.found {
        let mut reported = HashSet::new();
        for (index, task) in plan.tasks.iter().enumerate() {
            for identifier in &task.uses {
                let name = last_segment(identifier);
                if plan.definer(identifier).is_some()
                    || found.contains(name)
                    || !reported.insert(name.to_string())
                {
                    continue;
                }
                findings.push(Finding {
                    key: format!("undefined:{name}"),
                    kind: FindingKind::UndefinedIdentifier,
                    task: Some(index),
                    anchor: Some(identifier.clone()),
                    title: format!(
                        "{identifier} is used in {} but defined nowhere",
                        task_label(plan, index).to_lowercase()
                    ),
                    evidence: "Not in the plan, not in the repository".into(),
                });
            }
        }
    }

    let mut paths: Vec<&str> = Vec::new();
    for task in &plan.tasks {
        for file in &task.files {
            if file.role == FileRole::Modify && !paths.contains(&file.path.as_str()) {
                paths.push(&file.path);
            }
        }
    }
    for path in paths {
        let modifying: Vec<usize> = plan
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| {
                task.files
                    .iter()
                    .any(|file| file.path == path && file.role == FileRole::Modify)
            })
            .map(|(index, _)| index)
            .collect();
        if let [first, .., last] = modifying[..] {
            let numbers: Vec<String> = modifying
                .iter()
                .map(|index| number_of(plan, *index))
                .collect();
            findings.push(Finding {
                key: format!("shared:{path}"),
                kind: FindingKind::SharedFile,
                task: Some(first),
                anchor: Some(path.to_string()),
                title: format!(
                    "Tasks {} and {} both modify {path}",
                    numbers[..numbers.len() - 1].join(", "),
                    number_of(plan, last)
                ),
                evidence: "Check they agree on what the file looks like".into(),
            });
        }
    }

    if let Some((spec_path, spec_text)) = spec {
        let task_text: Vec<String> = plan
            .tasks
            .iter()
            .map(|task| {
                source
                    .get(task.range.clone())
                    .unwrap_or_default()
                    .to_lowercase()
            })
            .collect();
        for requirement in requirement_headings(spec_text) {
            let words = significant_words(&requirement);
            if words.is_empty() {
                continue;
            }
            let needed = words.len().div_ceil(2);
            let covered = task_text.iter().any(|text| {
                words
                    .iter()
                    .filter(|word| text.contains(word.as_str()))
                    .count()
                    >= needed
            });
            if !covered {
                findings.push(Finding {
                    key: format!("requirement:{requirement}"),
                    kind: FindingKind::UncoveredRequirement,
                    task: None,
                    anchor: None,
                    title: format!("No task covers “{requirement}”"),
                    evidence: format!("A requirement in {spec_path}"),
                });
            }
        }
    }

    findings.sort_by_key(|finding| (finding.kind, finding.task));
    findings
}

fn number_of(plan: &Plan, index: usize) -> String {
    plan.tasks
        .get(index)
        .and_then(|task| task.number.clone())
        .unwrap_or_else(|| (index + 1).to_string())
}

/// "Task 3", or the task's own id for Spec Kit ("T003").
pub fn task_label(plan: &Plan, index: usize) -> String {
    let number = number_of(plan, index);
    if number.starts_with(|character: char| character.is_ascii_digit()) {
        format!("Task {number}")
    } else {
        number
    }
}

fn missing_file_evidence(plan: &Plan, index: usize, path: &str) -> String {
    let created: Vec<(usize, &str)> = plan
        .tasks
        .iter()
        .enumerate()
        .flat_map(|(task_index, task)| {
            task.files
                .iter()
                .filter(|file| file.role == FileRole::Create)
                .map(move |file| (task_index, file.path.as_str()))
        })
        .collect();
    if let Some((task_index, _)) = created
        .iter()
        .find(|(task_index, created)| *task_index > index && *created == path)
    {
        return format!(
            "{} creates it later; the order may be wrong",
            task_label(plan, *task_index)
        );
    }
    let directory = |path: &str| {
        path.rsplit_once('/')
            .map(|(directory, _)| directory.to_string())
    };
    if let Some((task_index, created)) = created
        .iter()
        .find(|(task_index, created)| *task_index < index && directory(created) == directory(path))
    {
        return format!(
            "{} creates {created}; is this the same file?",
            task_label(plan, *task_index)
        );
    }
    "Not in the repository, and no earlier task creates it".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_plan;

    const SUPERPOWERS: &str = include_str!("../test_fixtures/superpowers.md");
    const SPEC: &str = include_str!("../test_fixtures/spec.md");

    fn billing_exists(path: &str) -> bool {
        [
            "Cargo.toml",
            "src/main.rs",
            "src/payments.rs",
            "src/ledger.rs",
            "src/jobs.rs",
            "docs/architecture.md",
        ]
        .contains(&path)
    }

    fn titles(findings: &[Finding]) -> Vec<&str> {
        findings
            .iter()
            .map(|finding| finding.title.as_str())
            .collect()
    }

    #[test]
    fn file_states_follow_the_plan_order() {
        let plan = parse_plan(SUPERPOWERS).expect("a plan");
        let charge = &plan.tasks[2];
        let states: Vec<_> = charge
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_str(),
                    file_state(&plan, 2, file, &billing_exists),
                )
            })
            .collect();
        assert_eq!(
            states,
            [
                ("src/payments.rs", FileState::Exists),
                ("src/gateway.rs", FileState::NotFound),
                ("src/retry.rs", FileState::New),
            ]
        );
        let nightly = &plan.tasks[4];
        assert_eq!(
            file_state(&plan, 4, &nightly.files[1], &billing_exists),
            FileState::New,
            "a test file still to be written"
        );
    }

    #[test]
    fn checks_without_the_search_or_spec() {
        let plan = parse_plan(SUPERPOWERS).expect("a plan");
        let findings = check_plan(
            &plan,
            SUPERPOWERS,
            &Repository {
                exists: &billing_exists,
                found: None,
            },
            None,
        );
        assert_eq!(
            titles(&findings),
            [
                "Task 3 modifies src/gateway.rs, which doesn't exist",
                "Task 6 adds code with no test",
            ]
        );
        assert_eq!(
            findings[0].evidence,
            "Task 1 creates src/retry.rs; is this the same file?"
        );
        assert_eq!(findings[0].anchor.as_deref(), Some("src/gateway.rs"));
        assert!(findings[0].is_error() && !findings[1].is_error());
        assert_eq!(
            findings[1].evidence,
            "Metrics counter for retries, so the dashboard shows how often charges are retried"
        );
    }

    #[test]
    fn undefined_identifiers_need_the_search() {
        let plan = parse_plan(SUPERPOWERS).expect("a plan");
        assert_eq!(
            unresolved_identifiers(&plan),
            [
                "is_retryable",
                "record_payment",
                "retry_failed_invoices",
                "failed_invoices"
            ]
        );
        let found: HashSet<String> = ["record_payment", "retry_failed_invoices", "failed_invoices"]
            .into_iter()
            .map(String::from)
            .collect();
        let findings = check_plan(
            &plan,
            SUPERPOWERS,
            &Repository {
                exists: &billing_exists,
                found: Some(&found),
            },
            None,
        );
        let undefined: Vec<_> = findings
            .iter()
            .filter(|finding| finding.kind == FindingKind::UndefinedIdentifier)
            .collect();
        assert_eq!(undefined.len(), 1);
        assert_eq!(
            undefined[0].title,
            "is_retryable is used in task 3 but defined nowhere"
        );
        assert_eq!(undefined[0].task, Some(2));
        assert_eq!(
            undefined[0].evidence,
            "Not in the plan, not in the repository"
        );
    }

    #[test]
    fn shared_files_and_uncovered_requirements() {
        let source = "# Plan\n\n### Task 1: One\n\n**Files:**\n- Modify: `src/a.rs`\n\nRun: `cargo test`\n\n\
             ### Task 2: Two\n\n**Files:**\n- Modify: `src/b.rs`\n\nRun: `cargo test`\n\n\
             ### Task 3: Three\n\n**Files:**\n- Modify: `src/a.rs`\n\nRun: `cargo test`\n";
        let plan = parse_plan(source).expect("a plan");
        let findings = check_plan(
            &plan,
            source,
            &Repository {
                exists: &|_| true,
                found: None,
            },
            Some((
                "docs/spec.md",
                "# Spec\n\n## Partial refunds\n\n## Three things\n",
            )),
        );
        assert_eq!(
            titles(&findings),
            [
                "Tasks 1 and 3 both modify src/a.rs",
                "No task covers “Partial refunds”",
            ]
        );
        assert_eq!(findings[1].evidence, "A requirement in docs/spec.md");
        assert_eq!(findings[1].task, None);
    }

    #[test]
    fn the_billing_spec_has_one_uncovered_requirement() {
        let plan = parse_plan(SUPERPOWERS).expect("a plan");
        let findings = check_plan(
            &plan,
            SUPERPOWERS,
            &Repository {
                exists: &billing_exists,
                found: None,
            },
            Some(("docs/specs/retry.md", SPEC)),
        );
        let uncovered: Vec<_> = findings
            .iter()
            .filter(|finding| finding.kind == FindingKind::UncoveredRequirement)
            .map(|finding| finding.title.as_str())
            .collect();
        assert_eq!(uncovered, ["No task covers “Refund partial captures”"]);
    }
}
