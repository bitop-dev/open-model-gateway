//! Every JSON body of an inference route is measured before any parser runs
//! (#872): a body whose structure estimate exceeds
//! `inference::json_weight::BUDGET` is refused with 413, its bytes never
//! parsed or echoed.
//!
//! The body is read whole here, under the route's own byte limit (the
//! layers outside this one bound it), as the handler's JSON extractor would
//! read it; the handler then parses these same bytes, uncopied. A body that
//! is not JSON by its `Content-Type`, or of a route that ignores its body,
//! passes through unread: the handler refuses it, reads it as the multipart
//! form it is, or never reads it. The image input
//! routes take the same measure inside `image_input_body`, in the same pass
//! as their image scan.
use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::{StatusCode, request::Parts},
    middleware::Next,
    response::Response,
};

use crate::{inference::json_weight, metrics::METRICS};

/// The route family a path's refusals count under (the counter's only
/// label), for every route that parses a JSON body; `None` for a route that
/// ignores its body (the realtime companions), which is never read.
pub(super) fn family(path: &str) -> Option<&'static str> {
    Some(match path {
        "/v1/chat/completions" => "chat_completions",
        "/v1/responses" => "responses",
        "/v1/messages" | "/v1/messages/count_tokens" => "messages",
        "/v1/embeddings" => "embeddings",
        "/v1/rerank" => "rerank",
        "/v1/systemone" => "systemone",
        "/v1/images/generations" => "images",
        "/v1/audio/speech" => "audio",
        "/v1/videos" => "videos",
        p if p.starts_with("/v1/batches") => "batches",
        _ => return None,
    })
}

/// The refusal of a body of `path` whose structure estimate is `structure`,
/// if it exceeds the budget: 413 in the route's own shape.
pub(super) fn refusal(path: &str, structure: usize) -> Option<Response> {
    if structure <= json_weight::BUDGET {
        return None;
    }
    METRICS.json_structure_refused(family(path).unwrap_or("other"));
    Some(if super::anthropic_path(path) {
        crate::protocols::messages::http_error(StatusCode::PAYLOAD_TOO_LARGE)
    } else {
        crate::protocols::payload_too_large()
    })
}

/// The middleware of the inference routes outside the image input routes.
pub(super) async fn bound(request: Request, next: Next) -> Response {
    if !super::json_content_type(request.headers()) || family(request.uri().path()).is_none() {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    match measured(parts, body).await {
        Ok(request) => next.run(request).await,
        Err(response) => response,
    }
}

/// `body` read whole and measured: the request to run with it, or the
/// response that refuses it.
pub(super) async fn measured(parts: Parts, body: Body) -> Result<Request, Response> {
    let bytes = match http_body_util::BodyExt::collect(body).await {
        Ok(collected) => collected.to_bytes(),
        // Past the route's byte limit: the 413 the handler's extractor
        // would answer.
        Err(error) if over_limit(&error) => {
            return Err(if super::anthropic_path(parts.uri.path()) {
                crate::protocols::messages::http_error(StatusCode::PAYLOAD_TOO_LARGE)
            } else {
                crate::protocols::payload_too_large()
            });
        }
        // Any other read failure reaches the handler as the same failure,
        // which its extractor refuses as before.
        Err(error) => {
            let failed =
                Body::from_stream(futures_util::stream::once(
                    async move { Err::<Bytes, _>(error) },
                ));
            return Ok(Request::from_parts(parts, failed));
        }
    };
    if let Some(response) = refusal(parts.uri.path(), json_weight::estimate(&bytes)) {
        return Err(response);
    }
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

/// Whether a body read failed because it passed its byte limit.
fn over_limit(error: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = source {
        if error.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = error.source();
    }
    false
}
