//! Files referenced by `file_id` in Chat Completions and Responses (#51).
//!
//! A reference is resolved before routing, inside the key's workspace and
//! with the Files API's own visibility (`FileStorage::get_visible`): the
//! creator, a service key or a workspace owner or admin, as the workspace's
//! object visibility allows. Every way a key may not have a file (no such
//! id, a malformed one, another workspace's, deleted, expired, not visible
//! to this key) is the same refusal, `file_not_found`, decided by one
//! lookup before anything else about the file is looked at.
//!
//! Then, in this order: the purpose (`user_data`, `vision`, `assistants`),
//! the size, the key's file reads this minute, the replica's large input
//! buffer (the stored size and its base64 form, under the key's share), a
//! bounded read of the whole file, the same visibility check again (a file
//! deleted, expired or hidden while it was read is `file_not_found`, never
//! a partial body) and the type, decided from the bytes alone. An image
//! becomes the request's inline image (`image_input::Images::take` checks
//! it as one); bytes of any other type are refused naming the field.
//!
//! Nothing of a file reaches a log, a metric label, an error, a trace or the
//! database: the bytes live in the request only, and errors carry fixed
//! codes and the field name, never the id or the file's name.
use std::time::Duration;

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use futures_util::StreamExt as _;
use sha2::Digest as _;

use crate::{
    auth::Principal,
    filestore::files::{FileStorage, StoredFile, parse_public_id, public_id},
    inference::{
        error::InferenceError,
        image_buffer::{Admission, BufferPermit, ImageBuffer},
        types::ImageType,
    },
    visibility::Viewer,
};

/// File references per request (as many as inline images).
pub const MAX_FILES: usize = super::image_input::MAX_IMAGES;
/// The largest file a request may reference: an image's decoded limit.
pub const MAX_FILE_BYTES: u64 = super::image_input::MAX_DECODED_BYTES as u64;
/// Purposes whose files a request may reference.
pub const PURPOSES: [&str; 3] = ["user_data", "vision", "assistants"];
/// A read that receives nothing for this long fails.
const READ_IDLE: Duration = Duration::from_secs(10);
/// A file's read in all.
const READ_TOTAL: Duration = Duration::from_secs(30);

/// One `file_id` of a request and the field that names it in a refusal.
pub struct Reference {
    pub id: String,
    pub field: &'static str,
}

/// Why a reference was refused. Each names the field, never the id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The key may not have this file (any reason: see the module).
    NotFound(&'static str),
    Purpose(&'static str),
    TooLarge(&'static str),
    /// The bytes are none of the types this request may carry.
    UnsupportedType(&'static str),
    /// More references than [`MAX_FILES`].
    TooMany(&'static str),
    /// The replica's large input buffer, or this key's share of it, is
    /// spent for now: nothing was read.
    Busy,
    /// The key's file reads this minute are spent: nothing was read.
    Throttled(u32),
    /// The database or the file store failed.
    Storage,
}

impl Refusal {
    pub fn response(self) -> Response {
        let (status, kind, code, message, field) = match self {
            Self::NotFound(field) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "file_not_found",
                "No such file, or this API key may not read it",
                field,
            ),
            Self::Purpose(field) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "file_purpose_not_supported",
                "Only files uploaded with purpose user_data, vision or assistants can be referenced",
                field,
            ),
            Self::TooLarge(field) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "file_too_large",
                "The referenced file is larger than a request may carry (5 MiB)",
                field,
            ),
            Self::UnsupportedType(field) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "unsupported_file_type",
                "The referenced file is not a PNG, JPEG, WebP or GIF image",
                field,
            ),
            Self::TooMany(field) => {
                return super::chat_completions::error_response(InferenceError::InvalidField(
                    field,
                ));
            }
            Self::Busy => return super::image_input_busy(),
            Self::Throttled(seconds) => {
                let mut response = (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(serde_json::json!({"error":{"message":"Too many file reads by this API key this minute; nothing was read or sent. Retry after the indicated delay","type":"rate_limit_error","code":"file_reads_rate_limited","param":null}})),
                )
                    .into_response();
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(seconds.max(1)));
                return response;
            }
            Self::Storage => {
                return super::chat_completions::error_response(InferenceError::Storage);
            }
        };
        let mut response = (
            status,
            // The field leads the message, as every field refusal of these
            // routes names it (`param` stays null in the OpenAI envelope).
            axum::Json(serde_json::json!({"error":{"message":format!("{field}: {message}"),"type":kind,"code":code,"param":null}})),
        )
            .into_response();
        response
            .headers_mut()
            .insert("x-should-retry", HeaderValue::from_static("false"));
        response
    }
}

