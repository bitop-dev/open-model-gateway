//! Delivery 2: exact native wire, not general request/header passthrough.
use super::{tests::*, *};

fn native(body: Value) -> ChatRequest {
    crate::protocols::messages::batch_request(body).unwrap()
}
fn fixture() -> Value {
    json!({"model":"public","max_tokens":8192,
    "system":[{"type":"text","text":"one","cache_control":{"type":"ephemeral","ttl":"1h"}},{"type":"text","text":"two"}],
    "metadata":{"user_id":"http://attacker.invalid/label"},
    "thinking":{"type":"enabled","budget_tokens":1024},
    "output_config":{"effort":"high","task_budget":{"type":"tokens","total":64000,"remaining":32000}},
    "context_management":{"edits":[{"type":"clear_tool_uses_20250919","exclude_tools":["http://attacker.invalid/tool"]}]},
    "tools":[{"name":"lookup","input_schema":{"type":"object"},"defer_loading":true,"cache_control":{"type":"ephemeral","ttl":"1h"}}],
    "messages":[
        {"role":"assistant","content":[{"type":"thinking","thinking":"http://attacker.invalid/reason","signature":"opaque+/=="},{"type":"redacted_thinking","data":"opaque-data"},{"type":"tool_use","id":"c","name":"lookup","input":{"url":"http://attacker.invalid/input"},"cache_control":{"type":"ephemeral"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"c","is_error":true,"content":[{"type":"text","text":"failed","cache_control":{"type":"ephemeral"}},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}],"cache_control":{"type":"ephemeral","ttl":"1h"}},{"type":"image","source":{"type":"base64","media_type":"image/gif","data":"R0lGODlh"}},{"type":"text","text":"continue","cache_control":{"type":"ephemeral"}}]}
    ]})
}
#[tokio::test]
async fn native_fidelity_exact_upstream_wire_and_headers() {
    for thinking in [
        json!({"type":"enabled","budget_tokens":1024}),
        json!({"type":"adaptive"}),
    ] {
        let mock = Mock::new(200, None).await;
        let mut wire = fixture();
        wire["thinking"] = thinking;
        let mut request = native(wire.clone());
        request.messages_options.as_mut().unwrap().beta_headers = vec![
            "interleaved-thinking-2025-05-14, context-management-2025-06-27".into(),
            "claude-code-20250219, extended-cache-ttl-2025-04-11, effort-2025-11-24".into(),
        ];
        mock.adapter.execute(&target(), request).await.unwrap();
        let capture = mock.capture.lock().unwrap();
        let (headers, uri, body) = &capture[0];
        assert_eq!(uri.path(), "/v1/messages");
        assert!(uri.query().is_none());
        assert_eq!(headers["x-api-key"], "mock-key");
        assert!(!headers.contains_key("authorization"));
        assert_eq!(
            headers
                .get_all("anthropic-beta")
                .iter()
                .map(|v| v.to_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "interleaved-thinking-2025-05-14, context-management-2025-06-27",
                "claude-code-20250219, extended-cache-ttl-2025-04-11, effort-2025-11-24"
            ]
        );
        wire.as_object_mut().unwrap().remove("metadata");
        wire["model"] = json!("private-model");
        wire["stream"] = json!(false);
        assert_eq!(body, &wire);
    }
}
#[tokio::test]
async fn thinking_signatures_and_redacted_blocks_round_trip() {
    let blocks = json!([{"type":"thinking","thinking":"reason","signature":"signed+/=="},{"type":"redacted_thinking","data":"encrypted+/=="},{"type":"text","text":"answer"}]);
    let mut response = complete();
    response["content"] = blocks.clone();
    let mock = Mock::new(200, Some(response.to_string().into_bytes())).await;
    let ProviderOutput::Complete(response) = mock
        .adapter
        .execute(&target(), native(fixture()))
        .await
        .unwrap()
    else {
        panic!()
    };
    let body = crate::protocols::messages::batch_response("id", "public", &response).unwrap();
    assert_eq!(body["content"], blocks);
    assert_eq!(body["model"], "public");
}
#[tokio::test]
async fn thinking_signature_deltas_stream_without_reconstruction() {
    let native_events = vec![
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed+/=="}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque+/=="}}),
        json!({"type":"content_block_stop","index":1}),
    ];
    let mut frames = vec![stream()[0].clone()];
    frames.extend(native_events.clone());
    frames.extend(stream()[4..].iter().cloned());
    for terminal in [true, false] {
        let frames = if terminal {
            &frames[..]
        } else {
            &frames[..frames.len() - 1]
        };
        let bytes = frames
            .iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect::<String>()
            .into_bytes();
        let mock = Mock::new(200, Some(bytes)).await;
        let mut r = native(fixture());
        r.stream = true;
        let ProviderOutput::Stream(s) = mock.adapter.execute(&target(), r).await.unwrap() else {
            panic!()
        };
        let events: Vec<_> = s.collect().await;
        let observed: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(ChatEvent::MessagesBlock(v)) => Some(v.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(observed, native_events);
        assert_eq!(
            events.iter().any(|e| matches!(e, Ok(ChatEvent::Done))),
            terminal
        );
        assert_eq!(events.last().unwrap().is_err(), !terminal);
    }
}
#[test]
fn native_images_reject_remote_urls_bad_base64_and_type_mismatches() {
    for source in [
        json!({"type":"url","url":"http://127.0.0.1:1/private"}),
        json!({"type":"url","url":"https://attacker.invalid/image"}),
        json!({"type":"base64","media_type":"image/png","data":"not base64"}),
        json!({"type":"base64","media_type":"image/jpeg","data":"iVBORw0KGgo="}),
        json!({"type":"base64","media_type":"image/svg+xml","data":"PHN2Zz4="}),
        json!({"type":"base64","media_type":"image/png","data":"A".repeat(8*1024*1024)}),
    ] {
        let body = json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":[{"type":"image","source":source}]}]});
        assert!(crate::protocols::messages::batch_request(body).is_err());
    }
}
#[test]
fn image_types_count_and_data_limits_are_enforced_before_dispatch() {
    use base64::Engine as _;
    for (media, bytes) in [
        ("image/png", &b"\x89PNG\r\n\x1a\n"[..]),
        ("image/jpeg", &[0xff, 0xd8, 0xff][..]),
        ("image/gif", &b"GIF89a"[..]),
        ("image/webp", &b"RIFF0000WEBP"[..]),
    ] {
        let block = json!({"type":"image","source":{"type":"base64","media_type":media,"data":base64::engine::general_purpose::STANDARD.encode(bytes)}});
        let body = |blocks| json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":blocks}]});
        let r = native(body(vec![block.clone()]));
        assert_eq!(
            encode("private", &r).unwrap()["messages"][0]["content"][0],
            block
        );
        assert!(crate::protocols::messages::batch_request(body(vec![block; 21])).is_err());
    }
}

