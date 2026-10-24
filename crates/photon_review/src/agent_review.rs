use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Global, Task, WeakEntity, Window};
use serde::Deserialize;
use workspace::Workspace;

use crate::{FileDiff, Step};

/// Runs a review prompt through one of the user's agents and returns its final reply. The app
/// provides it (the agent panel lives outside this crate); tests provide a fake.
pub trait ReviewAgent {
    fn review(
        &self,
        workspace: WeakEntity<Workspace>,
        prompt: String,
        work_dir: PathBuf,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<String>>;
}

struct GlobalReviewAgent(Rc<dyn ReviewAgent>);

impl Global for GlobalReviewAgent {}

pub fn set_review_agent(agent: Rc<dyn ReviewAgent>, cx: &mut App) {
    cx.set_global(GlobalReviewAgent(agent));
}

pub fn review_agent(cx: &App) -> Option<Rc<dyn ReviewAgent>> {
    cx.try_global::<GlobalReviewAgent>()
        .map(|global| global.0.clone())
}

/// What the agent found at one place in the pull request.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AgentComment {
    pub path: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub body: String,
}

/// Agents read whole files themselves; the prompt carries the diff up to this size.
const MAX_DIFF_BYTES: usize = 200_000;

pub struct PromptInput<'a> {
    pub number: u64,
    pub title: &'a str,
    pub description: &'a str,
    pub steps: &'a [Step],
    pub files: Vec<&'a FileDiff>,
}

pub fn review_prompt(input: &PromptInput) -> String {
    let mut prompt = format!(
        "You are reviewing pull request #{} \"{}\". The working directory is checked out at the \
         pull request's head.\n\n\
         Read any files you need, but don't modify files, don't run commands that change \
         anything, and don't post anything: your findings become draft comments the reviewer \
         reads first.\n",
        input.number, input.title
    );
    let description = input.description.trim();
    if !description.is_empty() {
        prompt.push_str(&format!("\nThe author's description:\n\n{description}\n"));
    }
    if !input.steps.is_empty() {
        prompt.push_str("\nThe change, in the order to read it:\n");
        for (index, step) in input.steps.iter().enumerate() {
            prompt.push_str(&format!(
                "{}. {} ({})\n",
                index + 1,
                step.title,
                step.files.join(", ")
            ));
        }
    }
    prompt.push_str("\nThe diff:\n\n```diff\n");
    let mut size = 0;
    let mut left_out = Vec::new();
    for file in &input.files {
        let text: String = file
            .hunks
            .iter()
            .map(|hunk| {
                format!(
                    "@@ -{},{} +{},{} @@\n{}",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len, hunk.body
                )
            })
            .collect();
        if file.is_opaque() || text.is_empty() {
            continue;
        }
        if size + text.len() > MAX_DIFF_BYTES {
            left_out.push(file.path.as_str());
            continue;
        }
        size += text.len();
        prompt.push_str(&format!("--- a/{0}\n+++ b/{0}\n{text}", file.path));
    }
    prompt.push_str("```\n");
    if !left_out.is_empty() {
        prompt.push_str(&format!(
            "\nLeft out of the diff for size; read them from disk: {}\n",
            left_out.join(", ")
        ));
    }
    prompt.push_str(
        "\nReply with your findings as one JSON array in a ```json block, and nothing after it:\n\n\
         ```json\n[{\"path\": \"src/file.rs\", \"line\": 42, \"body\": \"What is wrong and how to fix it.\"}]\n```\n\n\
         `path` is relative to the repository root. `line` is a line number in the new version \
         of the file, on or next to lines the diff changes; use null for a comment on the whole \
         file. Only report real problems: bugs, unhandled errors, security issues, missing \
         tests. Reply with [] if you find none.\n",
    );
    prompt
}

/// The comments in the last ```json block of the agent's reply.
pub fn parse_agent_comments(reply: &str) -> Result<Vec<AgentComment>> {
    let start = reply
        .rfind("```json")
        .ok_or_else(|| anyhow!("The agent's reply has no ```json block of findings."))?;
    let rest = &reply[start + "```json".len()..];
    let end = rest
        .find("```")
        .ok_or_else(|| anyhow!("The agent's ```json block isn't closed."))?;
    let comments: Vec<AgentComment> =
        serde_json::from_str(rest[..end].trim()).context("reading the agent's findings")?;
    Ok(comments
        .into_iter()
        .filter(|comment| !comment.body.trim().is_empty() && !comment.path.trim().is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileStatus, Hunk};

    #[test]
    fn findings_come_from_the_last_json_block() {
        let reply = "Thinking...\n```json\n[{\"path\":\"x\",\"line\":1,\"body\":\"draft\"}]\n```\n\
                     Final:\n```json\n[\n {\"path\": \"src/a.rs\", \"line\": 3, \"body\": \"Unchecked unwrap.\"},\n \
                     {\"path\": \"src/b.rs\", \"line\": null, \"body\": \"No tests.\"},\n \
                     {\"path\": \"src/c.rs\", \"line\": 9, \"body\": \"  \"}\n]\n```";
        assert_eq!(
            parse_agent_comments(reply).expect("parse"),
            [
                AgentComment {
                    path: "src/a.rs".into(),
                    line: Some(3),
                    body: "Unchecked unwrap.".into()
                },
                AgentComment {
                    path: "src/b.rs".into(),
                    line: None,
                    body: "No tests.".into()
                },
            ]
        );
        assert!(parse_agent_comments("Looks good to me!").is_err());
        assert!(parse_agent_comments("```json\n[{]\n```").is_err());
        assert_eq!(parse_agent_comments("```json\n[]\n```").expect("empty"), []);
    }

    #[test]
    fn prompt_carries_order_diff_and_rules() {
        let file = FileDiff {
            path: "src/retry.rs".into(),
            status: FileStatus::Modified,
            binary: false,
            submodule: false,
            symlink: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 2,
                body: " a\n+b\n".into(),
            }],
        };
        let steps = [Step {
            key: "k".into(),
            title: "retry.rs · retry()".into(),
            subtitle: String::new(),
            files: vec!["src/retry.rs".into()],
            functions: Vec::new(),
        }];
        let prompt = review_prompt(&PromptInput {
            number: 482,
            title: "Retry uploads",
            description: "Adds retries.",
            steps: &steps,
            files: vec![&file],
        });
        assert!(prompt.contains("pull request #482 \"Retry uploads\""));
        assert!(prompt.contains("don't modify files"));
        assert!(prompt.contains("Adds retries."));
        assert!(prompt.contains("1. retry.rs · retry() (src/retry.rs)"));
        assert!(
            prompt.contains("--- a/src/retry.rs\n+++ b/src/retry.rs\n@@ -1,1 +1,2 @@\n a\n+b\n")
        );
        assert!(prompt.contains("```json"));
    }
}
