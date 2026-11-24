//! Fields local OpenAI-wire servers add to Chat replies.
use super::{InferenceError, Result, Value, known_fields};

/// Local servers (SGLang, vLLM, Ollama) add stop metadata to a choice
/// (`matched_stop`, `stop_reason`: the matched stop token id or string) and,
/// with a reasoning parser, a separate reasoning trace to the message or delta
/// (`reasoning_content`, `reasoning`). As on OpenRouter, the trace reaches only
/// Messages clients (as unsigned thinking), never Chat or Responses clients;
/// its tokens stay in `completion_tokens` either way. The extras are accepted
/// only for local profiles and only with those scalar shapes.
pub(super) type LocalExtras = (&'static [&'static str], fn(&Value) -> bool);
pub(super) const LOCAL_STOP_FIELDS: LocalExtras = (&["matched_stop", "stop_reason"], |v| {
    v.is_null() || v.is_string() || v.is_u64() || v.is_i64()
});
pub(super) const LOCAL_REASONING_FIELDS: LocalExtras = (&["reasoning_content", "reasoning"], |v| {
    v.is_null() || v.is_string()
});
/// A local reasoning trace for a Messages client. Servers name the field
/// differently; a server that sends both must send the same text.
pub(super) fn local_reasoning(value: &Value) -> Result<Option<String>> {
    let texts: Vec<&str> = LOCAL_REASONING_FIELDS
        .0
        .iter()
        .filter_map(|key| value[*key].as_str())
        .filter(|text| !text.is_empty())
        .collect();
    match texts.as_slice() {
        [] => Ok(None),
        [text] => Ok(Some((*text).to_owned())),
        [first, second] if first == second => Ok(Some((*first).to_owned())),
        _ => Err(InferenceError::InvalidUpstream),
    }
}
pub(super) fn known_profile_fields(
    value: &Value,
    allowed: &[&str],
    profile: Option<&str>,
    (extras, valid): LocalExtras,
) -> Result<()> {
    if profile.is_none() {
        return known_fields(value, allowed);
    }
    if extras.iter().any(|key| !valid(&value[*key])) {
        return Err(InferenceError::InvalidUpstream);
    }
    let all: Vec<&str> = allowed.iter().chain(extras).copied().collect();
    known_fields(value, &all)
}
