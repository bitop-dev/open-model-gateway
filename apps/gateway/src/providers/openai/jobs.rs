//! OpenAI async jobs at the fixed origin (shapes from the official OpenAPI
//! spec via openai-python, 2026-10-08):
//! - Videos (the OpenAI video contract, [`VideoWire`]): `POST /videos`
//!   (multipart `model, prompt, seconds` and `size` only when the request
//!   named one; the gateway builds the form itself), `GET /videos/{id}`,
//!   `DELETE /videos/{id}`, `GET /videos/{id}/content`. Served on
//!   `openai_compatible` connections; OpenAI shut down its own API on
//!   2026-09-24, so its adapter no longer offers `videos` and these calls
//!   remain there only for jobs created earlier.
//! - Native batches (`jobs::native`): `POST /files` (multipart
//!   `purpose=batch` + a streamed JSONL `file` the gateway encodes), `POST
//!   /batches`, `GET /batches/{id}`, `POST /batches/{id}/cancel`, `GET
//!   /files/{id}/content` (output and error files) and `DELETE /files/{id}`.
//!
//! Upstream ids are validated (`[A-Za-z0-9._:-]{1,128}`) before they are put
//! in a path. Error bodies are never read; job error messages are dropped
//! (codes only). Content bodies are streamed, never buffered or logged.
use futures_util::StreamExt;
use reqwest::header::{self, HeaderValue};
use serde_json::{Value, json};

use super::{BASE, OpenAiAdapter, check_status, transport_error};
use crate::{
    billing::MeterVariant,
    inference::{
        error::InferenceError,
        types::{Deployment, Usage},
    },
    jobs::types::*,
    providers::{audio::Multipart, metering},
};

type Result<T> = std::result::Result<T, InferenceError>;

/// Job/file objects are small JSON documents.
const OBJECT_LIMIT: usize = 1024 * 1024;

/// Validate the connection before resolving credentials.
fn authorization(adapter: &OpenAiAdapter, target: &Deployment) -> Result<HeaderValue> {
    if target.provider != "openai"
        || target.credential_ref == "none"
        || target
            .endpoint
            .as_deref()
            .is_some_and(|v| v != BASE && v != "https://api.openai.com/v1/")
        || target.region.as_deref().is_some_and(|v| !v.is_empty())
        || target.upstream_model.trim().is_empty()
    {
        return Err(InferenceError::Configuration);
    }
    let secret = adapter.resolver.resolve(&target.credential_ref)?;
    let mut auth = HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
        .map_err(|_| InferenceError::Configuration)?;
    auth.set_sensitive(true);
    Ok(auth)
}

fn media_type(response: &reqwest::Response) -> Option<String> {
    crate::providers::audio::media_type(response)
}

/// Bounded JSON object (status already checked).
async fn read_object(response: reqwest::Response) -> Result<Value> {
    check_status(response.status())?;
    if media_type(&response).as_deref() != Some("application/json")
        || response
            .content_length()
            .is_some_and(|n| n > OBJECT_LIMIT as u64)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if chunk.len() > OBJECT_LIMIT - body.len() {
            return Err(InferenceError::InvalidUpstream);
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value = crate::providers::framing::json(&body)?;
    if !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(value)
}

fn object_kind(value: &Value, kind: &str) -> Result<()> {
    match &value["object"] {
        Value::Null => Ok(()),
        Value::String(s) if s == kind => Ok(()),
        _ => Err(InferenceError::InvalidUpstream),
    }
}
fn id(value: &Value) -> Result<UpstreamId> {
    value
        .as_str()
        .and_then(UpstreamId::parse)
        .ok_or(InferenceError::InvalidUpstream)
}
fn optional_id(value: &Value) -> Result<Option<UpstreamId>> {
    match value {
        Value::Null => Ok(None),
        v => id(v).map(Some),
    }
}
/// Unix seconds; anything else is unknown (never guessed).
fn timestamp(value: &Value) -> Option<i64> {
    value.as_i64().filter(|n| *n >= 0)
}

// ---------------------------------------------------------------- Video ----

/// The Sora family only: the OpenAI origin serves video only for jobs
/// created before its Videos API shut down (2026-09-24). The generic contract
/// is served by approved `openai_compatible` connections (`providers::local`).
pub(super) fn supports_video(target: &Deployment, request: &VideoRequest) -> bool {
    target.upstream_model.starts_with("sora-") && request.validate().is_ok()
}

/// An OpenAI-contract video server: the OpenAI origin, or an approved
/// `openai_compatible` connection. Requests carry its credential (when it
/// has one) and are counted against its client.
pub(crate) struct VideoWire<'a> {
    pub client: &'a crate::egress::HttpClient,
    /// The API base, ending in `/v1` (no trailing slash).
    pub base: &'a str,
    pub auth: Option<HeaderValue>,
    pub kind: crate::egress::UpstreamClient,
}
impl VideoWire<'_> {
    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Some(auth) => request.header(header::AUTHORIZATION, auth.clone()),
            None => request,
        }
    }
}
/// A transport failure of `kind`'s client, counted before any response.
fn lost(kind: crate::egress::UpstreamClient, error: &reqwest::Error) -> InferenceError {
    kind.lost_before_response(error);
    if error.is_timeout() {
        InferenceError::Timeout
    } else {
        InferenceError::UpstreamUnavailable
    }
}

