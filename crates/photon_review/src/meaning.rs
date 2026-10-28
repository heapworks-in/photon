use std::collections::HashSet;
use std::ops::Range;

use buffer_diff::BufferDiffSnapshot;
use language::{BufferSnapshot, Point};
use similar::{Algorithm, DiffOp, capture_diff_slices};

use crate::{ChangedFunction, FunctionChange, Move};

/// What one hunk of the diff does, read from the syntax trees of both sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chip {
    SignatureChanged,
    BehaviourChange,
    RenameOnly {
        old: String,
        new: String,
        places: usize,
    },
    Moved {
        from_line: u32,
        to_line: u32,
    },
    FormattingOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinceViewed {
    /// The file wasn't viewed at an older head, or that head's text isn't available.
    NotViewed,
    /// Rows of the hunk that changed after the viewed head.
    New(Vec<u32>),
    Reviewed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkMeaning {
    pub path: String,
    /// Head rows, as `changed_rows` gives them: a deletion is the one row at the deletion point.
    pub rows: Range<u32>,
    /// Whether the hunk has head rows of its own, rather than only deleting.
    pub adds: bool,
    /// Base rows the hunk deletes.
    pub base_rows: Range<u32>,
    pub chips: Vec<Chip>,
    /// The innermost changed function the hunk starts in, as an index into the review's functions.
    pub function: Option<usize>,
    pub since_viewed: SinceViewed,
}

impl HunkMeaning {
    /// The word the file summary counts this hunk under.
    pub fn summary_word(&self) -> Option<&'static str> {
        let has = |wanted: fn(&Chip) -> bool| self.chips.iter().any(wanted);
        if has(|chip| *chip == Chip::BehaviourChange) {
            Some("behaviour change")
        } else if has(|chip| *chip == Chip::SignatureChanged) {
            Some("signature change")
        } else if has(|chip| matches!(chip, Chip::RenameOnly { .. })) {
            Some("rename")
        } else if has(|chip| matches!(chip, Chip::Moved { .. })) {
            Some("moved")
        } else if has(|chip| *chip == Chip::FormattingOnly) {
            Some("formatting only")
        } else {
            None
        }
    }

    pub fn rename(&self) -> Option<(&str, &str, usize)> {
        self.chips.iter().find_map(|chip| match chip {
            Chip::RenameOnly { old, new, places } => Some((old.as_str(), new.as_str(), *places)),
            _ => None,
        })
    }
}

/// "3 hunks: 1 behaviour change, 1 rename, 1 moved".
pub fn file_summary<'a>(hunks: impl IntoIterator<Item = &'a HunkMeaning>) -> Option<String> {
    let mut total = 0;
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for hunk in hunks {
        total += 1;
        if let Some(word) = hunk.summary_word() {
            match counts.iter_mut().find(|(seen, _)| *seen == word) {
                Some((_, count)) => *count += 1,
                None => counts.push((word, 1)),
            }
        }
    }
    if total == 0 {
        return None;
    }
    let order = [
        "behaviour change",
        "signature change",
        "rename",
        "moved",
        "formatting only",
    ];
    counts.sort_by_key(|(word, _)| order.iter().position(|ordered| ordered == word));
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(word, count)| match (word, count) {
            ("behaviour change" | "signature change" | "rename", count) if count > 1 => {
                format!("{count} {word}s")
            }
            _ => format!("{count} {word}"),
        })
        .collect();
    let hunks = if total == 1 { "hunk" } else { "hunks" };
    Some(if parts.is_empty() {
        format!("{total} {hunks}")
    } else {
        format!("{total} {hunks}: {}", parts.join(", "))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    text: String,
    identifier: bool,
    comment: bool,
}

/// The leaves of the syntax tree that start inside `range`, or `None` without a tree.
fn tokens(snapshot: &BufferSnapshot, range: Range<usize>) -> Option<Vec<Token>> {
    let layer = snapshot.syntax_layers().next()?;
    let mut cursor = layer.node().walk();
    let mut tokens = Vec::new();
    loop {
        let node = cursor.node();
        let overlaps = node.start_byte() < range.end && range.start < node.end_byte();
        let comment = node.kind().contains("comment");
        if overlaps && (node.child_count() == 0 || comment) {
            if node.start_byte() >= range.start {
                let text: String = snapshot.text_for_range(node.byte_range()).collect();
                if !text.trim().is_empty() {
                    tokens.push(Token {
                        identifier: node.kind().ends_with("identifier"),
                        comment,
                        text: if comment { squash(&text) } else { text },
                    });
                }
            }
        } else if overlaps && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some(tokens);
            }
        }
    }
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect()
}

