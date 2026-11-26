//! Inline image input on Chat Completions and Responses through the real
//! router, engine and OpenAI adapter on PostgreSQL (#24): the deployment
//! opt-in before the route plan, the body budget decided from headers after
//! authentication and before any body byte is read, the permit held to the
//! end of the response body, and an image's bytes in no log line, table,
//! metric or error body.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, StatusCode, header},
    response::IntoResponse,
};
use base64::Engine as _;
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::{
    governance::tests::db::{Fixture, fixture},
    inference::{
        Engine, EngineLimits,
        admission_validation_tests::{
            Server, add_deployment, add_unpriced_deployment, issue_key, serve,
        },
        image_buffer::{ImageInputLimits, MIB},
    },
    providers::{
        ProviderRegistry,
        openai::OpenAiAdapter,
        secrets::{Secret, SecretResolver},
    },
};

struct Resolves;
impl SecretResolver for Resolves {
    fn resolve(&self, _: &str) -> Result<Secret, crate::inference::error::InferenceError> {
        Secret::new("test-provider-key".into())
    }
}

/// A PNG of `decoded` bytes (signature, then filler that spells `marker`),
/// as a data URL.
fn png_url(decoded: usize, marker: &str) -> String {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    while bytes.len() < decoded {
        bytes.extend_from_slice(marker.as_bytes());
    }
    bytes.truncate(decoded.max(8));
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn chat_body(url: &str, stream: bool) -> Value {
    json!({"model":"company/smart","max_tokens":16,"stream":stream,"messages":[{"role":"user","content":[
        {"type":"text","text":"what is this?"},
        {"type":"image_url","image_url":{"url":url}}]}]})
}

/// The bodies the upstream received, and a gate that holds a streamed reply
/// open until released.
#[derive(Default)]
struct Upstream {
    bodies: Mutex<Vec<Value>>,
    release: tokio::sync::Notify,
    hold: std::sync::atomic::AtomicBool,
}

fn upstream(state: Arc<Upstream>) -> Router {
    Router::new().fallback(move |uri: axum::http::Uri, body: Bytes| {
        let state = state.clone();
        async move {
            let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let stream = value["stream"] == true;
            state.bodies.lock().unwrap().push(value);
            if uri.path().ends_with("/responses") {
                return axum::Json(json!({"id":"r","object":"response","model":"private","status":"completed","output":[{"id":"m","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"a cat","annotations":[]}]}],"usage":{"input_tokens":5,"output_tokens":2}})).into_response();
            }
            if !stream {
                return axum::Json(json!({"id":"c","object":"chat.completion","created":1,"model":"private","choices":[{"index":0,"message":{"role":"assistant","content":"a cat"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}})).into_response();
            }
            let hold = state.hold.load(Ordering::SeqCst);
            let events = async_stream::stream! {
                yield Ok::<_, std::convert::Infallible>(Bytes::from(format!("data: {}\n\n", json!({"id":"c","object":"chat.completion.chunk","created":1,"model":"private","choices":[{"index":0,"delta":{"role":"assistant","content":"a cat"},"finish_reason":null}]}))));
                if hold { state.release.notified().await; }
                yield Ok(Bytes::from(format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"id":"c","object":"chat.completion.chunk","created":1,"model":"private","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                    json!({"id":"c","object":"chat.completion.chunk","created":1,"model":"private","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}))));
            };
            ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(events)).into_response()
        }
    })
    .layer(axum::extract::DefaultBodyLimit::disable())
}

struct World {
    f: Fixture,
    token: String,
    router: Router,
    deployment: Uuid,
    upstream: Arc<Upstream>,
    _servers: Vec<Server>,
}

async fn world(pool: PgPool, limits: ImageInputLimits, priced: bool) -> World {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','responses']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let state = Arc::new(Upstream::default());
    let (mock_base, mock) = serve(upstream(state.clone())).await;
    let deployment = if priced {
        add_deployment(&f, "openai", "env:OMG_IMAGE_TEST_KEY", None, "private", 0).await
    } else {
        add_unpriced_deployment(&f, "openai", "env:OMG_IMAGE_TEST_KEY", None, "private", 0).await
    };
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolves),
            format!("{mock_base}/v1"),
        )))
        .unwrap();
    let engine = Engine::new(
        Arc::new(f.store.clone()),
        registry,
        EngineLimits {
            image_input: limits,
            ..EngineLimits::default()
        },
    )
    .unwrap();
    let router = crate::http::router_with_engine(f.store.clone(), None, engine);
    World {
        f,
        token,
        router,
        deployment,
        upstream: state,
        _servers: vec![mock],
    }
}