/// The OpenAI origin's wire for `target`.
pub(super) fn openai_wire<'a>(
    adapter: &'a OpenAiAdapter,
    target: &Deployment,
) -> Result<VideoWire<'a>> {
    Ok(VideoWire {
        client: &adapter.client,
        base: &adapter.base,
        auth: Some(authorization(adapter, target)?),
        kind: crate::egress::UpstreamClient::Openai,
    })
}

pub(super) fn parse_video(value: &Value) -> Result<UpstreamVideo> {
    object_kind(value, "video")?;
    let state = match value["status"].as_str() {
        Some("queued") => JobState::Queued,
        Some("in_progress") => JobState::InProgress,
        Some("completed") => JobState::Completed,
        Some("failed") => JobState::Failed,
        _ => return Err(InferenceError::InvalidUpstream),
    };
    // `seconds` is a decimal string ("8"); accept a whole JSON number too.
    let seconds = match &value["seconds"] {
        Value::String(s)
            if !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit()) =>
        {
            s.parse().ok()
        }
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        _ => None,
    }
    .filter(|n| (1..=VIDEO_MAX_SECONDS).contains(n));
    let error = match &value["error"] {
        Value::Null => None,
        Value::Object(e) => Some(ErrorCode::parse(
            e.get("code").and_then(Value::as_str).unwrap_or(""),
        )),
        _ => return Err(InferenceError::InvalidUpstream),
    };
    Ok(UpstreamVideo {
        id: id(&value["id"])?,
        state,
        progress: value["progress"]
            .as_u64()
            .filter(|n| *n <= 100)
            .map(|n| n as u8),
        seconds,
        size: value["size"].as_str().and_then(MeterVariant::new),
        completed_at: timestamp(&value["completed_at"]),
        expires_at: timestamp(&value["expires_at"]),
        error,
    })
}

