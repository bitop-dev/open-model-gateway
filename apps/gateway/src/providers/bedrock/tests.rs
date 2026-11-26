use super::*;
#[path = "auth_tests.rs"]
mod auth_tests;
#[cfg(feature = "integration-tests")]
#[path = "messages_options_tests.rs"]
mod messages_options_tests;
#[path = "stream_decode_tests.rs"]
mod stream_decode_tests;
#[cfg(feature = "integration-tests")]
#[path = "stream_error_tests.rs"]
mod stream_error_tests;
#[path = "stream_transport_tests.rs"]
mod stream_transport_tests;
#[path = "structured_tests.rs"]
mod structured_tests;
#[path = "timeout_tests.rs"]
mod timeout_tests;
use crate::timestamp::TimestampFormat as _;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use futures_util::StreamExt;
use serde_json::json;
use std::{
    convert::Infallible,
    sync::{Arc, Mutex as StdMutex},
};

impl BedrockAdapter {
    pub(super) fn for_test_clients(
        base: &str,
        timeouts: transport::Timeouts,
        http: aws_smithy_runtime_api::client::http::SharedHttpClient,
        streaming_http: aws_smithy_runtime_api::client::http::SharedHttpClient,
    ) -> Self {
        Self::for_test_identity(
            base,
            timeouts,
            http,
            streaming_http,
            aws_sdk_bedrockruntime::config::SharedCredentialsProvider::new(
                aws_sdk_bedrockruntime::config::Credentials::new(
                    "AKIDLOCALTEST",
                    "local-not-a-secret",
                    Some("local-session-token".into()),
                    None,
                    "local-test",
                ),
            ),
        )
    }

    /// A real adapter against a local mock whose SDK credential provider
    /// fails: a named profile (`auth::profile_provider`, the production
    /// profile path) missing from an explicit credentials file, so the SDK
    /// never gets an identity and never calls its connector.
    #[cfg(feature = "integration-tests")]
    #[allow(deprecated)]
    pub(crate) fn for_test_without_credentials(base: &str) -> Self {
        use aws_config::profile::profile_file::{ProfileFileKind, ProfileFiles};
        // Same content for every caller, so concurrent tests may share it.
        let path = std::env::temp_dir().join("omg-bedrock-without-absent-profile.ini");
        std::fs::write(
            &path,
            "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key = default-not-a-secret\n",
        )
        .unwrap();
        let files = ProfileFiles::builder()
            .with_file(ProfileFileKind::Credentials, &path)
            .build();
        let timeouts = transport::Timeouts::default();
        Self::for_test_identity(
            base,
            timeouts,
            transport::client(timeouts, false).unwrap(),
            transport::client(timeouts, true).unwrap(),
            aws_sdk_bedrockruntime::config::SharedCredentialsProvider::new(auth::profile_provider(
                "absent",
                "us-east-1",
                Some(files),
            )),
        )
    }

    fn for_test_identity(
        base: &str,
        timeouts: transport::Timeouts,
        http: aws_smithy_runtime_api::client::http::SharedHttpClient,
        streaming_http: aws_smithy_runtime_api::client::http::SharedHttpClient,
        credentials: aws_sdk_bedrockruntime::config::SharedCredentialsProvider,
    ) -> Self {
        let mut adapter = Self::new().unwrap();
        adapter.timeouts = timeouts;
        adapter.http = http;
        adapter.streaming_http = streaming_http;
        let config = aws_sdk_bedrockruntime::Config::builder()
            .behavior_version(auth::sdk_behavior_version())
            .region(Region::new("us-east-1"))
            .credentials_provider(credentials)
            .endpoint_url(base)
            .http_client(adapter.http.clone())
            .timeout_config(adapter.operation_timeouts())
            .stalled_stream_protection(
                aws_sdk_bedrockruntime::config::StalledStreamProtectionConfig::enabled()
                    .grace_period(timeouts.read.min(Duration::from_secs(5)))
                    .build(),
            )
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            .build();
        Self {
            test_client: Some(Client::from_conf(config)),
            ..adapter
        }
    }
}

