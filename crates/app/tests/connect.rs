//! Headless tests of the connect screen against a real HTTP server.

mod common;

use bunsin_app::connect::{AppView, Source};
use bunsin_app::connection::{
    Connection, MemoryTokenStore, TokenStore, load_connection, save_connection,
};
use bunsin_daemon::server::{Running, ServeConfig, serve};
use gpui_kit::test::{TestQueryExt, TestWindowExt};
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Entity, Point, TestAppContext, Window, WindowBounds,
    WindowOptions, px, size,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEAD_SERVER: &str = "http://127.0.0.1:1";

/// A real server with the engine of `common::rig`, and a token for profile `kim`.
struct Server {
    rig: common::Rig,
    running: Running,
    token: String,
}

impl Server {
    fn url(&self) -> String {
        self.running.url()
    }

    fn revoke(&self) {
        let store = self.rig.engine.app_store();
        let id = store.list_tokens().unwrap()[0].id.clone();
        assert_eq!(store.revoke_token(&id).unwrap(), 1);
    }
}

struct Test {
    rt: tokio::runtime::Runtime,
    server: Server,
    dir: tempfile::TempDir,
    tokens: Arc<MemoryTokenStore>,
}

impl Test {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(async {
            let rig = common::rig().await;
            let token = rig.engine.app_store().add_token("kim", "test", 1).unwrap();
            let config = ServeConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                tls: None,
            };
            let running = serve(rig.engine.clone(), config).await.unwrap();
            Server {
                rig,
                running,
                token,
            }
        });
        Self {
            rt,
            server,
            dir: tempfile::tempdir().unwrap(),
            tokens: Arc::new(MemoryTokenStore::default()),
        }
    }

    fn saved(&self) -> Source {
        Source::Saved {
            dir: self.dir.path().to_owned(),
            tokens: self.tokens.clone(),
        }
    }

    /// Saves the server address and the good token, as an earlier connect would.
    fn save_good_connection(&self) {
        let connection = Connection {
            server: self.server.url(),
            ca_cert: None,
        };
        save_connection(self.dir.path(), &connection).unwrap();
        self.tokens.set(&self.server.token).unwrap();
    }

    fn nothing_saved(&self) -> bool {
        load_connection(self.dir.path()).is_none()
            && !self.dir.path().join("connection.json").exists()
            && self.tokens.get().unwrap().is_none()
    }

    fn open(&self, cx: &mut TestAppContext, source: Source) -> (AnyWindowHandle, Entity<AppView>) {
        // The server wakes the view from tokio threads. Tell the test scheduler.
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let handle = self.rt.handle().clone();
        cx.update(|cx| {
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(900.), px(700.)),
            };
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            };
            let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| AppView::new(handle, source, window, cx))
            })
            .unwrap();
            (window, view)
        })
    }
}

/// Waits in real time, because the server runs on tokio threads. Fails after 10 seconds.
fn wait_until(
    cx: &mut TestAppContext,
    view: &Entity<AppView>,
    what: &str,
    done: impl Fn(&AppView, &App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, cx| done(view, cx)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn shell_loaded(view: &AppView, cx: &App) -> bool {
    view.shell()
        .is_some_and(|shell| shell.read(cx).state.snapshot.is_some())
}

fn failed(view: &AppView, _: &App) -> bool {
    !view.connecting() && view.error().is_some()
}

/// Types into the form and presses Connect.
fn fill_and_connect(cx: &mut TestAppContext, window: AnyWindowHandle, server: &str, token: &str) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("server", cx);
        window.input(server, cx);
        window.click("token", cx);
        window.input(token, cx);
        window.click("connect", cx);
    })
    .unwrap();
}

/// Every accessibility text of the window: labels, values, descriptions and placeholders.
fn screen_texts(window: &Window) -> Vec<String> {
    window
        .elements()
        .iter()
        .flat_map(|e| [e.label(), e.value(), e.description(), e.placeholder()])
        .flatten()
        .map(str::to_owned)
        .collect()
}

/// Fails if `secret` is in any accessibility text. `shown` must be found, so the scan is real.
fn assert_not_on_screen(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    secret: &str,
    shown: &str,
) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let texts = screen_texts(window);
        assert!(
            texts.iter().any(|t| t.contains(shown)),
            "the scan did not find {shown:?}: {texts:?}"
        );
        let leaks: Vec<_> = texts.iter().filter(|t| t.contains(secret)).collect();
        assert!(leaks.is_empty(), "the token is on screen: {leaks:?}");
    })
    .unwrap();
}

fn label(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) -> Option<String> {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.find(id).label().map(str::to_owned)
    })
    .unwrap()
}

