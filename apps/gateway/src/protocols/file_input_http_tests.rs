//! Images referenced by `file_id` in Chat Completions and Responses (#51)
//! through the real router, engine and OpenAI adapter on PostgreSQL, with an
//! encrypted in-memory file store: the file reaches the upstream as the
//! inline image would; the Files API's visibility in both modes; every way a
//! key may not have a file is one refusal; purpose, size, type, count and
//! inline data refused by name; a file hidden while it is read; per-key
//! reads and the buffer bounding reads before admission, with nothing held
//! after a refusal; and a file's bytes in no table, metric, log or answer.
use std::sync::Arc;

use axum::{
    Extension,
    body::Body,
    http::{Request, StatusCode, header},
};
use base64::Engine as _;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use super::image_input_http_tests::{
    World, allow_images, assert_nowhere, executions, in_use, png_url, world,
};
use crate::{
    filestore::{ByteStream, FileStore, FileStoreError, FileStoreRuntime, files::public_id},
    inference::image_buffer::{ImageInputLimits, MIB},
    visibility::test_support::{CALLERS, Owner, Who, expected, team},
};

const BOUNDARY: &str = "fileInputBoundary9";

/// The PNG bytes of [`png_url`].
fn png(decoded: usize, marker: &str) -> Vec<u8> {
    let url = png_url(decoded, marker);
    base64::engine::general_purpose::STANDARD
        .decode(url.split_once(',').unwrap().1)
        .unwrap()
}

fn form(purpose: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\n{purpose}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"picture.png\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    out
}

/// A world whose router has a file store, with user and batch files on and
/// image input allowed on its route.
async fn files_world(pool: PgPool, limits: ImageInputLimits) -> World {
    files_world_on(pool, limits, FileStoreRuntime::memory()).await
}

async fn files_world_on(pool: PgPool, limits: ImageInputLimits, rt: FileStoreRuntime) -> World {
    let mut w = world(pool, limits, true).await;
    allow_images(&w, true).await;
    sqlx::query(
        "UPDATE installation_settings SET file_batch_enabled=true,file_user_files_enabled=true",
    )
    .execute(&w.f.store.pool)
    .await
    .unwrap();
    w.router = w.router.clone().layer(Extension(rt));
    w
}

async fn call(
    w: &World,
    bearer: &str,
    method: &str,
    path: &str,
    body: Body,
    form: bool,
) -> (StatusCode, Option<String>, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, bearer);
    request = request.header(
        header::CONTENT_TYPE,
        if form {
            format!("multipart/form-data; boundary={BOUNDARY}")
        } else {
            "application/json".to_owned()
        },
    );
    let response = w
        .router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let retry = response
        .headers()
        .get(header::RETRY_AFTER)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        retry,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn upload(w: &World, bearer: &str, purpose: &str, data: &[u8]) -> String {
    let (status, _, file) = call(
        w,
        bearer,
        "POST",
        "/v1/files",
        Body::from(form(purpose, data)),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{file}");
    file["id"].as_str().unwrap().to_owned()
}

fn chat_file(id: &str) -> Value {
    json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":[
        {"type":"text","text":"what is this?"},
        {"type":"file","file":{"file_id":id}}]}]})
}

async fn chat(w: &World, bearer: &str, body: &Value) -> (StatusCode, Option<String>, Value) {
    call(
        w,
        bearer,
        "POST",
        "/v1/chat/completions",
        Body::from(serde_json::to_vec(body).unwrap()),
        false,
    )
    .await
}

fn bearer(w: &World) -> String {
    format!("Bearer {}", w.token)
}