struct Captured {
    headers: HeaderMap,
    uri: Uri,
    body: Value,
}
struct Mock {
    base: String,
    requests: Arc<StdMutex<Vec<Captured>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn serve(complete: Value, frames: Vec<u8>, status: u16) -> Self {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let capture = requests.clone();
        let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, bytes: Bytes| {
            let capture = capture.clone();
            let complete = complete.clone();
            let frames = frames.clone();
            async move {
                let stream = status == 200 && uri.path().ends_with("converse-stream");
                capture.lock().unwrap().push(Captured {
                    headers,
                    uri,
                    body: serde_json::from_slice(&bytes).unwrap(),
                });
                let data = if stream {
                    frames
                } else {
                    serde_json::to_vec(&complete).unwrap()
                };
                // Arbitrary fragmentation exercises AWS binary decoder across frame/header/CRC boundaries.
                let chunk_size = if data.len() > 4096 { 1024 } else { 7 };
                let chunks: Vec<_> = data
                    .chunks(chunk_size)
                    .map(|b| Ok::<_, Infallible>(Bytes::copy_from_slice(b)))
                    .collect();
                Response::builder()
                    .status(status)
                    .header(
                        "content-type",
                        if stream {
                            "application/vnd.amazon.eventstream"
                        } else {
                            "application/json"
                        },
                    )
                    .body(Body::from_stream(futures_util::stream::iter(chunks)))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            requests,
            task,
        }
    }
    fn adapter(&self) -> BedrockAdapter {
        // Only test code can inject an endpoint or credentials. The normal constructor has
        // no endpoint argument and never inherits shared endpoint configuration.
        let mut adapter = BedrockAdapter::new().unwrap();
        let config = aws_sdk_bedrockruntime::Config::builder()
            .behavior_version(auth::sdk_behavior_version())
            .region(Region::new("us-east-1"))
            .credentials_provider(aws_sdk_bedrockruntime::config::Credentials::new(
                "AKIDLOCALTEST",
                "local-not-a-secret",
                Some("local-session-token".into()),
                None,
                "local-test",
            ))
            .endpoint_url(&self.base)
            .http_client(adapter.http.clone())
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            .build();
        adapter.test_client = Some(Client::from_conf(config));
        adapter
    }
}
fn target() -> Deployment {
    Deployment {
        image_input: false,
        id: uuid::Uuid::new_v4(),
        provider: "bedrock".into(),
        upstream_model: "test-model".into(),
        credential_ref: "aws:default".into(),
        endpoint: None,
        region: Some("us-east-1".into()),
        supported_protocols: vec!["chat_completions".into(), "messages".into()],
    }
}
fn message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: Some(text.into()),
        tool_calls: Vec::new(),
        tool_call_id: None,
        tool_result_is_error: false,
    }
}
fn request() -> ChatRequest {
    ChatRequest {
        messages_options: None,
        model: "public-model".into(),
        messages: vec![
            message(Role::System, "instructions"),
            message(Role::User, "hello"),
        ],
        tools: Vec::new(),
        tool_choice: None,
        temperature: Some(0.5),
        max_output_tokens: Some(100),
        stream: false,
    }
}
fn tool() -> FunctionTool {
    FunctionTool {
        name: "weather".into(),
        description: Some("Get weather".into()),
        strict: Some(false),
        parameters: json!({"type":"object", "properties":{"city":{"type":"string"}}, "required":["city"]}),
    }
}
fn complete(tool: bool) -> Value {
    json!({"output":{"message":{"role":"assistant", "content": if tool { json!([
        {"text":"Looking up weather"}, {"toolUse":{"toolUseId":"call_1", "name":"weather", "input":{"city":"Paris"}}}
    ]) } else { json!([{"text":"Hello"}]) }}}, "stopReason":if tool {"tool_use"} else {"end_turn"},
        "usage":{"inputTokens":3, "outputTokens":5,"totalTokens":8}, "metrics":{"latencyMs":2}})
}
fn events(tool: bool) -> Vec<(&'static str, Value)> {
    let mut events = vec![
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0, "delta":{"text":"Hello"}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
    ];
    if tool {
        events.extend([
            ("contentBlockStart", json!({"contentBlockIndex":4, "start":{"toolUse":{"toolUseId":"call_1", "name":"weather"}}})),
            ("contentBlockDelta", json!({"contentBlockIndex":4, "delta":{"toolUse":{"input":"{\"city\":"}}})),
            ("contentBlockDelta", json!({"contentBlockIndex":4, "delta":{"toolUse":{"input":"\"Paris\"}"}}})),
            ("contentBlockStop", json!({"contentBlockIndex":4}))]);
    }
    events.extend([("messageStop", json!({"stopReason":if tool {"tool_use"} else {"end_turn"}})),
        ("metadata", json!({"usage":{"inputTokens":3,"outputTokens":5,"totalTokens":8}, "metrics":{"latencyMs":2}}))]);
    events
}
// AWS event-stream framing: big endian prelude, typed headers, payload, two CRC32s.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
/// A header value in the AWS event-stream binary encoding.
#[derive(Clone, Copy)]
enum Hdr<'a> {
    True,
    Bytes(&'a [u8]),
    Int(i32),
    Str(&'a str),
    Time(i64),
}
/// A frame with exactly these headers, in this order, and this raw payload.
fn raw_frame(headers: &[(&str, Hdr)], payload: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for (name, value) in headers {
        encoded.push(name.len() as u8);
        encoded.extend(name.as_bytes());
        match value {
            Hdr::True => encoded.push(0),
            Hdr::Bytes(bytes) => {
                encoded.push(6);
                encoded.extend((bytes.len() as u16).to_be_bytes());
                encoded.extend(*bytes);
            }
            Hdr::Int(n) => {
                encoded.push(4);
                encoded.extend(n.to_be_bytes());
            }
            Hdr::Str(text) => {
                encoded.push(7);
                encoded.extend((text.len() as u16).to_be_bytes());
                encoded.extend(text.as_bytes());
            }
            Hdr::Time(millis) => {
                encoded.push(8);
                encoded.extend(millis.to_be_bytes());
            }
        }
    }
    let mut result = Vec::new();
    result.extend(((16 + encoded.len() + payload.len()) as u32).to_be_bytes());
    result.extend((encoded.len() as u32).to_be_bytes());
    result.extend(crc32(&result).to_be_bytes());
    result.extend(encoded);
    result.extend(payload);
    result.extend(crc32(&result).to_be_bytes());
    result
}
fn frame(kind: &str, message_type: &str, payload: Value) -> Vec<u8> {
    let type_header = if message_type == "exception" {
        ":exception-type"
    } else {
        ":event-type"
    };
    raw_frame(
        &[
            (":message-type", Hdr::Str(message_type)),
            (type_header, Hdr::Str(kind)),
            (":content-type", Hdr::Str("application/json")),
        ],
        &serde_json::to_vec(&payload).unwrap(),
    )
}
fn wire(events: Vec<(&str, Value)>) -> Vec<u8> {
    events
        .into_iter()
        .flat_map(|(kind, value)| frame(kind, "event", value))
        .collect()
}

#[tokio::test]
async fn messages_parallel_calls_and_results_preserve_converse_turns() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let request = super::super::contract::parallel_messages_request();
    let adapter = mock.adapter();
    adapter.validate_chat_request(&request).unwrap();
    adapter.execute(&target(), request).await.unwrap();
    let capture = mock.requests.lock().unwrap();
    assert_eq!(capture.len(), 1);
    let messages = capture[0].body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0]["content"], json!([{"text":"hello there"}]));
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"],
        json!([
            {"text":"checking both!"},
            {"toolUse":{"toolUseId":"first-call","name":"ping","input":{"value":1}}},
            {"toolUse":{"toolUseId":"second-call","name":"pong","input":{"value":2}}}
        ])
    );
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["content"],
        json!([
            {"toolResult":{"toolUseId":"second-call","content":[{"text":"second result"}]}},
            {"toolResult":{"toolUseId":"first-call","content":[{"text":"first result"}]}}
        ])
    );
    assert_eq!(messages[3]["role"], "assistant");
    assert_eq!(messages[3]["content"], json!([{"text":"done"}]));
    assert_eq!(messages[4]["role"], "user");
    assert_eq!(messages[4]["content"], json!([{"text":"continue"}]));
}

