//! Converse structured output, strict tools and returned reasoning content.
//! Model support is Bedrock's decision (`outputConfig` and `toolSpec.strict`
//! are first-class Converse fields): the adapter sends them for every model
//! and a model that cannot honour them fails upstream with a validation error,
//! so no gateway model list can refuse a family released later.
use super::*;

fn messages(extra: Value) -> ChatRequest {
    let mut body = json!({
        "model":"public-model",
        "max_tokens":256,
        "messages":[{"role":"user","content":"hello"}],
    });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    crate::protocols::messages::batch_request(body).unwrap()
}
fn schema() -> Value {
    json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false})
}

#[tokio::test]
async fn structured_output_and_strict_tools_reach_converse_and_converse_stream() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let adapter = mock.adapter();
    let extra = json!({
        "output_config":{"format":{"type":"json_schema","schema":schema()}},
        "tools":[
            {"name":"strict_tool","input_schema":schema(),"strict":true},
            {"name":"loose_tool","input_schema":{"type":"object"},"strict":false},
            {"name":"plain_tool","input_schema":{"type":"object"}},
        ],
    });
    for stream in [false, true] {
        let mut request = messages(extra.clone());
        request.stream = stream;
        adapter.validate_chat_request(&request).unwrap();
        match adapter.execute(&target(), request).await.unwrap() {
            ProviderOutput::Complete(response) => {
                assert_eq!(response.content.as_deref(), Some("Hello"));
            }
            ProviderOutput::Stream(mut events) => {
                while let Some(event) = events.next().await {
                    if matches!(event.unwrap(), ChatEvent::Done) {
                        break;
                    }
                }
            }
        }
    }
    let capture = mock.requests.lock().unwrap();
    assert_eq!(capture.len(), 2);
    for (captured, operation) in capture.iter().zip(["/converse", "/converse-stream"]) {
        assert!(captured.uri.path().ends_with(operation));
        let format = &captured.body["outputConfig"]["textFormat"];
        assert_eq!(format["type"], "json_schema");
        // Converse takes the schema as a JSON string; it must be the client's
        // schema exactly, with no gateway name or description added.
        let structure = &format["structure"]["jsonSchema"];
        assert_eq!(
            structure.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["schema"]
        );
        let sent: Value = serde_json::from_str(structure["schema"].as_str().unwrap()).unwrap();
        assert_eq!(sent, schema());
        assert_eq!(
            captured.body["outputConfig"].as_object().unwrap().len(),
            1,
            "no effort or other output control is invented"
        );
        let tools = captured.body["toolConfig"]["tools"].as_array().unwrap();
        assert_eq!(tools[0]["toolSpec"]["strict"], true);
        // `strict:false` is Converse's default; it is not sent, so a model
        // without structured-output support is not refused for it.
        assert!(tools[1]["toolSpec"].get("strict").is_none());
        assert!(tools[2]["toolSpec"].get("strict").is_none());
    }
}

#[tokio::test]
async fn chat_strict_tools_reach_converse() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let mut request = request();
    request.tools.push(tool());
    request.tools[0].strict = Some(true);
    mock.adapter().validate_chat_request(&request).unwrap();
    mock.adapter().execute(&target(), request).await.unwrap();
    let capture = mock.requests.lock().unwrap();
    assert_eq!(
        capture[0].body["toolConfig"]["tools"][0]["toolSpec"]["strict"],
        true
    );
    assert!(capture[0].body.get("outputConfig").is_none());
}

#[tokio::test]
async fn requests_without_structured_controls_send_no_output_config() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let adapter = mock.adapter();
    adapter
        .execute(
            &target(),
            messages(json!({"output_config":{"effort":"high"}})),
        )
        .await
        .unwrap();
    adapter.execute(&target(), request()).await.unwrap();
    for captured in mock.requests.lock().unwrap().iter() {
        assert!(captured.body.get("outputConfig").is_none());
    }
}

fn reasoning_complete() -> Value {
    json!({"output":{"message":{"role":"assistant","content":[
        {"reasoningContent":{"reasoningText":{"text":"think first","signature":"opaque-signature"}}},
        {"reasoningContent":{"redactedContent":"b3BhcXVl"}},
        {"text":"Hello"},
    ]}},"stopReason":"end_turn","usage":{"inputTokens":3,"outputTokens":5,"totalTokens":8},"metrics":{"latencyMs":2}})
}
fn reasoning_events() -> Vec<(&'static str, Value)> {
    vec![
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"think "}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"first"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"opaque-signature"}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"reasoningContent":{"redactedContent":"b3BhcXVl"}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":1})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":2,"delta":{"text":"Hello"}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":2})),
        ("messageStop", json!({"stopReason":"end_turn"})),
        (
            "metadata",
            json!({"usage":{"inputTokens":3,"outputTokens":5,"totalTokens":8},"metrics":{"latencyMs":2}}),
        ),
    ]
}