#[dbtest::test]
async fn a_referenced_image_reaches_the_upstream_as_an_inline_image_does(pool: PgPool) {
    let w = files_world(pool, ImageInputLimits::default()).await;
    let me = bearer(&w);
    let bytes = png(4096, "picture");
    let id = upload(&w, &me, "vision", &bytes).await;
    let url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    let (status, _, value) = chat(&w, &me, &chat_file(&id)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let responses = json!({"model":"company/smart","input":[{"role":"user","content":[
        {"type":"input_text","text":"what is this?"},
        {"type":"input_image","file_id":id,"detail":"low"},
        {"type":"input_file","file_id":id}]}]});
    let (status, _, value) = call(
        &w,
        &me,
        "POST",
        "/v1/responses",
        Body::from(responses.to_string()),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let bodies = w.upstream.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2);
    let parts = &bodies[0]["messages"][0]["content"];
    assert_eq!(
        parts[1],
        json!({"type":"image_url","image_url":{"url":url}})
    );
    let input = &bodies[1]["input"][0]["content"];
    assert_eq!(
        input[1],
        json!({"type":"input_image","image_url":url,"detail":"low"})
    );
    assert_eq!(input[2], json!({"type":"input_image","image_url":url}));
    // The gateway's id never reaches a provider.
    for body in &bodies {
        assert!(!body.to_string().contains(&id), "{body}");
    }
}

#[dbtest::test]
async fn every_way_a_key_may_not_have_a_file_is_one_refusal(pool: PgPool) {
    let w = files_world(pool, ImageInputLimits::default()).await;
    let team = team(&w.f).await;
    let image = png(512, "matrix");
    let mut files = Vec::new();
    for owner in [Owner::Member, Owner::Service] {
        files.push((
            owner,
            upload(&w, &team.owner(owner).bearer, "user_data", &image).await,
        ));
    }
    // The answer every refusal below must equal, from an id that never existed.
    let (status, _, refusal) = chat(
        &w,
        &team.get(Who::Owner).bearer,
        &chat_file(&public_id(Uuid::new_v4())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        refusal,
        json!({"error":{"message":"messages.content.file.file_id: No such file, or this API key may not read it","type":"invalid_request_error","code":"file_not_found","param":null}})
    );
    let mut served = 0;
    for mode in ["member", "workspace"] {
        team.mode(&w.f, mode).await;
        for (owner, id) in &files {
            for who in CALLERS {
                let (status, _, value) = chat(&w, &team.get(who).bearer, &chat_file(id)).await;
                if expected(mode, *owner, who) {
                    assert_eq!(status, StatusCode::OK, "{mode} {owner:?} {who:?} {value}");
                    served += 1;
                } else {
                    assert_eq!(
                        (status, &value),
                        (StatusCode::BAD_REQUEST, &refusal),
                        "{mode} {owner:?} {who:?}"
                    );
                }
            }
        }
    }
    // A malformed id, another workspace's file, a deleted and an expired one.
    let personal = upload(&w, &bearer(&w), "user_data", &image).await;
    let deleted = upload(&w, &team.get(Who::Owner).bearer, "user_data", &image).await;
    let (status, _, _) = call(
        &w,
        &team.get(Who::Owner).bearer,
        "DELETE",
        &format!("/v1/files/{deleted}"),
        Body::empty(),
        false,
    )
    .await;
    assert!(status.is_success(), "{status}");
    let expired = upload(&w, &team.get(Who::Owner).bearer, "user_data", &image).await;
    sqlx::query("UPDATE stored_files SET expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(crate::filestore::files::parse_public_id(&expired).unwrap())
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    for id in ["file-xyz", "not-a-file", &personal, &deleted, &expired] {
        let (status, _, value) = chat(&w, &team.get(Who::Owner).bearer, &chat_file(id)).await;
        assert_eq!(
            (status, &value),
            (StatusCode::BAD_REQUEST, &refusal),
            "{id}"
        );
    }
    // Only what was served reached the provider or the accounting.
    assert_eq!(w.upstream.bodies.lock().unwrap().len(), served);
    assert_eq!(executions(&w).await, served as i64);
}

#[dbtest::test]
async fn purpose_size_type_count_and_inline_data_are_refused_by_name(pool: PgPool) {
    let w = files_world(pool, ImageInputLimits::default()).await;
    let me = bearer(&w);
    let batch = upload(&w, &me, "batch", b"{\"custom_id\":\"a\"}\n").await;
    let text = upload(&w, &me, "user_data", b"plain text, not an image").await;
    let big = upload(&w, &me, "user_data", &png(5 * MIB + 1, "big")).await;
    let image = upload(&w, &me, "vision", &png(256, "ok")).await;
    let code = |v: &Value| {
        let field = v["error"]["message"]
            .as_str()
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_owned();
        (v["error"]["code"].clone(), json!(field))
    };
    for (id, status, expected) in [
        (
            &batch,
            StatusCode::BAD_REQUEST,
            "file_purpose_not_supported",
        ),
        (&text, StatusCode::BAD_REQUEST, "unsupported_file_type"),
        (&big, StatusCode::PAYLOAD_TOO_LARGE, "file_too_large"),
    ] {
        let (got, _, value) = chat(&w, &me, &chat_file(id)).await;
        assert_eq!(
            (got, code(&value)),
            (
                status,
                (json!(expected), json!("messages.content.file.file_id"))
            ),
            "{value}"
        );
    }
    let too_many: Vec<Value> = (0..=crate::protocols::file_input::MAX_FILES)
        .map(|_| json!({"type":"file","file":{"file_id":image}}))
        .collect();
    let body = json!({"model":"company/smart","messages":[{"role":"user","content":too_many}]});
    let (status, _, value) = chat(&w, &me, &body).await;
    assert_eq!(
        (status, value["error"]["message"].as_str()),
        (
            StatusCode::BAD_REQUEST,
            Some("messages.content.file.file_id")
        )
    );
    // Inline data, a file outside a user message, and Responses' URL and data.
    for (path, body, field) in [
        (
            "/v1/chat/completions",
            json!({"model":"company/smart","messages":[{"role":"user","content":[{"type":"file","file":{"file_data":"data:application/pdf;base64,JVBERi0=","filename":"a.pdf"}}]}]}),
            "messages.content.file.file_data",
        ),
        (
            "/v1/chat/completions",
            json!({"model":"company/smart","messages":[{"role":"system","content":[{"type":"file","file":{"file_id":image}}]}]}),
            "messages.content.file.file_id",
        ),
        (
            "/v1/responses",
            json!({"model":"company/smart","input":[{"role":"user","content":[{"type":"input_file","file_url":"https://example.invalid/a.pdf"}]}]}),
            "input.content.file_url",
        ),
        (
            "/v1/responses",
            json!({"model":"company/smart","input":[{"role":"user","content":[{"type":"input_file","file_data":"JVBERi0=","filename":"a.pdf"}]}]}),
            "input.content.file_data",
        ),
    ] {
        let (status, _, value) =
            call(&w, &me, "POST", path, Body::from(body.to_string()), false).await;
        assert_eq!(
            (status, value["error"]["message"].as_str()),
            (StatusCode::BAD_REQUEST, Some(field)),
            "{path} {value}"
        );
    }
    assert!(w.upstream.bodies.lock().unwrap().is_empty());
    assert_eq!(executions(&w).await, 0);
}

#[test]
fn a_batch_line_does_not_resolve_a_reference_and_refuses_it_by_name() {
    use crate::inference::error::InferenceError;
    let chat =
        super::chat_completions::batch_request(chat_file("file-00000000000000000000000000000000"));
    assert_eq!(
        chat.err(),
        Some(InferenceError::InvalidField(
            "messages.content.file.file_id"
        ))
    );
    for part in [
        json!({"type":"input_file","file_id":"file-0"}),
        json!({"type":"input_image","file_id":"file-0"}),
    ] {
        let line = json!({"model":"m","input":[{"role":"user","content":[part]}]});
        assert_eq!(
            super::responses::batch_request(line).err(),
            Some(InferenceError::InvalidField("input.content.file_id"))
        );
    }
}

/// What [`Wrapped`] does to a file's read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Expires the file while it is read.
    Hide,
    /// Serves half of the file's bytes.
    Truncate,
    /// Counts reads only.
    Count,
}

/// The in-memory store with a hook on each read.
struct Wrapped {
    inner: Arc<dyn FileStore>,
    pool: PgPool,
    mode: Mode,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait::async_trait]
impl FileStore for Wrapped {
    async fn put(
        &self,
        key: &crate::filestore::ObjectKey,
        body: ByteStream,
        meta: crate::filestore::PutMeta,
    ) -> Result<crate::filestore::StoredObject, FileStoreError> {
        self.inner.put(key, body, meta).await
    }
    async fn get(&self, key: &crate::filestore::ObjectKey) -> Result<ByteStream, FileStoreError> {
        use futures_util::StreamExt as _;
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.mode == Mode::Hide {
            sqlx::query(
                "UPDATE stored_files SET expires_at=now()-interval '1 second' WHERE object_key=$1",
            )
            .bind(key.to_string())
            .execute(&self.pool)
            .await
            .unwrap();
        }
        let stream = self.inner.get(key).await?;
        if self.mode != Mode::Truncate {
            return Ok(stream);
        }
        let mut whole = Vec::new();
        let mut stream = stream;
        while let Some(chunk) = stream.next().await {
            whole.extend_from_slice(&chunk?);
        }
        whole.truncate(whole.len() / 2);
        Ok(futures_util::stream::iter([Ok(bytes::Bytes::from(whole))]).boxed())
    }
    async fn delete(&self, key: &crate::filestore::ObjectKey) -> Result<(), FileStoreError> {
        self.inner.delete(key).await
    }
    async fn head(
        &self,
        key: &crate::filestore::ObjectKey,
    ) -> Result<Option<crate::filestore::ObjectInfo>, FileStoreError> {
        self.inner.head(key).await
    }
    async fn health(&self) -> Result<crate::filestore::StoreHealth, FileStoreError> {
        self.inner.health().await
    }
    async fn rekey(
        &self,
        key: &crate::filestore::ObjectKey,
        to: &str,
    ) -> Result<crate::filestore::Rekeyed, FileStoreError> {
        self.inner.rekey(key, to).await
    }
}

/// A files world whose store is [`Wrapped`] in `mode`, and its read count.
async fn wrapped_world(
    pool: PgPool,
    mode: Mode,
) -> (World, Arc<std::sync::atomic::AtomicUsize>, FileStoreRuntime) {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rt = FileStoreRuntime::from_store(
        Arc::new(Wrapped {
            inner: crate::filestore::memory_store(),
            pool: pool.clone(),
            mode,
            reads: reads.clone(),
        }),
        crate::filestore::BackendKind::Memory,
        vec!["memory".into()],
    );
    (
        files_world_on(pool, ImageInputLimits::default(), rt.clone()).await,
        reads,
        rt,
    )
}

#[dbtest::test]
async fn a_file_hidden_while_it_is_read_is_not_found(pool: PgPool) {
    let (w, _, _) = wrapped_world(pool, Mode::Hide).await;
    let me = bearer(&w);
    let id = upload(&w, &me, "vision", &png(256, "hidden")).await;
    let (status, _, value) = chat(&w, &me, &chat_file(&id)).await;
    assert_eq!(
        (status, value["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("file_not_found")),
        "{value}"
    );
    assert!(w.upstream.bodies.lock().unwrap().is_empty());
}

#[dbtest::test]
async fn a_request_refused_after_its_files_were_read_holds_nothing(pool: PgPool) {
    let w = files_world(pool, ImageInputLimits::default()).await;
    let me = bearer(&w);
    let id = upload(&w, &me, "vision", &png(MIB, "held")).await;
    let before = in_use();
    let mut body = chat_file(&id);
    body["model"] = json!("company/unknown");
    let (status, _, value) = chat(&w, &me, &body).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{value}");
    assert_eq!(in_use(), before, "the buffer share is back");
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!((held, executions(&w).await), (0, 0));
    // Served, the share is back once the response has ended.
    let (status, _, _) = chat(&w, &me, &chat_file(&id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(in_use(), before);
}

#[dbtest::test]
async fn reads_are_bounded_per_key_and_by_the_buffer_before_anything_is_read(pool: PgPool) {
    let limits = ImageInputLimits {
        body_bytes: 4 * MIB,
        buffer_bytes: 4 * MIB,
        file_reads_per_key_per_minute: 3,
        ..ImageInputLimits::default()
    };
    let w = files_world(pool, limits).await;
    let me = bearer(&w);
    // Its stored bytes and their base64 (about 4.7 MiB) can never fit the
    // key's 4 MiB share: refused for good, before any read, and not charged.
    let big = upload(&w, &me, "vision", &png(2 * MIB, "big")).await;
    let (status, retry, value) = chat(&w, &me, &chat_file(&big)).await;
    assert_eq!(
        (status, retry, value["error"]["code"].as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, None, Some("file_too_large")),
        "{value}"
    );
    let small = upload(&w, &me, "vision", &png(256, "small")).await;
    let refs = |n: usize| {
        let parts: Vec<Value> = (0..n)
            .map(|_| json!({"type":"file","file":{"file_id":small}}))
            .collect();
        json!({"model":"company/smart","messages":[{"role":"user","content":parts}]})
    };
    // Three reads fit the minute's limit; one more does not.
    let (status, _, value) = chat(&w, &me, &refs(3)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let (status, retry, value) = chat(&w, &me, &refs(1)).await;
    assert_eq!(
        (status, value["error"]["code"].as_str()),
        (
            StatusCode::TOO_MANY_REQUESTS,
            Some("file_reads_rate_limited")
        ),
        "{value}"
    );
    let seconds: u32 = retry.unwrap().parse().unwrap();
    assert!((1..=60).contains(&seconds));
    assert_eq!(w.upstream.bodies.lock().unwrap().len(), 1);
}

#[dbtest::test]
async fn a_referenced_image_reaches_no_log_table_metric_or_answer(pool: PgPool) {
    let capture = crate::log_capture_test::Capture::start();
    let w = files_world(pool, ImageInputLimits::default()).await;
    let me = bearer(&w);
    let canary = "QwFileCanaryQ";
    let bytes = png(4096, canary);
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let id = upload(&w, &me, "vision", &bytes).await;
    let text = upload(&w, &me, "user_data", format!("{canary} as text").as_bytes()).await;
    let mut answers = String::new();
    for body in [chat_file(&id), chat_file(&text)] {
        let (_, _, value) = chat(&w, &me, &body).await;
        answers.push_str(&value.to_string());
    }
    let mut stream = chat_file(&id);
    stream["stream"] = json!(true);
    let (status, _, _) = chat(&w, &me, &stream).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !answers.contains(&data[40..80]) && !answers.contains("FileCanary"),
        "{answers}"
    );
    assert!(
        w.upstream.bodies.lock().unwrap()[0]
            .to_string()
            .contains(&data)
    );
    assert_nowhere(&w, &data, &capture).await;
    assert!(!capture.text().contains("FileCanary"));
}

#[dbtest::test]
async fn a_short_read_is_refused_and_nothing_is_sent(pool: PgPool) {
    let (w, reads, _) = wrapped_world(pool, Mode::Truncate).await;
    let me = bearer(&w);
    let id = upload(&w, &me, "vision", &png(64 * 1024, "short")).await;
    let (status, _, value) = chat(&w, &me, &chat_file(&id)).await;
    assert_eq!(
        (status, value["error"]["code"].as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("file_store_unavailable")
        ),
        "{value}"
    );
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(w.upstream.bodies.lock().unwrap().is_empty());
    assert_eq!(executions(&w).await, 0);
}

#[dbtest::test]
async fn a_revoked_key_reads_no_file_even_while_its_cache_entry_lives(pool: PgPool) {
    let (w, reads, rt) = wrapped_world(pool, Mode::Count).await;
    let me = bearer(&w);
    let id = upload(&w, &me, "vision", &png(256, "revoked")).await;
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(w.f.principal.key_id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    // The principal a cached authentication would still hand the handler.
    let files = crate::filestore::FileStorage::new(w.f.store.clone(), rt);
    let buffer = crate::inference::image_buffer::ImageBuffer::new(ImageInputLimits::default());
    let reference = crate::protocols::file_input::Reference {
        id,
        field: "messages.content.file.file_id",
    };
    let refused = crate::protocols::file_input::resolve(
        &w.f.store,
        &files,
        &buffer,
        &w.f.principal,
        std::slice::from_ref(&reference),
    )
    .await
    .err();
    assert_eq!(
        refused,
        Some(crate::protocols::file_input::Refusal::Unauthenticated)
    );
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no file read"
    );
}