#[tokio::test]
async fn no_argument_tool_defaults_preserve_converse_wire_and_correlation() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let adapter = mock.adapter();
    for request in super::super::contract::no_argument_tool_requests() {
        for strict in [None, Some(false), Some(true)] {
            let mut r = request.clone();
            r.tools[0].strict = strict;
            adapter.execute(&target(), r).await.unwrap();
        }
        // Empty content is not permission to detach a result from its call.
        for id in [None, Some("wrong-call".into())] {
            let mut r = request.clone();
            r.messages[2].tool_call_id = id;
            assert!(matches!(
                adapter.execute(&target(), r).await,
                Err(InferenceError::InvalidRequest)
            ));
        }
        let mut duplicate = request.clone();
        duplicate.messages.push(request.messages[2].clone());
        assert!(matches!(
            adapter.execute(&target(), duplicate).await,
            Err(InferenceError::InvalidRequest)
        ));
    }
    let capture = mock.requests.lock().unwrap();
    assert_eq!(capture.len(), 9, "correlation refusals never dispatch");
    for (i, captured) in capture.iter().enumerate() {
        let body = &captured.body;
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"].get("strict"),
            (i % 3 == 2).then_some(&json!(true))
        );
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"],
            json!({"type":"object","properties":{},"additionalProperties":false})
        );
        assert_eq!(
            body["messages"][1]["content"][0]["toolUse"],
            json!({"toolUseId":"ping-call","name":"ping","input":{}})
        );
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"],
            json!({"toolUseId":"ping-call","content":[{"text":""}]})
        );
    }
}
#[tokio::test]
async fn shared_text_contract_and_actual_sdk_signed_request() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let adapter = mock.adapter();
    // Converse reports neither the served model (outside prompt routers) nor
    // reasoning tokens: both stay unknown and Logs fall back to the configured id.
    super::super::contract::assert_text_chat_contract(
        &adapter,
        &target(),
        request(),
        super::super::contract::Telemetry::UNKNOWN,
    )
    .await;
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for (i, req) in requests.iter().enumerate() {
        assert_eq!(
            req.uri.path(),
            if i == 0 {
                "/model/test-model/converse"
            } else {
                "/model/test-model/converse-stream"
            }
        );
        let auth = req.headers["authorization"].to_str().unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDLOCALTEST/"));
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
        assert!(auth.contains("Signature="));
        assert_eq!(req.headers["x-amz-security-token"], "local-session-token");
        assert!(req.headers.contains_key("x-amz-date"));
        assert_eq!(req.body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(req.body["system"][0]["text"], "instructions");
        assert_eq!(
            req.body["inferenceConfig"],
            json!({"maxTokens":100,"temperature":0.5})
        );
        assert!(!req.body.to_string().contains("local-session-token"));
    }
}

fn routed(mut events: Vec<(&'static str, Value)>, invoked: &str) -> Vec<(&'static str, Value)> {
    for (kind, value) in &mut events {
        if *kind == "metadata" {
            value["trace"] = json!({"promptRouter":{"invokedModelId":invoked}});
        }
    }
    events
}

#[tokio::test]
async fn prompt_router_trace_reports_the_invoked_model() {
    const INVOKED: &str =
        "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-3-haiku-20240307-v1:0";
    let mut body = complete(false);
    body["trace"] = json!({"promptRouter":{"invokedModelId":INVOKED}});
    let mock = Mock::serve(body, wire(routed(events(false), INVOKED)), 200).await;
    super::super::contract::assert_text_chat_contract(
        &mock.adapter(),
        &target(),
        request(),
        super::super::contract::Telemetry {
            reported_model: Some(INVOKED),
            reasoning_tokens: None,
        },
    )
    .await;
    // An invalid reported id is unknown and never fails the request.
    let mut body = complete(false);
    body["trace"] = json!({"promptRouter":{"invokedModelId":"not a model id"}});
    let mock = Mock::serve(body, wire(routed(events(false), "")), 200).await;
    super::super::contract::assert_text_chat_contract(
        &mock.adapter(),
        &target(),
        request(),
        super::super::contract::Telemetry::UNKNOWN,
    )
    .await;
}

#[tokio::test]
async fn tools_json_and_stream_tool_indices_usage_after_finish() {
    let mock = Mock::serve(complete(true), wire(events(true)), 200).await;
    let adapter = mock.adapter();
    let mut req = request();
    req.tools.push(tool());
    req.tool_choice = Some(ToolChoice::Required);
    let ProviderOutput::Complete(result) = adapter.execute(&target(), req.clone()).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(result.tool_calls[0].arguments, "{\"city\":\"Paris\"}");
    assert_eq!(result.finish_reason, FinishReason::ToolCalls);
    assert_eq!(result.usage.input_tokens, Some(3));
    req.stream = true;
    let ProviderOutput::Stream(mut stream) = adapter.execute(&target(), req).await.unwrap() else {
        panic!()
    };
    let mut args = String::new();
    let mut order = Vec::new();
    let mut starts = 0;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            ChatEvent::Delta { tool_calls, .. } => {
                for delta in tool_calls {
                    assert_eq!(
                        delta.index, 0,
                        "canonical tool index must not be AWS content block index"
                    );
                    if let Some(id) = delta.id {
                        assert_eq!(id, "call_1");
                        starts += 1;
                    }
                    if let Some(value) = delta.arguments {
                        args.push_str(&value);
                    }
                }
            }
            ChatEvent::Finish(reason) => {
                assert_eq!(reason, FinishReason::ToolCalls);
                order.push("finish");
            }
            ChatEvent::Usage(usage) => {
                assert_eq!(usage.output_tokens, Some(5));
                order.push("usage");
            }
            ChatEvent::Done => order.push("done"),
            ChatEvent::MessagesBlock(_) => panic!("native Anthropic event on Converse"),
            ChatEvent::Reasoning(_) => panic!("Converse reasoning is not mapped"),
        }
    }
    assert_eq!(starts, 1);
    assert_eq!(args, "{\"city\":\"Paris\"}");
    assert_eq!(order, ["finish", "usage", "done"]);
    let requests = mock.requests.lock().unwrap();
    assert_eq!(
        requests[0].body["toolConfig"]["toolChoice"],
        json!({"any":{}})
    );
    assert_eq!(
        requests[0].body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"],
        tool().parameters
    );
}