#[test]
fn complete_thinking_is_native_messages_only() {
    let mut body = complete();
    body["content"] = json!([{"type":"thinking","thinking":"reason","signature":"opaque"},{"type":"redacted_thinking","data":"opaque"}]);
    assert!(decode_for(&body, true).is_ok());
    assert!(matches!(
        decode_for(&body, false),
        Err(InferenceError::InvalidUpstream)
    ));
}

#[test]
fn cached_native_batch_lines_use_request_aware_gateway_admission() {
    let request = crate::jobs::types::BatchRequest::Chat(native(fixture()));
    assert!(matches!(
        batches::encode_native_line(
            &target(),
            crate::jobs::types::BatchEndpoint::Messages,
            "l0",
            &request
        ),
        Err(InferenceError::InvalidField("cache_control"))
    ));
}

#[test]
fn malformed_native_thinking_streams_fail_without_done() {
    for delta in [
        json!({"type":"signature_delta","signature":7}),
        json!({"type":"thinking_delta","thinking":"reason","headers":{"Host":"attacker.invalid"}}),
        json!({"type":"text_delta","text":"wrong block"}),
    ] {
        let mut state = State {
            messages: true,
            ..Default::default()
        };
        state.push(stream()[0].clone()).unwrap();
        state.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}})).unwrap();
        assert!(
            state
                .push(json!({"type":"content_block_delta","index":0,"delta":delta}))
                .is_err()
        );
    }
}

