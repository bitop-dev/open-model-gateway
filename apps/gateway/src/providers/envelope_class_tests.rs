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
    complete_raw(kind, messages, body.to_string().into_bytes()).await
}
/// [`complete`] for a body given as its exact bytes.
pub(crate) async fn complete_raw(
    kind: Kind,
    messages: bool,
    body: Vec<u8>,
) -> (Option<InferenceError>, Option<Usage>) {
    let (base, _server) = serve(upstream(200, "application/json", vec![Step::Bytes(body)])).await;
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

/// A recognised rate-limit or quota error in a clean 200 is `Busy`, as an
/// HTTP 429 is, on every adapter and client protocol.
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
    call_steps(
        kind,
        endpoint,
        vec![Step::Bytes(body.to_string().into_bytes())],
    )
    .await
}
/// [`call`] for an upstream body made of `steps`.
async fn call_steps(
    kind: Kind,
    endpoint: Endpoint,
    steps: Vec<Step>,
) -> (Option<InferenceError>, Option<Usage>) {
    use crate::inference::{audio::*, images::*};
    let (base, _server) = serve(upstream(200, "application/json", steps)).await;
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

/// Output the shared transcription decoder knows (segments, words, log
/// probabilities, a duration) counts next to an error even when `text` is
/// empty: the provider transcribed something.
#[tokio::test]
async fn transcription_output_beyond_text_makes_an_error_an_invalid_response() {
    for kind in [Kind::OpenAi, Kind::OpenRouter] {
        for (member, value) in [
            ("segments", json!([{"text":"hello"}])),
            ("words", json!([{"word":"hello"}])),
            ("logprobs", json!([{"token":"hello"}])),
            ("duration", json!(3)),
        ] {
            let mut body = json!({"error":{"code":429},"text":""});
            body[member] = value;
            let (class, _) = call(kind, Endpoint::Transcription, &body).await;
            assert_eq!(
                class,
                Some(InferenceError::InvalidUpstream),
                "{kind:?} {member}"
            );
        }
    }
}

/// A cost or count too small for a float to tell from zero, a float zero and
/// a cost written as text all count as usage: only an integer zero or a
/// textual zero is no usage.
#[tokio::test]
async fn a_tiny_or_textual_usage_value_is_never_read_as_zero() {
    for usage in [
        r#"{"cost":1e-400}"#,
        r#"{"cost":0.0}"#,
        r#"{"cost":"0.0000001"}"#,
        r#"{"cost":"1e-400"}"#,
        r#"{"prompt_tokens":1e-400}"#,
    ] {
        let body = format!(r#"{{"error":{{"code":429}},"usage":{usage}}}"#).into_bytes();
        for kind in [Kind::OpenRouter, Kind::OpenAi] {
            let (class, _) = complete_raw(kind, false, body.clone()).await;
            assert_eq!(
                class,
                Some(InferenceError::InvalidUpstream),
                "{kind:?} {usage}"
            );
        }
    }
    // OpenRouter reads its exact cost: one it cannot read is not a zero.
    let body = br#"{"error":{"code":429},"usage":{"cost":"abc"}}"#.to_vec();
    let (class, _) = complete_raw(Kind::OpenRouter, false, body).await;
    assert_eq!(
        class,
        Some(InferenceError::InvalidUpstream),
        "unreadable cost"
    );
    // Zeros stay clean: an integer zero everywhere, a textual zero under the
    // generic rule (OpenRouter's exact cost is a number, so text is unreadable).
    for (kind, zero) in [
        (Kind::OpenRouter, r#"{"cost":0,"prompt_tokens":0}"#),
        (Kind::OpenAi, r#"{"prompt_tokens":0}"#),
        (Kind::OpenAi, r#"{"cost":"0"}"#),
        (Kind::OpenAi, r#"{"cost":"0.00"}"#),
    ] {
        let body = format!(r#"{{"error":{{"code":429}},"usage":{zero}}}"#).into_bytes();
        let (class, _) = complete_raw(kind, false, body).await;
        assert_eq!(class, Some(InferenceError::Busy), "{kind:?} {zero}");
    }
    let body = br#"{"error":{"code":429},"usage":{"cost":"0"}}"#.to_vec();
    let (class, _) = complete_raw(Kind::OpenRouter, false, body).await;
    assert_eq!(
        class,
        Some(InferenceError::InvalidUpstream),
        "OpenRouter text cost"
    );
}

/// A body that names a member twice is ambiguous (a JSON reader keeps one of
/// them): it is an invalid response, never a clean envelope.
#[tokio::test]
async fn a_body_with_a_duplicate_member_is_an_invalid_response() {
    let bodies: [&[u8]; 4] = [
        br#"{"error":{"code":400},"choices":[{"index":0,"message":{"role":"assistant","content":"answered"},"finish_reason":"stop"}],"choices":[]}"#,
        br#"{"error":{"code":429},"usage":{"prompt_tokens":3,"prompt_tokens":0}}"#,
        br#"{"error":{"code":429},"error":{"code":400}}"#,
        br#"{"error":{"code":429,"code":400}}"#,
    ];
    for kind in [Kind::OpenAi, Kind::OpenRouter, Kind::Local] {
        for body in bodies {
            let (class, _) = complete_raw(kind, false, body.to_vec()).await;
            assert_eq!(
                class,
                Some(InferenceError::InvalidUpstream),
                "{kind:?} {}",
                String::from_utf8_lossy(body)
            );
        }
    }
    for kind in [Kind::Anthropic, Kind::Responses] {
        let body = envelope(kind, named(kind, "rate_limit_error")).to_string();
        let duplicated = body.replacen('{', r#"{"usage":null,"usage":null,"#, 1);
        let (class, _) = complete_raw(kind, false, duplicated.into_bytes()).await;
        assert_eq!(class, Some(InferenceError::InvalidUpstream), "{kind:?}");
    }
}

/// OpenRouter's token counts, duration and exact cost are independent
/// evidence next to an error: one being malformed does not drop the other,
/// and the body's `model` string is not recorded from an error body.
#[tokio::test]
async fn openrouter_keeps_each_valid_meter_next_to_an_error_and_no_model() {
    let (class, usage) = complete(
        Kind::OpenRouter,
        false,
        &json!({"error":{"code":429},"model":"user_2xPrivateAccountIdentifier",
            "usage":{"prompt_tokens":"bad","cost":0.01}}),
    )
    .await;
    assert_eq!(class, Some(InferenceError::InvalidUpstream));
    let usage = usage.expect("the cost is evidence");
    assert_eq!(usage.provider_cost_microusd, Some(10_000));
    assert_eq!(usage.reported_model, None);
    let (_, usage) = complete(
        Kind::OpenRouter,
        false,
        &json!({"error":{"code":429},"model":"user_2xPrivateAccountIdentifier",
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,"cost":0.01}}),
    )
    .await;
    let usage = usage.expect("tokens and cost are evidence");
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(3), Some(2))
    );
    assert_eq!(usage.provider_cost_microusd, Some(10_000));
    assert_eq!(usage.reported_model, None);
    let (class, usage) = call(
        Kind::OpenRouter,
        Endpoint::Transcription,
        &json!({"error":{"code":429},"usage":{"type":"duration","seconds":3,"cost":0.01}}),
    )
    .await;
    assert_eq!(class, Some(InferenceError::InvalidUpstream));
    assert_eq!(
        usage.expect("duration and cost").provider_cost_microusd,
        Some(10_000)
    );
    for (endpoint, body) in [
        (
            Endpoint::Rerank,
            json!({"error":{"code":429},"usage":{"total_tokens":3,"cost":"bad"}}),
        ),
        (
            Endpoint::Images,
            json!({"error":{"code":429},"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,"cost":"bad"}}),
        ),
    ] {
        let (class, usage) = call(Kind::OpenRouter, endpoint, &body).await;
        assert_eq!(class, Some(InferenceError::InvalidUpstream), "{endpoint:?}");
        assert_eq!(
            usage.and_then(|u| u.input_tokens),
            Some(3),
            "{endpoint:?}: tokens kept"
        );
    }
}

/// A JSON speech answer whose body breaks before it is complete is an invalid
/// response, as any non-audio answer was: not an unavailable provider that
/// may fail over.
#[tokio::test]
async fn a_broken_json_speech_body_is_an_invalid_response() {
    let (class, _) = call_steps(
        Kind::OpenAi,
        Endpoint::Speech,
        vec![Step::Bytes(br#"{"error":{"code":"#.to_vec()), Step::Reset],
    )
    .await;
    assert_eq!(class, Some(InferenceError::InvalidUpstream));
}
