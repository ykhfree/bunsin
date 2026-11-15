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

#[gpui_kit::test]
fn a_stdio_connector_is_listed_read_only(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let stdio = McpConfig::Stdio {
        command: "/usr/local/bin/local-mcp".into(),
        args: vec!["--secret-arg".into()],
        env: Default::default(),
    };
    let section = ConnectorSection {
        mcp: stdio.clone(),
        token_secret: None,
        token_header: None,
    };
    let mut config = rt.block_on(rig.engine.profile_config("kim")).unwrap();
    config
        .connectors
        .insert("local".into(), serde_json::to_value(&section).unwrap());
    rt.block_on(rig.engine.set_profile_config("kim", config)).unwrap();
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            row(window, "local", "address").as_deref(),
            Some("runs as a command on the server; set by the server operator")
        );
        assert_eq!(row(window, "local", "edit"), None);
        assert_eq!(row(window, "local", "remove"), None);
        assert_not_on_screen(window, "--secret-arg", "set by the server operator");
        // The form cannot turn it into an HTTP connector.
        fill(window, cx, "connector-name", "local");
        fill(window, cx, "connector-address", "https://mcp.example/local");
        window.click("save-connector", cx);
        assert_eq!(
            label(window, "connector-message"),
            "This connector runs as a command on the server. Only the server operator can change it."
        );
        assert_eq!(value(window, "connector-address"), "https://mcp.example/local");
    });
    std::thread::sleep(REFRESH * 2);
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |s, _| s.state.connection_commands), 0);
    let connectors = rt.block_on(rig.engine.connections("kim")).unwrap().connectors;
    assert_eq!(connectors["local"].mcp, stdio);
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
