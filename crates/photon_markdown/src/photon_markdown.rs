use gpui::{DismissEvent, EventEmitter, FocusHandle, Focusable, ImageSource, ObjectFit, img, px};
use ui::prelude::*;
use workspace::{ModalView, Workspace};

/// Shows an image or diagram from a Markdown preview at the size of the window.
pub struct ImageLightbox {
    source: ImageSource,
    name: SharedString,
    focus_handle: FocusHandle,
}

pub fn open_image(
    workspace: &mut Workspace,
    source: ImageSource,
    name: SharedString,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, |_, cx| ImageLightbox {
        source,
        name,
        focus_handle: cx.focus_handle(),
    });
}

impl ImageLightbox {
    pub fn name(&self) -> &SharedString {
        &self.name
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ImageLightbox {}

impl ModalView for ImageLightbox {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Focusable for ImageLightbox {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImageLightbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        // The modal layer places modals 80px from the top; leave room below too.
        let height = (viewport.height - px(120.)).max(px(200.));
        let name = self.name.clone();
        v_flex()
            .key_context("ImageLightbox")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::dismiss))
            .elevation_3(cx)
            .overflow_hidden()
            .w(viewport.width * 0.9)
            .h(height)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new(self.name.clone()).truncate())
                    .child(
                        div()
                            .debug_selector(|| "photon-image-lightbox-close".into())
                            .child(
                                IconButton::new("photon-image-lightbox-close", IconName::Close)
                                    .icon_size(IconSize::Small)
                                    .tooltip(ui::Tooltip::text("Close"))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            ),
                    ),
            )
            .child(
                div().flex_1().min_h_0().p_2().child(
                    img(self.source.clone())
                        .size_full()
                        .object_fit(ObjectFit::Contain)
                        .with_fallback(move || {
                            Label::new(format!("Couldn't load {name}"))
                                .color(Color::Warning)
                                .into_any_element()
                        }),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use project::Project;
    use workspace::{AppState, MultiWorkspace};

    fn open_lightbox(workspace: &gpui::Entity<Workspace>, cx: &mut VisualTestContext) {
        workspace.update_in(cx, |workspace, window, cx| {
            open_image(
                workspace,
                ImageSource::Image(std::sync::Arc::new(gpui::Image::empty())),
                "Architecture".into(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
    }

    fn is_open(workspace: &gpui::Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<ImageLightbox>(cx).is_some()
        })
    }

    #[gpui::test]
    async fn escape_and_the_close_button_close_the_image(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let project = Project::test(project::FakeFs::new(cx.executor()), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

        open_lightbox(&workspace, cx);
        assert!(is_open(&workspace, cx));
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
        assert!(!is_open(&workspace, cx));

        open_lightbox(&workspace, cx);
        let close = cx
            .debug_bounds("photon-image-lightbox-close")
            .expect("close button is drawn")
            .center();
        cx.simulate_click(close, Modifiers::none());
        cx.run_until_parked();
        assert!(!is_open(&workspace, cx));
    }
}
