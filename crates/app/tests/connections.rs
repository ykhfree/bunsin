//! Headless tests of the connections section: real view, real engine with the server's address rules.

mod common;

use bunsin_api::{ConnectorUpdate, EngineApi, Expected, LlmUpdate, TokenChange, TokenSource};
use bunsin_app::bridge::ConnectionResult;
use bunsin_app::shell::Shell;
use bunsin_core::settings::{ANTHROPIC_BASE_URL, ANTHROPIC_DEFAULT_MODEL, ConnectorSection, McpConfig, Wire};
use bunsin_testkit::tool;
use common::window::{REFRESH, label, on_screen, open_settings, runtime, screen_texts, value, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{App, Entity, SharedString, TestAppContext, Window};

const TOKEN: &str = "sk-llm-token-7f3a9c";
const SLACK_TOKEN: &str = "xoxb-slack-token-51d2";

/// Replaces the text of a field with `text`.
fn fill(window: &mut Window, cx: &mut App, id: &'static str, text: &str) {
    window.click(id, cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

/// The label of element `id` in the list row of connector `name`, if the row has that element.
fn row(window: &mut Window, name: &str, id: &'static str) -> Option<String> {
    window
        .within("connector-list")
        .within(SharedString::from(name.to_owned()))
        .try_find(id)
        .map(|e| e.label().unwrap_or_default().to_owned())
}

/// True if the list shows a row for connector `name`. The list must show at least one row.
fn has_row(window: &mut Window, name: &str) -> bool {
    window
        .within("connector-list")
        .find_all("name")
        .iter()
        .any(|e| e.label() == Some(name))
}

fn click_row(window: &mut Window, cx: &mut App, name: &str, id: &'static str) {
    window
        .within("connector-list")
        .within(SharedString::from(name.to_owned()))
        .click(id, cx);
}

/// Waits until the window has received the state after the `n`th connection command.
fn wait_for_commands(cx: &mut TestAppContext, shell: &Entity<Shell>, n: u64) {
    wait_until(cx, shell, "the connection command result", |s| {
        s.state.connection_commands == n
    });
}

/// Fails if `secret` is in any accessibility text. `shown` must be found, so the scan is real.
fn assert_not_on_screen(window: &Window, secret: &str, shown: &str) {
    let texts = screen_texts(window);
    assert!(
        texts.iter().any(|t| t.contains(shown)),
        "the scan did not find {shown:?}: {texts:?}"
    );
    let leaks: Vec<_> = texts.iter().filter(|t| t.contains(secret)).collect();
    assert!(leaks.is_empty(), "the token is on screen: {leaks:?}");
}

fn llm(wire: Wire, base_url: &str, model: &str, token: TokenChange) -> LlmUpdate {
    LlmUpdate {
        wire,
        base_url: base_url.into(),
        model: model.into(),
        timeout_secs: 30,
        token,
        expected: bunsin_api::Expected::Unchecked,
    }
}

fn connector(url: &str, token: TokenChange) -> ConnectorUpdate {
    ConnectorUpdate {
        url: url.into(),
        token,
        token_header: None,
        expected: bunsin_api::Expected::Unchecked,
    }
}

const CONFLICT: &str = "The settings changed on the server since this form loaded them. Nothing was saved.";

/// Fails if form `form` shows the conflict warning or its controls.
fn assert_no_conflict(window: &Window, form: &str) {
    for part in ["conflict", "load", "save-anyway"] {
        let id = SharedString::from(format!("{form}-{part}"));
        assert!(window.try_find(id.clone()).is_none(), "{id}");
    }
}

/// Saves connector `name` for kim from outside the window and waits until a refresh shows it.
fn set_connector_from_outside(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
    shell: &Entity<Shell>,
    name: &'static str,
    url: &str,
) {
    rt.block_on(rig.engine.set_connector("kim", name, connector(url, TokenChange::Keep)))
        .unwrap();
    let shown = McpConfig::Http { url: url.into() };
    wait_until(cx, shell, "the outside change", |s| {
        s.state
            .connections
            .as_ref()
            .and_then(|c| c.connectors.get(name))
            .is_some_and(|c| c.mcp == shown)
    });
}

/// Which API format the radio group shows as chosen: 0 Anthropic, 1 OpenAI.
fn wire_checked(window: &mut Window) -> (Option<bool>, Option<bool>) {
    let group = window.within("llm-wire");
    (group.find(0usize).checked(), group.find(1usize).checked())
}

#[gpui_kit::test]
fn the_section_shows_the_server_connections_of_the_selected_profile(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        // No endpoint is saved, so the form offers the Claude API.
        assert_eq!(value(window, "llm-address"), ANTHROPIC_BASE_URL);
        assert_eq!(value(window, "llm-model"), ANTHROPIC_DEFAULT_MODEL);
        assert_eq!(label(window, "llm-token-state"), "no token");
        assert_eq!(wire_checked(window), (Some(true), Some(false)));
        assert!(window.try_find("remove-llm-token").is_none());
        assert_eq!(label(window, "connector-count"), "Connectors: 0");
        assert_eq!(label(window, "llm-result"), "");
        assert_eq!(label(window, "connector-result"), "");
    });

    let lee = llm(
        Wire::OpenAiChat,
        "https://llm.example/v1",
        "m-lee",
        TokenChange::Set(TOKEN.into()),
    );
    rt.block_on(rig.engine.set_llm("lee", lee)).unwrap();
    let jira = connector("https://mcp.example/jira", TokenChange::Keep);
    rt.block_on(rig.engine.set_connector("lee", "jira", jira)).unwrap();
    on_screen(cx, window, |window, cx| window.click("profile-lee", cx));
    wait_until(cx, &shell, "lee's connections", |s| {
        s.state.selected.as_deref() == Some("lee") && s.state.connections.is_some()
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(value(window, "llm-address"), "https://llm.example/v1");
        assert_eq!(value(window, "llm-model"), "m-lee");
        assert_eq!(wire_checked(window), (Some(false), Some(true)));
        assert_eq!(label(window, "llm-token-state"), "token stored on the server");
        assert!(window.try_find("remove-llm-token").is_some());
        assert_eq!(label(window, "connector-count"), "Connectors: 1");
        assert_eq!(row(window, "jira", "name").as_deref(), Some("jira"));
        assert_eq!(
            row(window, "jira", "address").as_deref(),
            Some("https://mcp.example/jira")
        );
        assert_eq!(row(window, "jira", "token").as_deref(), Some("no token"));
        assert_not_on_screen(window, TOKEN, "https://llm.example/v1");
    });
}

#[gpui_kit::test]
fn saving_the_llm_endpoint_stores_the_token_on_the_server_only(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.within("llm-wire").click(1usize, cx);
        fill(window, cx, "llm-address", "https://llm.example/v1");
        fill(window, cx, "llm-model", "gpt-test");
        fill(window, cx, "llm-token", TOKEN);
        // The token sits in a masked field that reports no value.
        assert_eq!(window.find("llm-token").role(), Some(gpui_kit::Role::PasswordInput));
        assert_not_on_screen(window, TOKEN, "https://llm.example/v1");
        window.click("save-llm", cx);
    });
    wait_for_commands(cx, &shell, 1);

    let saved = rt.block_on(rig.engine.connections("kim")).unwrap().llm.unwrap();
    assert_eq!(
        (saved.wire, saved.base_url.as_str(), saved.model.as_str()),
        (Wire::OpenAiChat, "https://llm.example/v1", "gpt-test")
    );
    assert_eq!(saved.token, TokenSource::Stored);
    // The form has no timeout field. A new endpoint gets the server's default.
    assert_eq!(saved.timeout_secs, bunsin_core::settings::default_timeout_secs());
    assert!(rt.block_on(rig.engine.connections("lee")).unwrap().llm.is_none());
    let state = shell.read_with(cx, |s, _| format!("{:?}", s.state));
    assert!(state.contains("gpt-test") && !state.contains(TOKEN), "{state}");
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "llm-result"), "Saved");
        assert_eq!(label(window, "llm-token-state"), "token stored on the server");
        assert_eq!(value(window, "llm-address"), "https://llm.example/v1");
        assert_eq!(wire_checked(window), (Some(false), Some(true)));
        assert_not_on_screen(window, TOKEN, "https://llm.example/v1");
        // A changed form is no longer "Saved".
        fill(window, cx, "llm-model", "gpt-other");
        assert_eq!(label(window, "llm-result"), "");
        fill(window, cx, "llm-model", "gpt-test");
        assert_eq!(label(window, "llm-result"), "Saved");

        // The token field is empty now: moving the address to another host needs the token again.
        fill(window, cx, "llm-address", "https://other.example/v1");
        window.click("save-llm", cx);
    });
    wait_for_commands(cx, &shell, 2);
    on_screen(cx, window, |window, _| {
        let result = label(window, "llm-result");
        assert!(result.contains("enter the token again"), "{result}");
        assert_no_conflict(window, "llm");
        assert_eq!(value(window, "llm-address"), "https://other.example/v1");
        assert_eq!(value(window, "llm-model"), "gpt-test");
    });
    let kept = rt.block_on(rig.engine.connections("kim")).unwrap().llm.unwrap();
    assert_eq!(kept.base_url, "https://llm.example/v1");
    assert_eq!(kept.token, TokenSource::Stored);
}

