//! OpenAI-compatible Videos API (async jobs, see `crate::jobs`):
//! - `POST /v1/videos`: multipart (as the official SDKs send it) or JSON
//!   `{model, prompt, seconds?, size?}`. Each route states the lengths and
//!   sizes it accepts (0097): a request naming one no route of its model
//!   takes is refused before admission (`unsupported_video_parameter`, with
//!   the parameter). The length (the route's first when none is named) is
//!   always sent upstream, so the billed duration is known; `size` only when
//!   the request names one. `input_reference`, remix, edits, extensions and
//!   characters are not supported.
//! - `GET /v1/videos` (gateway records of this workspace, cursor `after`),
//!   `GET /v1/videos/{id}`, `GET /v1/videos/{id}/content?variant=`,
//!   `DELETE /v1/videos/{id}`.
//!
//! Prompts and media are never logged or stored.
use axum::{
    Extension, Json,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::audio::multipart;
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{Engine, error::InferenceError},
    jobs::{
        JobError, Jobs,
        types::{JobKind, VideoAsset, VideoRequest, client_id},
        video::render,
    },
    store::Store,
};

/// Job error envelope: unknown/foreign ids are 404, invalid job state 400.
pub(crate) fn job_error(e: JobError) -> Response {
    let body = |status: StatusCode, code: &str, message: &str| {
        (
            status,
            Json(json!({"error":{"message":message,"type":"invalid_request_error","code":code,"param":null}})),
        )
            .into_response()
    };
    match e {
        JobError::NotFound => body(StatusCode::NOT_FOUND, "not_found", "No such object"),
        JobError::Conflict(code, message) | JobError::Invalid(code, message) => {
            body(StatusCode::BAD_REQUEST, code, message)
        }
        JobError::InvalidParameter(code, message, param) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":message,"type":"invalid_request_error","code":code,"param":param.as_str()}})),
        )
            .into_response(),
        JobError::Forbidden(code, message) => (
            StatusCode::FORBIDDEN,
            Json(json!({"error":{"message":message,"type":"permission_error","code":code,"param":null}})),
        )
            .into_response(),
        JobError::Inference(e) => super::workload_error(e, StatusCode::BAD_REQUEST),
    }
}

/// The video options the gateway takes. Any other field (another server's
/// `quality`, `seed`, `audio`, `aspect_ratio`, ...) is refused, naming it,
/// never dropped; so is OpenAI's `input_reference` unless it is `null`.
const FIELDS: [&str; 4] = ["model", "prompt", "seconds", "size"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRequest {
    model: Option<String>,
    prompt: Option<String>,
    seconds: Option<Value>,
    size: Option<String>,
    input_reference: Option<Value>,
}

/// The refusal of a request field the gateway does not take, naming it
/// (a name that is not a plain identifier is refused without one).
fn unsupported_field(name: &str) -> JobError {
    match crate::inference::error::FieldName::parse(name) {
        Some(field) => JobError::InvalidParameter(
            "unsupported_video_parameter",
            "The gateway does not take this video option",
            field,
        ),
        None => InferenceError::InvalidRequest.into(),
    }
}

/// A named clip length (decimal); `None` when the request names none.
fn seconds(value: Option<&str>) -> Result<Option<u32>, InferenceError> {
    value
        .map(|s| {
            if s.is_empty()
                || s.len() > 4
                || s.starts_with('0')
                || !s.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(InferenceError::InvalidRequest);
            }
            s.parse().map_err(|_| InferenceError::InvalidRequest)
        })
        .transpose()
}
fn normalize(
    model: Option<String>,
    prompt: Option<String>,
    secs: Option<&str>,
    size: Option<&str>,
) -> Result<VideoRequest, InferenceError> {
    let request = VideoRequest {
        // The gateway has no implicit default model.
        model: model.ok_or(InferenceError::InvalidRequest)?,
        prompt: prompt.ok_or(InferenceError::InvalidRequest)?,
        seconds: seconds(secs)?,
        size: size.map(str::to_owned),
    };
    request.validate()?;
    Ok(request)
}

