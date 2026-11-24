//! Explicit Messages provider policy. Native fidelity is opt-in, never passthrough.
use crate::inference::{
    error::InferenceError,
    types::{ChatRequest, ProviderOutput},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderClass {
    Anthropic,
    OpenAi,
    Bedrock,
    Local,
}

/// Open beta list, except unpriced long-context premiums and unsupported
/// hosted/file capabilities. Body capabilities and cache TTL prices are gated
/// independently; never reject extended-cache-ttl merely for being a beta.
pub(crate) const DENIED_BETA_PREFIXES: &[&str] = &[
    "context-1m-",
    "code-execution-",
    "web-search-",
    "web-fetch-",
    "files-api-",
    "mcp-client-",
    "computer-use-",
];

/// Denied names are reflected only by the Messages HTTP codec to support
/// Claude Code's capability recovery. Never put them in inference telemetry.
pub(crate) enum BetaHeaderError {
    Invalid,
    Denied(Vec<String>),
}
impl std::fmt::Debug for BetaHeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid or denied anthropic-beta header")
    }
}

/// Validate every comma-separated token while retaining original header bytes.
/// Shared by the HTTP frontend and the pre-admission/dispatch adapter policy.
pub(crate) fn validate_beta_headers<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), BetaHeaderError> {
    let invalid = || BetaHeaderError::Invalid;
    let mut tokens = 0_usize;
    let mut denied = Vec::new();
    for (index, value) in values.into_iter().enumerate() {
        if index >= 16
            || value.is_empty()
            || value.len() > 1024
            || !value
                .bytes()
                .all(|b| b == b'\t' || b == b' ' || b.is_ascii_graphic())
        {
            return Err(invalid());
        }
        tokens = value.split(',').try_fold(tokens, |count, token| {
            let token = token.trim_matches([' ', '\t']);
            if count >= 32 || token.is_empty() {
                return Err(invalid());
            }
            if DENIED_BETA_PREFIXES.iter().any(|prefix| {
                token
                    .get(..prefix.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
            }) {
                denied.push(token.to_owned());
            }
            Ok(count + 1)
        })?;
    }
    if denied.is_empty() {
        Ok(())
    } else {
        Err(BetaHeaderError::Denied(denied))
    }
}

/// A `thinking` history block carries a signature only when it has a
/// nonblank one: SDK accumulators replay a translated (unsigned) block with
/// `"signature": ""`, which Anthropic would reject after admission. Translated
/// routes remove thinking history whatever its signature.
fn signed(block: &serde_json::Value) -> bool {
    block
        .get("signature")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

pub(crate) fn validate(request: &ChatRequest, class: ProviderClass) -> Result<(), InferenceError> {
    let Some(options) = &request.messages_options else {
        return Ok(());
    };
    let refuse = |field| Err(InferenceError::InvalidField(field));
    if class != ProviderClass::Anthropic && !options.images.is_empty() {
        return refuse("image");
    }
    // OpenAI-style adapters map structured output and strict tools to their
    // JSON-schema controls; no other route has a faithful mapping yet.
    if class != ProviderClass::OpenAi {
        if options
            .output_config
            .as_ref()
            .is_some_and(|v| v.get("format").is_some())
        {
            return refuse("output_config.format");
        }
        if options.strict_tools {
            return refuse("strict");
        }
    }
    // A native route forwards history with its signature; an unsigned block
    // (a translated reply replayed) cannot be forwarded and is refused.
    if class == ProviderClass::Anthropic
        && options
            .history_thinking
            .iter()
            .any(|b| b["type"] == "thinking" && !signed(b))
    {
        return refuse("thinking");
    }
    if class == ProviderClass::Bedrock {
        // The Converse mapping cannot carry these Messages controls.
        // Both admission and dispatch use this encoder policy.
        if options.thinking.is_some() || !options.history_thinking.is_empty() {
            return refuse("thinking");
        }
        if !options.cache_controls.is_empty() {
            return refuse("cache_control");
        }
        if options.context_management.is_some() {
            return refuse("context_management");
        }
        if !options.defer_loading.is_empty() {
            return refuse("defer_loading");
        }
    }
    if class != ProviderClass::Anthropic {
        if !options.history_thinking.is_empty()
            && request
                .messages
                .iter()
                .all(|m| m.content.is_none() && m.tool_calls.is_empty())
        {
            // Preserve the previous translated refusal when removing foreign
            // history leaves no text/tool conversation at all.
            return Err(InferenceError::InvalidRequest);
        }
        if options.top_k.is_some() {
            return refuse("top_k");
        }
    }
    validate_beta_headers(options.beta_headers.iter().map(String::as_str))
        .map_err(|_| InferenceError::InvalidField("anthropic-beta"))
}

/// Translated reasoning text follows Anthropic's rule for thinking text: it is
/// returned only when the client asked for thinking (`enabled` or `adaptive`)
/// without `display:"omitted"`. Otherwise it is withheld, as a native route
/// would never return it; its tokens stay in the reported output count.
pub(crate) fn shows_thinking(request: &ChatRequest) -> bool {
    request
        .messages_options
        .as_ref()
        .and_then(|o| o.thinking.as_ref())
        .is_some_and(|t| t["type"] != "disabled" && t["display"] != "omitted")
}

/// Withholds unrequested reasoning from a complete reply (gateway-run batch
/// lines have no other place to apply it). The whole body was already read
/// within the response bound. Streams keep their reasoning events so the
/// attempt and output-bound checks see them; the Messages codec withholds
/// them after those checks.
pub(crate) fn thinking_display(request: &ChatRequest, output: ProviderOutput) -> ProviderOutput {
    match output {
        ProviderOutput::Complete(mut response) if !shows_thinking(request) => {
            response.reasoning = None;
            ProviderOutput::Complete(response)
        }
        output => output,
    }
}
