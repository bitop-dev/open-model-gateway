//! The JSON body extractor of the inference routes (#872): the body's bytes,
//! read under the route's limit as Axum's `Json` reads them, parsed with
//! `serde_json::from_slice` alone.
//!
//! Axum's `Json` parses through `serde_path_to_error`, which keeps a copy of
//! every map key on the path being parsed (and copies the path again into a
//! rejection), so a body of long nested keys allocated three to four times
//! the bound the structure estimate proves for a `serde_json` parse
//! (`inference::json_weight`). This extractor parses exactly as that proof
//! measures. A handler learns only what it reads from a rejection: its
//! status, and `serde_json`'s own message (from which the Messages handler
//! names an unknown field).
use axum::{
    body::Bytes,
    extract::{FromRequest, Request, rejection::BytesRejection},
    http::StatusCode,
};
use serde::de::DeserializeOwned;

/// A request body parsed as `T`.
pub struct JsonBody<T>(pub T);

/// Why a body was not parsed.
pub enum JsonBodyRejection {
    /// `Content-Type` is not JSON (as Axum's `Json` decides it).
    ContentType,
    /// The body could not be read: past its limit, or a read failure.
    Body(BytesRejection),
    /// The bytes are not valid JSON, or not a `T`.
    Data(serde_json::Error),
}

impl JsonBodyRejection {
    /// The status Axum's `Json` gives the same rejection.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::ContentType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Body(rejection) => rejection.status(),
            Self::Data(error) if error.is_data() => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Data(_) => StatusCode::BAD_REQUEST,
        }
    }
    /// `serde_json`'s message for a body that is not a `T`; a fixed text
    /// otherwise. Never returned to a client as such: it can hold body text.
    pub fn body_text(&self) -> String {
        match self {
            Self::Data(error) => error.to_string(),
            Self::ContentType => "Expected request with `Content-Type: application/json`".into(),
            Self::Body(_) => "Failed to read the request body".into(),
        }
    }
}

/// Every handler takes the rejection and answers in its own shape; this
/// fallback answers with the status alone, never the body's text.
impl axum::response::IntoResponse for JsonBodyRejection {
    fn into_response(self) -> axum::response::Response {
        self.status().into_response()
    }
}

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for JsonBody<T> {
    type Rejection = JsonBodyRejection;
    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !super::json_content_type(request.headers()) {
            return Err(JsonBodyRejection::ContentType);
        }
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(JsonBodyRejection::Body)?;
        serde_json::from_slice(&bytes)
            .map(JsonBody)
            .map_err(JsonBodyRejection::Data)
    }
}
