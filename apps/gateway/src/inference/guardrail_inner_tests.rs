//! A JSON document held in a request's string (a tool call's `arguments`),
//! which adapters and guardrails parse, counts with its body against the
//! structure budget (#872): over it, the request is refused before any
//! parse of it, on an OpenAI, an Anthropic and a Bedrock route under a
//! blocking guardrail rule, on Chat Completions, Responses and Messages; a
//! legitimate large tool call passes the bound and is sent.
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::atomic::Ordering;

use super::admission_validation_tests::snapshot;
use super::guardrail_db_tests::{send, type_default, world_on};
use crate::metrics::METRICS;

/// One-key objects: about 18 MiB of structure as a parsed document, in a
/// string of a few hundred KiB.
fn objects(n: usize) -> Value {
    json!({ "k": vec![json!({"a": 0}); n] })
}

fn hostile() -> [(&'static str, &'static str, Value); 3] {
    let inner = objects(20_000).to_string();
    [
        (
            "/v1/chat/completions",
            "chat_completions",
            json!({"model":"company/smart","max_tokens":16,
                "tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
                "messages":[
                    {"role":"user","content":"hi"},
                    {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":inner}}]},
                    {"role":"tool","tool_call_id":"c1","content":"ok"}]}),
        ),
        (
            "/v1/responses",
            "responses",
            json!({"model":"company/smart","max_output_tokens":16,
                "tools":[{"type":"function","name":"f","parameters":{"type":"object"}}],
                "input":[
                    {"type":"function_call","call_id":"c1","name":"f","arguments":inner},
                    {"type":"function_call_output","call_id":"c1","output":"ok"}]}),
        ),
        // The tool input is part of the body (and within the budget alone);
        // the request holds it again as the call's serialized arguments.
        (
            "/v1/messages",
            "messages",
            json!({"model":"company/smart","max_tokens":16,
                "tools":[{"name":"f","input_schema":{"type":"object"}}],
                "messages":[
                    {"role":"user","content":"hi"},
                    {"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"f","input":objects(9_500)}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"ok"}]}]}),
        ),
    ]
}

async fn inner_documents_on(pool: PgPool, provider: &str) {
    // The refusal counter is the process's: one router test at a time.
    let _counters = crate::inference::image_buffer::ROUTER_TESTS.lock().await;
    let w = world_on(pool, "'chat_completions','responses','messages'", provider).await;
    type_default(&w, "block", "{secret}", &[]).await;
    let before = snapshot(&w.f.store.pool).await;
    for (path, family, body) in hostile() {
        let bytes = serde_json::to_vec(&body).unwrap();
        assert!(
            crate::inference::json_weight::estimate(&bytes)
                <= crate::inference::json_weight::BUDGET,
            "{path}: the body alone is within the budget"
        );
        let refused = METRICS.json_structure_refusal_count(family);
        let (status, _, value) = send(&w, path, body).await;
        assert_eq!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{provider} {path}: {value}"
        );
        assert_eq!(
            METRICS.json_structure_refusal_count(family),
            refused + 1,
            "{provider} {path}"
        );
        assert!(
            !value.to_string().contains("\"a\""),
            "{provider} {path}: no echo"
        );
    }
    assert_eq!(w.hits.load(Ordering::SeqCst), 0, "{provider}: nothing sent");
    assert_eq!(
        snapshot(&w.f.store.pool).await,
        before,
        "{provider}: nothing admitted"
    );
    // A tool that wrote a long file: its arguments are large but plain.
    let file: String = (0..12_000)
        .map(|i| format!("    let line_{i} = compute({i});\n"))
        .collect();
    let arguments = json!({"path": "src/main.rs", "content": file}).to_string();
    assert!(arguments.len() > 400 * 1024, "{}", arguments.len());
    let (status, _, value) = send(
        &w,
        "/v1/chat/completions",
        json!({"model":"company/smart","max_tokens":16,
            "tools":[{"type":"function","function":{"name":"write","parameters":{"type":"object"}}}],
            "messages":[
                {"role":"user","content":"write it"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"write","arguments":arguments}}]},
                {"role":"tool","tool_call_id":"c1","content":"written"},
                {"role":"user","content":"thanks"}]}),
    )
    .await;
    // Passed the bound and reached the provider (the mock answers each
    // wire but Bedrock's with a completion).
    assert_ne!(status, StatusCode::PAYLOAD_TOO_LARGE, "{provider}: {value}");
    assert_eq!(w.hits.load(Ordering::SeqCst), 1, "{provider}: sent once");
    if provider != "bedrock" {
        assert_eq!(status, StatusCode::OK, "{provider}: {value}");
    }
}

#[dbtest::test]
async fn inner_documents_are_bounded_on_an_openai_route(pool: PgPool) {
    inner_documents_on(pool, "openai").await;
}

#[dbtest::test]
async fn inner_documents_are_bounded_on_an_anthropic_route(pool: PgPool) {
    inner_documents_on(pool, "anthropic").await;
}

#[dbtest::test]
async fn inner_documents_are_bounded_on_a_bedrock_route(pool: PgPool) {
    inner_documents_on(pool, "bedrock").await;
}
