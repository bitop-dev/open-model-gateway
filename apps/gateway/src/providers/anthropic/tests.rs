use super::super::secrets::Secret;
use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use std::{convert::Infallible, sync::Mutex};
pub(super) struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret> {
        Secret::new("mock-key".into())
    }
}
pub(super) fn target() -> Deployment {
    Deployment {
        image_input: false,
        id: uuid::Uuid::new_v4(),
        provider: "anthropic".into(),
        upstream_model: "private-model".into(),
        credential_ref: "ref".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["chat_completions".into(), "messages".into()],
    }
}
pub(super) fn request(stream: bool) -> ChatRequest {
    ChatRequest {
        messages_options: None,
        model: "public".into(),
        messages: vec![Message {
            role: Role::User,
            content: Some("hello".into()),
            tool_calls: vec![],
            tool_call_id: None,
            tool_result_is_error: false,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(50),
        stream,
    }
}
#[tokio::test]
async fn messages_frontend_safe_errors_through_mock_upstream() {
    for (status, body, expected) in [
        (
            400,
            json!({"error":{"type":"invalid_request_error","message":"prompt is too long: SECRET_BODY"}}),
            InferenceError::ContextLength,
        ),
        (
            529,
            json!({"error":{"message":"SECRET_KEY"}}),
            InferenceError::Overloaded,
        ),
        (
            429,
            json!({"error":{"message":"SECRET_PROMPT"}}),
            InferenceError::Busy,
        ),
    ] {
        let mock = Mock::new(status, Some(body.to_string().into_bytes())).await;
        let mut r = request(false);
        r.messages_options = Some(Box::default());
        let result = mock.adapter.execute(&target(), r).await;
        assert!(matches!(result, Err(e) if e == expected));
        assert!(!expected.message().contains("SECRET"));
    }
}

/// #614: an `error` event inside a stream is classified like the same error as
/// an HTTP response, for every client protocol. The cross-adapter parity
/// against real HTTP responses is `stream_class_tests`.
#[test]
fn in_stream_errors_are_classified_like_the_same_http_error() {
    use InferenceError as E;
    // (error type, chat or Responses client, Messages client)
    for (kind, chat, messages) in [
        ("overloaded_error", E::UpstreamUnavailable, E::Overloaded),
        ("rate_limit_error", E::Busy, E::Busy),
        ("api_error", E::UpstreamUnavailable, E::UpstreamUnavailable),
        (
            "invalid_request_error",
            E::UpstreamRejected,
            E::UpstreamRejected,
        ),
        ("authentication_error", E::Configuration, E::Configuration),
        ("not_found_error", E::UpstreamRejected, E::UpstreamRejected),
        (
            "unheard_of_error",
            E::UpstreamUnavailable,
            E::UpstreamUnavailable,
        ),
    ] {
        let value = json!({"type":"error","error":{"type":kind,"message":"provider diagnostic"}});
        for (is_messages, expected) in [(false, chat), (true, messages)] {
            let mut state = State {
                messages: is_messages,
                ..State::default()
            };
            assert_eq!(state.push(value.clone()).err(), Some(expected), "{kind}");
        }
    }
    let too_long = json!({"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 9"}});
    let mut messages = State {
        messages: true,
        ..State::default()
    };
    assert_eq!(
        messages.push(too_long.clone()).err(),
        Some(InferenceError::ContextLength)
    );
    // The refinement is the Messages response path's own; a chat client's is a rejection.
    assert_eq!(
        State::default().push(too_long).err(),
        Some(InferenceError::UpstreamRejected)
    );
}

#[test]
fn message_stop_before_start_closes_the_parser() {
    let mut state = State::default();
    assert_eq!(
        state.push(json!({"type":"message_stop"})).err(),
        Some(InferenceError::InvalidUpstream)
    );
    assert_eq!(
        state.push(stream()[0].clone()).err(),
        Some(InferenceError::InvalidUpstream)
    );
}

#[test]
fn a_started_message_cannot_finish_after_a_rejected_terminal() {
    let frames = stream();
    for (terminal, expected) in [
        (
            json!({"type":"error","error":{"type":"rate_limit_error"}}),
            InferenceError::Busy,
        ),
        (
            json!({"type":"message_stop"}),
            InferenceError::InvalidUpstream,
        ),
    ] {
        let mut state = State {
            messages: true,
            ..State::default()
        };
        for frame in &frames[..3] {
            state.push(frame.clone()).unwrap();
        }
        assert_eq!(state.push(terminal).err(), Some(expected));
        for late in &frames[3..] {
            assert_eq!(
                state.push(late.clone()).err(),
                Some(InferenceError::InvalidUpstream)
            );
        }
    }
}

#[test]
fn messages_sampling_is_forwarded_and_client_labels_are_not() {
    let mut r = request(false);
    r.messages_options = Some(Box::new(MessagesOptions {
        top_p: Some(0.7),
        top_k: Some(20),
        stop_sequences: Some(vec!["STOP".into()]),
        ..Default::default()
    }));
    let body = encode("upstream", &r).unwrap();
    assert_eq!(body["top_p"], 0.7);
    assert_eq!(body["top_k"], 20);
    assert_eq!(body["stop_sequences"], json!(["STOP"]));
    // `metadata.user_id` is a gateway client label, never sent upstream.
    assert!(body.get("metadata").is_none());
    assert!(body.get("authorization").is_none());
}

#[tokio::test]
async fn unpriced_omission_keeps_legacy_anthropic_wire_fallback() {
    for stream in [false, true] {
        let mock = Mock::new(200, None).await;
        let mut r = request(stream);
        r.max_output_tokens = None; // Legitimate unpriced admission retains None.
        let output = mock.adapter.execute(&target(), r).await.unwrap();
        assert_eq!(mock.capture.lock().unwrap()[0].2["max_tokens"], 1024);
        drop(output);
    }
}

pub(super) fn complete() -> Value {
    json!({"id":"upstream","type":"message","role":"assistant","model":"private","content":[{"type":"text","text":"世界"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":4,"output_tokens":2,"output_tokens_details":{"thinking_tokens":1}}})
}
pub(super) fn stream() -> Vec<Value> {
    vec![
        json!({"type":"message_start","message":{"id":"x","type":"message","role":"assistant","model":"private","content":[],"stop_reason":null,"usage":{"input_tokens":4,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"世界"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2,"output_tokens_details":{"thinking_tokens":1}}}),
        json!({"type":"message_stop"}),
    ]
}
pub(super) struct Mock {
    pub(super) adapter: AnthropicAdapter,
    pub(super) capture: Arc<Mutex<Vec<(HeaderMap, Uri, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    pub(super) async fn new(status: u16, content: Option<Vec<u8>>) -> Self {
        let capture = Arc::new(Mutex::new(vec![]));
        let c = capture.clone();
        let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, bytes: Bytes| {
            let c = c.clone();
            let content = content.clone();
            async move {
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                let streaming = value["stream"] == true;
                c.lock().unwrap().push((headers, uri, value));
                let bytes = content.unwrap_or_else(|| {
                    if streaming {
                        stream()
                            .iter()
                            .map(|v| {
                                format!(
                                    "event: {}\r\ndata: {v}\r\n\r\n",
                                    v["type"].as_str().unwrap()
                                )
                            })
                            .collect::<String>()
                            .into_bytes()
                    } else {
                        complete().to_string().into_bytes()
                    }
                });
                let chunks = bytes
                    .into_iter()
                    .map(|b| Ok::<_, Infallible>(Bytes::from(vec![b])))
                    .collect::<Vec<_>>();
                Response::builder()
                    .status(status)
                    .header(
                        "content-type",
                        if streaming {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    )
                    .body(Body::from_stream(futures_util::stream::iter(chunks)))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            adapter: AnthropicAdapter::for_test(Arc::new(Resolver), base),
            capture,
            task,
        }
    }
}
#[tokio::test]
async fn native_terminal_reasons_survive_real_http_json_and_sse() {
    for reason in [
        "refusal",
        "model_context_window_exceeded",
        "end_turn",
        "max_tokens",
    ] {
        for streaming in [false, true] {
            let bytes = if streaming {
                let mut frames = stream();
                frames[4]["delta"]["stop_reason"] = json!(reason);
                frames
                    .iter()
                    .map(|v| format!("data: {v}\n\n"))
                    .collect::<String>()
                    .into_bytes()
            } else {
                let mut body = complete();
                body["stop_reason"] = json!(reason);
                body.to_string().into_bytes()
            };
            let mock = Mock::new(200, Some(bytes)).await;
            let output = mock.adapter.execute(&target(), request(streaming)).await;
            assert!(output.is_ok(), "{reason} streaming={streaming}");
            let observed = match output.unwrap() {
                ProviderOutput::Complete(r) => serde_json::to_value(r.finish_reason).unwrap(),
                ProviderOutput::Stream(s) => {
                    let events: Vec<_> = s.collect().await;
                    assert!(events.iter().all(Result::is_ok), "{reason}");
                    assert!(matches!(events.last(), Some(Ok(ChatEvent::Done))));
                    events
                        .into_iter()
                        .find_map(|e| match e.unwrap() {
                            ChatEvent::Finish(r) => Some(serde_json::to_value(r).unwrap()),
                            _ => None,
                        })
                        .unwrap()
                }
            };
            assert_eq!(
                observed,
                json!(match reason {
                    "model_context_window_exceeded" => "context_window_exceeded",
                    "end_turn" => "stop",
                    "max_tokens" => "length",
                    other => other,
                })
            );
            assert_eq!(mock.capture.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn no_argument_tool_defaults_preserve_messages_wire() {
    let mock = Mock::new(200, None).await;
    for request in super::super::contract::no_argument_tool_requests() {
        for strict in [None, Some(false), Some(true)] {
            let mut r = request.clone();
            r.tools[0].strict = strict;
            // A Chat strict tool has no encoding here and is refused. A
            // Messages request sends its validated wire tools, so a setting on
            // the shared tool cannot add or remove `strict` (none was sent).
            if strict == Some(true) && r.messages_options.is_none() {
                assert!(matches!(
                    encode("private", &r),
                    Err(InferenceError::Unsupported)
                ));
                assert!(matches!(
                    mock.adapter.execute(&target(), r).await,
                    Err(InferenceError::Unsupported)
                ));
            } else {
                mock.adapter.execute(&target(), r).await.unwrap();
            }
        }
    }
    let capture = mock.capture.lock().unwrap();
    assert_eq!(capture.len(), 8, "strict Chat refusals never dispatch");
    for (_, uri, body) in capture.iter() {
        assert_eq!(uri.path(), "/v1/messages");
        assert_eq!(
            body["tools"][0]["input_schema"],
            json!({"type":"object","properties":{},"additionalProperties":false})
        );
        assert!(body["tools"][0].get("strict").is_none());
        assert_eq!(body["messages"][1]["content"][0]["id"], "ping-call");
        assert_eq!(body["messages"][1]["content"][0]["input"], json!({}));
        let result = &body["messages"][2]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "ping-call");
        assert_eq!(result["content"], "");
        // Native Messages retains explicit false; omission stays omitted.
        assert!(result.get("is_error").is_none_or(|v| v == false));
    }
}
#[tokio::test]
async fn local_contract_and_native_headers() {
    let mock = Mock::new(200, None).await;
    // `message.model` is reported; thinking tokens come from
    // `usage.output_tokens_details.thinking_tokens` (final cumulative usage).
    super::super::contract::assert_text_chat_contract(
        &mock.adapter,
        &target(),
        request(false),
        super::super::contract::Telemetry {
            reported_model: Some("private"),
            reasoning_tokens: Some(1),
        },
    )
    .await;
    let capture = mock.capture.lock().unwrap();
    assert_eq!(capture.len(), 2);
    for (h, uri, v) in capture.iter() {
        assert_eq!(uri.path(), "/v1/messages");
        assert_eq!(h["x-api-key"], "mock-key");
        assert_eq!(h["anthropic-version"], "2023-06-01");
        assert!(!h.contains_key("authorization"));
        assert_eq!(v["model"], "private-model");
        assert_eq!(v["max_tokens"], 50);
    }
}
#[tokio::test]
async fn eof_is_not_done_and_http_errors_are_sanitized() {
    let bytes = stream()
        .iter()
        .take(5)
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        .into_bytes();
    let mock = Mock::new(200, Some(bytes)).await;
    let ProviderOutput::Stream(s) = mock
        .adapter
        .execute(&target(), request(true))
        .await
        .unwrap()
    else {
        panic!()
    };
    let events: Vec<_> = s.collect().await;
    assert!(events.last().unwrap().is_err());
    assert!(!events.iter().any(|v| matches!(v, Ok(ChatEvent::Done))));
    for (code, expected) in [
        (302, InferenceError::Configuration),
        (401, InferenceError::Configuration),
        (429, InferenceError::Busy),
        (503, InferenceError::UpstreamUnavailable),
    ] {
        let mock = Mock::new(code, Some(b"secret upstream body".to_vec())).await;
        assert!(
            matches!(mock.adapter.execute(&target(), request(false)).await, Err(e) if e == expected)
        );
    }
}
#[test]
fn strict_subset_and_unknown_usage() {
    let mut r = request(false);
    r.temperature = Some(1.1);
    assert!(encode("x", &r).is_err());
    let mut v = complete();
    v["content"][0] = json!({"type":"thinking","thinking":"private"});
    assert!(decode(&v).is_err());
    let mut v = complete();
    v["content"][0]["citations"] = json!([{"url":"private"}]);
    assert!(decode(&v).is_err());
    let mut v = complete();
    v.as_object_mut().unwrap().remove("usage");
    // Missing usage stays unknown; only the reported model is observed.
    assert_eq!(
        decode(&v).unwrap().usage,
        Usage {
            reported_model: ReportedModel::parse("private"),
            ..Usage::default()
        }
    );
    assert!(
        AnthropicAdapter::new(Arc::new(Resolver))
            .unwrap()
            .supports_protocol(ApiProtocol::Messages)
    );
    assert!(
        !AnthropicAdapter::new(Arc::new(Resolver))
            .unwrap()
            .supports_protocol(ApiProtocol::Responses)
    );
}
#[test]
fn tool_deltas_and_ordering() {
    let open_tool = || {
        let mut state = State::default();
        state.push(stream()[0].clone()).unwrap();
        let events = state.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call","name":"weather","input":{}}})).unwrap();
        assert!(
            matches!(&events[0], ChatEvent::Delta { tool_calls, .. } if tool_calls[0].index == 0)
        );
        for part in ["{\"city\":", "\"東京\"}"] {
            state.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":part}})).unwrap();
        }
        state
    };
    let mut rejected = open_tool();
    assert_eq!(
        rejected.push(json!({"type":"message_stop"})).err(),
        Some(InferenceError::InvalidUpstream)
    );
    assert_eq!(
        rejected
            .push(json!({"type":"content_block_stop","index":0}))
            .err(),
        Some(InferenceError::InvalidUpstream)
    );

    let mut state = open_tool();
    state
        .push(json!({"type":"content_block_stop","index":0}))
        .unwrap();
    state
        .push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}))
        .unwrap();
    let events = state.push(json!({"type":"message_stop"})).unwrap();
    assert!(matches!(
        events[0],
        ChatEvent::Usage(Usage {
            input_tokens: Some(4),
            output_tokens: None,
            ..
        })
    ));
    assert!(matches!(events[1], ChatEvent::Done));
}
#[tokio::test]
async fn rejects_custom_endpoint_before_network() {
    let adapter = AnthropicAdapter::new(Arc::new(Resolver)).unwrap();
    let mut t = target();
    t.endpoint = Some("http://127.0.0.1:1".into());
    assert!(matches!(
        adapter.execute(&t, request(false)).await,
        Err(InferenceError::Configuration)
    ));
}

#[test]
fn invalid_stop_reason_cannot_emit_usage_or_done() {
    let mut frames = stream();
    let stop = frames.pop().unwrap();
    let mut delta = frames.pop().unwrap();
    delta["delta"]["stop_reason"] = json!("unknown_reason");
    let mut state = State::default();
    for frame in frames {
        state.push(frame).unwrap();
    }
    assert_eq!(
        state.push(delta).err(),
        Some(InferenceError::InvalidUpstream)
    );
    assert_eq!(
        state.push(stop).err(),
        Some(InferenceError::InvalidUpstream),
        "message_stop after a rejected stop reason must fail"
    );
}

#[test]
fn cumulative_cache_usage_has_one_finish_and_one_final_usage() {
    let mut state = State::default();
    state.push(json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":4,"output_tokens":0,"cache_read_input_tokens":10,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":8,"ephemeral_1h_input_tokens":12}}}})).unwrap();
    assert!(state.push(json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":2}})).unwrap().is_empty());
    assert!(state.push(json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":5,"input_tokens":6}})).unwrap().is_empty());
    let finished=state.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}})).unwrap();
    assert_eq!(finished.len(), 1);
    assert!(matches!(finished[0], ChatEvent::Finish(FinishReason::Stop)));
    assert!(
        state
            .push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}))
            .is_err()
    );
    let terminal = state.push(json!({"type":"message_stop"})).unwrap();
    let ChatEvent::Usage(u) = terminal[0] else {
        panic!()
    };
    assert_eq!(u.input_tokens, Some(6));
    assert_eq!(u.output_tokens, Some(7));
    assert_eq!(u.billing.unwrap().total_input_tokens, Some(36));
    assert_eq!(u.billing.unwrap().cache_write_1h_input_tokens, Some(12));
    assert!(matches!(terminal[1], ChatEvent::Done));
    assert!(state.push(json!({"type":"message_stop"})).is_err());
}
#[tokio::test]
async fn invalid_messages_body_preserves_validated_usage_as_evidence() {
    use crate::inference::evidence::capture;
    let bad = json!({"id":"m","type":"message","role":"assistant","model":"private",
        "content":[{"type":"thinking","thinking":"x"}],"stop_reason":"end_turn","stop_sequence":null,
        "usage":{"input_tokens":13,"output_tokens":14,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}});
    let (result, observed) = capture(async { decode(&bad) }).await;
    assert_eq!(result.err(), Some(InferenceError::InvalidUpstream));
    let observed = observed.unwrap();
    assert_eq!(
        (observed.input_tokens, observed.output_tokens),
        (Some(13), Some(14))
    );
}
#[test]
fn thinking_tokens_and_reported_model_are_telemetry_only() {
    // A preliminary start-event breakdown is not final evidence.
    let mut state = State::default();
    state.push(json!({"type":"message_start","message":{"type":"message","role":"assistant","model":"claude-served-1","content":[],"stop_reason":null,"usage":{"input_tokens":4,"output_tokens":1,"output_tokens_details":{"thinking_tokens":0}}}})).unwrap();
    state.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}})).unwrap();
    let ChatEvent::Usage(u) = state.push(json!({"type":"message_stop"})).unwrap()[0] else {
        panic!()
    };
    assert_eq!(
        u.reasoning_tokens, None,
        "unknown stays unknown, never zero"
    );
    assert_eq!(u.reported_model.unwrap().as_str(), "claude-served-1");
    // Cumulative deltas: the last reported breakdown wins.
    let mut state = State::default();
    state.push(stream()[0].clone()).unwrap();
    state.push(json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":5,"output_tokens_details":{"thinking_tokens":3}}})).unwrap();
    state.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8,"output_tokens_details":{"thinking_tokens":6}}})).unwrap();
    let ChatEvent::Usage(u) = state.push(json!({"type":"message_stop"})).unwrap()[0] else {
        panic!()
    };
    assert_eq!((u.output_tokens, u.reasoning_tokens), (Some(8), Some(6)));
    // Implausible or malformed breakdowns and invalid model ids are unknown,
    // never a failed response.
    for (details, model) in [
        (json!({"thinking_tokens":3}), json!("has space")),
        (json!({"thinking_tokens":"1"}), json!(7)),
        (json!(null), json!("")),
    ] {
        let mut v = complete();
        v["usage"]["output_tokens_details"] = details;
        v["model"] = model;
        let u = decode(&v).unwrap().usage;
        assert_eq!(
            (u.output_tokens, u.reasoning_tokens, u.reported_model),
            (Some(2), None, None)
        );
    }
    let mut v = complete();
    v["model"] = json!("m".repeat(ReportedModel::MAX + 1));
    assert_eq!(decode(&v).unwrap().usage.reported_model, None);
}