#[test]
fn translated_error_results_are_supported() {
    use super::super::messages::{ProviderClass, validate};
    for (request, error, _) in crate::providers::contract::error_tool_result_requests() {
        assert_eq!(
            request.messages.iter().any(|m| m.tool_result_is_error),
            error,
        );
        for class in [
            ProviderClass::OpenAi,
            ProviderClass::Bedrock,
            ProviderClass::Local,
        ] {
            assert!(validate(&request, class).is_ok());
        }
    }
}
/// Structured output and strict tools are generally available on the Messages
/// API without a beta header (Anthropic structured-outputs documentation,
/// checked 2026-11-23), so they are forwarded exactly and no beta is added.
#[tokio::test]
async fn structured_output_and_strict_tools_forward_exactly_without_added_betas() {
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false});
    let wire = json!({"model":"public","max_tokens":1024,
        "output_config":{"effort":"medium","format":{"type":"json_schema","schema":schema}},
        "tools":[
            {"name":"strict_tool","input_schema":schema,"strict":true},
            {"name":"loose_tool","input_schema":{"type":"object"},"strict":false},
        ],
        "messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]});
    for stream in [false, true] {
        let mock = Mock::new(200, None).await;
        let mut request = native(wire.clone());
        request.stream = stream;
        mock.adapter.validate_chat_request(&request).unwrap();
        mock.adapter.execute(&target(), request).await.unwrap();
        let capture = mock.capture.lock().unwrap();
        let (headers, _, body) = &capture[0];
        assert!(!headers.contains_key("anthropic-beta"));
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        let mut sent = wire.clone();
        sent["model"] = json!("private-model");
        sent["stream"] = json!(stream);
        assert_eq!(body, &sent);
    }
}

/// A loopback Messages upstream that answers with structured JSON text, or
/// refuses every request with Anthropic's 400 when `status` is 400.
#[cfg(feature = "integration-tests")]
async fn structured_upstream(
    status: u16,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{Router, body::Bytes, http::Response};
    let capture = Arc::new(std::sync::Mutex::new(vec![]));
    let seen = capture.clone();
    let app = Router::new().fallback(move |bytes: Bytes| {
        let seen = seen.clone();
        async move {
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            let streaming = body["stream"] == true;
            seen.lock().unwrap().push(body);
            let text = r#"{"answer":"yes"}"#;
            let (kind, payload) = if status != 200 {
                ("application/json", json!({"type":"error","error":{"type":"invalid_request_error","message":"schema unsupported: SECRET"}}).to_string())
            } else if streaming {
                ("text/event-stream", [
                    json!({"type":"message_start","message":{"id":"x","type":"message","role":"assistant","model":"private","content":[],"stop_reason":null,"usage":{"input_tokens":4,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}),
                    json!({"type":"message_stop"}),
                ].iter().map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap())).collect::<String>())
            } else {
                ("application/json", json!({"id":"x","type":"message","role":"assistant","model":"private","content":[{"type":"text","text":text}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":4,"output_tokens":5}}).to_string())
            };
            Response::builder().status(status).header("content-type", kind).body(axum::body::Body::from(payload)).unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, capture, task)
}

/// Official Anthropic SDK -> listening gateway -> Anthropic adapter -> mock.
#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn structured_output_sdk_anthropic(pool: sqlx::PgPool) {
    let (base, capture, task) = structured_upstream(200).await;
    let adapter = AnthropicAdapter::for_test(Arc::new(Resolver), base);
    let mut deployment = target();
    deployment.credential_ref = "env:FIXTURE_ONLY".into();
    let attempts = super::super::contract::run_messages_sdk(
        pool,
        Arc::new(adapter),
        deployment,
        "structured-output.mjs",
        &[],
        true,
    )
    .await;
    task.abort();
    assert_eq!(attempts, 2, "one attempt per SDK request");
    let capture = capture.lock().unwrap();
    assert_eq!(capture.len(), 2);
    for body in capture.iter() {
        assert_eq!(
            body["output_config"],
            json!({"format":{"type":"json_schema","schema":super::super::contract::output_schema()}})
        );
        assert_eq!(body["tools"][0]["strict"], true);
    }
}

/// The upstream's schema refusal reaches the client as Anthropic's 400, with
/// no upstream text, and nothing is retried.
#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn structured_output_sdk_anthropic_upstream_refusal(pool: sqlx::PgPool) {
    let (base, capture, task) = structured_upstream(400).await;
    let adapter = AnthropicAdapter::for_test(Arc::new(Resolver), base);
    let mut deployment = target();
    deployment.credential_ref = "env:FIXTURE_ONLY".into();
    let attempts = super::super::contract::run_messages_sdk(
        pool,
        Arc::new(adapter),
        deployment,
        "structured-output.mjs",
        &["refused"],
        true,
    )
    .await;
    task.abort();
    assert_eq!(attempts, 2);
    assert_eq!(capture.lock().unwrap().len(), 2);
}
