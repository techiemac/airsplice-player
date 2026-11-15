//! The external output: a fullscreen window on another display showing only
//! the picture, while the controls stay in the main window.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyWindowHandle, App, Bounds, Context, DisplayId, Entity, MouseButton, ObjectFit, RenderImage, Subscription, WeakEntity,
    Window, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, img, rgb,
};

use super::app::MediaPlayer;

pub struct OutputView {
    player: WeakEntity<MediaPlayer>,
    main: AnyWindowHandle,
    /// The image last drawn here. Each window uploads images to its own
    /// atlas, so this window frees its copy when the picture changes.
    shown: Option<Arc<RenderImage>>,
    _observe: Subscription,
}

/// Open the output fullscreen on `display`, showing what `player` shows.
pub fn open(display: DisplayId, player: &Entity<MediaPlayer>, main: AnyWindowHandle, cx: &mut App) -> anyhow::Result<WindowHandle<OutputView>> {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Fullscreen(Bounds::maximized(Some(display), cx))),
        display_id: Some(display),
        titlebar: None,
        // The main window keeps the keyboard.
        focus: false,
        show: true,
        kind: WindowKind::Normal,
        is_movable: false,
        is_minimizable: false,
        app_id: Some("airsplice-player".into()),
        ..Default::default()
    };
    cx.open_window(options, |_, cx| {
        cx.new(|cx| OutputView { player: player.downgrade(), main, shown: None, _observe: cx.observe(player, |_, _, cx| cx.notify()) })
    })
}

impl OutputView {
    pub fn shown(&self) -> Option<&Arc<RenderImage>> {
        self.shown.as_ref()
    }
}

impl Render for OutputView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let picture = self.player.upgrade().and_then(|p| p.read(cx).picture());
        let same = match (&self.shown, &picture) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same && let Some(old) = std::mem::replace(&mut self.shown, picture.clone()) {
            let _ = window.drop_image(old);
        }
        let main = self.main;
        div()
            .id("output")
            .relative()
            .size_full()
            .bg(rgb(0x000000))
            // Keys always go to the controls: a click here (an operator
            // reaching for them) hands the keyboard back.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                let _ = main.update(cx, |_, window, _| window.activate_window());
            })
            .when_some(picture, |el, p| el.child(img(p).absolute().inset_0().size_full().object_fit(ObjectFit::Contain)))
    }
}