#[gpui_kit::test]
fn a_connector_is_saved_and_its_address_changed_under_the_server_rules(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "jira");
        fill(window, cx, "connector-address", "https://mcp.example/jira");
        fill(window, cx, "connector-token", SLACK_TOKEN);
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 1);
    let jira = || rt.block_on(rig.engine.connections("kim")).unwrap().connectors["jira"].clone();
    let url = |url: &str| McpConfig::Http { url: url.into() };
    assert_eq!(jira().mcp, url("https://mcp.example/jira"));
    assert_eq!(jira().token, TokenSource::Stored);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-result"), "Saved: jira");
        // A successful save empties the form for the next connector.
        assert_eq!(value(window, "connector-name"), "");
        assert_eq!(value(window, "connector-address"), "");
        assert_eq!(
            row(window, "jira", "token").as_deref(),
            Some("token stored on the server")
        );
        assert_not_on_screen(window, SLACK_TOKEN, "https://mcp.example/jira");

        // The next connector, typed without a token, gets none: the saved token left the field.
        fill(window, cx, "connector-name", "confluence");
        fill(window, cx, "connector-address", "https://mcp.example/confluence");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 2);
    let connectors = rt.block_on(rig.engine.connections("kim")).unwrap().connectors;
    assert_eq!(connectors["confluence"].token, TokenSource::None);

    on_screen(cx, window, |window, cx| {
        // Edit fills the form. Another path on the same host keeps the token.
        click_row(window, cx, "jira", "edit");
        assert_eq!(value(window, "connector-name"), "jira");
        assert_eq!(value(window, "connector-address"), "https://mcp.example/jira");
        fill(window, cx, "connector-address", "https://mcp.example/jira/v2");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 3);
    assert_eq!(jira().mcp, url("https://mcp.example/jira/v2"));
    assert_eq!(jira().token, TokenSource::Stored);

    // Another host with an empty token field is refused by the server.
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-result"), "Saved: jira");
        click_row(window, cx, "jira", "edit");
        fill(window, cx, "connector-address", "https://evil.example/jira");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 4);
    on_screen(cx, window, |window, _| {
        let result = label(window, "connector-result");
        assert!(result.contains("enter the token again"), "{result}");
        assert_eq!(value(window, "connector-name"), "jira");
        assert_eq!(value(window, "connector-address"), "https://evil.example/jira");
        assert_eq!(
            row(window, "jira", "address").as_deref(),
            Some("https://mcp.example/jira/v2")
        );
    });
    assert_eq!(jira().mcp, url("https://mcp.example/jira/v2"));
    assert_eq!(jira().token, TokenSource::Stored);
}

#[gpui_kit::test]
fn a_stored_token_is_removed_only_with_an_empty_token_field(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let endpoint = llm(
        Wire::AnthropicMessages,
        "https://llm.example",
        "m1",
        TokenChange::Set(TOKEN.into()),
    );
    rt.block_on(rig.engine.set_llm("kim", endpoint)).unwrap();
    let jira = connector("https://mcp.example/jira", TokenChange::Set(SLACK_TOKEN.into()));
    rt.block_on(rig.engine.set_connector("kim", "jira", jira)).unwrap();
    let (window, shell) = open_settings(cx, &rt, &rig);
    let sources = || {
        let connections = rt.block_on(rig.engine.connections("kim")).unwrap();
        (connections.llm.unwrap().token, connections.connectors["jira"].token)
    };

    on_screen(cx, window, |window, cx| {
        // The remove control is offered only for the connector in the form.
        assert!(window.try_find("remove-connector-token").is_none());
        click_row(window, cx, "jira", "edit");
        // A typed token and Remove contradict each other. Nothing is sent.
        fill(window, cx, "connector-token", "xoxb-typed");
        window.click("remove-connector-token", cx);
        assert_eq!(
            label(window, "connector-message"),
            "Empty the token field first, or save the token."
        );
        fill(window, cx, "llm-token", "sk-typed");
        window.click("remove-llm-token", cx);
        assert_eq!(
            label(window, "llm-message"),
            "Empty the token field first, or save the token."
        );
    });
    std::thread::sleep(REFRESH * 2);
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |s, _| s.state.connection_commands), 0);
    assert_eq!(sources(), (TokenSource::Stored, TokenSource::Stored));

    on_screen(cx, window, |window, cx| {
        window.click("connector-token", cx);
        window.press("cmd-a", cx);
        window.press("backspace", cx);
        window.click("remove-connector-token", cx);
    });
    wait_for_commands(cx, &shell, 1);
    assert_eq!(sources(), (TokenSource::Stored, TokenSource::None));
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-result"), "Saved: jira");
        assert_eq!(label(window, "connector-message"), "");
        assert_eq!(row(window, "jira", "token").as_deref(), Some("no token"));
        window.click("llm-token", cx);
        window.press("cmd-a", cx);
        window.press("backspace", cx);
        window.click("remove-llm-token", cx);
    });
    wait_for_commands(cx, &shell, 2);
    assert_eq!(sources(), (TokenSource::None, TokenSource::None));
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "llm-token-state"), "no token");
        assert_eq!(label(window, "llm-message"), "");
        assert!(window.try_find("remove-llm-token").is_none());
    });
    // Removing the token kept the rest of the endpoint.
    let kept = rt.block_on(rig.engine.connections("kim")).unwrap().llm.unwrap();
    assert_eq!((kept.base_url.as_str(), kept.timeout_secs), ("https://llm.example", 30));
}

#[gpui_kit::test]
fn remove_deletes_the_connector_on_the_server_and_in_the_list(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    for name in ["jira", "confluence"] {
        let url = format!("https://mcp.example/{name}");
        let update = connector(&url, TokenChange::Set(SLACK_TOKEN.into()));
        rt.block_on(rig.engine.set_connector("kim", name, update)).unwrap();
    }
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-count"), "Connectors: 2");
        click_row(window, cx, "jira", "remove");
    });
    wait_for_commands(cx, &shell, 1);
    let connectors = rt.block_on(rig.engine.connections("kim")).unwrap().connectors;
    assert_eq!(connectors.keys().collect::<Vec<_>>(), ["confluence"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "connector-result"), "Removed: jira");
        assert_eq!(label(window, "connector-count"), "Connectors: 1");
        assert!(!has_row(window, "jira"));
        assert!(has_row(window, "confluence"));
    });
}

#[gpui_kit::test]
fn a_bad_name_and_a_refused_address_show_the_server_error_and_keep_the_draft(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "Bad Name");
        fill(window, cx, "connector-address", "https://mcp.example/x");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        let result = label(window, "connector-result");
        assert!(result.contains("the connector name `Bad Name` is invalid"), "{result}");
        assert_eq!(value(window, "connector-name"), "Bad Name");
        assert_eq!(value(window, "connector-address"), "https://mcp.example/x");
        fill(window, cx, "connector-name", "jira");
        fill(window, cx, "connector-address", "https://localhost/mcp");
        fill(window, cx, "connector-token", SLACK_TOKEN);
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 2);
    on_screen(cx, window, |window, _| {
        let result = label(window, "connector-result");
        assert!(
            result.contains("an address inside the server's own network"),
            "{result}"
        );
        assert_no_conflict(window, "connector");
        assert_eq!(value(window, "connector-name"), "jira");
        assert_eq!(value(window, "connector-address"), "https://localhost/mcp");
        assert_eq!(label(window, "connector-count"), "Connectors: 0");
        // The typed token stays in its masked field, and nowhere else.
        assert_not_on_screen(window, SLACK_TOKEN, "https://localhost/mcp");
    });
    assert!(
        rt.block_on(rig.engine.connections("kim"))
            .unwrap()
            .connectors
            .is_empty()
    );
}

#[gpui_kit::test]
fn check_shows_the_tools_or_the_error_and_approve_makes_the_list_clean(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    for name in ["slack", "jira"] {
        let url = format!("https://mcp.example/{name}");
        let update = connector(&url, TokenChange::Keep);
        rt.block_on(rig.engine.set_connector("kim", name, update)).unwrap();
    }
    // Slack lists a tool that the owner has not approved.
    rig.slack
        .set_tools(vec![tool("post", "Post a message"), tool("search", "Search")]);
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(row(window, "slack", "tools"), None);
        click_row(window, cx, "slack", "check");
    });
    wait_for_commands(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        let tools = row(window, "slack", "tools").unwrap();
        assert!(tools.starts_with("works: 2 tool(s)"), "{tools}");
        assert!(tools.contains("1 new (search)"), "{tools}");
        assert_eq!(
            row(window, "slack", "approve-note").as_deref(),
            Some("Approving tools does not create permission rules.")
        );
        click_row(window, cx, "slack", "approve-tools");
    });
    wait_for_commands(cx, &shell, 2);
    let report = rt.block_on(rig.engine.connector_tools("kim", "slack")).unwrap();
    assert!(report.diff.is_clean(), "{:?}", report.diff);
    assert_eq!(report.tools.len(), 2);
    on_screen(cx, window, |window, cx| {
        assert_eq!(row(window, "slack", "tools").as_deref(), Some("Tools approved"));
        assert_eq!(row(window, "slack", "approve-tools"), None);
        click_row(window, cx, "slack", "check");
    });
    wait_for_commands(cx, &shell, 3);
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            row(window, "slack", "tools").as_deref(),
            Some("works: 2 tool(s), the same as the approved list")
        );
        assert_eq!(row(window, "slack", "approve-tools"), None);
        click_row(window, cx, "jira", "check");
    });
    wait_for_commands(cx, &shell, 4);
    on_screen(cx, window, |window, _| {
        let tools = row(window, "jira", "tools").unwrap();
        assert!(tools.contains(common::REFUSED), "{tools}");
        assert_eq!(row(window, "jira", "approve-tools"), None);
        // The result shows next to the connector it belongs to.
        assert_eq!(row(window, "slack", "tools"), None);
    });
}

