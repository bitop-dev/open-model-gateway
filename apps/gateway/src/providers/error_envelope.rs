//! A provider error object inside a non-streaming HTTP 200 JSON body.
//!
//! Some providers, and proxies in front of them, answer a request they did
//! not serve with HTTP 200 and an `error` member. Such a body has the class
//! the same error has inside a returned stream (`super::stream_error`): the
//! class of the HTTP response whose status the error's documented code or
//! type stands for, in the adapter's own status table. So a recognised
//! rate-limit or quota error is `Busy`, exactly as an HTTP 429 is, and engine
//! failover and gateway batch retries treat it as they treat a 429; an
//! unrecognised error is `UpstreamUnavailable`, never a success and never a
//! client fault.
//!
//! That holds only for a clean envelope. A body that also carries anything
//! the provider may have produced for this request (a non-empty output
//! member, or usage or cost that is not all zero) does not show that the
//! request was refused before processing, so the attempt fails as
//! `InvalidUpstream`: no failover, no batch retry, the hold retained. The
//! adapter still reports a valid usage object as evidence through its
//! `evidence::preserve` path, which every caller keeps around this check.
//!
//! Only documented names and numeric statuses are read. The message text is
//! never logged, returned or retained (the Messages `prompt is too long`
//! refinement inside an adapter's own `as_http` is the one documented
//! exception, as on the HTTP path).
use crate::inference::error::InferenceError;
use serde_json::Value;

/// Whether `value` is no error at all: absent, null, or an empty string,
/// array or object.
pub(super) fn empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Whether a usage member reports anything other than zero. `null` and an
/// empty usage object report nothing. Inside it, only an integer zero and
/// `null` (OpenAI sends `*_details: null`) are zero: a float cannot tell a
/// tiny exact cost (`1e-400`) from zero, a string or boolean in place of a
/// count is a malformed meter, and an empty object or array where a count
/// or its details belong is malformed too; none of them is evidence that
/// nothing was processed. The labels (`label`) are exempt, and only as
/// direct members of the usage object.
fn usage_nonzero(usage: &Value) -> bool {
    match usage {
        Value::Null => false,
        Value::Object(members) => members
            .iter()
            .any(|(name, value)| !label(name, value) && nonzero(value)),
        other => nonzero(other),
    }
}
fn nonzero(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Number(n) => n.as_u64() != Some(0) && n.as_i64() != Some(0),
        Value::Bool(_) | Value::String(_) => true,
        Value::Array(a) => a.is_empty() || a.iter().any(nonzero),
        Value::Object(o) => o.is_empty() || o.values().any(nonzero),
    }
}
/// A direct member of a usage object that names or flags it rather than
/// counting anything: OpenAI's transcription `"type":"tokens"` (or
/// `"duration"`) and OpenRouter's `"is_byok"` boolean.
fn label(name: &str, value: &Value) -> bool {
    matches!(
        (name, value),
        ("type", Value::String(_)) | ("is_byok", Value::Bool(_))
    )
}

/// `Ok` when `body` has no `error`; otherwise the class of its error. A body
/// with a non-empty member named in `output`, or a nonzero value in a member
/// named in `usage`, is `InvalidUpstream`; a clean one is classified as the
/// same error inside a stream is, with `as_http`, the adapter's
/// classification of an HTTP error status.
pub(super) fn check(
    body: &Value,
    output: &[&str],
    usage: &[&str],
    as_http: impl FnOnce(u16) -> InferenceError,
) -> Result<(), InferenceError> {
    let error = &body["error"];
    if empty(error) {
        return Ok(());
    }
    // Usage reported inside the error object itself is evidence too.
    if output.iter().any(|member| !empty(&body[*member]))
        || usage.iter().any(|member| usage_nonzero(&body[*member]))
        || usage_nonzero(&error["usage"])
    {
        return Err(InferenceError::InvalidUpstream);
    }
    Err(super::stream_error::classify(error, as_http))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn class(body: Value) -> Result<(), InferenceError> {
        check(&body, &["choices"], &["usage"], |code| {
            super::super::framing::status_error(code)
        })
    }

    #[test]
    fn an_absent_or_empty_error_is_no_error() {
        for body in [
            json!({}),
            json!({"error":null}),
            json!({"error":""}),
            json!({"error":{}}),
            json!({"error":[]}),
        ] {
            assert_eq!(class(body.clone()), Ok(()), "{body}");
        }
    }

    #[test]
    fn a_clean_envelope_is_classified_by_its_name() {
        for (error, expected) in [
            (json!({"code":"rate_limit_exceeded"}), InferenceError::Busy),
            (json!({"type":"rate_limit_error"}), InferenceError::Busy),
            (json!({"code":"insufficient_quota"}), InferenceError::Busy),
            (json!({"code":429}), InferenceError::Busy),
            (
                json!({"code":"server_error"}),
                InferenceError::UpstreamUnavailable,
            ),
            (
                json!({"type":"invalid_request_error"}),
                InferenceError::UpstreamRejected,
            ),
            (
                json!({"code":"surprise"}),
                InferenceError::UpstreamUnavailable,
            ),
            (json!("text"), InferenceError::UpstreamUnavailable),
        ] {
            assert_eq!(class(json!({ "error": error })), Err(expected), "{error}");
            // Empty output and all-zero usage leave it clean.
            assert_eq!(
                class(
                    json!({"error":error,"choices":[],"usage":{"prompt_tokens":0,"cost":0,"is_byok":true,"type":"tokens"}})
                ),
                Err(expected),
                "{error}"
            );
        }
    }

    #[test]
    fn output_or_nonzero_usage_next_to_an_error_is_an_invalid_response() {
        let error = json!({"code":"rate_limit_exceeded"});
        for extra in [
            json!({"choices":[{"index":0}]}),
            json!({"choices":"x"}),
            json!({"usage":{"prompt_tokens":1}}),
            json!({"usage":{"details":{"cached_tokens":2}}}),
            json!({"usage":{"cost":"0.0001"}}),
            json!({"usage":{"cost":0.5}}),
            json!({"usage":7}),
            json!({"usage":{"cost":0.0}}),
            json!({"usage":{"cost":"1e-400"}}),
            json!({"usage":{"cost":"-0.5"}}),
            json!({"usage":{"cost":"0"}}),
            json!({"usage":{"prompt_tokens":"NaN"}}),
            json!({"usage":{"prompt_tokens":false}}),
            json!({"usage":{"type":1}}),
        ] {
            let mut body = extra.clone();
            body["error"] = error.clone();
            assert_eq!(class(body), Err(InferenceError::InvalidUpstream), "{extra}");
        }
    }
}
