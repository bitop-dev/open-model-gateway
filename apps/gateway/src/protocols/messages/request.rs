//! Explicit Claude Code field grammar. Nothing here is a passthrough payload.
use super::super::responses::{message, name, string, validate};
use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    model: String,
    messages: Vec<WireMessage>,
    system: Option<Value>,
    max_tokens: u32,
    #[serde(default)]
    pub(super) stream: bool,
    // Outer `Some` = the key was sent (even as `null`); 2026-10-21 refuses the
    // key itself (#430). 2023-06-01 keeps treating `null` as absent.
    #[serde(default, deserialize_with = "present")]
    temperature: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    top_p: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    top_k: Option<Option<u32>>,
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    tools: Vec<Value>,
    tool_choice: Option<Value>,
    pub(super) metadata: Option<WireMetadata>,
    thinking: Option<Value>,
    output_config: Option<Value>,
    context_management: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireMetadata {
    pub(super) user_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMessage {
    role: String,
    content: Value,
}
fn present<'de, D, T>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
fn invalid(field: &'static str) -> InferenceError {
    InferenceError::InvalidField(field)
}
fn object(value: &Value, allowed: &[&str], field: &'static str) -> Result<()> {
    let values = value.as_object().ok_or_else(|| invalid(field))?;
    if let Some(unknown) = values.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(crate::inference::error::FieldName::parse(unknown)
            .map_or_else(|| invalid(field), InferenceError::UnknownField));
    }
    Ok(())
}
fn cache(block: &Value, options: &mut MessagesOptions) -> Result<()> {
    if let Some(v) = block.get("cache_control") {
        object(v, &["type", "ttl"], "cache_control")?;
        if v["type"] != "ephemeral" || v.get("ttl").is_some_and(|v| v != "5m" && v != "1h") {
            return Err(invalid("cache_control"));
        }
        options.cache_controls.push(v.clone());
    }
    Ok(())
}
// Version note (#430): `document`, `search_result` and `citations` are not in
// this grammar. anthropic-version 2026-10-21 changes their meaning, so adding
// any of them needs a version-aware rule (see `Request::check_version`).
fn text_blocks(value: &Value, options: &mut MessagesOptions) -> Result<Vec<String>> {
    if let Value::String(s) = value {
        return Ok(vec![s.clone()]);
    }
    let blocks = value.as_array().ok_or_else(|| invalid("content"))?;
    if blocks.len() > 256 {
        return Err(invalid("content"));
    }
    let mut text = vec![];
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                object(block, &["type", "text", "cache_control"], "text")?;
                cache(block, options)?;
                text.push(string(&block["text"])?);
            }
            _ => return Err(invalid("content.type")),
        }
    }
    Ok(text)
}
/// A base64 image block as the canonical image, validated as Chat and
/// Responses images are (`image_input::Images::take`: strict base64, the
/// declared type's signature, the decoded size, the header's dimensions).
/// No remote URL sources: neither gateway nor upstream fetches caller URLs.
fn image(block: &Value, options: &mut MessagesOptions) -> Result<ImageInput> {
    object(block, &["type", "source", "cache_control"], "image")?;
    cache(block, options)?;
    let source = &block["source"];
    if source["type"].as_str() != Some("base64") {
        return Err(invalid("image.source"));
    }
    object(source, &["type", "media_type", "data"], "image.source")?;
    let media_type = match source["media_type"].as_str() {
        Some(t @ ("image/png" | "image/jpeg" | "image/gif" | "image/webp")) => t,
        _ => return Err(invalid("image.source")),
    };
    let data = source["data"]
        .as_str()
        .ok_or_else(|| invalid("image.source.data"))?;
    if options.image_count >= crate::protocols::image_input::MAX_IMAGES {
        return Err(invalid("image"));
    }
    let image = crate::protocols::image_input::Images::default().take(
        format!("data:{media_type};base64,{data}"),
        None,
        "image.source",
    )?;
    options.image_count += 1;
    Ok(image)
}
/// Appends text to a canonical content, joined to the text before it.
fn push_text(content: &mut Option<Content>, text: String) {
    match content.get_or_insert_with(|| Content::Text(String::new())) {
        Content::Text(joined) => joined.push_str(&text),
        Content::Parts(parts) => match parts.last_mut() {
            Some(Part::Text(joined)) => joined.push_str(&text),
            _ => parts.push(Part::Text(text)),
        },
    }
}
/// Appends an image to a canonical content, which then holds parts.
fn push_image(content: &mut Option<Content>, image: ImageInput) {
    let mut parts = match content.take() {
        None => vec![],
        Some(Content::Text(text)) if text.is_empty() => vec![],
        Some(Content::Text(text)) => vec![Part::Text(text)],
        Some(Content::Parts(parts)) => parts,
    };
    parts.push(Part::Image(image));
    *content = Some(Content::Parts(parts));
}
fn thinking(value: &Value, maximum: u32) -> Result<()> {
    object(value, &["type", "budget_tokens", "display"], "thinking")?;
    match value["type"].as_str() {
        Some("adaptive" | "disabled") if value.get("budget_tokens").is_none() => {}
        Some("enabled")
            if value["budget_tokens"]
                .as_u64()
                .is_some_and(|n| n >= 1024 && n < maximum as u64) => {}
        _ => return Err(invalid("thinking")),
    }
    if value
        .get("display")
        .is_some_and(|v| !matches!(v.as_str(), Some("summarized" | "omitted")))
    {
        return Err(invalid("thinking.display"));
    }
    Ok(())
}
fn output(value: &Value) -> Result<()> {
    object(value, &["effort", "format", "task_budget"], "output_config")?;
    if let Some(v) = value.get("task_budget") {
        object(
            v,
            &["type", "total", "remaining"],
            "output_config.task_budget",
        )?;
        if v["type"] != "tokens"
            || v["total"].as_u64().is_none_or(|n| n == 0)
            || v.get("remaining").is_some_and(|n| n.as_u64().is_none())
        {
            return Err(invalid("output_config.task_budget"));
        }
    }
    if value.get("effort").is_some_and(|v| {
        !matches!(
            v.as_str(),
            Some("low" | "medium" | "high" | "xhigh" | "max")
        )
    }) {
        return Err(invalid("output_config.effort"));
    }
    if let Some(format) = value.get("format") {
        object(format, &["type", "schema"], "output_config.format")?;
        if format["type"] != "json_schema" || !format["schema"].is_object() {
            return Err(invalid("output_config.format"));
        }
    }
    Ok(())
}
fn context(value: &Value) -> Result<()> {
    object(value, &["edits"], "context_management")?;
    let edits = value["edits"]
        .as_array()
        .ok_or_else(|| invalid("context_management.edits"))?;
    if edits.len() > 256 {
        return Err(invalid("context_management.edits"));
    }
    for edit in edits {
        object(
            edit,
            &[
                "type",
                "trigger",
                "keep",
                "clear_at_least",
                "clear_tool_inputs",
                "exclude_tools",
            ],
            "context_management.edits",
        )?;
        if !matches!(
            edit["type"].as_str(),
            Some("clear_tool_uses_20250919" | "clear_thinking_20251015")
        ) {
            return Err(invalid("context_management.edits.type"));
        }
        for field in ["trigger", "keep", "clear_at_least"] {
            if let Some(v) = edit.get(field) {
                if v == "all" && field == "keep" {
                    continue;
                }
                object(v, &["type", "value"], "context_management.edits")?;
                if !matches!(
                    v["type"].as_str(),
                    Some("input_tokens" | "tool_uses" | "thinking_turns")
                ) || v["value"].as_u64().is_none()
                {
                    return Err(invalid("context_management.edits"));
                }
            }
        }
        if edit
            .get("clear_tool_inputs")
            .is_some_and(|v| !v.is_boolean())
            || edit.get("exclude_tools").is_some_and(|v| {
                v.as_array()
                    .is_none_or(|a| a.iter().any(|v| !v.is_string()))
            })
        {
            return Err(invalid("context_management.edits"));
        }
    }
    Ok(())
}
/// Sampling, limit and option bounds, in the order `normalize` has always
/// checked them; nothing is consumed here.
fn check_bounds(
    request: &Request,
    temperature: Option<f64>,
    top_p: Option<f64>,
    top_k: Option<u32>,
) -> Result<()> {
    if request.messages.is_empty()
        || request.messages.len() > 4096
        || request.max_tokens == 0
        || temperature.is_some_and(|v| !(0.0..=1.0).contains(&v))
    {
        return Err(InferenceError::InvalidRequest);
    }
    if top_p.is_some_and(|v| !(0.0..=1.0).contains(&v)) {
        return Err(invalid("top_p"));
    }
    if top_k == Some(0) {
        return Err(invalid("top_k"));
    }
    if request
        .stop_sequences
        .as_ref()
        .is_some_and(|v| v.len() > 64 || v.iter().any(String::is_empty))
    {
        return Err(invalid("stop_sequences"));
    }
    if let Some(v) = &request.thinking {
        thinking(v, request.max_tokens)?;
    }
    if let Some(v) = &request.output_config {
        output(v)?;
    }
    if let Some(v) = &request.context_management {
        context(v)?;
    }
    Ok(())
}
fn check_metadata(metadata: Option<WireMetadata>) -> Result<()> {
    // `metadata.user_id` is a client label only (recorded as the session
    // label by the protocol layer); it is validated, never forwarded.
    let user_id = metadata.and_then(|m| m.user_id);
    if user_id.as_ref().is_some_and(|u| u.chars().count() > 256) {
        return Err(invalid("metadata.user_id"));
    }
    Ok(())
}
/// The native envelope of the messages. An image block's base64 is not
/// copied: its `data` is the placeholder of that image (numbered in wire
/// order, as the parser takes images), and the body sent splices the
/// canonical image's base64 in (`image_body::json_body_base64`), so an
/// image's bytes are held once.
fn native_messages(wire: &[WireMessage]) -> Vec<Value> {
    let mut images = 0;
    wire.iter()
        .map(|m| {
            let mut content = match &m.content {
                Value::String(text) => json!([{"type":"text","text":text}]),
                Value::Array(blocks) => Value::Array(
                    blocks
                        .iter()
                        .map(|block| native_block(block, &mut images))
                        .collect(),
                ),
                other => other.clone(),
            };
            if let Some(blocks) = content.as_array_mut() {
                for block in blocks {
                    if block["type"] == "tool_result" && block.get("content").is_none() {
                        block["content"] = json!("");
                    }
                }
            }
            json!({"role":m.role,"content":content})
        })
        .collect()
}
/// One content block of the native envelope, at the two places the parser
/// takes images: a message's content and a tool result's content.
fn native_block(block: &Value, images: &mut usize) -> Value {
    match block["type"].as_str() {
        Some("image") => without_data(block, images),
        // Built field by field, so the content's images are never cloned.
        Some("tool_result") => match (block.as_object(), block.get("content")) {
            (Some(fields), Some(Value::Array(inner))) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        let value = if key == "content" {
                            Value::Array(
                                inner
                                    .iter()
                                    .map(|b| {
                                        if b["type"] == "image" {
                                            without_data(b, images)
                                        } else {
                                            b.clone()
                                        }
                                    })
                                    .collect(),
                            )
                        } else {
                            value.clone()
                        };
                        (key.clone(), value)
                    })
                    .collect(),
            ),
            _ => block.clone(),
        },
        _ => block.clone(),
    }
}
/// An image block with its source's `data` replaced by the next placeholder.
fn without_data(block: &Value, images: &mut usize) -> Value {
    let Some(fields) = block.as_object() else {
        return block.clone();
    };
    let mut out = serde_json::Map::new();
    for (key, value) in fields {
        let value = match (key.as_str(), value.as_object()) {
            ("source", Some(source)) => Value::Object(
                source
                    .iter()
                    .map(|(k, v)| {
                        let v = if k == "data" {
                            json!(crate::providers::image_body::placeholder(*images))
                        } else {
                            v.clone()
                        };
                        (k.clone(), v)
                    })
                    .collect(),
            ),
            _ => value.clone(),
        };
        out.insert(key.clone(), value);
    }
    *images += 1;
    Value::Object(out)
}
/// The bytes of the image sources' base64 in `messages`, at the two places
/// the parser takes images, read without a copy: what a large body may
/// hold beyond the text limit, checked before the request is normalized.
fn image_data_bytes(messages: &[WireMessage]) -> usize {
    let data = |block: &Value| {
        if block["type"] == "image" {
            block["source"]["data"].as_str().map_or(0, str::len)
        } else {
            0
        }
    };
    messages
        .iter()
        .filter_map(|m| m.content.as_array())
        .flatten()
        .map(|block| {
            data(block)
                + block["content"]
                    .as_array()
                    .filter(|_| block["type"] == "tool_result")
                    .map_or(0, |inner| inner.iter().map(data).sum())
        })
        .sum()
}
fn system_messages(
    system: Option<Value>,
    options: &mut MessagesOptions,
    messages: &mut Vec<Message>,
) -> Result<()> {
    if let Some(system) = system {
        for text in text_blocks(&system, options)? {
            messages.push(message(Role::System, text));
        }
    }
    Ok(())
}
/// One wire message; may append several canonical messages.
fn wire_message(
    wire: WireMessage,
    options: &mut MessagesOptions,
    messages: &mut Vec<Message>,
) -> Result<()> {
    let role = match wire.role.as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return Err(invalid("messages.role")),
    };
    if let Value::String(s) = wire.content {
        messages.push(message(role, s));
        return Ok(());
    }
    let blocks = wire
        .content
        .as_array()
        .ok_or_else(|| invalid("messages.content"))?;
    if blocks.is_empty() || blocks.len() > 256 {
        return Err(invalid("messages.content"));
    }
    // One wire assistant turn must remain one canonical turn: all
    // parallel calls need to precede their correlated tool results.
    // Text is consolidated within this wire message, never across it.
    let mut turn = Message {
        role,
        content: None,
        tool_calls: vec![],
        tool_call_id: None,
        tool_result_is_error: false,
    };
    for block in blocks {
        content_block(block, &mut turn, options, messages)?;
    }
    if turn.content.is_some() || !turn.tool_calls.is_empty() {
        messages.push(turn);
    }
    Ok(())
}
fn content_block(
    block: &Value,
    turn: &mut Message,
    options: &mut MessagesOptions,
    messages: &mut Vec<Message>,
) -> Result<()> {
    let role = turn.role;
    match block["type"].as_str() {
        Some("text") => {
            object(block, &["type", "text", "cache_control"], "text")?;
            cache(block, options)?;
            push_text(&mut turn.content, string(&block["text"])?);
        }
        // In any turn: native Anthropic takes an image in every turn, and a
        // translated route refuses one outside a user turn or tool result
        // (`providers::messages::validate`).
        Some("image") => push_image(&mut turn.content, image(block, options)?),
        Some("thinking") if role == Role::Assistant => {
            object(block, &["type", "thinking", "signature"], "thinking")?;
            string(&block["thinking"])?;
            // Translated replies return unsigned thinking, which clients
            // replay; `providers::messages` decides per route what it means.
            block.get("signature").map(string).transpose()?;
            options.history_thinking.push(block.clone());
        }
        Some("redacted_thinking") if role == Role::Assistant => {
            object(block, &["type", "data"], "redacted_thinking")?;
            string(&block["data"])?;
            options.history_thinking.push(block.clone());
        }
        Some("tool_use") if role == Role::Assistant => {
            turn.tool_calls.push(tool_use(block, options)?);
        }
        Some("tool_result") if role == Role::User => tool_result(block, turn, options, messages)?,
        Some("server_tool_use" | "web_search_tool_result" | "code_execution_tool_result") => {
            return Err(invalid("tools.type"));
        }
        _ => return Err(invalid("messages.content.type")),
    }
    Ok(())
}
fn tool_use(block: &Value, options: &mut MessagesOptions) -> Result<ToolCall> {
    object(
        block,
        &["type", "id", "name", "input", "cache_control"],
        "tool_use",
    )?;
    cache(block, options)?;
    let id = string(&block["id"])?;
    let n = string(&block["name"])?;
    if id.is_empty() || !name(&n) || !block["input"].is_object() {
        return Err(invalid("tool_use"));
    }
    Ok(ToolCall {
        id,
        name: n,
        arguments: block["input"].to_string(),
    })
}
fn tool_result(
    block: &Value,
    turn: &mut Message,
    options: &mut MessagesOptions,
    messages: &mut Vec<Message>,
) -> Result<()> {
    object(
        block,
        &[
            "type",
            "tool_use_id",
            "content",
            "is_error",
            "cache_control",
        ],
        "tool_result",
    )?;
    cache(block, options)?;
    let id = string(&block["tool_use_id"])?;
    if id.is_empty() {
        return Err(invalid("tool_result.tool_use_id"));
    }
    let is_error = block
        .get("is_error")
        .map(|flag| flag.as_bool().ok_or(InferenceError::InvalidRequest))
        .transpose()?
        .unwrap_or(false);
    let content = block
        .get("content")
        .map(|v| tool_content(v, options))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| Content::Text(String::new()));
    // Canonical tool results each carry one correlation id.
    // Keep any preceding user text in its original order;
    // adapters retain their normal correlation validation.
    if let Some(content) = turn.content.take() {
        messages.push(Message {
            content: Some(content),
            ..message(turn.role, String::new())
        });
    }
    messages.push(Message {
        role: Role::Tool,
        content: Some(content),
        tool_call_id: Some(id),
        tool_result_is_error: is_error,
        tool_calls: vec![],
    });
    Ok(())
}
/// A tool result's content: a string, or text and image blocks in order.
fn tool_content(value: &Value, options: &mut MessagesOptions) -> Result<Option<Content>> {
    if let Value::String(s) = value {
        return Ok(Some(Content::Text(s.clone())));
    }
    let blocks = value.as_array().ok_or_else(|| invalid("content"))?;
    if blocks.len() > 256 {
        return Err(invalid("content"));
    }
    let mut content = None;
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                object(block, &["type", "text", "cache_control"], "text")?;
                cache(block, options)?;
                push_text(&mut content, string(&block["text"])?);
            }
            Some("image") => push_image(&mut content, image(block, options)?),
            _ => return Err(invalid("content.type")),
        }
    }
    Ok(content)
}
fn function_tool(t: &Value, options: &mut MessagesOptions) -> Result<FunctionTool> {
    if t.get("type").is_some_and(|v| v != "custom") {
        return Err(invalid("tools.type"));
    }
    object(
        t,
        &[
            "type",
            "name",
            "description",
            "input_schema",
            "strict",
            "defer_loading",
            "cache_control",
        ],
        "tools",
    )?;
    cache(t, options)?;
    options.strict_tools |= t.get("strict").is_some();
    if let Some(v) = t.get("defer_loading") {
        v.as_bool().ok_or_else(|| invalid("defer_loading"))?;
        options.defer_loading.push(v.clone());
    }
    Ok(FunctionTool {
        name: string(&t["name"])?,
        description: t.get("description").map(string).transpose()?,
        parameters: t["input_schema"].clone(),
        strict: t
            .get("strict")
            .map(|v| v.as_bool().ok_or_else(|| invalid("strict")))
            .transpose()?,
    })
}
fn tool_choice(v: &Value) -> Result<ToolChoice> {
    object(
        v,
        &["type", "name", "disable_parallel_tool_use"],
        "tool_choice",
    )?;
    if let Some(flag) = v.get("disable_parallel_tool_use") {
        flag.as_bool()
            .ok_or_else(|| invalid("tool_choice.disable_parallel_tool_use"))?;
        // No shared parallel-tool control yet; refuse, never discard.
        return Err(invalid("tool_choice.disable_parallel_tool_use"));
    }
    Ok(match v["type"].as_str() {
        Some("auto") if v.get("name").is_none() => ToolChoice::Auto,
        Some("none") if v.get("name").is_none() => ToolChoice::None,
        Some("any") if v.get("name").is_none() => ToolChoice::Required,
        Some("tool") => ToolChoice::Function(string(&v["name"])?),
        _ => return Err(invalid("tool_choice")),
    })
}
impl Request {
    /// Version-specific request rules, checked before the shared grammar.
    /// `2026-10-21` removes the sampling parameters: any occurrence, even a
    /// default value or `null`, is refused rather than dropped.
    /// Returns the first refused field. Ill-typed values never get here: they
    /// fail deserialization with the generic 400 under either version.
    pub(super) fn check_version(
        &self,
        version: ApiVersion,
    ) -> std::result::Result<(), &'static str> {
        if version == ApiVersion::V2026_10_21 {
            for (field, sent) in [
                ("temperature", self.temperature.is_some()),
                ("top_p", self.top_p.is_some()),
                ("top_k", self.top_k.is_some()),
            ] {
                if sent {
                    return Err(field);
                }
            }
        }
        Ok(())
    }
    /// See [`image_data_bytes`].
    pub(super) fn image_data_bytes(&self) -> usize {
        image_data_bytes(&self.messages)
    }
    pub(super) fn normalize(self) -> Result<ChatRequest> {
        let temperature = self.temperature.flatten();
        let top_p = self.top_p.flatten();
        let top_k = self.top_k.flatten();
        check_bounds(&self, temperature, top_p, top_k)?;
        let task_budget = self
            .output_config
            .as_ref()
            .and_then(|v| v.get("task_budget"))
            .cloned();
        check_metadata(self.metadata)?;
        let mut options = MessagesOptions {
            top_p,
            top_k,
            stop_sequences: self.stop_sequences,
            system: self.system.clone(),
            thinking: self.thinking,
            output_config: self.output_config,
            context_management: self.context_management,
            task_budget,
            // These envelopes are used only after every nested field below has
            // passed the explicit grammar. Never retain top-level caller JSON.
            native_messages: native_messages(&self.messages),
            native_tools: self.tools.clone(),
            ..Default::default()
        };
        let mut messages = vec![];
        system_messages(self.system, &mut options, &mut messages)?;
        for wire in self.messages {
            wire_message(wire, &mut options, &mut messages)?;
        }
        // Thinking-only turns have no text representation, but still need a
        // canonical envelope for capability/admission validation. (An image
        // is a part of its turn's canonical message.)
        if messages.is_empty() && !options.history_thinking.is_empty() {
            messages.push(Message {
                role: Role::User,
                content: None,
                tool_calls: vec![],
                tool_call_id: None,
                tool_result_is_error: false,
            });
        }
        let tools = self
            .tools
            .iter()
            .map(|t| function_tool(t, &mut options))
            .collect::<Result<Vec<_>>>()?;
        let tool_choice = self.tool_choice.as_ref().map(tool_choice).transpose()?;
        validate(ChatRequest {
            model: self.model,
            messages,
            tools,
            tool_choice,
            max_output_tokens: Some(self.max_tokens),
            temperature,
            stream: self.stream,
            messages_options: Some(Box::new(options)),
        })
    }
}
#[cfg(test)]
pub(super) fn parse_request(value: Value) -> Result<ChatRequest> {
    serde_json::from_value::<Request>(value)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()
}

