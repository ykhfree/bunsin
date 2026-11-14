//! The Bunsin desktop app: a window that manages agents on a Bunsin server.

use bunsin_app::bridge::spawn_bridge;
use bunsin_app::shell::Shell;
use bunsin_client::{RemoteConfig, RemoteEngine};
use gpui_kit::{AppContext, Global, WindowOptions};
use std::sync::Arc;

/// Keeps the tokio runtime alive as long as the GPUI app.
struct Tokio(#[allow(dead_code)] tokio::runtime::Runtime);

impl Global for Tokio {}

fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    std::process::exit(1);
}

fn main() {
    let Ok(url) = std::env::var("BUNSIN_SERVER") else {
        fail("set BUNSIN_SERVER to the server address")
    };
    let Ok(token) = std::env::var("BUNSIN_TOKEN") else {
        fail("set BUNSIN_TOKEN to your access token")
    };
    let ca_cert_pem = match std::env::var_os("BUNSIN_CA_CERT") {
        Some(path) => match std::fs::read(&path) {
            Ok(pem) => Some(pem),
            Err(error) => fail(&format!("cannot read the CA certificate: {error}")),
        },
        None => None,
    };
    let engine = match RemoteEngine::new(&RemoteConfig { url, ca_cert_pem }, &token) {
        Ok(engine) => Arc::new(engine),
        Err(error) => fail(&error.to_string()),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime starts");
    let (commands, states) = spawn_bridge(runtime.handle(), engine);

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.set_global(Tokio(runtime));
        gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
            cx.new(|cx| Shell::new(states, commands, cx))
        })
        .expect("the window opens");
    });
}