/// Commas come and go with formatters (trailing commas), so they don't count as code.
fn code(tokens: &[Token]) -> Vec<&Token> {
    tokens
        .iter()
        .filter(|token| !token.comment && token.text != ",")
        .collect()
}

/// The one identifier renamed throughout, with the number of places.
fn single_rename(old: &[&Token], new: &[&Token]) -> Option<(String, String, usize)> {
    if old.len() != new.len() {
        return None;
    }
    let mut rename: Option<(&str, &str)> = None;
    let mut places = 0;
    for (old, new) in old.iter().zip(new) {
        if old.text == new.text {
            continue;
        }
        if !old.identifier || !new.identifier {
            return None;
        }
        match rename {
            None => rename = Some((&old.text, &new.text)),
            Some(pair) if pair == (old.text.as_str(), new.text.as_str()) => {}
            Some(_) => return None,
        }
        places += 1;
    }
    rename.map(|(old, new)| (old.to_string(), new.to_string(), places))
}

/// Rows of a function's header: from its first row to the row that opens its body.
fn header_rows(function: &ChangedFunction, buffer: &BufferSnapshot) -> Range<u32> {
    let start = function.range.start.row;
    let last = function.range.end.row;
    let mut row = function.selection_point.row;
    while row < last {
        let line: String = buffer
            .text_for_range(Point::new(row, 0)..Point::new(row, buffer.line_len(row)))
            .collect();
        let line = line.trim_end();
        if line.contains('{') || line.ends_with(':') || line.ends_with(';') {
            break;
        }
        row += 1;
    }
    start..row + 1
}

fn overlaps(a: &Range<u32>, b: &Range<u32>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Each hunk of one file with its chips. `functions` are the review's changed functions with their
/// indices; `moves` are blocks moved anywhere in the pull request.
pub fn classify_file(
    path: &str,
    buffer: &BufferSnapshot,
    diff: &BufferDiffSnapshot,
    functions: &[(usize, ChangedFunction)],
    moves: &[Move],
) -> Vec<HunkMeaning> {
    let base = diff.base_text();
    diff.hunks(&buffer.text)
        .map(|hunk| {
            let adds = !hunk.range.is_empty();
            let rows = hunk.range.start.row..if adds {
                hunk.range.end.row.max(hunk.range.start.row + 1)
            } else {
                hunk.range.start.row + 1
            };
            let base_range = hunk.diff_base_byte_range.clone();
            let base_start = base.offset_to_point(base_range.start);
            let base_end = base.offset_to_point(base_range.end);
            let base_rows = base_start.row..base_end.row.max(base_start.row);
            let head_range =
                buffer.point_to_offset(hunk.range.start)..buffer.point_to_offset(hunk.range.end);
            let function = functions
                .iter()
                .filter(|(_, function)| {
                    function.path == path
                        && function.range.start.row <= rows.start
                        && rows.start <= function.range.end.row
                })
                .max_by_key(|(_, function)| function.range.start)
                .map(|(index, _)| *index);
            let mut chips = Vec::new();
            let moved = moves.iter().find(|moved| {
                let to = moved.to_line - 1..moved.to_line - 1 + moved.lines;
                let from = moved.from_line - 1..moved.from_line - 1 + moved.lines;
                (moved.to_path == path && adds && overlaps(&to, &rows))
                    || (moved.from_path == path && overlaps(&from, &base_rows))
            });
            if let Some(moved) = moved {
                chips.push(Chip::Moved {
                    from_line: moved.from_line,
                    to_line: moved.to_line,
                });
            } else {
                let old_text: String = base.text_for_range(base_range.clone()).collect();
                let new_text: String = buffer.text_for_range(head_range.clone()).collect();
                let old_tokens = tokens(base, base_range);
                let new_tokens = tokens(buffer, head_range);
                let kind = match (&old_tokens, &new_tokens) {
                    (Some(old_tokens), Some(new_tokens))
                        if !old_tokens.is_empty() || !new_tokens.is_empty() =>
                    {
                        let (old_code, new_code) = (code(old_tokens), code(new_tokens));
                        if old_code == new_code {
                            // Equal code with different comments says nothing about behaviour.
                            let comments = |tokens: &[Token]| {
                                tokens
                                    .iter()
                                    .filter(|token| token.comment)
                                    .map(|token| token.text.clone())
                                    .collect::<Vec<_>>()
                            };
                            (comments(old_tokens) == comments(new_tokens))
                                .then_some(Chip::FormattingOnly)
                        } else if let Some((old, new, places)) = single_rename(&old_code, &new_code)
                        {
                            Some(Chip::RenameOnly { old, new, places })
                        } else {
                            Some(Chip::BehaviourChange)
                        }
                    }
                    _ if squash(&old_text) == squash(&new_text) => Some(Chip::FormattingOnly),
                    _ => None,
                };
                match kind {
                    Some(Chip::BehaviourChange) => {
                        let header = function
                            .and_then(|index| functions.iter().find(|(each, _)| *each == index))
                            .filter(|(_, function)| function.change == FunctionChange::Signature)
                            .map(|(_, function)| header_rows(function, buffer));
                        match header {
                            Some(header) if adds && overlaps(&header, &rows) => {
                                chips.push(Chip::SignatureChanged);
                                if rows.start < header.start || header.end < rows.end {
                                    chips.push(Chip::BehaviourChange);
                                }
                            }
                            _ => chips.push(Chip::BehaviourChange),
                        }
                    }
                    Some(chip) => chips.push(chip),
                    None => {}
                }
            }
            HunkMeaning {
                path: path.to_string(),
                rows,
                adds,
                base_rows,
                chips,
                function,
                since_viewed: SinceViewed::NotViewed,
            }
        })
        .collect()
}

/// Head rows that differ from the text the user viewed: inserted or changed rows, and the row
/// where something was deleted.
pub fn rows_changed_since(viewed: &str, head: &str) -> HashSet<u32> {
    let viewed: Vec<&str> = viewed.lines().collect();
    let head: Vec<&str> = head.lines().collect();
    let mut rows = HashSet::new();
    for op in capture_diff_slices(Algorithm::Myers, &viewed, &head) {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { new_index, .. } => {
                rows.insert(new_index as u32);
            }
            DiffOp::Insert { .. } | DiffOp::Replace { .. } => {
                rows.extend(op.new_range().map(|row| row as u32));
            }
        }
    }
    rows
}