#[gpui_kit::test]
fn a_good_address_and_token_show_the_shell_and_are_saved(cx: &mut TestAppContext) {
    let t = Test::new();
    let (window, view) = t.open(cx, t.saved());
    let url = t.server.url();
    fill_and_connect(cx, window, &url, &t.server.token);
    wait_until(cx, &view, "the shell with a snapshot", shell_loaded);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // The token gives access to kim only, so the server lists only kim.
        assert_eq!(window.find("profile-kim").label(), Some("● Kim (kim)"));
        assert!(window.try_find("profile-lee").is_none());
        assert_eq!(window.find("inbox-1").label(), Some("pending · slack/post"));
        assert!(window.try_find("connect").is_none());
        assert!(window.try_find("disconnect").is_some());
    })
    .unwrap();

    let file = std::fs::read_to_string(t.dir.path().join("connection.json")).unwrap();
    assert!(file.contains(&url), "{file}");
    assert!(!file.contains(&t.server.token), "{file}");
    let expected = Connection {
        server: url.clone(),
        ca_cert: None,
    };
    assert_eq!(load_connection(t.dir.path()), Some(expected));
    assert_eq!(t.tokens.get().unwrap(), Some(t.server.token.clone()));
    assert_not_on_screen(cx, window, &t.server.token, "Kim (kim)");
    // Connecting only reads. The pending post did not run.
    assert!(t.server.rig.slack.calls().is_empty());
}

#[gpui_kit::test]
fn a_wrong_token_is_not_authorized_and_nothing_is_saved(cx: &mut TestAppContext) {
    let t = Test::new();
    let (window, view) = t.open(cx, t.saved());
    let url = t.server.url();
    let wrong = "bsn_wrong-token-7f3a9c";
    fill_and_connect(cx, window, &url, wrong);
    wait_until(cx, &view, "the error", failed);

    let error = view.read_with(cx, |view, _| view.error().unwrap().to_owned());
    assert!(error.contains("not authorized"), "{error}");
    assert!(!error.contains(wrong), "{error}");
    assert_eq!(label(cx, window, "connect-error"), Some(error));
    assert!(view.read_with(cx, |view, _| view.shell().is_none()));
    assert!(t.nothing_saved());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let token = window.find("token");
        assert_eq!(token.role(), Some(gpui_kit::Role::PasswordInput));
        assert_eq!(token.value(), None);
        assert_eq!(window.find("server").value(), Some(url.as_str()));
    })
    .unwrap();
    // The wrong token is still in the masked field here, and still not readable.
    assert_not_on_screen(cx, window, wrong, &url);
}

#[gpui_kit::test]
fn a_dead_address_cannot_be_reached_and_nothing_is_saved(cx: &mut TestAppContext) {
    let t = Test::new();
    let (window, view) = t.open(cx, t.saved());
    fill_and_connect(cx, window, DEAD_SERVER, &t.server.token);
    wait_until(cx, &view, "the error", failed);

    let error = label(cx, window, "connect-error").unwrap();
    assert!(error.contains("cannot be reached"), "{error}");
    assert!(view.read_with(cx, |view, _| view.shell().is_none()));
    assert!(t.nothing_saved());
    assert_not_on_screen(cx, window, &t.server.token, DEAD_SERVER);
}

#[gpui_kit::test]
fn plain_http_to_another_machine_is_refused_before_any_request(cx: &mut TestAppContext) {
    let t = Test::new();
    let (window, view) = t.open(cx, t.saved());
    fill_and_connect(cx, window, "http://bunsin.example.com", &t.server.token);
    // No wait: the address check runs in the click, before any request starts.
    let (connecting, error) = view.read_with(cx, |view, _| {
        (view.connecting(), view.error().map(str::to_owned))
    });
    assert!(!connecting);
    let error = error.unwrap();
    assert!(error.contains("use https://"), "{error}");
    assert!(t.nothing_saved());
}

#[gpui_kit::test]
fn a_saved_connection_goes_straight_to_the_shell(cx: &mut TestAppContext) {
    let t = Test::new();
    t.save_good_connection();
    let (window, view) = t.open(cx, t.saved());
    wait_until(cx, &view, "the shell with a snapshot", shell_loaded);
    assert_eq!(
        label(cx, window, "profile-kim").as_deref(),
        Some("● Kim (kim)")
    );
    assert_not_on_screen(cx, window, &t.server.token, "Kim (kim)");
}

#[gpui_kit::test]
fn a_revoked_token_shows_the_form_with_the_address_and_the_error(cx: &mut TestAppContext) {
    let t = Test::new();
    t.save_good_connection();
    t.server.revoke();
    let (window, view) = t.open(cx, t.saved());
    wait_until(cx, &view, "the error", failed);

    let url = t.server.url();
    let error = label(cx, window, "connect-error").unwrap();
    assert!(error.contains("not authorized"), "{error}");
    assert!(view.read_with(cx, |view, _| view.shell().is_none()));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("server").value(), Some(url.as_str()));
        assert!(window.try_find("connect").is_some());
    })
    .unwrap();
    // A failed start-up keeps what was saved. The user replaces it with a new connect.
    assert!(load_connection(t.dir.path()).is_some());
    assert_eq!(t.tokens.get().unwrap(), Some(t.server.token.clone()));
    assert_not_on_screen(cx, window, &t.server.token, &url);
}

