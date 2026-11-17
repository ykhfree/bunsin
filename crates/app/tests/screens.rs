//! Draws the main window offscreen and writes one PNG per view. No window is shown.
//! The data comes from a test engine, so no real name or token is in a picture.
//!
//! Run: `cargo test -p bunsin-app --test screens -- <output folder>`

mod common;

use bunsin_api::{
    CommandConnectorUpdate, ConnectorUpdate, EngineApi, Expected, LlmUpdate, PollsUpdate, PrinciplesUpdate,
    TokenChange, WikiPageUpdate,
};
use bunsin_app::bridge::{NoteResult, SaveError, UiCommand, UiState, apply};
use bunsin_app::connect::{AppView, Source};
use bunsin_app::connection::MemoryTokenStore;
use bunsin_app::shell::Shell;
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
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
        channel: Default::default(),
    };
    rig.engine.ingest("kim", event).await.unwrap();
}

/// A profile with items in every state the inbox shows, three pages and a connector.
/// The second state adds decisions for the permissions view: counts, a suggestion and a hold.
/// The third adds what the learning view shows: corrections, candidates and an evaluation run.
async fn fixture() -> (common::Rig, UiState, UiState, UiState) {
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
    // An earlier text of the page, for the history.
    let first = page(
        "Team",
        "Who does what",
        "Kim leads the platform team.\nLee owns the billing service.",
    );
    rig.engine.set_wiki_page("kim", "team", first).await.unwrap();
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
    // Tools with typed arguments, and one rule of each class, for the permissions view.
    let text = json!({"type": "string"});
    let spec = |name: &str, description: &str, properties: serde_json::Value| bunsin_core::tool::ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type": "object", "properties": properties}),
    };
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    let slack = vec![
        spec(
            "post",
            "Post a message to a channel or a thread.",
            json!({"channel": text, "text": text, "thread_ts": text}),
        ),
        spec(
            "search",
            "Search messages. Returns the newest matches first.",
            json!({"query": text}),
        ),
        spec(
            "add_reaction",
            "Add an emoji reaction to a message.",
            json!({"channel": text, "ts": text, "emoji": text}),
        ),
        spec(
            "delete_message",
            "Delete a message that you posted.",
            json!({"channel": text, "ts": text}),
        ),
    ];
    // The connector lists the same tools, so the check in the settings finds no difference.
    rig.slack.set_tools(slack.clone());
    config.approved_tools.insert("slack".into(), slack);
    let rule = |class, tier, text| ToolRule {
        class,
        tier,
        outgoing_text: text,
    };
    config.policy.rules.insert(
        "slack/add_reaction".into(),
        rule(ToolClass::Write, Tier::Autonomous, OutgoingText::None),
    );
    config.policy.rules.insert(
        "slack/delete_message".into(),
        rule(ToolClass::Delete, Tier::Approve, OutgoingText::None),
    );
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let principles = [
        "Answer in two sentences.",
        "Never promise a date without checking Jira.",
        "If a question is about money or people, leave it to me.",
    ];
    let principles = PrinciplesUpdate {
        principles: principles.map(str::to_owned).to_vec(),
        expected: Expected::Unchecked,
    };
    rig.engine.set_principles("kim", principles).await.unwrap();
    // A search that the server repeats, after one run.
    let poll = json!({
        "connector": "slack", "tool": "search", "args": {"query": "to:me after:yesterday"}, "every_secs": 60,
        "mapping": {"items": "messages", "id": "ts", "text": "text", "author": "user", "thread": "thread_ts"},
        "skip_authors": ["U-KIM"]
    });
    let polls = PollsUpdate {
        polls: vec![serde_json::from_value(poll).unwrap()],
        expected: Expected::Unchecked,
    };
    rig.engine.set_polls("kim", polls).await.unwrap();
    rig.slack.set_result(
        "search",
        json!({"messages": [
            {"ts": "1731750000.000100", "user": "U-LEE", "text": "Can you review the release notes?"},
            {"ts": "1731750060.000200", "user": "U-PARK", "thread_ts": "1731749000.000100",
             "text": "The build is green again."},
            {"ts": "1731750120.000300", "user": "U-KIM", "text": "Thanks, both."}
        ]}),
    );
    rig.engine.poll_profile("kim").await.unwrap();
    notes_checks(&rig).await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    // Two caller tokens, one of them revoked, for the callers card. They are added after the refresh,
    // so the audit log of the inbox pictures stays the same.
    rig.engine.add_caller_token("kim", "Lee's agent").await.unwrap();
    let old = rig.engine.add_caller_token("kim", "Park's old agent").await.unwrap();
    rig.engine.revoke_caller_token("kim", old.id).await.unwrap();
    state.callers = Some(rig.engine.caller_tokens("kim").await.unwrap());

    // The inbox pictures use the state from before these decisions.
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.autonomy = Some(bunsin_core::autonomy::AutonomyThresholds {
        min_reviewed: 5,
        min_unchanged_percent: 80,
        window_days: 30,
    });
    rig.engine.set_profile_config("kim", config).await.unwrap();
    // Three more posts approved unchanged: 4 of 5 reviewed posts.
    for (event_id, text) in [("s6", "You are welcome."), ("s7", "Thanks!"), ("s8", "Noted.")] {
        let call = json!({"kind": "tool_call", "rationale": "Lee said thanks", "summary": null,
            "tool": "slack/post", "args_json": json!({"channel": "C1", "text": text}).to_string()});
        handle(&rig, event_id, "thanks", call).await;
    }
    for item in rig.engine.snapshot("kim").await.unwrap().inbox {
        if item.state == bunsin_core::approval::ApprovalState::Pending
            && matches!(&item.body, bunsin_core::records::InboxBody::ToolCall { args, .. } if args["channel"] == "C1")
        {
            rig.engine.approve("kim", item.id, None).await.unwrap();
        }
    }
    // A reaction ran without asking, and the owner reported it.
    let react = json!({"kind": "tool_call", "rationale": "Lee shipped the release", "summary": null,
        "tool": "slack/add_reaction",
        "args_json": json!({"channel": "C1", "ts": "1731750060.000200", "emoji": "tada"}).to_string()});
    handle(&rig, "s9", "shipped!", react).await;
    let ran = rig.engine.snapshot("kim").await.unwrap().inbox[0].id;
    rig.engine.report_autonomous("kim", ran).await.unwrap();
    let mut decided = UiState::default();
    apply(&*rig.engine, &mut decided, UiCommand::Refresh).await;
    let learning = learning_fixture(&rig).await;
    (rig, state, decided, learning)
}

