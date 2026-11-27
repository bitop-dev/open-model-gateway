use std::{
    convert::Infallible,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::http::json_body::JsonBodyRejection;
use axum::{
    Extension, Json,
    http::StatusCode,
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::Principal,
    http::RequestId,
    inference::{Engine, client, error::InferenceError, types::*},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(default, deserialize_with = "super::false_if_null")]
    stream: bool,
    stream_options: Option<StreamOptions>,
    temperature: Option<f64>,
    max_completion_tokens: Option<u32>,
    /// Legacy alias of `max_completion_tokens` (still sent by many
    /// OpenAI-compatible clients). Both may be sent only with equal values.
    max_tokens: Option<u32>,
    n: Option<u32>,
    #[serde(default)]
    tools: Vec<WireTool>,
    tool_choice: Option<Value>,
    /// Client labels only (session id source for Logs); not forwarded upstream.
    user: Option<String>,
    metadata: Option<client::OpenAiMetadata>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamOptions {
    #[serde(default)]
    include_usage: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMessage {
    role: Role,
    content: Option<super::image_input::WireContent>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
    tool_call_id: Option<String>,
    #[serde(default)]
    refusal: NoRefusal,
}
/// Public assistant metadata: null means no refusal. Non-null refusal content
/// is outside this text/function subset; do not silently discard it.
#[derive(Default)]
enum NoRefusal {
    #[default]
    Absent,
    Null,
}
impl<'de> Deserialize<'de> for NoRefusal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <()>::deserialize(deserializer).map(|()| Self::Null)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunctionCall,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTool {
    #[serde(rename = "type")]
    kind: String,
    function: WireFunction,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFunction {
    name: String,
    description: Option<String>,
    #[serde(default = "no_argument_parameters")]
    parameters: Value,
    strict: Option<bool>,
}

// OpenAI defines omission as an empty parameter list, not an unconstrained
// schema. Serde calls this only for absence; explicit null is still invalid.
fn no_argument_parameters() -> Value {
    json!({"type":"object","properties":{},"additionalProperties":false})
}

fn function_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A message's content. Text parts join into one text (as Responses input
/// parts do); content with an image keeps its ordered parts, on user
/// messages only.
fn content(
    role: Role,
    wire: super::image_input::WireContent,
    images: &mut super::image_input::Images,
) -> Result<Content, InferenceError> {
    use super::image_input::WireContent;
    let parts = match wire {
        WireContent::Text(text) => return Ok(Content::Text(text)),
        WireContent::Parts(parts) if !parts.is_empty() => parts,
        WireContent::Parts(_) => return Err(InferenceError::InvalidRequest),
    };
    let mut out: Vec<Part> = Vec::new();
    for part in parts {
        // A file or audio reference is refused by name, whatever the part's
        // type, never dropped beside it.
        if part.file.is_some() {
            return Err(InferenceError::InvalidField("messages.content.file"));
        }
        if part.input_audio.is_some() {
            return Err(InferenceError::InvalidField("messages.content.input_audio"));
        }
        let next = match (part.kind.as_str(), part.text, part.image_url) {
            ("text", Some(text), None) => Part::Text(text),
            ("image_url", None, Some(image)) if role == Role::User => Part::Image(images.take(
                image.url,
                image.detail.as_deref(),
                "messages.content.image_url",
            )?),
            ("image_url", _, _) => {
                return Err(InferenceError::InvalidField("messages.content.image_url"));
            }
            ("file", _, _) => return Err(InferenceError::InvalidField("messages.content.file")),
            ("input_audio", _, _) => {
                return Err(InferenceError::InvalidField("messages.content.input_audio"));
            }
            _ => return Err(InferenceError::InvalidRequest),
        };
        // Adjacent text parts join (as Responses input parts do).
        match (out.last_mut(), next) {
            (Some(Part::Text(previous)), Part::Text(text)) => previous.push_str(&text),
            (_, next) => out.push(next),
        }
    }
    if let [Part::Text(_)] = out.as_slice() {
        let Some(Part::Text(text)) = out.pop() else {
            unreachable!("one text part")
        };
        return Ok(Content::Text(text));
    }
    Ok(Content::Parts(out))
}

/// One wire message: role rules, then its tool calls.
fn message(
    message: WireMessage,
    images: &mut super::image_input::Images,
) -> Result<Message, InferenceError> {
    let invalid = InferenceError::InvalidRequest;
    if message.role != Role::Assistant && matches!(message.refusal, NoRefusal::Null) {
        return Err(invalid);
    }
    if message.role == Role::Tool {
        if message.tool_call_id.as_ref().is_none_or(|id| id.is_empty())
            || message.content.is_none()
            || !message.tool_calls.is_empty()
        {
            return Err(invalid);
        }
    } else if message.tool_call_id.is_some() {
        return Err(invalid);
    }
    if message.role != Role::Assistant
        && (message.content.is_none() || !message.tool_calls.is_empty())
    {
        return Err(invalid);
    }
    if message.content.is_none() && message.tool_calls.is_empty() {
        return Err(invalid);
    }
    let tool_calls = message
        .tool_calls
        .into_iter()
        .map(|call| {
            if call.kind != "function" || call.id.is_empty() || !function_name(&call.function.name)
            {
                return Err(invalid);
            }
            Ok(ToolCall {
                id: call.id,
                name: call.function.name,
                arguments: call.function.arguments,
            })
        })
        .collect::<Result<_, _>>()?;
    let role = message.role;
    let content = message
        .content
        .map(|wire| content(role, wire, images))
        .transpose()?;
    Ok(Message {
        role,
        content,
        tool_calls,
        tool_call_id: message.tool_call_id,
        tool_result_is_error: false,
    })
}

/// The function tools; names are distinct and recorded in `names`.
fn function_tools(
    tools: Vec<WireTool>,
    names: &mut std::collections::BTreeSet<String>,
) -> Result<Vec<FunctionTool>, InferenceError> {
    tools
        .into_iter()
        .map(|tool| {
            if tool.kind != "function"
                || !function_name(&tool.function.name)
                || !tool.function.parameters.is_object()
                || !names.insert(tool.function.name.clone())
            {
                return Err(InferenceError::InvalidRequest);
            }
            Ok(FunctionTool {
                name: tool.function.name,
                description: tool.function.description,
                parameters: tool.function.parameters,
                strict: tool.function.strict,
            })
        })
        .collect::<Result<Vec<_>, _>>()
}

/// `tool_choice`: a mode string, or a named function among the tools.
fn tool_choice(
    choice: Option<Value>,
    tools: &[FunctionTool],
    names: &std::collections::BTreeSet<String>,
) -> Result<Option<ToolChoice>, InferenceError> {
    let invalid = InferenceError::InvalidRequest;
    Ok(match choice {
        None => None,
        Some(Value::String(s)) => Some(match s.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" if !tools.is_empty() => ToolChoice::Required,
            _ => return Err(invalid),
        }),
        Some(value) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Named {
                #[serde(rename = "type")]
                kind: String,
                function: Name,
            }
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Name {
                name: String,
            }
            let choice: Named = serde_json::from_value(value).map_err(|_| invalid)?;
            if choice.kind != "function" || !names.contains(&choice.function.name) {
                return Err(invalid);
            }
            Some(ToolChoice::Function(choice.function.name))
        }
    })
}

impl Request {
    /// Request-level bounds, checked before anything is consumed.
    fn check_bounds(&self) -> Result<(), InferenceError> {
        if self.model.trim().is_empty()
            || self.model.len() > 200
            || self.messages.is_empty()
            || self.messages.len() > 4096
            || self.tools.len() > 128
            || self.n.is_some_and(|n| n != 1)
            || self.temperature.is_some_and(|n| !(0.0..=2.0).contains(&n))
            || self.max_completion_tokens == Some(0)
            || self.max_tokens == Some(0)
            || self
                .max_completion_tokens
                .zip(self.max_tokens)
                .is_some_and(|(a, b)| a != b)
            || (!self.stream && self.stream_options.is_some())
        {
            return Err(InferenceError::InvalidRequest);
        }
        Ok(())
    }

    fn normalize(self) -> Result<ChatRequest, InferenceError> {
        self.check_bounds()?;
        // One maximum for the upstream request and the admission reservation.
        let max_output_tokens = self.max_completion_tokens.or(self.max_tokens);
        let mut messages = Vec::new();
        let mut images = super::image_input::Images::default();
        for wire in self.messages {
            messages.push(message(wire, &mut images)?);
        }
        let mut names = std::collections::BTreeSet::new();
        let tools = function_tools(self.tools, &mut names)?;
        let tool_choice = tool_choice(self.tool_choice, &tools, &names)?;
        Ok(ChatRequest {
            messages_options: None,
            model: self.model,
            messages,
            tools,
            tool_choice,
            temperature: self.temperature,
            max_output_tokens,
            stream: self.stream,
        })
    }
}

pub async fn handle(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    large: Option<Extension<crate::inference::image_buffer::LargeBody>>,
    input: Result<crate::http::json_body::JsonBody<Request>, JsonBodyRejection>,
) -> Response {
    let wire = match input {
        Ok(crate::http::json_body::JsonBody(request)) => request,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return super::payload_too_large();
        }
        Err(_) => return error_response(InferenceError::InvalidRequest),
    };
    if !client::valid_openai_metadata(wire.metadata.as_ref()) {
        return error_response(InferenceError::InvalidRequest);
    }
    let labels = client::current().with_body_session(client::openai_session(
        wire.metadata.as_ref(),
        wire.user.as_deref(),
    ));
    let include_usage = wire
        .stream_options
        .as_ref()
        .is_some_and(|options| options.include_usage);
    let request = match wire.normalize() {
        Ok(request) => request,
        Err(error) => return error_response(error),
    };
    // Only image data may take a body past the shared text limit.
    if large.is_some_and(|Extension(large)| !large.allows(&request)) {
        crate::metrics::METRICS.image_input_refused("too_large");
        return super::payload_too_large();
    }
    let model = request.model.clone();
    let id = format!("chatcmpl-{}", request_id.0.simple());
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let output = match labels
        .scope(engine.execute(principal, request, request_id.0))
        .await
    {
        Ok(output) => output,
        Err(error) => return error_response(error),
    };
    match output {
        ProviderOutput::Complete(response) => {
            Json(complete_body(&id, created, &model, response)).into_response()
        }
        ProviderOutput::Stream(mut upstream) => {
            let events = async_stream::stream! {
                let chunk = |delta: Value, finish: Value| {
                    let mut value = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,
                        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
                    if include_usage { value["usage"] = Value::Null; }
                    value
                };
                yield Ok::<_,Infallible>(Event::default().data(chunk(json!({"role":"assistant"}), Value::Null).to_string()));
                while let Some(event) = upstream.next().await {
                    let data = match event {
                        Ok(ChatEvent::Delta { text, tool_calls }) => {
                            let mut delta = json!({});
                            if let Some(text) = text { delta["content"] = Value::String(text); }
                            if !tool_calls.is_empty() {
                                delta["tool_calls"] = Value::Array(tool_calls.into_iter().map(|call| {
                                    let mut value = json!({"index":call.index});
                                    if let Some(id) = call.id { value["id"] = id.into(); value["type"] = "function".into(); }
                                    let mut function = json!({});
                                    if let Some(name) = call.name { function["name"] = name.into(); }
                                    if let Some(arguments) = call.arguments { function["arguments"] = arguments.into(); }
                                    if function.as_object().is_some_and(|o| !o.is_empty()) { value["function"] = function; }
                                    value
                                }).collect());
                            }
                            chunk(delta, Value::Null)
                        }
                        // Messages-only events; adapters emit them only for Messages requests.
                        Ok(ChatEvent::MessagesBlock(_) | ChatEvent::Reasoning(_)) => continue,
                        Ok(ChatEvent::Finish(reason)) => chunk(json!({}), json!(finish_reason(reason))),
                        Ok(ChatEvent::Usage(usage)) => {
                            if !include_usage { continue; }
                            let Some(usage) = usage_json(usage) else { continue; };
                            json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[],"usage":usage})
                        }
                        Ok(ChatEvent::Done) => { yield Ok(Event::default().data("[DONE]")); break; }
                        Err(error) => { yield Ok(Event::default().data(error_body(error).to_string())); break; }
                    };
                    yield Ok(Event::default().data(data.to_string()));
                }
            };
            Sse::new(events)
                .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
                .into_response()
        }
    }
}

