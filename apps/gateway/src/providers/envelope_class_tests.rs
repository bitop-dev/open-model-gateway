//! #665: a provider error object inside a non-stream HTTP 200 JSON body,
//! across the adapters and endpoints that read one, against a mock upstream
//! that answers 200 with the case's body.
//!
//! - **A clean envelope** (the error and nothing the provider may have
//!   produced) has the class of the same error as an HTTP response: for every
//!   documented name, each client protocol, the class equals both a real HTTP
//!   error response with that name's status and the hand-written oracle
//!   (`stream_class_tests::http_class`). An unrecognised error is
//!   `UpstreamUnavailable`, never a success. This is the in-stream contract
//!   (`stream_class_tests`) applied to a complete body.
//! - **An envelope next to output or nonzero usage** may describe a request
//!   the provider processed: it fails as `InvalidUpstream` (never retried or
//!   failed over), and a valid usage object is kept as evidence.
//!
//! The engine, batch and settlement consequences run on PostgreSQL in
//! `stream_class_e2e_tests` and `jobs::batch_tests`.
use super::stream_class_tests::{
    KINDS, Kind, NAMES, Step, http_class, request, responses_complete, run, serve, upstream,
};
use crate::inference::{error::InferenceError, evidence, types::*};
use serde_json::{Value, json};

/// The envelope a non-stream body of `kind` carries for `error`.
pub(crate) fn envelope(kind: Kind, error: Value) -> Value {
    match kind {
        Kind::Anthropic => json!({"type":"error","error":error}),
        Kind::Responses => json!({"id":"resp_private","object":"response","status":"failed",
            "error":error,"output":[],"usage":null}),
        _ => json!({"error":error}),
    }
}
/// The error object `kind` uses for the documented `name`.
pub(crate) fn named(kind: Kind, name: &str) -> Value {
    match kind {
        Kind::Anthropic => json!({"type":name,"message":"secret provider text"}),
        _ => json!({"message":"secret provider text","param":null,"code":name}),
    }
}
/// `body` with the output a successful answer of `kind` carries.
pub(crate) fn with_content(kind: Kind, mut body: Value) -> Value {
    match kind {
        Kind::Anthropic => body["content"] = json!([{"type":"text","text":"Hello"}]),
        Kind::Responses => body["output"] = responses_complete()["output"].clone(),
        _ => {
            body["choices"] = json!([{"index":0,"message":{"role":"assistant","content":"Hello"},
                "finish_reason":"stop"}]);
        }
    }
    body
}
/// `body` with a usage object of `kind` reporting 3 input and 2 output tokens
/// (or all zeros).
pub(crate) fn with_usage(kind: Kind, mut body: Value, zero: bool) -> Value {
    let (input, output) = if zero { (0, 0) } else { (3, 2) };
    body["usage"] = match kind {
        Kind::Anthropic => json!({"input_tokens":input,"output_tokens":output}),
        Kind::Responses => {
            json!({"input_tokens":input,"output_tokens":output,"total_tokens":input+output})
        }
        _ => json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output}),
    };
    body
}

/// One non-stream request of `kind` whose upstream answers 200 with `body`:
/// the class it failed with (`None` for a success) and the usage evidence the
/// adapter reported for the failed attempt.
pub(crate) async fn complete(
    kind: Kind,
    messages: bool,
    body: &Value,
) -> (Option<InferenceError>, Option<Usage>) {
    let (base, _server) = serve(upstream(
        200,
        "application/json",
        vec![Step::Bytes(body.to_string().into_bytes())],
    ))
    .await;
    let adapter = kind.adapter(&base);
    let mut chat = request(messages);
    chat.stream = false;
    let (output, evidence) =
        evidence::capture(adapter.execute_protocol(&kind.target(&base), chat, kind.protocol()))
            .await;
    match output {
        Ok(ProviderOutput::Complete(_)) => (None, evidence),
        Ok(ProviderOutput::Stream(_)) => panic!("a complete response was requested"),
        Err(error) => (Some(error), evidence),
    }
}

