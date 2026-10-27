use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanKind {
    Superpowers,
    SpecKit,
    Kiro,
    OpenSpec,
    Checklist,
}

impl PlanKind {
    pub fn label(self) -> &'static str {
        match self {
            PlanKind::Superpowers => "Superpowers plan",
            PlanKind::SpecKit => "Spec Kit tasks",
            PlanKind::Kiro => "Kiro tasks",
            PlanKind::OpenSpec => "OpenSpec tasks",
            PlanKind::Checklist => "Plan",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileRole {
    Create,
    Modify,
    Test,
    Mention,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRef {
    pub path: String,
    pub role: FileRole,
}

/// The overview or one task: where it sits in the plan and what it claims.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Section {
    pub key: String,
    /// "3" for "Task 3", "T001" for Spec Kit.
    pub number: Option<String>,
    pub title: String,
    /// Byte range in the plan, heading included.
    pub range: Range<usize>,
    /// Paragraphs, list runs, headings and code blocks, each commentable on its own.
    pub blocks: Vec<Range<usize>>,
    pub files: Vec<FileRef>,
    /// Files the task names as tests, which `files` may list under a stronger role.
    pub test_files: Vec<String>,
    /// Interfaces the task defines, as written.
    pub produces: Vec<String>,
    /// The names it defines, last path segment only, for matching uses.
    pub produced_names: Vec<String>,
    /// Identifiers it depends on that it doesn't define itself.
    pub uses: Vec<String>,
    pub tests: Vec<String>,
    pub commands: Vec<String>,
    pub decisions: Vec<String>,
    pub has_code: bool,
    /// The bold labels of the overview ("goal", "constraints").
    pub labels: Vec<String>,
    /// The first sentence of prose.
    pub summary: String,
}

impl Section {
    pub fn names_test(&self) -> bool {
        !self.tests.is_empty() || !self.commands.is_empty() || !self.test_files.is_empty()
    }

    pub fn adds_code(&self) -> bool {
        self.has_code
            || self.files.iter().any(|file| {
                matches!(file.role, FileRole::Create | FileRole::Modify) && !is_doc(&file.path)
            })
    }

    pub fn defines(&self, identifier: &str) -> bool {
        let name = last_segment(identifier);
        self.produced_names.iter().any(|produced| produced == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub title: Option<String>,
    pub kind: PlanKind,
    pub overview: Section,
    pub tasks: Vec<Section>,
    /// The spec the plan names, as written in it.
    pub spec_path: Option<String>,
}

impl Plan {
    /// The overview, then the tasks.
    pub fn sections(&self) -> impl Iterator<Item = &Section> {
        std::iter::once(&self.overview).chain(&self.tasks)
    }

    /// The task that defines `identifier`, by index.
    pub fn definer(&self, identifier: &str) -> Option<usize> {
        self.tasks.iter().position(|task| task.defines(identifier))
    }
}

/// The number of tasks, when `source` reads as a plan.
pub fn plan_task_count(source: &str) -> Option<usize> {
    let lines = lines(source);
    find_tasks(source, &lines).map(|(_, tasks)| tasks.len())
}

pub fn parse_plan(source: &str) -> Option<Plan> {
    let lines = lines(source);
    let (kind, spans) = find_tasks(source, &lines)?;
    let title_line = lines
        .iter()
        .find(|line| heading(line).is_some_and(|(level, _)| level == 1));
    let title = title_line
        .and_then(|line| heading(line))
        .map(|(_, text)| clean_title(text))
        .filter(|title| !title.is_empty());
    let first_task = spans.first().map_or(source.len(), |span| span.range.start);
    let overview_start = title_line
        .map(|line| line.end)
        .filter(|end| *end <= first_task)
        .unwrap_or(0);
    let mut overview = analyze(source, &lines, overview_start..first_task);
    overview.key = "overview".into();
    overview.title = "Overview".into();
    let tasks: Vec<Section> = spans
        .into_iter()
        .map(|span| {
            let mut section = analyze(source, &lines, span.range);
            section.key = span.key;
            section.number = span.number;
            section.title = span.title;
            section
        })
        .collect();
    let spec_path = overview
        .files
        .iter()
        .map(|file| file.path.as_str())
        .find(|path| {
            let lower = path.to_lowercase();
            lower.ends_with(".md")
                && ["spec", "design", "requirements", "proposal"]
                    .iter()
                    .any(|word| lower.contains(word))
        })
        .map(str::to_string);
    Some(Plan {
        title,
        kind,
        overview,
        tasks,
        spec_path,
    })
}

/// The headings of a spec that read as requirements: "Requirement …" headings when it has them,
/// otherwise every H2 and H3 that isn't boilerplate.
pub fn requirement_headings(spec: &str) -> Vec<String> {
    const BOILERPLATE: &[&str] = &[
        "overview",
        "summary",
        "goals",
        "non-goals",
        "context",
        "background",
        "rules",
        "testing",
        "open questions",
        "not in this round",
        "references",
        "glossary",
        "introduction",
        "requirements",
    ];
    let lines = lines(spec);
    let headings: Vec<String> = lines
        .iter()
        .filter_map(|line| heading(line))
        .filter(|(level, _)| matches!(level, 2 | 3))
        .map(|(_, text)| strip_markup(text))
        .collect();
    let requirements: Vec<String> = headings
        .iter()
        .filter(|text| text.to_lowercase().starts_with("requirement "))
        .cloned()
        .collect();
    if !requirements.is_empty() {
        return requirements;
    }
    headings
        .into_iter()
        .filter(|text| !BOILERPLATE.contains(&text.to_lowercase().as_str()))
        .collect()
}

/// Words of a requirement worth looking for in tasks.
pub fn significant_words(text: &str) -> Vec<String> {
    const COMMON: &[&str] = &[
        "requirement",
        "with",
        "that",
        "this",
        "from",
        "into",
        "when",
        "they",
        "their",
        "have",
        "should",
        "must",
        "will",
        "user",
        "story",
    ];
    text.split(|character: char| !character.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| word.len() >= 4 && !COMMON.contains(&word.as_str()))
        .collect()
}

pub fn last_segment(identifier: &str) -> &str {
    identifier.rsplit("::").next().unwrap_or(identifier)
}

fn is_doc(path: &str) -> bool {
    let lower = path.to_lowercase();
    [".md", ".mdx", ".txt", ".rst", ".adoc"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

struct Line<'a> {
    start: usize,
    /// After the line break.
    end: usize,
    text: &'a str,
    /// Inside a fenced code block, or one of its fences.
    fenced: bool,
    fence: bool,
}

fn lines(source: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut open_fence: Option<char> = None;
    for raw in source.split_inclusive('\n') {
        let text = raw.trim_end_matches(['\n', '\r']);
        let trimmed = text.trim_start();
        let marker = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        let fence = match (open_fence, marker) {
            (None, Some(marker)) => {
                open_fence = Some(marker);
                true
            }
            (Some(open), Some(marker)) if open == marker => {
                open_fence = None;
                true
            }
            _ => false,
        };
        lines.push(Line {
            start,
            end: start + raw.len(),
            text,
            fenced: fence || open_fence.is_some(),
            fence,
        });
        start += raw.len();
    }
    lines
}

fn heading<'a>(line: &Line<'a>) -> Option<(usize, &'a str)> {
    if line.fenced {
        return None;
    }
    let trimmed = line.text.trim_start();
    let level = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    Some((level, rest.trim().trim_end_matches('#').trim()))
}

struct TaskSpan {
    key: String,
    number: Option<String>,
    title: String,
    range: Range<usize>,
}

static SUPERPOWERS_TASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[Tt]ask|TASK)[ \t]+([0-9]+)[ \t]*[:.)\-–—][ \t]*(.+)$").expect("valid regex")
});
static SPEC_KIT_TASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t]*[-*][ \t]+\[[ xX]\][ \t]+(T[0-9]{3,})(?-u:\b)[ \t]*(.*)$")
        .expect("valid regex")
});
static KIRO_TASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[-*][ \t]+\[[ xX]\]\*?[ \t]+([0-9]+)\.[ \t]+(.+)$").expect("valid regex")
});
static OPENSPEC_SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]+)\.[ \t]+(.+)$").expect("valid regex"));
static OPENSPEC_ITEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t]*[-*][ \t]+\[[ xX]\][ \t]+[0-9]+\.[0-9]+").expect("valid regex")
});
static CHECKBOX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[ \t]*[-*][ \t]+\[[ xX]\]").expect("valid regex"));
static SPEC_KIT_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[[^\]]*\][ \t]*").expect("valid regex"));

