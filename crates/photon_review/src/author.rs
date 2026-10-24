use crate::ReviewThread;

/// Unresolved threads in reading order: by file, then line. Threads without a line (outdated, or
/// on a whole file) come first in their file.
pub fn open_feedback(threads: &[ReviewThread]) -> Vec<&ReviewThread> {
    let mut open: Vec<&ReviewThread> = threads
        .iter()
        .filter(|thread| !thread.is_resolved)
        .collect();
    open.sort_by(|left, right| {
        (&left.path, left.line.unwrap_or(0)).cmp(&(&right.path, right.line.unwrap_or(0)))
    });
    open
}

/// Commits everything changed in the review checkout and pushes it to the pull request's
/// branch. Typed into a terminal for the author to read and run, never run by Photon.
pub fn push_command(host: &str, head_repository: &str, branch: &str) -> String {
    format!(
        "git commit -am {} && git push {} HEAD:{}",
        shell_quote("Address review feedback"),
        shell_quote(&format!("https://{host}/{head_repository}.git")),
        shell_quote(&format!("refs/heads/{branch}")),
    )
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Side;

    fn thread(id: &str, path: &str, line: Option<u32>, resolved: bool) -> ReviewThread {
        ReviewThread {
            id: id.into(),
            path: path.into(),
            line,
            start_line: None,
            side: Side::Right,
            is_resolved: resolved,
            is_outdated: line.is_none(),
            comments: Vec::new(),
        }
    }

    #[test]
    fn open_feedback_is_in_reading_order() {
        let threads = [
            thread("c", "src/b.rs", Some(4), false),
            thread("a", "src/a.rs", Some(30), false),
            thread("done", "src/a.rs", Some(1), true),
            thread("b", "src/a.rs", None, false),
            thread("d", "src/a.rs", Some(31), false),
        ];
        let ids: Vec<_> = open_feedback(&threads)
            .into_iter()
            .map(|thread| thread.id.as_str())
            .collect();
        assert_eq!(ids, ["b", "a", "d", "c"]);
    }

    #[test]
    fn push_command_targets_the_branch() {
        assert_eq!(
            push_command("ghe.corp", "maya/web", "fix-it's"),
            r"git commit -am 'Address review feedback' && git push 'https://ghe.corp/maya/web.git' HEAD:'refs/heads/fix-it'\''s'"
        );
    }
}