#[gpui_kit::test]
fn a_refresh_does_not_overwrite_the_connection_drafts(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.within("llm-wire").click(1usize, cx);
        fill(window, cx, "llm-address", "https://typed.example/v1");
        fill(window, cx, "llm-model", "typed-model");
        fill(window, cx, "llm-token", TOKEN);
        fill(window, cx, "connector-name", "typed");
        fill(window, cx, "connector-address", "https://typed.example/mcp");
    });

    // Another client changes the server's connections, so a refresh brings a new state.
    let other = llm(
        Wire::AnthropicMessages,
        "https://server.example",
        "server-model",
        TokenChange::Keep,
    );
    rt.block_on(rig.engine.set_llm("kim", other)).unwrap();
    wait_until(cx, &shell, "the refreshed connections", |s| {
        s.state.connections.as_ref().is_some_and(|c| c.llm.is_some())
    });
    // Let more refreshes run.
    std::thread::sleep(REFRESH * 4);
    cx.run_until_parked();

    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "llm-address"), "https://typed.example/v1");
        assert_eq!(value(window, "llm-model"), "typed-model");
        assert_eq!(wire_checked(window), (Some(false), Some(true)));
        assert_eq!(value(window, "connector-name"), "typed");
        assert_eq!(value(window, "connector-address"), "https://typed.example/mcp");
        window.click("save-llm", cx);
    });
    // The form was made from "no endpoint". The server has one now, so the save is refused.
    wait_for_commands(cx, &shell, 1);
    let server = rt.block_on(rig.engine.connections("kim")).unwrap().llm.unwrap();
    assert_eq!(
        (server.base_url.as_str(), server.token),
        ("https://server.example", TokenSource::None)
    );
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "llm-conflict"), CONFLICT);
        assert_eq!(label(window, "llm-result"), "");
        assert_eq!(value(window, "llm-address"), "https://typed.example/v1");
        assert_eq!(value(window, "llm-model"), "typed-model");
        assert_not_on_screen(window, TOKEN, "https://typed.example/v1");
        // The token field still holds the typed token: "Save anyway" stores it.
        window.click("llm-save-anyway", cx);
    });
    wait_for_commands(cx, &shell, 2);
    on_screen(cx, window, |window, _| {
        assert_no_conflict(window, "llm");
        assert_eq!(label(window, "llm-result"), "Saved");
    });
    let saved = rt.block_on(rig.engine.connections("kim")).unwrap().llm.unwrap();
    assert_eq!(
        (saved.base_url.as_str(), saved.model.as_str(), saved.wire, saved.token),
        (
            "https://typed.example/v1",
            "typed-model",
            Wire::OpenAiChat,
            TokenSource::Stored
        )
    );
    // The timeout that the other client set stays.
    assert_eq!(saved.timeout_secs, 30);
}

/// Adds connector `name` to kim from outside the window, then waits until a refresh shows it
/// and lets more refreshes run. The state still carries the result of the last command.
fn change_from_outside(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
    shell: &Entity<Shell>,
    name: &'static str,
) {
    let url = format!("https://mcp.example/{name}");
    rt.block_on(
        rig.engine
            .set_connector("kim", name, connector(&url, TokenChange::Keep)),
    )
    .unwrap();
    wait_until(cx, shell, "the refreshed connections", |s| {
        s.state
            .connections
            .as_ref()
            .is_some_and(|c| c.connectors.contains_key(name))
    });
    std::thread::sleep(REFRESH * 4);
    cx.run_until_parked();
}

#[gpui_kit::test]
fn drafts_typed_after_a_successful_save_survive_a_refresh(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    let result = |cx: &mut TestAppContext| shell.read_with(cx, |s, _| s.state.connection_result.clone());
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "llm-address", "https://llm.example");
        fill(window, cx, "llm-model", "saved-model");
        window.click("save-llm", cx);
    });
    wait_for_commands(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "llm-model", "typed-model");
        fill(window, cx, "connector-name", "typed");
    });
    change_from_outside(cx, &rt, &rig, &shell, "first");
    assert_eq!(result(cx), Some(ConnectionResult::LlmSaved(Ok(()))));
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "llm-model"), "typed-model");
        assert_eq!(value(window, "connector-name"), "typed");
        fill(window, cx, "connector-name", "jira");
        fill(window, cx, "connector-address", "https://mcp.example/jira");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 2);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "next");
        fill(window, cx, "connector-address", "https://mcp.example/next");
    });
    change_from_outside(cx, &rt, &rig, &shell, "second");
    let saved = ConnectionResult::ConnectorSaved {
        name: "jira".into(),
        result: Ok(()),
    };
    assert_eq!(result(cx), Some(saved));
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "connector-name"), "next");
        assert_eq!(value(window, "connector-address"), "https://mcp.example/next");
        assert_eq!(value(window, "llm-model"), "typed-model");
        // Check is not a save. It leaves the drafts alone.
        click_row(window, cx, "jira", "check");
    });
    wait_for_commands(cx, &shell, 3);
    on_screen(cx, window, |window, _| {
        assert_eq!(value(window, "connector-name"), "next");
        assert_eq!(value(window, "llm-model"), "typed-model");
    });
}

#[gpui_kit::test]
fn another_profile_discards_the_drafts_and_saves_only_to_itself(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "llm-address", "https://kim.example");
        fill(window, cx, "llm-model", "kim-model");
        fill(window, cx, "llm-token", TOKEN);
        fill(window, cx, "connector-name", "kim-only");
        fill(window, cx, "connector-address", "https://kim.example/mcp");
        fill(window, cx, "connector-token", SLACK_TOKEN);
        window.click("profile-lee", cx);
    });
    wait_until(cx, &shell, "lee's connections", |s| {
        s.state.selected.as_deref() == Some("lee") && s.state.connections.is_some()
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "llm-address"), ANTHROPIC_BASE_URL);
        assert_eq!(value(window, "llm-model"), ANTHROPIC_DEFAULT_MODEL);
        for id in ["connector-name", "connector-address"] {
            assert_eq!(value(window, id), "", "{id}");
        }
        // Save without typing a token. A token left over from kim would be stored for lee.
        fill(window, cx, "llm-address", "https://lee.example");
        fill(window, cx, "llm-model", "lee-model");
        window.click("save-llm", cx);
        fill(window, cx, "connector-name", "jira");
        fill(window, cx, "connector-address", "https://lee.example/jira");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 2);
    let lee = rt.block_on(rig.engine.connections("lee")).unwrap();
    let lee_llm = lee.llm.unwrap();
    assert_eq!(
        (lee_llm.base_url.as_str(), lee_llm.token),
        ("https://lee.example", TokenSource::None)
    );
    assert_eq!(lee.connectors["jira"].token, TokenSource::None);
    let kim = rt.block_on(rig.engine.connections("kim")).unwrap();
    assert!(kim.llm.is_none() && kim.connectors.is_empty(), "{kim:?}");
}

/// Stores a command connector through the settings, as `config-set` does.
fn store_command(rt: &tokio::runtime::Runtime, rig: &common::Rig, name: &str, mcp: McpConfig) {
    let section = ConnectorSection {
        mcp,
        token_secret: None,
        token_header: None,
        secret_env: Default::default(),
    };
    let mut config = rt.block_on(rig.engine.profile_config("kim")).unwrap();
    config
        .connectors
        .insert(name.into(), serde_json::to_value(&section).unwrap());
    rt.block_on(rig.engine.set_profile_config("kim", config)).unwrap();
}

/// The text of the area inside element `id`.
fn area(window: &mut Window, id: &'static str) -> String {
    let area = window.within(id).find("input-state");
    area.value().unwrap_or_default().to_owned()
}

/// Replaces the text of the area inside element `id`.
fn fill_area(window: &mut Window, cx: &mut App, id: &'static str, text: &str) {
    window.within(id).click("input-state", cx);
    window.press("cmd-a", cx);
    window.press("backspace", cx);
    window.input(text, cx);
}

/// Chooses the kind "Command on the server" in the connector form.
fn choose_command(cx: &mut TestAppContext, window: gpui_kit::AnyWindowHandle) {
    on_screen(cx, window, |window, cx| {
        window.within("connector-kind").click(1usize, cx)
    });
}

const ENV_SECRET: &str = "env-secret-9d04";

