//! The test of the saved LLM endpoint: one request without data of the profile.

mod common;

use bunsin_api::EngineApi;
use bunsin_core::llm::{CompletionResponse, LlmError, StopReason, Usage};
use bunsin_core::proposal::proposal_schema;
use common::*;

fn response(content: &str, stop: StopReason) -> Result<CompletionResponse, LlmError> {
    Ok(CompletionResponse {
        content: content.into(),
        stop,
        usage: Usage {
            input_tokens: 40,
            output_tokens: 9,
        },
    })
}

#[tokio::test]
async fn a_reply_in_the_form_of_the_agent_passes_and_nothing_else_changes() {
    let rig = rig().await;
    let before = rig.engine.snapshot("kim").await.unwrap();
    // The test runs with the kill switch on: it changes nothing outside the server.
    rig.engine.set_kill_switch("kim", true).await.unwrap();
    rig.llm.push(response(&ignore(), StopReason::End));
    let test = rig.engine.test_llm("kim").await.unwrap();
    assert!(test.works, "{test:?}");
    assert_eq!(
        test.detail,
        "the endpoint answered in the form the agent needs (40 input tokens, 9 output tokens)"
    );

    // The request has the schema and the output limit of a real request, and no data of the profile.
    let requests = rig.llm.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.response_schema, Some(proposal_schema()));
    assert_eq!(request.max_output_tokens, config().max_output_tokens);
    let sent = format!("{} {:?}", request.system, request.messages);
    let config = config();
    assert!(!sent.contains(&config.owner_name), "{sent}");
    for principle in &config.principles {
        assert!(!sent.contains(principle.as_str()), "{sent}");
    }

    let after = rig.engine.snapshot("kim").await.unwrap();
    assert_eq!(after.inbox, before.inbox);
    assert!(rig.slack.calls().is_empty());
    let entry = &after.audit[0];
    assert_eq!(
        (entry.kind.as_str(), &entry.detail),
        ("llm_tested", &serde_json::json!({"works": true}))
    );
}

#[tokio::test]
async fn each_failure_is_reported_with_its_reason() {
    let rig = rig().await;
    let cases = [
        (
            response("Hello! How can I help?", StopReason::End),
            "the endpoint answered, but not in the form the agent needs: ",
        ),
        (
            response("{\"kind\":", StopReason::MaxTokens),
            "the endpoint answered, but the reply was cut off at the output limit of ",
        ),
        (
            response("", StopReason::Refusal),
            "the endpoint answered, but it refused the request",
        ),
        (
            Err(LlmError::Http {
                status: 401,
                body: "invalid x-api-key".into(),
            }),
            "the request failed: the endpoint returned HTTP 401: invalid x-api-key",
        ),
        (
            Err(LlmError::Transport("operation timed out".into())),
            "the request failed: transport failure: operation timed out",
        ),
    ];
    for (reply, expected) in cases {
        rig.llm.push(reply);
        let test = rig.engine.test_llm("kim").await.unwrap();
        assert!(!test.works, "{test:?}");
        assert!(test.detail.starts_with(expected), "{}", test.detail);
        let audit = rig.engine.snapshot("kim").await.unwrap().audit;
        assert_eq!(audit[0].detail, serde_json::json!({"works": false}));
    }
    // A failed test is not retried: one request for each test.
    assert_eq!(rig.llm.requests().len(), 5);
}

#[tokio::test]
async fn a_profile_that_does_not_exist_is_an_error() {
    let rig = rig().await;
    assert!(rig.engine.test_llm("nobody").await.is_err());
    assert!(rig.llm.requests().is_empty());
}