/// `POST /videos`: the route's resolved length is always sent (the billed
/// duration is then known); `size` only when the request named one.
pub(crate) async fn create_video(
    wire: &VideoWire<'_>,
    target: &Deployment,
    request: VideoRequest,
) -> Result<UpstreamVideo> {
    let seconds = request.seconds.ok_or(InferenceError::Configuration)?;
    if target.upstream_model.trim().is_empty() {
        return Err(InferenceError::Configuration);
    }
    let mut form = Multipart::new(request.prompt.len());
    form.text("model", &target.upstream_model);
    form.text("prompt", &request.prompt);
    form.text("seconds", &seconds.to_string());
    if let Some(size) = &request.size {
        form.text("size", size);
    }
    let (content_type, body) = form.finish();
    let response = wire
        .authorized(wire.client.post(format!("{}/videos", wire.base)))
        .header(header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .map_err(|e| lost(wire.kind, &e))?;
    parse_video(&read_object(response).await?)
}

pub(crate) async fn retrieve_video(
    wire: &VideoWire<'_>,
    video: &UpstreamId,
) -> Result<UpstreamVideo> {
    let response = wire
        .authorized(
            wire.client
                .get(format!("{}/videos/{}", wire.base, video.as_str())),
        )
        .send()
        .await
        .map_err(|e| lost(wire.kind, &e))?;
    let parsed = parse_video(&read_object(response).await?)?;
    if &parsed.id != video {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(parsed)
}

/// `DELETE /videos/{id}`. A server that answers `405 Method Not Allowed`
/// offers no delete: its copy is left to its own expiry, and that is
/// reported, never a pretended deletion.
pub(crate) async fn delete_video(
    wire: &VideoWire<'_>,
    video: &UpstreamId,
) -> Result<VideoDeletion> {
    let response = wire
        .authorized(
            wire.client
                .delete(format!("{}/videos/{}", wire.base, video.as_str())),
        )
        .send()
        .await
        .map_err(|e| lost(wire.kind, &e))?;
    if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        return Ok(VideoDeletion::NotOffered);
    }
    // The server no longer has it (expired or deleted before): gone.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(VideoDeletion::Deleted);
    }
    let value = read_object(response).await?;
    object_kind(&value, "video.deleted")?;
    if value["deleted"] != json!(true) || &id(&value["id"])? != video {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(VideoDeletion::Deleted)
}

/// Allowlisted pass-through media types (normalized).
fn content_type(response: &reqwest::Response, allowed: &[&'static str]) -> Result<&'static str> {
    let actual = media_type(response).ok_or(InferenceError::InvalidUpstream)?;
    allowed
        .iter()
        .copied()
        .find(|t| *t == actual)
        .ok_or(InferenceError::InvalidUpstream)
}
fn stream(
    kind: crate::egress::UpstreamClient,
    response: reqwest::Response,
    media: &'static str,
) -> ContentStream {
    ContentStream {
        content_type: media,
        content_length: response.content_length(),
        body: Box::pin(
            response
                .bytes_stream()
                .map(move |chunk| chunk.map_err(|e| lost(kind, &e))),
        ),
    }
}

pub(crate) async fn video_content(
    wire: &VideoWire<'_>,
    video: &UpstreamId,
    asset: VideoAsset,
) -> Result<ContentStream> {
    let response = wire
        .authorized(wire.client.get(format!(
            "{}/videos/{}/content?variant={}",
            wire.base,
            video.as_str(),
            asset.as_str()
        )))
        .header(header::ACCEPT, "application/binary")
        .send()
        .await
        .map_err(|e| lost(wire.kind, &e))?;
    check_status(response.status())?;
    let media = match asset {
        // The video's own type, as the server labels it, when it is one the
        // gateway stores ([`VIDEO_OUTPUT_TYPES`]); any other label (none, a
        // generic binary type, a non-video type) is reported as
        // `application/octet-stream`, which the gateway never stores.
        VideoAsset::Video => media_type(&response)
            .and_then(|m| VIDEO_OUTPUT_TYPES.iter().copied().find(|t| *t == m))
            .unwrap_or("application/octet-stream"),
        VideoAsset::Thumbnail | VideoAsset::Spritesheet => {
            let media = content_type(
                &response,
                &[
                    "image/webp",
                    "image/jpeg",
                    "image/png",
                    "application/octet-stream",
                    "application/binary",
                ],
            )?;
            if media == "application/binary" {
                "application/octet-stream"
            } else {
                media
            }
        }
    };
    Ok(stream(wire.kind, response, media))
}

// ---------------------------------------------------------------- Batch ----
//
// Native batches (`jobs::native`): the gateway encodes each validated line
// itself (`custom_id` = the gateway's `l<n>`, model = the upstream model),
// uploads the JSONL as one streamed multipart file, creates the batch, and
// later downloads the output and error files, decodes each line into a
// canonical result and deletes the provider's files.

/// The upstream URL of an endpoint's lines: chat-like lines are sent as
/// Chat Completions, embeddings as Embeddings.
fn upstream_url(endpoint: BatchEndpoint) -> &'static str {
    if endpoint.is_generation() {
        "/v1/chat/completions"
    } else {
        "/v1/embeddings"
    }
}

/// Connection shape accepted for native batches (before any secret lookup).
pub(super) fn native_batch(target: &Deployment) -> bool {
    target.provider == "openai"
        && target.credential_ref != "none"
        && target
            .endpoint
            .as_deref()
            .is_none_or(|v| v == BASE || v == "https://api.openai.com/v1/")
        && target.region.as_deref().is_none_or(str::is_empty)
        && !target.upstream_model.trim().is_empty()
}

pub(super) fn encode_native_line(
    target: &Deployment,
    endpoint: BatchEndpoint,
    custom_id: &str,
    request: &BatchRequest,
) -> Result<Vec<u8>> {
    let body = match (request, endpoint.is_generation()) {
        // Chat and Responses batch lines refuse images when parsed, and a
        // Messages line with images is not encoded for this native batch; a
        // line is serialized without `image_body`'s splice, so an image here
        // could only leave a placeholder in the file. Refused, never written.
        (BatchRequest::Chat(r), true) if r.has_images() => {
            return Err(InferenceError::InvalidField("image"));
        }
        (BatchRequest::Chat(r), true) if !r.stream => super::encode_chat(&target.upstream_model, r),
        (BatchRequest::Embeddings(r), false) => {
            crate::providers::embeddings::validate(r)?;
            crate::providers::embeddings::encode(&target.upstream_model, r)
        }
        _ => return Err(InferenceError::InvalidRequest),
    };
    serde_json::to_vec(&json!({
        "custom_id": custom_id,
        "method": "POST",
        "url": upstream_url(endpoint),
        "body": body,
    }))
    .map_err(|_| InferenceError::InvalidRequest)
}

/// Stream the records as one JSONL `purpose=batch` file. An error item
/// aborts the request body, so the provider never receives a complete form.
async fn upload_records(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    records: ByteStream,
) -> Result<UpstreamId> {
    let auth = authorization(adapter, target)?;
    let boundary = format!("omg-{}", uuid::Uuid::new_v4().simple());
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"batch.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n"
    );
    let tail = format!("\r\n--{boundary}--\r\n");
    let lines = records.map(|record| {
        record.map(|bytes| {
            let mut line = Vec::with_capacity(bytes.len() + 1);
            line.extend_from_slice(&bytes);
            line.push(b'\n');
            axum::body::Bytes::from(line)
        })
    });
    let body = futures_util::stream::once(async move { Ok(axum::body::Bytes::from(head)) })
        .chain(lines)
        .chain(futures_util::stream::once(async move {
            Ok(axum::body::Bytes::from(tail))
        }));
    let response = adapter
        .client
        .post(format!("{}/files", adapter.base))
        .header(header::AUTHORIZATION, auth)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await
        .map_err(transport_error)?;
    let value = read_object(response).await?;
    object_kind(&value, "file")?;
    if value["purpose"].as_str().is_some_and(|p| p != "batch") {
        return Err(InferenceError::InvalidUpstream);
    }
    id(&value["id"])
}

pub(super) async fn submit_native_batch(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    endpoint: BatchEndpoint,
    records: ByteStream,
) -> Result<UpstreamBatch> {
    let input = upload_records(adapter, target, records).await?;
    let auth = authorization(adapter, target)?;
    let created = async {
        let response = adapter
            .client
            .post(format!("{}/batches", adapter.base))
            .header(header::AUTHORIZATION, auth)
            .json(&json!({
                "input_file_id": input.as_str(),
                "endpoint": upstream_url(endpoint),
                "completion_window": BATCH_COMPLETION_WINDOW,
            }))
            .send()
            .await
            .map_err(transport_error)?;
        parse_batch(&read_object(response).await?)
    }
    .await;
    match created {
        Ok(mut batch) => {
            batch.input_file.get_or_insert(input);
            Ok(batch)
        }
        Err(e) => {
            // The batch was not created: remove the uploaded copy (best effort).
            let _ = delete_file(adapter, target, &input).await;
            Err(e)
        }
    }
}

pub(super) fn schema_usage(
    value: &Value,
    input_key: &str,
    output_key: &str,
    details_key: &str,
) -> Option<Usage> {
    if !value.is_object() {
        return None;
    }
    let input = metering::count(&value[input_key]).ok()??;
    let output = metering::count(&value[output_key]).ok()??;
    let details = &value[details_key];
    if !(details.is_null() || details.is_object()) {
        return None;
    }
    let cached = metering::count(&details["cached_tokens"])
        .ok()?
        .unwrap_or(0);
    if let Some(total) = metering::count(&value["total_tokens"]).ok()?
        && input.checked_add(output) != Some(total)
    {
        return None;
    }
    let billing = crate::billing::BillingUsage {
        total_input_tokens: Some(input),
        uncached_input_tokens: Some(input.checked_sub(cached)?),
        cache_read_input_tokens: Some(cached),
        cache_write_input_tokens: Some(0),
        cache_write_default_input_tokens: Some(0),
        cache_write_5m_input_tokens: Some(0),
        cache_write_1h_input_tokens: Some(0),
    };
    billing.validate().ok()?;
    let reasoning = ["output_tokens_details", "completion_tokens_details"]
        .into_iter()
        .find_map(|k| value[k]["reasoning_tokens"].as_u64())
        .filter(|n| *n <= output);
    Some(Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        billing: Some(billing),
        reasoning_tokens: reasoning,
        ..Usage::default()
    })
}
fn batch_usage(value: &Value) -> Option<Usage> {
    schema_usage(
        value,
        "input_tokens",
        "output_tokens",
        "input_tokens_details",
    )
}