#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn error_tool_sdk_anthropic_preserves_boolean_presence(pool: sqlx::PgPool) {
    let mock = Mock::new(200, None).await;
    let adapter = AnthropicAdapter::for_test(Arc::new(Resolver), mock.adapter.base.clone());
    let mut deployment = target();
    deployment.credential_ref = "env:FIXTURE_ONLY".into();
    super::super::contract::assert_error_tool_sdk(pool, Arc::new(adapter), deployment).await;
    let capture = mock.capture.lock().unwrap();
    assert_eq!(capture.len(), 6);
    let expected = super::super::contract::error_tool_result_requests();
    for (i, ((_, _, body), (_, _, text))) in capture.iter().zip(expected).enumerate() {
        let results = &body["messages"][2]["content"];
        assert_eq!(results[0]["tool_use_id"], "second-call");
        assert_eq!(results[0]["content"], text);
        assert_eq!(
            results[0].get("is_error"),
            match i / 2 {
                0 => None,
                1 => Some(&json!(false)),
                _ => Some(&json!(true)),
            }
        );
        assert_eq!(
            results[1],
            json!({"type":"tool_result","tool_use_id":"first-call","content":"unchanged"})
        );
    }
}

/// Official Anthropic SDK -> listening gateway -> native Anthropic route: an
/// unsigned thinking block replayed with an empty or blank signature is
/// refused before admission, so nothing reaches Anthropic and no attempt exists.
#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn empty_signature_sdk_anthropic_is_refused_before_admission(pool: sqlx::PgPool) {
    let mock = Mock::new(200, None).await;
    let adapter = AnthropicAdapter::for_test(Arc::new(Resolver), mock.adapter.base.clone());
    let mut deployment = target();
    deployment.credential_ref = "env:FIXTURE_ONLY".into();
    let attempts = super::super::contract::run_messages_sdk(
        pool,
        Arc::new(adapter),
        deployment,
        "thinking-refusal.mjs",
        &["signature"],
        false,
    )
    .await;
    assert_eq!(attempts, 0);
    assert!(mock.capture.lock().unwrap().is_empty());
}

