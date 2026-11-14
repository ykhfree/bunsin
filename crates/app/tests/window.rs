//! Headless window tests: real view, real engine, scripted LLM and connector.

mod common;

use bunsin_api::EngineApi;
use bunsin_app::bridge::spawn_bridge;
use bunsin_app::shell::Shell;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, Bounds, Entity, Point, TestAppContext, WindowBounds, WindowOptions, px, size};
use std::time::{Duration, Instant};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

/// Waits in real time, because the engine runs on tokio threads.
fn wait_until(cx: &mut TestAppContext, shell: &Entity<Shell>, what: &str, done: impl Fn(&Shell) -> bool) {
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

#[gpui_kit::test]
fn the_window_shows_engine_state_and_sends_commands(cx: &mut TestAppContext) {
    // The engine wakes the view from tokio threads, as in the real app. Tell the test scheduler.
    cx.executor().allow_parking();
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (commands, states) = spawn_bridge(rt.handle(), rig.engine.clone(), Duration::from_millis(50));

    cx.update(gpui_kit::init);
    let (window, shell) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(900.), px(700.)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |_, cx| cx.new(|cx| Shell::new(states, commands, cx))).unwrap()
    });

    wait_until(cx, &shell, "the first snapshot", |s| s.state.snapshot.is_some());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("status").label(), Some("Running"));
        assert_eq!(window.find("profile-kim").label(), Some("● Kim (kim)"));
        assert_eq!(window.find("profile-lee").label(), Some("Lee (lee)"));
        assert_eq!(window.find("inbox-count").label(), Some("Approval inbox: 1"));
        assert_eq!(window.find("inbox-1").label(), Some("pending · slack/post"));
        // The destination is on screen before the user approves, not only the message text.
        let detail = window.find("detail-1").label().unwrap_or_default().to_owned();
        assert!(detail.contains("\"channel\": \"C1\""), "{detail}");
        assert!(detail.contains("We deploy at 3 pm."), "{detail}");
        window.click("approve-1", cx);
    })
    .unwrap();

    wait_until(cx, &shell, "the executed item", |s| {
        s.state
            .snapshot
            .as_ref()
            .is_some_and(|snap| snap.inbox[0].state.as_str() == "executed")
    });
    assert_eq!(rig.slack.calls().len(), 1);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("inbox-1").label(), Some("executed · slack/post"));
        // An executed item offers no buttons.
        assert!(window.try_find("approve-1").is_none());
        assert!(window.try_find("reject-1").is_none());
        window.click("kill-switch", cx);
    })
    .unwrap();

    wait_until(cx, &shell, "the kill switch", |s| {
        s.state.snapshot.as_ref().is_some_and(|snap| snap.kill_switch)
    });
    // The engine stored the switch. The window did not only change its own label.
    assert!(rt.block_on(rig.engine.snapshot("kim")).unwrap().kill_switch);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("status").label(),
            Some("Stopped: the agent makes no changes")
        );
        window.click("profile-lee", cx);
    })
    .unwrap();

    wait_until(cx, &shell, "the other profile", |s| {
        s.state.selected.as_deref() == Some("lee")
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("profile-lee").label(), Some("● Lee (lee)"));
        assert_eq!(window.find("status").label(), Some("Running"));
        assert_eq!(window.find("inbox-count").label(), Some("Approval inbox: 0"));
        assert!(window.try_find("inbox-1").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_window_shows_a_new_item_without_a_click(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (commands, states) = spawn_bridge(rt.handle(), rig.engine.clone(), Duration::from_millis(50));
    cx.update(gpui_kit::init);
    let (_window, shell) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
            cx.new(|cx| Shell::new(states, commands, cx))
        })
        .unwrap()
    });
    wait_until(cx, &shell, "the first snapshot", |s| s.state.snapshot.is_some());
    let inbox = |s: &Shell| s.state.snapshot.as_ref().map_or(0, |snapshot| snapshot.inbox.len());
    assert_eq!(shell.read_with(cx, |s, _| inbox(s)), 1);

    // An event reaches the server while the window is only open.
    rt.block_on(common::ingest_second_event(&rig));
    wait_until(cx, &shell, "the new item", |s| inbox(s) == 2);
}
