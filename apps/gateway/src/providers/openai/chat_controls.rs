//! OpenAI Chat's controls for Messages fields (docs/protocol-matrix.md,
//! "Reasoning and structured output on OpenAI-style routes").
use super::{ChatRequest, Value, encode, json};

/// OpenAI Chat's own wire: the shared encoding plus OpenAI's controls for
/// Messages fields. Interactive requests and native batch lines both use it.
pub(in crate::providers) fn encode_chat(model: &str, request: &ChatRequest) -> Value {
    let mut value = encode(model, request);
    let config = request
        .messages_options
        .as_ref()
        .and_then(|o| o.output_config.as_ref());
    // `reasoning_effort` takes the same five values as Messages effort. Chat
    // has no thinking budget and no on/off switch every reasoning model
    // accepts, so `thinking` itself is dropped (docs/protocol-matrix.md).
    if let Some(effort) = config.and_then(|c| c.get("effort")) {
        value["reasoning_effort"] = effort.clone();
    }
    if let Some(format) = config.and_then(|c| c.get("format")) {
        value["response_format"] = json_schema_format(&format["schema"]);
    }
    value
}

/// OpenAI-wire structured output for a Messages `output_config.format`.
/// Messages output is schema-constrained, so `strict` is always true; the
/// Messages format has no name and OpenAI requires one, so it is fixed.
pub(in crate::providers) fn json_schema_format(schema: &Value) -> Value {
    json!({"type":"json_schema","json_schema":{"name":"output","schema":schema,"strict":true}})
}