#[tokio::test]
async fn inline_images_reach_messages_as_base64_image_blocks() {
    use super::super::contract::{IMAGE_URL, image_request_with};
    let data = IMAGE_URL.split_once(',').unwrap().1;
    let blocks = json!([
        {"type":"text","text":"what is"},
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":data}},
        {"type":"text","text":"this?"}
    ]);
    for stream in [false, true] {
        let mock = Mock::new(200, None).await;
        assert!(mock.adapter.capabilities().image_input);
        let request = image_request_with(stream, Some("auto"));
        assert!(mock.adapter.supports_chat_request(&request));
        mock.adapter.validate_chat_request(&request).unwrap();
        drop(mock.adapter.execute(&target(), request).await.unwrap());
        let captured = mock.capture.lock().unwrap();
        assert_eq!(
            captured[0].0[reqwest::header::CONTENT_TYPE],
            "application/json"
        );
        assert_eq!(
            captured[0].2["messages"][0]["content"], blocks,
            "stream {stream}"
        );
    }
    // The provider's own count sees the same blocks.
    let mock = Mock::new(200, Some(br#"{"input_tokens":12}"#.to_vec())).await;
    let counted = mock
        .adapter
        .count_input_tokens(&target(), &image_request_with(false, None))
        .await
        .unwrap();
    assert_eq!(counted, Some(12));
    assert_eq!(
        mock.capture.lock().unwrap()[0].2["messages"][0]["content"],
        blocks
    );
}

#[tokio::test]
async fn images_across_turns_keep_their_order_and_blank_text_is_not_sent() {
    let (request, data) = super::super::contract::multi_image_request();
    let mock = Mock::new(200, None).await;
    mock.adapter.execute(&target(), request).await.unwrap();
    let sent = mock.capture.lock().unwrap()[0].2.clone();
    let image = |k: usize| json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":data[k]}});
    assert_eq!(sent["system"], json!([{"type":"text","text":"be brief"}]));
    assert_eq!(
        sent["messages"],
        json!([
            {"role":"user","content":[{"type":"text","text":"compare"}, image(0), image(1)]},
            {"role":"assistant","content":[{"type":"text","text":"two images"}]},
            {"role":"user","content":[image(2), {"type":"text","text":"and this"}]}
        ])
    );
}

#[test]
fn the_request_size_counts_images_and_the_rest_of_the_body() {
    use super::super::{contract::image_request_with, image_body::ImageLimits};
    let request = image_request_with(false, None);
    let image = super::super::image_body::images(&request)
        .map(|i| i.base64().len())
        .sum::<usize>();
    let other = serde_json::to_vec(&encode("private-model", &request).unwrap())
        .unwrap()
        .len();
    let limits = |request_bytes| ImageLimits {
        per_image: usize::MAX,
        per_image_base64: usize::MAX,
        count: 100,
        side: 8000,
        request_bytes,
        detail: false,
    };
    // The images' base64 and the body around them, exactly at the limit,
    // and one byte over it.
    assert!(encode_within("private-model", &request, &limits(image + other)).is_ok());
    assert_eq!(
        encode_within("private-model", &request, &limits(image + other - 1)).err(),
        Some(InferenceError::InvalidField("image"))
    );
}

#[tokio::test]
async fn images_the_messages_wire_cannot_carry_are_refused_before_anything_is_sent() {
    use super::super::contract::image_request_with;
    // Nothing listens at the target, so a request that was sent would fail
    // with a transport error instead of the named field.
    let adapter = AnthropicAdapter::for_test(Arc::new(Resolver), "http://127.0.0.1:9/v1".into());
    for detail in ["low", "high"] {
        let request = image_request_with(false, Some(detail));
        assert_eq!(
            adapter.validate_chat_request(&request).err(),
            Some(InferenceError::InvalidField("detail"))
        );
        assert_eq!(
            adapter.execute(&target(), request).await.err(),
            Some(InferenceError::InvalidField("detail"))
        );
    }
    // A native batch line never carries an image or its placeholder.
    let line = crate::jobs::types::BatchRequest::Chat(image_request_with(false, None));
    assert_eq!(
        adapter
            .encode_native_line(
                &target(),
                crate::jobs::types::BatchEndpoint::Messages,
                "l0",
                &line
            )
            .err(),
        Some(InferenceError::InvalidField("image"))
    );
}