/// Chat has no native refusal/context-window stop reasons. Never serialize the
/// canonical enum directly into either JSON or a terminal stream chunk.
fn finish_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length | FinishReason::ContextWindowExceeded => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter | FinishReason::Refusal => "content_filter",
    }
}

/// A non-streaming `chat.completion` object.
fn complete_body(id: &str, created: u64, model: &str, response: ChatResponse) -> Value {
    let mut message = json!({"role":"assistant", "content": response.content});
    if !response.tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(response.tool_calls.into_iter().map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}})).collect());
    }
    let mut body = json!({"id":id,"object":"chat.completion","created":created,"model":model,
        "choices":[{"index":0,"message":message,"finish_reason":finish_reason(response.finish_reason)}]});
    if let Some(usage) = usage_json(response.usage) {
        body["usage"] = usage;
    }
    body
}

/// `/v1/batches` line body (`crate::jobs::lines`): the same contract as an
/// interactive request (including the `max_tokens` alias), never streamed.
pub(crate) fn batch_request(body: Value) -> Result<ChatRequest, InferenceError> {
    let wire: Request = serde_json::from_value(body).map_err(|_| InferenceError::InvalidRequest)?;
    if wire.stream || !client::valid_openai_metadata(wire.metadata.as_ref()) {
        return Err(InferenceError::InvalidRequest);
    }
    let request = wire.normalize()?;
    // Batch files are not under the image buffer budget: image input is for
    // interactive requests only.
    if request.has_images() {
        return Err(InferenceError::InvalidField("messages.content.image_url"));
    }
    Ok(request)
}
/// `/v1/batches` line result body.
pub(crate) fn batch_response(id: &str, created: u64, model: &str, response: ChatResponse) -> Value {
    complete_body(id, created, model, response)
}