#[gpui_kit::test]
fn a_saved_address_without_a_token_shows_the_form_without_an_error(cx: &mut TestAppContext) {
    let t = Test::new();
    t.save_good_connection();
    t.tokens.delete().unwrap();
    let (window, view) = t.open(cx, t.saved());
    cx.run_until_parked();
    let (connecting, error) = view.read_with(cx, |view, _| {
        (view.connecting(), view.error().map(str::to_owned))
    });
    assert!(!connecting);
    assert_eq!(error, None);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("server").value(), Some(t.server.url().as_str()));
    })
    .unwrap();
}

#[gpui_kit::test]
fn disconnect_deletes_the_token_and_shows_the_form(cx: &mut TestAppContext) {
    let t = Test::new();
    t.save_good_connection();
    let (window, view) = t.open(cx, t.saved());
    wait_until(cx, &view, "the shell with a snapshot", shell_loaded);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("disconnect", cx);
    })
    .unwrap();
    assert!(view.read_with(cx, |view, _| view.shell().is_none()));
    assert_eq!(t.tokens.get().unwrap(), None);
    // The address stays saved, so the form shows it.
    assert!(load_connection(t.dir.path()).is_some());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("profile-kim").is_none());
        assert!(window.try_find("connect").is_some());
        assert_eq!(window.find("server").value(), Some(t.server.url().as_str()));
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_connect_clears_the_token_field(cx: &mut TestAppContext) {
    let t = Test::new();
    let (window, view) = t.open(cx, t.saved());
    fill_and_connect(cx, window, &t.server.url(), &t.server.token);
    wait_until(cx, &view, "the shell with a snapshot", shell_loaded);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("disconnect", cx);
        window.render_frame(cx);
        // Connect again without typing. The field must be empty now.
        window.click("connect", cx);
    })
    .unwrap();
    let error = view.read_with(cx, |view, _| view.error().map(str::to_owned));
    let error = error.unwrap();
    assert!(error.contains("no access token is set"), "{error}");
    assert!(view.read_with(cx, |view, _| view.shell().is_none()));
}

#[gpui_kit::test]
fn the_ca_file_field_is_used_and_shown_again(cx: &mut TestAppContext) {
    let t = Test::new();
    let (window, view) = t.open(cx, t.saved());
    let missing = t.dir.path().join("missing-ca.pem").display().to_string();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("ca-cert", cx);
        window.input(&missing, cx);
    })
    .unwrap();
    fill_and_connect(cx, window, &t.server.url(), &t.server.token);
    // No wait: the file is read in the click, before any request starts.
    let error = view.read_with(cx, |view, _| view.error().map(str::to_owned));
    let error = error.unwrap();
    assert!(error.contains("CA certificate"), "{error}");
    assert!(error.contains(&missing), "{error}");
    assert!(t.nothing_saved());

    // A saved CA path is filled in at start-up.
    let saved = Connection {
        server: t.server.url(),
        ca_cert: Some(missing.clone().into()),
    };
    save_connection(t.dir.path(), &saved).unwrap();
    let (window, _) = t.open(cx, t.saved());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("ca-cert").value(), Some(missing.as_str()));
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_fixed_connection_goes_straight_to_the_shell(cx: &mut TestAppContext) {
    // This is the start-up with the BUNSIN_* environment variables.
    let t = Test::new();
    let source = Source::Fixed {
        connection: Connection {
            server: t.server.url(),
            ca_cert: None,
        },
        token: t.server.token.clone(),
    };
    let (window, view) = t.open(cx, source);
    wait_until(cx, &view, "the shell with a snapshot", shell_loaded);
    assert_eq!(
        label(cx, window, "profile-kim").as_deref(),
        Some("● Kim (kim)")
    );
}

#[gpui_kit::test]
fn an_empty_token_field_uses_the_stored_token(cx: &mut TestAppContext) {
    let t = Test::new();
    // The saved address does not answer, as if the server was down at start-up.
    let dead = Connection {
        server: "http://127.0.0.1:1".into(),
        ca_cert: None,
    };
    save_connection(t.dir.path(), &dead).unwrap();
    t.tokens.set(&t.server.token).unwrap();
    let (window, view) = t.open(cx, t.saved());
    wait_until(cx, &view, "the error", failed);
    assert!(
        label(cx, window, "connect-error")
            .unwrap()
            .contains("cannot be reached")
    );

    // The user corrects the address and connects without typing the token again.
    let url = t.server.url();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("server", cx);
        window.press("cmd-a", cx);
        window.input(&url, cx);
        window.click("connect", cx);
    })
    .unwrap();
    wait_until(cx, &view, "the shell", shell_loaded);
    assert_eq!(load_connection(t.dir.path()).unwrap().server, url);
    assert_eq!(t.tokens.get().unwrap(), Some(t.server.token.clone()));
    assert_not_on_screen(cx, window, &t.server.token, "Kim (kim)");
}
