//! OpenRouter's Chat Completions wire: its Messages controls, and the
//! decoding of a complete answer and of a stream. OpenRouter-only members
//! are type-checked and removed here, so the shared OpenAI decoder still
//! refuses every other unknown content field.
use super::*;

/// Remove OpenRouter-only choice fields after type-checking them, so the
/// shared OpenAI decoder still rejects every other unknown content field.
/// Returns the nonempty `reasoning` text (the decoder allows one choice).
fn normalize_choices(value: &mut Value) -> Result<Option<String>> {
    let mut reasoning = None;
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
        return Ok(reasoning);
    };
    for choice in choices {
        let Some(choice) = choice.as_object_mut() else {
            return Err(InferenceError::InvalidUpstream);
        };
        if let Some(native) = choice.remove("native_finish_reason")
            && !(native.is_null() || native.is_string())
        {
            return Err(InferenceError::InvalidUpstream);
        }
        for key in ["message", "delta"] {
            if let Some(message) = choice.get_mut(key).and_then(Value::as_object_mut) {
                match message.remove("reasoning") {
                    Some(Value::String(text)) if !text.is_empty() => reasoning = Some(text),
                    Some(text) if !(text.is_null() || text.is_string()) => {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    _ => {}
                }
                if let Some(details) = message.remove("reasoning_details")
                    && !(details.is_null() || details.is_array())
                {
                    return Err(InferenceError::InvalidUpstream);
                }
            }
        }
    }
    Ok(reasoning)
}

/// OpenRouter's controls for Messages fields. `reasoning` takes an effort or
/// a token budget, never both: an explicit effort wins over a thinking budget,
/// and disabled thinking turns reasoning off whatever the effort. Thinking
/// `display:"omitted"` asks OpenRouter not to return the text. Structured
/// output and strict tools set `require_parameters`, so OpenRouter routes only
/// to providers that support every sent parameter rather than ignoring them.
pub(super) fn messages_controls(body: &mut Value, request: &ChatRequest) {
    let Some(options) = &request.messages_options else {
        return;
    };
    let config = options.output_config.as_ref();
    let mut reasoning = serde_json::Map::new();
    if let Some(thinking) = &options.thinking
        && thinking["type"] == "disabled"
    {
        reasoning.insert("enabled".into(), json!(false));
    } else {
        if let Some(effort) = config.and_then(|c| c.get("effort")) {
            reasoning.insert("effort".into(), effort.clone());
        } else if let Some(thinking) = &options.thinking {
            match thinking.get("budget_tokens") {
                Some(budget) => reasoning.insert("max_tokens".into(), budget.clone()),
                None => reasoning.insert("enabled".into(), json!(true)),
            };
        }
        if options
            .thinking
            .as_ref()
            .is_some_and(|t| t["display"] == "omitted")
        {
            reasoning.insert("exclude".into(), json!(true));
        }
    }
    if !reasoning.is_empty() {
        body["reasoning"] = Value::Object(reasoning);
    }
    let format = config.and_then(|c| c.get("format"));
    if let Some(format) = format {
        body["response_format"] = openai::json_schema_format(&format["schema"]);
    }
    if format.is_some() || request.tools.iter().any(|t| t.strict == Some(true)) {
        body["provider"]["require_parameters"] = json!(true);
    }
}

pub(super) fn decode_chat(bytes: &[u8], messages: bool) -> Result<ChatResponse> {
    let mut value = object(bytes)?;
    let cost = provider_cost(bytes);
    embedded_error(
        &value,
        &["choices"],
        &["usage"],
        &cost,
        |code| error_status(&value, messages, code),
        || {
            value["usage"]
                .is_object()
                .then(|| openai::usage(&value["usage"]))
        },
    )?;
    let cost = cost?;
    let reasoning = normalize_choices(&mut value)?;
    let mut response = openai::decode_complete_protocol(&value, None, messages)?;
    response.usage.provider_cost_microusd = cost;
    // Only Messages clients receive the trace.
    response.reasoning = reasoning.filter(|_| messages);
    Ok(response)
}

/// Normalize one SSE data frame. `finish` is the first finish reason seen.
/// Returns the frame for the shared decoder, its cost and reasoning text.
fn normalize_frame(
    data: &[u8],
    finish: &mut Option<String>,
    messages: bool,
) -> Result<(Vec<u8>, Option<i64>, Option<String>)> {
    if data == b"[DONE]" {
        return Ok((data.to_vec(), None, None));
    }
    let mut value: Value =
        serde_json::from_slice(data).map_err(|_| InferenceError::InvalidUpstream)?;
    if !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    stream_embedded_error(&value, messages)?;
    let cost = provider_cost(data)?;
    let reasoning = normalize_choices(&mut value)?;
    // For Messages, a frame carrying reasoning is content, never the
    // accounting repeat; other clients never receive reasoning.
    if let Some(previous) = finish.as_deref()
        && (reasoning.is_none() || !messages)
        && !value["usage"].is_null()
        && let Some([choice]) = value["choices"].as_array().map(Vec::as_slice)
    {
        let delta = &choice["delta"];
        let accounting = delta.as_object().is_some_and(|d| {
            d.iter().all(|(k, v)| match k.as_str() {
                "content" | "tool_calls" => empty(v),
                "role" => v.is_null() || v == "assistant",
                _ => false,
            })
        }) && (choice["finish_reason"].is_null()
            || choice["finish_reason"].as_str() == Some(previous))
            && choice["index"].as_u64() == Some(0);
        if accounting {
            value["choices"] = json!([]);
        }
    }
    if finish.is_none()
        && let Some(reason) = value["choices"][0]["finish_reason"].as_str()
    {
        *finish = Some(reason.to_owned());
    }
    Ok((
        serde_json::to_vec(&value).map_err(|_| InferenceError::InvalidUpstream)?,
        cost,
        reasoning,
    ))
}

pub(super) fn decode_chat_stream(response: reqwest::Response, messages: bool) -> EventStream {
    Box::pin(async_stream::try_stream! {
        let mut chunks = response.bytes_stream();
        let mut decoder = openai::SseDecoder::new();
        let mut state = openai::StreamState::new(None, messages);
        let mut finish = None;
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(transport)?;
            for &byte in chunk.iter() {
                if let Some(data) = decoder.push(byte)? {
                    let (frame, cost, reasoning) = normalize_frame(&data, &mut finish, messages)?;
                    // The frame is validated before its reasoning is released.
                    let events = state.decode(&frame)?;
                    if let Some(reasoning) = reasoning.filter(|_| messages) {
                        yield ChatEvent::Reasoning(reasoning);
                    }
                    for event in events {
                        let event = match event {
                            ChatEvent::Usage(mut usage) => {
                                usage.provider_cost_microusd = cost;
                                ChatEvent::Usage(usage)
                            }
                            other => other,
                        };
                        let done = matches!(event, ChatEvent::Done);
                        yield event;
                        if done { return; }
                    }
                }
            }
        }
        // Only [DONE] proves completion.
        Err(super::super::framing::TRANSPORT)?;
    })
}
