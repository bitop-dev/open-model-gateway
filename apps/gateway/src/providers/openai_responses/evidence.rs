//! Accounting evidence from a rejected Responses terminal. Read the counters from
//! the wire so their JSON types and the uniqueness of each field are checked together.
use super::super::{framing::Result, metering};
use crate::inference::{
    error::InferenceError,
    types::{ReportedModel, Usage},
};
use serde::{Deserialize, de::IgnoredAny};
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct Frame<'a> {
    #[serde(rename = "type")]
    _kind: String,
    #[serde(borrow)]
    response: &'a RawValue,
}

#[derive(Deserialize)]
struct Snapshot<'a> {
    #[serde(rename = "status")]
    _status: Option<IgnoredAny>,
    #[serde(borrow)]
    model: Option<&'a RawValue>,
    #[serde(borrow)]
    usage: &'a RawValue,
}

#[derive(Deserialize)]
struct Counters<'a> {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    #[serde(borrow)]
    input_tokens_details: Option<&'a RawValue>,
    #[serde(borrow)]
    output_tokens_details: Option<&'a RawValue>,
    #[serde(borrow)]
    completion_tokens_details: Option<&'a RawValue>,
}

#[derive(Default, Deserialize)]
struct CacheCounters {
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct Reasoning<'a> {
    #[serde(borrow)]
    reasoning_tokens: Option<&'a RawValue>,
}

// Serde also accepts positional arrays for structs. These provider fields are objects.
fn object<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    if !bytes.trim_ascii_start().starts_with(b"{") {
        return Err(InferenceError::InvalidUpstream);
    }
    serde_json::from_slice(bytes).map_err(|_| InferenceError::InvalidUpstream)
}

fn reasoning(details: Option<&RawValue>, output: Option<u64>) -> Result<Option<u64>> {
    let Some(raw) = details.filter(|raw| raw.get().starts_with('{')) else {
        return Ok(None);
    };
    let details: Reasoning<'_> = object(raw.get().as_bytes())?;
    Ok(details
        .reasoning_tokens
        .and_then(|raw| serde_json::from_str::<u64>(raw.get()).ok())
        .filter(|&n| metering::plausible_reasoning(n, output)))
}

pub(super) fn mismatch_usage(bytes: &[u8]) -> Result<Usage> {
    let frame: Frame<'_> = object(bytes)?;
    let response: Snapshot<'_> = object(frame.response.get().as_bytes())?;
    let counts: Counters<'_> = object(response.usage.get().as_bytes())?;
    let cache: CacheCounters = counts
        .input_tokens_details
        .map(|raw| object(raw.get().as_bytes()))
        .transpose()?
        .unwrap_or_default();
    let mut observed = metering::inclusive_counts(
        counts.input_tokens,
        counts.output_tokens,
        cache.cached_tokens,
        cache.cache_write_tokens,
    )?;
    if let Some(total) = counts.total_tokens {
        // The supplied total can disqualify counts, but cannot supply missing evidence.
        let contradictory = match (observed.input_tokens, observed.output_tokens) {
            (Some(input), Some(output)) => input.checked_add(output) != Some(total),
            (_, output) => {
                let input_floor = observed
                    .billing
                    .ok_or(InferenceError::InvalidUpstream)?
                    .input_lower_bound()
                    .map_err(|_| InferenceError::InvalidUpstream)?;
                input_floor
                    .checked_add(output.unwrap_or(0))
                    .is_none_or(|floor| floor > total)
            }
        };
        if total > i64::MAX as u64 || contradictory {
            return Err(InferenceError::InvalidUpstream);
        }
    }
    let native = reasoning(counts.output_tokens_details, counts.output_tokens)?;
    let compatible = reasoning(counts.completion_tokens_details, counts.output_tokens)?;
    observed.reasoning_tokens = native.or(compatible);
    observed.reported_model = response
        .model
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
        .and_then(|name| ReportedModel::parse(&name));
    Ok(observed)
}