fn usage_json(usage: Usage) -> Option<Value> {
    // OpenAI's three required counts are emitted only when actually known.
    let input = match usage.billing {
        Some(b) => b.total_input_tokens?,
        None => usage.input_tokens?,
    };
    let output = usage.output_tokens?;
    let mut value = json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input.checked_add(output)?});
    if let Some(b) = usage.billing {
        let mut details = json!({});
        if let Some(n) = b.cache_read_input_tokens {
            details["cached_tokens"] = n.into();
        }
        if let Some(n) = b.cache_write_input_tokens {
            details["cache_write_tokens"] = n.into();
        }
        if details.as_object().is_some_and(|d| !d.is_empty()) {
            value["prompt_tokens_details"] = details;
        }
    }
    Some(value)
}

fn error_body(error: InferenceError) -> Value {
    super::openai_error_body(error)
}
pub(super) fn error_response(error: InferenceError) -> Response {
    super::error_with_body(error, super::openai_error_body(error))
}

#[cfg(test)]
pub(crate) fn openapi_normalize(value: Value) -> Result<ChatRequest, InferenceError> {
    serde_json::from_value::<Request>(value)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(value: Value) -> Result<ChatRequest, InferenceError> {
        serde_json::from_value::<Request>(value)
            .map_err(|_| InferenceError::InvalidRequest)?
            .normalize()
    }
    #[test]
    fn nullable_stream_defaults_only_on_the_supported_field() {
        for (extra, streaming) in [
            (json!({}), false),
            (json!({"stream":null}), false),
            (json!({"stream":false}), false),
            (json!({"stream":true}), true),
        ] {
            let mut value = json!({"model":"m","messages":[{"role":"user","content":"hello"}]});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let request = parse(value.clone()).unwrap();
            assert_eq!(request.stream, streaming);
            assert_eq!(batch_request(value).is_err(), streaming);
        }
        for extra in [
            json!({"stream":0}),
            json!({"stream":"false"}),
            json!({"tools":null}),
            json!({"unknown":null}),
            json!({"response_format":null}),
            json!({"stream":true,"stream_options":{"include_usage":null}}),
        ] {
            let mut value = json!({"model":"m","messages":[{"role":"user","content":"hello"}]});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(parse(value).is_err());
        }
    }
    #[test]
    fn nullable_assistant_refusal_is_no_refusal_not_an_extension() {
        let base = json!({"model":"m","messages":[{"role":"assistant","content":"hello"}]});
        let mut value = base.clone();
        value["messages"][0]["refusal"] = Value::Null;
        assert_eq!(
            parse(value.clone()).unwrap().messages[0]
                .content
                .as_ref()
                .and_then(crate::inference::types::Content::text),
            Some("hello")
        );
        assert!(batch_request(value).is_ok());
        for refusal in [
            json!("no"),
            json!(""),
            json!(false),
            json!(0),
            json!([]),
            json!({}),
        ] {
            let mut value = base.clone();
            value["messages"][0]["refusal"] = refusal;
            assert!(parse(value.clone()).is_err());
            assert!(batch_request(value).is_err());
        }
        for message in [
            json!({"role":"user","content":"hello","refusal":null}),
            json!({"role":"system","content":"hello","refusal":null}),
            json!({"role":"assistant","refusal":null}),
            json!({"role":"assistant","content":null,"refusal":null}),
            json!({"role":"assistant","content":"hello","parsed":null}),
            json!({"role":"assistant","content":"hello","unknown":null}),
        ] {
            assert!(parse(json!({"model":"m","messages":[message]})).is_err());
        }
    }
    #[test]
    fn accepts_text_and_tools_without_provider_types_in_engine() {
        let request = parse(json!({"model":"company/smart","messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"weather"}},"max_completion_tokens":32})).unwrap();
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.max_output_tokens, Some(32));
    }
    #[test]
    fn omitted_function_parameters_mean_no_arguments_without_implying_strict() {
        let request = |function| {
            json!({"model":"m","messages":[{"role":"user","content":"ping"}],
                "tools":[{"type":"function","function":function}]})
        };
        for strict in [None, Some(false), Some(true)] {
            let mut function = json!({"name":"ping"});
            if let Some(strict) = strict {
                function["strict"] = json!(strict);
            }
            let r = parse(request(function.clone())).unwrap();
            // A closed object with no properties accepts only {}, not arbitrary
            // arguments (unlike {} or {"type":"object"}).
            assert_eq!(
                r.tools[0].parameters,
                json!({"type":"object","properties":{},"additionalProperties":false})
            );
            assert_eq!(r.tools[0].strict, strict);
            for schema in [
                json!({}),
                json!({"type":"object"}),
                json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}),
            ] {
                function["parameters"] = schema.clone();
                let r = parse(request(function.clone())).unwrap();
                assert_eq!(r.tools[0].parameters, schema);
                assert_eq!(r.tools[0].strict, strict);
            }
        }
        for parameters in [
            Value::Null,
            json!(false),
            json!(0),
            json!("object"),
            json!([]),
        ] {
            assert!(parse(request(json!({"name":"ping","parameters":parameters}))).is_err());
        }
        assert!(parse(request(json!({"name":"ping","extra":true}))).is_err());
        for strict in [json!("false"), json!(0), json!({})] {
            assert!(parse(request(json!({"name":"ping","strict":strict}))).is_err());
        }
    }
    #[test]
    fn legacy_max_tokens_is_an_alias_of_max_completion_tokens() {
        let base = json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        let with = |extra: Value| {
            let mut v = base.clone();
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            parse(v)
        };
        assert_eq!(
            with(json!({"max_tokens":7})).unwrap().max_output_tokens,
            Some(7)
        );
        assert_eq!(
            with(json!({"max_completion_tokens":7}))
                .unwrap()
                .max_output_tokens,
            Some(7)
        );
        assert_eq!(
            with(json!({"max_tokens":7,"max_completion_tokens":7}))
                .unwrap()
                .max_output_tokens,
            Some(7)
        );
        assert_eq!(with(json!({})).unwrap().max_output_tokens, None);
        for bad in [
            json!({"max_tokens":7,"max_completion_tokens":8}),
            json!({"max_tokens":0}),
            json!({"max_tokens":-1}),
            json!({"max_tokens":"7"}),
        ] {
            assert!(with(bad.clone()).is_err(), "{bad}");
        }
        // Batch lines share the same contract.
        let mut line = base.clone();
        line["max_tokens"] = json!(9);
        assert_eq!(batch_request(line).unwrap().max_output_tokens, Some(9));
        let mut line = base;
        line["max_tokens"] = json!(9);
        line["max_completion_tokens"] = json!(8);
        assert!(batch_request(line).is_err());
    }
    #[test]
    fn rejects_unsupported_fields_choices_and_multimodal_inputs() {
        for extra in [
            json!({"response_format":{"type":"json_object"}}),
            json!({"n":2}),
            json!({"temperature":3}),
            json!({"stream_options":{"include_usage":true}}),
            json!({"max_tokens":1,"max_completion_tokens":2}),
        ] {
            let mut request = json!({"model":"test","messages":[{"role":"user","content":"hi"}]});
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(parse(request).is_err());
        }
    }
    const PNG_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==";
    fn user(content: Value) -> Result<ChatRequest, InferenceError> {
        parse(json!({"model":"m","messages":[{"role":"user","content":content}]}))
    }
    #[test]
    fn text_parts_join_into_one_text() {
        let request = user(json!([{"type":"text","text":"a"},{"type":"text","text":"b"}])).unwrap();
        let content = request.messages[0].content.as_ref().unwrap();
        assert_eq!(content.text(), Some("ab"));
        assert!(!request.has_images());
        // Adjacent text parts join around an image too.
        let mixed = user(json!([{"type":"text","text":"a"},{"type":"text","text":"b"},{"type":"image_url","image_url":{"url":PNG_URL}}])).unwrap();
        let Some(Content::Parts(parts)) = &mixed.messages[0].content else {
            panic!("parts")
        };
        assert!(matches!(&parts[..], [Part::Text(t), Part::Image(_)] if t == "ab"));
        assert!(user(json!([])).is_err());
    }
    #[test]
    fn image_parts_keep_their_order_and_detail() {
        let request = user(json!([
            {"type":"text","text":"what is"},
            {"type":"image_url","image_url":{"url":PNG_URL,"detail":"high"}},
            {"type":"text","text":"this?"}
        ]))
        .unwrap();
        assert!(request.has_images());
        let Some(Content::Parts(parts)) = &request.messages[0].content else {
            panic!("parts")
        };
        assert!(matches!(&parts[0], Part::Text(t) if t == "what is"));
        let Part::Image(image) = &parts[1] else {
            panic!("image")
        };
        assert_eq!(image.data_url(), PNG_URL);
        assert_eq!(image.detail(), Some(ImageDetail::High));
        assert!(matches!(&parts[2], Part::Text(t) if t == "this?"));
    }
    #[test]
    fn remote_and_file_images_and_images_outside_user_messages_are_refused_by_name() {
        let field = |content: Value, role: &str| {
            parse(json!({"model":"m","messages":[{"role":role,"content":content}]})).err()
        };
        let image = |url: &str| json!([{"type":"image_url","image_url":{"url":url}}]);
        let named = Some(InferenceError::InvalidField("messages.content.image_url"));
        assert_eq!(field(image("https://example.invalid/a.png"), "user"), named);
        assert_eq!(
            field(
                image("data:image/jpeg;base64,iVBORw0KGgoAAAANSUhEUg=="),
                "user"
            ),
            named
        );
        for role in ["system", "developer", "assistant"] {
            assert_eq!(field(image(PNG_URL), role), named, "{role}");
        }
        assert_eq!(
            field(json!([{"type":"file","file":{"file_id":"file-1"}}]), "user"),
            Some(InferenceError::InvalidField("messages.content.file"))
        );
        // A reference beside another part's type is refused, never dropped.
        assert_eq!(
            field(
                json!([{"type":"text","text":"hi","file":{"file_id":"file-1"}}]),
                "user"
            ),
            Some(InferenceError::InvalidField("messages.content.file"))
        );
        assert_eq!(
            field(
                json!([{"type":"image_url","image_url":{"url":PNG_URL},"input_audio":{}}]),
                "user"
            ),
            Some(InferenceError::InvalidField("messages.content.input_audio"))
        );
        assert_eq!(
            field(
                json!([{"type":"input_audio","input_audio":{"data":"","format":"wav"}}]),
                "user"
            ),
            Some(InferenceError::InvalidField("messages.content.input_audio"))
        );
        let many: Vec<Value> = (0..21)
            .map(|_| json!({"type":"image_url","image_url":{"url":PNG_URL}}))
            .collect();
        assert_eq!(field(Value::Array(many), "user"), named);
    }
    #[test]
    fn batch_lines_refuse_images() {
        let line = json!({"model":"m","messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":PNG_URL}}]}]});
        assert_eq!(
            batch_request(line).err(),
            Some(InferenceError::InvalidField("messages.content.image_url"))
        );
    }
    #[test]
    fn missing_usage_is_not_fabricated_as_zero() {
        assert!(usage_json(Usage::default()).is_none());
    }
    #[test]
    fn exclusive_provider_usage_is_rendered_as_inclusive_chat_numbers() {
        let usage = Usage {
            input_tokens: Some(4),
            output_tokens: Some(3),
            billing: Some(crate::billing::BillingUsage {
                total_input_tokens: Some(34),
                uncached_input_tokens: Some(4),
                cache_read_input_tokens: Some(10),
                cache_write_input_tokens: Some(20),
                cache_write_default_input_tokens: Some(0),
                cache_write_5m_input_tokens: Some(8),
                cache_write_1h_input_tokens: Some(12),
            }),
            ..Default::default()
        };
        let value = usage_json(usage).unwrap();
        assert_eq!(value["prompt_tokens"], 34);
        assert_eq!(value["total_tokens"], 37);
        assert_eq!(value["prompt_tokens_details"]["cache_write_tokens"], 20);
        let usage = Usage {
            billing: Some(crate::billing::BillingUsage {
                total_input_tokens: None,
                ..usage.billing.unwrap()
            }),
            ..usage
        };
        assert!(usage_json(usage).is_none());
    }
}