/// Models that reason by default return reasoning content without being asked.
/// It is decoded, bounded with the reply and never fails the attempt. Only a
/// Messages stream carries its text (the codec withholds it unless thinking
/// was requested); signatures and redacted content are never returned, since
/// translated routes return unsigned thinking only.
#[tokio::test]
async fn returned_reasoning_content_is_decoded_for_both_protocols() {
    let mock = Mock::serve(reasoning_complete(), wire(reasoning_events()), 200).await;
    let adapter = mock.adapter();
    for messages_request in [false, true] {
        let base = if messages_request {
            messages(json!({}))
        } else {
            request()
        };
        let ProviderOutput::Complete(response) =
            adapter.execute(&target(), base.clone()).await.unwrap()
        else {
            panic!("complete request");
        };
        assert_eq!(response.content.as_deref(), Some("Hello"));
        // Thinking cannot be requested on Bedrock, so the complete reply never
        // carries it (`messages::thinking_display`).
        assert!(response.reasoning.is_none());
        let mut streamed = base;
        streamed.stream = true;
        let ProviderOutput::Stream(mut events) =
            adapter.execute(&target(), streamed).await.unwrap()
        else {
            panic!("stream request");
        };
        let (mut reasoning, mut text, mut done) = (String::new(), String::new(), false);
        while let Some(event) = events.next().await {
            match event.unwrap() {
                ChatEvent::Reasoning(piece) => reasoning.push_str(&piece),
                ChatEvent::Delta {
                    text: Some(piece), ..
                } => text.push_str(&piece),
                ChatEvent::Done => done = true,
                _ => {}
            }
        }
        assert!(done);
        assert_eq!(text, "Hello");
        assert_eq!(reasoning, if messages_request { "think first" } else { "" });
    }
}

#[test]
fn complete_reasoning_text_is_kept_for_messages_and_bounded() {
    let output = |text: &str| {
        aws_sdk_bedrockruntime::operation::converse::ConverseOutput::builder()
            .output(aws::ConverseOutput::Message(
                aws::Message::builder()
                    .role(aws::ConversationRole::Assistant)
                    .content(aws::ContentBlock::ReasoningContent(
                        aws::ReasoningContentBlock::ReasoningText(
                            aws::ReasoningTextBlock::builder()
                                .text(text)
                                .signature("opaque")
                                .build()
                                .unwrap(),
                        ),
                    ))
                    .content(aws::ContentBlock::Text("Hello".into()))
                    .build()
                    .unwrap(),
            ))
            .stop_reason(aws::StopReason::EndTurn)
            .build()
            .unwrap()
    };
    let response = decode_complete_limit(output("think"), 1024, true).unwrap();
    assert_eq!(response.reasoning.as_deref(), Some("think"));
    assert_eq!(response.content.as_deref(), Some("Hello"));
    let response = decode_complete_limit(output("think"), 1024, false).unwrap();
    assert!(response.reasoning.is_none());
    // Reasoning shares the reply's output bound.
    assert!(matches!(
        decode_complete_limit(output(&"x".repeat(1024)), 1024, true),
        Err(InferenceError::InvalidUpstream)
    ));
}

#[test]
fn reasoning_and_text_never_share_a_stream_block() {
    let mut state = stream::State::for_test(true);
    state
        .push(aws::ConverseStreamOutput::MessageStart(
            aws::MessageStartEvent::builder()
                .role(aws::ConversationRole::Assistant)
                .build()
                .unwrap(),
        ))
        .unwrap();
    let delta = |index: i32, delta: aws::ContentBlockDelta| {
        aws::ConverseStreamOutput::ContentBlockDelta(
            aws::ContentBlockDeltaEvent::builder()
                .content_block_index(index)
                .delta(delta)
                .build()
                .unwrap(),
        )
    };
    state
        .push(delta(
            0,
            aws::ContentBlockDelta::ReasoningContent(aws::ReasoningContentBlockDelta::Text(
                "think".into(),
            )),
        ))
        .unwrap();
    assert!(matches!(
        state.push(delta(0, aws::ContentBlockDelta::Text("answer".into()))),
        Err(InferenceError::InvalidUpstream)
    ));
    let mut state = stream::State::for_test(true);
    state
        .push(aws::ConverseStreamOutput::MessageStart(
            aws::MessageStartEvent::builder()
                .role(aws::ConversationRole::Assistant)
                .build()
                .unwrap(),
        ))
        .unwrap();
    state
        .push(delta(1, aws::ContentBlockDelta::Text("answer".into())))
        .unwrap();
    assert!(matches!(
        state.push(delta(
            1,
            aws::ContentBlockDelta::ReasoningContent(aws::ReasoningContentBlockDelta::Text(
                "think".into(),
            )),
        )),
        Err(InferenceError::InvalidUpstream)
    ));
}