pub(super) fn parse_batch(value: &Value) -> Result<UpstreamBatch> {
    object_kind(value, "batch")?;
    if value["endpoint"]
        .as_str()
        .is_some_and(|e| e != "/v1/chat/completions" && e != "/v1/embeddings")
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let status = value["status"]
        .as_str()
        .and_then(BatchStatus::parse)
        .ok_or(InferenceError::InvalidUpstream)?;
    let counts = match &value["request_counts"] {
        Value::Null => None,
        c => {
            let n = |k: &str| c[k].as_u64().and_then(|n| u32::try_from(n).ok());
            match (n("total"), n("completed"), n("failed")) {
                (Some(total), Some(completed), Some(failed))
                    if u64::from(completed) + u64::from(failed) <= u64::from(total) =>
                {
                    Some(RequestCounts {
                        total,
                        completed,
                        failed,
                    })
                }
                _ => return Err(InferenceError::InvalidUpstream),
            }
        }
    };
    let metadata = value["metadata"]
        .as_object()
        .filter(|m| valid_metadata(m))
        .cloned();
    Ok(UpstreamBatch {
        id: id(&value["id"])?,
        status,
        input_file: optional_id(&value["input_file_id"])?,
        output_file: optional_id(&value["output_file_id"])?,
        error_file: optional_id(&value["error_file_id"])?,
        counts,
        usage: batch_usage(&value["usage"]),
        created_at: timestamp(&value["created_at"]),
        in_progress_at: timestamp(&value["in_progress_at"]),
        finalizing_at: timestamp(&value["finalizing_at"]),
        completed_at: timestamp(&value["completed_at"]),
        failed_at: timestamp(&value["failed_at"]),
        expired_at: timestamp(&value["expired_at"]),
        cancelling_at: timestamp(&value["cancelling_at"]),
        cancelled_at: timestamp(&value["cancelled_at"]),
        expires_at: timestamp(&value["expires_at"]),
        metadata,
    })
}

