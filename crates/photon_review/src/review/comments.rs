use std::cell::RefCell;
use std::rc::Rc;

use editor::{CompletionProvider, actions::MoveToEnd};
use language::{CodeLabel, ToOffset as _};
use project::{
    CompletionDisplayOptions, CompletionResponse, CompletionSource,
    lsp_store::CompletionDocumentation,
};

use super::*;

/// A row of the Comments view: a review thread, or one of the user's drafts by its local id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CommentKey {
    Thread(String),
    Draft(usize),
}

impl CommentKey {
    fn selector(&self) -> String {
        match self {
            CommentKey::Thread(id) => format!("photon-review-comment-row-{id}"),
            CommentKey::Draft(local_id) => format!("photon-review-comment-row-draft-{local_id}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum CommentFilter {
    Open,
    Resolved,
    Mine,
    Ai,
    ThisFile,
}

impl CommentFilter {
    const ALL: [Self; 5] = [
        Self::Open,
        Self::Resolved,
        Self::Mine,
        Self::Ai,
        Self::ThisFile,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Resolved => "Resolved",
            Self::Mine => "Mine",
            Self::Ai => "AI",
            Self::ThisFile => "This file",
        }
    }
}

pub(super) struct CommentRow {
    key: CommentKey,
    author: String,
    /// When the last comment was written; drafts have none and count as newest.
    activity: Option<String>,
    first_line: String,
    path: String,
    /// None when the code can't show it: an outdated thread or draft, or one on a whole file.
    line: Option<u32>,
    resolved: bool,
    mine: bool,
    from_ai: bool,
    severity: Option<String>,
    suggestion: bool,
    replies: usize,
}

impl CommentRow {
    fn is_draft(&self) -> bool {
        matches!(self.key, CommentKey::Draft(_))
    }
}

fn first_line(text: &str) -> String {
    let prose = without_suggestion(text);
    prose
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "Suggested change".to_string())
}

/// `text` as a Markdown quote, then a blank line to write under it.
pub(super) fn quote_text(text: &str) -> String {
    let quoted: Vec<String> = text
        .trim_end_matches('\n')
        .lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect();
    format!("{}\n\n", quoted.join("\n"))
}

/// The pull request's people to mention, each with how they take part, without the viewer.
pub(super) fn participants(detail: &PrDetail, login: &str) -> Vec<(String, &'static str)> {
    let mut people = vec![(detail.summary.author.clone(), "author")];
    people.extend(
        detail
            .reviewers
            .iter()
            .map(|reviewer| (reviewer.clone(), "reviewed")),
    );
    people.extend(
        detail
            .requested_reviewers
            .iter()
            .map(|reviewer| (reviewer.clone(), "requested")),
    );
    people.extend(
        detail
            .threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .map(|comment| (comment.author.clone(), "commented")),
    );
    let mut seen = HashSet::new();
    people.retain(|(person, _)| {
        let lower = person.to_lowercase();
        lower != login.to_lowercase() && lower != "ghost" && seen.insert(lower)
    });
    people
}

/// Where an `@login` being typed starts, when the cursor is in one. An `@` inside a word, as in
/// an email address, doesn't start one.
fn mention_start(buffer: &language::Buffer, position: language::Anchor) -> Option<usize> {
    let mut start = position.to_offset(buffer);
    for character in buffer.reversed_chars_at(position) {
        if character == '@' {
            let at = start.checked_sub(1)?;
            let before = buffer.reversed_chars_at(at).next();
            return before
                .is_none_or(|before| !before.is_alphanumeric())
                .then_some(at);
        }
        if !(character.is_ascii_alphanumeric() || character == '-' || character == '_') {
            return None;
        }
        start -= character.len_utf8();
    }
    None
}

/// Offers the pull request's participants after `@` in a reply or draft.
pub(super) struct MentionProvider {
    pub(super) participants: Rc<RefCell<Vec<(String, &'static str)>>>,
}

impl CompletionProvider for MentionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _: editor::CompletionContext,
        _: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let Some(start) = mention_start(buffer, buffer_position) else {
            return Task::ready(Ok(Vec::new()));
        };
        let replace_range = buffer.anchor_before(start)..buffer_position;
        let completions = self
            .participants
            .borrow()
            .iter()
            .map(|(login, role)| project::Completion {
                replace_range: replace_range.clone(),
                label: CodeLabel::plain(login.clone(), None),
                new_text: format!("@{login} "),
                documentation: Some(CompletionDocumentation::SingleLine((*role).into())),
                source: CompletionSource::Custom,
                icon_path: None,
                icon_color: None,
                match_start: None,
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
                group: None,
            })
            .collect();
        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions::default(),
            is_incomplete: false,
        }]))
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        _: &str,
        _: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        mention_start(buffer.read(cx), position).is_some()
    }

    fn sort_completions(&self) -> bool {
        false
    }
}