fn find_tasks(source: &str, lines: &[Line]) -> Option<(PlanKind, Vec<TaskSpan>)> {
    let superpowers = heading_tasks(source, lines, |level, text| {
        let captures = SUPERPOWERS_TASK.captures(text)?;
        (level >= 2).then(|| {
            let number = captures[1].to_string();
            (
                format!("task-{number}"),
                Some(number),
                strip_markup(&captures[2]),
            )
        })
    });
    if !superpowers.is_empty() {
        return Some((PlanKind::Superpowers, superpowers));
    }

    let spec_kit = list_tasks(source, lines, true, |text| {
        let captures = SPEC_KIT_TASK.captures(text)?;
        let id = captures[1].to_string();
        let mut title = captures[2].trim();
        while let Some(marker) = SPEC_KIT_MARKER.find(title) {
            title = &title[marker.end()..];
        }
        Some((id.clone(), Some(id), strip_markup(title)))
    });
    if !spec_kit.is_empty() {
        return Some((PlanKind::SpecKit, spec_kit));
    }

    let has_numbered_items = lines
        .iter()
        .any(|line| !line.fenced && OPENSPEC_ITEM.is_match(line.text));
    if has_numbered_items {
        let openspec = heading_tasks(source, lines, |level, text| {
            let captures = OPENSPEC_SECTION.captures(text)?;
            (level == 2).then(|| {
                let number = captures[1].to_string();
                (number.clone(), Some(number), strip_markup(&captures[2]))
            })
        });
        if !openspec.is_empty() {
            return Some((PlanKind::OpenSpec, openspec));
        }
    }

    let kiro = list_tasks(source, lines, false, |text| {
        let captures = KIRO_TASK.captures(text)?;
        let number = captures[1].to_string();
        Some((number.clone(), Some(number), strip_markup(&captures[2])))
    });
    if kiro.len() >= 2 {
        return Some((PlanKind::Kiro, kiro));
    }

    let checklist = checklist_tasks(source, lines);
    (checklist.len() >= 2).then_some((PlanKind::Checklist, checklist))
}

