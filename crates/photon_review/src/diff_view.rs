use std::collections::{HashMap, HashSet};
use std::ops::Range;

use similar::{Algorithm, DiffOp, capture_diff_slices};

/// One file of the review as text: `old` is `None` for an added file.
pub struct ViewFile<'a> {
    pub path: &'a str,
    pub old: Option<&'a str>,
    pub new: &'a str,
}

/// A block deleted in one place and added unchanged (apart from indentation) in another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Move {
    pub from_path: String,
    pub from_line: u32,
    pub to_path: String,
    pub to_line: u32,
    pub lines: u32,
}

pub struct SimplifiedView {
    /// A stand-in base text per file, in which hidden changes count as unchanged.
    pub bases: Vec<Option<String>>,
    pub moves: Vec<Move>,
}

/// Fewer non-blank lines than this are too common to call a move (braces, `return Ok(())`).
const MIN_MOVED_LINES: usize = 3;

fn whitespace_key(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// The editor diffs each file against the base returned here, so whitespace-only edits (when
/// `hide_whitespace`) and moved blocks (when `hide_moves`) drop out of the shown diff.
pub fn simplified_bases(
    files: &[ViewFile],
    hide_whitespace: bool,
    hide_moves: bool,
) -> SimplifiedView {
    let texts: Vec<(Vec<&str>, Vec<&str>)> = files
        .iter()
        .map(|file| (lines(file.old.unwrap_or("")), lines(file.new)))
        .collect();
    let key = |line: &str| {
        if hide_whitespace {
            whitespace_key(line)
        } else {
            line.trim_end_matches(['\n', '\r']).to_string()
        }
    };
    let ops: Vec<Vec<DiffOp>> = texts
        .iter()
        .map(|(old, new)| {
            let old: Vec<String> = old.iter().map(|line| key(line)).collect();
            let new: Vec<String> = new.iter().map(|line| key(line)).collect();
            capture_diff_slices(Algorithm::Myers, &old, &new)
        })
        .collect();

    let mut moves = Vec::new();
    // (file, line) pairs taken by a move, on each side.
    let mut moved_old: HashSet<(usize, usize)> = HashSet::new();
    let mut moved_new: HashSet<(usize, usize)> = HashSet::new();
    if hide_moves {
        let removed_runs: Vec<(usize, Range<usize>)> = ops
            .iter()
            .enumerate()
            .flat_map(|(file, ops)| {
                ops.iter()
                    .filter(|op| !matches!(op, DiffOp::Equal { .. }))
                    .map(move |op| (file, op.old_range()))
            })
            .collect();
        let old_key = |file: usize, line: usize| whitespace_key(texts[file].0[line]);
        let new_key = |file: usize, line: usize| whitespace_key(texts[file].1[line]);
        let mut starts: HashMap<String, Vec<(usize, usize, usize)>> = HashMap::new();
        for (file, range) in &removed_runs {
            for line in range.clone() {
                let line_key = old_key(*file, line);
                if !line_key.is_empty() {
                    starts
                        .entry(line_key)
                        .or_default()
                        .push((*file, line, range.end));
                }
            }
        }
        for (file, ops) in ops.iter().enumerate() {
            for op in ops {
                if matches!(op, DiffOp::Equal { .. }) {
                    continue;
                }
                let added = op.new_range();
                let mut line = added.start;
                while line < added.end {
                    let mut best: Option<(usize, usize, usize)> = None;
                    for &(from_file, from_line, run_end) in
                        starts.get(&new_key(file, line)).into_iter().flatten()
                    {
                        let length = (0..)
                            .take_while(|offset| {
                                line + offset < added.end
                                    && from_line + offset < run_end
                                    && !moved_old.contains(&(from_file, from_line + offset))
                                    && old_key(from_file, from_line + offset)
                                        == new_key(file, line + offset)
                            })
                            .count();
                        if best.is_none_or(|(_, _, best_length)| length > best_length) {
                            best = Some((from_file, from_line, length));
                        }
                    }
                    let Some((from_file, from_line, length)) = best.filter(|(_, _, length)| {
                        (line..line + length)
                            .filter(|line| !new_key(file, *line).is_empty())
                            .count()
                            >= MIN_MOVED_LINES
                    }) else {
                        line += 1;
                        continue;
                    };
                    moved_old.extend((from_line..from_line + length).map(|line| (from_file, line)));
                    moved_new.extend((line..line + length).map(|line| (file, line)));
                    moves.push(Move {
                        from_path: files[from_file].path.to_string(),
                        from_line: from_line as u32 + 1,
                        to_path: files[file].path.to_string(),
                        to_line: line as u32 + 1,
                        lines: length as u32,
                    });
                    line += length;
                }
            }
        }
    }

    let bases = ops
        .iter()
        .enumerate()
        .map(|(file, ops)| {
            let (old, new) = &texts[file];
            let mut base = String::new();
            for op in ops {
                if let DiffOp::Equal { .. } = op {
                    base.extend(new[op.new_range()].iter().copied());
                    continue;
                }
                base.extend(
                    op.old_range()
                        .filter(|line| !moved_old.contains(&(file, *line)))
                        .map(|line| old[line]),
                );
                base.extend(
                    op.new_range()
                        .filter(|line| moved_new.contains(&(file, *line)))
                        .map(|line| new[line]),
                );
            }
            (files[file].old.is_some() || !base.is_empty()).then_some(base)
        })
        .collect();
    SimplifiedView { bases, moves }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file<'a>(path: &'a str, old: Option<&'a str>, new: &'a str) -> ViewFile<'a> {
        ViewFile { path, old, new }
    }

    #[test]
    fn reindented_and_trailing_space_lines_count_as_unchanged() {
        let old = "fn a() {\n    one();\n  two();  \n    three();\n}\n";
        let new = "fn a() {\n        one();\n  two();\n    three(4);\n}\n";
        let view = simplified_bases(&[file("a.rs", Some(old), new)], true, false);
        assert_eq!(
            view.bases[0].as_deref(),
            Some("fn a() {\n        one();\n  two();\n    three();\n}\n")
        );
        assert!(view.moves.is_empty());
        let same = simplified_bases(&[file("a.rs", Some(old), new)], false, false);
        assert_eq!(same.bases[0].as_deref(), Some(old));
    }

    #[test]
    fn spaces_inside_a_line_still_count() {
        let old = "call(a,b)\n";
        let new = "call(a, b)\n";
        let view = simplified_bases(&[file("a.rs", Some(old), new)], true, false);
        assert_eq!(view.bases[0].as_deref(), Some(old));
    }

    #[test]
    fn a_block_moved_between_files_is_hidden_and_listed() {
        let old_a = "keep\nfn moved() {\n    body();\n}\nend\n";
        let new_a = "keep\nend\n";
        let old_b = "top\n";
        let new_b = "top\n  fn moved() {\n      body();\n  }\n";
        let files = [
            file("a.rs", Some(old_a), new_a),
            file("b.rs", Some(old_b), new_b),
        ];
        let view = simplified_bases(&files, false, true);
        assert_eq!(view.bases[0].as_deref(), Some(new_a));
        assert_eq!(view.bases[1].as_deref(), Some(new_b));
        assert_eq!(
            view.moves,
            [Move {
                from_path: "a.rs".into(),
                from_line: 2,
                to_path: "b.rs".into(),
                to_line: 2,
                lines: 3,
            }]
        );
        assert!(simplified_bases(&files, false, false).moves.is_empty());
    }

    #[test]
    fn short_or_edited_moves_still_show() {
        let old = "x\na();\nb();\n\nmiddle\nc();\nd();\ne();\n";
        let new = "middle\na();\nb();\nc();\nd();\nE();\nx\n";
        let view = simplified_bases(&[file("a.rs", Some(old), new)], false, true);
        assert!(view.moves.is_empty(), "{:?}", view.moves);
        assert_eq!(view.bases[0].as_deref(), Some(old));
    }

    #[test]
    fn moved_into_a_new_file_keeps_it_a_new_file_only_when_nothing_moved() {
        let old_a = "one\ntwo\nthree\n";
        let files = [
            file("a.rs", Some(old_a), ""),
            file("b.rs", None, "one\ntwo\nthree\nfour\n"),
            file("c.rs", None, "new\n"),
        ];
        let view = simplified_bases(&files, false, true);
        assert_eq!(view.bases[0].as_deref(), Some(""));
        assert_eq!(view.bases[1].as_deref(), Some("one\ntwo\nthree\n"));
        assert_eq!(view.bases[2], None);
        assert_eq!(view.moves.len(), 1);
    }
}