/// What a reference resolved to.
pub enum Resolved {
    /// An image, as the data URL an inline image would carry.
    Image(String),
}

/// A request's resolved references, in order, and the buffer share their
/// bytes hold until the response ends.
pub struct Files {
    pub contents: Vec<Resolved>,
    pub permit: Option<BufferPermit>,
    /// Bytes of data URLs the files added to the request.
    pub data_bytes: usize,
}

/// The base64 length of `bytes` bytes, with the longest data URL prefix.
fn data_url_len(bytes: u64) -> usize {
    usize::try_from(bytes.div_ceil(3) * 4).unwrap_or(usize::MAX)
        + crate::inference::image_buffer::LONGEST_IMAGE_PREFIX
}

/// Resolves `references` for `principal` (see the module for the order).
pub async fn resolve(
    files: &FileStorage,
    buffer: &ImageBuffer,
    principal: &Principal,
    references: &[Reference],
) -> Result<Files, Refusal> {
    if references.is_empty() {
        return Ok(Files {
            contents: Vec::new(),
            permit: None,
            data_bytes: 0,
        });
    }
    if let Some(extra) = references.get(MAX_FILES) {
        return Err(Refusal::TooMany(extra.field));
    }
    let viewer = Viewer::of(principal);
    let workspace = principal.workspace_id;
    // Every reference is looked up before any purpose, size or type is
    // considered, so a missing file and a present one take the same path.
    let mut found = Vec::with_capacity(references.len());
    for reference in references {
        found.push((
            visible(files, &reference.id, workspace, viewer).await?,
            reference.field,
        ));
    }
    let mut stored = Vec::with_capacity(found.len());
    for (file, field) in found {
        let file = file.ok_or(Refusal::NotFound(field))?;
        stored.push((file, field));
    }
    for (file, field) in &stored {
        if !file
            .api_purpose
            .as_deref()
            .is_some_and(|purpose| PURPOSES.contains(&purpose))
        {
            return Err(Refusal::Purpose(field));
        }
        if file.size_bytes > MAX_FILE_BYTES {
            return Err(Refusal::TooLarge(field));
        }
    }
    if let Err(seconds) = buffer.admit_file_reads(principal.key_id, stored.len()) {
        crate::metrics::METRICS.image_input_refused("file_reads");
        return Err(Refusal::Throttled(seconds));
    }
    // The stored bytes and their base64 form are held at once.
    let held: usize = stored
        .iter()
        .map(|(file, _)| {
            usize::try_from(file.size_bytes).unwrap_or(usize::MAX) + data_url_len(file.size_bytes)
        })
        .fold(0, usize::saturating_add);
    let permit = match buffer.reserve(held, principal.key_id) {
        Admission::Large(permit) => permit,
        Admission::KeyShare => {
            crate::metrics::METRICS.image_input_refused("key_share");
            return Err(Refusal::Busy);
        }
        Admission::Busy | Admission::TooLarge | Admission::Text => {
            crate::metrics::METRICS.image_input_refused("busy");
            return Err(Refusal::Busy);
        }
    };
    let mut contents = Vec::with_capacity(stored.len());
    let mut data_bytes = 0usize;
    for (file, field) in &stored {
        let read = read(files, file).await;
        // Visible when its read began and still visible now that it ended:
        // a file deleted, expired or hidden meanwhile (its read may have
        // failed for that reason) is not found, whatever the read gave.
        if visible(files, &public_id(file.id), workspace, viewer)
            .await?
            .is_none()
        {
            return Err(Refusal::NotFound(field));
        }
        let bytes = read?;
        let Some(kind) = ImageType::ALL.into_iter().find(|t| t.matches(&bytes)) else {
            return Err(Refusal::UnsupportedType(field));
        };
        let mut url = String::with_capacity(data_url_len(file.size_bytes));
        url.push_str(kind.data_url_prefix());
        base64::engine::general_purpose::STANDARD.encode_string(&bytes, &mut url);
        drop(bytes);
        data_bytes += url.len();
        contents.push(Resolved::Image(url));
    }
    Ok(Files {
        contents,
        permit: Some(permit),
        data_bytes,
    })
}

/// The live Files API file `id` names in `workspace`, if this viewer may
/// read it. A malformed id is no file.
async fn visible(
    files: &FileStorage,
    id: &str,
    workspace: uuid::Uuid,
    viewer: Viewer,
) -> Result<Option<StoredFile>, Refusal> {
    let Some(id) = parse_public_id(id) else {
        return Ok(None);
    };
    match files.get_visible(id, workspace, viewer).await {
        Ok(file) => Ok(file.filter(|f| f.api_purpose.is_some())),
        Err(_) => Err(Refusal::Storage),
    }
}