/// Tasks that are headings; each runs to the next heading at its level or above.
fn heading_tasks(
    source: &str,
    lines: &[Line],
    task: impl Fn(usize, &str) -> Option<(String, Option<String>, String)>,
) -> Vec<TaskSpan> {
    let mut tasks = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some((level, text)) = heading(line) else {
            continue;
        };
        let Some((key, number, title)) = task(level, text) else {
            continue;
        };
        let end = lines[index + 1..]
            .iter()
            .find(|line| heading(line).is_some_and(|(other, _)| other <= level))
            .map_or(source.len(), |line| line.start);
        tasks.push(TaskSpan {
            key,
            number,
            title,
            range: line.start..trim_end(source, line.start..end),
        });
    }
    tasks
}

/// Tasks that are list items. `indented_only` keeps only indented continuation lines (Spec Kit);
/// otherwise a task runs to the next task or heading (Kiro, whose subtasks sit unindented).
fn list_tasks(
    source: &str,
    lines: &[Line],
    indented_only: bool,
    task: impl Fn(&str) -> Option<(String, Option<String>, String)>,
) -> Vec<TaskSpan> {
    let mut tasks = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.fenced {
            continue;
        }
        let Some((key, number, title)) = task(line.text) else {
            continue;
        };
        let mut end = line.end;
        for next in &lines[index + 1..] {
            if heading(next).is_some() || (!next.fenced && task(next.text).is_some()) {
                break;
            }
            if indented_only {
                let continues = next.text.starts_with([' ', '\t']) && !next.text.trim().is_empty();
                if !continues {
                    break;
                }
            }
            end = next.end;
        }
        tasks.push(TaskSpan {
            key,
            number,
            title,
            range: line.start..trim_end(source, line.start..end),
        });
    }
    tasks
}

/// H2 and H3 sections whose own text holds a checklist.
fn checklist_tasks(source: &str, lines: &[Line]) -> Vec<TaskSpan> {
    let mut tasks = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some((level, text)) = heading(line) else {
            continue;
        };
        if !matches!(level, 2 | 3) {
            continue;
        }
        let rest = &lines[index + 1..];
        let own = rest
            .iter()
            .position(|line| heading(line).is_some_and(|(other, _)| other <= 3))
            .unwrap_or(rest.len());
        if !rest[..own]
            .iter()
            .any(|line| !line.fenced && CHECKBOX.is_match(line.text))
        {
            continue;
        }
        let end = rest.get(own).map_or(source.len(), |line| line.start);
        let title = strip_markup(text);
        tasks.push(TaskSpan {
            key: slug(&title),
            number: Some((tasks.len() + 1).to_string()),
            title,
            range: line.start..trim_end(source, line.start..end),
        });
    }
    tasks
}

/// Drops trailing blank lines and thematic breaks.
fn trim_end(source: &str, range: Range<usize>) -> usize {
    let text = &source[range.clone()];
    let mut end = text.len();
    loop {
        let trimmed = text[..end].trim_end();
        if let Some(stripped) = trimmed.strip_suffix("---")
            && (stripped.is_empty() || stripped.ends_with('\n'))
        {
            end = stripped.len();
            continue;
        }
        end = trimmed.len();
        break;
    }
    range.start + end
}