async fn allow_images(w: &World, allowed: bool) {
    sqlx::query("UPDATE deployments SET image_input=$2 WHERE id=$1")
        .bind(w.deployment)
        .bind(allowed)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}

/// A body stream that counts the bytes the server pulls from it.
fn counted(bytes: Vec<u8>, pulled: Arc<AtomicUsize>) -> Body {
    let chunks: Vec<Bytes> = bytes
        .chunks(64 * 1024)
        .map(Bytes::copy_from_slice)
        .collect();
    Body::from_stream(futures_util::stream::iter(chunks.into_iter().map(
        move |chunk| {
            pulled.fetch_add(chunk.len(), Ordering::SeqCst);
            Ok::<_, std::convert::Infallible>(chunk)
        },
    )))
}

async fn send(
    w: &World,
    path: &str,
    token: Option<&str>,
    body: Vec<u8>,
    declared: bool,
) -> (StatusCode, Option<String>, Value, usize) {
    let pulled = Arc::new(AtomicUsize::new(0));
    let mut request = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if declared {
        request = request.header(header::CONTENT_LENGTH, body.len());
    }
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = w
        .router
        .clone()
        .oneshot(request.body(counted(body, pulled.clone())).unwrap())
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
        pulled.load(Ordering::SeqCst),
    )
}

async fn executions(w: &World) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM inference_executions")
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap()
}

