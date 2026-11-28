//! OpenRouter video generation (#52): `POST /videos`, `GET /videos/{id}` and
//! `GET /videos/{id}/content?index=0`, at the fixed origin.
//!
//! - The length is always sent (`duration`), the size only when the request
//!   names one (`size`, `WIDTHxHEIGHT`); `provider.data_collection` is sent
//!   as for every OpenRouter workload. The route's video options (0097) say
//!   which lengths and sizes it takes, as for any video route.
//! - OpenRouter reports no duration, only its cost: `usage.cost` on the
//!   completed job, read from the JSON number's text as exact micro-USD
//!   (more than six decimals rounded up), never through a float. That figure
//!   is the video's charge (`governance::settlement`), an estimate like every
//!   other price. An absent, null, negative or unreadable cost is unknown:
//!   the hold is kept, never a zero.
//! - `pending` is queued; `failed`, `cancelled` and `expired` end the job as
//!   failed with the code `upstream_failed`, `upstream_cancelled` or
//!   `upstream_expired` (its hold kept, as for any provider failure). A completed job reported
//!   `in_progress` again is an observed regression, which the job state
//!   machine ignores.
//! - Content is the video only (`index=0`): OpenRouter offers no thumbnail or
//!   spritesheet. It offers no delete either: a delete leaves its copy until
//!   its own expiry (`upstream_deleted: false`).
//! - The provider's error text is never recorded: it may quote the request.
use futures_util::StreamExt;
use reqwest::header;
use serde_json::{Value, json};

use super::{OpenRouterAdapter, object, provider_cost, read, transport};
use crate::{
    inference::{error::InferenceError, types::Deployment},
    jobs::types::{
        ContentStream, ErrorCode, JobState, ReportedCost, UpstreamId, UpstreamVideo,
        VIDEO_OUTPUT_TYPES, VideoAsset, VideoRequest,
    },
};

type Result<T> = std::result::Result<T, InferenceError>;

pub(super) fn supports(target: &Deployment, request: &VideoRequest) -> bool {
    !target.upstream_model.trim().is_empty() && request.validate().is_ok()
}

pub(super) async fn create(
    adapter: &OpenRouterAdapter,
    target: &Deployment,
    request: &VideoRequest,
) -> Result<UpstreamVideo> {
    // A route always resolves the length before sending (`jobs::video_route`).
    let seconds = request.seconds.ok_or(InferenceError::InvalidRequest)?;
    let mut body = json!({
        "model": target.upstream_model,
        "prompt": request.prompt,
        "duration": seconds,
        "provider": adapter.provider_preferences(),
    });
    if let Some(size) = &request.size {
        body["size"] = json!(size);
    }
    let response = adapter.post(target, "/videos", &body).await?;
    parse(&read(response).await?)
}

pub(super) async fn retrieve(
    adapter: &OpenRouterAdapter,
    target: &Deployment,
    video: &UpstreamId,
) -> Result<UpstreamVideo> {
    let response = adapter
        .get(target, &format!("/videos/{}", video.as_str()))
        .await?;
    let parsed = parse(&read(response).await?)?;
    if parsed.id != *video {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(parsed)
}

pub(super) async fn content(
    adapter: &OpenRouterAdapter,
    target: &Deployment,
    video: &UpstreamId,
    asset: VideoAsset,
) -> Result<ContentStream> {
    if asset != VideoAsset::Video {
        return Err(InferenceError::Unsupported);
    }
    let response = adapter
        .get(
            target,
            &format!("/videos/{}/content?index=0", video.as_str()),
        )
        .await?;
    // The whole content or nothing (as on the OpenAI contract).
    if response.status() != reqwest::StatusCode::OK
        || response.headers().contains_key(header::CONTENT_RANGE)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let media = crate::providers::audio::media_type(&response)
        .and_then(|m| VIDEO_OUTPUT_TYPES.iter().copied().find(|t| *t == m))
        .unwrap_or("application/octet-stream");
    Ok(ContentStream {
        content_type: media,
        content_length: response.content_length(),
        body: Box::pin(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(transport)),
        ),
    })
}

/// A video job object (submit `202`, or a poll `200`).
fn parse(bytes: &[u8]) -> Result<UpstreamVideo> {
    let value = object(bytes)?;
    let id = value["id"]
        .as_str()
        .and_then(UpstreamId::parse)
        .ok_or(InferenceError::InvalidUpstream)?;
    // How a job ended is kept as a fixed code, never the provider's text.
    let (state, error) = match value["status"].as_str() {
        Some("pending") => (JobState::Queued, None),
        Some("in_progress") => (JobState::InProgress, None),
        Some("completed") => (JobState::Completed, None),
        Some(end @ ("failed" | "cancelled" | "expired")) => (
            JobState::Failed,
            Some(ErrorCode::parse(&format!("upstream_{end}"))),
        ),
        _ => return Err(InferenceError::InvalidUpstream),
    };
    // Only a completed job reports its cost, as the number `usage.cost` of
    // a `usage` object; any doubt about it is unknown.
    let cost =
        (state == JobState::Completed).then(|| match (&value["usage"], provider_cost(bytes)) {
            (Value::Object(usage), Ok(Some(cost)))
                if usage.get("cost").is_some_and(Value::is_number) =>
            {
                ReportedCost::Known(cost)
            }
            _ => ReportedCost::Unknown,
        });
    Ok(UpstreamVideo {
        id,
        state,
        // A percentage, possibly fractional, for display only: the whole
        // part of the number as the JSON parser read it (a negative, or one
        // above 100, is unknown).
        progress: value["progress"]
            .as_number()
            .and_then(|n| n.to_string().split('.').next()?.parse::<u8>().ok())
            .filter(|p| *p <= 100),
        seconds: None,
        size: None,
        completed_at: None,
        expires_at: value["expires_at"].as_i64().filter(|t| *t > 0),
        error,
        cost,
    })
}

#[cfg(test)]
#[path = "video_tests.rs"]
mod tests;