fn parse(headers: &HeaderMap, body: &[u8]) -> Result<VideoRequest, JobError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or(InferenceError::InvalidRequest)?;
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if media == "application/json" {
        let value: Value =
            serde_json::from_slice(body).map_err(|_| InferenceError::InvalidRequest)?;
        if let Value::Object(fields) = &value
            && let Some(other) = fields
                .keys()
                .find(|k| !FIELDS.contains(&k.as_str()) && *k != "input_reference")
        {
            return Err(unsupported_field(other));
        }
        let r: JsonRequest =
            serde_json::from_value(value).map_err(|_| InferenceError::InvalidRequest)?;
        if r.input_reference.is_some() {
            return Err(unsupported_field("input_reference"));
        }
        let secs = match &r.seconds {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) if n.is_u64() => Some(n.to_string()),
            Some(_) => return Err(InferenceError::InvalidRequest.into()),
        };
        return Ok(normalize(
            r.model,
            r.prompt,
            secs.as_deref(),
            r.size.as_deref(),
        )?);
    }
    let boundary = multipart::boundary(content_type)?;
    let parts = multipart::parse(body, &boundary, 8)?;
    let mut fields: std::collections::BTreeMap<&str, String> = Default::default();
    for part in &parts {
        let name = part.name.as_str();
        if !FIELDS.contains(&name) {
            return Err(unsupported_field(name));
        }
        if part.filename.is_some() {
            return Err(InferenceError::InvalidRequest.into());
        }
        let text = std::str::from_utf8(part.data)
            .map_err(|_| InferenceError::InvalidRequest)?
            .to_owned();
        if fields.insert(name, text).is_some() {
            return Err(InferenceError::InvalidRequest.into());
        }
    }
    Ok(normalize(
        fields.remove("model"),
        fields.remove("prompt"),
        fields.get("seconds").map(String::as_str),
        fields.get("size").map(String::as_str),
    )?)
}

pub async fn create(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    files: Option<Extension<crate::filestore::FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let body = match body {
        Ok(b) => b,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return super::payload_too_large(),
        Err(_) => return job_error(InferenceError::InvalidRequest.into()),
    };
    let request = match parse(&headers, &body) {
        Ok(r) => r,
        Err(e) => return job_error(e),
    };
    drop(body);
    let jobs = Jobs::new(store, &engine).with_files(files.map(|Extension(f)| f));
    match jobs.create_video(principal, request, request_id.0).await {
        Ok(job) => Json(render(&job)).into_response(),
        Err(e) => job_error(e),
    }
}