fn slug(text: &str) -> String {
    text.to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn clean_title(text: &str) -> String {
    let title = strip_markup(text);
    if title.eq_ignore_ascii_case("implementation plan") || title.eq_ignore_ascii_case("tasks") {
        return String::new();
    }
    for suffix in [" Implementation Plan", " implementation plan", " Plan"] {
        if let Some(stripped) = title.strip_suffix(suffix)
            && !stripped.trim().is_empty()
        {
            return stripped.trim().to_string();
        }
    }
    title.strip_prefix("Tasks: ").unwrap_or(&title).to_string()
}

fn strip_markup(text: &str) -> String {
    text.replace("**", "")
        .replace("__", "")
        .replace('`', "")
        .trim()
        .to_string()
}

static CODE_SPAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`([^`]+)`").expect("valid regex"));
static FILES_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[ \t]*(?:[-*][ \t]+)?(?:\*\*|__)?(?:[Ff]iles|FILES)(?::(?:\*\*|__)?|(?:\*\*|__)?:)",
    )
    .expect("valid regex")
});
static PRODUCES_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[ \t]*(?:[-*][ \t]+)?(?:\*\*|__)?(?:[Pp]roduces|[Ii]nterfaces?|[Ee]xports?)(?::(?:\*\*|__)?|(?:\*\*|__)?:)",
    )
    .expect("valid regex")
});
static DECISION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[ \t]*(?:[-*][ \t]+)?(?:\*\*|__)?(?:[Dd]ecision|DECISION|[Rr]uling|RULING)(?::(?:\*\*|__)?|(?:\*\*|__)?:)[ \t]*(.+)$",
    )
    .expect("valid regex")
});
static LABEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\*\*([A-Za-z][A-Za-z -]*?):?\*\*:?").expect("valid regex"));
static ROLE_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i-u)(?-u:\b)(create|creates|new|add|adds|modify|modifies|update|updates|edit|change|test|tests)(?-u:\b)")
        .expect("valid regex")
});
static PLAIN_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[ \t(])((?:[A-Za-z0-9_.-]+/)+[A-Za-z0-9_-]+\.[A-Za-z0-9]+)")
        .expect("valid regex")
});
static LINE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r":[0-9]+(?:-[0-9]+)?$").expect("valid regex"));
static IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*").expect("valid regex")
});
static DEFINITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:pub(?:\([^)]*\))?[ \t]+)?(?:export[ \t]+)?(?:default[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?(?:const[ \t]+)?(?:fn|def|function|func(?:[ \t]*\([^)]*\))?|struct|enum|trait|type|class|interface)[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
    )
    .expect("valid regex")
});
static JS_TEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(?:it|test)\([ \t]*["'`]([^"'`]+)["'`]"#).expect("valid regex")
});

const KNOWN_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "kt", "rb", "md", "toml", "json", "yaml",
    "yml", "swift", "c", "h", "cc", "cpp", "cs", "sql", "sh", "css", "scss", "html", "lock", "ex",
    "exs", "php", "vue", "svelte", "proto", "graphql",
];

const COMMON_WORDS: &[&str] = &[
    "Self", "Some", "None", "Ok", "Err", "Result", "Option", "String", "Vec", "HashMap", "HashSet",
    "Box", "Arc", "Rc", "Default", "Clone", "Debug", "Duration", "Future", "Send", "Sync", "True",
    "False", "Null", "Error", "Promise", "Array", "Object", "Map", "Set",
];

fn normalize_path(span: &str) -> Option<String> {
    let path = span.trim();
    if path.is_empty()
        || path.contains(char::is_whitespace)
        || path.contains("::")
        || path.contains('(')
        || path.contains("://")
        || path.ends_with('/')
    {
        return None;
    }
    let path = LINE_SUFFIX.replace(path, "");
    let path = path.strip_prefix("./").unwrap_or(&path);
    let file_name = path.rsplit('/').next()?;
    let (_, extension) = file_name.rsplit_once('.')?;
    if extension.is_empty() || !extension.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let known = KNOWN_EXTENSIONS.contains(&extension.to_lowercase().as_str());
    (known || (path.contains('/') && !extension.chars().all(|c| c.is_ascii_lowercase())))
        .then(|| path.to_string())
}

fn is_command(span: &str) -> bool {
    const RUNNERS: &[&str] = &[
        "cargo", "npm", "pnpm", "yarn", "bun", "deno", "npx", "pytest", "python", "go", "make",
        "just", "mvn", "gradle", "./", "uv", "dotnet", "mix", "rspec", "bundle",
    ];
    let first = span.split_whitespace().next().unwrap_or_default();
    span.contains(' ') && RUNNERS.iter().any(|runner| first.starts_with(runner))
}

fn is_identifier(token: &str, called: bool) -> bool {
    if COMMON_WORDS.contains(&token) {
        return false;
    }
    let name = last_segment(token);
    if token.contains("::") || name.contains('_') && name.trim_matches('_').len() >= 2 {
        return !name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit());
    }
    let first = name.chars().next().unwrap_or_default();
    let has_lower = name.chars().any(|c| c.is_ascii_lowercase());
    let inner_upper = name.chars().skip(1).any(|c| c.is_ascii_uppercase());
    if first.is_ascii_uppercase() {
        return has_lower && name.len() >= 3;
    }
    (inner_upper && has_lower) || (called && name.len() >= 3)
}

fn identifiers_in(span: &str) -> Vec<String> {
    let bytes = span.as_bytes();
    IDENTIFIER
        .find_iter(span)
        .filter(|found| {
            let before = found.start().checked_sub(1).map(|index| bytes[index]);
            let after = bytes.get(found.end()).copied();
            if matches!(before, Some(b'.' | b'\'' | b'"' | b'$' | b'-' | b'/'))
                || after == Some(b'!')
            {
                return false;
            }
            is_identifier(found.as_str(), after == Some(b'('))
        })
        .map(|found| found.as_str().to_string())
        .collect()
}