pub(super) async fn batch_action(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    batch: &UpstreamId,
    cancel: bool,
) -> Result<UpstreamBatch> {
    let auth = authorization(adapter, target)?;
    let url = format!(
        "{}/batches/{}{}",
        adapter.base,
        batch.as_str(),
        if cancel { "/cancel" } else { "" }
    );
    let request = if cancel {
        adapter.client.post(url)
    } else {
        adapter.client.get(url)
    };
    let response = request
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(transport_error)?;
    let parsed = parse_batch(&read_object(response).await?)?;
    if &parsed.id != batch {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(parsed)
}

const FILE_TYPES: &[&str] = &[
    "application/octet-stream",
    "application/jsonl",
    "application/x-ndjson",
    "application/json",
    "text/plain",
];

async fn open_file(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    file: &UpstreamId,
) -> Result<reqwest::Response> {
    let auth = authorization(adapter, target)?;
    let response = adapter
        .client
        .get(format!("{}/files/{}/content", adapter.base, file.as_str()))
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(transport_error)?;
    check_status(response.status())?;
    content_type(&response, FILE_TYPES)?;
    Ok(response)
}

/// The output file followed by the error file (a newline between them).
pub(super) async fn native_batch_results(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    batch: &UpstreamBatch,
) -> Result<ByteStream> {
    let mut parts: Vec<ByteStream> = Vec::new();
    for file in [&batch.output_file, &batch.error_file]
        .into_iter()
        .flatten()
    {
        let response = open_file(adapter, target, file).await?;
        if !parts.is_empty() {
            parts.push(Box::pin(futures_util::stream::once(async {
                Ok(axum::body::Bytes::from_static(b"\n"))
            })));
        }
        parts.push(Box::pin(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(transport_error)),
        ));
    }
    Ok(Box::pin(futures_util::stream::iter(parts).flatten()))
}

