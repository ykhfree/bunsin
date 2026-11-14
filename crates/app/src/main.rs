//! The Bunsin desktop app. It runs the engine in this process and shows its window.

use bunsin_app::bridge::spawn_bridge;
use bunsin_app::shell::Shell;
use bunsin_daemon::RealBackends;
use bunsin_engine::Engine;
use gpui_kit::{AppContext, Global, WindowOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Keeps the tokio runtime alive as long as the GPUI app.
struct Tokio(#[allow(dead_code)] tokio::runtime::Runtime);

impl Global for Tokio {}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn main() {
    let Some(data_dir) = std::env::var_os("BUNSIN_DATA_DIR").map(PathBuf::from) else {
        eprintln!("error: set BUNSIN_DATA_DIR to the folder that holds the profiles");
        std::process::exit(1);
    };
    if let Err(error) = std::fs::create_dir_all(&data_dir) {
        eprintln!("error: cannot create the data directory: {error}");
        std::process::exit(1);
    }
    // Fails if `bunsind` or another window already uses this data directory.
    let engine = match Engine::open(&data_dir, Arc::new(RealBackends::from_env()), Arc::new(now)) {
        Ok(engine) => Arc::new(engine),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
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
