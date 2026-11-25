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
    /// The images last drawn here. Each window uploads images to its own
    /// atlas, so this window frees its copies when the picture changes.
    shown: Vec<Arc<RenderImage>>,
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
        cx.new(|cx| OutputView { player: player.downgrade(), main, shown: Vec::new(), _observe: cx.observe(player, |_, _, cx| cx.notify()) })
    })
}

impl OutputView {
    /// The picture on top: the one being shown, or faded in.
    pub fn shown(&self) -> Option<&Arc<RenderImage>> {
        self.shown.last()
    }
}

impl Render for OutputView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The picture, or during a dissolve the old one under the new.
        let layers = self.player.upgrade().map(|p| p.read(cx).layers(true)).unwrap_or_default();
        for old in std::mem::take(&mut self.shown) {
            if !layers.iter().any(|(l, _)| Arc::ptr_eq(l, &old)) {
                let _ = window.drop_image(old);
            }
        }
        self.shown = layers.iter().map(|(l, _)| l.clone()).collect();
        let main = self.main;
        div()
            .id("output")
            .relative()
            .size_full()
            // Black only around the picture (see `letterbox`).
            .when(layers.is_empty(), |el| el.bg(rgb(0x000000)))
            // Keys always go to the controls: a click here (an operator
            // reaching for them) hands the keyboard back.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                let _ = main.update(cx, |_, window, _| window.activate_window());
            })
            .children(layers.into_iter().map(|(p, opacity)| {
                div()
                    .absolute()
                    .inset_0()
                    .size_full()
                    .when(opacity < 1.0, |el| el.opacity(opacity))
                    .child(super::letterbox::bars(&p))
                    .child(img(p).absolute().inset_0().size_full().object_fit(ObjectFit::Contain))
            }))
    }
}
