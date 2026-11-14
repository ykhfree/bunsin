//! A save that names what its form loaded is refused if the server holds something else.

mod common;

use bunsin_api::{ApiError, ConnectorUpdate, EngineApi, Expected, LlmUpdate, PrivacyUpdate, TokenChange};
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use bunsin_core::settings::Wire;
use common::*;

fn pattern(label: &str) -> CustomPattern {
    CustomPattern {
        label: label.into(),
        pattern: "abc".into(),
    }
}

fn llm(model: &str, expected: Expected<Option<bunsin_api::LlmConnection>>) -> LlmUpdate {
    LlmUpdate {
        wire: Wire::AnthropicMessages,
        base_url: "https://llm.example".into(),
        model: model.into(),
        timeout_secs: 120,
        token: TokenChange::Keep,
        expected,
    }
}

fn connector(url: &str, expected: Expected<Option<bunsin_api::ConnectorConnection>>) -> ConnectorUpdate {
    ConnectorUpdate {
        url: url.into(),
        token: TokenChange::Keep,
        expected,
    }
}

#[tokio::test]
async fn a_privacy_save_is_refused_when_the_server_changed_since_the_form_loaded() {
    let rig = rig().await;
    let loaded = rig.engine.privacy("kim").await.unwrap();

    // Another client saves a change.
    let mut other = loaded.clone();
    other.custom.push(pattern("from the other client"));
    rig.engine
        .set_privacy("kim", PrivacyUpdate::unchecked(other.clone()))
        .await
        .unwrap();

    // This form still holds what it loaded, and saves its own change.
    let mut mine = loaded.clone();
    mine.email = false;
    let update = PrivacyUpdate {
        privacy: mine.clone(),
        expected: Expected::Value(loaded),
    };
    let error = rig.engine.set_privacy("kim", update).await.unwrap_err();
    assert!(matches!(error, ApiError::Conflict(_)), "{error:?}");
    // The other client's change is still there.
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), other);

    // "Save anyway" overwrites.
    rig.engine
        .set_privacy("kim", PrivacyUpdate::unchecked(mine.clone()))
        .await
        .unwrap();
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), mine);

    // A save that names the current value goes through.
    let mut next = mine.clone();
    next.phone = false;
    rig.engine
        .set_privacy(
            "kim",
            PrivacyUpdate {
                privacy: next.clone(),
                expected: Expected::Value(mine),
            },
        )
        .await
        .unwrap();
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), next);
}

#[tokio::test]
async fn a_change_elsewhere_in_the_settings_is_not_a_privacy_conflict() {
    let rig = rig().await;
    let loaded = rig.engine.privacy("kim").await.unwrap();
    // Someone saves a connector. The privacy settings are the same as before.
    rig.engine
        .set_connector(
            "kim",
            "jira",
            connector("https://mcp.example/jira", Expected::Unchecked),
        )
        .await
        .unwrap();
    let mut mine = loaded.clone();
    mine.email = false;
    rig.engine
        .set_privacy(
            "kim",
            PrivacyUpdate {
                privacy: mine.clone(),
                expected: Expected::Value(loaded),
            },
        )
        .await
        .unwrap();
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), mine);
    // The connector is still there: neither save lost the other.
    assert!(
        rig.engine
            .connections("kim")
            .await
            .unwrap()
            .connectors
            .contains_key("jira")
    );
}

#[tokio::test]
async fn an_llm_save_is_refused_when_the_endpoint_changed_since_the_form_loaded() {
    let rig = rig().await;
    // The form loaded "no LLM endpoint".
    let loaded = rig.engine.connections("kim").await.unwrap().llm;
    assert_eq!(loaded, None);
    rig.engine
        .set_llm("kim", llm("model-a", Expected::Unchecked))
        .await
        .unwrap();

    let error = rig
        .engine
        .set_llm("kim", llm("model-b", Expected::Value(loaded)))
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Conflict(_)), "{error:?}");
    let current = rig.engine.connections("kim").await.unwrap().llm;
    assert_eq!(current.as_ref().unwrap().model, "model-a");

    rig.engine
        .set_llm("kim", llm("model-b", Expected::Value(current)))
        .await
        .unwrap();
    assert_eq!(
        rig.engine.connections("kim").await.unwrap().llm.unwrap().model,
        "model-b"
    );
}

#[tokio::test]
async fn a_connector_save_is_refused_when_it_changed_or_appeared_since_the_form_loaded() {
    let rig = rig().await;
    // Two forms both start a new connector with the same name.
    rig.engine
        .set_connector(
            "kim",
            "jira",
            connector("https://first.example/jira", Expected::Value(None)),
        )
        .await
        .unwrap();
    let second = rig
        .engine
        .set_connector(
            "kim",
            "jira",
            connector("https://second.example/jira", Expected::Value(None)),
        )
        .await;
    assert!(matches!(second, Err(ApiError::Conflict(_))), "{second:?}");

    // A form that loaded the first address does not overwrite a later change.
    let loaded = rig
        .engine
        .connections("kim")
        .await
        .unwrap()
        .connectors
        .get("jira")
        .cloned();
    rig.engine
        .set_connector(
            "kim",
            "jira",
            connector("https://moved.example/jira", Expected::Unchecked),
        )
        .await
        .unwrap();
    let stale = rig
        .engine
        .set_connector(
            "kim",
            "jira",
            connector("https://mine.example/jira", Expected::Value(loaded)),
        )
        .await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
    let now = rig.engine.connections("kim").await.unwrap().connectors["jira"].clone();
    assert_eq!(
        now.mcp,
        bunsin_core::settings::McpConfig::Http {
            url: "https://moved.example/jira".into()
        }
    );

    // Another connector of the same profile is not part of the comparison.
    rig.engine
        .set_connector(
            "kim",
            "wiki",
            connector("https://mcp.example/wiki", Expected::Value(None)),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_save_without_an_expected_value_is_not_checked() {
    let rig = rig().await;
    let update: PrivacyUpdate = serde_json::from_str(
        r#"{"privacy": {"email": false, "phone": true, "resident_number": true, "card_number": true, "custom": []}}"#,
    )
    .unwrap();
    assert_eq!(update.expected, Expected::Unchecked);
    rig.engine.set_privacy("kim", update).await.unwrap();
    assert!(!rig.engine.privacy("kim").await.unwrap().email);
    let _ = PrivacyConfig::default();
}
