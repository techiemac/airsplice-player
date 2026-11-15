use std::path::PathBuf;

use gpui::{
    App, Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*,
    px, size,
};
use airsplice_player::ui::app::{self, MediaPlayer, OpenFiles, OpenSettings, Quit};
use airsplice_player::ui::icons::Assets;
use airsplice_player::ui::session::SessionStore;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,symphonia_core=error")).init();
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    Application::new().with_assets(Assets).run(move |cx: &mut App| {
        app::bind_keys(cx);
        cx.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![
            Menu {
                name: "airsplice-player".into(),
                items: vec![MenuItem::action("Settings…", OpenSettings), MenuItem::separator(), MenuItem::action("Quit", Quit)],
            },
            Menu { name: "File".into(), items: vec![MenuItem::action("Open…", OpenFiles)] },
        ]);

        let bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("airsplice-player".into()),
                        // macOS draws the traffic lights over our own title bar,
                        // like Zed. Elsewhere the system title bar is kept.
                        appears_transparent: cfg!(target_os = "macos"),
                        traffic_light_position: Some(point(px(12.), px(11.))),
                    }),
                    window_min_size: Some(size(px(720.), px(440.))),
                    app_id: Some("airsplice-player".into()),
                    focus: true,
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        let mut player = MediaPlayer::new(files, SessionStore::default_location(), window, cx);
                        player.enable_os_media(window);
                        player
                    })
                },
            )
            .expect("failed to open the main window");
        let _ = window;
        // Closing the main window quits; closing the external output does not.
        cx.on_window_closed(|cx| {
            if !cx.windows().iter().any(|w| w.downcast::<MediaPlayer>().is_some()) {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
