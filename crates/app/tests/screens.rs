//! Draws the main window offscreen and writes one PNG per view. No window is shown.
//! The data comes from a test engine, so no real name or token is in a picture.
//!
//! Run: `cargo test -p bunsin-app --test screens -- <output folder>`

mod common;

use bunsin_api::{ConnectorUpdate, EngineApi, Expected, LlmUpdate, TokenChange, WikiPageUpdate};
use bunsin_app::bridge::{NoteResult, SaveError, UiCommand, UiState, apply};
use bunsin_app::connect::{AppView, Source};
use bunsin_app::connection::MemoryTokenStore;
use bunsin_app::shell::Shell;
use bunsin_core::event::InboundEvent;
use bunsin_core::settings::Wire;
use bunsin_core::wiki::WikiText;
use gpui_kit::assets::Assets;
use gpui_kit::component::ThemeMode;
use gpui_kit::test::{TestQueryExt, TestWindowExt};
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, HeadlessAppContext, ScrollDelta, WindowBounds, WindowOptions, point, px, size,
};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

fn page(title: &str, summary: &str, body: &str) -> WikiPageUpdate {
    WikiPageUpdate {
        text: WikiText {
            title: title.into(),
            summary: summary.into(),
            body: body.into(),
        },
        expected_revision: None,
    }
}

/// Handles one event with a scripted reply of the agent.
async fn handle(rig: &common::Rig, event_id: &str, text: &str, reply: serde_json::Value) {
    rig.llm.reply(&reply.to_string(), 10);
    let event = InboundEvent {
        source: "slack".into(),
        external_id: event_id.into(),
        thread: None,
        author: "lee".into(),
        text: text.into(),
    };
    rig.engine.ingest("kim", event).await.unwrap();
}

/// A profile with items in every state the inbox shows, three pages and a connector.
async fn fixture() -> (common::Rig, UiState) {
    // 2026-11-16 09:30 UTC.
    let rig = common::rig_at(1_794_821_400).await;
    let body = "The team deploys on Fridays at 3 pm.\nA release needs two approvals.";
    common::ingest_note(&rig, "n1", false, body).await;
    // Item 1 was run, item 2 (the note) is kept, item 3 waits.
    rig.engine.approve("kim", 1, None).await.unwrap();
    rig.engine.approve("kim", 2, None).await.unwrap();
    // A call with a long value without spaces and with Korean text: nothing of it may be cut.
    let long = format!("https://wiki.example.com/pages/{}", "release-notes-2026-".repeat(20));
    // A value without any place to break a line, a long argument name and a nested value.
    let digest = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".repeat(5);
    let args = json!({"channel": "C-ALL-COMPANY", "text": format!("배포는 금요일 오후 3시입니다. 자세한 내용: {long}"),
        "broadcast": true, "attachment_sha256_digest_of_the_file": digest,
        "options": {"unfurl_links": false, "mentions": ["@here", "@lee"]}});
    let call = json!({"kind": "tool_call", "rationale": "Lee asked where the release notes are", "summary": null,
        "tool": "slack/post", "args_json": args.to_string()});
    handle(&rig, "s2", "where are the release notes?", call).await;
    let ask = json!({"kind": "escalate", "rationale": "the question is about a salary", "tool": null, "args_json": null,
        "summary": "Lee asks if the budget covers a raise for the team. This needs your own answer."});
    handle(&rig, "s3", "does the budget cover a raise?", ask).await;
    let call = json!({"kind": "tool_call", "rationale": "Lee said thanks", "summary": null,
        "tool": "slack/post", "args_json": json!({"channel": "C1", "text": "You are welcome."}).to_string()});
    handle(&rig, "s4", "thanks", call).await;
    let last = rig
        .engine
        .snapshot("kim")
        .await
        .unwrap()
        .inbox
        .iter()
        .map(|item| item.id)
        .max()
        .unwrap();
    rig.engine.reject("kim", last, "not needed").await.unwrap();
    // The agent proposes a change of the page it wrote.
    common::ingest_note(&rig, "n2", true, "The team deploys on Thursdays at 3 pm.").await;

    let team = "Kim leads the platform team.\nLee reviews releases.\nPark owns the billing service.";
    rig.engine
        .set_wiki_page("kim", "team", page("Team", "Who does what", team))
        .await
        .unwrap();
    let oncall = page(
        "On-call",
        "How the on-call rotation works",
        "The rotation changes on Mondays at 10 am.",
    );
    rig.engine.set_wiki_page("kim", "on-call", oncall).await.unwrap();
    let connector = ConnectorUpdate {
        url: "https://mcp.example.com/slack".into(),
        token: TokenChange::Set("fixture-token".into()),
        token_header: None,
        expected: Expected::Unchecked,
    };
    rig.engine.set_connector("kim", "slack", connector).await.unwrap();
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    (rig, state)
}

/// An offscreen window of laptop size.
fn options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Default::default(),
            size: size(px(1280.), px(800.)),
        })),
        focus: false,
        show: false,
        ..Default::default()
    }
}

fn shot(cx: &mut HeadlessAppContext, window: AnyWindowHandle, dir: &Path, name: &str) {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    let image = cx
        .capture_screenshot(window)
        .expect("offscreen rendering must be available");
    let path = dir.join(format!("{name}.png"));
    image.save(&path).unwrap();
    println!("wrote {}", path.display());
}