/// The whole decrypted file, within its stored size and the read deadlines,
/// checked against its stored length and digest.
async fn read(files: &FileStorage, file: &StoredFile) -> Result<Vec<u8>, Refusal> {
    let store = files.runtime().store().ok_or(Refusal::Storage)?;
    let limit = usize::try_from(file.size_bytes).map_err(|_| Refusal::Storage)?;
    let deadline = tokio::time::Instant::now() + READ_TOTAL;
    let read = async {
        let mut stream = store.get(&file.key).await.map_err(|_| Refusal::Storage)?;
        let mut bytes = Vec::with_capacity(limit);
        loop {
            let next = tokio::time::timeout(READ_IDLE, stream.next())
                .await
                .map_err(|_| Refusal::Storage)?;
            match next {
                None => break,
                Some(Ok(chunk)) => {
                    if bytes.len() + chunk.len() > limit {
                        return Err(Refusal::Storage);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Some(Err(_)) => return Err(Refusal::Storage),
            }
        }
        Ok(bytes)
    };
    let bytes = tokio::time::timeout_at(deadline, read)
        .await
        .map_err(|_| Refusal::Storage)??;
    if bytes.len() != limit || sha2::Sha256::digest(&bytes).as_slice() != file.sha256 {
        return Err(Refusal::Storage);
    }
    Ok(bytes)
}

/// `response` holding `permit` until its body ends; at the latest the share
/// is back once the request can no longer be running.
pub fn holding(response: Response, permit: Option<BufferPermit>, after: Duration) -> Response {
    let Some(permit) = permit else {
        return response;
    };
    permit.release_after(READ_TOTAL + after);
    response.map(|body| {
        axum::body::Body::new(crate::http::Holding {
            body,
            _permit: permit,
        })
    })
}

/// [`resolve`] for a handler: the file store and the buffer from its
/// extensions, and the refusal as the response to send.
pub async fn resolve_request(
    store: crate::store::Store,
    runtime: Option<axum::Extension<crate::filestore::FileStoreRuntime>>,
    buffer: Option<axum::Extension<ImageBuffer>>,
    principal: &Principal,
    references: &[Reference],
) -> Result<Files, Response> {
    if references.is_empty() {
        return Ok(Files {
            contents: Vec::new(),
            permit: None,
            data_bytes: 0,
        });
    }
    // Mounted only behind the buffer's layer (`http::image_input_routes`).
    let Some(axum::Extension(buffer)) = buffer else {
        return Err(Refusal::Storage.response());
    };
    let files = super::files::storage(store, runtime);
    resolve(&files, &buffer, principal, references)
        .await
        .map_err(Refusal::response)
}

/// Whether a Responses content `part` references a stored file and nothing
/// else: an `input_image` or `input_file` with a `file_id` (and an image's
/// `detail`).
fn is_responses_reference(part: &serde_json::Value) -> bool {
    use serde_json::Value;
    let Some(part) = part.as_object() else {
        return false;
    };
    let allowed: &[&str] = match part.get("type").and_then(Value::as_str) {
        Some("input_image") => &["type", "file_id", "detail"],
        Some("input_file") => &["type", "file_id"],
        _ => return false,
    };
    part.get("file_id").is_some_and(Value::is_string)
        && part.keys().all(|k| allowed.contains(&k.as_str()))
}

/// The file references of a Responses `input`'s user message items, in
/// order: what [`resolve`] takes. Any other reference stays for the parser
/// to refuse by name.
pub fn responses_references(
    input: &mut serde_json::Value,
) -> impl Iterator<Item = &mut serde_json::Value> {
    use serde_json::Value;
    input
        .as_array_mut()
        .into_iter()
        .flatten()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                None | Some("message")
            ) && item.get("role").and_then(Value::as_str) == Some("user")
        })
        .filter_map(|item| item.get_mut("content")?.as_array_mut())
        .flatten()
        .filter(|part| is_responses_reference(part))
}

/// A resolved Responses reference: the inline `input_image` with `url`,
/// keeping an image's `detail`.
pub fn responses_image(part: &mut serde_json::Value, url: String) {
    use serde_json::Value;
    let detail = part.get_mut("detail").map(Value::take);
    let mut image = serde_json::Map::new();
    image.insert("type".to_owned(), Value::from("input_image"));
    image.insert("image_url".to_owned(), Value::String(url));
    if let Some(detail) = detail {
        image.insert("detail".to_owned(), detail);
    }
    *part = Value::Object(image);
}
