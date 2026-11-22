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

fn reasoning(details: Option<&RawValue>, output_bound: Option<u64>) -> Result<Option<u64>> {
    let Some(raw) = details.filter(|raw| raw.get().starts_with('{')) else {
        return Ok(None);
    };
    let details: Reasoning<'_> = object(raw.get().as_bytes())?;
    Ok(details
        .reasoning_tokens
        .and_then(|raw| serde_json::from_str::<u64>(raw.get()).ok())
        .filter(|&n| metering::plausible_reasoning(n, output_bound)))
}

pub(super) fn rejected_terminal_usage(bytes: &[u8]) -> Result<Usage> {
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
    let mut output_bound = counts.output_tokens;
    if let Some(total) = counts.total_tokens {
        let input_floor = observed
            .billing
            .ok_or(InferenceError::InvalidUpstream)?
            .input_lower_bound()
            .map_err(|_| InferenceError::InvalidUpstream)?;
        let remaining = total
            .checked_sub(input_floor)
            .ok_or(InferenceError::InvalidUpstream)?;
        if total > i64::MAX as u64
            || counts.output_tokens.is_some_and(|output| {
                output > remaining || (counts.input_tokens.is_some() && output != remaining)
            })
        {
            return Err(InferenceError::InvalidUpstream);
        }
        // The remainder bounds reasoning telemetry when the output count is absent.
        // Billing keeps the original observation.
        output_bound = Some(counts.output_tokens.unwrap_or(remaining));
    }
    let native = reasoning(counts.output_tokens_details, output_bound)?;
    let compatible = reasoning(counts.completion_tokens_details, output_bound)?;
    observed.reasoning_tokens = native.or(compatible);
    observed.reported_model = response
        .model
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
        .and_then(|name| ReportedModel::parse(&name));
    Ok(observed)
}
