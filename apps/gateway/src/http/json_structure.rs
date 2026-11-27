//! Every JSON body of an inference route is measured before any parser runs
//! (#872): a body whose estimate exceeds `inference::json_weight::BUDGET`
//! is refused with 413, its bytes never parsed or echoed.
//!
//! The measure wraps each POST handler that reads a JSON body
//! (`http::json_post`), inside authentication: never a method's 405
//! fallback, never a handler that reads no body. The body is read whole
//! there, never past the route's own byte limit (the layer is given it and
//! reads through it), as the handler's JSON extractor would read it; the
//! handler then parses these same bytes, uncopied. A body that is not JSON by
//! its `Content-Type` passes through unread, and the handler refuses it,
//! except on the route whose handler reads a loosely typed body as JSON
//! (`any_type`), where only a multipart form passes. The image input routes
//! take the same measure inside `image_input_body`, in the same pass as
//! their image scan. Each handler then adds the JSON documents its request
//! holds in strings ([`inner_refusal`]).
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
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
        "/v1/batches" => "batches",
        _ => return None,
    })
}

/// The estimate of the body a request was parsed from (both parts of
/// `json_weight::Weight`), for the handler's check of the JSON documents its
/// strings hold ([`inner_refusal`]).
#[derive(Clone, Copy)]
pub struct BodyWeight(pub usize);

