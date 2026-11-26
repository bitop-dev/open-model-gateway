//! Anthropic Messages / Claude Code frontend; adapter-specific field rules.
use super::responses::{Accumulator, Result, event};
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{Engine, error::InferenceError, types::*},
    providers::messages::BetaHeaderError,
};
use axum::{
    Extension, Json,
    extract::rejection::JsonRejection,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse, sse::KeepAlive},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{convert::Infallible, time::Duration};
mod request;
#[cfg(test)]
use request::parse_request;
pub use request::{CountRequest, Request};
pub(crate) const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
/// The two Anthropic API versions the gateway accepts (#430). Requests are
/// always sent upstream as `2023-06-01`: under `2026-10-21` the gateway refuses
/// every field whose meaning differs between the versions (the sampling
/// parameters; documents and citations are outside the supported grammar under
/// both), so the forwarded request means the same thing under either version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApiVersion {
    V2023_06_01,
    V2026_10_21,
}
fn version(headers: &HeaderMap) -> Option<ApiVersion> {
    if headers.get_all("anthropic-version").iter().count() != 1 || beta_headers(headers).is_err() {
        return None;
    }
    match headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
    {
        Some("2023-06-01") => Some(ApiVersion::V2023_06_01),
        Some("2026-10-21") => Some(ApiVersion::V2026_10_21),
        _ => None,
    }
}
/// Kept for the header tests written against the boolean form.
#[cfg(test)]
fn valid_version(headers: &HeaderMap) -> bool {
    version(headers).is_some()
}
fn beta_headers(headers: &HeaderMap) -> std::result::Result<Vec<String>, BetaHeaderError> {
    let values = headers
        .get_all("anthropic-beta")
        .iter()
        .map(|v| v.to_str().map_err(|_| BetaHeaderError::Invalid))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    crate::providers::messages::validate_beta_headers(values.iter().copied())?;
    Ok(values.into_iter().map(str::to_owned).collect())
}
/// A field the client's `anthropic-version` does not allow: the message names
/// the version, so a client that upgraded the header knows what to remove.
fn version_error_response(field: &'static str) -> Response {
    let message = format!("{field}: not supported with anthropic-version 2026-10-21");
    super::error_with_body(
        InferenceError::InvalidField(field),
        json!({"type":"error","error":{"type":"invalid_request_error","message":message}}),
    )
}
fn beta_error_response(tokens: &[String]) -> Response {
    // Exact Anthropic wording triggers Claude Code's retry without rejected
    // betas. Only bounded client token names are reflected; never upstream
    // bodies or diagnostic text, and never dynamic inference/log error values.
    let message = format!(
        "Unexpected value(s) `{}` for the `anthropic-beta` header. Please consult our documentation at docs.anthropic.com or try again without the header.",
        tokens.join(", ")
    );
    super::error_with_body(
        InferenceError::InvalidField("anthropic-beta"),
        json!({"type":"error","error":{"type":"invalid_request_error","message":message}}),
    )
}
/// The validated `anthropic-beta` values and `anthropic-version` of a
/// Messages or count request, or the Anthropic-shaped refusal (boxed: a
/// response is large and the success path is the common one).
fn protocol_headers(
    headers: &HeaderMap,
) -> std::result::Result<(Vec<String>, ApiVersion), Box<Response>> {
    let betas = match beta_headers(headers) {
        Ok(v) => v,
        Err(BetaHeaderError::Denied(tokens)) => {
            return Err(Box::new(beta_error_response(&tokens)));
        }
        Err(BetaHeaderError::Invalid) => {
            return Err(Box::new(error_response(InferenceError::InvalidField(
                "anthropic-beta",
            ))));
        }
    };
    let Some(version) = version(headers) else {
        return Err(Box::new(error_response(InferenceError::InvalidRequest)));
    };
    Ok((betas, version))
}
/// The decoded body, or the Anthropic-shaped refusal (413 for a streamed
/// body over the cap, the unknown field's name, else the generic 400).
fn body<T>(
    input: std::result::Result<Json<T>, JsonRejection>,
) -> std::result::Result<T, Box<Response>> {
    match input {
        Ok(Json(v)) => Ok(v),
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            Err(Box::new(http_error(StatusCode::PAYLOAD_TOO_LARGE)))
        }
        Err(e) => Err(Box::new(rejection(e))),
    }
}
/// `POST /v1/messages/count_tokens` (#72): the headers, grammar and live
/// key/workspace/model authorization of `handle`, answered by
/// `Engine::count_tokens`. The reply is `{"input_tokens":N}`; the
/// `x-omg-token-count` header says whether the provider counted it or the
/// gateway estimated it. Nothing is billed or admitted.
pub async fn count_tokens(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    input: std::result::Result<Json<CountRequest>, JsonRejection>,
) -> Response {
    let (betas, _version) = match protocol_headers(&headers) {
        Ok(v) => v,
        Err(response) => return *response,
    };
    let wire = match body(input) {
        Ok(v) => v,
        Err(response) => return *response,
    };
    let mut request = match wire.normalize() {
        Ok(v) => v,
        Err(e) => return error_response(e),
    };
    if let Some(options) = &mut request.messages_options {
        options.beta_headers = betas;
    }
    match engine.count_tokens(principal, request, request_id.0).await {
        Ok(count) => {
            let mut response = Json(json!({"input_tokens":count.input_tokens})).into_response();
            response.headers_mut().insert(
                "x-omg-token-count",
                axum::http::HeaderValue::from_static(count.source.header()),
            );
            response
        }
        Err(e) => error_response(e),
    }
}
pub async fn handle(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    input: std::result::Result<Json<Request>, JsonRejection>,
) -> Response {
    let (betas, version) = match protocol_headers(&headers) {
        Ok(v) => v,
        Err(response) => return *response,
    };
    let wire = match body(input) {
        Ok(v) => v,
        Err(response) => return *response,
    };
    if let Err(field) = wire.check_version(version) {
        return version_error_response(field);
    }
    // Anthropic bounds metadata.user_id to 256 characters.
    if wire
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .is_some_and(|u| u.chars().count() > 256)
    {
        return error_response(InferenceError::InvalidRequest);
    }
    let labels = crate::inference::client::current()
        .with_body_session([wire.metadata.as_ref().and_then(|m| m.user_id.as_deref())]);
    let mut request = match wire.normalize() {
        Ok(v) => v,
        Err(e) => return error_response(e),
    };
    if let Some(options) = &mut request.messages_options {
        options.beta_headers = betas;
    }
    let model = request.model.clone();
    let show_thinking = crate::providers::messages::shows_thinking(&request);
    let id = format!("msg_{}", request_id.0.simple());
    let output = match labels
        .scope(engine.execute_protocol(principal, request, request_id.0, ApiProtocol::Messages))
        .await
    {
        Ok(v) => v,
        Err(e) => return error_response(e),
    };
    render_messages(output, id, model, show_thinking)
}
fn usage_json(usage: Usage) -> Value {
    let mut value = json!({});
    if let Some(n) = usage.input_tokens {
        value["input_tokens"] = n.into();
    }
    if let Some(n) = usage.output_tokens {
        value["output_tokens"] = n.into();
    }
    if let Some(b) = usage.billing {
        if let Some(n) = b.cache_read_input_tokens {
            value["cache_read_input_tokens"] = n.into();
        }
        if let Some(n) = b.cache_write_input_tokens {
            value["cache_creation_input_tokens"] = n.into();
        }
        let mut allocation = json!({});
        if let Some(n) = b.cache_write_5m_input_tokens {
            allocation["ephemeral_5m_input_tokens"] = n.into();
        }
        if let Some(n) = b.cache_write_1h_input_tokens {
            allocation["ephemeral_1h_input_tokens"] = n.into();
        }
        if allocation.as_object().is_some_and(|d| !d.is_empty()) {
            value["cache_creation"] = allocation;
        }
    }
    value
}
/// `/v1/batches` line body (`crate::jobs::lines`, Anthropic shape): the
/// interactive contract, never streamed. Batch creation carries no
/// `anthropic-version`, so every line follows the `2023-06-01` rules (#430).
pub(crate) fn batch_request(body: Value) -> Result<ChatRequest> {
    let wire: Request = serde_json::from_value(body).map_err(|_| InferenceError::InvalidRequest)?;
    if wire.stream
        || wire
            .metadata
            .as_ref()
            .and_then(|m| m.user_id.as_ref())
            .is_some_and(|u| u.chars().count() > 256)
    {
        return Err(InferenceError::InvalidRequest);
    }
    wire.normalize()
}
/// `/v1/batches` line result body (an Anthropic `message`).
pub(crate) fn batch_response(id: &str, model: &str, response: &ChatResponse) -> Result<Value> {
    snapshot(id, model, response)
}
/// `/v1/batches` line error body (Anthropic shape).
pub(crate) fn batch_error_body(e: InferenceError) -> Value {
    error_body(e)
}
fn snapshot(id: &str, model: &str, response: &ChatResponse) -> Result<Value> {
    let size = response.content.as_ref().map_or(0, String::len)
        + response.reasoning.as_ref().map_or(0, String::len)
        + response
            .tool_calls
            .iter()
            .map(|c| c.id.len() + c.name.len() + c.arguments.len())
            .sum::<usize>();
    if size > OUTPUT_LIMIT {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut content = vec![];
    // Translated reasoning has no signature, and none is invented.
    if let Some(thinking) = &response.reasoning {
        content.push(json!({"type":"thinking","thinking":thinking}));
    }
    // A translated refusal carries no output: the empty text its frames may
    // hold is not a block (the stream renders the same terminal content).
    if let Some(text) = &response.content
        && !(text.is_empty() && response.finish_reason == FinishReason::Refusal)
    {
        content.push(json!({"type":"text","text":text}));
    }
    for call in &response.tool_calls {
        let input: Value =
            serde_json::from_str(&call.arguments).map_err(|_| InferenceError::InvalidUpstream)?;
        if !input.is_object() {
            return Err(InferenceError::InvalidUpstream);
        }
        content.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":input}));
    }
    if let Some(blocks) = &response.messages_content {
        if serde_json::to_vec(blocks)
            .map_err(|_| InferenceError::InvalidUpstream)?
            .len()
            > OUTPUT_LIMIT
        {
            return Err(InferenceError::InvalidUpstream);
        }
        content.clone_from(blocks);
    }
    let stop = match response.finish_reason {
        FinishReason::Stop => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Refusal => "refusal",
        FinishReason::ContextWindowExceeded => "model_context_window_exceeded",
        FinishReason::ContentFilter => return Err(InferenceError::Unsupported),
    };
    Ok(
        json!({"id":id,"type":"message","role":"assistant","model":model,"content":content,"stop_reason":stop,"stop_sequence":null,"usage":usage_json(response.usage)}),
    )
}
#[cfg(test)]
fn render(output: ProviderOutput, id: String, model: String) -> Response {
    render_messages(output, id, model, true)
}
/// `show_thinking` (`providers::messages::shows_thinking`) withholds translated
/// reasoning the client did not ask for. It is still pushed through the
/// accumulator first, so it stays bounded and refused after Finish.
fn render_messages(
    output: ProviderOutput,
    id: String,
    model: String,
    show_thinking: bool,
) -> Response {
    match output {
        ProviderOutput::Complete(mut response) => {
            if !show_thinking {
                response.reasoning = None;
            }
            match snapshot(&id, &model, &response) {
                Ok(v) => Json(v).into_response(),
                Err(e) => error_response(e),
            }
        }
        ProviderOutput::Stream(mut stream) => {
            let events = async_stream::stream! {
                // Sent when the upstream stream opens, before any usage is observed (#23).
                // `input_tokens: 0` is a stream-start placeholder that strict clients require
                // (e.g. Vercel's @ai-sdk/anthropic); message_delta carries the observed input,
                // cache and output counters, which the SDKs apply. `output_tokens` is left out:
                // clients merge usage, so a start value that message_delta never replaces would
                // turn unknown output into zero. Gateway accounting never uses these values.
                yield Ok::<_, Infallible>(event("message_start", json!({"message":{"id":id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0}}})));
                // Native Anthropic preserves validated block lifecycles. Translated
                // streams open one block per run of reasoning (unsigned `thinking`)
                // or text, in arrival order, closing each before the next opens;
                // tools are buffered after them. Both paths wait for engine Done.
                let mut accumulator = Accumulator::with_limit(OUTPUT_LIMIT); let mut done = false; let mut native = false;
                // (index, is_thinking) of the open translated block; `blocks` counts opened blocks.
                let mut open: Option<(usize, bool)> = None; let mut blocks = 0_usize; let mut text_streamed = false;
                while let Some(next) = stream.next().await {
                    let block = match &next { Ok(ChatEvent::MessagesBlock(v)) => Some(v.clone()), _ => None };
                    let piece = match &next {
                        Ok(ChatEvent::Delta { text: Some(text), .. }) => Some((false, text.clone())),
                        Ok(ChatEvent::Reasoning(text)) => Some((true, text.clone())),
                        _ => None,
                    };
                    match next.and_then(|e| accumulator.push(e)) {
                        Ok(finished) => {
                            if let Some(block) = block {
                                native = true;
                                let Some(kind) = block.get("type").and_then(Value::as_str) else {
                                    yield Ok(event("error", error_body(InferenceError::InvalidUpstream))); return;
                                };
                                yield Ok(event(kind, block.clone()));
                            }
                            // Empty pieces (OpenAI-wire `"content":""` beside reasoning)
                            // never open or switch a block; see the empty text block below.
                            if let Some((thinking, piece)) = piece.filter(|(t, p)| !native && !p.is_empty() && (show_thinking || !t)) {
                                let index = match open {
                                    Some((index, kind)) if kind == thinking => index,
                                    previous => {
                                        if let Some((index, _)) = previous {
                                            yield Ok(event("content_block_stop", json!({"index":index})));
                                        }
                                        let index = blocks; blocks += 1; open = Some((index, thinking)); text_streamed |= !thinking;
                                        let start = if thinking { json!({"type":"thinking","thinking":""}) } else { json!({"type":"text","text":""}) };
                                        yield Ok(event("content_block_start", json!({"index":index,"content_block":start})));
                                        index
                                    }
                                };
                                let delta = if thinking { json!({"type":"thinking_delta","thinking":piece}) } else { json!({"type":"text_delta","text":piece}) };
                                yield Ok(event("content_block_delta", json!({"index":index,"delta":delta})));
                            }
                            if finished { done = true; break; }
                        }
                        Err(e) => { yield Ok(event("error", error_body(e))); return; }
                    }
                }
                if !done { yield Ok(event("error", error_body(InferenceError::InvalidUpstream))); return; }
                let terminal = match accumulator.finish().and_then(|r| snapshot(&id, &model, &r)) { Ok(v) => v, Err(e) => { yield Ok(event("error", error_body(e))); return; } };
                let Some(content) = terminal["content"].as_array() else {
                    yield Ok(event("error", error_body(InferenceError::InvalidUpstream))); return;
                };
                if !native {
                    if let Some((index, _)) = open {
                        yield Ok(event("content_block_stop", json!({"index":index})));
                    }
                    // Text that was only ever empty still gets its (empty) block, as in JSON.
                    if !text_streamed && content.iter().any(|b| b["type"] == "text") {
                        let index = blocks; blocks += 1;
                        yield Ok(event("content_block_start", json!({"index":index,"content_block":{"type":"text","text":""}})));
                        yield Ok(event("content_block_stop", json!({"index":index})));
                    }
                    for block in content.iter().filter(|b| b["type"] == "tool_use") {
                        let index = blocks; blocks += 1;
                        let mut start = block.clone(); start["input"] = json!({});
                        yield Ok(event("content_block_start", json!({"index":index,"content_block":start})));
                        yield Ok(event("content_block_delta", json!({"index":index,"delta":{"type":"input_json_delta","partial_json":block["input"].to_string()}})));
                        yield Ok(event("content_block_stop", json!({"index":index})));
                    }
                }
                yield Ok(event("message_delta", json!({"delta":{"stop_reason":terminal["stop_reason"],"stop_sequence":null},"usage":terminal["usage"]})));
                yield Ok(event("message_stop", json!({})));
            };
            Sse::new(events)
                .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
                .into_response()
        }
    }
}
/// Outer transport rejections must not become plain text/OpenAI errors.
pub(crate) fn http_error(status: StatusCode) -> Response {
    let (kind, message) = if status == StatusCode::PAYLOAD_TOO_LARGE {
        ("request_too_large", "Request body too large")
    } else {
        ("invalid_request_error", "Method not allowed")
    };
    (
        status,
        Json(json!({"type":"error","error":{"type":kind,"message":message}})),
    )
        .into_response()
}
fn rejection(e: JsonRejection) -> Response {
    // Never echo serde's value/type diagnostics (they may include prompt text).
    // Only an identifier from its unknown-field diagnostic is safe to expose.
    let diagnostic = e.body_text();
    let field = diagnostic
        .split("unknown field `")
        .nth(1)
        .and_then(|v| v.split('`').next());
    if let Some(field) = field.filter(|v| {
        !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }) {
        return (StatusCode::BAD_REQUEST, Json(json!({"type":"error","error":{"type":"invalid_request_error","message":format!("Unknown field: {field}")}}))).into_response();
    }
    error_response(InferenceError::InvalidRequest)
}
fn error_body(e: InferenceError) -> Value {
    let kind = match e {
        InferenceError::InvalidRequest
        | InferenceError::InvalidField(_)
        | InferenceError::UnknownField(_)
        | InferenceError::ContextLength
        | InferenceError::UpstreamRejected
        | InferenceError::Unsupported => "invalid_request_error",
        InferenceError::ModelUnavailable => "not_found_error",
        InferenceError::Unauthenticated => "authentication_error",
        // Anthropic has no budget type for HTTP 429; the message distinguishes it.
        InferenceError::Busy
        | InferenceError::BudgetExceeded(_)
        | InferenceError::UnresolvedUsage(_)
        | InferenceError::TokenReservationExceedsLimit(_)
        | InferenceError::JobLimitExceeded(_) => "rate_limit_error",
        InferenceError::UpstreamUnavailable
        | InferenceError::Overloaded
        | InferenceError::ScopeBusy
        | InferenceError::RouteCoolingDown(_)
        | InferenceError::RouteDrained(_) => "overloaded_error",
        _ => "api_error",
    };
    let message = match e {
        InferenceError::UnknownField(field) => format!("Unknown field: {}", field.as_str()),
        InferenceError::InvalidField(field) => format!("Unsupported or invalid field: {field}"),
        _ => e.message().to_owned(),
    };
    json!({"type":"error","error":{"type":kind,"message":message}})
}
pub(super) fn status(e: InferenceError) -> StatusCode {
    match e {
        // Only a provider-reported overload is Anthropic's 529; an unavailable
        // upstream or a cooling route keeps the gateway status the official
        // SDK retry contracts depend on.
        InferenceError::Overloaded => StatusCode::from_u16(529).unwrap(),
        _ => super::http_status(e),
    }
}
fn error_response(e: InferenceError) -> Response {
    let mut response = super::error_with_body(e, error_body(e));
    *response.status_mut() = status(e);
    response
}
#[cfg(test)]
mod frontend_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) fn openapi_normalize(value: Value) -> Result<ChatRequest> {
    request::parse_request(value)
}
/// The count route's normalization, for the contract's semantic constraint.
#[cfg(test)]
pub(crate) fn openapi_count_normalize(value: Value) -> Result<ChatRequest> {
    serde_json::from_value::<CountRequest>(value)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()
}
