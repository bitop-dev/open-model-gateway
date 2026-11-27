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
    /// The path of each body, in the same order.
    paths: Mutex<Vec<String>>,
    /// Paths answered 429 (`Busy`: failover needs no ambiguity opt-in).
    failing: Mutex<Vec<&'static str>>,
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
            state.paths.lock().unwrap().push(uri.path().to_owned());
            if state.failing.lock().unwrap().iter().any(|p| uri.path().ends_with(p)) {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("x-amzn-errortype", "ThrottlingException")],
                    axum::Json(json!({"error":{"type":"rate_limit_error","message":"slow down"},"message":"slow down"})),
                )
                    .into_response();
            }
            if uri.path().ends_with("/count_tokens") {
                return axum::Json(json!({"input_tokens":42})).into_response();
            }
            if uri.path().ends_with("/messages") {
                return axum::Json(json!({"id":"msg_1","type":"message","role":"assistant","model":"private","content":[{"type":"text","text":"a cat"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":2}})).into_response();
            }
            if uri.path().ends_with("/converse") {
                return axum::Json(json!({"output":{"message":{"role":"assistant","content":[{"text":"a cat"}]}},"stopReason":"end_turn","usage":{"inputTokens":5,"outputTokens":2,"totalTokens":7},"metrics":{"latencyMs":2}})).into_response();
            }
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
    _counters: tokio::sync::MutexGuard<'static, ()>,
}

async fn world(pool: PgPool, limits: ImageInputLimits, priced: bool) -> World {
    world_for(pool, limits, priced, "openai").await
}

/// A world whose first route is on `provider` (`openai`, `anthropic` or
/// `bedrock`); every adapter is registered against the same mock upstream,
/// which answers each wire by its path.
async fn world_for(pool: PgPool, limits: ImageInputLimits, priced: bool, provider: &str) -> World {
    world_at(pool, limits, priced, provider, None).await
}

/// [`world_for`] with the adapters sent to `upstream` instead of the
/// in-process mock.
async fn world_at(
    pool: PgPool,
    limits: ImageInputLimits,
    priced: bool,
    provider: &str,
    base: Option<String>,
) -> World {
    let counters = crate::inference::image_buffer::ROUTER_TESTS.lock().await;
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','responses','messages']",
    )
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
    let deployment = route(&f, provider, 0, priced).await;
    let mock_base = base.unwrap_or(mock_base);
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolves),
            format!("{mock_base}/v1"),
        )))
        .unwrap();
    registry
        .register(Arc::new(
            crate::providers::anthropic::AnthropicAdapter::for_test(
                Arc::new(Resolves),
                format!("{mock_base}/v1"),
            ),
        ))
        .unwrap();
    registry
        .register(Arc::new(
            crate::providers::bedrock::BedrockAdapter::for_test(&mock_base),
        ))
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
        _counters: counters,
    }
}

/// A route of the fixture's model on `provider`, in routing `order`.
async fn route(f: &Fixture, provider: &str, order: i64, priced: bool) -> Uuid {
    let (credential, region) = match provider {
        "bedrock" => ("aws:default", Some("us-east-1")),
        _ => ("env:OMG_IMAGE_TEST_KEY", None),
    };
    if priced {
        add_deployment(f, provider, credential, region, "private", order).await
    } else {
        add_unpriced_deployment(f, provider, credential, region, "private", order).await
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
    let mut request = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01");
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
    // A large text-only body is still refused, before any parse.
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
    assert_nowhere(&w, &data, &capture).await;
}

/// `data` (an image's base64) is in no table (partitions included), metric
/// or captured log line.
async fn assert_nowhere(w: &World, data: &str, capture: &crate::log_capture_test::Capture) {
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
            body_min_rate: crate::inference::image_buffer::BODY_MIN_RATE,
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

#[dbtest::test]
async fn a_trickled_large_body_is_cut_by_the_rate_floor(pool: PgPool) {
    let w = world(
        pool,
        ImageInputLimits {
            body_bytes: 8 * MIB,
            buffer_bytes: 4 * MIB,
            body_idle: std::time::Duration::from_millis(300),
            body_total: std::time::Duration::from_secs(30),
            body_min_rate: 64 * 1024,
        },
        true,
    )
    .await;
    allow_images(&w, true).await;
    let body = serde_json::to_vec(&chat_body(&png_url(2 * MIB + MIB / 4, "t"), false)).unwrap();
    let declared = body.len();
    // Ten bytes every 50 ms: never idle, far below the floor.
    let trickle = Body::from_stream(
        futures_util::stream::iter(
            body.chunks(10)
                .map(Bytes::copy_from_slice)
                .collect::<Vec<_>>(),
        )
        .then(|chunk| async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok::<_, std::convert::Infallible>(chunk)
        }),
    );
    let started = std::time::Instant::now();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        w.router.clone().oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, declared)
                .header(header::AUTHORIZATION, format!("Bearer {}", w.token))
                .body(trickle)
                .unwrap(),
        ),
    )
    .await
    .expect("the rate floor ends the request")
    .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    // The 128 KiB allowance lasts 2 s at this floor of 64 KiB/s.
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(executions(&w).await, 0);
}

