//! The OpenAI Chat wire stream decoder, shared with OpenRouter and the local
//! profiles.
use super::*;

#[derive(Default)]
pub(crate) struct StreamState {
    profile: Option<&'static str>,
    messages: bool,
    finished: bool,
    used: bool,
    /// Text or a tool call was emitted; a refusal can no longer start.
    emitted: bool,
    /// A refusal started: no output may follow, and the finish is Refusal.
    refused: bool,
    /// Refusal text received; it is never emitted but stays bounded by the
    /// Messages output limit.
    pub(super) refusal_bytes: usize,
    /// First valid `model` any chunk reported; carried on the usage event.
    model: Option<ReportedModel>,
}

impl StreamState {
    /// The decoder of one stream: `profile` for local servers, `messages` for
    /// a Messages request (local reasoning text, refusals, error classes).
    pub(crate) fn new(profile: Option<&'static str>, messages: bool) -> Self {
        Self {
            profile,
            messages,
            ..Self::default()
        }
    }
    pub(crate) fn decode(&mut self, data: &[u8]) -> Result<Vec<ChatEvent>> {
        if data == b"[DONE]" {
            if !self.finished {
                return Err(InferenceError::InvalidUpstream);
            }
            return Ok(vec![ChatEvent::Done]);
        }
        let value: Value =
            serde_json::from_slice(data).map_err(|_| InferenceError::InvalidUpstream)?;
        object(&value)?;
        if !empty(&value["error"]) {
            return Err(super::super::stream_error::openai_chat(
                &value,
                self.messages,
            ));
        }
        if self.model.is_none() {
            self.model = ReportedModel::from_json(&value["model"]);
        }
        let choices = array(&value["choices"])?;
        if choices.len() > 1 {
            return Err(InferenceError::InvalidUpstream);
        }
        let mut events = Vec::new();
        if let Some(choice) = choices.first() {
            if self.finished || choice["index"].as_u64() != Some(0) {
                return Err(InferenceError::InvalidUpstream);
            }
            known_profile_fields(
                choice,
                &["index", "delta", "finish_reason", "logprobs"],
                self.profile,
                LOCAL_STOP_FIELDS,
            )?;
            let delta = &choice["delta"];
            let accepts_refusal = accepts_refusal(self.profile, self.messages);
            known_profile_fields(
                delta,
                if accepts_refusal {
                    REFUSAL_MESSAGE_FIELDS
                } else {
                    MESSAGE_FIELDS
                },
                self.profile,
                LOCAL_REASONING_FIELDS,
            )?;
            if !delta["role"].is_null() && delta["role"].as_str() != Some("assistant") {
                return Err(InferenceError::InvalidUpstream);
            }
            let text = optional_string(&delta["content"])?;
            let tool_calls = delta_tool_calls(delta)?;
            if self.messages
                && self.profile.is_some()
                && let Some(reasoning) = local_reasoning(delta)?
            {
                events.push(ChatEvent::Reasoning(reasoning));
            }
            let output = text.as_deref().is_some_and(|t| !t.is_empty()) || !tool_calls.is_empty();
            self.refusal(delta, output, accepts_refusal)?;
            if text.is_some() || !tool_calls.is_empty() {
                events.push(ChatEvent::Delta { text, tool_calls });
            }
            if !choice["finish_reason"].is_null() {
                let reason = finish(&choice["finish_reason"])?;
                events.push(ChatEvent::Finish(if self.refused {
                    FinishReason::Refusal
                } else {
                    reason
                }));
                self.finished = true;
            }
        } else if value["usage"].is_null() {
            return Err(InferenceError::InvalidUpstream);
        }
        if !value["usage"].is_null() {
            if self.used || !self.finished {
                return Err(InferenceError::InvalidUpstream);
            }
            let mut usage = match self.profile {
                Some(profile) => local_usage(&value["usage"], profile)?,
                None => usage(&value["usage"])?,
            };
            usage.reported_model = self.model;
            events.push(ChatEvent::Usage(usage));
            self.used = true;
        }
        Ok(events)
    }
}

impl StreamState {
    /// A refusal is the whole reply: it cannot follow or precede output, and
    /// its text is never emitted but stays within the Messages output bound.
    fn refusal(&mut self, delta: &Value, output: bool, accepts: bool) -> Result<()> {
        if accepts && let Some(piece) = refusal(&delta["refusal"])? {
            self.refusal_bytes = self.refusal_bytes.saturating_add(piece.len());
            if self.emitted
                || output
                || self.refusal_bytes > crate::protocols::messages::OUTPUT_LIMIT
            {
                return Err(InferenceError::InvalidUpstream);
            }
            self.refused = true;
        }
        if self.refused && output {
            return Err(InferenceError::InvalidUpstream);
        }
        self.emitted |= output;
        Ok(())
    }
}

/// The tool-call fragments of one delta, each index at most once.
fn delta_tool_calls(delta: &Value) -> Result<Vec<ToolCallDelta>> {
    let mut tool_calls = Vec::new();
    if !delta["tool_calls"].is_null() {
        for tool in array(&delta["tool_calls"])? {
            known_fields(tool, &["index", "id", "type", "function"])?;
            if !tool["type"].is_null() && tool["type"].as_str() != Some("function") {
                return Err(InferenceError::InvalidUpstream);
            }
            let index = tool["index"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or(InferenceError::InvalidUpstream)?;
            if index as usize >= MAX_TOOL_CALLS
                || tool_calls.iter().any(|t: &ToolCallDelta| t.index == index)
            {
                return Err(InferenceError::InvalidUpstream);
            }
            let function = &tool["function"];
            if !function.is_null() {
                known_fields(function, &["name", "arguments"])?;
            }
            tool_calls.push(ToolCallDelta {
                index,
                id: optional_string(&tool["id"])?,
                name: optional_string(&function["name"])?,
                arguments: optional_string(&function["arguments"])?,
            });
        }
    }
    Ok(tool_calls)
}
