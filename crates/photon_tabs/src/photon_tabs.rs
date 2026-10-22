use std::collections::HashMap;

use gpui::{
    App, Context, Entity, EntityId, Hsla, MouseButton, Pixels, ScrollHandle, SharedString,
    Subscription, WeakEntity, Window, actions, canvas,
};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{
    ContextMenu, DynamicSpacing, IconButton, IconButtonShape, IconName, IconSize, Indicator, Label,
    LabelSize, Tab, prelude::*, right_click_menu,
};
use util::ResultExt as _;
use workspace::{
    CloseWindow, ItemSettings, MultiWorkspace, RemovalIntent, Workspace,
    item::{ClosePosition, ShowCloseButton},
};

actions!(
    photon_tabs,
    [
        /// Opens a new empty project tab in this window.
        NewTab,
        /// Closes the active project tab.
        CloseTab
    ]
);

/// The project tabs of one window, drawn in its title bar.
pub struct ProjectTabs {
    multi_workspace: WeakEntity<MultiWorkspace>,
    measured_tab_width: Pixels,
    scroll_handle: ScrollHandle,
    workspace_subscriptions: HashMap<EntityId, Subscription>,
    _subscription: Option<Subscription>,
}

#[derive(Clone)]
struct DraggedProjectTab {
    ix: usize,
    title: SharedString,
    width: Pixels,
    is_active: bool,
    active_background_color: Hsla,
    inactive_background_color: Hsla,
}

pub fn tab_label(workspace: &Workspace, cx: &App) -> SharedString {
    let key = workspace.project_group_key(cx);
    if key.path_list().is_empty() {
        // The same words the window title uses for a project without folders.
        "empty project".into()
    } else {
        key.display_name(&HashMap::default())
    }
}