pub async fn retrieve(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    files: Option<Extension<crate::filestore::FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match Jobs::new(store, &engine)
        .with_files(files.map(|Extension(f)| f))
        .get_video(&principal, &id)
        .await
    {
        Ok(job) => Json(render(&job)).into_response(),
        Err(e) => job_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub(crate) after: Option<String>,
    pub(crate) limit: Option<i64>,
    pub(crate) order: Option<String>,
}
/// `limit` 1..=100 (default 20), `order` asc|desc (default desc).
pub(crate) fn page_params(q: &ListQuery) -> Result<(i64, bool), JobError> {
    let limit = q.limit.unwrap_or(20);
    let ascending = match q.order.as_deref() {
        None | Some("desc") => false,
        Some("asc") => true,
        Some(_) => return Err(InferenceError::InvalidRequest.into()),
    };
    if !(1..=100).contains(&limit) {
        return Err(InferenceError::InvalidRequest.into());
    }
    Ok((limit, ascending))
}
pub(crate) fn list_page(mut data: Vec<Value>, limit: i64) -> Value {
    let has_more = data.len() > limit as usize;
    data.truncate(limit as usize);
    let first = data.first().map(|v| v["id"].clone());
    let last = data.last().map(|v| v["id"].clone());
    json!({"object":"list","data":data,"first_id":first,"last_id":last,"has_more":has_more})
}

pub async fn list(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    files: Option<Extension<crate::filestore::FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    query: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(q)) = query else {
        return job_error(InferenceError::InvalidRequest.into());
    };
    let (limit, ascending) = match page_params(&q) {
        Ok(p) => p,
        Err(e) => return job_error(e),
    };
    match Jobs::new(store, &engine)
        .with_files(files.map(|Extension(f)| f))
        .list_videos(&principal, q.after.as_deref(), limit + 1, ascending)
        .await
    {
        Ok(rows) => Json(list_page(rows.iter().map(render).collect(), limit)).into_response(),
        Err(e) => job_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentQuery {
    variant: Option<String>,
}

pub(crate) fn stream_response(content: crate::jobs::types::ContentStream) -> Response {
    let mut response = Response::new(Body::from_stream(content.body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content.content_type),
    );
    if let Some(n) = content.content_length {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    }
    // The body is the provider's or a stored copy of it: never sniffed into
    // another type than the one stated.
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

pub async fn content(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    files: Option<Extension<crate::filestore::FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    query: Result<Query<ContentQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(q)) = query else {
        return job_error(InferenceError::InvalidRequest.into());
    };
    let asset = match q.variant.as_deref().map(VideoAsset::parse) {
        None => VideoAsset::Video,
        Some(Some(a)) => a,
        Some(None) => return job_error(InferenceError::InvalidRequest.into()),
    };
    match Jobs::new(store, &engine)
        .with_files(files.map(|Extension(f)| f))
        .video_content(&principal, &id, asset)
        .await
    {
        Ok(content) => stream_response(content),
        Err(e) => job_error(e),
    }
}

pub async fn delete(
    State(store): State<Store>,
    Extension(engine): Extension<Engine>,
    files: Option<Extension<crate::filestore::FileStoreRuntime>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match Jobs::new(store, &engine)
        .with_files(files.map(|Extension(f)| f))
        .delete_video(&principal, &id)
        .await
    {
        // `upstream_deleted`: whether the provider's copy is gone (`false`
        // when its delete was not offered, failed or could not be sent; the
        // copy then stays until the provider's expiry).
        Ok((job, upstream_deleted)) => Json(json!({
            "id": client_id(JobKind::Video.prefix(), job.id),
            "object": "video.deleted",
            "deleted": true,
            "upstream_deleted": upstream_deleted,
        }))
        .into_response(),
        Err(e) => job_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn headers(ct: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, ct.parse().unwrap());
        h
    }
    /// The parameter an `unsupported_video_parameter` refusal names.
    fn named(e: Option<JobError>) -> Option<String> {
        match e {
            Some(JobError::InvalidParameter("unsupported_video_parameter", _, f)) => {
                Some(f.as_str().to_owned())
            }
            _ => None,
        }
    }
    fn invalid(e: Option<JobError>) -> bool {
        matches!(e, Some(JobError::Inference(InferenceError::InvalidRequest)))
    }
    #[test]
    fn requests_are_strict_and_explicit() {
        let json = |v: Value| parse(&headers("application/json"), v.to_string().as_bytes());
        // Nothing named: the route decides the length, and no size is sent.
        let r = json(json!({"model":"vid","prompt":"a cat"})).ok().unwrap();
        assert_eq!((r.seconds, r.size), (None, None));
        let r = json(json!({"model":"vid","prompt":"a cat","seconds":"6","size":"1344x768"}))
            .ok()
            .unwrap();
        assert_eq!((r.seconds, r.size.as_deref()), (Some(6), Some("1344x768")));
        assert!(json(json!({"model":"vid","prompt":"a","seconds":8})).is_ok());
        for bad in [
            json!({"prompt":"a"}),
            json!({"model":"vid"}),
            json!({"model":"vid","prompt":"a","seconds":"0"}),
            json!({"model":"vid","prompt":"a","seconds":"3601"}),
            json!({"model":"vid","prompt":"a","seconds":"+5"}),
            json!({"model":"vid","prompt":"a","seconds":"5.5"}),
            json!({"model":"vid","prompt":"a","seconds":"012"}),
            json!({"model":"vid","prompt":"a","seconds":true}),
            json!({"model":"vid","prompt":"a","size":"auto"}),
            json!({"model":"vid","prompt":"a","size":"01280x720"}),
            json!({"model":"vid","prompt":"a","size":1280}),
            json!({"model":"vid","prompt":" "}),
            json!(["model"]),
        ] {
            assert!(invalid(json(bad.clone()).err()), "{bad}");
        }
        // An option the gateway does not take is refused by name.
        for field in [
            "quality",
            "seed",
            "audio",
            "aspect_ratio",
            "input_reference",
            "extra",
        ] {
            let mut body = json!({"model":"vid","prompt":"a"});
            body[field] = json!("x");
            assert_eq!(named(json(body).err()).as_deref(), Some(field));
        }
        assert!(invalid(
            json(json!({"model":"vid","prompt":"a","bad-name":1})).err()
        ));
        let multipart =
            |form: &str| parse(&headers("multipart/form-data; boundary=B"), form.as_bytes());
        let form = "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nvid\r\n--B\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\na dog\r\n--B\r\nContent-Disposition: form-data; name=\"seconds\"\r\n\r\n8\r\n--B\r\nContent-Disposition: form-data; name=\"size\"\r\n\r\n1280x720\r\n--B--\r\n";
        let r = multipart(form).ok().unwrap();
        assert_eq!(
            (r.model.as_str(), r.seconds, r.size.as_deref()),
            ("vid", Some(8), Some("1280x720"))
        );
        let reference = "--B\r\nContent-Disposition: form-data; name=\"input_reference\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n--B--\r\n";
        assert_eq!(
            named(multipart(reference).err()).as_deref(),
            Some("input_reference")
        );
        let seed = "--B\r\nContent-Disposition: form-data; name=\"seed\"\r\n\r\n7\r\n--B--\r\n";
        assert_eq!(named(multipart(seed).err()).as_deref(), Some("seed"));
        let file = "--B\r\nContent-Disposition: form-data; name=\"prompt\"; filename=\"p.txt\"\r\n\r\nx\r\n--B--\r\n";
        assert!(invalid(multipart(file).err()));
        assert!(parse(&headers("text/plain"), b"x").is_err());
    }
}
