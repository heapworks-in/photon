use anyhow::{Context as _, Result, bail};
use base64::Engine as _;

use crate::{GhRequest, PrKey, rest_request};

/// A GitHub suggestion block replacing the commented lines with `lines`.
pub fn suggestion_block(lines: &str) -> String {
    format!("```suggestion\n{lines}\n```")
}

/// The replacement lines of the single suggestion in a comment; none means delete the lines. A
/// comment with no suggestion, or with several, has nothing one click can commit.
pub fn parse_suggestion(body: &str) -> Option<Vec<String>> {
    let mut found = None;
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "```suggestion" {
            continue;
        }
        let mut replacement = Vec::new();
        let mut closed = false;
        for line in lines.by_ref() {
            if line.trim() == "```" {
                closed = true;
                break;
            }
            replacement.push(line.to_string());
        }
        if !closed || found.is_some() {
            return None;
        }
        found = Some(replacement);
    }
    found
}

/// Replaces 1-based lines `start..=end` of `text` with `replacement` (no lines deletes them).
/// Every other line keeps its own ending; the new lines take the replaced lines' ending, and the
/// last one keeps the last replaced line's, so a file without a final newline stays that way.
pub fn apply_suggestion(
    text: &str,
    start: u32,
    end: u32,
    replacement: &[String],
) -> Result<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let (start, end) = (start as usize, end as usize);
    if start == 0 || start > end || end > lines.len() {
        bail!(
            "The suggestion is for lines {start}–{end}, but the file has {} lines.",
            lines.len()
        );
    }
    let ending = |line: &str| {
        if line.ends_with("\r\n") {
            "\r\n"
        } else if line.ends_with('\n') {
            "\n"
        } else {
            ""
        }
    };
    let inner_ending = match ending(lines[start - 1]) {
        "" => "\n",
        ending => ending,
    };
    let last_ending = ending(lines[end - 1]);
    let mut result: String = lines[..start - 1].concat();
    for (index, line) in replacement.iter().enumerate() {
        result.push_str(line);
        result.push_str(if index + 1 == replacement.len() {
            last_ending
        } else {
            inner_ending
        });
    }
    result.push_str(&lines[end..].concat());
    Ok(result)
}

fn contents_path(head_repository: &str, path: &str) -> String {
    let path = path
        .split('/')
        .map(|segment| urlencoding::encode(segment).into_owned())
        .collect::<Vec<_>>()
        .join("/");
    format!("repos/{head_repository}/contents/{path}")
}

/// The file as it is at `commit`, with its blob sha.
pub fn file_at_commit_request(
    key: &PrKey,
    head_repository: &str,
    path: &str,
    commit: &str,
) -> GhRequest {
    rest_request(
        &key.host,
        "GET",
        &format!("{}?ref={commit}", contents_path(head_repository, path)),
        None,
    )
}

/// The text and blob sha from a contents API response.
pub fn parse_file_contents(json: &str) -> Result<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Contents {
        sha: String,
        #[serde(default)]
        content: String,
        #[serde(default)]
        encoding: String,
    }
    let contents: Contents = serde_json::from_str(json).context("reading the file from GitHub")?;
    if contents.encoding != "base64" {
        bail!("GitHub didn't send this file's text; it may be over 1 MB.");
    }
    let encoded: String = contents
        .content
        .chars()
        .filter(|char| !char.is_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("decoding the file from GitHub")?;
    let text = String::from_utf8(bytes).context("the file isn't text")?;
    Ok((text, contents.sha))
}