#[gpui_kit::test]
fn a_command_connector_shows_its_command_and_counts_not_its_arguments(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let stdio = McpConfig::Stdio {
        command: "/usr/local/bin/local-mcp".into(),
        args: vec!["--secret-arg".into()],
        env: [("API_TOKEN".to_owned(), ENV_SECRET.to_owned())].into(),
    };
    store_command(&rt, &rig, "local", stdio.clone());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            row(window, "local", "address").as_deref(),
            Some("runs the command /usr/local/bin/local-mcp")
        );
        assert_eq!(
            row(window, "local", "command-counts").as_deref(),
            Some("1 argument, 1 environment variable")
        );
        // The list shows no argument and no value. They can hold a secret.
        assert_not_on_screen(window, "--secret-arg", "runs the command");
        assert_not_on_screen(window, ENV_SECRET, "runs the command");
        // An address cannot take the name of a command. The window says so and sends nothing.
        fill(window, cx, "connector-name", "local");
        fill(window, cx, "connector-address", "https://mcp.example/local");
        window.click("save-connector", cx);
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "connector-message"),
            "A connector with this name is of the other kind. Remove it first, or use another name."
        );
        assert_eq!(value(window, "connector-address"), "https://mcp.example/local");
        assert!(window.try_find("connector-save-anyway").is_none());
    });
    std::thread::sleep(REFRESH * 2);
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |s, _| s.state.connection_commands), 0);
    let connectors = rt.block_on(rig.engine.connections("kim")).unwrap().connectors;
    assert_eq!(connectors["local"].mcp, stdio);
}

#[gpui_kit::test]
fn a_command_connector_is_saved_edited_and_removed_in_the_window(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    let local = || {
        rt.block_on(rig.engine.connections("kim"))
            .unwrap()
            .connectors
            .get("local")
            .map(|c| c.mcp.clone())
    };
    choose_command(cx, window);
    on_screen(cx, window, |window, cx| {
        // Only the fields of the chosen kind are shown.
        assert!(window.try_find("connector-address").is_none());
        assert!(label(window, "command-help").contains("--allow-stdio-connectors"));
        fill(window, cx, "connector-name", "local");
        fill(window, cx, "connector-command", "/opt/mcp/does-not-exist");
        // A line is one argument, with the spaces in it. The first `=` ends a name.
        fill_area(window, cx, "connector-args", "-y\n\nsome server --flag");
        fill_area(
            window,
            cx,
            "connector-env",
            &format!("API_TOKEN={ENV_SECRET}=x\nEMPTY="),
        );
    });
    // A refresh does not change the draft.
    std::thread::sleep(REFRESH * 4);
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "connector-counts"),
            "2 arguments, 2 environment variables"
        );
        assert_eq!(area(window, "connector-args"), "-y\n\nsome server --flag");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 1);
    let stored = McpConfig::Stdio {
        command: "/opt/mcp/does-not-exist".into(),
        args: vec!["-y".into(), "some server --flag".into()],
        env: [
            ("API_TOKEN".to_owned(), format!("{ENV_SECRET}=x")),
            ("EMPTY".to_owned(), String::new()),
        ]
        .into(),
    };
    assert_eq!(local(), Some(stored));
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-result"), "Saved: local");
        // A successful save empties the form. The kind stays for the next connector.
        assert_eq!(value(window, "connector-name"), "");
        assert_eq!(value(window, "connector-command"), "");
        assert_eq!(area(window, "connector-env"), "");
        assert_eq!(
            row(window, "local", "address").as_deref(),
            Some("runs the command /opt/mcp/does-not-exist")
        );
        assert_eq!(
            row(window, "local", "command-counts").as_deref(),
            Some("2 arguments, 2 environment variables")
        );
        assert_not_on_screen(window, ENV_SECRET, "runs the command");
        click_row(window, cx, "local", "edit");
    });
    // Edit shows the stored command. A changed argument is saved, and the rest stays.
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "connector-name"), "local");
        assert_eq!(value(window, "connector-command"), "/opt/mcp/does-not-exist");
        assert_eq!(area(window, "connector-args"), "-y\nsome server --flag");
        assert_eq!(
            area(window, "connector-env"),
            format!("API_TOKEN={ENV_SECRET}=x\nEMPTY=")
        );
        fill_area(window, cx, "connector-args", "--stdio");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 2);
    match local() {
        Some(McpConfig::Stdio { args, env, .. }) => {
            assert_eq!(args, vec!["--stdio".to_owned()]);
            assert_eq!(env.len(), 2);
        }
        other => panic!("expected a command, got {other:?}"),
    }
    on_screen(cx, window, |window, cx| click_row(window, cx, "local", "remove"));
    wait_for_commands(cx, &shell, 3);
    assert_eq!(local(), None);
    on_screen(cx, window, |window, _| {
        // The list is empty again.
        assert_eq!(label(window, "connector-count"), "Connectors: 0");
        assert_eq!(label(window, "connector-result"), "Removed: local");
    });
}

#[gpui_kit::test]
fn a_bad_environment_line_is_not_sent_and_a_server_refusal_keeps_the_draft(cx: &mut TestAppContext) {
    let rt = runtime();
    // The operator of this server does not allow connectors that run a command.
    let rig = rt.block_on(common::rig_with(1_000, false));
    let (window, shell) = open_settings(cx, &rt, &rig);
    choose_command(cx, window);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "local");
        fill(window, cx, "connector-command", "/opt/mcp/does-not-exist");
        fill_area(window, cx, "connector-env", "API_TOKEN");
        window.click("save-connector", cx);
    });
    let no_equals = "Write each environment variable as NAME=value, one in a line.";
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-message"), no_equals);
        assert_eq!(label(window, "connector-counts"), no_equals);
        fill_area(window, cx, "connector-env", "=value");
        window.click("save-connector", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-message"), no_equals);
        fill_area(window, cx, "connector-env", "A=1\nA=2");
        window.click("save-connector", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "connector-message"),
            "An environment variable is in the list twice."
        );
        fill_area(window, cx, "connector-env", &format!("API_TOKEN={ENV_SECRET}"));
    });
    std::thread::sleep(REFRESH * 2);
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |s, _| s.state.connection_commands), 0);

    // The server refuses the save. The reason names the flag, and the draft stays.
    on_screen(cx, window, |window, cx| window.click("save-connector", cx));
    wait_for_commands(cx, &shell, 1);
    on_screen(cx, window, |window, _| {
        let result = label(window, "connector-result");
        assert!(result.contains("--allow-stdio-connectors"), "{result}");
        assert!(!result.contains(ENV_SECRET), "{result}");
        assert_eq!(value(window, "connector-command"), "/opt/mcp/does-not-exist");
        assert_eq!(area(window, "connector-env"), format!("API_TOKEN={ENV_SECRET}"));
    });
    assert!(
        rt.block_on(rig.engine.connections("kim"))
            .unwrap()
            .connectors
            .is_empty()
    );
}

#[gpui_kit::test]
fn a_command_the_form_cannot_show_is_not_loaded_for_editing(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let unusual = McpConfig::Stdio {
        command: "/opt/mcp/does-not-exist".into(),
        // A line-based form would drop the empty argument and split the second one.
        args: vec!["".into(), "two\nlines".into()],
        env: Default::default(),
    };
    store_command(&rt, &rig, "local", unusual.clone());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "draft");
        click_row(window, cx, "local", "edit");
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "connector-message"),
            "This connector has an empty argument or a line break in a value. \
             This form cannot show it. Change it in the settings file."
        );
        // The draft is as it was. Nothing of the connector was loaded.
        assert_eq!(value(window, "connector-name"), "draft");
    });
    std::thread::sleep(REFRESH * 2);
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |s, _| s.state.connection_commands), 0);
    assert_eq!(
        rt.block_on(rig.engine.connections("kim")).unwrap().connectors["local"].mcp,
        unusual
    );
}

#[gpui_kit::test]
fn a_command_draft_follows_the_draft_rules_of_the_form(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let command = |arg: &str| McpConfig::Stdio {
        command: "/opt/mcp/does-not-exist".into(),
        args: vec![arg.into()],
        env: Default::default(),
    };
    store_command(&rt, &rig, "local", command("--first"));
    let (window, shell) = open_settings(cx, &rt, &rig);
    let server_arg = |cx: &mut TestAppContext, shell: &Entity<Shell>, arg: &'static str| {
        wait_until(cx, shell, "the outside change", move |s| {
            s.state.connections.as_ref().is_some_and(
                |c| matches!(&c.connectors["local"].mcp, McpConfig::Stdio { args, .. } if args == &[arg.to_owned()]),
            )
        });
    };
    on_screen(cx, window, |window, cx| click_row(window, cx, "local", "edit"));
    // An unchanged draft follows the server.
    store_command(&rt, &rig, "local", command("--second"));
    server_arg(cx, &shell, "--second");
    on_screen(cx, window, |window, cx| {
        assert_eq!(area(window, "connector-args"), "--second");
        // A changed draft stays, and its save is refused after another change on the server.
        fill_area(window, cx, "connector-args", "--mine");
    });
    store_command(&rt, &rig, "local", command("--third"));
    server_arg(cx, &shell, "--third");
    on_screen(cx, window, |window, cx| {
        assert_eq!(area(window, "connector-args"), "--mine");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-conflict"), CONFLICT);
        window.click("connector-load", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(area(window, "connector-args"), "--third");
        assert_no_conflict(window, "connector");
        // Another profile gets an empty form.
        fill_area(window, cx, "connector-args", "--typed");
        window.click("profile-lee", cx);
    });
    wait_until(cx, &shell, "the other profile", |s| {
        s.state.selected.as_deref() == Some("lee") && s.state.connections.is_some()
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(value(window, "connector-name"), "");
        // The kind stays, so the command fields are on screen and empty.
        assert_eq!(value(window, "connector-command"), "");
        assert_eq!(area(window, "connector-args"), "");
    });
}