#[tokio::test]
async fn tool_history_preserves_json_input_and_result_text() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let mut req = request();
    req.tools.push(tool());
    req.messages.push(Message {
        role: Role::Assistant,
        content: None,
        tool_call_id: None,
        tool_result_is_error: false,
        tool_calls: vec![
            ToolCall {
                id: "call_1".into(),
                name: "weather".into(),
                arguments: "{\"city\":\"Paris\"}".into(),
            },
            ToolCall {
                id: "call_2".into(),
                name: "weather".into(),
                arguments: "{}".into(),
            },
        ],
    });
    for id in ["call_1", "call_2"] {
        req.messages.push(Message {
            role: Role::Tool,
            content: Some("{ \"temp\": 20 }".into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
            tool_result_is_error: false,
        });
    }
    mock.adapter()
        .execute_protocol(&target(), req, ApiProtocol::Messages)
        .await
        .unwrap();
    let captured = mock.requests.lock().unwrap();
    let messages = &captured[0].body["messages"];
    assert_eq!(messages.as_array().unwrap().len(), 3);
    assert_eq!(
        messages[1]["content"][0]["toolUse"]["input"],
        json!({"city":"Paris"})
    );
    assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
    assert_eq!(
        messages[2]["content"][0]["toolResult"]["content"][0],
        json!({"text":"{ \"temp\": 20 }"})
    );
}

async fn assert_bad_stream(frames: Vec<u8>) {
    let mock = Mock::serve(complete(false), frames, 200).await;
    let mut req = request();
    req.stream = true;
    let mut output = match mock.adapter().execute(&target(), req).await {
        Ok(ProviderOutput::Stream(output)) => output,
        // SDK versions differ on whether an empty/corrupt first frame fails
        // during response establishment or the first stream read. Both fail closed.
        Err(InferenceError::InvalidUpstream | InferenceError::UpstreamUnavailable) => return,
        _ => panic!("invalid stream was not rejected"),
    };
    let mut failed = false;
    while let Some(event) = output.next().await {
        match event {
            Err(_) => failed = true,
            Ok(ChatEvent::Done) => panic!("invalid stream emitted Done"),
            _ => {}
        }
    }
    assert!(failed);
}

#[tokio::test]
async fn truncated_corrupt_exception_and_post_stop_streams_fail_closed() {
    let valid = events(false);
    for length in 0..valid.len() {
        assert_bad_stream(wire(valid[..length].to_vec())).await;
    }
    let mut bytes = wire(valid.clone());
    bytes.pop();
    assert_bad_stream(bytes).await;
    let mut bytes = wire(valid.clone());
    bytes[15] ^= 1;
    assert_bad_stream(bytes).await;
    let mut bytes = wire(valid.clone());
    bytes.extend(frame(
        "internalServerException",
        "exception",
        json!({"message":"secret provider details"}),
    ));
    assert_bad_stream(bytes).await;
    let mut bad = valid.clone();
    bad.push((
        "contentBlockDelta",
        json!({"contentBlockIndex":0,"delta":{"text":"late"}}),
    ));
    assert_bad_stream(wire(bad)).await;
    let mut duplicate = valid.clone();
    duplicate.push(valid.last().unwrap().clone());
    assert_bad_stream(wire(duplicate)).await;
}

#[tokio::test]
async fn incomplete_tool_arguments_unknown_content_and_stop_reason_fail() {
    let mut bad = events(true);
    bad[5].1["delta"]["toolUse"]["input"] = json!("broken");
    assert_bad_stream(wire(bad)).await;
    let mut bad = events(false);
    // Content the gateway cannot return (here a citation) fails the stream.
    // Reasoning content is decoded (`structured_tests`).
    bad[1].1["delta"] = json!({"citation":{"title":"hidden source"}});
    assert_bad_stream(wire(bad)).await;
    let mut bad = events(false);
    bad[3].1["stopReason"] = json!("new_future_reason");
    assert_bad_stream(wire(bad)).await;
    let mut bad = events(false);
    bad[1].1["contentBlockIndex"] = json!(-1);
    assert_bad_stream(wire(bad)).await;
}

#[tokio::test]
async fn messages_bedrock_context_envelope_does_not_match_quoted_client_text() {
    for (code, text, expected) in [
        (
            "ValidationException",
            "Input is too long for requested model.",
            InferenceError::ContextLength,
        ),
        (
            "ValidationException",
            "Invalid field quoting client text: prompt is too long",
            InferenceError::UpstreamRejected,
        ),
        (
            "ThrottlingException",
            "Input is too long for requested model.",
            InferenceError::Busy,
        ),
    ] {
        let mock = Mock::serve(json!({"__type":code,"message":text}), Vec::new(), 400).await;
        let mut r = request();
        r.messages_options = Some(Box::default());
        assert!(
            matches!(mock.adapter().execute(&target(), r).await, Err(error) if error == expected)
        );
    }
}

