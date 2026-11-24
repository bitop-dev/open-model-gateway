//! Messages option contracts through HTTP and real admission, with the Bedrock
//! SDK pointed at a loopback listener.
use super::*;
use crate::{
    governance::tests::db::fixture,
    inference::admission_validation_tests::{
        add_unpriced_deployment, issue_key, snapshot, start_gateway,
    },
    providers::ProviderRegistry,
};
use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

async fn route(
    pool: PgPool,
    mock: &Mock,
) -> (
    String,
    String,
    crate::inference::admission_validation_tests::Server,
) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['messages'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let deployment = add_unpriced_deployment(
        &f,
        "bedrock",
        "aws:default",
        Some("us-east-1"),
        "anthropic.claude-test",
        0,
    )
    .await;
    sqlx::query(
        "INSERT INTO deployment_prices(
            id,deployment_id,input_microusd_per_million,output_microusd_per_million,
            input_token_limit,output_token_limit,pricing_version
         ) VALUES($1,$2,1000000,1000000,4096,4096,1)",
    )
    .bind(Uuid::new_v4())
    .bind(deployment)
    .execute(&f.store.pool)
    .await
    .unwrap();
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(mock.adapter())).unwrap();
    let (base, gateway) = start_gateway(&f, registry).await;
    (base, token, gateway)
}

async fn post(base: &str, token: &str, stream: bool, extra: Value) -> reqwest::Response {
    let mut body = json!({
        "model": "company/smart",
        "max_tokens": 2048,
        "messages": [{"role":"user","content":"hello"}],
        "stream": stream,
    });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", token)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn assert_refused(pool: PgPool, cases: &[(&str, Value)]) {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let (base, token, _gateway) = route(pool.clone(), &mock).await;
    let before = snapshot(&pool).await;
    for (field, extra) in cases {
        for stream in [false, true] {
            let response = post(&base, &token, stream, extra.clone()).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{field}, stream={stream}"
            );
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["type"], "error");
            assert_eq!(body["error"]["type"], "invalid_request_error");
            assert_eq!(
                body["error"]["message"],
                format!("Unsupported or invalid field: {field}"),
            );
            assert!(
                mock.requests.lock().unwrap().is_empty(),
                "{field}, stream={stream}"
            );
            assert_eq!(snapshot(&pool).await, before, "{field}, stream={stream}");
        }
    }
}

#[dbtest::test]
async fn rejects_current_thinking_controls(pool: PgPool) {
    assert_refused(
        pool,
        &[
            (
                "thinking",
                json!({"thinking":{"type":"adaptive","display":"omitted"}}),
            ),
            (
                "thinking",
                json!({"thinking":{"type":"enabled","budget_tokens":1024}}),
            ),
            ("thinking", json!({"thinking":{"type":"disabled"}})),
        ],
    )
    .await;
}

#[dbtest::test]
async fn rejects_thinking_history(pool: PgPool) {
    let cases = [
        json!({"type":"thinking","thinking":"reasoning","signature":"opaque"}),
        json!({"type":"redacted_thinking","data":"opaque"}),
    ]
    .map(|block| {
        (
            "thinking",
            json!({"messages":[
                {"role":"user","content":"initial"},
                {"role":"assistant","content":[block, {"type":"text","text":"answer"}]},
                {"role":"user","content":"continue"},
            ]}),
        )
    });
    assert_refused(pool, &cases).await;
}

