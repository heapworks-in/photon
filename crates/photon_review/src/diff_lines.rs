use buffer_diff::DiffHunkStatusKind;
use editor::GutterColumns;
use gpui::{AnyElement, App, IntoElement, ParentElement, Pixels, Styled, TextStyle, Window, div};
use language::Point;
use multi_buffer::{MultiBufferSnapshot, RowInfo};
use ui::{ActiveTheme, FluentBuilder, h_flex, v_flex};
use util::ResultExt;

/// The review diff's gutter, GitHub-style: old and new line numbers, then `+` or `−`.
pub struct DiffLineNumbers;

const MIN_DIGITS: u32 = 3;
const ROW_TINT: f32 = 0.14;

#[derive(Debug, PartialEq)]
pub struct DiffLine {
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub kind: Option<DiffHunkStatusKind>,
}

/// Deleted rows come from the base text, so their buffer row is already the old line.
pub fn diff_line(row: &RowInfo, buffer: &MultiBufferSnapshot) -> DiffLine {
    let kind = row.diff_status.map(|status| status.kind);
    let Some(buffer_row) = row.buffer_row else {
        return DiffLine {
            old: None,
            new: None,
            kind,
        };
    };
    let number = buffer_row + 1;
    match kind {
        Some(DiffHunkStatusKind::Deleted) => DiffLine {
            old: Some(number),
            new: None,
            kind,
        },
        Some(DiffHunkStatusKind::Added) => DiffLine {
            old: None,
            new: Some(number),
            kind,
        },
        _ => {
            let old = row
                .buffer_id
                .and_then(|buffer_id| {
                    let diff = buffer.diff_for_buffer_id(buffer_id)?;
                    let snapshot = buffer.buffer_for_id(buffer_id)?;
                    let base_point =
                        diff.buffer_point_to_base_text_point(Point::new(buffer_row, 0), snapshot);
                    Some(base_point.row + 1)
                })
                .unwrap_or(number);
            DiffLine {
                old: Some(old),
                new: Some(number),
                kind,
            }
        }
    }
}

// ponytail: sized from the new side's widest line; an old side with more digits overflows its cell.
fn digits(buffer: &MultiBufferSnapshot) -> u32 {
    (buffer.widest_line_number().max(1).ilog10() + 1).max(MIN_DIGITS)
}

/// In characters: a cell holding both numbers with a one-character gap and half a character
/// of padding each side, then two for the marker.
fn columns(digits: u32) -> f32 {
    (2 * digits + 4) as f32
}

impl GutterColumns for DiffLineNumbers {
    fn width(&self, buffer: &MultiBufferSnapshot, ch_advance: Pixels, _: &App) -> Pixels {
        ch_advance * columns(digits(buffer))
    }

