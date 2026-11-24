//! Reasoning models on local servers. Fixtures are the exact field shapes a
//! live SGLang server (Qwen with a reasoning parser) returned on 2026-10-09:
//! `reasoning_content` beside `content`, an integer `matched_stop` on a stop
//! finish, and top-level `usage.reasoning_tokens`.
use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::Response,
};
use futures_util::StreamExt;
use serde_json::{Value, json};

struct NoSecrets;
impl SecretResolver for NoSecrets {
    fn resolve(&self, _: &str) -> Result<super::super::secrets::Secret> {
        Err(InferenceError::Configuration)
    }
}

async fn serve(complete: Value, frames: Vec<Value>) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().fallback(move |bytes: Bytes| {
        let (complete, frames) = (complete.clone(), frames.clone());
        async move {
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            if request["stream"] == true {
                let mut body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
                body.push_str("data: [DONE]\n\n");
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(body))
                    .unwrap()
            } else {
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(complete.to_string()))
                    .unwrap()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

fn target(profile: Profile, base: &str) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: profile.id().into(),
        upstream_model: "qwen".into(),
        credential_ref: "none".into(),
        endpoint: Some(base.into()),
        region: None,
        supported_protocols: vec!["chat_completions".into()],
    }
}

fn request(stream: bool) -> ChatRequest {
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
        max_output_tokens: Some(1024),
        stream,
    }
}

fn sglang_complete(matched_stop: Value, reasoning: Value) -> Value {
    json!({"id":"x","object":"chat.completion","created":1,"model":"qwen3.8-27b",
        "choices":[{"index":0,"message":{"role":"assistant","content":"OK","reasoning_content":reasoning,"tool_calls":null},
            "logprobs":null,"finish_reason":"stop","matched_stop":matched_stop}],
        "usage":{"prompt_tokens":59,"total_tokens":83,"completion_tokens":24,"prompt_tokens_details":null,"reasoning_tokens":21},
        "metadata":{"weight_version":"default"}})
}

fn sglang_frames() -> Vec<Value> {
    vec![
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_content":null},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":"The user wants"},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[{"index":0,"delta":{"role":null,"content":"OK","reasoning_content":null},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null},"logprobs":null,"finish_reason":"stop","matched_stop":248046}],"usage":null}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[],"usage":{"prompt_tokens":59,"total_tokens":99,"completion_tokens":40,"prompt_tokens_details":null,"reasoning_tokens":37}}),
    ]
}

#[tokio::test]
async fn sglang_reasoning_trace_is_dropped_and_its_tokens_stay_in_output() {
    let (base, task) = serve(
        sglang_complete(json!(248046), json!("The user wants OK.")),
        sglang_frames(),
    )
    .await;
    for profile in [Profile::Sglang, Profile::Vllm, Profile::OpenAiCompatible] {
        let adapter = LocalAdapter::new(
            profile,
            Arc::new(NoSecrets),
            Arc::new(ApprovedEndpoints::for_test(&base)),
        );
        let target = target(profile, &base);
        let ProviderOutput::Complete(response) =
            adapter.execute(&target, request(false)).await.unwrap()
        else {
            panic!("expected a complete response")
        };
        assert_eq!(response.content.as_deref(), Some("OK"));
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert_eq!(response.usage.output_tokens, Some(24));
        assert_eq!(response.usage.reasoning_tokens, Some(21));

        let ProviderOutput::Stream(stream) = adapter.execute(&target, request(true)).await.unwrap()
        else {
            panic!("expected a stream")
        };
        let events: Vec<_> = stream.collect().await;
        let mut text = String::new();
        let mut usage = None;
        for event in &events {
            match event.as_ref().expect("stream must decode") {
                ChatEvent::Delta { text: Some(t), .. } => text.push_str(t),
                ChatEvent::Usage(u) => usage = Some(*u),
                _ => {}
            }
        }
        // Only `content` reaches the client; the trace never does.
        assert_eq!(text, "OK");
        assert!(matches!(events.last(), Some(Ok(ChatEvent::Done))));
        let usage = usage.unwrap();
        assert_eq!(usage.output_tokens, Some(40));
        assert_eq!(usage.reasoning_tokens, Some(37));
    }
    task.abort();
}

#[tokio::test]
async fn local_extras_keep_their_scalar_shapes() {
    for (matched_stop, reasoning) in [
        (json!({"token": 1}), json!("trace")),
        (json!([1]), json!("trace")),
        (json!(1), json!({"text": "trace"})),
        (json!(1), json!(7)),
    ] {
        let (base, task) = serve(sglang_complete(matched_stop, reasoning), vec![]).await;
        let adapter = LocalAdapter::new(
            Profile::Sglang,
            Arc::new(NoSecrets),
            Arc::new(ApprovedEndpoints::for_test(&base)),
        );
        let result = adapter
            .execute(&target(Profile::Sglang, &base), request(false))
            .await;
        assert!(matches!(result, Err(InferenceError::InvalidUpstream)));
        task.abort();
    }
}