/// Closes one project tab. Closing the active tab shows its right neighbour,
/// or the left one when it was last, and the last tab closes the window.
pub fn close_tab(
    multi_workspace: &mut MultiWorkspace,
    workspace: Entity<Workspace>,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    let workspaces = multi_workspace.workspaces().cloned().collect::<Vec<_>>();
    if workspaces.len() <= 1 {
        window.dispatch_action(Box::new(CloseWindow), cx);
        return;
    }
    let Some(ix) = workspaces.iter().position(|held| held == &workspace) else {
        return;
    };
    let neighbour = (multi_workspace.workspace() == &workspace)
        .then(|| {
            workspaces
                .get(ix + 1)
                .or_else(|| workspaces.get(ix.wrapping_sub(1)))
        })
        .flatten()
        .cloned();
    let remove = multi_workspace.remove([workspace], RemovalIntent::CloseProject, window, cx);
    cx.spawn_in(window, async move |this, cx| {
        // `remove` picks its own replacement from the sidebar's project order;
        // tabs follow their own order instead.
        if remove.await? {
            if let Some(neighbour) = neighbour {
                this.update_in(cx, |this, window, cx| {
                    if this.workspaces().any(|held| held == &neighbour) {
                        this.activate(neighbour, None, window, cx);
                    }
                })?;
            }
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

impl ProjectTabs {
    pub fn new(multi_workspace: WeakEntity<MultiWorkspace>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            multi_workspace: multi_workspace.clone(),
            measured_tab_width: Pixels::ZERO,
            scroll_handle: ScrollHandle::new(),
            workspace_subscriptions: HashMap::default(),
            _subscription: None,
        };
        if let Some(multi_workspace) = multi_workspace.upgrade() {
            this.observe_workspaces(&multi_workspace, cx);
            this._subscription = Some(cx.observe(&multi_workspace, |this, multi_workspace, cx| {
                this.observe_workspaces(&multi_workspace, cx);
                cx.notify();
            }));
        }
        this
    }

    /// Background projects are not drawn, so their title or unsaved state only
    /// reaches the strip through these observers.
    fn observe_workspaces(
        &mut self,
        multi_workspace: &Entity<MultiWorkspace>,
        cx: &mut Context<Self>,
    ) {
        let workspaces = multi_workspace
            .read(cx)
            .workspaces()
            .cloned()
            .collect::<Vec<_>>();
        self.workspace_subscriptions.retain(|id, _| {
            workspaces
                .iter()
                .any(|workspace| workspace.entity_id() == *id)
        });
        for workspace in workspaces {
            self.workspace_subscriptions
                .entry(workspace.entity_id())
                .or_insert_with(|| cx.observe(&workspace, |_, _, cx| cx.notify()));
        }
    }

    fn render_tab(
        &self,
        ix: usize,
        workspace: Entity<Workspace>,
        is_active: bool,
        active_background_color: Hsla,
        inactive_background_color: Hsla,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = ItemSettings::get_global(cx);
        let close_side = settings.close_position;
        let show_close_button = settings.show_close_button;
        let width = self.measured_tab_width.max(window.rem_size() * 10);
        let title = tab_label(workspace.read(cx), cx);
        let is_edited = workspace.read(cx).is_window_edited(cx);
        let multi_workspace = self.multi_workspace.clone();

        let close = {
            let multi_workspace = multi_workspace.clone();
            let workspace = workspace.clone();
            move |window: &mut Window, cx: &mut App| {
                multi_workspace
                    .update(cx, |multi_workspace, cx| {
                        close_tab(multi_workspace, workspace.clone(), window, cx)
                    })
                    .log_err();
            }
        };

        let tab = h_flex()
            .id(("photon-project-tab", ix))
            .debug_selector(move || format!("photon-project-tab-{ix}"))
            .group("tab")
            .w_full()
            .overflow_hidden()
            .h(Tab::content_height(cx))
            .relative()
            .px(DynamicSpacing::Base16.px(cx))
            .gap_1()
            .justify_center()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .cursor_pointer()
            .on_drag(
                DraggedProjectTab {
                    ix,
                    title: title.clone(),
                    width,
                    is_active,
                    active_background_color,
                    inactive_background_color,
                },
                |tab, _, _, cx| cx.new(|_| tab.clone()),
            )
            .drag_over::<DraggedProjectTab>(move |element, dragged_tab, _, cx| {
                let element = element
                    .bg(cx.theme().colors().drop_target_background)
                    .border_color(cx.theme().colors().drop_target_border)
                    .border_0();
                if ix < dragged_tab.ix {
                    element.border_l_2()
                } else if ix > dragged_tab.ix {
                    element.border_r_2()
                } else {
                    element
                }
            })
            .on_drop({
                let multi_workspace = multi_workspace.clone();
                move |dragged_tab: &DraggedProjectTab, _, cx| {
                    multi_workspace
                        .update(cx, |multi_workspace, cx| {
                            multi_workspace.move_workspace(dragged_tab.ix, ix, cx)
                        })
                        .log_err();
                }
            })
            .on_click({
                let multi_workspace = multi_workspace.clone();
                let workspace = workspace.clone();
                move |_, window, cx| {
                    multi_workspace
                        .update(cx, |multi_workspace, cx| {
                            multi_workspace.activate(workspace.clone(), None, window, cx)
                        })
                        .log_err();
                }
            })
            .on_mouse_up(MouseButton::Middle, {
                let close = close.clone();
                move |_, window, cx| close(window, cx)
            })
            .child(
                Label::new(title)
                    .size(LabelSize::Small)
                    .truncate()
                    .color(if is_active {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .when(is_edited, |this| {
                this.child(
                    div()
                        .debug_selector(move || format!("photon-project-tab-dirty-{ix}"))
                        .child(Indicator::dot().color(Color::Modified)),
                )
            })
            .map(|this| match show_close_button {
                ShowCloseButton::Hidden => this,
                _ => this.child(
                    div()
                        .absolute()
                        .top_2()
                        .w_4()
                        .h_4()
                        .map(|this| match close_side {
                            ClosePosition::Left => this.left_1(),
                            ClosePosition::Right => this.right_1(),
                        })
                        .child(
                            IconButton::new("close", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .icon_color(Color::Muted)
                                .icon_size(IconSize::XSmall)
                                .on_click({
                                    let close = close.clone();
                                    move |_, window, cx| close(window, cx)
                                })
                                .map(|this| match show_close_button {
                                    ShowCloseButton::Hover => this.visible_on_hover("tab"),
                                    _ => this,
                                }),
                        ),
                ),
            })
            .into_any();

        let menu = right_click_menu(("photon-project-tab-menu", ix))
            .trigger(|_, _, _| tab)
            .menu(move |window, cx| {
                let close = close.clone();
                let close_others = {
                    let multi_workspace = multi_workspace.clone();
                    let workspace = workspace.clone();
                    move |window: &mut Window, cx: &mut App| {
                        multi_workspace
                            .update(cx, |multi_workspace, cx| {
                                multi_workspace.activate(workspace.clone(), None, window, cx);
                                let others = multi_workspace
                                    .workspaces()
                                    .filter(|held| *held != &workspace)
                                    .cloned()
                                    .collect::<Vec<_>>();
                                multi_workspace
                                    .remove(others, RemovalIntent::CloseProject, window, cx)
                                    .detach_and_log_err(cx);
                            })
                            .log_err();
                    }
                };
                let move_to_new_window = {
                    let multi_workspace = multi_workspace.clone();
                    let workspace = workspace.clone();
                    move |window: &mut Window, cx: &mut App| {
                        multi_workspace
                            .update(cx, |multi_workspace, cx| {
                                move_tab_to_new_window(
                                    multi_workspace,
                                    workspace.clone(),
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                    }
                };
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.entry("Close Tab", None, move |window, cx| close(window, cx))
                        .entry("Close Other Tabs", None, move |window, cx| {
                            close_others(window, cx)
                        })
                        .entry("Move Tab to New Window", None, move |window, cx| {
                            move_to_new_window(window, cx)
                        })
                })
            });

        div()
            .flex_1()
            .min_w(window.rem_size() * 10)
            .when(is_active, |this| this.bg(active_background_color))
            .border_t_1()
            .border_color(if is_active {
                active_background_color
            } else {
                cx.theme().colors().border
            })
            .child(menu)
            .into_any_element()
    }
}

/// Reopens the tab's project in its own window, then closes the tab here.
pub fn move_tab_to_new_window(
    multi_workspace: &mut MultiWorkspace,
    workspace: Entity<Workspace>,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    let key = workspace.read(cx).project_group_key(cx);
    let open = multi_workspace.open_project_group_in_new_window(&key, window, cx);
    cx.spawn_in(window, async move |this, cx| {
        open.await?;
        this.update_in(cx, |this, window, cx| {
            close_tab(this, workspace, window, cx)
        })
    })
    .detach_and_log_err(cx);
}

impl Render for ProjectTabs {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return h_flex().into_any_element();
        };
        let (workspaces, active) = {
            let multi_workspace = multi_workspace.read(cx);
            (
                multi_workspace.workspaces().cloned().collect::<Vec<_>>(),
                multi_workspace.workspace().clone(),
            )
        };
        if workspaces.len() < 2 {
            return h_flex().into_any_element();
        }

        let active_background_color = cx.theme().colors().title_bar_background;
        let inactive_background_color = cx.theme().colors().tab_bar_background;
        let number_of_tabs = workspaces.len();
        let tabs = workspaces
            .into_iter()
            .enumerate()
            .map(|(ix, workspace)| {
                let is_active = workspace == active;
                self.render_tab(
                    ix,
                    workspace,
                    is_active,
                    active_background_color,
                    inactive_background_color,
                    window,
                    cx,
                )
            })
            .collect::<Vec<_>>();
        let entity = cx.entity();

        h_flex()
            .w_full()
            .h(Tab::container_height(cx))
            .bg(inactive_background_color)
            .child(
                h_flex()
                    .id("photon-project-tabs")
                    .w_full()
                    .h(Tab::container_height(cx))
                    .bg(inactive_background_color)
                    .overflow_x_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(tabs)
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, _, cx| {
                                entity.update(cx, |this, cx| {
                                    let width = bounds.size.width / number_of_tabs as f32;
                                    if width != this.measured_tab_width {
                                        this.measured_tab_width = width;
                                        cx.notify();
                                    }
                                });
                            },
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .child(
                h_flex()
                    .h_full()
                    .px(DynamicSpacing::Base06.rems(cx))
                    .border_t_1()
                    .border_l_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        IconButton::new("photon-new-tab", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(NewTab), cx)),
                    ),
            )
            .into_any_element()
    }
}

impl Render for DraggedProjectTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(Tab::container_height(cx))
            .w(self.width)
            .px(DynamicSpacing::Base16.px(cx))
            .justify_center()
            .bg(if self.is_active {
                self.active_background_color
            } else {
                self.inactive_background_color
            })
            .border_1()
            .border_color(cx.theme().colors().border)
            .font(ThemeSettings::get_global(cx).ui_font.clone())
            .child(
                Label::new(self.title.clone())
                    .size(LabelSize::Small)
                    .truncate()
                    .color(if self.is_active {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{Focusable as _, Modifiers, TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use workspace::item::test::TestItem;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    async fn window_with_projects(
        roots: &[&str],
        cx: &mut TestAppContext,
    ) -> (
        Entity<MultiWorkspace>,
        Entity<ProjectTabs>,
        VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let mut projects = Vec::new();
        for root in roots {
            if root.is_empty() {
                projects.push(Project::test(fs.clone(), [], cx).await);
            } else {
                fs.insert_tree(root, json!({ "file.txt": "" })).await;
                projects.push(Project::test(fs.clone(), [root.as_ref()], cx).await);
            }
        }
        let mut projects = projects.into_iter();
        let first = projects.next().expect("at least one project");
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(first, window, cx));
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let multi_workspace = window.root(&mut cx).expect("window root");
        for project in projects {
            multi_workspace.update_in(&mut cx, |mw, window, cx| {
                mw.test_add_workspace(project, window, cx);
            });
        }
        let tabs = cx.new(|cx| ProjectTabs::new(multi_workspace.downgrade(), cx));
        show_tabs(&multi_workspace, &tabs, &mut cx);
        (multi_workspace, tabs, cx)
    }

    /// Puts the strip in every workspace's title bar, as the app's title bar does.
    fn show_tabs(
        multi_workspace: &Entity<MultiWorkspace>,
        tabs: &Entity<ProjectTabs>,
        cx: &mut VisualTestContext,
    ) {
        let workspaces =
            multi_workspace.read_with(cx, |mw, _| mw.workspaces().cloned().collect::<Vec<_>>());
        for workspace in workspaces {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.set_titlebar_item(tabs.clone().into(), window, cx);
            });
        }
        cx.run_until_parked();
    }

    fn labels(multi_workspace: &Entity<MultiWorkspace>, cx: &mut VisualTestContext) -> Vec<String> {
        multi_workspace.read_with(cx, |mw, cx| {
            mw.workspaces()
                .map(|workspace| tab_label(workspace.read(cx), cx).to_string())
                .collect()
        })
    }

    fn active_label(
        multi_workspace: &Entity<MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) -> String {
        multi_workspace.read_with(cx, |mw, cx| {
            tab_label(mw.workspace().read(cx), cx).to_string()
        })
    }

    fn click_tab(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx.debug_bounds(selector).expect("tab is drawn");
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    fn close(multi_workspace: &Entity<MultiWorkspace>, label: &str, cx: &mut VisualTestContext) {
        multi_workspace.update_in(cx, |mw, window, cx| {
            let workspace = mw
                .workspaces()
                .find(|workspace| tab_label(workspace.read(cx), cx) == label)
                .cloned()
                .expect("tab exists");
            close_tab(mw, workspace, window, cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn tab_labels_follow_projects(cx: &mut TestAppContext) {
        let (multi_workspace, _tabs, mut cx) =
            window_with_projects(&["/a/alpha", "/b/beta"], cx).await;
        assert_eq!(labels(&multi_workspace, &mut cx), ["alpha", "beta"]);
    }

    #[gpui::test]
    async fn empty_projects_are_separate_tabs(cx: &mut TestAppContext) {
        let (multi_workspace, _tabs, mut cx) = window_with_projects(&["", ""], cx).await;
        assert_eq!(
            labels(&multi_workspace, &mut cx),
            ["empty project", "empty project"]
        );
        assert!(cx.debug_bounds("photon-project-tab-1").is_some());
    }

    #[gpui::test]
    async fn strip_hidden_with_one_project(cx: &mut TestAppContext) {
        let (multi_workspace, tabs, mut cx) = window_with_projects(&["/a"], cx).await;
        assert!(cx.debug_bounds("photon-project-tab-0").is_none());

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/b", json!({ "file.txt": "" })).await;
        let project = Project::test(fs, ["/b".as_ref()], &mut cx).await;
        multi_workspace.update_in(&mut cx, |mw, window, cx| {
            mw.test_add_workspace(project, window, cx);
        });
        show_tabs(&multi_workspace, &tabs, &mut cx);
        assert!(cx.debug_bounds("photon-project-tab-0").is_some());
    }

    #[gpui::test]
    async fn dirty_project_shows_dot(cx: &mut TestAppContext) {
        let (multi_workspace, _tabs, mut cx) = window_with_projects(&["/a", "/b"], cx).await;
        assert!(cx.debug_bounds("photon-project-tab-dirty-1").is_none());

        let workspace_b = multi_workspace.read_with(&cx, |mw, _| mw.workspace().clone());
        let item = cx.new(|cx| TestItem::new(cx).with_dirty(true));
        workspace_b.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("photon-project-tab-dirty-1").is_some());

        item.update(&mut cx, |item, cx| {
            item.is_dirty = false;
            cx.emit(workspace::item::ItemEvent::UpdateTab);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("photon-project-tab-dirty-1").is_none());
    }

    #[gpui::test]
    async fn closing_active_tab_activates_neighbour(cx: &mut TestAppContext) {
        let (multi_workspace, _tabs, mut cx) = window_with_projects(&["/a", "/b", "/c"], cx).await;
        click_tab("photon-project-tab-1", &mut cx);
        assert_eq!(active_label(&multi_workspace, &mut cx), "b");

        close(&multi_workspace, "b", &mut cx);
        assert_eq!(labels(&multi_workspace, &mut cx), ["a", "c"]);
        assert_eq!(active_label(&multi_workspace, &mut cx), "c");

        close(&multi_workspace, "c", &mut cx);
        assert_eq!(labels(&multi_workspace, &mut cx), ["a"]);
        assert_eq!(active_label(&multi_workspace, &mut cx), "a");
    }

    #[gpui::test]
    async fn close_tab_with_unsaved_changes_can_be_cancelled(cx: &mut TestAppContext) {
        let (multi_workspace, _tabs, mut cx) = window_with_projects(&["/a", "/b"], cx).await;
        let workspace_b = multi_workspace.read_with(&cx, |mw, _| mw.workspace().clone());
        let item = cx.new(|cx| TestItem::new(cx).with_dirty(true));
        workspace_b.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
        });

        close(&multi_workspace, "b", &mut cx);
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert_eq!(labels(&multi_workspace, &mut cx), ["a", "b"]);
        assert_eq!(active_label(&multi_workspace, &mut cx), "b");
        assert!(item.read_with(&cx, |item, _| item.is_dirty));
    }

    #[gpui::test]
    async fn switching_tab_moves_focus(cx: &mut TestAppContext) {
        let (multi_workspace, _tabs, mut cx) = window_with_projects(&["/a", "/b"], cx).await;
        let workspace_a = multi_workspace.read_with(&cx, |mw, _| {
            mw.workspaces().next().cloned().expect("first tab")
        });
        let item = cx.new(|cx| TestItem::new(cx));
        workspace_a.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
        });

        click_tab("photon-project-tab-0", &mut cx);
        assert_eq!(active_label(&multi_workspace, &mut cx), "a");
        let focused = cx.update(|window, cx| item.read(cx).focus_handle(cx).is_focused(window));
        assert!(focused, "the active item of the new tab takes focus");
    }

    #[gpui::test]
    async fn background_tab_keeps_its_items(cx: &mut TestAppContext) {
        let (multi_workspace, _tabs, mut cx) = window_with_projects(&["/a", "/b"], cx).await;
        let workspace_a = multi_workspace.read_with(&cx, |mw, _| {
            mw.workspaces().next().cloned().expect("first tab")
        });
        let item = cx.new(|cx| TestItem::new(cx));
        workspace_a.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
        });

        click_tab("photon-project-tab-1", &mut cx);
        click_tab("photon-project-tab-0", &mut cx);
        let active_item = workspace_a.read_with(&cx, |workspace, cx| {
            workspace.active_item(cx).map(|item| item.item_id())
        });
        assert_eq!(active_item, Some(item.entity_id()));
    }
}
