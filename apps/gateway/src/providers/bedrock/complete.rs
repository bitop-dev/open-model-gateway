//! Converse (non-streaming) reply decoding.
use super::*;

/// `messages` keeps returned reasoning text for the Messages codec (as unsigned
/// `thinking`); other protocols have no reasoning output. Signatures and
/// redacted reasoning are never returned: translated routes return unsigned
/// thinking only. Reasoning shares the reply's output bound either way.
pub(super) fn decode_complete_limit(
    output: aws_sdk_bedrockruntime::operation::converse::ConverseOutput,
    limit: usize,
    messages: bool,
) -> Result<ChatResponse> {
    let Some(aws::ConverseOutput::Message(message)) = output.output else {
        return Err(InferenceError::InvalidUpstream);
    };
    if message.role != aws::ConversationRole::Assistant || message.content.len() > MAX_BLOCKS {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    let mut ids = BTreeSet::new();
    let mut size = 0;
    for block in message.content {
        match block {
            aws::ContentBlock::Text(value) => {
                size += value.len();
                text.push_str(&value);
            }
            aws::ContentBlock::ReasoningContent(aws::ReasoningContentBlock::ReasoningText(
                value,
            )) => {
                size += value.text.len() + value.signature.as_ref().map_or(0, String::len);
                reasoning.push_str(&value.text);
            }
            aws::ContentBlock::ReasoningContent(aws::ReasoningContentBlock::RedactedContent(
                value,
            )) => size += value.as_ref().len(),
            aws::ContentBlock::ToolUse(value) => {
                if !identifier(&value.tool_use_id)
                    || !identifier(&value.name)
                    || !ids.insert(value.tool_use_id.clone())
                    || calls.len() >= MAX_TOOLS
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                let args = json_document(&value.input)?;
                if !args.is_object() {
                    return Err(InferenceError::InvalidUpstream);
                }
                let arguments =
                    serde_json::to_string(&args).map_err(|_| InferenceError::InvalidUpstream)?;
                size += arguments.len() + value.name.len() + value.tool_use_id.len();
                calls.push(ToolCall {
                    id: value.tool_use_id,
                    name: value.name,
                    arguments,
                });
            }
            _ => return Err(InferenceError::InvalidUpstream),
        }
        if size > limit {
            return Err(InferenceError::InvalidUpstream);
        }
    }
    let reason = finish(&output.stop_reason)?;
    if (reason == FinishReason::ToolCalls) != !calls.is_empty() {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut usage = output
        .usage
        .as_ref()
        .map(usage)
        .transpose()?
        .unwrap_or_default();
    usage.reported_model =
        invoked_model(output.trace.as_ref().and_then(|t| t.prompt_router.as_ref()));
    Ok(ChatResponse {
        messages_content: None,
        reasoning: (messages && !reasoning.is_empty()).then_some(reasoning),
        content: (!text.is_empty()).then_some(text),
        tool_calls: calls,
        finish_reason: reason,
        usage,
    })
}
/// Telemetry only: Converse reports the served model only for prompt routers
/// (`trace.promptRouter.invokedModelId`). Otherwise it is unknown and Logs
/// show the configured model id. Invalid values are unknown, never an error.
pub(super) fn invoked_model(trace: Option<&aws::PromptRouterTrace>) -> Option<ReportedModel> {
    trace
        .and_then(|t| t.invoked_model_id.as_deref())
        .and_then(ReportedModel::parse)
}