/// Write and Preview for a composer; `key` is the composer's slot.
pub(super) fn write_preview_toggle(
    item: WeakEntity<ReviewItem>,
    key: String,
    previewing: bool,
) -> AnyElement {
    let button = |label: &'static str, preview: bool| {
        let item = item.clone();
        let key = key.clone();
        let selector = format!("photon-review-{}-{key}", label.to_lowercase());
        div().debug_selector(move || selector).child(
            Button::new(label, label)
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .label_size(LabelSize::Small)
                .toggle_state(previewing == preview)
                .on_click(move |_, _, cx| {
                    item.update(cx, |item, cx| item.set_preview(&key, preview, cx))
                        .log_err();
                }),
        )
    };
    h_flex()
        .gap_0p5()
        .child(button("Write", false))
        .child(button("Preview", true))
        .into_any_element()
}

pub(super) fn draft_slot(local_id: usize) -> String {
    format!("draft:{local_id}")
}

impl ReviewItem {
    pub(super) fn refresh_participants(&self) {
        *self.participants.borrow_mut() = participants(&self.detail, &self.login);
    }

    pub(super) fn mention_provider(&self) -> Rc<dyn CompletionProvider> {
        Rc::new(MentionProvider {
            participants: self.participants.clone(),
        })
    }

    fn comment_rows(&self) -> Vec<CommentRow> {
        let mut rows = Vec::new();
        for thread in &self.detail.threads {
            let (Some(first), Some(last)) = (thread.comments.first(), thread.comments.last())
            else {
                continue;
            };
            let (_, severity, text) = agent_lead(&first.body);
            rows.push(CommentRow {
                key: CommentKey::Thread(thread.id.clone()),
                author: first.author.clone(),
                activity: Some(last.created_at.clone()),
                first_line: first_line(text),
                path: thread.path.clone(),
                line: thread.line,
                resolved: thread.is_resolved,
                mine: first.author.eq_ignore_ascii_case(&self.login),
                from_ai: severity.is_some() || first.author.ends_with("[bot]"),
                severity: severity.map(str::to_string),
                suggestion: thread
                    .comments
                    .iter()
                    .any(|comment| parse_suggestion(&comment.body).is_some()),
                replies: thread.comments.len() - 1,
            });
        }
        for (draft, view) in self.drafts.iter().zip(&self.draft_views) {
            if draft.general {
                continue;
            }
            let (_, severity, text) = if draft.from_agent {
                agent_lead(&draft.body)
            } else {
                (None, None, draft.body.as_str())
            };
            rows.push(CommentRow {
                key: CommentKey::Draft(view.local_id),
                author: "you".to_string(),
                activity: None,
                first_line: first_line(text),
                path: draft.path.clone(),
                line: (!draft.outdated).then_some(draft.end_line),
                resolved: false,
                mine: true,
                from_ai: draft.from_agent,
                severity: severity.map(str::to_string),
                suggestion: parse_suggestion(&draft.body).is_some(),
                replies: 0,
            });
        }
        rows
    }

    /// Where the comment sits in the review: its step, its file in the step, its line.
    fn diff_position(&self, row: &CommentRow) -> Option<(usize, usize, u32)> {
        let line = row.line?;
        self.steps.iter().enumerate().find_map(|(index, step)| {
            let file = step.files.iter().position(|path| *path == row.path)?;
            Some((index, file, line))
        })
    }