#[dbtest::test]
async fn rejects_cache_controls(pool: PgPool) {
    assert_refused(
        pool,
        &[
            ("cache_control", json!({"system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}]})),
            ("cache_control", json!({"messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]}]})),
            ("cache_control", json!({"tools":[{"name":"weather","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral","ttl":"5m"}}]})),
        ],
    )
    .await;
}

#[dbtest::test]
async fn rejects_context_management(pool: PgPool) {
    assert_refused(
        pool,
        &[(
            "context_management",
            json!({"context_management":{"edits":[{"type":"clear_tool_uses_20250919"}]}}),
        )],
    )
    .await;
}

#[dbtest::test]
async fn rejects_hosted_tools_and_server_tool_history(pool: PgPool) {
    assert_refused(
        pool,
        &[
            ("tools.type", json!({"tools":[{"type":"web_search_20250305","name":"web_search"}]})),
            ("tools.type", json!({"messages":[{"role":"assistant","content":[{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"weather"}}]}]})),
        ],
    )
    .await;
}

#[dbtest::test]
async fn rejects_deferred_tool_loading(pool: PgPool) {
    let cases = [true, false].map(|defer| (
        "defer_loading",
        json!({"tools":[{"type":"custom","name":"weather","input_schema":{"type":"object"},"defer_loading":defer}]}),
    ));
    assert_refused(pool, &cases).await;
}

#[dbtest::test]
async fn preserves_converse_sampling_stops_and_custom_tools(pool: PgPool) {
    let mock = Mock::serve(complete(true), wire(events(true)), 200).await;
    let (base, token, _gateway) = route(pool, &mock).await;
    let extra = json!({
        "system":[{"type":"text","text":"instructions"}],
        "temperature":0.25,
        "top_p":0.5,
        "stop_sequences":["end"],
        "tools":[{"type":"custom","name":"weather","description":"Get weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}],
        "tool_choice":{"type":"tool","name":"weather"},
    });
    for stream in [false, true] {
        let response = post(&base, &token, stream, extra.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        if stream {
            let body = response.text().await.unwrap();
            assert!(body.contains("event: message_stop"), "{body}");
        } else {
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["stop_reason"], "tool_use");
            assert_eq!(body["content"][1]["name"], "weather");
        }
    }
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for (request, operation) in requests.iter().zip(["/converse", "/converse-stream"]) {
        assert!(request.uri.path().ends_with(operation));
        assert_eq!(
            request.body["inferenceConfig"],
            json!({"temperature":0.25,"topP":0.5,"stopSequences":["end"],"maxTokens":2048}),
        );
        assert_eq!(request.body["system"], json!([{"text":"instructions"}]));
        assert_eq!(
            request.body["messages"],
            json!([{"role":"user","content":[{"text":"hello"}]}])
        );
        assert_eq!(
            request.body["toolConfig"],
            json!({
                "tools":[{"toolSpec":{"name":"weather","description":"Get weather","inputSchema":{"json":{"type":"object","properties":{"city":{"type":"string"}}}}}}],
                "toolChoice":{"tool":{"name":"weather"}},
            }),
        );
    }
}

#[dbtest::test]
async fn maps_structured_output_and_strict_tools_through_admission(pool: PgPool) {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let (base, token, _gateway) = route(pool, &mock).await;
    let schema = json!({"type":"object","properties":{"a":{"type":"string"}},"required":["a"],"additionalProperties":false});
    let extra = json!({
        "output_config":{"format":{"type":"json_schema","schema":schema}},
        "tools":[{"name":"weather","input_schema":schema,"strict":true}],
    });
    for stream in [false, true] {
        let response = post(&base, &token, stream, extra.clone()).await;
        assert_eq!(response.status(), StatusCode::OK, "stream={stream}");
        let _ = response.bytes().await.unwrap();
    }
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        let structure = &request.body["outputConfig"]["textFormat"]["structure"]["jsonSchema"];
        let sent: Value = serde_json::from_str(structure["schema"].as_str().unwrap()).unwrap();
        assert_eq!(sent, schema);
        assert_eq!(
            request.body["toolConfig"]["tools"][0]["toolSpec"]["strict"],
            true
        );
    }
}

/// A Bedrock model that reasons by default streams reasoning content, but
/// thinking cannot be requested on Bedrock: through the real router, engine
/// and Messages codec, no thinking block reaches the client.
#[dbtest::test]
async fn streamed_reasoning_never_reaches_a_messages_client(pool: PgPool) {
    use super::structured_tests::{reasoning_complete, reasoning_events};
    let mock = Mock::serve(reasoning_complete(), wire(reasoning_events()), 200).await;
    let (base, token, _gateway) = route(pool, &mock).await;
    for stream in [false, true] {
        let response = post(&base, &token, stream, json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("Hello"), "{body}");
        assert!(!body.contains("thinking"), "{body}");
        assert!(!body.contains("think first"), "{body}");
        assert!(!body.contains("opaque-signature"), "{body}");
    }
}