#[dbtest::test]
async fn an_image_goes_only_to_a_route_that_allows_it(pool: PgPool) {
    let w = world(pool, ImageInputLimits::default(), true).await;
    let small = png_url(64, "x");
    let body = serde_json::to_vec(&chat_body(&small, false)).unwrap();
    let (status, _, value, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        body.clone(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{value}");
    assert_eq!(value["error"]["code"], "unsupported_capability");
    assert_eq!(executions(&w).await, 0, "refused before admission");
    assert!(w.upstream.bodies.lock().unwrap().is_empty());
    allow_images(&w, true).await;
    let (status, _, value, _) = send(&w, "/v1/chat/completions", Some(&w.token), body, true).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let sent = w.upstream.bodies.lock().unwrap()[0].clone();
    assert_eq!(
        sent["messages"][0]["content"][1],
        json!({"type":"image_url","image_url":{"url":small}})
    );
    // Responses too, on the same route.
    let responses = json!({"model":"company/smart","max_output_tokens":16,"input":[{"role":"user","content":[
        {"type":"input_text","text":"what is this?"},{"type":"input_image","image_url":small}]}]});
    let (status, _, value, _) = send(
        &w,
        "/v1/responses",
        Some(&w.token),
        serde_json::to_vec(&responses).unwrap(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
}

#[dbtest::test]
async fn authentication_and_the_budget_run_before_any_body_byte_is_read(pool: PgPool) {
    let w = world(
        pool,
        ImageInputLimits {
            body_bytes: 8 * MIB,
            buffer_bytes: 4 * MIB,
            ..ImageInputLimits::default()
        },
        true,
    )
    .await;
    allow_images(&w, true).await;
    let three_mib =
        serde_json::to_vec(&chat_body(&png_url(2 * MIB + MIB / 4, "y"), false)).unwrap();
    assert!(three_mib.len() > 2 * MIB && three_mib.len() <= 4 * MIB);
    // No key, or a wrong one: refused without reading the body.
    for token in [None, Some("omg_not_a_key")] {
        let (status, _, _, pulled) =
            send(&w, "/v1/chat/completions", token, three_mib.clone(), true).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(pulled, 0, "nothing read before authentication");
    }
    // Over the route cap, or over the whole budget: refused outright.
    let nine_mib = vec![b' '; 9 * MIB];
    let (status, _, value, pulled) =
        send(&w, "/v1/chat/completions", Some(&w.token), nine_mib, true).await;
    assert_eq!(
        (status, pulled),
        (StatusCode::PAYLOAD_TOO_LARGE, 0),
        "{value}"
    );
    let five_mib = vec![b' '; 5 * MIB];
    let (status, _, _, pulled) = send(&w, "/v1/responses", Some(&w.token), five_mib, true).await;
    assert_eq!((status, pulled), (StatusCode::PAYLOAD_TOO_LARGE, 0));
    // An undeclared length is held to the text limit as it streams.
    let (status, _, _, pulled) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        three_mib.clone(),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        pulled <= 2 * MIB + 64 * 1024,
        "stopped at the text limit: {pulled}"
    );
    // A declared large body with an image is admitted and served.
    let (status, _, value, pulled) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        three_mib.clone(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(pulled, three_mib.len());
    // One small image cannot carry text past the text limit.
    let riding = json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":[
        {"type":"text","text":"x".repeat(3 * MIB)},{"type":"image_url","image_url":{"url":png_url(64, "r")}}]}]});
    let (status, _, value, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        serde_json::to_vec(&riding).unwrap(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{value}");
    // A large text-only body is still refused (after parsing).
    let text = json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"x".repeat(3 * MIB)}]});
    let (status, _, value, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        serde_json::to_vec(&text).unwrap(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{value}");
    assert_eq!(executions(&w).await, 1, "only the admitted image request");
}

#[dbtest::test]
async fn a_large_body_keeps_its_share_until_its_streamed_response_ends(pool: PgPool) {
    let w = world(
        pool,
        ImageInputLimits {
            body_bytes: 8 * MIB,
            buffer_bytes: 4 * MIB,
            ..ImageInputLimits::default()
        },
        true,
    )
    .await;
    allow_images(&w, true).await;
    w.upstream.hold.store(true, Ordering::SeqCst);
    let url = png_url(2 * MIB + MIB / 4, "z");
    let streamed = serde_json::to_vec(&chat_body(&url, true)).unwrap();
    let response = w
        .router
        .clone()
        .oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, streamed.len())
                .header(header::AUTHORIZATION, format!("Bearer {}", w.token))
                .body(Body::from(streamed))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // While its stream is open, a second large body finds no budget.
    let second = serde_json::to_vec(&chat_body(&url, false)).unwrap();
    let (status, retry, value, pulled) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        second.clone(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{value}");
    assert_eq!(retry.as_deref(), Some("1"));
    assert_eq!(value["error"]["code"], "image_input_busy");
    assert_eq!(pulled, 0, "nothing read");
    // A text request is not affected.
    let text = json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"hi"}]});
    let (status, _, _, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        serde_json::to_vec(&text).unwrap(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The stream ends: its share is back.
    w.upstream.release.notify_one();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("[DONE]"));
    let (status, _, value, _) =
        send(&w, "/v1/chat/completions", Some(&w.token), second, true).await;
    assert_eq!(status, StatusCode::OK, "{value}");
}

#[dbtest::test]
async fn an_unpriced_image_route_is_refused_for_a_budgeted_key_as_text_is(pool: PgPool) {
    let w = world(pool, ImageInputLimits::default(), false).await;
    allow_images(&w, true).await;
    w.f.policy(
        "workspace_local_policies",
        None,
        None,
        None,
        Some(1_000_000),
    )
    .await;
    let text = json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"hi"}]});
    let (text_status, _, text_error, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        serde_json::to_vec(&text).unwrap(),
        true,
    )
    .await;
    let image = serde_json::to_vec(&chat_body(&png_url(64, "u"), false)).unwrap();
    let (status, _, error, _) = send(&w, "/v1/chat/completions", Some(&w.token), image, true).await;
    assert_ne!(text_status, StatusCode::OK, "{text_error}");
    assert_eq!(
        (status, &error["error"]["code"]),
        (text_status, &text_error["error"]["code"])
    );
    assert!(w.upstream.bodies.lock().unwrap().is_empty());
}