/// A span after "Run:" and before "Expected:" on its line.
fn run_command(line: &str, position: usize) -> bool {
    line.find("Run:").is_some_and(|run| run < position)
        && line
            .find("Expected:")
            .is_none_or(|expected| position < expected)
}

fn role_for(word: &str) -> FileRole {
    match word.to_lowercase().as_str() {
        "create" | "creates" | "new" | "add" | "adds" => FileRole::Create,
        "test" | "tests" => FileRole::Test,
        _ => FileRole::Modify,
    }
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

fn add_file(section: &mut Section, path: String, role: FileRole) {
    if role == FileRole::Test {
        push_unique(&mut section.test_files, path.clone());
    }
    match section.files.iter_mut().find(|file| file.path == path) {
        Some(file) => file.role = file.role.min(role),
        None => section.files.push(FileRef { path, role }),
    }
}

fn first_sentence(text: &str) -> String {
    let text = strip_markup(text);
    let sentence = text
        .find(". ")
        .map_or(text.as_str(), |end| &text[..end])
        .trim_end_matches('.');
    if sentence.chars().count() > 100 {
        let cut: String = sentence.chars().take(99).collect();
        format!("{}…", cut.trim_end())
    } else {
        sentence.to_string()
    }
}

/// Reads what a section claims: files, interfaces, identifiers, tests and decisions.
fn analyze(source: &str, all_lines: &[Line], range: Range<usize>) -> Section {
    let mut section = Section {
        range: range.clone(),
        ..Section::default()
    };
    let lines: Vec<&Line> = all_lines
        .iter()
        .filter(|line| line.start >= range.start && line.start < range.end)
        .collect();

    let mut block: Option<Range<usize>> = None;
    let mut in_code = false;
    let flush = |block: &mut Option<Range<usize>>, blocks: &mut Vec<Range<usize>>| {
        if let Some(block) = block.take() {
            blocks.push(block.start..trim_end(source, block));
        }
    };
    for line in &lines {
        let end = line.end.min(range.end);
        if in_code {
            if let Some(block) = &mut block {
                block.end = end;
            }
            if line.fence {
                in_code = false;
                flush(&mut block, &mut section.blocks);
            }
            continue;
        }
        if line.fence {
            flush(&mut block, &mut section.blocks);
            block = Some(line.start..end);
            in_code = true;
            continue;
        }
        if line.text.trim().is_empty() {
            flush(&mut block, &mut section.blocks);
            continue;
        }
        if heading(line).is_some() {
            flush(&mut block, &mut section.blocks);
            section
                .blocks
                .push(line.start..trim_end(source, line.start..end));
            continue;
        }
        match &mut block {
            Some(block) => block.end = end,
            None => block = Some(line.start..end),
        }
    }
    flush(&mut block, &mut section.blocks);

    let mut code: Option<(String, Vec<&str>)> = None;
    let mut code_signatures = Vec::new();
    let mut explicit_produces = false;
    let mut in_files = false;
    let mut in_produces = false;
    for line in &lines {
        if line.fenced {
            if line.fence {
                match code.take() {
                    Some((language, body)) => {
                        scan_code(&language, &body, &mut section, &mut code_signatures)
                    }
                    None => {
                        let language = line
                            .text
                            .trim()
                            .trim_start_matches(['`', '~'])
                            .trim()
                            .to_lowercase();
                        code = Some((language, Vec::new()));
                    }
                }
            } else if let Some((_, body)) = &mut code {
                body.push(line.text);
            }
            continue;
        }
        let text = line.text;
        if text.trim().is_empty() {
            in_files = false;
            in_produces = false;
            continue;
        }
        let is_list =
            text.trim_start().starts_with(['-', '*']) && !text.trim_start().starts_with("**");
        let files_label = FILES_LABEL.find(text);
        let produces_label = PRODUCES_LABEL.find(text);
        if files_label.is_some() {
            in_files = true;
            in_produces = false;
        } else if produces_label.is_some() {
            in_produces = true;
            in_files = false;
            explicit_produces = true;
        } else if !is_list {
            in_files = false;
            in_produces = false;
        }

        if let Some(captures) = DECISION.captures(text) {
            let decision = strip_markup(&captures[1]);
            push_unique(
                &mut section.decisions,
                decision.trim_end_matches('.').to_string(),
            );
        }
        if let Some(captures) = LABEL.captures(text) {
            let label = captures[1].trim().to_lowercase();
            const OVERVIEW_LABELS: &[&str] = &[
                "goal",
                "goals",
                "architecture",
                "tech stack",
                "constraints",
                "scope",
                "context",
                "non-goals",
            ];
            if OVERVIEW_LABELS.contains(&label.as_str()) {
                push_unique(&mut section.labels, label);
            }
        }

        let skip_identifiers =
            text.contains("Run:") || text.contains("Expected:") || in_files || in_produces;
        let mut previous_end = files_label.map_or(0, |label| label.end());
        for captures in CODE_SPAN.captures_iter(text) {
            let (Some(whole), Some(inner)) = (captures.get(0), captures.get(1)) else {
                continue;
            };
            let span = inner.as_str();
            if let Some(path) = normalize_path(span) {
                let role = if in_files {
                    ROLE_WORD
                        .find_iter(&text[previous_end.min(whole.start())..whole.start()])
                        .last()
                        .map_or(FileRole::Modify, |word| role_for(word.as_str()))
                } else {
                    FileRole::Mention
                };
                add_file(&mut section, path, role);
            } else if is_command(span) || run_command(text, whole.start()) {
                push_unique(&mut section.commands, span.trim().to_string());
            } else if in_produces {
                let signature = span.trim().to_string();
                if let Some(name) = IDENTIFIER.find(&signature) {
                    push_unique(
                        &mut section.produced_names,
                        last_segment(name.as_str()).to_string(),
                    );
                }
                push_unique(&mut section.produces, signature);
            } else if !skip_identifiers {
                for identifier in identifiers_in(span) {
                    push_unique(&mut section.uses, identifier);
                }
            }
            previous_end = whole.end();
        }
        let without_spans = CODE_SPAN.replace_all(text, " ");
        for captures in PLAIN_PATH.captures_iter(&without_spans) {
            if let Some(path) = normalize_path(&captures[1]) {
                add_file(&mut section, path, FileRole::Mention);
            }
        }

        let trimmed = text.trim_start();
        let is_prose = !CHECKBOX.is_match(text)
            && !trimmed.starts_with('>')
            && !trimmed.starts_with('|')
            && !LABEL.is_match(trimmed)
            && !in_files
            && !in_produces
            && heading(line).is_none()
            && !trimmed.starts_with("Run:");
        if section.summary.is_empty() && is_prose {
            section.summary = first_sentence(trimmed.trim_start_matches(['-', '*', ' ']));
        }
    }
    if let Some((language, body)) = code.take() {
        scan_code(&language, &body, &mut section, &mut code_signatures);
    }
    if !explicit_produces {
        section.produces = code_signatures;
    }
    let produced = section.produced_names.clone();
    section
        .uses
        .retain(|identifier| !produced.iter().any(|name| name == last_segment(identifier)));
    section
}

/// Signatures and test names from one fenced code block.
fn scan_code(language: &str, body: &[&str], section: &mut Section, signatures: &mut Vec<String>) {
    const SHELL: &[&str] = &[
        "sh",
        "bash",
        "shell",
        "zsh",
        "console",
        "fish",
        "powershell",
    ];
    const PROSE: &[&str] = &[
        "", "text", "txt", "md", "markdown", "plain", "output", "diff", "json", "yaml", "toml",
    ];
    let language = language.split([' ', ',', '{']).next().unwrap_or_default();
    if SHELL.contains(&language) {
        for line in body {
            let command = line.trim().trim_start_matches("$ ").trim();
            if command.contains("test") {
                push_unique(&mut section.commands, command.to_string());
            }
        }
        return;
    }
    if PROSE.contains(&language) {
        return;
    }
    section.has_code = true;
    let mut test_attribute = false;
    for (index, line) in body.iter().enumerate() {
        let trimmed = line.trim();
        if (trimmed.starts_with("#[") && trimmed.contains("test"))
            || trimmed.starts_with("@Test")
            || trimmed.starts_with("@pytest")
        {
            test_attribute = true;
            continue;
        }
        if let Some(captures) = JS_TEST.captures(trimmed) {
            push_unique(&mut section.tests, captures[1].to_string());
            continue;
        }
        if let Some(captures) = DEFINITION.captures(trimmed) {
            let name = captures[1].to_string();
            let is_test = test_attribute
                || name.starts_with("test_")
                || (trimmed.starts_with("func") && name.starts_with("Test"));
            test_attribute = false;
            if is_test {
                push_unique(&mut section.tests, name);
                continue;
            }
            if name.starts_with("__") {
                continue;
            }
            push_unique(&mut section.produced_names, name);
            push_unique(signatures, signature_at(&body[index..]));
            continue;
        }
        if !trimmed.is_empty() && !trimmed.starts_with("#[") && !trimmed.starts_with('@') {
            test_attribute = false;
        }
    }
}

/// A definition's signature, joined across lines up to its body.
fn signature_at(lines: &[&str]) -> String {
    let mut signature = String::new();
    for line in lines.iter().take(8) {
        let line = line.trim();
        if line.starts_with("where") {
            break;
        }
        if !signature.is_empty() && !signature.ends_with('(') && !line.starts_with(')') {
            signature.push(' ');
        }
        let end = line.find(['{', ';']).unwrap_or(line.len());
        signature.push_str(line[..end].trim_end());
        if end < line.len() || line.ends_with(':') {
            break;
        }
    }
    signature
        .trim_end_matches(':')
        .trim_end_matches(',')
        .trim()
        .replace(",)", ")")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUPERPOWERS: &str = include_str!("../test_fixtures/superpowers.md");
    const SPEC_KIT: &str = include_str!("../test_fixtures/spec_kit_tasks.md");
    const KIRO: &str = include_str!("../test_fixtures/kiro_tasks.md");
    const OPENSPEC: &str = include_str!("../test_fixtures/openspec_tasks.md");
    const CHECKLIST: &str = include_str!("../test_fixtures/checklist.md");
    const NOT_A_PLAN: &str = include_str!("../test_fixtures/not_a_plan.md");
    const SPEC: &str = include_str!("../test_fixtures/spec.md");

    fn files(section: &Section) -> Vec<(&str, FileRole)> {
        section
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.role))
            .collect()
    }

    fn text<'a>(source: &'a str, range: &Range<usize>) -> &'a str {
        &source[range.clone()]
    }

    #[test]
    fn superpowers_plan() {
        let plan = parse_plan(SUPERPOWERS).expect("a plan");
        assert_eq!(plan.kind, PlanKind::Superpowers);
        assert_eq!(
            plan.title.as_deref(),
            Some("Retry declined card charges with backoff")
        );
        assert_eq!(
            plan.spec_path.as_deref(),
            Some("docs/specs/2026-10-20-retry-charges-design.md")
        );
        assert_eq!(plan.overview.labels, ["goal", "constraints"]);
        assert_eq!(plan.overview.decisions, ["Declines are never retried"]);
        let titles: Vec<_> = plan.tasks.iter().map(|task| task.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Backoff type",
                "retry_with_backoff()",
                "Retry in Gateway::charge",
                "Idempotency key",
                "Nightly job picks up retries",
                "Metrics",
                "Docs",
            ]
        );

        let backoff = &plan.tasks[0];
        assert_eq!(backoff.key, "task-1");
        assert!(text(SUPERPOWERS, &backoff.range).starts_with("### Task 1: Backoff type"));
        assert!(text(SUPERPOWERS, &backoff.range).ends_with("caps at 2 s."));
        assert_eq!(
            files(backoff),
            [
                ("src/retry.rs", FileRole::Create),
                ("src/main.rs", FileRole::Modify)
            ]
        );
        assert_eq!(backoff.tests, ["delays_double_up_to_the_cap"]);
        assert_eq!(
            backoff.produces,
            [
                "pub struct Backoff",
                "pub fn delay(&self, attempt: u32) -> Duration"
            ]
        );
        assert!(backoff.uses.is_empty(), "{:?}", backoff.uses);

        let retry = &plan.tasks[1];
        assert_eq!(
            retry.produces,
            [
                "pub async fn retry_with_backoff<T, E, F, Fut>(backoff: Backoff, retryable: impl Fn(&E) -> bool, mut call: F) -> Result<T, E>"
            ]
        );
        assert_eq!(retry.uses, ["Backoff::delay"]);
        assert_eq!(
            retry.decisions,
            ["Jitter is added after the cap, on purpose"]
        );
        assert_eq!(retry.commands, ["cargo test gives_up_after_max_attempts"]);

        let charge = &plan.tasks[2];
        assert_eq!(
            files(charge),
            [
                ("src/payments.rs", FileRole::Modify),
                ("src/gateway.rs", FileRole::Modify),
                ("src/retry.rs", FileRole::Mention),
            ]
        );
        assert_eq!(charge.test_files, ["src/payments.rs"]);
        assert_eq!(
            charge.produces,
            ["Gateway::charge(&self, invoice: &Invoice) -> Result<Receipt, ChargeError>"]
        );
        assert_eq!(charge.uses, ["retry_with_backoff", "is_retryable"]);
        assert_eq!(charge.tests, ["retries_gateway_timeouts"]);
        assert_eq!(
            charge.commands,
            ["cargo test retries_gateway_timeouts", "cargo test"]
        );
        let blocks: Vec<_> = charge
            .blocks
            .iter()
            .map(|block| text(SUPERPOWERS, block))
            .collect();
        assert_eq!(blocks[0], "### Task 3: Retry in Gateway::charge");
        assert!(blocks[1].starts_with("**Files:**"));
        assert!(blocks.iter().any(|block| block.starts_with("```rust\n#[tokio::test]")
            && block.ends_with("```")));
        assert!(
            blocks
                .iter()
                .any(|block| block.starts_with("Replace the direct"))
        );

        let metrics = &plan.tasks[5];
        assert!(metrics.adds_code() && !metrics.names_test());
        assert_eq!(
            metrics.summary,
            "Metrics counter for retries, so the dashboard shows how often charges are retried"
        );
        let docs = &plan.tasks[6];
        assert!(!docs.adds_code());
        assert_eq!(plan.definer("retry_with_backoff"), Some(1));
        assert_eq!(plan.definer("Backoff::delay"), Some(0));
        assert_eq!(plan.definer("is_retryable"), None);
    }

    #[test]
    fn spec_kit_tasks() {
        let plan = parse_plan(SPEC_KIT).expect("a plan");
        assert_eq!(plan.kind, PlanKind::SpecKit);
        assert_eq!(plan.title.as_deref(), Some("Photo albums"));
        let ids: Vec<_> = plan
            .tasks
            .iter()
            .map(|task| task.number.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(
            ids,
            ["T001", "T002", "T003", "T004", "T005", "T006", "T007"]
        );
        assert_eq!(plan.tasks[1].title, "Configure linting in .eslintrc.json");
        assert_eq!(
            files(&plan.tasks[1]),
            [(".eslintrc.json", FileRole::Mention)]
        );
        assert_eq!(
            plan.tasks[2].title,
            "Contract test POST /api/albums in tests/contract/test_albums_post.py"
        );
        assert_eq!(
            files(&plan.tasks[2]),
            [("tests/contract/test_albums_post.py", FileRole::Mention)]
        );
        let service = &plan.tasks[5];
        assert!(text(SPEC_KIT, &service.range).ends_with("from src/models/album.py"));
        assert_eq!(service.uses, ["AlbumRepository"]);
        assert_eq!(
            files(service),
            [
                ("src/services/album_service.py", FileRole::Mention),
                ("src/models/album.py", FileRole::Mention),
            ]
        );
        assert!(!service.adds_code());
        assert_eq!(
            plan.tasks[6].decisions,
            Vec::<String>::new(),
            "the trailing decision is outside the task's list item"
        );
    }

    #[test]
    fn kiro_tasks() {
        let plan = parse_plan(KIRO).expect("a plan");
        assert_eq!(plan.kind, PlanKind::Kiro);
        assert_eq!(plan.title, None);
        let titles: Vec<_> = plan.tasks.iter().map(|task| task.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Set up project structure and core interfaces",
                "Implement data models",
                "Wire the API",
            ]
        );
        let models = &plan.tasks[1];
        assert!(text(KIRO, &models.range).contains("2.1 Create the User model"));
        assert_eq!(
            files(models),
            [
                ("src/models/user.ts", FileRole::Mention),
                ("src/models/user.test.ts", FileRole::Mention),
            ]
        );
        assert_eq!(models.uses, ["User"]);
        assert_eq!(plan.tasks[2].uses, ["createUser"]);
        assert_eq!(plan.tasks[2].decisions, ["emails are stored lower-cased"]);
    }

    #[test]
    fn openspec_tasks() {
        let plan = parse_plan(OPENSPEC).expect("a plan");
        assert_eq!(plan.kind, PlanKind::OpenSpec);
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].title, "Implementation");
        assert_eq!(
            files(&plan.tasks[0]),
            [
                ("db/migrations/0042_two_factor.sql", FileRole::Mention),
                ("src/auth/totp.py", FileRole::Mention),
                ("src/auth/login.py", FileRole::Mention),
            ]
        );
        assert_eq!(plan.tasks[0].uses, ["two_factor_secret", "verify_totp"]);
        assert_eq!(plan.tasks[1].uses, ["test_login_requires_totp"]);
    }

    #[test]
    fn checklist_plan() {
        let plan = parse_plan(CHECKLIST).expect("a plan");
        assert_eq!(plan.kind, PlanKind::Checklist);
        assert_eq!(plan.title.as_deref(), Some("Move search to the new index"));
        let titles: Vec<_> = plan.tasks.iter().map(|task| task.title.as_str()).collect();
        assert_eq!(titles, ["Build the index", "Switch readers over"]);
        assert_eq!(plan.tasks[0].key, "build-the-index");
        assert_eq!(plan.tasks[0].uses, ["IndexBuilder"]);
        assert_eq!(plan.tasks[0].commands, ["cargo test index"]);
        assert_eq!(
            plan.tasks[1].uses,
            ["SearchService::query", "legacy_lookup"]
        );
        assert_eq!(plan.overview.summary, "Some context about why");
    }

    #[test]
    fn documents_without_tasks_are_not_plans() {
        assert_eq!(plan_task_count(NOT_A_PLAN), None);
        assert_eq!(plan_task_count("# Notes\n\nJust text.\n"), None);
        assert_eq!(plan_task_count(SUPERPOWERS), Some(7));
    }

    #[test]
    fn headings_inside_code_blocks_are_not_tasks() {
        let source = "# Plan\n\n```md\n### Task 1: Fake\n```\n\n### Task 1: Real\n\nText.\n";
        let plan = parse_plan(source).expect("a plan");
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].title, "Real");
    }

    #[test]
    fn spec_requirements() {
        assert_eq!(
            requirement_headings(SPEC),
            [
                "Retry timeouts and server errors",
                "Never retry declines",
                "Nightly job",
                "Metrics",
                "Refund partial captures",
            ]
        );
        let kiro = "# Requirements\n\n## Introduction\n\n### Requirement 1: Sign up\n\n### Requirement 2: Log in\n";
        assert_eq!(
            requirement_headings(kiro),
            ["Requirement 1: Sign up", "Requirement 2: Log in"]
        );
    }
}