/// For every documented name and each client protocol: the class of the 200
/// envelope equals the class of the HTTP response with that name's status,
/// and both equal the oracle. Nothing is reported as usage.
#[tokio::test]
async fn a_clean_200_error_envelope_has_the_class_of_the_same_http_error() {
    let mut problems = Vec::new();
    for kind in KINDS {
        for &messages in kind.clients() {
            for (name, status) in NAMES {
                let expected = http_class(kind, messages, name, *status);
                let body = envelope(kind, named(kind, name));
                let http = run(
                    kind,
                    messages,
                    *status,
                    "application/json",
                    vec![Step::Bytes(body.to_string().into_bytes())],
                )
                .await
                .class();
                let (class, usage) = complete(kind, messages, &body).await;
                let at = format!("{kind:?} messages={messages} {name}");
                if class != Some(expected) || http != Some(expected) {
                    problems.push(format!(
                        "{at}: 200 {class:?}, http {http:?}, expected {expected:?}"
                    ));
                }
                if usage.is_some() {
                    problems.push(format!("{at}: usage {usage:?}"));
                }
                // All-zero usage is not evidence of processing.
                let (class, _) = complete(kind, messages, &with_usage(kind, body, true)).await;
                if class != Some(expected) {
                    problems.push(format!("{at} with zero usage: {class:?}"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The owner's 2026-11-23 decision in one place: a recognised rate-limit or
/// quota error in a clean 200 is `Busy`, as an HTTP 429 is, on every adapter
/// and client protocol.
#[tokio::test]
async fn a_rate_limit_or_quota_error_in_a_clean_200_is_busy() {
    for kind in KINDS {
        for &messages in kind.clients() {
            for name in [
                "rate_limit_error",
                "rate_limit_exceeded",
                "insufficient_quota",
            ] {
                let (class, _) = complete(kind, messages, &envelope(kind, named(kind, name))).await;
                assert_eq!(
                    class,
                    Some(InferenceError::Busy),
                    "{kind:?} messages={messages} {name}"
                );
            }
            let numeric = match kind {
                Kind::Anthropic => json!({"type":"error","error":{"code":429}}),
                _ => envelope(kind, json!({"code":429,"message":"secret provider text"})),
            };
            let (class, _) = complete(kind, messages, &numeric).await;
            assert_eq!(class, Some(InferenceError::Busy), "{kind:?} numeric 429");
        }
    }
}

/// A name nobody documents, a case-changed name and an error with no readable
/// name: the provider failed, in the class that counts against routing
/// health, and never succeeded.
#[tokio::test]
async fn an_unrecognised_200_error_envelope_is_an_unavailable_provider() {
    for kind in KINDS {
        for &messages in kind.clients() {
            let errors = [
                named(kind, "brand_new_error_name"),
                named(kind, "Rate_Limit_Error"),
                json!({"message":"rate_limit_error prompt is too long"}),
                json!("rate_limit_error"),
            ];
            for error in errors {
                let (class, _) = complete(kind, messages, &envelope(kind, error.clone())).await;
                assert_eq!(
                    class,
                    Some(InferenceError::UpstreamUnavailable),
                    "{kind:?} messages={messages} {error}"
                );
            }
        }
    }
}

/// An error next to output, or next to nonzero usage, is never a retryable
/// class whatever its name: the provider may have processed the request. A
/// valid usage object is kept as the failed attempt's evidence.
#[tokio::test]
async fn an_error_next_to_output_or_usage_is_an_invalid_response_that_keeps_its_usage() {
    let mut problems = Vec::new();
    for kind in KINDS {
        for &messages in kind.clients() {
            for name in [
                "rate_limit_exceeded",
                "server_error",
                "overloaded_error",
                "invalid_request_error",
            ] {
                let clean = envelope(kind, named(kind, name));
                let at = format!("{kind:?} messages={messages} {name}");
                let (class, usage) =
                    complete(kind, messages, &with_content(kind, clean.clone())).await;
                if class != Some(InferenceError::InvalidUpstream) || usage.is_some() {
                    problems.push(format!("{at} with output: {class:?} {usage:?}"));
                }
                for body in [
                    with_usage(kind, clean.clone(), false),
                    with_usage(kind, with_content(kind, clean.clone()), false),
                ] {
                    let (class, usage) = complete(kind, messages, &body).await;
                    let tokens = usage.map(|u| (u.input_tokens, u.output_tokens));
                    if class != Some(InferenceError::InvalidUpstream)
                        || tokens.is_none_or(|(_, output)| output != Some(2))
                    {
                        problems.push(format!("{at} with usage {body}: {class:?} {tokens:?}"));
                    }
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Numeric codes are statuses, and an explicit code wins over the type, as
/// in a stream.
#[tokio::test]
async fn numeric_codes_and_string_precedence_match_http_statuses() {
    for kind in KINDS {
        for &messages in kind.clients() {
            for (code, typ, status) in [
                (json!(408), "rate_limit_error", 408),
                (json!(429), "invalid_request_error", 429),
                (json!(503), "invalid_request_error", 503),
                (json!(524), "invalid_request_error", 524),
                (json!("400"), "rate_limit_error", 400),
                (json!("429"), "invalid_request_error", 429),
                (json!(""), "invalid_request_error", 400),
                (json!("new_name"), "invalid_request_error", 400),
            ] {
                let body = envelope(kind, json!({"code":code,"type":typ}));
                let (class, _) = complete(kind, messages, &body).await;
                assert_eq!(
                    class,
                    Some(http_class(kind, messages, "", status)),
                    "{kind:?} messages={messages} code={code}"
                );
            }
        }
    }
}

/// The non-chat endpoints that read a 200 JSON body: (adapter, endpoint).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endpoint {
    Embeddings,
    Images,
    Transcription,
    Speech,
    Rerank,
    Systemone,
}
const ENDPOINTS: &[(Kind, Endpoint)] = &[
    (Kind::OpenAi, Endpoint::Embeddings),
    (Kind::OpenAi, Endpoint::Images),
    (Kind::OpenAi, Endpoint::Transcription),
    (Kind::OpenAi, Endpoint::Speech),
    (Kind::OpenRouter, Endpoint::Embeddings),
    (Kind::OpenRouter, Endpoint::Images),
    (Kind::OpenRouter, Endpoint::Transcription),
    (Kind::OpenRouter, Endpoint::Speech),
    (Kind::OpenRouter, Endpoint::Rerank),
    (Kind::OpenRouter, Endpoint::Systemone),
    (Kind::Local, Endpoint::Embeddings),
];
impl Endpoint {
    /// The member that carries this endpoint's output, and a non-empty value
    /// for it (`None`: the JSON answer carries no output).
    fn output(self) -> Option<(&'static str, Value)> {
        match self {
            Self::Embeddings | Self::Images => Some((
                "data",
                json!([{"object":"embedding","index":0,"embedding":[0.1]}]),
            )),
            Self::Transcription => Some(("text", json!("Hello"))),
            Self::Rerank => Some(("results", json!([{"index":0,"relevance_score":0.5}]))),
            Self::Systemone => Some(("answers", json!({"is_q":true}))),
            Self::Speech => None,
        }
    }
    /// A usage object of this endpoint on `kind` reporting 3 input tokens.
    fn usage(self, kind: Kind) -> Value {
        match (self, kind) {
            (Self::Images, Kind::OpenRouter) => {
                json!({"prompt_tokens":3,"completion_tokens":2,"total_tokens":5})
            }
            (Self::Images | Self::Transcription | Self::Systemone, _) => {
                json!({"input_tokens":3,"output_tokens":2,"total_tokens":5})
            }
            _ => json!({"prompt_tokens":3,"total_tokens":3}),
        }
    }
}

/// One request of `endpoint` on `kind` whose upstream answers 200 with `body`.
async fn call(
    kind: Kind,
    endpoint: Endpoint,
    body: &Value,
) -> (Option<InferenceError>, Option<Usage>) {
    use crate::inference::{audio::*, images::*};
    let (base, _server) = serve(upstream(
        200,
        "application/json",
        vec![Step::Bytes(body.to_string().into_bytes())],
    ))
    .await;
    let adapter = kind.adapter(&base);
    let mut target = kind.target(&base);
    if endpoint == Endpoint::Images && kind == Kind::OpenAi {
        target.upstream_model = "gpt-image-1".into();
    }
    let (result, usage) = evidence::capture(async {
        match endpoint {
            Endpoint::Embeddings => adapter
                .execute_embeddings(
                    &target,
                    EmbeddingRequest {
                        model: "public".into(),
                        input: vec!["hello".into()],
                        dimensions: None,
                    },
                )
                .await
                .map(drop),
            Endpoint::Images => adapter
                .execute_images(
                    &target,
                    ImageRequest {
                        model: "public".into(),
                        prompt: "a red dot".into(),
                        n: 1,
                        size: None,
                        quality: None,
                        seed: None,
                        max_response_bytes: 1024 * 1024,
                    },
                )
                .await
                .map(drop),
            Endpoint::Transcription => adapter
                .execute_audio_transcription(
                    &target,
                    TranscriptionRequest::new(
                        "public".into(),
                        std::sync::Arc::from(crate::inference::audio::tests_support::wav(850)),
                        AudioFormat::Wav,
                        None,
                        None,
                        None,
                        TranscriptionFormat::Json,
                    ),
                )
                .await
                .map(drop),
            Endpoint::Speech => adapter
                .execute_audio_speech(
                    &target,
                    SpeechRequest {
                        model: "public".into(),
                        input: "Hi there".into(),
                        voice: "alloy".into(),
                        response_format: SpeechFormat::Mp3,
                        speed: None,
                    },
                )
                .await
                .map(drop),
            Endpoint::Rerank => adapter
                .execute_rerank(
                    &target,
                    RerankRequest {
                        model: "public".into(),
                        query: "cat".into(),
                        documents: vec!["kitten".into()],
                        top_n: None,
                    },
                )
                .await
                .map(drop),
            Endpoint::Systemone => adapter
                .execute_systemone(
                    &target,
                    SystemoneRequest {
                        model: "public".into(),
                        state: json!("hello"),
                        questions: std::collections::BTreeMap::from([(
                            "is_q".to_owned(),
                            Question {
                                kind: QuestionKind::Noul,
                                instructions: json!("Is it a greeting?"),
                                criteria: None,
                            },
                        )]),
                    },
                )
                .await
                .map(drop),
        }
    })
    .await;
    (result.err(), usage)
}

/// Embeddings, images, audio, rerank and System One follow the same contract
/// as chat on every adapter that serves them, with the adapter's own HTTP
/// status table (OpenRouter's 401/402 are configuration, OpenAI's 402 is a
/// rejection).
#[tokio::test]
async fn every_endpoint_classifies_a_200_error_envelope_like_chat() {
    let mut problems = Vec::new();
    for &(kind, endpoint) in ENDPOINTS {
        let at = format!("{kind:?} {endpoint:?}");
        for (name, status) in NAMES {
            let expected = http_class(kind, false, name, *status);
            let body = json!({"error":{"code":name,"message":"secret provider text"}});
            let (class, usage) = call(kind, endpoint, &body).await;
            if class != Some(expected) || usage.is_some() {
                problems.push(format!(
                    "{at} {name}: {class:?} {usage:?}, want {expected:?}"
                ));
            }
        }
        for (error, expected) in [
            (json!({"code":402}), http_class(kind, false, "", 402)),
            (json!({"code":429}), InferenceError::Busy),
            (
                json!({"code":"brand_new"}),
                InferenceError::UpstreamUnavailable,
            ),
            (json!("text only"), InferenceError::UpstreamUnavailable),
        ] {
            let (class, _) = call(kind, endpoint, &json!({ "error": error })).await;
            if class != Some(expected) {
                problems.push(format!("{at} {error}: {class:?}, want {expected:?}"));
            }
        }
        let clean = json!({"error":{"code":"rate_limit_exceeded","message":"secret"}});
        if let Some((member, value)) = endpoint.output() {
            let mut body = clean.clone();
            body[member] = value;
            let (class, _) = call(kind, endpoint, &body).await;
            if class != Some(InferenceError::InvalidUpstream) {
                problems.push(format!("{at} with output: {class:?}"));
            }
        }
        let mut body = clean.clone();
        body["usage"] = endpoint.usage(kind);
        let (class, usage) = call(kind, endpoint, &body).await;
        if class != Some(InferenceError::InvalidUpstream) {
            problems.push(format!("{at} with usage: {class:?}"));
        }
        // Speech answers with audio; its usage is the request's own, never read.
        if endpoint != Endpoint::Speech && usage.is_none_or(|u| u.input_tokens != Some(3)) {
            problems.push(format!("{at} with usage: evidence {usage:?}"));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