#[dbtest::test]
async fn an_image_reaches_no_log_table_metric_or_error_body(pool: PgPool) {
    let capture = crate::log_capture_test::Capture::start();
    let w = world(pool, ImageInputLimits::default(), true).await;
    allow_images(&w, true).await;
    let canary = "QwImageCanaryQ";
    let url = png_url(4096, canary);
    let data = url.split_once(',').unwrap().1.to_owned();
    // Served (stream and not), refused for a mismatched type, a remote URL
    // and a route without the opt-in: the bytes appear in no answer.
    let mut answers = String::new();
    for stream in [false, true] {
        let (status, _, value, _) = send(
            &w,
            "/v1/chat/completions",
            Some(&w.token),
            serde_json::to_vec(&chat_body(&url, stream)).unwrap(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        answers.push_str(&value.to_string());
    }
    let jpeg = url.replace("image/png", "image/jpeg");
    for body in [
        chat_body(&jpeg, false),
        chat_body(&format!("https://example.invalid/{data}"), false),
    ] {
        let (status, _, value, _) = send(
            &w,
            "/v1/chat/completions",
            Some(&w.token),
            serde_json::to_vec(&body).unwrap(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        answers.push_str(&value.to_string());
    }
    allow_images(&w, false).await;
    let (_, _, value, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        serde_json::to_vec(&chat_body(&url, false)).unwrap(),
        true,
    )
    .await;
    answers.push_str(&value.to_string());
    assert!(!answers.contains(&data) && !answers.contains("ImageCanary"));
    // The upstream did get it: the canary is in the image's base64.
    assert!(
        w.upstream.bodies.lock().unwrap()[0]
            .to_string()
            .contains(&data)
    );
    // Every table, partitions included.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT format('%I.%I',n.nspname,c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p') ORDER BY 1",
    )
    .fetch_all(&w.f.store.pool)
    .await
    .unwrap();
    assert!(tables.len() > 50);
    let needle = format!("%{}%", &data[40..80]);
    for table in tables {
        let hits: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} t WHERE t::text LIKE $1"
        ))
        .bind(&needle)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
        assert_eq!(hits, 0, "{table}");
    }
    let metrics = crate::metrics::METRICS.render(None).await;
    assert!(!metrics.contains(&data[40..80]));
    let logs = capture.text();
    assert!(!logs.is_empty());
    assert!(!logs.contains(&data[40..80]) && !logs.contains("ImageCanary"));
}

#[dbtest::test]
async fn a_stalled_large_body_times_out_and_returns_its_share(pool: PgPool) {
    let w = world(
        pool,
        ImageInputLimits {
            body_bytes: 8 * MIB,
            buffer_bytes: 4 * MIB,
            body_idle: std::time::Duration::from_millis(300),
            body_total: std::time::Duration::from_secs(5),
        },
        true,
    )
    .await;
    allow_images(&w, true).await;
    let body = serde_json::to_vec(&chat_body(&png_url(2 * MIB + MIB / 4, "s"), false)).unwrap();
    // The first half arrives, then nothing more.
    let (first, _) = body.split_at(body.len() / 2);
    let first = Bytes::copy_from_slice(first);
    let stalled = Body::from_stream(
        futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(first) })
            .chain(futures_util::stream::pending()),
    );
    let started = std::time::Instant::now();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        w.router.clone().oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, body.len())
                .header(header::AUTHORIZATION, format!("Bearer {}", w.token))
                .body(stalled)
                .unwrap(),
        ),
    )
    .await
    .expect("the idle deadline ends the request")
    .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    drop(response);
    assert_eq!(executions(&w).await, 0);
    // Its share is back: a full large body is admitted at once.
    let (status, _, value, _) = send(&w, "/v1/chat/completions", Some(&w.token), body, true).await;
    assert_eq!(status, StatusCode::OK, "{value}");
}
