//! Headless window tests of the publishing cards of the agent view: the inbound endpoint with its
//! ARD listing, the webhooks and the ARD registries. Real view, real engine, real settings rules.

mod common;

use bunsin_api::{ApiConnectorUpdate, EngineApi, Expected, InboundUpdate, TokenChange};
use bunsin_app::shell::Shell;
use bunsin_core::inbound::{InboundScope, REPLY_TOOL};
use bunsin_core::settings::{ApiAuthMethod, ApiConfig, ApiService};
use bunsin_engine::OuterListener;
use common::window::{label, on_screen, open_sized, runtime, screen_texts, value, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, App, Entity, TestAppContext, Window};

/// The agent view scrolls. The window holds the whole page, because a headless test cannot click
/// a control that is scrolled away.
const TALL: (f32, f32) = (1280., 3200.);
const SECRET: &str = "slack-signing-secret-window-4d1e";

fn open_agent(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
) -> (AnyWindowHandle, Entity<Shell>) {
    let (window, shell) = open_sized(cx, rt, rig, TALL);
    on_screen(cx, window, |window, cx| window.click("tab-agent", cx));
    wait_until(cx, &shell, "the inbound settings", |s| {
        s.state.inbound.is_some() && s.state.inbound_status.is_some()
    });
    (window, shell)
}

fn retype(window: &mut Window, cx: &mut App, id: &'static str, text: &str) {
    window.click(id, cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

fn wait_for_saves(cx: &mut TestAppContext, shell: &Entity<Shell>, n: u64) {
    wait_until(cx, shell, "the save result", |s| s.state.part_saves == n);
}

#[gpui_kit::test]
fn the_owner_turns_the_endpoint_on_lists_it_and_sees_why_a_catalog_leaves_it_out(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_agent(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "inbound-state"), "Endpoint: off");
        assert!(label(window, "inbound-help").contains("Every answer waits for your approval"));
        // The listing fields are shown only while the listing is on.
        assert!(window.try_find("ard-publisher").is_none());
        window.click("inbound-on", cx);
        retype(
            window,
            cx,
            "inbound-description",
            "Questions about the deploy schedule.",
        );
        window.click("ard-on", cx);
    });
    on_screen(cx, window, |window, cx| {
        retype(window, cx, "ard-publisher", common::PUBLISHER);
        retype(window, cx, "ard-agent", "kim");
        window.within("ard-queries").click("input-state", cx);
        window.input("When is the next deploy?\nWho approves a release?", cx);
        assert_eq!(label(window, "inbound-changes"), "Not saved");
        window.click("save-inbound", cx);
    });
    wait_for_saves(cx, &shell, 1);
    let stored = rt.block_on(rig.engine.inbound_settings("kim")).unwrap();
    assert!(stored.enabled);
    assert_eq!(stored.scope, InboundScope::Internal);
    assert_eq!(stored.title, None);
    let ard = stored.ard.clone().unwrap();
    assert_eq!((ard.publisher.as_str(), ard.agent_name.as_str()), ("acme.com", "kim"));
    assert_eq!(
        ard.representative_queries,
        ["When is the next deploy?", "Who approves a release?"]
    );
    // Turning the endpoint on added the reply rule, as a save of the settings file does.
    let rules = rt.block_on(rig.engine.permissions("kim")).unwrap().rules;
    assert!(rules.contains_key(REPLY_TOOL));
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "inbound-result"), "Saved");
        assert_eq!(label(window, "inbound-changes"), "");
        assert_eq!(label(window, "inbound-state"), "Endpoint: on, internal listener only");
        assert_eq!(label(window, "ard-identifier"), "urn:air:acme.com:kim");
        // No listener for callers runs, so the catalogs leave the entry out, and the window says why.
        assert_eq!(label(window, "ard-listed"), "ARD listing: not in any catalog");
        assert_eq!(
            label(window, "ard-refused-0"),
            "Left out: no listener for callers serves the endpoint of this profile"
        );
    });

    // The server starts its internal listener. The next refresh shows the addresses and the listing.
    rig.engine.add_outer_listener(OuterListener {
        scope: InboundScope::Internal,
        url: "https://agents.acme.com".into(),
    });
    wait_until(cx, &shell, "the listing", |s| {
        s.state
            .inbound_status
            .as_ref()
            .is_some_and(|status| !status.endpoints.is_empty())
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "inbound-endpoint-0"),
            "internal listener\nMCP: https://agents.acme.com/agents/kim/mcp\n\
             Server card: https://agents.acme.com/agents/kim/mcp/server-card\n\
             ARD catalog: https://agents.acme.com/.well-known/ard.json"
        );
        assert_eq!(
            label(window, "ard-listed"),
            "ARD listing: in the catalog of the internal listener"
        );
        assert!(window.try_find("ard-refused-0").is_none());
    });

    // A publisher domain that the operator does not allow is refused, and the form keeps the change.
    on_screen(cx, window, |window, cx| {
        retype(window, cx, "ard-publisher", "other.example");
        window.click("save-inbound", cx);
    });
    wait_for_saves(cx, &shell, 2);
    on_screen(cx, window, |window, _| {
        assert!(
            label(window, "inbound-result").contains("--ard-publisher-domain other.example"),
            "{}",
            label(window, "inbound-result")
        );
        assert_eq!(label(window, "inbound-changes"), "Not saved");
        assert_eq!(value(window, "ard-publisher"), "other.example");
    });
    assert_eq!(rt.block_on(rig.engine.inbound_settings("kim")).unwrap(), stored);
}