/// The refusal of a request of `path` whose body, with the JSON documents
/// its strings hold (`json_weight::inner_documents`: tool calls'
/// `arguments`, which adapters and guardrails parse), would parse into more
/// than the budget: the same 413 and counter as a body over it. Checked by
/// every generation handler once its request is normalized, before
/// admission.
pub(crate) fn inner_refusal(
    path: &str,
    body: Option<axum::Extension<BodyWeight>>,
    request: &crate::inference::types::ChatRequest,
) -> Option<Response> {
    let body = body.map_or(0, |axum::Extension(BodyWeight(weight))| weight);
    refusal(
        path,
        body.saturating_add(json_weight::inner_documents(request)),
    )
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

/// The middleware of the inference routes outside the image input routes,
/// given the route's byte limit.
pub(super) async fn bound(
    State((limit, any_type)): State<(usize, bool)>,
    request: Request,
    next: Next,
) -> Response {
    // A handler that reads only a JSON type refuses any other unread; one
    // that reads a loosely typed body as JSON (`any_type`) is measured
    // whatever the type says, but a multipart form, which it reads as one.
    let skip = if any_type {
        multipart(request.headers())
    } else {
        !super::json_content_type(request.headers())
    };
    if skip {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    match measured(parts, body, limit).await {
        Measured::Run(request) => next.run(request).await,
        Measured::Refused(response) => response,
    }
}

/// What measuring a body decided.
pub(super) enum Measured {
    /// The request to run, with the body read whole.
    Run(Request),
    /// The response that refuses it.
    Refused(Response),
}

/// `body` read whole, at most `limit` bytes of it, and measured.
pub(super) async fn measured(parts: Parts, body: Body, limit: usize) -> Measured {
    let body = http_body_util::Limited::new(body, limit);
    let bytes = match http_body_util::BodyExt::collect(body).await {
        Ok(collected) => collected.to_bytes(),
        // Past the route's byte limit: the 413 the handler's extractor
        // would answer.
        Err(error) if over_limit(&*error) => {
            return Measured::Refused(if super::anthropic_path(parts.uri.path()) {
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
            return Measured::Run(Request::from_parts(parts, failed));
        }
    };
    let weight = json_weight::estimate(&bytes);
    if let Some(response) = refusal(parts.uri.path(), weight) {
        return Measured::Refused(response);
    }
    let mut parts = parts;
    parts.extensions.insert(BodyWeight(weight));
    Measured::Run(Request::from_parts(parts, Body::from(bytes)))
}

/// Whether the body is a multipart form (file uploads, transcriptions and
/// video references), which no handler parses as one JSON document. Every
/// other body is measured, whatever its `Content-Type` says, so a handler
/// that reads a loosely typed body as JSON never parses one unmeasured.
fn multipart(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.trim_start()
                .get(..10)
                .is_some_and(|p| p.eq_ignore_ascii_case("multipart/"))
        })
}

/// Whether a body read failed because it passed its byte limit.
fn over_limit(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(error) = source {
        if error.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = error.source();
    }
    false
}

#[cfg(test)]
mod tests {
    /// The `serde(...)` attribute items of `source` that make a type buffer
    /// its input into a generic tree first: `untagged`, `flatten`, `tag` (an
    /// internally or adjacently tagged enum) and `content`, wherever they
    /// stand in the attribute and however it is spread over lines.
    fn buffering_items(source: &str) -> Vec<String> {
        let compact: String = source.chars().filter(|c| !c.is_whitespace()).collect();
        let mut found = Vec::new();
        for (at, _) in compact.match_indices("serde(") {
            let rest = &compact[at + "serde(".len()..];
            let mut depth = 1usize;
            let end = rest
                .char_indices()
                .find(|&(_, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                })
                .map_or(rest.len(), |(i, _)| i);
            for item in rest[..end].split(',') {
                let name = item.split('=').next().unwrap_or_default();
                if matches!(name, "untagged" | "flatten" | "tag" | "content") {
                    found.push(item.to_owned());
                }
            }
        }
        found
    }

    #[test]
    fn the_scan_finds_every_spelling_of_a_buffering_shape() {
        for source in [
            "#[serde(untagged)]",
            "#[serde(rename_all = \"snake_case\", tag = \"type\")]",
            "#[serde(default, flatten)]",
            "#[serde(\n    deny_unknown_fields,\n    untagged\n)]",
            "#[serde(tag = \"t\", content = \"c\")]",
        ] {
            assert!(!buffering_items(source).is_empty(), "{source}");
        }
        for source in [
            "#[serde(deny_unknown_fields)]",
            "#[serde(rename = \"tag\")]",
        ] {
            assert!(buffering_items(source).is_empty(), "{source}");
        }
    }

    /// A tool call's `arguments` from a request is parsed only through
    /// `json_weight::parse_document`, which refuses one over the budget. The
    /// parses listed here read a provider's output (a response or a stream),
    /// never a request.
    #[test]
    fn request_arguments_are_parsed_only_when_measured() {
        const UPSTREAM: [(&str, &str); 3] = [
            (
                "providers/anthropic.rs",
                "serde_json::from_str::<Value>(&self.arguments)",
            ),
            (
                "providers/bedrock/stream.rs",
                "serde_json::from_str(&block.arguments)",
            ),
            (
                "protocols/messages.rs",
                "serde_json::from_str(&call.arguments)",
            ),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root.clone()];
        let mut found = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !name.ends_with(".rs") || name.contains("test") {
                    continue;
                }
                // This file holds the list above.
                if path.ends_with("http/json_structure.rs") {
                    continue;
                }
                let relative = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                for line in std::fs::read_to_string(&path).unwrap().lines() {
                    let parses = line.contains("from_str") || line.contains("from_slice");
                    if parses
                        && line.contains("arguments")
                        && !UPSTREAM
                            .iter()
                            .any(|(file, call)| relative == *file && line.contains(call))
                    {
                        found.push(format!("{relative}: {}", line.trim()));
                    }
                }
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    /// The request wire types deserialize directly: none of the serde shapes
    /// that first buffer a value into a generic tree, whose cost per value
    /// the structure estimate does not cover (#872). Every non-test source
    /// file of the request paths is read.
    #[test]
    fn request_types_use_no_buffering_serde_shape() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![
            root.join("protocols"),
            root.join("jobs"),
            root.join("inference"),
        ];
        let mut found = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if path.is_dir() {
                    stack.push(path);
                } else if name.ends_with(".rs") && !name.contains("test") {
                    let source = std::fs::read_to_string(&path).unwrap();
                    for item in buffering_items(&source) {
                        found.push(format!("{}: {item}", path.display()));
                    }
                }
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }
}
