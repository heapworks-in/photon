use std::borrow::Cow;
use std::cell::RefCell;
use std::ptr;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{
    AnyElement, App, AssetSource, Bounds, Context, FocusHandle, Focusable as _, Global,
    ImageSource, IntoElement as _, ObjectFit, ParentElement as _, Pixels, RenderImage,
    SharedString, Styled as _, StyledImage as _, Subscription, Task, TaskExt as _, Window, canvas,
    div, img,
};
use objc2::rc::Retained;
use objc2_app_kit::NSImage;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::NSError;
use ui::{Color, Label, LabelCommon as _, LabelSize, h_flex};
use util::ResultExt as _;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::http::{self, StatusCode, header::CONTENT_TYPE};
use wry::{WebContext, WebView, WebViewBuilder, WebViewExtMacOS as _};

use crate::{
    Action, BRIDGE_JS, CSP, DROP_AFTER, FromPage, Lifecycle, NETWORK_GUARD_JS, PageConnection,
    SCHEME, ServeError, ToPage, WebEditor, WebEditorRegistry, WebEditorView, serve,
};

enum PageEvent {
    Message(String),
    Snapshot(Option<Retained<CGImage>>),
}

/// One WebKit context for every web editor page, alive while any page is.
#[derive(Default)]
struct SharedWebContext(Weak<RefCell<WebContext>>);

impl Global for SharedWebContext {}

fn shared_web_context(cx: &mut App) -> Rc<RefCell<WebContext>> {
    if let Some(context) = cx
        .try_global::<SharedWebContext>()
        .and_then(|shared| shared.0.upgrade())
    {
        return context;
    }
    let context = Rc::new(RefCell::new(WebContext::new(None)));
    cx.set_global(SharedWebContext(Rc::downgrade(&context)));
    context
}

pub struct NativePage {
    webview: WebView,
    events: mpsc::UnboundedSender<PageEvent>,
    _web_context: Rc<RefCell<WebContext>>,
}

impl NativePage {
    fn new(
        editor: WebEditor,
        assets: Arc<dyn AssetSource>,
        events: mpsc::UnboundedSender<PageEvent>,
        window: &Window,
        cx: &mut App,
    ) -> Result<Self> {
        let web_context = shared_web_context(cx);
        let url = format!("{SCHEME}://{}/{}", editor.id, editor.entry);
        let scheme_prefix = format!("{SCHEME}:");
        let ipc_events = events.clone();
        let webview = WebViewBuilder::new_with_web_context(&mut web_context.borrow_mut())
            .with_url(url)
            .with_visible(false)
            // Keys stay with Photon until the tab is focused and the view shown.
            .with_focused(false)
            // Photon paints the theme background underneath, so nothing flashes white while loading.
            .with_transparent(true)
            .with_initialization_script_for_main_only(NETWORK_GUARD_JS, false)
            .with_initialization_script(BRIDGE_JS)
            .with_custom_protocol(SCHEME.to_string(), move |_, request| {
                respond(&editor, request.uri(), assets.as_ref())
            })
            .with_navigation_handler(move |url| url.starts_with(&scheme_prefix))
            .with_ipc_handler(move |request| {
                if ipc_events
                    .unbounded_send(PageEvent::Message(request.into_body()))
                    .is_err()
                {
                    log::debug!("web editor message arrived after its tab closed");
                }
            })
            .build_as_child(window)
            .context("creating the web view")?;
        Ok(Self {
            webview,
            events,
            _web_context: web_context,
        })
    }

    fn set_bounds(&self, bounds: Bounds<Pixels>) {
        let rect = wry::Rect {
            position: LogicalPosition::new(
                f64::from(f32::from(bounds.origin.x)),
                f64::from(f32::from(bounds.origin.y)),
            )
            .into(),
            size: LogicalSize::new(
                f64::from(f32::from(bounds.size.width)),
                f64::from(f32::from(bounds.size.height)),
            )
            .into(),
        };
        self.webview.set_bounds(rect).log_err();
    }

    fn show(&self) {
        self.webview.set_visible(true).log_err();
        self.webview.focus().log_err();
    }

    fn hide(&self) {
        self.take_snapshot();
        self.webview.set_visible(false).log_err();
        self.webview.focus_parent().log_err();
    }

