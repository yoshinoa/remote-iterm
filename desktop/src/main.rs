#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod connection;
mod geometry;
mod keys;
mod model;
mod mouse;
mod prediction;
mod protocol;
mod ui;

use gpui::{App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*, px, size};

fn main() {
    let cfg = config::with_env_overrides(config::load(&config::config_path()));
    Application::new().run(move |cx: &mut App| {
        let bounds = match cfg.window {
            Some(w) => Bounds::new(point(px(w.x), px(w.y)), size(px(w.width), px(w.height))),
            None => Bounds::centered(None, size(px(1100.0), px(700.0)), cx),
        };
        let window_bounds = match cfg.window {
            Some(w) if w.maximized => WindowBounds::Maximized(bounds),
            _ => WindowBounds::Windowed(bounds),
        };
        let options = WindowOptions {
            window_bounds: Some(window_bounds),
            titlebar: Some(TitlebarOptions { title: Some("Remote iTerm".into()), ..Default::default() }),
            ..Default::default()
        };
        let cfg = cfg.clone();
        cx.open_window(options, move |window, cx| cx.new(|cx| ui::root::RootView::new(cfg, window, cx)))
            .expect("open window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