#[gpui_kit::test]
fn with_no_endpoint_a_token_alone_saves_the_claude_api(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(wire_checked(window), (Some(true), Some(false)));
        assert_eq!(value(window, "llm-address"), ANTHROPIC_BASE_URL);
        assert_eq!(value(window, "llm-model"), ANTHROPIC_DEFAULT_MODEL);
        fill(window, cx, "llm-token", TOKEN);
        window.click("save-llm", cx);
    });
    wait_for_commands(cx, &shell, 1);
    let saved = rt.block_on(rig.engine.connections("kim")).unwrap().llm.unwrap();
    assert_eq!(
        (saved.wire, saved.base_url.as_str(), saved.model.as_str(), saved.token),
        (
            Wire::AnthropicMessages,
            ANTHROPIC_BASE_URL,
            ANTHROPIC_DEFAULT_MODEL,
            TokenSource::Stored
        )
    );
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "llm-result"), "Saved");
        assert_not_on_screen(window, TOKEN, ANTHROPIC_BASE_URL);
    });
}

#[gpui_kit::test]
fn unchanged_connection_forms_follow_the_server(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    add_jira(&rt, &rig);
    let (window, shell) = open_settings(cx, &rt, &rig);
    // The prefilled LLM form and an edited-then-untouched connector form.
    on_screen(cx, window, |window, cx| click_row(window, cx, "jira", "edit"));

    let other = llm(
        Wire::OpenAiChat,
        "https://server.example/v1",
        "server-model",
        TokenChange::Keep,
    );
    rt.block_on(rig.engine.set_llm("kim", other)).unwrap();
    wait_until(cx, &shell, "the outside endpoint", |s| {
        s.state.connections.as_ref().is_some_and(|c| c.llm.is_some())
    });
    set_connector_from_outside(cx, &rt, &rig, &shell, "jira", "https://mcp.example/jira/moved");
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "llm-address"), "https://server.example/v1");
        assert_eq!(value(window, "llm-model"), "server-model");
        assert_eq!(wire_checked(window), (Some(false), Some(true)));
        assert_eq!(value(window, "connector-name"), "jira");
        assert_eq!(value(window, "connector-address"), "https://mcp.example/jira/moved");
        assert_no_conflict(window, "llm");
        assert_no_conflict(window, "connector");
        // The followed values are the new bases: saving them is not a conflict.
        fill(window, cx, "llm-model", "my-model");
        window.click("save-llm", cx);
        fill(window, cx, "connector-address", "https://mcp.example/jira/mine");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 2);
    let saved = rt.block_on(rig.engine.connections("kim")).unwrap();
    assert_eq!(saved.llm.unwrap().model, "my-model");
    assert_eq!(
        saved.connectors["jira"].mcp,
        McpConfig::Http {
            url: "https://mcp.example/jira/mine".into()
        }
    );
}

/// Saves connector `jira` for kim before a window opens.
fn add_jira(rt: &tokio::runtime::Runtime, rig: &common::Rig) {
    let jira = connector("https://mcp.example/jira", TokenChange::Keep);
    rt.block_on(rig.engine.set_connector("kim", "jira", jira)).unwrap();
}

#[gpui_kit::test]
fn a_connector_save_over_an_outside_change_warns_and_save_anyway_overwrites(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    add_jira(&rt, &rig);
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        click_row(window, cx, "jira", "edit");
        fill(window, cx, "connector-address", "https://mcp.example/jira/mine");
    });
    set_connector_from_outside(cx, &rt, &rig, &shell, "jira", "https://mcp.example/jira/outside");
    on_screen(cx, window, |window, cx| {
        // The edited draft did not follow the server.
        assert_eq!(value(window, "connector-address"), "https://mcp.example/jira/mine");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 1);
    let jira = || {
        rt.block_on(rig.engine.connections("kim")).unwrap().connectors["jira"]
            .mcp
            .clone()
    };
    let url = |url: &str| McpConfig::Http { url: url.into() };
    assert_eq!(jira(), url("https://mcp.example/jira/outside"));
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-conflict"), CONFLICT);
        assert_eq!(label(window, "connector-result"), "");
        assert_eq!(value(window, "connector-name"), "jira");
        assert_eq!(value(window, "connector-address"), "https://mcp.example/jira/mine");
        // The warning belongs to `jira`. Under another name it is not shown.
        fill(window, cx, "connector-name", "wiki");
        assert_no_conflict(window, "connector");
        fill(window, cx, "connector-name", "jira");
        window.click("connector-save-anyway", cx);
    });
    wait_for_commands(cx, &shell, 2);
    assert_eq!(jira(), url("https://mcp.example/jira/mine"));
    on_screen(cx, window, |window, _| {
        assert_no_conflict(window, "connector");
        assert_eq!(label(window, "connector-result"), "Saved: jira");
    });
}

#[gpui_kit::test]
fn a_second_new_connector_with_the_same_name_warns_and_load_shows_the_first(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "wiki");
        fill(window, cx, "connector-address", "https://mcp.example/wiki/mine");
        fill(window, cx, "connector-token", SLACK_TOKEN);
    });
    // Another client adds a new connector with the same name first.
    let first = ConnectorUpdate {
        token_header: None,
        expected: Expected::Value(None),
        ..connector("https://mcp.example/wiki/first", TokenChange::Keep)
    };
    rt.block_on(rig.engine.set_connector("kim", "wiki", first)).unwrap();
    wait_until(cx, &shell, "the outside connector", |s| {
        s.state
            .connections
            .as_ref()
            .is_some_and(|c| c.connectors.contains_key("wiki"))
    });
    on_screen(cx, window, |window, cx| window.click("save-connector", cx));
    wait_for_commands(cx, &shell, 1);
    let wiki = rt.block_on(rig.engine.connections("kim")).unwrap().connectors["wiki"].clone();
    assert_eq!(
        (wiki.mcp, wiki.token),
        (
            McpConfig::Http {
                url: "https://mcp.example/wiki/first".into()
            },
            TokenSource::None
        )
    );
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "connector-conflict"), CONFLICT);
        assert_eq!(value(window, "connector-address"), "https://mcp.example/wiki/mine");
        assert_not_on_screen(window, SLACK_TOKEN, "https://mcp.example/wiki/mine");
        window.click("connector-load", cx);
        assert_no_conflict(window, "connector");
        assert_eq!(value(window, "connector-name"), "wiki");
        assert_eq!(value(window, "connector-address"), "https://mcp.example/wiki/first");
        // Load dropped the typed token too. A save now changes nothing.
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 2);
    let wiki = rt.block_on(rig.engine.connections("kim")).unwrap().connectors["wiki"].clone();
    assert_eq!(wiki.token, TokenSource::None);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "connector-result"), "Saved: wiki");
    });
}

#[gpui_kit::test]
fn save_anyway_after_a_refused_token_removal_removes_the_token(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let endpoint = |model: &str, token| llm(Wire::AnthropicMessages, "https://llm.example", model, token);
    rt.block_on(
        rig.engine
            .set_llm("kim", endpoint("m1", TokenChange::Set(TOKEN.into()))),
    )
    .unwrap();
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| fill(window, cx, "llm-model", "my-model"));
    rt.block_on(rig.engine.set_llm("kim", endpoint("outside-model", TokenChange::Keep)))
        .unwrap();
    wait_until(cx, &shell, "the outside change", |s| {
        s.state
            .connections
            .as_ref()
            .and_then(|c| c.llm.as_ref())
            .is_some_and(|llm| llm.model == "outside-model")
    });
    on_screen(cx, window, |window, cx| window.click("remove-llm-token", cx));
    wait_for_commands(cx, &shell, 1);
    let server = || rt.block_on(rig.engine.connections("kim")).unwrap().llm.unwrap();
    assert_eq!(
        (server().model.as_str(), server().token),
        ("outside-model", TokenSource::Stored)
    );
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "llm-conflict"), CONFLICT);
        window.click("llm-save-anyway", cx);
    });
    wait_for_commands(cx, &shell, 2);
    // "Save anyway" repeats the refused save: the token goes, and the typed model is saved.
    assert_eq!(
        (server().model.as_str(), server().token),
        ("my-model", TokenSource::None)
    );
}

#[gpui_kit::test]
fn a_saved_endpoint_is_shown_not_the_defaults(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let saved = llm(
        Wire::AnthropicMessages,
        "https://gateway.example",
        "saved-model",
        TokenChange::Keep,
    );
    rt.block_on(rig.engine.set_llm("kim", saved)).unwrap();
    let (window, _shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        assert_eq!(value(window, "llm-address"), "https://gateway.example");
        assert_eq!(value(window, "llm-model"), "saved-model");
        assert_eq!(label(window, "llm-token-state"), "no token");
    });
}

