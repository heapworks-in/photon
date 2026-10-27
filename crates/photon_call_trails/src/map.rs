use call_hierarchy::CallHierarchyMode;
use collections::HashMap;
use gpui::{ClickEvent, Context, Hsla, PathBuilder, Pixels, Point, Window, canvas, point, px};
use ui::prelude::*;

use crate::{CallHierarchyPanel, Node, glyph};

const PADDING: Pixels = px(24.);
const COLUMN_GAP: Pixels = px(48.);
const ROW_HEIGHT: Pixels = px(40.);
const NODE_HEIGHT: Pixels = px(26.);
const NAME_SIZE: Pixels = px(12.);
const DETAIL_SIZE: Pixels = px(10.);

/// Where a call sits on the map, in columns and rows rather than pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MapNode {
    pub node_ix: usize,
    /// Distance from the root.
    pub column: usize,
    pub row: f32,
}

/// The explored tree: columns by depth, rows in list order. A leaf takes the next row and a
/// parent sits midway between its first and last child, so the layout never depends on timing.
pub(crate) fn layout(nodes: &[Node]) -> Vec<MapNode> {
    let mut placed = Vec::new();
    if !nodes.is_empty() {
        let mut next_row = 0.;
        place(nodes, 0, &mut next_row, &mut placed);
    }
    placed
}

fn place(nodes: &[Node], node_ix: usize, next_row: &mut f32, placed: &mut Vec<MapNode>) -> f32 {
    let Some(node) = nodes.get(node_ix) else {
        return *next_row;
    };
    let children = match &node.children {
        Some(children) if node.expanded => children.as_slice(),
        _ => &[],
    };
    let child_rows = children
        .iter()
        .map(|child| place(nodes, *child, next_row, placed))
        .collect::<Vec<_>>();
    let row = match (child_rows.first(), child_rows.last()) {
        (Some(first), Some(last)) => (first + last) / 2.,
        _ => {
            let row = *next_row;
            *next_row += 1.;
            row
        }
    };
    placed.push(MapNode {
        node_ix,
        column: node.depth,
        row,
    });
    row
}