/// An old page and the findings of a check of the notes, for the notes view. The rows are written beside
/// the engine, so the audit log of the inbox pictures stays the same.
async fn notes_checks(rig: &common::Rig) {
    use bunsin_core::lint::{FindingKind, Found};
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.notes.stale_days = Some(30);
    config.notes.lint_conflicts = true;
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let store = rig.engine.app_store().open_profile("kim").unwrap();
    let text = WikiText {
        title: "Release checklist".into(),
        summary: "What a release needs".into(),
        body: "A release needs one approval. Releases go out on Thursdays.".into(),
    };
    let long_ago = 1_794_821_400 - 45 * 86_400;
    store
        .write_wiki_page(
            "release-checklist",
            &text,
            &[],
            bunsin_core::wiki::WikiAuthor::Owner,
            0,
            None,
            long_ago,
        )
        .unwrap();
    let found = |kind, slugs: &[&str], detail: &str| Found {
        kind,
        slugs: slugs.iter().map(|slug| (*slug).to_owned()).collect(),
        detail: detail.into(),
    };
    store
        .replace_lint_findings(
            1_794_821_400,
            &[
                found(
                    FindingKind::Stale,
                    &["release-checklist"],
                    "nobody changed it for 45 day(s)",
                ),
                found(
                    FindingKind::Orphan,
                    &["release-checklist"],
                    "no other page names it, and the agent never read it",
                ),
                found(
                    FindingKind::Conflict,
                    &["deploy-schedule", "release-checklist"],
                    "One page says a release needs two approvals, the other says one.",
                ),
            ],
        )
        .unwrap();
}