#[gpui_kit::test]
fn another_format_empties_the_claude_defaults_and_keeps_typed_text(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    // The third choice is the Responses format. The Claude address and model do not fit it.
    on_screen(cx, window, |window, cx| window.within("llm-wire").click(2usize, cx));
    on_screen(cx, window, |window, cx| {
        assert_eq!(window.within("llm-wire").find(2usize).checked(), Some(true));
        assert_eq!(value(window, "llm-address"), "");
        assert_eq!(value(window, "llm-model"), "");
        // Empty fields get the Claude values back with the Claude format.
        window.within("llm-wire").click(0usize, cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "llm-address"), ANTHROPIC_BASE_URL);
        assert_eq!(value(window, "llm-model"), ANTHROPIC_DEFAULT_MODEL);
        // Text that the user typed stays when the format changes.
        fill(window, cx, "llm-address", "https://gateway.example/v1");
        fill(window, cx, "llm-model", "my-model");
        window.within("llm-wire").click(2usize, cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "llm-address"), "https://gateway.example/v1");
        assert_eq!(value(window, "llm-model"), "my-model");
        fill(window, cx, "llm-token", TOKEN);
        window.click("save-llm", cx);
    });
    wait_for_commands(cx, &shell, 1);
    let saved = rt.block_on(rig.engine.connections("kim")).unwrap().llm.unwrap();
    assert_eq!(
        (saved.wire, saved.base_url.as_str(), saved.model.as_str()),
        (Wire::OpenAiResponses, "https://gateway.example/v1", "my-model")
    );
}

#[gpui_kit::test]
fn the_header_of_a_token_is_saved_shown_and_kept_by_an_edit(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "wiki");
        fill(window, cx, "connector-address", "https://mcp.example/wiki");
        fill(window, cx, "connector-token", SLACK_TOKEN);
        fill(window, cx, "connector-token-header", "X-API-Key");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 1);
    let wiki = || rt.block_on(rig.engine.connections("kim")).unwrap().connectors["wiki"].clone();
    assert_eq!(wiki().token_header.as_deref(), Some("X-API-Key"));
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            row(window, "wiki", "token-header").as_deref(),
            Some("sent in the header X-API-Key")
        );
        // A successful save empties the header field too.
        assert_eq!(value(window, "connector-token-header"), "");
        click_row(window, cx, "wiki", "edit");
    });
    // Edit shows the header. A save that changes only the address keeps it.
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "connector-token-header"), "X-API-Key");
        fill(window, cx, "connector-address", "https://mcp.example/wiki/v2");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 2);
    assert_eq!(wiki().token_header.as_deref(), Some("X-API-Key"));
    assert_eq!(
        wiki().mcp,
        McpConfig::Http {
            url: "https://mcp.example/wiki/v2".into()
        }
    );

    // An emptied header field goes back to `Authorization: Bearer`.
    on_screen(cx, window, |window, cx| click_row(window, cx, "wiki", "edit"));
    on_screen(cx, window, |window, cx| {
        // Typing nothing does not remove the selected text. Backspace does.
        window.click("connector-token-header", cx);
        window.press("cmd-a", cx);
        window.press("backspace", cx);
        assert_eq!(value(window, "connector-token-header"), "");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 3);
    assert_eq!(wiki().token_header, None);
    on_screen(cx, window, |window, _| {
        assert!(row(window, "wiki", "token-header").is_none())
    });

    // A header that the connection sets itself is refused with the server's reason.
    on_screen(cx, window, |window, cx| click_row(window, cx, "wiki", "edit"));
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-token-header", "Host");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 4);
    on_screen(cx, window, |window, _| {
        assert!(label(window, "connector-result").contains("cannot carry a token"));
        assert_eq!(value(window, "connector-token-header"), "Host");
    });
    assert_eq!(wiki().token_header, None);
}

#[gpui_kit::test]
fn test_sends_one_request_to_the_saved_endpoint_and_shows_the_result(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    // The rig made requests for its own event. Only the ones after this count.
    let before = rig.llm.requests().len();
    // Nothing is saved yet. The test would not use the fields, so it is refused.
    on_screen(cx, window, |window, cx| window.click("test-llm", cx));
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "llm-message"),
            "Save the endpoint first. The test uses the saved settings."
        );
        assert_eq!(label(window, "llm-test"), "");
    });
    assert_eq!(rig.llm.requests().len(), before);

    let saved = llm(
        Wire::OpenAiResponses,
        "https://llm.example/v1",
        "m",
        TokenChange::Set(TOKEN.into()),
    );
    rt.block_on(rig.engine.set_llm("kim", saved)).unwrap();
    wait_until(cx, &shell, "the saved endpoint", |s| {
        s.state.connections.as_ref().is_some_and(|c| c.llm.is_some())
    });
    let ignore = r#"{"kind":"ignore","rationale":"connection test","summary":null,"tool":null,"args_json":null}"#;
    rig.llm.reply(ignore, 12);
    on_screen(cx, window, |window, cx| window.click("test-llm", cx));
    wait_for_commands(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "llm-test"),
            "Works: the endpoint answered in the form the agent needs (12 input tokens, 0 output tokens)"
        );
        assert_eq!(label(window, "llm-message"), "");
        // A changed field is a draft again. The test is refused until it is saved.
        fill(window, cx, "llm-model", "another-model");
        window.click("test-llm", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "llm-message"),
            "Save the endpoint first. The test uses the saved settings."
        );
        // The earlier "Works" was for the saved model. It is not shown beside another one.
        assert_eq!(label(window, "llm-test"), "");
        fill(window, cx, "llm-model", "m");
    });
    assert_eq!(rig.llm.requests().len(), before + 1);

    // An answer in another form is a failed test, with the reason.
    rig.llm.reply("Hello! How can I help?", 12);
    on_screen(cx, window, |window, cx| window.click("test-llm", cx));
    wait_for_commands(cx, &shell, 2);
    on_screen(cx, window, |window, _| {
        let shown = label(window, "llm-test");
        assert!(
            shown.starts_with("Does not work: the endpoint answered, but not in the form the agent needs"),
            "{shown}"
        );
        assert_not_on_screen(window, TOKEN, "https://llm.example/v1");
    });
    assert_eq!(rig.llm.requests().len(), before + 2);
}

#[gpui_kit::test]
fn the_window_says_that_a_test_runs_until_its_result_arrives(cx: &mut TestAppContext) {
    use bunsin_api::{Connections, LlmConnection, LlmTest};
    use bunsin_app::bridge::{ConnectionResult, UiState};
    use gpui_kit::AppContext;
    // The test is the bridge: it sends the states, and it answers no command.
    let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (states, receiver) = tokio::sync::mpsc::unbounded_channel();
    cx.update(gpui_kit::init);
    let (window, _shell) = cx.update(|cx| {
        gpui_kit::open_window(Default::default(), cx, |window, cx| {
            cx.new(|cx| Shell::new(receiver, commands, window, cx))
        })
        .unwrap()
    });
    let saved = UiState {
        selected: Some("kim".into()),
        connections: Some(Connections {
            llm: Some(LlmConnection {
                wire: Wire::OpenAiResponses,
                base_url: "https://llm.example/v1".into(),
                model: "m".into(),
                timeout_secs: 120,
                token: TokenSource::Stored,
            }),
            connectors: Default::default(),
        }),
        connected: true,
        ..UiState::default()
    };
    states.send(saved.clone()).unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| window.click("tab-settings", cx));
    on_screen(cx, window, |window, cx| window.click("test-llm", cx));
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "llm-test"),
            "Testing… One short request goes to the endpoint. It waits for the answer up to the timeout."
        );
    });
    // The first command was the refresh of a new window. The second is the test.
    let sent: Vec<_> = std::iter::from_fn(|| received.try_recv().ok()).collect();
    assert_eq!(
        sent.last(),
        Some(&bunsin_app::bridge::UiCommand::TestLlm { profile: "kim".into() })
    );

    // A refresh without a new command result does not end the text.
    states.send(saved.clone()).unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, _| {
        assert!(label(window, "llm-test").starts_with("Testing…"))
    });

    let done = UiState {
        connection_result: Some(ConnectionResult::LlmTested(Ok(LlmTest {
            works: true,
            detail: "the endpoint answered".into(),
        }))),
        connection_commands: 1,
        ..saved
    };
    states.send(done).unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "llm-test"), "Works: the endpoint answered");
    });
}

const KEYCHAIN_VALUE: &str = "sk-keychain-VALUE-41c7";
const KEYCHAIN_READ: &str = "The token is in the token field now. Save sends it to the server.";

/// A Keychain with one item. It counts the lookups. No test reads the real Keychain.
#[derive(Default)]
struct FakeKeychain(std::sync::Mutex<usize>);

impl bunsin_app::connection::KeychainLookup for FakeKeychain {
    fn password(&self, service: &str, account: &str) -> Result<String, String> {
        *self.0.lock().unwrap() += 1;
        match (service, account) {
            // The stored password has a line end after it, as a pasted secret often has.
            ("api.anthropic.com", "kim") => Ok(format!("  {KEYCHAIN_VALUE}\n")),
            ("empty.example", "kim") => Ok(" \n".into()),
            _ => Err("the login Keychain has no item with this service and account".into()),
        }
    }
}