    /// Open comments the diff can show, in the order the review walks through them.
    fn open_comments_in_diff_order(&self) -> Vec<CommentKey> {
        let mut open: Vec<_> = self
            .comment_rows()
            .into_iter()
            .filter(|row| !row.resolved)
            .filter_map(|row| Some((self.diff_position(&row)?, row.key)))
            .collect();
        open.sort_by_key(|(position, _)| *position);
        open.into_iter().map(|(_, key)| key).collect()
    }

    fn step_through_comments(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self.open_comments_in_diff_order();
        let current = self
            .selected_comment
            .as_ref()
            .and_then(|selected| open.iter().position(|key| key == selected));
        let next = match (current, forward) {
            (None, true) => Some(0),
            (None, false) => open.len().checked_sub(1),
            (Some(index), true) => Some(index + 1),
            (Some(index), false) => index.checked_sub(1),
        };
        if let Some(key) = next.and_then(|index| open.get(index)).cloned() {
            self.jump_to_comment(key, window, cx);
        }
    }

    pub(super) fn next_comment(
        &mut self,
        _: &NextComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_through_comments(true, window, cx);
    }

    pub(super) fn previous_comment(
        &mut self,
        _: &PreviousComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_through_comments(false, window, cx);
    }

    /// Selects the comment's row, then shows its card: the step holding its line, scrolled to it.
    pub(super) fn jump_to_comment(
        &mut self,
        key: CommentKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match &key {
            CommentKey::Thread(thread_id) => self
                .detail
                .threads
                .iter()
                .find(|thread| thread.id == *thread_id)
                .map(|thread| {
                    if thread.is_resolved {
                        self.expanded_resolved.insert(thread.id.clone());
                    }
                    (thread.path.clone(), thread.side, thread.line)
                }),
            CommentKey::Draft(local_id) => self.draft_index(*local_id).and_then(|index| {
                let draft = self.drafts.get(index)?;
                Some((
                    draft.path.clone(),
                    draft.side,
                    (!draft.outdated).then_some(draft.end_line),
                ))
            }),
        };
        let Some((path, side, line)) = target else {
            return;
        };
        self.selected_comment = Some(key);
        self.show_comments(cx);
        self.show_line(&path, side, line, window, cx);
        cx.notify();
    }

    /// The thread on the line under the diff's cursor.
    fn thread_at_cursor(&self, cx: &App) -> Option<String> {
        let lines = self.selected_lines(cx)?;
        self.detail
            .threads
            .iter()
            .find(|thread| {
                let Some(line) = thread.line else {
                    return false;
                };
                let start = thread.start_line.unwrap_or(line).min(line);
                thread.path == lines.path
                    && thread.side == lines.side
                    && start <= lines.end_line
                    && lines.start_line <= line
            })
            .map(|thread| thread.id.clone())
    }

    pub(super) fn reply_to_comment(
        &mut self,
        _: &ReplyToComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.thread_at_cursor(cx) else {
            return;
        };
        self.focus_reply(&thread_id, window, cx);
    }

    fn focus_reply(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.reply_editors.get(thread_id).cloned() else {
            return;
        };
        // A folded resolved thread or a preview has no reply box to type in.
        self.expanded_resolved.insert(thread_id.to_string());
        self.previews.remove(&reply_slot(thread_id));
        self.selected_comment = Some(CommentKey::Thread(thread_id.to_string()));
        self.show_comments(cx);
        window.focus(&editor.focus_handle(cx), cx);
        cx.notify();
    }

    pub(super) fn resolve_comment(
        &mut self,
        _: &ResolveComment,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.thread_at_cursor(cx) else {
            return;
        };
        let open = self
            .detail
            .threads
            .iter()
            .any(|thread| thread.id == thread_id && !thread.is_resolved);
        if open {
            self.set_thread_resolved(&thread_id, true, cx)
                .detach_and_log_err(cx);
        }
    }

    fn diff_selection_text(&self, cx: &App) -> Option<String> {
        let selection = *self.editor.read(cx).selections.newest_anchor();
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let range = multi_buffer::ToOffset::to_offset(&selection.start, &snapshot)
            ..multi_buffer::ToOffset::to_offset(&selection.end, &snapshot);
        (!range.is_empty()).then(|| snapshot.text_for_range(range).collect())
    }