/// Corrections, principle candidates and a finished evaluation run with a pass mark and a judge.
/// The rows are written beside the engine where no command makes them, so the other views stay the same.
async fn learning_fixture(rig: &common::Rig) -> UiState {
    use bunsin_core::eval::{EvalPass, Expect};
    let store = rig.engine.app_store().open_profile("kim").unwrap();
    let correction = |edited: &str, reason: &str| bunsin_core::context::Correction {
        tool: "slack/post".into(),
        source: "slack".into(),
        task: "when do we deploy?".into(),
        original: "We deploy on Friday at 3 pm, I think.".into(),
        edited: edited.into(),
        reason: reason.into(),
    };
    let old = store
        .add_correction(1, &correction("Friday, 3 pm.", "shorter"), 1_794_700_000)
        .unwrap();
    store.retire_correction(old, 1_794_800_000).unwrap();
    store
        .add_correction(
            3,
            &correction(
                "Thursday at 3 pm. I checked PROJ-12 in Jira.",
                "the date moved; name the ticket",
            ),
            1_794_821_000,
        )
        .unwrap();
    for (text, evidence) in [
        ("Answer in one or two short sentences.", "Friday, 3 pm."),
        (
            "Name the Jira ticket when you give a date.",
            "I checked PROJ-12 in Jira.",
        ),
        ("Use emoji in replies.", "shorter"),
    ] {
        store
            .add_principle_candidate(text, evidence, "corrections", 1_794_821_100)
            .unwrap();
    }
    store
        .move_candidate(
            3,
            bunsin_core::candidates::CandidateState::New,
            bunsin_core::candidates::CandidateState::Dismissed,
            1_794_821_200,
        )
        .unwrap();
    let case = |id: &str, text: &str, reply: &str, expect| bunsin_api::ReplayCase {
        event: InboundEvent {
            source: "slack".into(),
            external_id: id.into(),
            thread: None,
            author: "lee".into(),
            text: text.into(),
            channel: Default::default(),
        },
        actual_reply: reply.into(),
        expect,
    };
    let cases = vec![
        case(
            "r1",
            "When do we deploy?",
            "Thursday at 3 pm, see PROJ-12.",
            Expect::Reply,
        ),
        case("r2", "Is the build green?", "Yes, since this morning.", Expect::Reply),
        case("r3", "Can the team get a raise?", "", Expect::Escalate),
    ];
    rig.engine.add_replay_cases("kim", cases).await.unwrap();
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.eval.pass = Some(EvalPass {
        kind_percent: 80,
        overlap_percent: 40,
        boundary_percent: 100,
    });
    config.eval.judge = true;
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let post = |text: &str| {
        json!({"kind": "tool_call", "rationale": "asked", "summary": null, "tool": "slack/post",
            "args_json": json!({"channel": "C1", "text": text}).to_string()})
        .to_string()
    };
    let escalate = json!({"kind": "escalate", "rationale": "money", "tool": null, "args_json": null,
        "summary": "Lee asks about a raise. This needs your own answer."});
    rig.llm
        .reply(&post("We deploy on Thursday at 3 pm."), 10)
        .reply(
            &json!({"verdict": "partial", "reason": "the ticket is missing"}).to_string(),
            10,
        )
        .reply(&escalate.to_string(), 10)
        .reply(&escalate.to_string(), 10);
    rig.engine.start_eval("kim").await.unwrap();
    for _ in 0..2 {
        assert!(rig.engine.background_tick().await.is_empty());
    }
    let mut learning = UiState::default();
    apply(&*rig.engine, &mut learning, UiCommand::Refresh).await;
    learning
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

/// Scrolls the page so that the top of element `id` is `y` pixels below the top of the window.
/// The scroll goes to the page through the first element of `on_page` that is on screen.
fn bring(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App, on_page: &[&'static str], id: &'static str, y: f32) {
    let top = window.find(id).bounds().origin.y;
    let anchor = *on_page
        .iter()
        .find(|anchor| window.try_find(**anchor).is_some_and(|element| element.visible()))
        .expect("an element of the page is on screen");
    window.scroll(anchor, ScrollDelta::Pixels(point(px(0.), px(y) - top)), cx);
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
fn draw(
    mode: ThemeMode,
    dir: &Path,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
    state: &UiState,
    decided: &UiState,
    learning: &UiState,
) {
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

    // The owner edits the text of the long waiting call before approval. Its card is on screen now.
    let edited = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let id = (1..=20)
                .find(|id| {
                    window
                        .try_find(format!("edit-{id}"))
                        .is_some_and(|button| button.visible())
                })
                .expect("an Edit button is on screen");
            window.click(format!("edit-{id}"), cx);
            id
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // The scroll goes to the list through a part of the card that is on screen.
        window.scroll(
            format!("edit-place-{edited}-0"),
            ScrollDelta::Pixels(point(px(0.), px(-430.))),
            cx,
        );
        window.render_frame(cx);
        window.click(format!("edit-reason-{edited}"), cx);
        window.input("shorter, with the link", cx);
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("inbox-edit"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(format!("cancel-edit-{edited}"), cx);
    })
    .unwrap();

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
        ..with_page.clone()
    };
    states.send(refused).unwrap();
    shot(&mut cx, window, dir, &name("notes-warnings"));

    // The history of a page that the owner wrote twice, with the first revision chosen.
    click(&mut cx, "note-load");
    let open = UiCommand::OpenNote {
        profile: "kim".into(),
        slug: "team".into(),
    };
    let history = UiCommand::NoteHistory {
        profile: "kim".into(),
        show: true,
    };
    let mut with_history = state.clone();
    rt.block_on(apply(&*rig.engine, &mut with_history, open));
    rt.block_on(apply(&*rig.engine, &mut with_history, history));
    states.send(with_history).unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.scroll("note-heading", ScrollDelta::Pixels(point(px(0.), px(-300.))), cx);
    })
    .unwrap();
    click(&mut cx, "note-rev-0");
    shot(&mut cx, window, dir, &name("notes-history"));

    // The permissions, with the counts, a suggestion and a hold, and one rule changed.
    states.send(decided.clone()).unwrap();
    click(&mut cx, "tab-permissions");
    click(&mut cx, "perm-slack/post-tier-draft");
    // The list scrolls so that the held tool is on screen too.
    let scroll = |cx: &mut HeadlessAppContext, by: f32| {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.scroll("perm-slack/post-counts", ScrollDelta::Pixels(point(px(0.), px(by))), cx);
        })
        .unwrap();
    };
    scroll(&mut cx, -330.);
    shot(&mut cx, window, dir, &name("permissions"));
    scroll(&mut cx, 330.);
    // More changes than the box beside Save shows at once.
    for id in [
        "perm-slack/post-text-channel",
        "perm-slack/search-off",
        "perm-slack/add_reaction-tier-approve",
        "perm-slack/add_reaction-text-emoji",
        "perm-slack/add_reaction-delete",
    ] {
        click(&mut cx, id);
    }
    shot(&mut cx, window, dir, &name("permissions-changes"));

    // The principles, with one being changed, and a poll with the result of its test.
    let test = UiCommand::TestPoll {
        profile: "kim".into(),
        index: 0,
    };
    states.send(on(test)).unwrap();
    click(&mut cx, "tab-agent");
    click(&mut cx, "principle-edit-1");
    shot(&mut cx, window, dir, &name("agent"));
    click(&mut cx, "cancel-principle-edit");
    // The callers card is below the polls.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        bring(
            window,
            cx,
            &["principle-edit-1", "principles-count"],
            "callers-help",
            100.,
        );
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("agent-callers"));

    // What the agent learns: corrections and candidates, then the evaluation and the thresholds.
    states.send(learning.clone()).unwrap();
    click(&mut cx, "tab-learning");
    shot(&mut cx, window, dir, &name("learning"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.scroll("corrections-count", ScrollDelta::Pixels(point(px(0.), px(-560.))), cx);
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("learning-lower"));
    states.send(state.clone()).unwrap();

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

    // The Keychain fields, opened. This window was given no Keychain, so a read says so.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.scroll("llm-token-state", ScrollDelta::Pixels(point(px(0.), px(330.))), cx);
        window.render_frame(cx);
        window.click("open-keychain", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("keychain-service", cx);
        window.input("api.anthropic.com", cx);
        window.click("keychain-account", cx);
        window.input("kim", cx);
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("settings-keychain"));

    // The connector form for a command that the server starts, under a command in the list.
    let command = CommandConnectorUpdate {
        command: "/usr/local/bin/npx".into(),
        args: vec!["-y".into(), "example-mcp-server".into()],
        env: [("EXAMPLE_API_TOKEN".to_owned(), "fixture-value".to_owned())].into(),
        secret_env: Default::default(),
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_command_connector("kim", "files", command))
        .unwrap();
    states.send(on(UiCommand::Refresh)).unwrap();
    cx.run_until_parked();
    // The list with a command and an address.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.scroll("llm-token-state", ScrollDelta::Pixels(point(px(0.), px(-520.))), cx);
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("settings-connectors"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.scroll("connector-count", ScrollDelta::Pixels(point(px(0.), px(-240.))), cx);
        window.render_frame(cx);
        window.within("connector-kind").click(1usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    // The command form is taller than the address form, so the page scrolls again.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        bring(
            window,
            cx,
            &["connector-name", "connector-count"],
            "connector-name",
            140.,
        );
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("connector-name", cx);
        window.input("search", cx);
        window.click("connector-command", cx);
        window.input("/usr/local/bin/uvx", cx);
        window.within("connector-args").click("input-state", cx);
        window.input("example-search-mcp\n--stdio", cx);
        window.within("connector-env").click("input-state", cx);
        window.input("EXAMPLE_REGION=eu", cx);
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("settings-command"));

    // The secret variables of the command: one that was added, and one that is being typed.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        bring(
            window,
            cx,
            &["connector-command", "connector-name"],
            "secret-name",
            420.,
        );
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("secret-name", cx);
        window.input("EXAMPLE_API_TOKEN", cx);
        window.click("secret-value", cx);
        window.input("fixture-value", cx);
        window.click("add-secret", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("secret-name", cx);
        window.input("EXAMPLE_SECOND_KEY", cx);
        window.click("secret-value", cx);
        window.input("fixture-value", cx);
    })
    .unwrap();
    shot(&mut cx, window, dir, &name("settings-secrets"));

    // The setup, in a new window whose forms hold no draft of the pictures above. It opens from the
    // Agent view, moves back to its first step, and shows a test that works.
    let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
    let (states, receiver) = tokio::sync::mpsc::unbounded_channel();
    let (setup, _shell) = cx
        .update(|cx| {
            gpui_kit::open_window(options(), cx, |window, cx| {
                cx.new(|cx| Shell::new(receiver, commands, window, cx))
            })
        })
        .unwrap();
    let window: AnyWindowHandle = setup.into();
    let click = |cx: &mut HeadlessAppContext, id: &'static str| {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
    };
    states.send(on(UiCommand::Refresh)).unwrap();
    click(&mut cx, "tab-agent");
    click(&mut cx, "open-setup");
    for _ in 0..6 {
        cx.run_until_parked();
        let first = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                let back = window.try_find("setup-back").is_some();
                if back {
                    window.click("setup-back", cx);
                }
                !back
            })
            .unwrap();
        if first {
            break;
        }
    }
    rig.llm.reply(
        r#"{"kind":"ignore","rationale":"connection test","summary":null,"tool":null,"args_json":null}"#,
        41,
    );
    states.send(on(UiCommand::TestLlm { profile: "kim".into() })).unwrap();
    shot(&mut cx, window, dir, &name("setup-llm"));
    // The tools step, with the list of a connector beside the rules.
    click(&mut cx, "setup-next");
    click(&mut cx, "setup-next");
    let check = UiCommand::CheckConnector {
        profile: "kim".into(),
        name: "slack".into(),
    };
    states.send(on(check)).unwrap();
    shot(&mut cx, window, dir, &name("setup-tools"));
    for _ in 0..4 {
        click(&mut cx, "setup-skip");
    }
    shot(&mut cx, window, dir, &name("setup-finish"));

    // A coding call that waits for the owner, and the result of a job that ended. It comes last,
    // because its rows would change the inbox of the other pictures.
    let coding = rt.block_on(coding_state(rig));
    states.send(coding).unwrap();
    click(&mut cx, "tab-inbox");
    shot(&mut cx, window, dir, &name("inbox-coding"));

    // The publishing cards and the checks cards of profile lee, in a new window whose forms hold no
    // draft. Lee's settings are not in any picture of kim.
    let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
    let (states, receiver) = tokio::sync::mpsc::unbounded_channel();
    let (publishing, _shell) = cx
        .update(|cx| {
            gpui_kit::open_window(options(), cx, |window, cx| {
                cx.new(|cx| Shell::new(receiver, commands, window, cx))
            })
        })
        .unwrap();
    let window: AnyWindowHandle = publishing.into();
    states.send(rt.block_on(publishing_state(rig))).unwrap();
    // Opens view `tab` and scrolls its page so that element `id` is near the top.
    let to_top = |cx: &mut HeadlessAppContext, tab: &'static str, id: &'static str| {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(tab, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let on_page = [
                "principles-count",
                "callers-help",
                "inbound-help",
                "connections-help",
                "llm-token-state",
                "privacy-help",
            ];
            bring(window, cx, &on_page, id, 100.);
        })
        .unwrap();
    };
    to_top(&mut cx, "tab-agent", "inbound-help");
    shot(&mut cx, window, dir, &name("agent-publishing"));
    to_top(&mut cx, "tab-agent", "webhooks-help");
    shot(&mut cx, window, dir, &name("agent-webhooks"));
    to_top(&mut cx, "tab-settings", "auto-apply-help");
    shot(&mut cx, window, dir, &name("settings-checks"));
}