/// Fills the Keychain fields, which must be open, and presses the read button.
fn read_keychain(window: &mut Window, cx: &mut App, service: &str, account: &str) {
    fill(window, cx, "keychain-service", service);
    fill(window, cx, "keychain-account", account);
    window.click("read-keychain", cx);
}

fn keychain_message(cx: &mut TestAppContext, window: gpui_kit::AnyWindowHandle) -> String {
    on_screen(cx, window, |window, _| label(window, "keychain-message"))
}

#[gpui_kit::test]
fn a_token_is_copied_from_a_keychain_item_and_goes_to_the_server_only_by_save(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    // The fields are hidden until the owner asks for them.
    on_screen(cx, window, |window, cx| {
        assert!(window.try_find("keychain-service").is_none());
        window.click("open-keychain", cx);
    });
    // A window that was given no Keychain says so and reads nothing.
    on_screen(cx, window, |window, cx| {
        read_keychain(window, cx, "api.anthropic.com", "kim")
    });
    assert_eq!(
        keychain_message(cx, window),
        "The token was not read: this window cannot read the Keychain."
    );

    let keychain = std::sync::Arc::new(FakeKeychain::default());
    shell.update(cx, |shell, _| shell.set_keychain(keychain.clone()));
    let lookups = || *keychain.0.lock().unwrap();
    // Without a service or an account there is no lookup.
    on_screen(cx, window, |window, cx| {
        read_keychain(window, cx, "api.anthropic.com", "  ")
    });
    assert_eq!(
        keychain_message(cx, window),
        "Enter the service (Where) and the account of the Keychain item."
    );
    assert_eq!(lookups(), 0);
    on_screen(cx, window, |window, cx| {
        read_keychain(window, cx, "api.anthropic.com", "lee")
    });
    assert_eq!(
        keychain_message(cx, window),
        "The token was not read: the login Keychain has no item with this service and account."
    );
    on_screen(cx, window, |window, cx| {
        read_keychain(window, cx, "empty.example", "kim")
    });
    assert_eq!(
        keychain_message(cx, window),
        "The password of the Keychain item is empty."
    );
    // The names are used without the spaces around them.
    on_screen(cx, window, |window, cx| {
        read_keychain(window, cx, " api.anthropic.com ", "kim")
    });
    assert_eq!(lookups(), 3);
    // A refresh keeps the fields and the token that was read.
    std::thread::sleep(REFRESH * 4);
    cx.run_until_parked();
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "keychain-message"), KEYCHAIN_READ);
        assert_eq!(value(window, "keychain-account"), "kim");
        // The token is in the masked field only.
        assert_eq!(window.find("llm-token").role(), Some(gpui_kit::Role::PasswordInput));
        assert_not_on_screen(window, KEYCHAIN_VALUE, ANTHROPIC_BASE_URL);
    });
    // Nothing went to the server yet.
    assert!(rt.block_on(rig.engine.connections("kim")).unwrap().llm.is_none());

    on_screen(cx, window, |window, cx| window.click("save-llm", cx));
    wait_for_commands(cx, &shell, 1);
    let saved = rt.block_on(rig.engine.connections("kim")).unwrap().llm.unwrap();
    assert_eq!(saved.token, TokenSource::Stored);
    let state = shell.read_with(cx, |s, _| format!("{:?}", s.state));
    assert!(!state.contains(KEYCHAIN_VALUE), "{state}");
    on_screen(cx, window, |window, _| {
        // A successful save empties and hides the Keychain fields.
        assert!(window.try_find("keychain-service").is_none());
        assert!(window.try_find("keychain-message").is_none());
        assert_not_on_screen(window, KEYCHAIN_VALUE, ANTHROPIC_BASE_URL);
    });
}

#[gpui_kit::test]
fn the_keychain_token_is_sent_as_it_is_and_never_for_another_profile(cx: &mut TestAppContext) {
    use bunsin_api::Connections;
    use bunsin_app::bridge::{UiCommand, UiState};
    use gpui_kit::AppContext;
    // The test is the bridge: it sends the states, and it answers no command.
    let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (states, receiver) = tokio::sync::mpsc::unbounded_channel();
    cx.update(gpui_kit::init);
    let (window, _shell) = cx.update(|cx| {
        gpui_kit::open_window(Default::default(), cx, |window, cx| {
            cx.new(|cx| {
                let mut shell = Shell::new(receiver, commands, window, cx);
                shell.set_keychain(std::sync::Arc::new(FakeKeychain::default()));
                shell
            })
        })
        .unwrap()
    });
    let profile = |id: &str| UiState {
        selected: Some(id.into()),
        connections: Some(Connections {
            llm: None,
            connectors: Default::default(),
        }),
        connected: true,
        ..UiState::default()
    };
    let mut last_save = |cx: &mut TestAppContext| {
        cx.run_until_parked();
        let sent: Vec<_> = std::iter::from_fn(|| received.try_recv().ok()).collect();
        match sent.last() {
            Some(UiCommand::SaveLlm { profile, update }) => (profile.clone(), update.token.clone()),
            other => panic!("expected a save of the endpoint, got {other:?}"),
        }
    };
    states.send(profile("kim")).unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| window.click("tab-settings", cx));
    on_screen(cx, window, |window, cx| window.click("open-keychain", cx));
    on_screen(cx, window, |window, cx| {
        read_keychain(window, cx, "api.anthropic.com", "kim");
        window.click("save-llm", cx);
    });
    // The token goes without the spaces and the line end around it.
    assert_eq!(
        last_save(cx),
        ("kim".to_owned(), TokenChange::Set(KEYCHAIN_VALUE.into()))
    );

    // A read that fails after a good one empties the token field: Save must not send the earlier token.
    // The scripted bridge answered no save, so the fields are still open.
    on_screen(cx, window, |window, cx| {
        read_keychain(window, cx, "api.anthropic.com", "kim")
    });
    assert_eq!(keychain_message(cx, window), KEYCHAIN_READ);
    on_screen(cx, window, |window, cx| {
        read_keychain(window, cx, "api.anthropic.com", "nobody");
        window.click("save-llm", cx);
    });
    assert_eq!(last_save(cx), ("kim".to_owned(), TokenChange::Keep));

    // A token that was read for one profile is dropped when another profile is selected.
    on_screen(cx, window, |window, cx| {
        read_keychain(window, cx, "api.anthropic.com", "kim")
    });
    assert_eq!(keychain_message(cx, window), KEYCHAIN_READ);
    states.send(profile("lee")).unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| {
        assert!(window.try_find("keychain-service").is_none());
        window.click("open-keychain", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "keychain-service"), "");
        assert_eq!(label(window, "keychain-message"), "");
        window.click("save-llm", cx);
    });
    assert_eq!(last_save(cx), ("lee".to_owned(), TokenChange::Keep));
}

const SECRET_VALUE: &str = "sv-secret-VALUE-63e0";

/// Types a secret variable into its fields. Does not press Add.
fn type_secret(window: &mut Window, cx: &mut App, name: &str, value: &str) {
    fill(window, cx, "secret-name", name);
    fill(window, cx, "secret-value", value);
}

fn secret_names(rt: &tokio::runtime::Runtime, rig: &common::Rig) -> Vec<String> {
    rt.block_on(rig.engine.connections("kim")).unwrap().connectors["local"]
        .secret_env
        .clone()
}

#[gpui_kit::test]
fn a_secret_variable_is_added_masked_saved_kept_and_removed(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    choose_command(cx, window);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "local");
        fill(window, cx, "connector-command", "/opt/mcp/does-not-exist");
        // Add needs a name and a value.
        fill(window, cx, "secret-name", "API_TOKEN");
        window.click("add-secret", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "connector-message"),
            "Enter the name and the value of the secret variable."
        );
        assert!(window.try_find("secret-API_TOKEN").is_none());
        type_secret(window, cx, "API_TOKEN", SECRET_VALUE);
        // The value sits in a masked field that reports no value.
        assert_eq!(window.find("secret-value").role(), Some(gpui_kit::Role::PasswordInput));
        window.click("add-secret", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "secret-API_TOKEN"), "API_TOKEN: new value, not saved yet");
        assert_eq!(
            label(window, "connector-counts"),
            "0 arguments, 0 environment variables, 1 secret variable"
        );
        assert_eq!(value(window, "secret-name"), "");
        assert_not_on_screen(window, SECRET_VALUE, "API_TOKEN: new value");
        // A typed variable that Add did not take is not saved without a word.
        type_secret(window, cx, "SECOND", "second-value-19");
        window.click("save-connector", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "connector-message"),
            "Add the secret variable or clear its fields first."
        );
        window.click("add-secret", cx);
    });
    std::thread::sleep(REFRESH * 2);
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |s, _| s.state.connection_commands), 0);

    on_screen(cx, window, |window, cx| window.click("save-connector", cx));
    wait_for_commands(cx, &shell, 1);
    assert_eq!(secret_names(&rt, &rig), ["API_TOKEN", "SECOND"]);
    // The values are in no state of the window and in no answer of the server.
    let state = shell.read_with(cx, |s, _| format!("{:?}", s.state));
    assert!(state.contains("API_TOKEN") && !state.contains(SECRET_VALUE) && !state.contains("second-value-19"));
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            row(window, "local", "command-counts").as_deref(),
            Some("0 arguments, 0 environment variables, 2 secret variables")
        );
        assert_not_on_screen(window, SECRET_VALUE, "runs the command");
        click_row(window, cx, "local", "edit");
    });
    // Edit shows the names. A save of another change keeps the stored values.
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "secret-API_TOKEN"), "API_TOKEN: stored on the server");
        assert_eq!(label(window, "secret-SECOND"), "SECOND: stored on the server");
        assert_not_on_screen(window, SECRET_VALUE, "stored on the server");
        fill_area(window, cx, "connector-args", "--stdio");
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 2);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "connector-result"), "Saved: local")
    });
    assert_eq!(secret_names(&rt, &rig), ["API_TOKEN", "SECOND"]);

    // A removed row removes the variable on the server with the next save.
    on_screen(cx, window, |window, cx| click_row(window, cx, "local", "edit"));
    on_screen(cx, window, |window, cx| window.click("remove-secret-SECOND", cx));
    on_screen(cx, window, |window, cx| {
        assert!(window.try_find("secret-SECOND").is_none());
        window.click("save-connector", cx);
    });
    wait_for_commands(cx, &shell, 3);
    assert_eq!(secret_names(&rt, &rig), ["API_TOKEN"]);
}

