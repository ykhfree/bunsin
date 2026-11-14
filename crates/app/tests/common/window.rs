//! Opens the main window headless on the engine of a rig, and reads what it shows.
#![allow(dead_code)]

use super::Rig;
use bunsin_app::bridge::spawn_bridge;
use bunsin_app::shell::Shell;
use gpui_kit::test::{TestQueryExt, TestWindowExt};
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Entity, Point, TestAppContext, Window, WindowBounds, WindowOptions, px,
    size,
};
use std::time::{Duration, Instant};

/// The bridge refresh interval in these tests. It is short, so a wait can outlast it.
pub const REFRESH: Duration = Duration::from_millis(50);

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

/// Waits in real time, because the engine runs on tokio threads. Fails after 10 seconds.
pub fn wait_until(cx: &mut TestAppContext, shell: &Entity<Shell>, what: &str, done: impl Fn(&Shell) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if shell.read_with(cx, |shell, _| done(shell)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Opens the main window on the engine of `rig` and waits for the first snapshot.
/// The window has GPUI's default size.
pub fn open(cx: &mut TestAppContext, rt: &tokio::runtime::Runtime, rig: &Rig) -> (AnyWindowHandle, Entity<Shell>) {
    // The engine wakes the view from tokio threads, as in the real app. Tell the test scheduler.
    cx.executor().allow_parking();
    let (commands, states) = spawn_bridge(rt.handle(), rig.engine.clone(), REFRESH);
    cx.update(gpui_kit::init);
    let (window, shell) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1536.), px(1095.)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Shell::new(states, commands, window, cx))
        })
        .unwrap()
    });
    wait_until(cx, &shell, "the first snapshot", |s| s.state.snapshot.is_some());
    (window, shell)
}

/// Runs `f` on a freshly drawn frame of the window.
pub fn on_screen<R>(cx: &mut TestAppContext, window: AnyWindowHandle, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        f(window, cx)
    })
    .unwrap()
}

pub fn label(window: &Window, id: &'static str) -> String {
    window.find(id).label().unwrap_or_default().to_owned()
}

pub fn value(window: &Window, id: &'static str) -> String {
    window.find(id).value().unwrap_or_default().to_owned()
}

/// Every accessibility text of the window: labels, values, descriptions and placeholders.
pub fn screen_texts(window: &Window) -> Vec<String> {
    window
        .elements()
        .iter()
        .flat_map(|e| [e.label(), e.value(), e.description(), e.placeholder()])
        .flatten()
        .map(str::to_owned)
        .collect()
}