/// Marks each hunk new or reviewed against the rows changed since the viewed head.
pub fn mark_since_viewed(hunks: &mut [HunkMeaning], changed: &HashSet<u32>) {
    for hunk in hunks {
        let new: Vec<u32> = hunk
            .rows
            .clone()
            .filter(|row| changed.contains(row))
            .collect();
        hunk.since_viewed = if new.is_empty() {
            SinceViewed::Reviewed
        } else {
            SinceViewed::New(new)
        };
    }
}

/// A test function, by name or by the module or file it sits in.
pub fn is_test_function(function: &ChangedFunction) -> bool {
    let name = function.name.to_lowercase();
    name.starts_with("test")
        || name.ends_with("_test")
        || function
            .outline_path
            .iter()
            .rev()
            .skip(1)
            .any(|segment| matches!(segment.as_str(), "tests" | "test"))
        || crate::classify_path(&function.path) == crate::FileKind::Test
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(chips: Vec<Chip>) -> HunkMeaning {
        HunkMeaning {
            path: "a.rs".into(),
            rows: 0..1,
            adds: true,
            base_rows: 0..1,
            chips,
            function: None,
            since_viewed: SinceViewed::NotViewed,
        }
    }

    #[test]
    fn summary_counts_each_hunk_once() {
        let hunks = [
            hunk(vec![Chip::SignatureChanged, Chip::BehaviourChange]),
            hunk(vec![Chip::RenameOnly {
                old: "a".into(),
                new: "b".into(),
                places: 2,
            }]),
            hunk(vec![Chip::Moved {
                from_line: 1,
                to_line: 9,
            }]),
        ];
        assert_eq!(
            file_summary(&hunks).as_deref(),
            Some("3 hunks: 1 behaviour change, 1 rename, 1 moved")
        );
        assert_eq!(file_summary(&[]), None);
    }

    #[test]
    fn rows_changed_since_viewing() {
        let viewed = "a\nb\nc\nd\n";
        let head = "a\nB\nc\nnew\nd\n";
        let mut rows: Vec<u32> = rows_changed_since(viewed, head).into_iter().collect();
        rows.sort();
        assert_eq!(rows, [1, 3]);
    }
}