#[gpui_kit::test]
fn a_change_on_the_server_meanwhile_is_a_conflict_with_two_choices(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_agent(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("inbound-on", cx);
        retype(window, cx, "inbound-description", "Mine.");
    });
    // Someone else turns the endpoint on with another text.
    let theirs = bunsin_core::inbound::InboundConfig {
        enabled: true,
        description: "Theirs.".into(),
        ..Default::default()
    };
    let update = InboundUpdate {
        inbound: theirs.clone(),
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_inbound_settings("kim", update)).unwrap();
    on_screen(cx, window, |window, cx| window.click("save-inbound", cx));
    wait_for_saves(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        assert!(label(window, "inbound-conflict").contains("Nothing was saved"));
        // The owner's text stays until the owner chooses.
        assert_eq!(value(window, "inbound-description"), "Mine.");
        window.click("inbound-load", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert!(window.try_find("inbound-conflict").is_none());
        assert_eq!(value(window, "inbound-description"), "Theirs.");
        retype(window, cx, "inbound-description", "Mine after all.");
        window.click("save-inbound", cx);
    });
    wait_for_saves(cx, &shell, 2);
    assert_eq!(
        rt.block_on(rig.engine.inbound_settings("kim")).unwrap().description,
        "Mine after all."
    );
}

#[gpui_kit::test]
fn registries_are_added_refused_by_the_address_rules_removed_and_saved(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_agent(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "registry-count"),
            "Registries: none, so the agent has no search"
        );
        window.click("add-registry", cx);
        assert_eq!(label(window, "registries-message"), "Enter a name and an address.");
        retype(window, cx, "new-registry-name", "corp");
        retype(window, cx, "new-registry-url", "http://127.0.0.1:9/v1");
        // Text in the fields that is not added would be lost, so Save refuses it.
        window.click("save-registries", cx);
        assert_eq!(
            label(window, "registries-message"),
            "Add the registry or clear its fields first"
        );
        window.click("add-registry", cx);
        assert_eq!(label(window, "registry-0"), "corp · http://127.0.0.1:9/v1");
        window.click("save-registries", cx);
    });
    wait_for_saves(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        // The server's address rules refuse an address inside its own network.
        assert!(label(window, "registries-result").contains("ARD registry `corp`"));
        assert_eq!(label(window, "registries-changes"), "Not saved");
        window.click("remove-registry-0", cx);
        retype(window, cx, "new-registry-name", "corp");
        retype(window, cx, "new-registry-url", "https://registry.example/v1");
        window.click("add-registry", cx);
        window.click("save-registries", cx);
    });
    wait_for_saves(cx, &shell, 2);
    let stored = rt.block_on(rig.engine.ard_registries("kim")).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].url, "https://registry.example/v1");
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "registries-result"), "Saved");
        assert_eq!(label(window, "registry-count"), "Registries: 1");
    });
}