/// `POST /v1/messages/count_tokens` body: Anthropic's counting fields, under
/// the same grammar as a Messages request. Generation controls (`max_tokens`,
/// `stream`, sampling, `stop_sequences`, `metadata`, `context_management`)
/// are not part of a count and are refused as unknown fields, like any other
/// field outside the grammar.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountRequest {
    model: String,
    messages: Vec<WireMessage>,
    system: Option<Value>,
    #[serde(default)]
    tools: Vec<Value>,
    tool_choice: Option<Value>,
    thinking: Option<Value>,
    output_config: Option<Value>,
}
impl CountRequest {
    /// See [`image_data_bytes`].
    pub(super) fn image_data_bytes(&self) -> usize {
        image_data_bytes(&self.messages)
    }
    /// The canonical request a count is validated, routed and counted as.
    /// A count has no output bound: the grammar runs with the largest
    /// maximum (so `thinking.budget_tokens` needs only its own lower bound),
    /// and the result has none, so no adapter sends one.
    pub(super) fn normalize(self) -> Result<ChatRequest> {
        let mut request = Request {
            model: self.model,
            messages: self.messages,
            system: self.system,
            max_tokens: u32::MAX,
            stream: false,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            tools: self.tools,
            tool_choice: self.tool_choice,
            metadata: None,
            thinking: self.thinking,
            output_config: self.output_config,
            context_management: None,
        }
        .normalize()?;
        request.max_output_tokens = None;
        Ok(request)
    }
}