/// Gives lee a listed inbound endpoint on an internal listener, a Slack webhook with its secret,
/// an ARD registry, and settings for the notes and the evaluation. Returns the state of lee.
async fn publishing_state(rig: &common::Rig) -> UiState {
    use bunsin_api::{
        ApiConnectorUpdate, ArdRegistriesUpdate, EvalSettingsUpdate, InboundUpdate, NotesSettingsUpdate, WebhooksUpdate,
    };
    use bunsin_core::ard::ArdRegistry;
    use bunsin_core::eval::{EvalConfig, EvalPass};
    use bunsin_core::inbound::{ArdListing, InboundConfig, InboundScope};
    use bunsin_core::settings::{ApiAuthMethod, ApiConfig, ApiService};
    use bunsin_core::webhook::WebhookConfig;
    use bunsin_core::wiki::NotesConfig;
    let engine = &rig.engine;
    engine.add_outer_listener(bunsin_engine::OuterListener {
        scope: InboundScope::Internal,
        url: "https://agents.acme.com".into(),
    });
    let inbound = InboundConfig {
        enabled: true,
        scope: InboundScope::Internal,
        title: Some("Lee's agent".into()),
        description: "Questions about the release calendar.".into(),
        ard: Some(ArdListing {
            publisher: common::PUBLISHER.into(),
            namespace: Some("team".into()),
            agent_name: "lee".into(),
            representative_queries: vec!["When is the next release?".into(), "Who approves a release?".into()],
        }),
    };
    let update = InboundUpdate {
        inbound,
        expected: Expected::Unchecked,
    };
    engine.set_inbound_settings("lee", update).await.unwrap();
    let slack = ApiConnectorUpdate {
        api: ApiConfig {
            service: ApiService::Slack,
            base_url: "https://slack.com/api".into(),
            auth: ApiAuthMethod::Bearer,
            email: None,
        },
        token: TokenChange::Set("fixture-token".into()),
        expected: Expected::Unchecked,
    };
    engine.set_api_connector("lee", "slackapi", slack).await.unwrap();
    let hook = WebhookConfig {
        connector: "slackapi".into(),
        scope: InboundScope::Internal,
        skip_authors: vec!["U-LEE".into()],
    };
    let update = WebhooksUpdate {
        webhooks: [("slack-events".to_owned(), hook)].into(),
        expected: Expected::Unchecked,
    };
    engine.set_webhooks("lee", update).await.unwrap();
    engine
        .set_webhook_secret("lee", "slack-events", "fixture-secret".into())
        .await
        .unwrap();
    let registries = ArdRegistriesUpdate {
        registries: vec![ArdRegistry {
            name: "corp".into(),
            url: "https://registry.acme.com/v1".into(),
        }],
        expected: Expected::Unchecked,
    };
    engine.set_ard_registries("lee", registries).await.unwrap();
    let notes = NotesSettingsUpdate {
        notes: NotesConfig {
            auto_apply: true,
            stale_days: Some(30),
            lint_every_hours: Some(24),
            lint_conflicts: false,
        },
        expected: Expected::Unchecked,
    };
    engine.set_notes_settings("lee", notes).await.unwrap();
    let eval = EvalSettingsUpdate {
        eval: EvalConfig {
            pass: Some(EvalPass {
                kind_percent: 80,
                overlap_percent: 40,
                boundary_percent: 100,
            }),
            judge: false,
        },
        expected: Expected::Unchecked,
    };
    engine.set_eval_settings("lee", eval).await.unwrap();
    let mut state = UiState::default();
    apply(&**engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    state
}

/// Writes a waiting coding call and the draft of an ended job beside the engine, once, and loads the state.
async fn coding_state(rig: &common::Rig) -> UiState {
    use bunsin_core::approval::ApprovalState;
    use bunsin_core::coding::{CodingOutcome, TestOutcome, report_text};
    use bunsin_core::records::InboxBody;
    let store = rig.engine.app_store().open_profile("kim").unwrap();
    let written = store
        .list_inbox(200)
        .unwrap()
        .iter()
        .any(|item| matches!(&item.body, InboxBody::ToolCall { tool, .. } if tool == "bunsin.coding/start"));
    if !written {
        let outcome = CodingOutcome {
            branch: "agent/b31-kim".into(),
            base_commit: "4f2a9c81d7e3b6a0c5d9e8f7a6b5c4d3e2f1a0b9".into(),
            head_commit: "9b8a7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0c".into(),
            diff_stat: " src/login.rs       | 12 ++++++++----\n tests/login.rs     | 18 ++++++++++++++++++\n 2 files changed, 26 insertions(+), 4 deletions(-)".into(),
            test: Some(TestOutcome {
                command: vec!["/usr/bin/make".into(), "test".into()],
                exit_code: Some(0),
                timed_out: false,
                output: "ok".into(),
            }),
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            summary: None,
            transcript: String::new(),
        };
        let done = InboxBody::Escalation {
            summary: report_text("b31-kim", "web-app", &Ok(outcome)),
        };
        let rationale = "a coding job ended; review its branch and merge it yourself";
        store
            .add_inbox_item("code-1", &done, rationale, &[], ApprovalState::Draft, 1_794_821_000)
            .unwrap();
        let args = json!({"repo": "web-app", "base_ref": "main",
            "instructions": "The login page shows no error when the password is wrong. Show the server's message under the field and add a test."});
        let call = InboxBody::ToolCall {
            tool: "bunsin.coding/start".into(),
            args: args.as_object().unwrap().clone(),
        };
        let why = "Lee reported that a wrong password gives no message";
        store
            .add_inbox_item("code-2", &call, why, &[], ApprovalState::Pending, 1_794_821_300)
            .unwrap();
    }
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    state
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
    let (rig, state, decided, learning) = rt.block_on(fixture());
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        draw(mode, dir, &rt, &rig, &state, &decided, &learning);
    }
}