#[test]
fn cloud_openai_decoding_still_rejects_reasoning_fields() {
    let value = sglang_complete(json!(248046), json!("trace"));
    assert!(super::super::openai::decode_complete(&value).is_err());
    let mut value = sglang_complete(Value::Null, Value::Null);
    assert!(super::super::openai::decode_complete(&value).is_ok());
    value["choices"][0]["message"]["reasoning_content"] = json!("trace");
    assert!(super::super::openai::decode_complete(&value).is_err());
}

/// Messages clients receive a local reasoning parser's trace as reasoning
/// (rendered as unsigned `thinking`), complete and streamed, whichever of the
/// two field names the server uses. Reasoning controls have no pinned local
/// dialect and never reach the wire; other clients still never get the trace.
#[tokio::test]
async fn messages_receive_local_reasoning_text_under_either_field_name() {
    use super::super::contract::{messages_request, reasoning_and_text};
    let rename = |mut value: Value| {
        let text = value.to_string().replace("reasoning_content", "reasoning");
        value = serde_json::from_str(&text).unwrap();
        value
    };
    for field in ["reasoning_content", "reasoning"] {
        let mut complete = sglang_complete(json!(248046), json!("The user wants OK."));
        let mut frames = sglang_frames();
        if field == "reasoning" {
            complete = rename(complete);
            frames = frames.into_iter().map(rename).collect();
        }
        let (base, task) = serve(complete, frames).await;
        for profile in [
            Profile::Sglang,
            Profile::Vllm,
            Profile::Ollama,
            Profile::OpenAiCompatible,
        ] {
            let adapter = LocalAdapter::new(
                profile,
                Arc::new(NoSecrets),
                Arc::new(ApprovedEndpoints::for_test(&base)),
            );
            let target = target(profile, &base);
            let controls = json!({"thinking":{"type":"enabled","budget_tokens":1024},
                "output_config":{"effort":"high"}});
            let messages = messages_request(controls.clone());
            assert!(adapter.validate_chat_request(&messages).is_ok());
            let wire = adapter.encode_chat(&target, &messages).unwrap();
            for absent in ["reasoning_effort", "reasoning", "thinking", "output_config"] {
                assert!(wire.get(absent).is_none(), "{absent}");
            }
            let output = adapter.execute(&target, messages).await.unwrap();
            assert_eq!(
                reasoning_and_text(output).await,
                (Some("The user wants OK.".into()), Some("OK".into())),
                "{field} {}",
                profile.id()
            );
            let mut streamed = controls.clone();
            streamed["stream"] = json!(true);
            let output = adapter
                .execute(&target, messages_request(streamed))
                .await
                .unwrap();
            assert_eq!(
                reasoning_and_text(output).await,
                (Some("The user wants".into()), Some("OK".into()))
            );
            for stream in [false, true] {
                let output = adapter.execute(&target, request(stream)).await.unwrap();
                assert_eq!(reasoning_and_text(output).await.0, None);
                // A Messages client that did not ask for thinking gets none.
                let output = adapter
                    .execute(&target, messages_request(json!({"stream":stream})))
                    .await
                    .unwrap();
                assert_eq!(reasoning_and_text(output).await, (None, Some("OK".into())));
            }
        }
        task.abort();
    }
}

/// A server sending both names must agree; two different traces are invalid.
#[tokio::test]
async fn messages_local_reasoning_fields_must_agree() {
    use super::super::contract::{messages_request, reasoning_and_text};
    for (other, valid) in [
        (json!("same"), true),
        (Value::Null, true),
        (json!("different"), false),
    ] {
        let mut complete = sglang_complete(json!(1), json!("same"));
        complete["choices"][0]["message"]["reasoning"] = other;
        let (base, task) = serve(complete, vec![]).await;
        let adapter = LocalAdapter::new(
            Profile::Vllm,
            Arc::new(NoSecrets),
            Arc::new(ApprovedEndpoints::for_test(&base)),
        );
        let result = adapter
            .execute(
                &target(Profile::Vllm, &base),
                messages_request(json!({"thinking":{"type":"adaptive"}})),
            )
            .await;
        if valid {
            assert_eq!(
                reasoning_and_text(result.unwrap()).await.0.as_deref(),
                Some("same")
            );
        } else {
            assert!(matches!(result, Err(InferenceError::InvalidUpstream)));
        }
        task.abort();
    }
}