    /// Puts the text selected in the thread's card, or else in the diff, at the top of its
    /// reply as a quote.
    pub(super) fn quote_into_reply(
        &mut self,
        thread_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = match self.card_quote.take() {
            Some((quoted_thread, text)) if quoted_thread == thread_id => Some(text),
            _ => self.diff_selection_text(cx),
        };
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            return;
        };
        let Some(editor) = self.reply_editors.get(thread_id).cloned() else {
            return;
        };
        editor.update(cx, |editor, cx| {
            let reply = format!("{}{}", quote_text(&text), editor.text(cx));
            editor.set_text(reply, window, cx);
            editor.move_to_end(&MoveToEnd, window, cx);
        });
        self.focus_reply(thread_id, window, cx);
    }

    /// Shows a composer's text as it will look once posted, or its editor again.
    pub(super) fn set_preview(&mut self, slot: &str, preview: bool, cx: &mut Context<Self>) {
        if !preview {
            self.previews.remove(slot);
        } else if let Some((text, path, side, start, end)) = self.composer_source(slot, cx) {
            let body = self.comment_body(
                &text,
                |this, cx| this.original_lines(&path, side, start, end, cx),
                &mut HashSet::new(),
                cx,
            );
            self.previews.insert(slot.to_string(), body);
        }
        self.show_comments(cx);
        cx.notify();
    }

    /// A composer's text, and the lines a suggestion in it would replace.
    fn composer_source(&self, slot: &str, cx: &App) -> Option<(String, String, Side, u32, u32)> {
        if let Some(thread_id) = slot.strip_prefix("reply:") {
            let thread = self
                .detail
                .threads
                .iter()
                .find(|thread| thread.id == thread_id)?;
            let text = self.reply_editors.get(thread_id)?.read(cx).text(cx);
            let end = thread.line.unwrap_or_default();
            let start = thread.start_line.unwrap_or(end);
            return Some((text, thread.path.clone(), thread.side, start, end));
        }
        let local_id = slot.strip_prefix("draft:")?.parse().ok()?;
        let draft = self.drafts.get(self.draft_index(local_id)?)?;
        Some((
            draft.body.clone(),
            draft.path.clone(),
            draft.side,
            draft.start_line,
            draft.end_line,
        ))
    }

    fn current_file(&self, cx: &App) -> Option<String> {
        self.selected_lines(cx).map(|lines| lines.path)
    }

    fn row_shown(&self, row: &CommentRow, current_file: Option<&str>) -> bool {
        let filters = &self.comment_filters;
        let (open, resolved) = (
            filters.contains(&CommentFilter::Open),
            filters.contains(&CommentFilter::Resolved),
        );
        let status = open == resolved || (open && !row.resolved) || (resolved && row.resolved);
        status
            && (!filters.contains(&CommentFilter::Mine) || row.mine)
            && (!filters.contains(&CommentFilter::Ai) || row.from_ai)
            && (!filters.contains(&CommentFilter::ThisFile)
                || current_file == Some(row.path.as_str()))
    }

    pub(super) fn toggle_comment_filter(&mut self, filter: CommentFilter, cx: &mut Context<Self>) {
        if !self.comment_filters.remove(&filter) {
            self.comment_filters.insert(filter);
        }
        cx.notify();
    }

    fn render_comment_row(&self, row: &CommentRow, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let selected = self.selected_comment.as_ref() == Some(&row.key);
        let mut chips = Vec::new();
        if row.from_ai {
            chips.push(state_chip("AI", Color::Muted, cx));
        }
        if let Some(severity) = &row.severity {
            chips.push(state_chip(severity.clone(), severity_color(severity), cx));
        }
        if row.suggestion {
            chips.push(state_chip("suggestion", Color::Success, cx));
        }
        if row.is_draft() {
            chips.push(state_chip("draft", Color::Accent, cx));
        }
        if row.replies > 0 {
            chips.push(state_chip(
                if row.replies == 1 {
                    "1 reply".to_string()
                } else {
                    format!("{} replies", row.replies)
                },
                Color::Muted,
                cx,
            ));
        }
        let place = match row.line {
            Some(line) => format!("{}:{line}", row.path),
            None => row.path.clone(),
        };
        let key = row.key.clone();
        let selector = row.key.selector();
        v_flex()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector)
            .relative()
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .when(selected, |this| {
                this.bg(colors.element_selected).child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(2.))
                        .bg(colors.text_accent),
                )
            })
            .on_click(
                cx.listener(move |this, _, window, cx| {
                    this.jump_to_comment(key.clone(), window, cx)
                }),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .min_w_0()
                    .child(
                        Label::new(row.author.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .children(
                        row.activity
                            .as_deref()
                            .and_then(relative_time)
                            .map(|age| Label::new(age).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .children(chips),
            )
            .child(
                Label::new(row.first_line.clone())
                    .size(LabelSize::Small)
                    .truncate(),
            )
            .child(
                Label::new(place)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .buffer_font(cx)
                    .truncate(),
            )
            .into_any_element()
    }

    /// The steps column's second view: every thread and draft, with filters.
    pub(super) fn render_comments_view(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut rows = self.comment_rows();
        // Open first, newest activity last; drafts are newest.
        rows.sort_by(|left, right| {
            (left.is_draft(), &left.activity).cmp(&(right.is_draft(), &right.activity))
        });
        let total = rows.len();
        let open_count = rows.iter().filter(|row| !row.resolved).count();
        let current_file = self.current_file(cx);
        let (open, resolved): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .filter(|row| self.row_shown(row, current_file.as_deref()))
            .partition(|row| !row.resolved);
        let mut elements = vec![
            h_flex()
                .gap_2()
                .child(Label::new("Comments").weight(gpui::FontWeight::SEMIBOLD))
                .child(
                    Label::new(format!("{total} · {open_count} open"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .debug_selector(|| "photon-review-show-steps".into())
                        .child(
                            Button::new("photon-review-show-steps", "Steps ›")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::XSmall)
                                .color(Color::Accent)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.comments_view = false;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
            h_flex()
                .flex_wrap()
                .gap_1()
                .pb_1()
                .children(CommentFilter::ALL.into_iter().map(|filter| {
                    let selector = format!("photon-review-filter-{}", filter.label());
                    div()
                        .debug_selector(move || selector)
                        .child(
                            Button::new(filter.label(), filter.label())
                                .style(ButtonStyle::Outlined)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::XSmall)
                                .toggle_state(self.comment_filters.contains(&filter))
                                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_comment_filter(filter, cx)
                                })),
                        )
                        .into_any_element()
                }))
                .into_any_element(),
        ];
        if open.is_empty() && resolved.is_empty() {
            elements.push(
                Label::new(if total == 0 {
                    "No comments yet"
                } else {
                    "No comments match these filters"
                })
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            );
        }
        elements.extend(open.iter().map(|row| self.render_comment_row(row, cx)));
        if !resolved.is_empty() {
            // Asking for resolved comments shows them; otherwise they stay folded.
            let unfolded =
                self.resolved_rows_open || self.comment_filters.contains(&CommentFilter::Resolved);
            elements.push(
                h_flex()
                    .id("photon-review-resolved-rows")
                    .debug_selector(|| "photon-review-resolved-rows".into())
                    .pt_1()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.resolved_rows_open = !this.resolved_rows_open;
                        cx.notify();
                    }))
                    .child(Disclosure::new(
                        "photon-review-resolved-disclosure",
                        unfolded,
                    ))
                    .child(
                        Label::new(format!("Resolved · {}", resolved.len()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
            if unfolded {
                elements.extend(resolved.iter().map(|row| self.render_comment_row(row, cx)));
            }
        }
        elements
    }

    /// The comment keys, under the review while the Comments view is open.
    pub(super) fn render_comment_keys(&self, cx: &Context<Self>) -> impl IntoElement {
        let focus = self.editor.focus_handle(cx);
        let hint = |action: &dyn gpui::Action, label: &'static str| {
            h_flex()
                .gap_1()
                .child(ui::KeyBinding::for_action_in(action, &focus, cx))
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        };
        h_flex()
            .debug_selector(|| "photon-review-comment-keys".into())
            .px_2()
            .py_1()
            .gap_3()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(hint(&NextComment, "next comment"))
            .child(hint(&PreviousComment, "previous"))
            .child(hint(&ReplyToComment, "reply"))
            .child(hint(&ResolveComment, "resolve"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_every_line_and_leaves_a_blank_line() {
        assert_eq!(quote_text("one\n\ntwo\n"), "> one\n>\n> two\n\n");
    }
}