impl CallHierarchyPanel {
    pub(crate) fn render_map(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let nodes = self.nodes();
        let placed = layout(nodes);
        let trail = self.trail();
        let colors = cx.theme().colors();

        // Nodes use the UI font like the rest of the panel, so each label is measured.
        let font = theme::theme_settings(cx).ui_font(cx).clone();
        let measure = |text: &SharedString, size: Pixels| {
            let run = gpui::TextRun {
                len: text.len(),
                font: font.clone(),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .layout_line(text, size, &[run], None)
                .width
        };

        let labels = placed
            .iter()
            .map(|placed| {
                let node = nodes.get(placed.node_ix)?;
                let name = SharedString::from(node.call.item.name.clone());
                let detail = node
                    .call
                    .item
                    .buffer
                    .read(cx)
                    .file()
                    .map(|file| SharedString::from(file.file_name(cx).to_string()));
                let width = px(8. + 16. + 6.)
                    + measure(&name, NAME_SIZE)
                    + detail
                        .as_ref()
                        .map_or(px(0.), |detail| px(6.) + measure(detail, DETAIL_SIZE))
                    + px(8.);
                Some((name, detail, width, node.tag))
            })
            .collect::<Vec<_>>();

        let columns = placed.iter().map(|node| node.column).max().unwrap_or(0) + 1;
        let mut column_widths = vec![px(0.); columns];
        for (node, label) in placed.iter().zip(&labels) {
            if let (Some(width), Some((_, _, node_width, _))) =
                (column_widths.get_mut(node.column), label)
            {
                *width = (*width).max(*node_width);
            }
        }
        // Callers read left to right into the root; calls go out from it.
        let mut column_x = vec![px(0.); columns];
        let mut x = PADDING;
        for screen_column in 0..columns {
            let column = match self.mode {
                CallHierarchyMode::Incoming => columns - 1 - screen_column,
                CallHierarchyMode::Outgoing => screen_column,
            };
            if let (Some(left), Some(width)) = (column_x.get_mut(column), column_widths.get(column))
            {
                *left = x;
                x += *width + COLUMN_GAP;
            }
        }
        let total_width = x - COLUMN_GAP + PADDING;
        let rows = placed.iter().map(|node| node.row).fold(0., f32::max) + 1.;
        let total_height = PADDING * 2. + ROW_HEIGHT * rows;

        let mut bounds = HashMap::default();
        for (node, label) in placed.iter().zip(&labels) {
            if let (Some(left), Some((_, _, width, _))) = (column_x.get(node.column), label) {
                let top = PADDING + ROW_HEIGHT * node.row + (ROW_HEIGHT - NODE_HEIGHT) / 2.;
                bounds.insert(node.node_ix, (*left, top, *width));
            }
        }

        let mut edges: Vec<(Point<Pixels>, Point<Pixels>, Hsla)> = Vec::new();
        for node in &placed {
            let Some(parent_ix) = nodes.get(node.node_ix).and_then(|node| node.parent) else {
                continue;
            };
            let (Some(child), Some(parent)) = (bounds.get(&node.node_ix), bounds.get(&parent_ix))
            else {
                continue;
            };
            let middle = NODE_HEIGHT / 2.;
            let (from, to) = match self.mode {
                CallHierarchyMode::Incoming => (
                    point(child.0 + child.2, child.1 + middle),
                    point(parent.0, parent.1 + middle),
                ),
                CallHierarchyMode::Outgoing => (
                    point(parent.0 + parent.2, parent.1 + middle),
                    point(child.0, child.1 + middle),
                ),
            };
            let color = if trail.contains(&node.node_ix) {
                colors.text_accent
            } else {
                colors.border
            };
            edges.push((from, to, color));
        }
        // Lit edges last, so they're drawn over the dimmed ones they cross.
        edges.sort_by_key(|(_, _, color)| *color == colors.text_accent);

        let node_elements = placed.iter().zip(labels).filter_map(|(node, label)| {
            let (name, detail, width, tag) = label?;
            let (left, top, _) = *bounds.get(&node.node_ix)?;
            let node_ix = node.node_ix;
            let lit = trail.contains(&node_ix);
            let selected = self.selected == Some(node_ix);
            let selector = name.clone();
            Some(
                h_flex()
                    .id(("call-trail-map-node", node_ix))
                    .debug_selector(move || format!("call-trail-map-{selector}"))
                    .absolute()
                    .left(left)
                    .top(top)
                    .w(width)
                    .h(NODE_HEIGHT)
                    .px(px(8.))
                    .gap(px(6.))
                    .rounded_md()
                    .border_1()
                    .border_color(if lit {
                        colors.text_accent
                    } else {
                        colors.border_variant
                    })
                    .bg(if selected {
                        colors.element_selected
                    } else {
                        colors.elevated_surface_background
                    })
                    .when(!lit, |this| this.opacity(0.55))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .cursor_pointer()
                    .hover(|style| style.border_color(colors.border_focused))
                    .child(glyph(tag, cx))
                    .child(
                        div()
                            .text_size(NAME_SIZE)
                            .text_color(colors.text)
                            .child(name),
                    )
                    .children(detail.map(|detail| {
                        div()
                            .text_size(DETAIL_SIZE)
                            .text_color(colors.text_muted)
                            .child(detail)
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            this.toggle_expanded(node_ix, window, cx);
                        } else {
                            this.selected = Some(node_ix);
                            cx.notify();
                            this.open_call(node_ix, window, cx);
                        }
                    })),
            )
        });

        div()
            .id("call-trail-map")
            .flex_1()
            .size_full()
            .overflow_scroll()
            .track_scroll(&self.map_scroll_handle)
            .child(
                div()
                    .relative()
                    .w(total_width)
                    .h(total_height)
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                for (from, to, color) in edges {
                                    let from = bounds.origin + from;
                                    let to = bounds.origin + to;
                                    let bend = (to.x - from.x) / 2.;
                                    let mut path = PathBuilder::stroke(px(1.5));
                                    path.move_to(from);
                                    path.cubic_bezier_to(
                                        to,
                                        point(from.x + bend, from.y),
                                        point(to.x - bend, to.y),
                                    );
                                    if let Ok(path) = path.build() {
                                        window.paint_path(path, color);
                                    }
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(node_elements),
            )
            .into_any_element()
    }
}