    fn take_snapshot(&self) {
        let events = self.events.clone();
        let handler = block2::RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
            // SAFETY: WebKit passes null or an image that stays alive for the duration of this call;
            // a null proposed rect means "the whole image", and no context or hints are needed.
            let image = unsafe { image.as_ref() }.and_then(|image| unsafe {
                image.CGImageForProposedRect_context_hints(ptr::null_mut(), None, None)
            });
            if events.unbounded_send(PageEvent::Snapshot(image)).is_err() {
                log::debug!("web editor snapshot arrived after its tab closed");
            }
        });
        // SAFETY: we are on the main thread (WKWebView is main-thread only), a nil configuration
        // means "the visible bounds", and WebKit copies the block before returning.
        unsafe {
            self.webview
                .webview()
                .takeSnapshotWithConfiguration_completionHandler(None, &handler);
        }
    }
}

impl PageConnection for NativePage {
    fn send(&self, message: ToPage) {
        // The bridge takes a JSON string, so the JSON is passed as a JS string literal.
        let script = serde_json::to_string(&message)
            .and_then(|json| serde_json::to_string(&json))
            .map(|literal| format!("window.photon._receive({literal})"));
        match script {
            Ok(script) => {
                self.webview.evaluate_script(&script).log_err();
            }
            Err(error) => log::error!("could not encode a message for a web editor: {error}"),
        }
    }
}