/// One output/error file line: `{custom_id, response:{status_code, body},
/// error}`. Error messages are never read; only codes.
pub(super) fn decode_native_result(endpoint: BatchEndpoint, line: &[u8]) -> Result<NativeResult> {
    let value: Value = crate::providers::framing::json(line)?;
    let custom_id = value["custom_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .ok_or(InferenceError::InvalidUpstream)?
        .to_owned();
    let response = &value["response"];
    let outcome = if response.is_object() {
        let status = response["status_code"]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| (100..=599).contains(n))
            .ok_or(InferenceError::InvalidUpstream)?;
        let body = &response["body"];
        if status == 200 {
            NativeOutcome::Succeeded(Box::new(if endpoint.is_generation() {
                // Messages lines may carry strict structured output, whose
                // refusal is a Messages stop reason (`decode_complete_protocol`).
                BatchResponse::Chat(super::decode_complete_protocol(
                    body,
                    None,
                    matches!(endpoint, BatchEndpoint::Messages),
                )?)
            } else {
                // A native line's body with an `error` member, even a null
                // one, is not a success, whatever the shared decoder makes
                // of it.
                if body.get("error").is_some() {
                    return Err(InferenceError::InvalidUpstream);
                }
                let n = body["data"].as_array().map_or(0, Vec::len);
                let request = crate::inference::types::EmbeddingRequest {
                    model: String::new(),
                    input: vec![String::from("x"); n],
                    dimensions: None,
                };
                BatchResponse::Embeddings(crate::providers::embeddings::decode(
                    body,
                    &request,
                    crate::providers::framing::status_error,
                )?)
            }))
        } else {
            let code = body["error"]["code"]
                .as_str()
                .or_else(|| body["error"]["type"].as_str())
                .unwrap_or("");
            NativeOutcome::Failed {
                status,
                code: ErrorCode::parse(code),
            }
        }
    } else {
        match value["error"]["code"].as_str() {
            Some("batch_expired") => NativeOutcome::Expired,
            Some("batch_cancelled") => NativeOutcome::Cancelled,
            Some(code) => NativeOutcome::Failed {
                status: 500,
                code: ErrorCode::parse(code),
            },
            None => return Err(InferenceError::InvalidUpstream),
        }
    };
    Ok(NativeResult { custom_id, outcome })
}

async fn delete_file(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    file: &UpstreamId,
) -> Result<()> {
    let auth = authorization(adapter, target)?;
    let response = adapter
        .client
        .delete(format!("{}/files/{}", adapter.base, file.as_str()))
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(transport_error)?;
    // Already gone is done.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    check_status(response.status())
}

/// Delete the provider's input, output and error files (batches themselves
/// cannot be deleted on OpenAI). Every file is attempted.
pub(super) async fn delete_native_batch(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    batch: &UpstreamBatch,
) -> Result<()> {
    let mut result = Ok(());
    for file in [&batch.input_file, &batch.output_file, &batch.error_file]
        .into_iter()
        .flatten()
    {
        if let Err(e) = delete_file(adapter, target, file).await {
            result = Err(e);
        }
    }
    result
}

#[cfg(test)]
mod tests;