#[gpui_kit::test]
fn a_secret_draft_follows_the_draft_rules_and_the_server_reason_is_shown(cx: &mut TestAppContext) {
    use bunsin_api::{CommandConnectorUpdate, SecretChange};
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let stored = |arg: &str, token: SecretChange| CommandConnectorUpdate {
        command: "/opt/mcp/does-not-exist".into(),
        args: vec![arg.into()],
        env: [("REGION".to_owned(), "eu".to_owned())].into(),
        secret_env: [("API_TOKEN".to_owned(), token)].into(),
        expected: Expected::Unchecked,
    };
    let first = stored("--first", SecretChange::Set(SECRET_VALUE.into()));
    rt.block_on(rig.engine.set_command_connector("kim", "local", first))
        .unwrap();
    let (window, shell) = open_settings(cx, &rt, &rig);
    let server_arg = |cx: &mut TestAppContext, shell: &Entity<Shell>, arg: &'static str| {
        wait_until(cx, shell, "the outside change", move |s| {
            s.state.connections.as_ref().is_some_and(
                |c| matches!(&c.connectors["local"].mcp, McpConfig::Stdio { args, .. } if args == &[arg.to_owned()]),
            )
        });
    };
    on_screen(cx, window, |window, cx| click_row(window, cx, "local", "edit"));
    // An unchanged draft follows the server.
    let second = stored("--second", SecretChange::Keep);
    rt.block_on(rig.engine.set_command_connector("kim", "local", second))
        .unwrap();
    server_arg(cx, &shell, "--second");
    on_screen(cx, window, |window, cx| {
        assert_eq!(area(window, "connector-args"), "--second");
        // A typed secret value makes the draft a changed one, before Add.
        type_secret(window, cx, "OTHER", "typed-value-55");
    });
    let third = stored("--third", SecretChange::Keep);
    rt.block_on(rig.engine.set_command_connector("kim", "local", third))
        .unwrap();
    server_arg(cx, &shell, "--third");
    on_screen(cx, window, |window, cx| {
        assert_eq!(area(window, "connector-args"), "--second");
        assert_eq!(value(window, "secret-name"), "OTHER");
        // The same name as a plain variable: the server refuses it, and the draft stays.
        type_secret(window, cx, "REGION", "typed-value-55");
        window.click("add-secret", cx);
    });
    on_screen(cx, window, |window, cx| window.click("save-connector", cx));
    wait_for_commands(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        // The draft started from an older state, so the conflict comes first.
        assert_eq!(label(window, "connector-conflict"), CONFLICT);
        window.click("connector-save-anyway", cx);
    });
    wait_for_commands(cx, &shell, 2);
    on_screen(cx, window, |window, cx| {
        let result = label(window, "connector-result");
        assert!(result.contains("in both lists"), "{result}");
        assert!(!result.contains("typed-value-55"), "{result}");
        assert_eq!(label(window, "secret-REGION"), "REGION: new value, not saved yet");
        // Another profile gets no row and no typed value.
        type_secret(window, cx, "THIRD", "typed-value-55");
        window.click("profile-lee", cx);
    });
    wait_until(cx, &shell, "the other profile", |s| {
        s.state.selected.as_deref() == Some("lee") && s.state.connections.is_some()
    });
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("secret-REGION").is_none());
        assert!(window.try_find("secret-API_TOKEN").is_none());
        assert_eq!(value(window, "secret-name"), "");
        assert_not_on_screen(window, "typed-value-55", "Lee (lee)");
    });
    // The stored variable of the first profile is as it was.
    assert_eq!(secret_names(&rt, &rig), ["API_TOKEN"]);
}

#[gpui_kit::test]
fn the_secret_value_goes_to_the_server_as_it_was_typed_and_is_not_printed(cx: &mut TestAppContext) {
    use bunsin_api::{Connections, SecretChange};
    use bunsin_app::bridge::{UiCommand, UiState};
    use gpui_kit::AppContext;
    // The test is the bridge: it sends the states, and it answers no command.
    let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (states, receiver) = tokio::sync::mpsc::unbounded_channel();
    cx.update(gpui_kit::init);
    let (window, _shell) = cx.update(|cx| {
        // The page scrolls. The window holds all of it, because a test cannot click below the fold.
        let options = gpui_kit::WindowOptions {
            window_bounds: Some(gpui_kit::WindowBounds::Windowed(gpui_kit::Bounds {
                origin: Default::default(),
                size: gpui_kit::size(gpui_kit::px(1280.), gpui_kit::px(2100.)),
            })),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Shell::new(receiver, commands, window, cx))
        })
        .unwrap()
    });
    let state = UiState {
        selected: Some("kim".into()),
        connections: Some(Connections {
            llm: None,
            connectors: Default::default(),
        }),
        connected: true,
        ..UiState::default()
    };
    states.send(state).unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| window.click("tab-settings", cx));
    choose_command(cx, window);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "local");
        fill(window, cx, "connector-command", "/opt/mcp/does-not-exist");
        // A space inside the value is part of it.
        type_secret(window, cx, "API_TOKEN", "two words");
        window.click("add-secret", cx);
    });
    on_screen(cx, window, |window, cx| window.click("save-connector", cx));
    cx.run_until_parked();
    let sent: Vec<_> = std::iter::from_fn(|| received.try_recv().ok()).collect();
    let Some(UiCommand::SaveCommandConnector { update, .. }) = sent.last() else {
        panic!("expected a save of the command, got {:?}", sent.last());
    };
    let expected = [("API_TOKEN".to_owned(), SecretChange::Set("two words".to_owned()))].into();
    assert_eq!(update.secret_env, expected);
    // The command can be logged: its debug text holds the name and not the value.
    let printed = format!("{:?}", sent.last());
    assert!(
        printed.contains("API_TOKEN") && !printed.contains("two words"),
        "{printed}"
    );
}

#[gpui_kit::test]
fn a_secret_variable_without_a_value_is_not_shown_as_stored(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    // The settings file names a secret variable. It cannot give the value.
    let section = ConnectorSection {
        mcp: McpConfig::Stdio {
            command: "/opt/mcp/does-not-exist".into(),
            args: vec![],
            env: Default::default(),
        },
        token_secret: None,
        token_header: None,
        secret_env: ["API_TOKEN".to_owned()].into(),
    };
    let mut config = rt.block_on(rig.engine.profile_config("kim")).unwrap();
    config
        .connectors
        .insert("local".into(), serde_json::to_value(&section).unwrap());
    rt.block_on(rig.engine.set_profile_config("kim", config)).unwrap();
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| click_row(window, cx, "local", "edit"));
    on_screen(cx, window, |window, cx| {
        // No row says "stored" for it. The form says what is missing.
        assert!(window.try_find("secret-API_TOKEN").is_none());
        assert_eq!(
            label(window, "secret-missing"),
            "No value is stored for: API_TOKEN. Add each with its value. A save without it removes the variable."
        );
        type_secret(window, cx, "API_TOKEN", SECRET_VALUE);
        window.click("add-secret", cx);
    });
    on_screen(cx, window, |window, cx| window.click("save-connector", cx));
    wait_for_commands(cx, &shell, 1);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "connector-result"), "Saved: local")
    });
    let stored = rt.block_on(rig.engine.connections("kim")).unwrap().connectors["local"].clone();
    assert_eq!(stored.secret_env, ["API_TOKEN"]);
    assert!(stored.secret_env_missing.is_empty());
    on_screen(cx, window, |window, cx| click_row(window, cx, "local", "edit"));
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "secret-API_TOKEN"), "API_TOKEN: stored on the server");
        assert_eq!(label(window, "secret-missing"), "");
    });
}