#[tokio::test]
async fn safe_error_mapping_and_no_sdk_retries() {
    for (code, status, expected) in [
        ("ThrottlingException", 429, InferenceError::Busy),
        ("AccessDeniedException", 403, InferenceError::Configuration),
        ("ValidationException", 400, InferenceError::UpstreamRejected),
        (
            "InternalServerException",
            500,
            InferenceError::UpstreamUnavailable,
        ),
    ] {
        let mock = Mock::serve(
            json!({"__type":code,"message":"upstream secret prompt"}),
            Vec::new(),
            status,
        )
        .await;
        let result = mock.adapter().execute(&target(), request()).await;
        assert!(matches!(result, Err(error) if error == expected));
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        assert!(!expected.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn invalid_config_and_protocol_never_reach_credentials_or_network() {
    let adapter = BedrockAdapter::new().unwrap();
    for change in 0..7 {
        let mut target = target();
        match change {
            0 => target.credential_ref = "env:AWS_SECRET_ACCESS_KEY".into(),
            1 => target.credential_ref = "plaintext-secret".into(),
            2 => target.endpoint = Some("http://127.0.0.1".into()),
            3 => target.region = None,
            4 => target.region = Some("".into()),
            5 => target.provider = "openai".into(),
            _ => target.region = Some("us-east-1/evil".into()),
        }
        assert!(matches!(
            adapter.execute(&target, request()).await,
            Err(InferenceError::Configuration)
        ));
    }
    assert!(matches!(
        adapter
            .execute_protocol(&target(), request(), ApiProtocol::Responses)
            .await,
        Err(InferenceError::Unsupported)
    ));
}

#[test]
fn validates_semantics_without_silent_loss() {
    assert_eq!(
        BedrockAdapter::new().unwrap().generation_output_limit(),
        i32::MAX as u32
    );
    let mut boundary = request();
    boundary.max_output_tokens = Some(i32::MAX as u32);
    assert!(encode(&boundary).is_ok());
    let check = |request: ChatRequest| assert!(encode(&request).is_err());
    for temp in [f64::NAN, f64::INFINITY, -0.1, 1.01] {
        let mut req = request();
        req.temperature = Some(temp);
        check(req);
    }
    for max in [0, u32::MAX] {
        let mut req = request();
        req.max_output_tokens = Some(max);
        check(req);
    }
    let mut req = request();
    req.messages.push(message(Role::System, "late"));
    check(req);
    let mut req = request();
    req.messages[0].role = Role::Developer;
    check(req);
    let mut req = request();
    req.tools.push(tool());
    req.tools[0].parameters = json!({"type":"array"});
    check(req);
    let mut req = request();
    req.tools.push(tool());
    req.tool_choice = Some(ToolChoice::None);
    check(req);
    let mut req = request();
    req.tool_choice = Some(ToolChoice::Required);
    check(req);
    let mut req = request();
    req.tools.push(tool());
    req.tool_choice = Some(ToolChoice::Function("missing".into()));
    check(req);
    let mut req = request();
    req.messages.push(message(Role::Assistant, "prefill"));
    check(req);
    let mut req = request();
    req.messages[1].tool_call_id = Some("orphan".into());
    check(req);
    let mut req = request();
    req.messages[1].content = Some("x".repeat(OUTPUT_LIMIT + 1).into());
    check(req);
    let mut req = request();
    req.tools.push(tool());
    req.tool_choice = Some(ToolChoice::Function("weather".into()));
    assert!(encode(&req).is_ok());
    assert_eq!(
        finish(&aws::StopReason::MaxTokens).unwrap(),
        FinishReason::Length
    );
    assert_eq!(
        finish(&aws::StopReason::GuardrailIntervened).unwrap(),
        FinishReason::ContentFilter
    );
    assert_eq!(
        finish(&aws::StopReason::StopSequence).unwrap(),
        FinishReason::Stop
    );
}

#[tokio::test]
async fn bounded_complete_and_stream_output() {
    let mut oversized = complete(false);
    oversized["output"]["message"]["content"][0]["text"] = json!("x".repeat(OUTPUT_LIMIT + 1));
    let mock = Mock::serve(oversized, Vec::new(), 200).await;
    assert!(matches!(
        mock.adapter().execute(&target(), request()).await,
        Err(InferenceError::InvalidUpstream)
    ));
    let mut events = events(false);
    events[1].1["delta"]["text"] = json!("x".repeat(OUTPUT_LIMIT + 1));
    assert_bad_stream(wire(events)).await;

    // Pre-deserialization wire bound, with no Content-Length, also limits error bodies.
    let mock = Mock::serve(
        json!({"message":"x".repeat(transport::WIRE_LIMIT + 1)}),
        Vec::new(),
        500,
    )
    .await;
    assert!(mock.adapter().execute(&target(), request()).await.is_err());
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn dropping_stream_cancels_upstream_body() {
    struct Dropped(Arc<tokio::sync::Notify>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    let dropped = Arc::new(tokio::sync::Notify::new());
    let signal = dropped.clone();
    let initial = wire(events(false)[..2].to_vec());
    let app = Router::new().fallback(move || {
        let signal = signal.clone();
        let initial = initial.clone();
        async move {
            let stream = async_stream::stream! {
                let _guard = Dropped(signal);
                yield Ok::<_, Infallible>(Bytes::from(initial));
                std::future::pending::<()>().await;
            };
            Response::builder()
                .header("content-type", "application/vnd.amazon.eventstream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = Mock {
        base: format!("http://{}", listener.local_addr().unwrap()),
        requests: Default::default(),
        task: tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    };
    let mut req = request();
    req.stream = true;
    let ProviderOutput::Stream(mut stream) = mock.adapter().execute(&target(), req).await.unwrap()
    else {
        panic!()
    };
    assert!(matches!(
        stream.next().await,
        Some(Ok(ChatEvent::Delta { .. }))
    ));
    drop(stream);
    tokio::time::timeout(Duration::from_secs(3), dropped.notified())
        .await
        .expect("dropping consumer must cancel upstream body");
}

#[test]
fn document_numeric_roundtrip_and_invalid_usage() {
    let value = json!({"huge":u64::MAX,"negative":i64::MIN,"fraction":0.5,"list":[true,null,"hi"]});
    assert_eq!(json_document(&document(&value).unwrap()).unwrap(), value);
    assert!(
        usage(
            &aws::TokenUsage::builder()
                .input_tokens(-1)
                .output_tokens(2)
                .total_tokens(1)
                .build()
                .unwrap()
        )
        .is_err()
    );
}

#[tokio::test]
async fn wire_missing_usage_never_becomes_sdk_zero_and_ttl_is_preserved() {
    for missing in [
        json!(null),
        json!({}),
        json!({"inputTokens":null,"outputTokens":0,"totalTokens":0}),
    ] {
        let mut body = complete(false);
        body["usage"] = missing.clone();
        let mock = Mock::serve(body, vec![], 200).await;
        assert!(mock.adapter().execute(&target(), request()).await.is_err());
        let mut e = events(false);
        e.last_mut().unwrap().1["usage"] = missing;
        let mock = Mock::serve(complete(false), wire(e), 200).await;
        let mut r = request();
        r.stream = true;
        let ProviderOutput::Stream(stream) = mock.adapter().execute(&target(), r).await.unwrap()
        else {
            panic!()
        };
        let events: Vec<_> = stream.collect().await;
        assert!(events.last().unwrap().is_err());
        assert!(!events.iter().any(|e| matches!(e, Ok(ChatEvent::Done))));
    }
    let mut body = complete(false);
    body["usage"] = json!({"inputTokens":3,"outputTokens":5,"totalTokens":38,"cacheReadInputTokens":10,"cacheWriteInputTokens":20,"cacheDetails":[{"ttl":"1h","inputTokens":12},{"ttl":"5m","inputTokens":8}]});
    let mock = Mock::serve(body, vec![], 200).await;
    let ProviderOutput::Complete(response) =
        mock.adapter().execute(&target(), request()).await.unwrap()
    else {
        panic!()
    };
    let b = response.usage.billing.unwrap();
    assert_eq!(response.usage.input_tokens, Some(3));
    assert_eq!(b.total_input_tokens, Some(33));
    assert_eq!(b.cache_write_1h_input_tokens, Some(12));
    assert_eq!(b.cache_write_5m_input_tokens, Some(8));
    assert_eq!(b.cache_write_default_input_tokens, Some(0));
}

/// Requests are signed with this host's clock, whatever a response's `Date`
/// header says (#456): the SDK's clock-skew correction, on by default since
/// the generation after 1.124, is turned off in the production configuration.
#[tokio::test]
async fn a_response_date_never_moves_the_signing_time() {
    let dates = Arc::new(StdMutex::new(Vec::<String>::new()));
    let seen = dates.clone();
    let app = Router::new().fallback(move |headers: HeaderMap| {
        let seen = seen.clone();
        async move {
            seen.lock()
                .unwrap()
                .push(headers["x-amz-date"].to_str().unwrap().to_owned());
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                // A server (or anyone on the path) whose clock reads an hour behind.
                .header(
                    "date",
                    (gateway_datetime_compat::UtcInstant::now()
                        - gateway_datetime_compat::SignedDuration::hours(1))
                    .format("%a, %d %b %Y %H:%M:%S GMT")
                    .to_string(),
                )
                .body(Body::from(serde_json::to_vec(&complete(false)).unwrap()))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut adapter = BedrockAdapter::new().unwrap();
    let plan = auth::Plan {
        region: "us-east-1".into(),
        auth: auth::AwsAuth::parse("aws:default").unwrap(),
        endpoint: Some(base),
    };
    // The production configuration; only the endpoint and credentials are the test's.
    adapter.test_client = Some(Client::from_conf(adapter.service_config(
        &plan,
        aws_sdk_bedrockruntime::config::SharedCredentialsProvider::new(
            aws_sdk_bedrockruntime::config::Credentials::new(
                "AKIDLOCALTEST",
                "local-not-a-secret",
                None,
                None,
                "local-test",
            ),
        ),
    )));
    for _ in 0..3 {
        let output = adapter.execute(&target(), request()).await.unwrap();
        assert!(matches!(output, ProviderOutput::Complete(_)));
    }
    server.abort();
    let dates = dates.lock().unwrap().clone();
    assert_eq!(dates.len(), 3);
    for (index, signed) in dates.iter().enumerate() {
        let signed = crate::timestamp::parse_sigv4(signed).unwrap();
        let off = (gateway_datetime_compat::UtcInstant::now() - signed)
            .whole_seconds()
            .abs();
        assert!(
            off < 300,
            "request {index} was signed {off} s away from this host's clock"
        );
    }
}

/// What a stream broken at its first frame looks like with the current SDK
/// generation (#456, #486): the SDK returns once the response headers arrive,
/// so the adapter hands back a stream, and that stream then fails without a
/// `Done`. Before the 1.15x generation the SDK read the first raw frame while
/// establishing the response, and a corrupt one failed the request instead.
/// The release notes describe this shape; a change of it should be noticed.
#[tokio::test]
async fn a_stream_corrupt_at_its_first_frame_fails_in_the_stream_without_done() {
    let mut corrupt_first = wire(events(false));
    corrupt_first[9] ^= 1; // the first frame's prelude CRC
    let mock = Mock::serve(complete(false), corrupt_first, 200).await;
    let mut req = request();
    req.stream = true;
    let Ok(ProviderOutput::Stream(mut output)) = mock.adapter().execute(&target(), req).await
    else {
        panic!("the SDK no longer returns a stream before reading the first frame");
    };
    let mut events_seen = Vec::new();
    while let Some(event) = output.next().await {
        events_seen.push(event.is_ok());
    }
    assert_eq!(events_seen, [false], "one error, no content and no Done");
}

#[tokio::test]
async fn error_tool_results_use_converse_status_without_changing_text() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let adapter = mock.adapter();
    for model in [
        "test-model",
        "anthropic.claude-sonnet-5-v1:0",
        "new-region.anthropic.claude-sonnet-4-20250514-v1:0",
        "amazon.nova-pro-v1:0",
        "apac.amazon.nova-2-lite-v1:0",
        "us.anthropic.claude-haiku-4-5-20251001-v1:0",
        "global.anthropic.claude-opus-4-1-20250805-v1:0",
        "arn:aws:bedrock:us-east-1:123456789012:custom-model-deployment/meta.llama3",
        "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-3-haiku-20240307-v1:0",
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/global.anthropic.claude-sonnet-4-20250514-v1:0",
    ] {
        let mut deployment = target();
        deployment.upstream_model = model.into();
        for (request, error, text) in super::super::contract::error_tool_result_requests() {
            adapter.execute(&deployment, request).await.unwrap();
            let capture = mock.requests.lock().unwrap();
            let results = &capture.last().unwrap().body["messages"][2]["content"];
            assert_eq!(results[0]["toolResult"]["toolUseId"], "second-call");
            assert_eq!(results[0]["toolResult"]["content"], json!([{"text":text}]));
            assert_eq!(
                results[0]["toolResult"].get("status"),
                error.then_some(&json!("error"))
            );
            assert_eq!(
                results[1]["toolResult"],
                json!({"toolUseId":"first-call","content":[{"text":"unchanged"}]})
            );
        }
    }
}

#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn error_tool_sdk_bedrock(pool: sqlx::PgPool) {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let mut deployment = target();
    deployment.upstream_model = "new-region.anthropic.claude-sonnet-5-v1:0".into();
    super::super::contract::assert_error_tool_sdk(pool, Arc::new(mock.adapter()), deployment).await;
    let capture = mock.requests.lock().unwrap();
    assert_eq!(capture.len(), 6);
    for (captured, (_, error, text)) in capture
        .iter()
        .zip(super::super::contract::error_tool_result_requests())
    {
        let results = &captured.body["messages"][2]["content"];
        assert_eq!(results[0]["toolResult"]["toolUseId"], "second-call");
        assert_eq!(results[0]["toolResult"]["content"], json!([{"text":text}]));
        assert_eq!(
            results[0]["toolResult"].get("status"),
            error.then_some(&json!("error"))
        );
        assert!(results[1]["toolResult"].get("status").is_none());
    }
}

#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn error_tool_sdk_bedrock_model_validation_refusal(pool: sqlx::PgPool) {
    assert_bedrock_tool_status_refusal(pool, "us.meta.llama3-1-70b-instruct-v1:0").await;
}

#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn error_tool_sdk_bedrock_opaque_validation_refusal(pool: sqlx::PgPool) {
    assert_bedrock_tool_status_refusal(
        pool,
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/a1b2c3d4e5",
    )
    .await;
}

#[cfg(feature = "integration-tests")]
async fn assert_bedrock_tool_status_refusal(pool: sqlx::PgPool, upstream_model: &str) {
    let mock = Mock::serve(
        json!({"__type":"ValidationException","message":"unsupported toolResult.status"}),
        vec![],
        400,
    )
    .await;
    let mut deployment = target();
    deployment.upstream_model = upstream_model.into();
    super::super::contract::run_error_tool_sdk(
        pool.clone(),
        Arc::new(mock.adapter()),
        deployment,
        true,
    )
    .await;
    // Priced requests rejected before the stream opens have no usage evidence.
    // Each 100-input + 32-output hold remains available for reconciliation.
    let unknown: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM governance_reservations r
         JOIN inference_executions e ON e.id=r.execution_id
         JOIN monetary_ledger l ON l.execution_id=e.id AND l.kind='unknown'
         WHERE r.state='unknown' AND r.held_microusd=132 AND r.actual_microusd IS NULL
           AND e.state='failed' AND e.error_code='upstream_rejected'
           AND e.input_tokens IS NULL AND e.output_tokens IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unknown, 2);
    let capture = mock.requests.lock().unwrap();
    assert_eq!(capture.len(), 2);
    for (request, operation) in capture.iter().zip(["/converse", "/converse-stream"]) {
        assert!(request.uri.path().ends_with(operation));
        assert_eq!(
            request.body["messages"][2]["content"],
            json!([
                {"toolResult":{"toolUseId":"second-call","status":"error","content":[{"text":"failed"}]}},
                {"toolResult":{"toolUseId":"first-call","content":[{"text":"unchanged"}]}}
            ])
        );
    }
}

/// Official Anthropic SDK -> listening gateway -> Bedrock adapter -> mock, for
/// a model family the gateway has never listed: structured output and the
/// strict tool reach Converse, and Bedrock alone decides model support.
#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn structured_output_sdk_bedrock(pool: sqlx::PgPool) {
    let text = r#"{"answer":"yes"}"#;
    let complete = json!({"output":{"message":{"role":"assistant","content":[{"text":text}]}},"stopReason":"end_turn","usage":{"inputTokens":3,"outputTokens":5,"totalTokens":8},"metrics":{"latencyMs":2}});
    let mut frames = events(false);
    frames[1].1["delta"]["text"] = json!(text);
    let mock = Mock::serve(complete, wire(frames), 200).await;
    let mut deployment = target();
    deployment.upstream_model = "new-region.newvendor.future-model-v9:0".into();
    let attempts = super::super::contract::run_messages_sdk(
        pool,
        Arc::new(mock.adapter()),
        deployment,
        "structured-output.mjs",
        &[],
        true,
    )
    .await;
    assert_eq!(attempts, 2, "one attempt per SDK request");
    let capture = mock.requests.lock().unwrap();
    assert_eq!(capture.len(), 2);
    for (captured, operation) in capture.iter().zip(["/converse", "/converse-stream"]) {
        assert!(captured.uri.path().ends_with(operation));
        let format = &captured.body["outputConfig"]["textFormat"];
        assert_eq!(format["type"], "json_schema");
        let schema: Value = serde_json::from_str(
            format["structure"]["jsonSchema"]["schema"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(schema, super::super::contract::output_schema());
        assert_eq!(
            captured.body["toolConfig"]["tools"][0]["toolSpec"]["strict"],
            true
        );
    }
}

/// A model without structured-output support: Bedrock's validation error
/// reaches the client as Anthropic's 400, with no upstream text.
#[cfg(feature = "integration-tests")]
#[dbtest::test]
async fn structured_output_sdk_bedrock_model_refusal(pool: sqlx::PgPool) {
    let mock = Mock::serve(
        json!({"__type":"ValidationException","message":"SECRET model does not support outputConfig"}),
        vec![],
        400,
    )
    .await;
    let mut deployment = target();
    deployment.upstream_model = "us.meta.llama3-1-70b-instruct-v1:0".into();
    let attempts = super::super::contract::run_messages_sdk(
        pool,
        Arc::new(mock.adapter()),
        deployment,
        "structured-output.mjs",
        &["refused"],
        true,
    )
    .await;
    assert_eq!(attempts, 2);
    assert_eq!(mock.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn inline_images_reach_converse_as_image_blocks_of_their_bytes() {
    use super::super::contract::{IMAGE_URL, image_request_with};
    let data = IMAGE_URL.split_once(',').unwrap().1;
    let blocks = json!([
        {"text":"what is"},
        {"image":{"format":"png","source":{"bytes":data}}},
        {"text":"this?"}
    ]);
    for stream in [false, true] {
        let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
        let adapter = mock.adapter();
        assert!(adapter.capabilities().image_input);
        let request = image_request_with(stream, None);
        assert!(adapter.supports_chat_request(&request));
        adapter.validate_chat_request(&request).unwrap();
        drop(adapter.execute(&target(), request).await.unwrap());
        let captured = mock.requests.lock().unwrap();
        assert_eq!(
            captured[0].body["messages"][0]["content"], blocks,
            "stream {stream}"
        );
    }
}

#[tokio::test]
async fn images_across_turns_keep_their_order_and_blank_text_is_not_sent() {
    let (request, data) = super::super::contract::multi_image_request();
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    mock.adapter().execute(&target(), request).await.unwrap();
    let sent = mock.requests.lock().unwrap()[0].body.clone();
    let image = |k: usize| json!({"image":{"format":"png","source":{"bytes":data[k]}}});
    assert_eq!(sent["system"], json!([{"text":"be brief"}]));
    assert_eq!(
        sent["messages"],
        json!([
            {"role":"user","content":[{"text":"compare"}, image(0), image(1)]},
            {"role":"assistant","content":[{"text":"two images"}]},
            {"role":"user","content":[image(2), {"text":"and this"}]}
        ])
    );
}

#[tokio::test]
async fn the_request_size_counts_images_and_everything_else_sent() {
    use base64::Engine as _;
    // Five images of 3.7 MB pass one by one (under 3.75 MB) but are 24.7 MB
    // of base64 together; four are 19.7 MB, under the 20 MB, until a tool
    // description takes the rest of it.
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(3_700_000, 7);
    let url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    let request = |count: usize, description: usize| {
        let mut images = crate::protocols::image_input::Images::default();
        let mut r = super::super::contract::image_request_with(false, None);
        r.messages[0].content = Some(Content::Parts(
            (0..count)
                .map(|_| Part::Image(images.take(url.clone(), None, "image").unwrap()))
                .collect(),
        ));
        r.tools = vec![FunctionTool {
            name: "lookup".into(),
            description: Some("d".repeat(description)),
            parameters: serde_json::json!({"type":"object"}),
            strict: None,
        }];
        r
    };
    let adapter = BedrockAdapter::for_test("http://127.0.0.1:9");
    let refused = Err(InferenceError::InvalidField("image"));
    assert_eq!(adapter.validate_chat_request(&request(5, 1)), refused);
    assert_eq!(adapter.validate_chat_request(&request(4, 1)), Ok(()));
    assert_eq!(adapter.validate_chat_request(&request(4, 300_000)), refused);
    // Refused in the encoder too, before anything is sent.
    assert_eq!(
        adapter.execute(&target(), request(4, 300_000)).await.err(),
        refused.err()
    );
}

#[tokio::test]
async fn images_converse_cannot_carry_are_refused_before_anything_is_sent() {
    use super::super::contract::image_request_with;
    // Nothing listens at the target, so a request that was sent would fail
    // with a transport error instead of the named field.
    let adapter = BedrockAdapter::for_test("http://127.0.0.1:9");
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
}

/// An image's bytes reach no log line, even with every target at TRACE:
/// the SDK as locked writes a request's body nowhere (its "transmitting
/// request" event shows a boxed body, `aws_sigv4` redacts it unless
/// `LOG_SIGNABLE_BODY=true`), and the process's log policy
/// (`log_policy::allowed`, installed by `main` beside `RUST_LOG`) never
/// enables TRACE from the `aws_*` crates, where those events live.
#[tokio::test]
async fn an_image_reaches_no_log_line_whatever_rust_log_enables() {
    use base64::Engine as _;
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::{EnvFilter, fmt::MakeWriter, layer::SubscriberExt as _};
    #[derive(Clone, Default)]
    struct Lines(Arc<StdMutex<Vec<u8>>>);
    impl std::io::Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Lines {
        type Writer = Lines;
        fn make_writer(&'a self) -> Lines {
            self.clone()
        }
    }
    let capture = |policy: bool| {
        let lines = Lines::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(lines.clone())
            .with_ansi(false)
            .with_env_filter(EnvFilter::new("trace"))
            .finish()
            .with(tracing_subscriber::filter::filter_fn(move |m| {
                !policy || crate::log_policy::allowed(m)
            }));
        (lines, subscriber)
    };
    let text = |lines: &Lines| String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    while bytes.len() < 4096 {
        bytes.extend_from_slice(b"QwImageCanaryQ");
    }
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let canary = &data[40..80];
    let image = crate::protocols::image_input::Images::default()
        .take(format!("data:image/png;base64,{data}"), None, "image")
        .unwrap();
    let mut request = super::super::contract::image_request_with(false, None);
    request.messages[0].content = Some(Content::Parts(vec![
        Part::Text("what is this?".into()),
        Part::Image(image),
    ]));
    // Served and refused by the upstream, complete and streamed.
    for (status, stream) in [(200, false), (400, false), (200, true), (400, true)] {
        for policy in [false, true] {
            let mock = Mock::serve(complete(false), wire(events(false)), status).await;
            let (lines, subscriber) = capture(policy);
            let (adapter, deployment) = (mock.adapter(), target());
            let mut request = request.clone();
            request.stream = stream;
            let result = async {
                let output = adapter.execute(&deployment, request).await?;
                if let ProviderOutput::Stream(events) = output {
                    // Read to its end under the same subscriber.
                    let events: Vec<_> = events.collect().await;
                    events
                        .into_iter()
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                }
                Ok::<_, InferenceError>(())
            }
            .with_subscriber(subscriber)
            .await;
            assert_eq!(
                result.is_ok(),
                status == 200,
                "status {status} stream {stream}"
            );
            if let Err(error) = &result {
                assert!(!format!("{error:?}{error}").contains(canary));
            }
            assert!(
                mock.requests.lock().unwrap()[0]
                    .body
                    .to_string()
                    .contains(canary)
            );
            let logged = text(&lines);
            // The capture was live: the SDK's TRACE events without the
            // policy, its DEBUG events with it.
            assert_eq!(logged.contains("transmitting request"), !policy);
            assert!(logged.contains("beginning attempt"), "policy {policy}");
            assert!(
                !logged.contains(canary) && !logged.contains("ImageCanary"),
                "status {status} policy {policy}"
            );
        }
    }
    // Those events live at TRACE under the SDK's targets, which the policy
    // never enables: an event there carrying the bytes is dropped with the
    // policy and written without it; DEBUG under the same target is kept.
    for policy in [false, true] {
        let (lines, subscriber) = capture(policy);
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "aws_sigv4::http_request::sign", body = canary, "signing request");
            tracing::trace!(target: "aws_smithy_runtime::client::orchestrator", body = canary, "transmitting request");
            tracing::debug!(target: "aws_smithy_runtime::client::orchestrator", "beginning attempt");
        });
        let logged = text(&lines);
        assert_eq!(logged.contains(canary), !policy, "policy {policy}");
        assert!(logged.contains("beginning attempt"));
    }
}