/// Commits `content` as `path` on the pull request's branch through GitHub's contents API, the
/// way github.com's "Commit suggestion" does. `blob_sha` is the file's blob at the reviewed
/// head; GitHub refuses the commit if the branch has a different version of the file by now.
pub fn commit_file_request(
    key: &PrKey,
    head_repository: &str,
    branch: &str,
    path: &str,
    content: &str,
    blob_sha: &str,
    message: &str,
) -> GhRequest {
    rest_request(
        &key.host,
        "PUT",
        &contents_path(head_repository, path),
        Some(serde_json::json!({
            "message": message,
            "content": base64::engine::general_purpose::STANDARD.encode(content),
            "sha": blob_sha,
            "branch": branch,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_are_parsed_only_when_there_is_one() {
        let body =
            "Simpler:\n```suggestion\n    let x = 1;\n    let y = 2;\n```\nWhat do you think?";
        assert_eq!(
            parse_suggestion(body),
            Some(vec!["    let x = 1;".to_string(), "    let y = 2;".into()])
        );
        assert_eq!(
            parse_suggestion("Delete it.\n```suggestion\n```"),
            Some(Vec::new())
        );
        assert_eq!(
            parse_suggestion("Blank it.\n```suggestion\n\n```"),
            Some(vec![String::new()])
        );
        assert_eq!(parse_suggestion("```rust\nlet x = 1;\n```"), None);
        assert_eq!(parse_suggestion("```suggestion\nunclosed"), None);
        assert_eq!(
            parse_suggestion("```suggestion\na\n```\n```suggestion\nb\n```"),
            None
        );
        assert_eq!(
            parse_suggestion(&suggestion_block("let z = 3;")),
            Some(vec!["let z = 3;".to_string()])
        );
    }

    #[test]
    fn suggestions_replace_the_commented_lines() {
        let lines = |text: &str| -> Vec<String> { text.split('\n').map(str::to_string).collect() };
        let text = "a\nb\nc\nd\n";
        assert_eq!(
            apply_suggestion(text, 2, 3, &lines("B\nC2\nC3")).unwrap(),
            "a\nB\nC2\nC3\nd\n"
        );
        assert_eq!(apply_suggestion(text, 4, 4, &[]).unwrap(), "a\nb\nc\n");
        assert_eq!(
            apply_suggestion(text, 2, 2, &[String::new()]).unwrap(),
            "a\n\nc\nd\n"
        );
        assert_eq!(
            apply_suggestion(text, 2, 2, &lines("B\n")).unwrap(),
            "a\nB\n\nc\nd\n"
        );
        // Mixed endings: only the replaced lines change.
        assert_eq!(
            apply_suggestion("a\r\nb\nc\r\n", 2, 2, &lines("B")).unwrap(),
            "a\r\nB\nc\r\n"
        );
        assert_eq!(apply_suggestion("a\nb", 2, 2, &lines("B")).unwrap(), "a\nB");
        assert!(apply_suggestion(text, 4, 5, &lines("x")).is_err());
        assert!(apply_suggestion(text, 0, 1, &lines("x")).is_err());
    }

    #[test]
    fn file_contents_are_decoded() {
        let (text, sha) =
            parse_file_contents(r#"{"sha":"b1","encoding":"base64","content":"bmV3\nCg==\n"}"#)
                .expect("contents");
        assert_eq!((text.as_str(), sha.as_str()), ("new\n", "b1"));
        assert!(parse_file_contents(r#"{"sha":"b1","encoding":"none","content":""}"#).is_err());
    }

    #[test]
    fn commit_request_targets_the_head_branch() {
        let key = PrKey {
            host: "ghe.corp".into(),
            owner: "acme".into(),
            repo: "web".into(),
            number: 7,
        };
        let request = commit_file_request(
            &key,
            "maya/web",
            "retry",
            "src/my file.rs",
            "new\n",
            "abc123",
            "Apply suggestion from code review",
        );
        assert_eq!(
            request.args,
            [
                "api",
                "--hostname",
                "ghe.corp",
                "--method",
                "PUT",
                "repos/maya/web/contents/src/my%20file.rs",
                "--input",
                "-"
            ]
        );
        let body: serde_json::Value =
            serde_json::from_str(request.stdin.as_deref().expect("stdin")).expect("json");
        assert_eq!(
            body,
            serde_json::json!({
                "message": "Apply suggestion from code review",
                "content": "bmV3Cg==",
                "sha": "abc123",
                "branch": "retry",
            })
        );
    }
}
