//! The OpenAI video contract (`POST /videos`, `GET`/`DELETE /videos/{id}`,
//! `GET /videos/{id}/content`), on OpenAI's origin and on approved
//! `openai_compatible` connections ([`VideoWire`]).
use super::*;

/// The Sora family only: the OpenAI origin serves video only for jobs
/// created before its Videos API shut down (2026-09-24). The generic contract
/// is served by approved `openai_compatible` connections (`providers::local`).
pub(in crate::providers::openai) fn supports_video(
    target: &Deployment,
    request: &VideoRequest,
) -> bool {
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
pub(in crate::providers::openai) fn openai_wire<'a>(
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

pub(in crate::providers::openai) fn parse_video(value: &Value) -> Result<UpstreamVideo> {
    object_kind(value, "video")?;
    let state = match value["status"].as_str() {
        Some("queued") => JobState::Queued,
        Some("in_progress") => JobState::InProgress,
        Some("completed") => JobState::Completed,
        Some("failed") => JobState::Failed,
        _ => return Err(InferenceError::InvalidUpstream),
    };
    // `seconds` is a decimal string ("8"); accept a whole JSON number too.
    let whole = |v: &Value| {
        match v {
            Value::String(s)
                if !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit()) =>
            {
                s.parse().ok()
            }
            Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
            _ => None,
        }
        .filter(|n| (1..=VIDEO_MAX_SECONDS).contains(n))
    };
    // The generated duration: the usage the server reports when it does
    // (then only that: an unusable usage value is an unknown duration, never
    // replaced by the requested length), else the video's length.
    let usage = &value["usage"]["output_video_seconds"];
    let mut seconds = if usage.is_null() {
        whole(&value["seconds"])
    } else {
        whole(usage)
    };
    // A size the gateway cannot read makes the report unusable for billing:
    // its duration counts as unknown (the hold is kept), never billed at
    // another size.
    let size = match &value["size"] {
        Value::Null => None,
        Value::String(s) => {
            let size = MeterVariant::new(s);
            if size.is_none() {
                seconds = None;
            }
            size
        }
        _ => {
            seconds = None;
            None
        }
    };
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
        size,
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
    // Redirects are not followed; one after a create may follow its
    // success, so its outcome is unknown (the job keeps its hold), never a
    // create that left nothing upstream.
    if response.status().is_redirection() {
        return Err(InferenceError::InvalidUpstream);
    }
    parse_video(&read_object_from(wire.kind, response).await?)
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
    let parsed = parse_video(&read_object_from(wire.kind, response).await?)?;
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
    // A success without a deletion object (`204`, or a body that is not
    // JSON) is the server's confirmation too; a JSON body must be one.
    check_status(response.status())?;
    if response.status() == reqwest::StatusCode::NO_CONTENT
        || media_type(&response).as_deref() != Some("application/json")
    {
        return Ok(VideoDeletion::Deleted);
    }
    let value = read_object_from(wire.kind, response).await?;
    object_kind(&value, "video.deleted")?;
    if value["deleted"] != json!(true) || &id(&value["id"])? != video {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(VideoDeletion::Deleted)
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
    // The whole content or nothing: an empty (`204`) or partial (`206`)
    // answer, or a `200` that says it holds a range, is not taken as a
    // complete one.
    if response.status() != reqwest::StatusCode::OK
        || response.headers().contains_key(header::CONTENT_RANGE)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let media = match asset {
        // The video's own type, as the server labels it, when it is one the
        // gateway serves ([`VIDEO_OUTPUT_TYPES`]); any other label (none, a
        // generic binary type, a non-video type) is reported as
        // `application/octet-stream`.
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
