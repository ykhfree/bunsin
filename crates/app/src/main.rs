//! The Bunsin desktop app: a window that manages agents on a Bunsin server.

use bunsin_app::connect::{AppView, Source};
use bunsin_app::connection::{Connection, KeychainTokenStore, MacKeychain};
use gpui_kit::{AppContext, Global, WindowOptions};
use std::sync::Arc;

/// Keeps the tokio runtime alive as long as the GPUI app.
struct Tokio(#[allow(dead_code)] tokio::runtime::Runtime);

impl Global for Tokio {}

fn source() -> Source {
    // For development, the environment overrides the saved connection.
    if let (Ok(server), Ok(token)) = (std::env::var("BUNSIN_SERVER"), std::env::var("BUNSIN_TOKEN")) {
        let ca_cert = std::env::var_os("BUNSIN_CA_CERT").map(Into::into);
        return Source::Fixed {
            connection: Connection { server, ca_cert },
            token,
        };
    }
    let Some(config) = dirs::config_dir() else {
        eprintln!("error: the config directory of this user is unknown");
        std::process::exit(1);
    };
    Source::Saved {
        dir: config.join("Bunsin"),
        tokens: Arc::new(KeychainTokenStore),
    }
}

fn main() {
    let source = source();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime starts");

    // The icons of the components (check marks, the icons of the views) come from these assets.
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let handle = runtime.handle().clone();
            cx.set_global(Tokio(runtime));
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                // The window has the colors of the system's light or dark mode, and follows a change.
                bunsin_app::theme::apply(bunsin_app::theme::system_mode(window), Some(window), cx);
                window
                    .observe_window_appearance(|window, cx| {
                        bunsin_app::theme::apply(bunsin_app::theme::system_mode(window), Some(window), cx);
                        window.refresh();
                    })
                    .detach();
                cx.new(|cx| {
                    let mut view = AppView::new(handle, source, window, cx);
                    // The connections form can copy a token from an item of the login Keychain.
                    view.set_keychain(Arc::new(MacKeychain), cx);
                    view
                })
            })
            .expect("the window opens");
        });
}