    fn render(
        &self,
        rows: &[RowInfo],
        buffer: &MultiBufferSnapshot,
        text_style: &TextStyle,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let font_id = window.text_system().resolve_font(&text_style.font());
        let ch = window
            .text_system()
            .ch_advance(font_id, font_size)
            .log_err()
            .unwrap_or_default();
        let digits = digits(buffer);
        let colors = cx.theme().colors();
        let number = |line: Option<u32>| {
            div()
                .w(ch * digits as f32)
                .flex()
                .justify_end()
                .when_some(line, |this, line| this.child(line.to_string()))
        };
        v_flex()
            .w(ch * columns(digits))
            .font(text_style.font())
            .text_size(font_size)
            .line_height(line_height)
            .children(rows.iter().map(|row| {
                let line = diff_line(row, buffer);
                let (tint, marker) = match line.kind {
                    Some(DiffHunkStatusKind::Added) => (Some(colors.version_control_added), "+"),
                    Some(DiffHunkStatusKind::Deleted) => {
                        (Some(colors.version_control_deleted), "−")
                    }
                    _ => (None, ""),
                };
                h_flex()
                    .h(line_height)
                    .child(
                        h_flex()
                            .h_full()
                            .px(ch * 0.5)
                            .gap(ch)
                            .text_color(colors.editor_line_number)
                            .when_some(tint, |this, tint| {
                                this.bg(tint.opacity(ROW_TINT))
                                    .text_color(colors.editor_foreground.blend(tint.opacity(0.6)))
                            })
                            .child(number(line.old))
                            .child(number(line.new)),
                    )
                    .child(
                        div()
                            .w(ch * 2.)
                            .flex()
                            .justify_center()
                            .when_some(tint, |this, tint| this.text_color(tint))
                            .child(marker),
                    )
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer_diff::BufferDiff;
    use editor::Editor;
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
    use language::{Buffer, Capability};
    use multi_buffer::{MultiBuffer, PathKey};
    use parking_lot::Mutex;
    use settings::SettingsStore;
    use std::sync::Arc;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    const BASE: &str = "one\ntwo\nthree\nfour\nfive\n";
    const TEXT: &str = "one\nthree\nFOUR\nfive\nsix\n";

    async fn diff_multibuffer(cx: &mut TestAppContext) -> Entity<MultiBuffer> {
        let buffer = cx.new(|cx| Buffer::local(TEXT, cx));
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let diff = cx.new(|cx| BufferDiff::new(&snapshot.text, None, None, cx));
        diff.update(cx, |diff, cx| {
            diff.set_base_text(Some(BASE.into()), snapshot.text.clone(), cx)
        })
        .await;
        cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer.set_excerpts_for_path(
                PathKey::for_buffer(&buffer, cx),
                buffer.clone(),
                [Point::zero()..snapshot.max_point()],
                0,
                cx,
            );
            multibuffer.add_diff(diff, cx);
            multibuffer
        })
    }

    struct Recording(Mutex<Vec<DiffLine>>);

    impl GutterColumns for Recording {
        fn width(&self, buffer: &MultiBufferSnapshot, ch_advance: Pixels, cx: &App) -> Pixels {
            DiffLineNumbers.width(buffer, ch_advance, cx)
        }

        fn render(
            &self,
            rows: &[RowInfo],
            buffer: &MultiBufferSnapshot,
            text_style: &TextStyle,
            line_height: Pixels,
            window: &mut Window,
            cx: &mut App,
        ) -> AnyElement {
            *self.0.lock() = rows.iter().map(|row| diff_line(row, buffer)).collect();
            DiffLineNumbers.render(rows, buffer, text_style, line_height, window, cx)
        }
    }

    fn line(old: Option<u32>, new: Option<u32>, kind: Option<DiffHunkStatusKind>) -> DiffLine {
        DiffLine { old, new, kind }
    }

    #[gpui::test]
    async fn the_review_gutter_numbers_old_and_new_lines(cx: &mut TestAppContext) {
        init_test(cx);
        let multibuffer = diff_multibuffer(cx).await;
        let window = cx.add_window(|window, cx| {
            Editor::for_multibuffer(multibuffer.clone(), None, window, cx)
        });
        let editor = window.root(cx).expect("editor window");
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let style = cx.update(|_, cx| editor.update(cx, |editor, cx| editor.style(cx).clone()));
        let gutter_width = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    let font_size = style.text.font_size.to_pixels(window.rem_size());
                    let font_id = window.text_system().resolve_font(&style.text.font());
                    editor
                        .snapshot(window, cx)
                        .gutter_dimensions(font_id, font_size, &style, window, cx)
                        .width
                })
            })
        };
        let plain_width = gutter_width(cx);

        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        cx.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_gutter_columns(Some(recording.clone()), cx)
            })
        });
        let ch = cx.update(|window, _| {
            let font_size = style.text.font_size.to_pixels(window.rem_size());
            let font_id = window.text_system().resolve_font(&style.text.font());
            window
                .text_system()
                .ch_advance(font_id, font_size)
                .expect("ch advance")
        });
        assert_eq!(gutter_width(cx), plain_width + ch * columns(MIN_DIGITS));

        cx.run_until_parked();
        let lines: Vec<DiffLine> = recording
            .0
            .lock()
            .drain(..)
            .filter(|line| line.old.is_some() || line.new.is_some())
            .collect();
        use DiffHunkStatusKind::{Added, Deleted};
        assert_eq!(
            lines[..7],
            [
                line(Some(1), Some(1), None),
                line(Some(2), None, Some(Deleted)),
                line(Some(3), Some(2), None),
                line(Some(4), None, Some(Deleted)),
                line(None, Some(3), Some(Added)),
                line(Some(5), Some(4), None),
                line(None, Some(5), Some(Added)),
            ]
        );
    }
}