/// Draws every view in one mode into `dir`.
fn draw(mode: ThemeMode, dir: &Path, rt: &tokio::runtime::Runtime, rig: &common::Rig, state: &UiState) {
    let mut cx = HeadlessAppContext::with_platform(
        gpui_kit::platform::current_platform(true).text_system(),
        Arc::new(Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(gpui_kit::init);
    cx.update(|cx| bunsin_app::theme::apply(mode, None, cx));
    let name = |view: &str| format!("{view}-{}", mode.name());

    // The connect screen, with a failed attempt.
    let store = tempfile::tempdir().unwrap();
    let source = Source::Saved {
        dir: store.path().to_owned(),
        tokens: Arc::new(MemoryTokenStore::default()),
    };
    let handle = rt.handle().clone();
    let (connect, _view) = cx
        .update(|cx| {
            gpui_kit::open_window(options(), cx, |window, cx| {
                cx.new(|cx| AppView::new(handle, source, window, cx))
            })
        })
        .unwrap();
    let connect: AnyWindowHandle = connect.into();
    cx.update_window(connect, |_, window, cx| {
        window.render_frame(cx);
        window.click("server", cx);
        window.input("https://bunsin.example.com:8787", cx);
        window.click("connect", cx);
    })
    .unwrap();
    shot(&mut cx, connect, dir, &name("connect"));

    // The program is the bridge: it sends the states, and it answers no command.
    let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
    let (states, receiver) = tokio::sync::mpsc::unbounded_channel();
    let (window, _shell) = cx
        .update(|cx| {
            gpui_kit::open_window(options(), cx, |window, cx| {
                cx.new(|cx| Shell::new(receiver, commands, window, cx))
            })
        })
        .unwrap();
    let window: AnyWindowHandle = window.into();
    let click = |cx: &mut HeadlessAppContext, id: &'static str| {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
    };
    let on = |command| {
        let mut next = state.clone();
        rt.block_on(apply(&*rig.engine, &mut next, command));
        next
    };

    states.send(state.clone()).unwrap();
    shot(&mut cx, window, dir, &name("inbox"));
    cx.update_window(window, |_, window, cx| {
        // The scroll goes to the list through one of its items.
        window.render_frame(cx);
        let on_top = (1..=20)
            .map(|id| format!("inbox-{id}"))
            .find(|id| window.try_find(id.clone()).is_some_and(|item| item.visible()))
            .expect("an item is on screen");
        window.scroll(on_top, ScrollDelta::Pixels(point(px(0.), px(-560.))), cx);
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("inbox-lower"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let on_top = (1..=20)
            .map(|id| format!("inbox-{id}"))
            .find(|id| window.try_find(id.clone()).is_some_and(|item| item.visible()))
            .expect("an item is on screen");
        window.scroll(on_top, ScrollDelta::Pixels(point(px(0.), px(-690.))), cx);
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("inbox-long"));

    // The kill switch is on, and a command failed.
    let mut stopped = on(UiCommand::SetKillSwitch(true));
    stopped.error = Some("the call failed: slack answered 503 Service Unavailable".into());
    states.send(stopped).unwrap();
    shot(&mut cx, window, dir, &name("inbox-stopped"));
    rt.block_on(rig.engine.set_kill_switch("kim", false)).unwrap();

    // A profile with nothing in it, then a server that does not answer.
    states.send(on(UiCommand::SelectProfile("lee".into()))).unwrap();
    shot(&mut cx, window, dir, &name("inbox-empty"));
    let lost = UiState {
        error: Some("the server did not answer: connection refused".into()),
        ..UiState::default()
    };
    states.send(lost).unwrap();
    shot(&mut cx, window, dir, &name("not-connected"));

    // A page of the agent, with unsaved text, a refused save and a waiting change.
    states.send(state.clone()).unwrap();
    click(&mut cx, "tab-notes");
    let open = UiCommand::OpenNote {
        profile: "kim".into(),
        slug: "deploy-schedule".into(),
    };
    let with_page = on(open);
    states.send(with_page.clone()).unwrap();
    shot(&mut cx, window, dir, &name("notes"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("note-body").click("input-state", cx);
        window.input("Releases on a holiday move to the next working day.\n", cx);
        window.click("note-row-team", cx);
        window.click("note-save", cx);
    })
    .unwrap();
    let refused = UiState {
        note_result: Some(NoteResult::Saved {
            slug: "deploy-schedule".into(),
            result: Err(SaveError::Conflict),
        }),
        note_commands: 1,
        ..with_page
    };
    states.send(refused).unwrap();
    shot(&mut cx, window, dir, &name("notes-warnings"));

    // The settings, with the result of a check of the connector.
    click(&mut cx, "tab-settings");
    let check = UiCommand::CheckConnector {
        profile: "kim".into(),
        name: "slack".into(),
    };
    states.send(on(check)).unwrap();
    shot(&mut cx, window, dir, &name("settings"));

    // The result of a test of the endpoint, and the connector form with its header field.
    let llm = LlmUpdate {
        wire: Wire::OpenAiResponses,
        base_url: "https://llm.example.com/v1".into(),
        model: "example-model".into(),
        timeout_secs: 120,
        token: TokenChange::Set("fixture-token".into()),
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_llm("kim", llm)).unwrap();
    rig.llm.reply(
        r#"{"kind":"ignore","rationale":"connection test","summary":null,"tool":null,"args_json":null}"#,
        41,
    );
    let test = UiCommand::TestLlm { profile: "kim".into() };
    states.send(on(test)).unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.scroll("llm-token-state", ScrollDelta::Pixels(point(px(0.), px(-330.))), cx);
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("settings-test"));
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: screens <output folder>");
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (rig, state) = rt.block_on(fixture());
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        draw(mode, dir, &rt, &rig, &state);
    }
}