fn respond(
    editor: &WebEditor,
    uri: &http::Uri,
    assets: &dyn AssetSource,
) -> http::Response<Cow<'static, [u8]>> {
    let result = if uri.host() == Some(editor.id.as_ref()) {
        let path = uri.path();
        let path = path.split(['?', '#']).next().unwrap_or(path);
        serve(editor, path.trim_start_matches('/'), assets)
    } else {
        Err(ServeError::OutsideEditor)
    };
    let (status, mime, body) = match result {
        Ok(served) => (StatusCode::OK, served.mime, served.bytes),
        Err(ServeError::NotFound) => (StatusCode::NOT_FOUND, "text/plain", Cow::Borrowed(&[][..])),
        Err(ServeError::OutsideEditor) => {
            (StatusCode::FORBIDDEN, "text/plain", Cow::Borrowed(&[][..]))
        }
        Err(error) => {
            log::error!("web editor {}: could not serve {uri}: {error}", editor.id);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain",
                Cow::Borrowed(&[][..]),
            )
        }
    };
    http::Response::builder()
        .status(status)
        .header(CONTENT_TYPE, mime)
        .header("Content-Security-Policy", CSP)
        .header("X-DNS-Prefetch-Control", "off")
        .body(body)
        .unwrap_or_else(|error| {
            log::error!(
                "web editor {}: could not build a response: {error}",
                editor.id
            );
            let mut response = http::Response::new(Cow::Borrowed(&[][..]));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
}

struct LivePage {
    page: Rc<NativePage>,
    _events: Task<()>,
}

pub(crate) struct NativeHost {
    lifecycle: Lifecycle,
    page: Option<LivePage>,
    snapshot: Option<Arc<RenderImage>>,
    error: Option<SharedString>,
    drop_check: Option<Task<()>>,
}

impl NativeHost {
    pub(crate) fn new() -> Self {
        Self {
            lifecycle: Lifecycle::default(),
            page: None,
            snapshot: None,
            error: None,
            drop_check: None,
        }
    }
}

pub(crate) fn subscribe(
    focus_handle: &FocusHandle,
    window: &mut Window,
    cx: &mut Context<WebEditorView>,
) -> Vec<Subscription> {
    vec![
        cx.on_focus_in(focus_handle, window, |_, _, cx| cx.notify()),
        // Handled here rather than at the next prepaint: when another tab takes focus this view is
        // not drawn again, but its native view must not stay on top.
        cx.on_focus_out(focus_handle, window, |view, _, window, cx| {
            on_frame(view, false, None, window, cx);
        }),
        cx.on_release_in(window, |view, window, _| {
            if let Some(snapshot) = view.native.snapshot.take() {
                window.drop_image(snapshot).log_err();
            }
        }),
    ]
}

/// Called when the tab is no longer the active one in its pane.
pub(crate) fn hidden(
    view: &mut WebEditorView,
    window: &mut Window,
    cx: &mut Context<WebEditorView>,
) {
    on_frame(view, false, None, window, cx);
}

/// The native view draws above everything in the window, so it is shown only while the tab has
/// focus; overlays that take focus (palettes, menus, modals) therefore draw over the snapshot.
/// Overlays that don't take focus (tooltips, toasts, tab drop targets) can end up under a focused
/// live diagram.
pub(crate) fn element(view: &WebEditorView, cx: &mut Context<WebEditorView>) -> AnyElement {
    let this = cx.entity().downgrade();
    let backdrop = match view.native.snapshot.clone() {
        Some(snapshot) => img(ImageSource::Render(snapshot))
            .size_full()
            .object_fit(ObjectFit::Fill)
            .into_any_element(),
        None => h_flex()
            .size_full()
            .justify_center()
            .child(
                Label::new("Click to edit")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element(),
    };
    div()
        .absolute()
        .size_full()
        .child(backdrop)
        .children(
            view.native
                .error
                .clone()
                .map(|error| div().p_4().child(Label::new(error).color(Color::Error))),
        )
        .child(
            canvas(
                move |bounds, window, cx| {
                    this.update(cx, |view, cx| {
                        on_frame(view, true, Some(bounds), window, cx)
                    })
                    .log_err();
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .into_any_element()
}

fn on_frame(
    view: &mut WebEditorView,
    visible: bool,
    bounds: Option<Bounds<Pixels>>,
    window: &mut Window,
    cx: &mut Context<WebEditorView>,
) {
    let focused = view.focus_handle(cx).contains_focused(window, cx);
    let now = Instant::now();
    let mut action = view.native.lifecycle.on_frame(visible, focused, now);
    if action == Action::Create {
        view.native.error = create(view, window, cx)
            .err()
            .map(|error| format!("Could not open the editor: {error:#}").into());
        if view.native.error.is_some() {
            cx.notify();
        }
        action = view.native.lifecycle.on_frame(visible, focused, now);
    }

    if let (Some(live), Some(bounds)) = (&view.native.page, bounds) {
        live.page.set_bounds(bounds);
    }
    match action {
        Action::ShowNative => {
            view.native.drop_check = None;
            if let Some(live) = &view.native.page {
                live.page.show();
            }
        }
        Action::ShowSnapshot => {
            if let Some(live) = &view.native.page {
                live.page.hide();
            }
        }
        Action::Drop => {
            view.native.page = None;
            view.set_page(None);
        }
        Action::None | Action::Create => {}
    }

    // One timer while the page exists but isn't shown live; showing it cancels the timer.
    if view.native.page.is_some()
        && !view.native.lifecycle.is_native()
        && view.native.drop_check.is_none()
    {
        view.native.drop_check = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DROP_AFTER).await;
            this.update_in(cx, |view, window, cx| {
                view.native.drop_check = None;
                on_frame(view, false, None, window, cx);
            })
            .log_err();
        }));
    }
}

fn create(
    view: &mut WebEditorView,
    window: &mut Window,
    cx: &mut Context<WebEditorView>,
) -> Result<()> {
    let editor_id = view.document().read(cx).editor_id().clone();
    let editor = cx
        .try_global::<WebEditorRegistry>()
        .and_then(|registry| registry.editor(&editor_id))
        .cloned()
        .with_context(|| format!("the web editor {editor_id:?} is not installed"))?;
    let assets = cx.asset_source().clone();
    let (sender, mut receiver) = mpsc::unbounded();
    let page = Rc::new(NativePage::new(editor, assets, sender, window, cx)?);
    let events = cx.spawn_in(window, async move |this, cx| {
        while let Some(event) = receiver.next().await {
            if this
                .update_in(cx, |view, window, cx| handle_event(view, event, window, cx))
                .is_err()
            {
                break;
            }
        }
    });
    view.native.page = Some(LivePage {
        page: page.clone(),
        _events: events,
    });
    view.set_page(Some(page));
    Ok(())
}

fn handle_event(
    view: &mut WebEditorView,
    event: PageEvent,
    window: &mut Window,
    cx: &mut Context<WebEditorView>,
) {
    match event {
        PageEvent::Message(text) => match serde_json::from_str::<FromPage>(&text) {
            Ok(message) => view.handle_page_message(message, window, cx),
            Err(error) => log::warn!("dropping an invalid web editor message: {error}"),
        },
        PageEvent::Snapshot(None) => log::warn!("web editor snapshot failed"),
        PageEvent::Snapshot(Some(image)) => {
            cx.spawn_in(window, async move |this, cx| {
                let snapshot = cx
                    .background_executor()
                    .spawn(async move { decode_snapshot(&image) })
                    .await?;
                this.update_in(cx, |view, window, cx| {
                    if let Some(previous) = view.native.snapshot.replace(snapshot) {
                        window.drop_image(previous).log_err();
                    }
                    cx.notify();
                })
            })
            .detach_and_log_err(cx);
        }
    }
}

fn decode_snapshot(image: &CGImage) -> Result<Arc<RenderImage>> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    let bytes_per_row = width.checked_mul(4).context("snapshot too wide")?;
    let mut pixels = vec![
        0u8;
        bytes_per_row
            .checked_mul(height)
            .context("snapshot too large")?
    ];
    let color_space = CGColorSpace::new_device_rgb().context("creating a color space")?;
    // Little-endian 32-bit with alpha first is BGRA in memory, the order GPUI images use.
    let bitmap_info =
        CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;
    // SAFETY: `pixels` holds `height` rows of `bytes_per_row` bytes and outlives the context,
    // which is dropped at the end of this block before `pixels` is moved.
    {
        let context = unsafe {
            CGBitmapContextCreate(
                pixels.as_mut_ptr().cast(),
                width,
                height,
                8,
                bytes_per_row,
                Some(&color_space),
                bitmap_info,
            )
        }
        .context("creating a bitmap context")?;
        CGContext::draw_image(
            Some(&context),
            CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64)),
            Some(image),
        );
    }
    let buffer = image::RgbaImage::from_raw(u32::try_from(width)?, u32::try_from(height)?, pixels)
        .context("snapshot buffer size")?;
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WebEditorSource;

    fn editor(root: &std::path::Path) -> WebEditor {
        WebEditor {
            id: "sketch".into(),
            extension_id: "sketch".into(),
            name: "Sketch".into(),
            file_suffixes: vec!["sketch".into()],
            entry: "index.html".into(),
            source: WebEditorSource::Directory(root.to_path_buf()),
            built_in: false,
        }
    }

    struct NoAssets;

    impl AssetSource for NoAssets {
        fn load(&self, _path: &str) -> Result<Option<Cow<'static, [u8]>>> {
            Ok(None)
        }

        fn list(&self, _path: &str) -> Result<Vec<gpui::SharedString>> {
            Ok(Vec::new())
        }
    }

    fn request(editor: &WebEditor, uri: &'static str) -> http::Response<Cow<'static, [u8]>> {
        respond(editor, &http::Uri::from_static(uri), &NoAssets)
    }

    #[test]
    fn scheme_requests_map_to_editor_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("index.html"), "<html>")?;
        std::fs::create_dir(directory.path().join("assets"))?;
        std::fs::write(directory.path().join("assets/app.js"), "1")?;
        let editor = editor(directory.path());

        let response = request(&editor, "photon-web://sketch/index.html?v=2");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_ref(), b"<html>");
        assert_eq!(response.headers()[CONTENT_TYPE], "text/html");
        assert_eq!(response.headers()["Content-Security-Policy"], CSP);
        assert_eq!(response.headers()["X-DNS-Prefetch-Control"], "off");

        let response = request(&editor, "photon-web://sketch/index.html#scene");
        assert_eq!(response.status(), StatusCode::OK);

        let response = request(&editor, "photon-web://sketch/assets/app.js");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/javascript");
        Ok(())
    }

    #[test]
    fn scheme_refusals_have_status_codes_and_the_policy() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let editor = editor(directory.path());
        for (uri, status) in [
            ("photon-web://sketch/missing.html", StatusCode::NOT_FOUND),
            ("photon-web://sketch/%2e%2e/secret", StatusCode::FORBIDDEN),
            ("photon-web://other/index.html", StatusCode::FORBIDDEN),
        ] {
            let response = request(&editor, uri);
            assert_eq!(response.status(), status, "{uri}");
            assert_eq!(response.headers()["Content-Security-Policy"], CSP, "{uri}");
        }
        Ok(())
    }
}