#[gpui_kit::test]
fn a_webhook_is_added_gets_its_secret_masked_and_is_removed_with_it(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let slack = ApiConnectorUpdate {
        api: ApiConfig {
            service: ApiService::Slack,
            base_url: "https://slack.com/api".into(),
            auth: ApiAuthMethod::Bearer,
            email: None,
        },
        token: TokenChange::Set("xoxb-window-test".into()),
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_api_connector("kim", "slackapi", slack))
        .unwrap();
    rig.engine.add_outer_listener(OuterListener {
        scope: InboundScope::Public,
        url: "https://hooks.acme.com".into(),
    });
    let (window, shell) = open_agent(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "webhook-count"), "Webhooks: none");
        // A connector that is not a REST API connector of Slack or Jira is refused on Save.
        retype(window, cx, "new-webhook-name", "other");
        retype(window, cx, "new-webhook-connector", "slack");
        window.click("add-webhook", cx);
        retype(window, cx, "new-webhook-name", "slack-events");
        retype(window, cx, "new-webhook-connector", "slackapi");
        retype(window, cx, "new-webhook-skip", "U-KIM, U-KIM-BOT");
        window.click("new-webhook-public", cx);
        window.click("add-webhook", cx);
        assert_eq!(
            label(window, "webhook-slack-events"),
            "slack-events → slackapi · public · skips U-KIM, U-KIM-BOT"
        );
        assert_eq!(label(window, "webhook-state-slack-events"), "Not saved yet");
        // A secret can be stored only for a webhook that the server has.
        assert!(window.try_find("webhook-secret-slack-events").is_none());
        window.click("save-webhooks", cx);
    });
    wait_for_saves(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        assert!(label(window, "webhooks-result").contains("needs a REST API connector of Slack or Jira"));
        window.click("remove-webhook-other", cx);
        window.click("save-webhooks", cx);
    });
    wait_for_saves(cx, &shell, 2);
    let stored = rt.block_on(rig.engine.webhooks("kim")).unwrap();
    assert_eq!(stored.keys().collect::<Vec<_>>(), ["slack-events"]);
    assert_eq!(stored["slack-events"].skip_authors, ["U-KIM", "U-KIM-BOT"]);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "webhooks-result"), "Saved");
        assert_eq!(
            label(window, "webhook-state-slack-events"),
            "No signing secret: every delivery is refused"
        );
        assert_eq!(
            label(window, "webhook-urls-slack-events"),
            "https://hooks.acme.com/hooks/kim/slack-events"
        );
        window.click("webhook-secret-slack-events", cx);
    });
    on_screen(cx, window, |window, cx| {
        window.click("webhook-secret", cx);
        window.input(SECRET, cx);
        // The secret sits in a masked field that reports no value, and in no label.
        assert_eq!(
            window.find("webhook-secret").role(),
            Some(gpui_kit::Role::PasswordInput)
        );
        assert!(!screen_texts(window).iter().any(|text| text.contains(SECRET)));
        window.click("store-webhook-secret", cx);
    });
    wait_for_saves(cx, &shell, 3);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "webhook-state-slack-events"), "Signing secret stored");
        assert_eq!(label(window, "webhook-secret-result"), "Signing secret stored");
        // The field closed after the store.
        assert!(window.try_find("webhook-secret").is_none());
        assert!(!screen_texts(window).iter().any(|text| text.contains(SECRET)));
    });
    // The window state never holds the secret.
    let state = shell.read_with(cx, |s, _| format!("{:?}", s.state));
    assert!(!state.contains(SECRET));

    // Removed, the webhook takes its secret with it: added back from the settings, it has none.
    on_screen(cx, window, |window, cx| {
        window.click("remove-webhook-slack-events", cx);
        window.click("save-webhooks", cx);
    });
    wait_for_saves(cx, &shell, 4);
    assert!(rt.block_on(rig.engine.webhooks("kim")).unwrap().is_empty());
    let update = bunsin_api::WebhooksUpdate {
        webhooks: stored,
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_webhooks("kim", update)).unwrap();
    let status = rt.block_on(rig.engine.inbound_status("kim")).unwrap();
    assert!(!status.webhooks[0].secret_stored);
}