/// Bytes of the process's image input buffers in use. Exact while the
/// World's lock keeps the other tests that send large bodies out.
fn in_use() -> i64 {
    crate::metrics::METRICS.image_input_buffer().0.get()
}

/// `item` repeated, comma-separated, to about `bytes`.
fn repeated(item: &str, bytes: usize) -> String {
    vec![item; bytes / (item.len() + 1)].join(",")
}

#[dbtest::test]
async fn structure_past_the_text_limit_is_refused_before_any_parse(pool: PgPool) {
    use crate::metrics::METRICS;
    let w = world(
        pool,
        ImageInputLimits {
            body_bytes: 8 * MIB,
            buffer_bytes: 8 * MIB,
            ..ImageInputLimits::default()
        },
        true,
    )
    .await;
    allow_images(&w, true).await;
    let size = 3 * MIB;
    let chat = |tail: String| {
        format!(
            r#"{{"model":"company/smart","messages":[{{"role":"user","content":"hi"}}],{tail}}}"#
        )
    };
    // Bodies a parse would turn into trees many times their size.
    let hostile = [
        (
            "/v1/responses",
            format!(
                r#"{{"model":"company/smart","input":[{}]}}"#,
                repeated(r#"{"a":0}"#, size)
            ),
        ),
        (
            "/v1/chat/completions",
            chat(format!(r#""tool_choice":[{}]"#, repeated("0", size))),
        ),
        (
            "/v1/chat/completions",
            format!(
                r#"{{"model":"company/smart","messages":[{}]}}"#,
                repeated(r#"{"role":"user"}"#, size)
            ),
        ),
        // Image-looking strings outside any image: at most twenty are set
        // aside, so thousands of small ones do not lift the bound.
        (
            "/v1/chat/completions",
            chat(format!(
                r#""tool_choice":[{}]"#,
                repeated(r#""data:image/png;base64,AAAA""#, size)
            )),
        ),
        // An image prefix over bytes outside base64, in a field of another
        // type: a parser's error would echo it escaped at six times its size,
        // so it is not set aside.
        (
            "/v1/chat/completions",
            chat(format!(
                r#""temperature":"data:image/png;base64,{}""#,
                "\u{7f}".repeat(size)
            )),
        ),
    ];
    for (path, body) in hostile {
        // With a trailing byte the body is not JSON: a parse would answer
        // 400, so a 413 shows the bound refused it before any parser ran.
        for tail in ["", "x"] {
            let body = format!("{body}{tail}").into_bytes();
            assert!(body.len() > size - 64 && body.len() <= 4 * MIB);
            let before = METRICS.image_input_refusal_count("too_large");
            let (status, _, value, pulled) =
                send(&w, path, Some(&w.token), body.clone(), true).await;
            assert_eq!(
                status,
                StatusCode::PAYLOAD_TOO_LARGE,
                "{path}{tail}: {value}"
            );
            assert_eq!(value["error"]["code"], "payload_too_large");
            assert_eq!(pulled, body.len());
            assert_eq!(METRICS.image_input_refusal_count("too_large"), before + 1);
            assert_eq!(in_use(), 0, "the share is back after the 413");
        }
    }
    assert_eq!(executions(&w).await, 0);
    assert!(w.upstream.bodies.lock().unwrap().is_empty());
    // A large body that is not JSON is refused as the handler would refuse
    // it, before a byte is read.
    let image = serde_json::to_vec(&chat_body(&png_url(2 * MIB + MIB / 4, "p"), false)).unwrap();
    let request = Request::post("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::CONTENT_LENGTH, image.len())
        .header(header::AUTHORIZATION, format!("Bearer {}", w.token));
    let pulled = Arc::new(AtomicUsize::new(0));
    let response = w
        .router
        .clone()
        .oneshot(request.body(counted(image, pulled.clone())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(pulled.load(Ordering::SeqCst), 0);
    drop(response);
    assert_eq!(in_use(), 0, "the share is back after the 400");
    // A body that fails mid-read (the client went away): 400, share back.
    let body = serde_json::to_vec(&chat_body(&png_url(2 * MIB + MIB / 4, "q"), false)).unwrap();
    let first = Bytes::copy_from_slice(&body[..body.len() / 2]);
    let broken = Body::from_stream(futures_util::stream::iter([
        Ok::<_, std::io::Error>(first),
        Err(std::io::Error::other("connection reset")),
    ]));
    let response = w
        .router
        .clone()
        .oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, body.len())
                .header(header::AUTHORIZATION, format!("Bearer {}", w.token))
                .body(broken)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    drop(response);
    assert_eq!(in_use(), 0, "the share is back after a failed read");
    // A large image request passes, also with every `/` escaped as JSON
    // allows (`\/`, as some encoders write it).
    let image = serde_json::to_vec(&chat_body(&png_url(4 * MIB, "l"), false)).unwrap();
    assert!(image.len() > 5 * MIB);
    let escaped = String::from_utf8(image.clone())
        .unwrap()
        .replace('/', "\\/")
        .into_bytes();
    for body in [image, escaped] {
        let (status, _, value, _) =
            send(&w, "/v1/chat/completions", Some(&w.token), body, true).await;
        assert_eq!(status, StatusCode::OK, "{value}");
    }
    assert_eq!(executions(&w).await, 2);
}

#[dbtest::test]
async fn every_large_body_refusal_is_counted_by_reason(pool: PgPool) {
    use crate::metrics::METRICS;
    let w = world(
        pool,
        ImageInputLimits {
            body_bytes: 8 * MIB,
            buffer_bytes: 4 * MIB,
            body_idle: std::time::Duration::from_secs(2),
            body_total: std::time::Duration::from_secs(10),
            body_min_rate: crate::inference::image_buffer::BODY_MIN_RATE,
        },
        true,
    )
    .await;
    allow_images(&w, true).await;
    let count = |reason| METRICS.image_input_refusal_count(reason);
    // too_large: over the route cap, from the header alone.
    let before = count("too_large");
    let (status, _, _, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        vec![b' '; 9 * MIB],
        true,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(count("too_large"), before + 1);
    // A stalled body holds 3 of the 4 MiB until its idle bound.
    let body = serde_json::to_vec(&chat_body(&png_url(2 * MIB + MIB / 4, "c"), false)).unwrap();
    let first = Bytes::copy_from_slice(&body[..body.len() / 2]);
    let stalled = Body::from_stream(
        futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(first) })
            .chain(futures_util::stream::pending()),
    );
    let (timeouts, busy) = (count("timeout"), count("busy"));
    let held = tokio::spawn(
        w.router.clone().oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, body.len())
                .header(header::AUTHORIZATION, format!("Bearer {}", w.token))
                .body(stalled)
                .unwrap(),
        ),
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // busy: the rest of the buffer is too small for a second one.
    let (status, retry, _, pulled) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        body.clone(),
        true,
    )
    .await;
    assert_eq!(
        (status, retry.as_deref(), pulled),
        (StatusCode::SERVICE_UNAVAILABLE, Some("1"), 0)
    );
    assert_eq!(count("busy"), busy + 1);
    // timeout: the stalled body reaches its idle bound.
    let response = tokio::time::timeout(std::time::Duration::from_secs(8), held)
        .await
        .expect("the idle bound ends it")
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(count("timeout"), timeouts + 1);
    drop(response);
    assert_eq!(in_use(), 0, "the share is back after the 408");
}

#[dbtest::test]
async fn images_reach_anthropic_routes_as_base64_image_blocks(pool: PgPool) {
    images_reach_their_routes(pool, "anthropic").await;
}

#[dbtest::test]
async fn images_reach_bedrock_routes_as_converse_image_blocks(pool: PgPool) {
    images_reach_their_routes(pool, "bedrock").await;
}

async fn images_reach_their_routes(pool: PgPool, provider: &str) {
    {
        let w = world_for(pool, ImageInputLimits::default(), true, provider).await;
        let url = png_url(64, "z");
        let data = url.split_once(',').unwrap().1.to_owned();
        let body = serde_json::to_vec(&chat_body(&url, false)).unwrap();
        // Without the deployment's opt-in the route is skipped.
        let (status, _, value, _) = send(
            &w,
            "/v1/chat/completions",
            Some(&w.token),
            body.clone(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{provider}: {value}");
        allow_images(&w, true).await;
        let (status, _, value, _) =
            send(&w, "/v1/chat/completions", Some(&w.token), body, true).await;
        assert_eq!(status, StatusCode::OK, "{provider}: {value}");
        let sent = w.upstream.bodies.lock().unwrap().last().unwrap().clone();
        let image = match provider {
            "anthropic" => {
                json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":data}})
            }
            _ => json!({"image":{"format":"png","source":{"bytes":data}}}),
        };
        assert_eq!(sent["messages"][0]["content"][1], image, "{provider}");
        // These profiles serve no Responses, with or without images.
        for input in [
            json!("hi"),
            json!([{"role":"user","content":[{"type":"input_image","image_url":url}]}]),
        ] {
            let responses = json!({"model":"company/smart","max_output_tokens":16,"input":input});
            let (status, _, _, _) = send(
                &w,
                "/v1/responses",
                Some(&w.token),
                serde_json::to_vec(&responses).unwrap(),
                true,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{provider}");
        }
        // `detail` other than `auto` has no place on these wires: refused.
        let mut low = chat_body(&url, false);
        low["messages"][0]["content"][1]["image_url"]["detail"] = json!("low");
        let (status, _, value, _) = send(
            &w,
            "/v1/chat/completions",
            Some(&w.token),
            serde_json::to_vec(&low).unwrap(),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{provider}: {value}");
        assert_eq!(executions(&w).await, 1, "{provider}");
    }
}

#[dbtest::test]
async fn an_image_reaches_no_log_table_metric_or_error_body_on_anthropic(pool: PgPool) {
    canary_on(pool, "anthropic").await;
}

#[dbtest::test]
async fn an_image_reaches_no_log_table_metric_or_error_body_on_bedrock(pool: PgPool) {
    canary_on(pool, "bedrock").await;
}

/// Served, refused before admission (a mismatched type, a `detail` the wire
/// cannot carry) and refused by the upstream: the bytes reach the upstream
/// body and nothing else.
async fn canary_on(pool: PgPool, provider: &'static str) {
    let capture = crate::log_capture_test::Capture::start();
    let w = world_for(pool, ImageInputLimits::default(), true, provider).await;
    allow_images(&w, true).await;
    let url = png_url(4096, "QwImageCanaryQ");
    let data = url.split_once(',').unwrap().1.to_owned();
    let mut low = chat_body(&url, false);
    low["messages"][0]["content"][1]["image_url"]["detail"] = json!("high");
    let mut answers = String::new();
    for (body, want) in [
        (chat_body(&url, false), StatusCode::OK),
        (
            chat_body(&url.replace("image/png", "image/jpeg"), false),
            StatusCode::BAD_REQUEST,
        ),
        (low, StatusCode::BAD_REQUEST),
    ] {
        let (status, _, value, _) = send(
            &w,
            "/v1/chat/completions",
            Some(&w.token),
            serde_json::to_vec(&body).unwrap(),
            true,
        )
        .await;
        assert_eq!(status, want, "{provider}: {value}");
        answers.push_str(&value.to_string());
    }
    w.upstream
        .failing
        .lock()
        .unwrap()
        .push(if provider == "anthropic" {
            "/messages"
        } else {
            "/converse"
        });
    let (status, _, value, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        serde_json::to_vec(&chat_body(&url, false)).unwrap(),
        true,
    )
    .await;
    assert!(!status.is_success(), "{provider}: {status} {value}");
    answers.push_str(&value.to_string());
    assert!(!answers.contains(&data[40..80]) && !answers.contains("ImageCanary"));
    assert!(
        w.upstream.bodies.lock().unwrap()[0]
            .to_string()
            .contains(&data[40..80])
    );
    assert_nowhere(&w, &data, &capture).await;
}

#[dbtest::test]
async fn an_image_fails_over_from_openai_to_anthropic_under_the_route_policy(pool: PgPool) {
    failover(pool, "openai", "anthropic").await;
}

#[dbtest::test]
async fn an_image_fails_over_from_bedrock_to_openai_under_the_route_policy(pool: PgPool) {
    failover(pool, "bedrock", "openai").await;
}

/// The first route answers 429 (`Busy`, before any content); with two
/// attempts allowed by the model's routing policy and both routes in one
/// residency, the image is served by the second, each attempt in its own
/// wire's form.
async fn failover(pool: PgPool, first: &'static str, second: &'static str) {
    let w = world_for(pool, ImageInputLimits::default(), true, first).await;
    let backup = route(&w.f, second, 1, true).await;
    sqlx::query("UPDATE deployments SET image_input=true WHERE id = ANY($1)")
        .bind(vec![w.deployment, backup])
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO routing_policies(model_id,max_attempts,allow_ambiguous_failover) VALUES($1,2,false)")
        .bind(w.f.model)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    // Failover stays within one declared residency.
    for (deployment, priority) in [(w.deployment, 0), (backup, 1)] {
        sqlx::query(
            "INSERT INTO deployment_routing(deployment_id,priority,residency) VALUES($1,$2,'us')",
        )
        .bind(deployment)
        .bind(priority)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    }
    let path = |provider: &str| match provider {
        "openai" => "/chat/completions",
        "anthropic" => "/messages",
        _ => "/converse",
    };
    w.upstream.failing.lock().unwrap().push(path(first));
    let body = serde_json::to_vec(&chat_body(&png_url(64, "f"), false)).unwrap();
    let (status, _, value, _) = send(&w, "/v1/chat/completions", Some(&w.token), body, true).await;
    let paths = w.upstream.paths.lock().unwrap().clone();
    assert_eq!(status, StatusCode::OK, "{value} {paths:?}");
    let bodies = w.upstream.bodies.lock().unwrap().clone();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(
        paths[0].ends_with(path(first)) && paths[1].ends_with(path(second)),
        "{paths:?}"
    );
    // Each attempt carried the image in its own wire's form.
    for (path, body) in paths.iter().zip(&bodies) {
        let content = &body["messages"][0]["content"][1];
        let carried = if path.ends_with("/chat/completions") {
            content["type"] == "image_url"
        } else if path.ends_with("/messages") {
            content["type"] == "image"
        } else {
            content.get("image").is_some()
        };
        assert!(carried, "{path}: {content}");
    }
    assert_eq!(executions(&w).await, 2);
}

/// The peak memory of large image requests on each adapter (docs/operations.md
/// "Image input memory"). Serves this router on a loopback port, against the
/// upstream the caller names, until a stop file appears: the client and the
/// upstream are other processes, so this process holds only the gateway.
#[dbtest::test]
#[ignore = "manual measurement: docs/operations.md \"Image input memory\""]
async fn image_memory_harness(pool: PgPool) {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    let dir = std::path::PathBuf::from(var("OMG_MEASURE_DIR"));
    let limits = ImageInputLimits {
        buffer_bytes: std::env::var("OMG_MEASURE_BUFFER_BYTES")
            .map_or(ImageInputLimits::default().buffer_bytes, |v| {
                v.parse().unwrap()
            }),
        ..ImageInputLimits::default()
    };
    let w = world_at(
        pool,
        limits,
        true,
        &var("OMG_MEASURE_PROVIDER"),
        Some(var("OMG_MEASURE_UPSTREAM")),
    )
    .await;
    allow_images(&w, true).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let ready = json!({"port": port, "pid": std::process::id(), "token": w.token});
    std::fs::write(dir.join("ready.json"), ready.to_string()).unwrap();
    let stop = dir.join("stop");
    axum::serve(
        listener,
        w.router
            .clone()
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        while !stop.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap();
}

/// A PNG data URL whose header states `width` × `height`.
fn png_sized(width: u32, height: u32) -> String {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.resize(256, 0);
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

#[dbtest::test]
async fn a_png_over_8000_px_a_side_is_refused_before_admission_on_anthropic(pool: PgPool) {
    over_the_pixel_limit(pool, "anthropic").await;
}

#[dbtest::test]
async fn a_png_over_8000_px_a_side_is_refused_before_admission_on_bedrock(pool: PgPool) {
    over_the_pixel_limit(pool, "bedrock").await;
}

/// The size the image's header states reaches the provider's limit: 8001 px
/// on one side is refused naming `image`, with nothing sent and no
/// execution; 8000 px is served.
async fn over_the_pixel_limit(pool: PgPool, provider: &str) {
    let w = world_for(pool, ImageInputLimits::default(), true, provider).await;
    allow_images(&w, true).await;
    for (width, height) in [(8001, 16), (16, 8001)] {
        let body = serde_json::to_vec(&chat_body(&png_sized(width, height), false)).unwrap();
        let (status, _, value, _) =
            send(&w, "/v1/chat/completions", Some(&w.token), body, true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{provider}: {value}");
        assert_eq!(value["error"]["message"], "image", "{provider}");
    }
    assert!(w.upstream.bodies.lock().unwrap().is_empty(), "{provider}");
    assert_eq!(executions(&w).await, 0, "{provider}");
    let body = serde_json::to_vec(&chat_body(&png_sized(8000, 8000), false)).unwrap();
    let (status, _, value, _) = send(&w, "/v1/chat/completions", Some(&w.token), body, true).await;
    assert_eq!(status, StatusCode::OK, "{provider}: {value}");
    assert_eq!(executions(&w).await, 1, "{provider}");
}

/// An image of about 4 MB decoded: within the gateway's 5 MiB, over
/// Bedrock's 3.75 MB. On a model whose only route is Bedrock it is refused
/// before admission; with an OpenAI route second, that route serves it in
/// one upstream request (Bedrock is skipped before the plan, not tried).
#[dbtest::test]
async fn an_image_over_bedrocks_size_is_refused_there_and_served_by_another_route(pool: PgPool) {
    let w = world_for(pool, ImageInputLimits::default(), true, "bedrock").await;
    allow_images(&w, true).await;
    let body = serde_json::to_vec(&chat_body(&png_url(4_000_000, "b"), false)).unwrap();
    assert!(body.len() > 2 * MIB);
    let (status, _, value, _) = send(
        &w,
        "/v1/chat/completions",
        Some(&w.token),
        body.clone(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"]["message"], "image");
    assert!(w.upstream.bodies.lock().unwrap().is_empty());
    assert_eq!(executions(&w).await, 0);
    let openai = route(&w.f, "openai", 1, true).await;
    sqlx::query("UPDATE deployments SET image_input=true WHERE id=$1")
        .bind(openai)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    let (status, _, value, _) = send(&w, "/v1/chat/completions", Some(&w.token), body, true).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let paths = w.upstream.paths.lock().unwrap().clone();
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert!(paths[0].ends_with("/chat/completions"), "{paths:?}");
    assert_eq!(executions(&w).await, 1);
}

/// A Messages request carrying `blocks` in its one user turn.
fn messages_body(blocks: Vec<Value>) -> Value {
    json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":blocks}]})
}

/// A Messages image block of the base64 in `url` (a data URL).
fn image_block(url: &str) -> Value {
    let (head, data) = url.split_once(',').unwrap();
    let media = head.trim_start_matches("data:").trim_end_matches(";base64");
    json!({"type":"image","source":{"type":"base64","media_type":media,"data":data}})
}

#[dbtest::test]
async fn a_messages_image_reaches_a_native_anthropic_route_without_the_opt_in(pool: PgPool) {
    let w = world_for(pool, ImageInputLimits::default(), true, "anthropic").await;
    let url = png_url(64, "m");
    let data = url.split_once(',').unwrap().1.to_owned();
    let body = messages_body(vec![
        json!({"type":"text","text":"what is this?"}),
        image_block(&url),
    ]);
    // Native Messages images never needed the deployment's opt-in.
    let (status, _, value, _) = send(
        &w,
        "/v1/messages",
        Some(&w.token),
        serde_json::to_vec(&body).unwrap(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let sent = w.upstream.bodies.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        sent["messages"][0]["content"][1],
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":data}})
    );
    // Anthropic's own count receives the image too.
    let (status, _, value, _) = send(
        &w,
        "/v1/messages/count_tokens",
        Some(&w.token),
        serde_json::to_vec(&json!({"model":"company/smart","messages":body["messages"]})).unwrap(),
        true,
    )
    .await;
    assert_eq!(
        (status, value["input_tokens"].as_u64()),
        (StatusCode::OK, Some(42)),
        "{value}"
    );
    let counted = w.upstream.bodies.lock().unwrap().last().unwrap().clone();
    assert_eq!(counted["messages"][0]["content"][1]["source"]["data"], data);
}

#[dbtest::test]
async fn a_messages_image_reaches_a_bedrock_route_as_converse_blocks(pool: PgPool) {
    translated_messages_images(pool, "bedrock").await;
}

#[dbtest::test]
async fn a_messages_image_reaches_an_openai_route_as_an_image_url_part(pool: PgPool) {
    translated_messages_images(pool, "openai").await;
}

/// On a translated route a Messages image needs the opt-in, reaches the
/// provider in its own wire, and an estimated count refuses it; a tool
/// result's image goes where a tool result takes one (Bedrock) and is
/// refused, naming it, where it cannot (OpenAI).
async fn translated_messages_images(pool: PgPool, provider: &str) {
    let w = world_for(pool, ImageInputLimits::default(), true, provider).await;
    let url = png_url(64, "t");
    let data = url.split_once(',').unwrap().1.to_owned();
    let body = messages_body(vec![
        json!({"type":"text","text":"what is this?"}),
        image_block(&url),
    ]);
    let bytes = serde_json::to_vec(&body).unwrap();
    let (status, _, value, _) = send(&w, "/v1/messages", Some(&w.token), bytes.clone(), true).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{provider}: {value}");
    assert_eq!(value["type"], "error", "Anthropic's error shape");
    allow_images(&w, true).await;
    let (status, _, value, _) = send(&w, "/v1/messages", Some(&w.token), bytes, true).await;
    assert_eq!(status, StatusCode::OK, "{provider}: {value}");
    let sent = w.upstream.bodies.lock().unwrap().last().unwrap().clone();
    let image = match provider {
        "bedrock" => json!({"image":{"format":"png","source":{"bytes":data}}}),
        _ => json!({"type":"image_url","image_url":{"url":url}}),
    };
    let content = sent["messages"].as_array().unwrap().last().unwrap()["content"].clone();
    assert!(
        content.as_array().unwrap().contains(&image),
        "{provider}: {content}"
    );
    // The gateway's estimate counts text only: it refuses the image.
    let (status, _, value, _) = send(
        &w,
        "/v1/messages/count_tokens",
        Some(&w.token),
        serde_json::to_vec(&json!({"model":"company/smart","messages":body["messages"]})).unwrap(),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{provider}: {value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("image"),
        "{value}"
    );
    // A tool result's image.
    let tool = json!({"model":"company/smart","max_tokens":16,"messages":[
        {"role":"user","content":"look"},
        {"role":"assistant","content":[{"type":"tool_use","id":"c","name":"shot","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":[{"type":"text","text":"here"}, image_block(&url)]}]}
    ],"tools":[{"name":"shot","input_schema":{"type":"object"}}]});
    let (status, _, value, _) = send(
        &w,
        "/v1/messages",
        Some(&w.token),
        serde_json::to_vec(&tool).unwrap(),
        true,
    )
    .await;
    if provider == "bedrock" {
        assert_eq!(status, StatusCode::OK, "{value}");
        let sent = w.upstream.bodies.lock().unwrap().last().unwrap().clone();
        let result = &sent["messages"][2]["content"][0]["toolResult"]["content"];
        assert_eq!(
            result[1],
            json!({"image":{"format":"png","source":{"bytes":data}}}),
            "{result}"
        );
    } else {
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("tool_result.content.image"),
            "{value}"
        );
    }
}

/// A Messages body above 2 MiB is admitted as Chat and Responses bodies are:
/// for its images. Base64 anywhere else is refused with 413 in Anthropic's
/// shape: in a text block before any parse, under a tool input's `data`
/// key (set aside by the scan as a Messages source would be) once the parse
/// shows it is no image; a document block is outside the grammar (400).
#[dbtest::test]
async fn a_large_messages_body_is_admitted_only_for_its_images(pool: PgPool) {
    use base64::Engine as _;
    let w = world_for(pool, ImageInputLimits::default(), true, "anthropic").await;
    let image = png_url(4_000_000, "l");
    let body = serde_json::to_vec(&messages_body(vec![
        json!({"type":"text","text":"what?"}),
        image_block(&image),
    ]))
    .unwrap();
    assert!(body.len() > 2 * MIB);
    let (status, _, value, _) = send(&w, "/v1/messages", Some(&w.token), body, true).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let base64 = base64::engine::general_purpose::STANDARD.encode(vec![7u8; 3 * MIB]);
    let refusals = [
        (
            messages_body(vec![json!({"type":"text","text":base64})]),
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
        ),
        (
            json!({"model":"company/smart","max_tokens":16,"messages":[
                {"role":"user","content":"go"},
                {"role":"assistant","content":[{"type":"tool_use","id":"c","name":"store","input":{"data":base64}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"ok"}]}
            ],"tools":[{"name":"store","input_schema":{"type":"object"}}]}),
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
        ),
        (
            messages_body(vec![
                json!({"type":"document","source":{"type":"base64","media_type":"application/pdf","data":base64}}),
            ]),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
    ];
    let executed = executions(&w).await;
    for (body, want, kind) in refusals {
        let (status, _, value, _) = send(
            &w,
            "/v1/messages",
            Some(&w.token),
            serde_json::to_vec(&body).unwrap(),
            true,
        )
        .await;
        assert_eq!(status, want, "{value}");
        assert_eq!(
            (value["type"].as_str(), value["error"]["type"].as_str()),
            (Some("error"), Some(kind)),
            "{value}"
        );
        assert_eq!(in_use(), 0, "the share is back");
    }
    assert_eq!(executions(&w).await, executed, "nothing admitted");
}

#[dbtest::test]
async fn a_messages_image_reaches_no_log_table_metric_or_error_body_natively(pool: PgPool) {
    messages_canary(pool, "anthropic").await;
}

#[dbtest::test]
async fn a_messages_image_reaches_no_log_table_metric_or_error_body_translated(pool: PgPool) {
    messages_canary(pool, "bedrock").await;
}

/// Served, refused before admission (bytes that are not the declared type)
/// and refused by the upstream: a Messages image's bytes reach the upstream
/// body and nothing else.
async fn messages_canary(pool: PgPool, provider: &'static str) {
    let capture = crate::log_capture_test::Capture::start();
    let w = world_for(pool, ImageInputLimits::default(), true, provider).await;
    allow_images(&w, true).await;
    let url = png_url(4096, "QwImageCanaryQ");
    let data = url.split_once(',').unwrap().1.to_owned();
    let mut answers = String::new();
    for (url, want) in [
        (url.clone(), StatusCode::OK),
        (
            url.replace("image/png", "image/jpeg"),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let body = messages_body(vec![
            json!({"type":"text","text":"what?"}),
            image_block(&url),
        ]);
        let (status, _, value, _) = send(
            &w,
            "/v1/messages",
            Some(&w.token),
            serde_json::to_vec(&body).unwrap(),
            true,
        )
        .await;
        assert_eq!(status, want, "{provider}: {value}");
        answers.push_str(&value.to_string());
    }
    w.upstream
        .failing
        .lock()
        .unwrap()
        .push(if provider == "anthropic" {
            "/messages"
        } else {
            "/converse"
        });
    let body = messages_body(vec![
        json!({"type":"text","text":"what?"}),
        image_block(&url),
    ]);
    let (status, _, value, _) = send(
        &w,
        "/v1/messages",
        Some(&w.token),
        serde_json::to_vec(&body).unwrap(),
        true,
    )
    .await;
    assert!(!status.is_success(), "{provider}: {status} {value}");
    answers.push_str(&value.to_string());
    assert!(!answers.contains(&data[40..80]) && !answers.contains("ImageCanary"));
    assert!(
        w.upstream.bodies.lock().unwrap()[0]
            .to_string()
            .contains(&data[40..80])
    );
    assert_nowhere(&w, &data, &capture).await;
}
