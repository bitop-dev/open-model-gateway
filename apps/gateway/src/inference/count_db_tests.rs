//! #72: `POST /v1/messages/count_tokens` through the real router, key
//! authentication, live catalog and routing on PostgreSQL, with the real
//! Anthropic and OpenAI adapters against loopback upstreams. A count writes
//! no execution, reservation, ledger, counter or health row.
use std::sync::{Arc, Mutex};

use axum::{Router, http::HeaderMap, routing::post};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::admission_validation_tests::{Server, issue_key, serve, snapshot};
use crate::{
    governance::tests::db::{Fixture, fixture},
    inference::error::InferenceError,
    providers::{
        ProviderRegistry,
        anthropic::AnthropicAdapter,
        openai::OpenAiAdapter,
        secrets::{Secret, SecretResolver},
    },
};

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
        Secret::new("provider-secret".into())
    }
}

type Captured = Arc<Mutex<Vec<(HeaderMap, String, Value)>>>;

/// What the loopback Anthropic upstream answers on its counting endpoint.
#[derive(Clone)]
pub(super) enum Reply {
    /// `{"input_tokens":42}`.
    Count,
    /// This status and body.
    Raw(u16, String),
    /// Never answers; `Upstream::dropped` becomes true when the gateway
    /// closes the request.
    Hang,
}
#[derive(Clone)]
pub(super) struct Upstream {
    pub(super) captured: Captured,
    pub(super) reply: Arc<Mutex<Reply>>,
    pub(super) started: Arc<tokio::sync::Notify>,
    pub(super) dropped: Arc<std::sync::atomic::AtomicBool>,
}
impl Upstream {
    fn new() -> Self {
        Self {
            captured: Captured::default(),
            reply: Arc::new(Mutex::new(Reply::Count)),
            started: Arc::new(tokio::sync::Notify::new()),
            dropped: Arc::default(),
        }
    }
    pub(super) fn answer(&self, reply: Reply) {
        *self.reply.lock().unwrap() = reply;
    }
}
/// Sets `dropped` when the handler future that holds it is dropped.
struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Anthropic's counting endpoint (per `Upstream::reply`) and its Messages
/// endpoint (a short reply); every other path is a 500.
async fn anthropic_upstream(upstream: Upstream) -> (String, Server) {
    let count = upstream.clone();
    let captured = upstream.captured.clone();
    let app = Router::new()
        .route(
            "/v1/messages/count_tokens",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let upstream = count.clone();
                async move {
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    upstream.captured.lock().unwrap().push((
                        headers,
                        "/v1/messages/count_tokens".into(),
                        body,
                    ));
                    let reply = upstream.reply.lock().unwrap().clone();
                    match reply {
                        Reply::Count => axum::response::IntoResponse::into_response(axum::Json(
                            json!({"input_tokens":42}),
                        )),
                        Reply::Raw(status, body) => axum::http::Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(body))
                            .unwrap(),
                        Reply::Hang => {
                            let _flag = DropFlag(upstream.dropped.clone());
                            upstream.started.notify_one();
                            std::future::pending::<()>().await;
                            unreachable!()
                        }
                    }
                }
            }),
        )
        .route(
            "/v1/messages",
            post(|| async {
                axum::Json(json!({"id":"m","type":"message","role":"assistant","model":"claude-upstream",
                    "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,
                    "usage":{"input_tokens":3,"output_tokens":1}}))
            }),
        )
        .fallback(move |uri: axum::http::Uri| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push((
                    HeaderMap::new(),
                    uri.path().to_owned(),
                    Value::Null,
                ));
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "unexpected")
            }
        });
    serve(app).await
}

/// How a test's gateway is built.
#[derive(Default)]
pub(super) struct Setup {
    pub(super) limits: crate::inference::EngineLimits,
    /// A fixed count window clock (a deterministic minute).
    pub(super) minute: Option<fn() -> i64>,
    /// Serve from a replica whose versioned caches are on and never refresh.
    pub(super) stale_cache: bool,
}

/// `f`'s personal workspace may count with `company/smart` on Anthropic;
/// `company/team-only` (OpenAI) is granted to the team workspace alone.
struct World {
    f: Fixture,
    base: String,
    personal: String,
    team: String,
    anthropic: Captured,
    openai: Captured,
    upstream: Upstream,
    _servers: Vec<Server>,
}
async fn world(pool: PgPool) -> World {
    world_with(pool, Setup::default()).await
}
async fn world_with(pool: PgPool, setup: Setup) -> World {
    let mut f = fixture(pool.clone()).await;
    let team = {
        let mut team = fixture_copy(&f);
        team.principal = f.team;
        issue_key(&mut team).await
    };
    let personal = issue_key(&mut f).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['messages'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(f.deployment)
        .execute(&f.store.pool)
        .await
        .unwrap();
    super::admission_validation_tests::add_deployment(
        &f,
        "anthropic",
        "env:FIXTURE_ONLY",
        None,
        "claude-upstream",
        0,
    )
    .await;
    let team_model = Uuid::new_v4();
    let connection = Uuid::new_v4();
    sqlx::raw_sql(&format!(
        "INSERT INTO models(id,public_name,supported_protocols) VALUES('{team_model}','company/team-only',ARRAY['messages']);
         INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES('{connection}','OpenAI','openai','env:FIXTURE_ONLY',true);
         INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES(gen_random_uuid(),'{team_model}','{connection}','gpt-upstream',true);
         INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES('{}','{team_model}','direct');",
        f.team.workspace_id
    ))
    .execute(&f.store.pool)
    .await
    .unwrap();
    let upstream = Upstream::new();
    let openai = Upstream::new();
    let (anthropic_base, a) = anthropic_upstream(upstream.clone()).await;
    let (openai_base, o) = anthropic_upstream(openai.clone()).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            format!("{anthropic_base}/v1"),
        )))
        .unwrap();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolver),
            format!("{openai_base}/v1"),
        )))
        .unwrap();
    let store = if setup.stale_cache {
        crate::cache::invalidation_tests::stale_replica(&pool).await
    } else {
        f.store.clone()
    };
    let mut engine =
        crate::inference::Engine::new(Arc::new(store.clone()), registry, setup.limits).unwrap();
    if let Some(minute) = setup.minute {
        engine = engine.with_count_clock(minute);
    }
    let (base, g) = serve(crate::http::router_with_engine(store, None, engine)).await;
    World {
        f,
        base,
        personal,
        team,
        anthropic: upstream.captured.clone(),
        openai: openai.captured.clone(),
        upstream,
        _servers: vec![a, o, g],
    }
}

fn fixture_copy(f: &Fixture) -> Fixture {
    Fixture {
        store: f.store.clone(),
        principal: f.principal,
        team: f.team,
        deployment: f.deployment,
        model: f.model,
        owner: f.owner,
        other: f.other,
    }
}

async fn count(base: &str, key: &str, body: &Value) -> (u16, Option<String>, Value) {
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{base}/v1/messages/count_tokens?beta=true"))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "token-counting-2024-11-01")
        .body(body.to_string())
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let source = response
        .headers()
        .get("x-omg-token-count")
        .map(|v| v.to_str().unwrap().to_owned());
    let body = response.json().await.unwrap_or(Value::Null);
    (status, source, body)
}

fn body(model: &str) -> Value {
    json!({"model":model,"system":"be brief","messages":[{"role":"user","content":"hello there"}],
        "tools":[{"name":"lookup","input_schema":{"type":"object"}}]})
}

#[dbtest::test]
async fn anthropic_counts_with_its_own_credential_and_writes_nothing(pool: PgPool) {
    let World {
        f,
        base,
        personal,
        anthropic,
        _servers,
        ..
    } = world(pool).await;
    let personal = personal.as_str();
    let before = snapshot(&f.store.pool).await;
    let (status, source, reply) = count(&base, personal, &body("company/smart")).await;
    assert_eq!(
        (status, source.as_deref()),
        (200, Some("provider")),
        "{reply}"
    );
    assert_eq!(reply, json!({"input_tokens":42}));
    let captured = anthropic.lock().unwrap().clone();
    assert_eq!(captured.len(), 1);
    let (headers, path, sent) = &captured[0];
    assert_eq!(path, "/v1/messages/count_tokens");
    // The connection's credential, never the client's inference key.
    assert_eq!(headers["x-api-key"], "provider-secret");
    assert!(headers.get("authorization").is_none());
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(headers["anthropic-beta"], "token-counting-2024-11-01");
    assert_eq!(
        sent,
        &json!({"model":"claude-upstream","system":"be brief",
            "messages":[{"role":"user","content":[{"type":"text","text":"hello there"}]}],
            "tools":[{"name":"lookup","input_schema":{"type":"object"}}]})
    );
    assert_eq!(snapshot(&f.store.pool).await, before, "no durable row");
}

#[dbtest::test]
async fn other_models_get_a_labelled_estimate_without_an_upstream_call(pool: PgPool) {
    let World {
        f,
        base,
        team,
        openai,
        _servers,
        ..
    } = world(pool).await;
    let team = team.as_str();
    let before = snapshot(&f.store.pool).await;
    let (status, source, reply) = count(&base, team, &body("company/team-only")).await;
    assert_eq!(
        (status, source.as_deref()),
        (200, Some("estimate; method=utf8-bytes-div-4")),
        "{reply}"
    );
    // "be brief" 8 + "hello there" 11 + "lookup" 6 + {"type":"object"} 17 = 42 bytes.
    assert_eq!(reply, json!({"input_tokens":11}));
    assert!(
        openai.lock().unwrap().is_empty(),
        "an estimate calls nothing"
    );
    assert_eq!(snapshot(&f.store.pool).await, before);
}

#[dbtest::test]
async fn workspace_and_model_come_from_the_key(pool: PgPool) {
    let World {
        f,
        base,
        personal,
        team,
        anthropic,
        openai,
        _servers,
        ..
    } = world(pool).await;
    let (personal, team) = (personal.as_str(), team.as_str());
    let before = snapshot(&f.store.pool).await;
    // A model granted only to another workspace, and an unknown model.
    for (key, model) in [
        (personal, "company/team-only"),
        (personal, "company/missing"),
        (team, "company/missing"),
    ] {
        let (status, _, reply) = count(&base, key, &body(model)).await;
        assert_eq!(status, 404, "{model}: {reply}");
        assert_eq!(reply["error"]["type"], "not_found_error");
    }
    // A revoked key, an unknown key and a missing key.
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE workspace_id=$1")
        .bind(f.team.workspace_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    for key in [team, "omg_not_a_key", ""] {
        let (status, _, reply) = count(&base, key, &body("company/team-only")).await;
        assert_eq!(status, 401, "{reply}");
        assert_eq!(reply["error"]["type"], "authentication_error");
    }
    assert!(anthropic.lock().unwrap().is_empty());
    assert!(openai.lock().unwrap().is_empty());
    assert_eq!(snapshot(&f.store.pool).await, before);
}

#[dbtest::test]
async fn generation_fields_bounds_and_body_size_are_refused_before_routing(pool: PgPool) {
    let World {
        f,
        base,
        personal,
        anthropic,
        _servers,
        ..
    } = world(pool).await;
    let personal = personal.as_str();
    let before = snapshot(&f.store.pool).await;
    for (extra, field) in [
        (json!({"max_tokens":16}), "max_tokens"),
        (json!({"stream":true}), "stream"),
        (json!({"temperature":0.5}), "temperature"),
        (json!({"metadata":{"user_id":"u"}}), "metadata"),
    ] {
        let mut wire = body("company/smart");
        wire.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let (status, _, reply) = count(&base, personal, &wire).await;
        assert_eq!(status, 400, "{field}");
        assert_eq!(reply["error"]["message"], format!("Unknown field: {field}"));
    }
    let mut wire = body("company/smart");
    wire["tools"] = json!([{"type":"web_search_20250305","name":"web_search"}]);
    let (status, _, reply) = count(&base, personal, &wire).await;
    assert_eq!(status, 400, "{reply}");
    let mut wire = body("company/smart");
    wire["messages"][0]["content"] = json!("x".repeat(2 * 1024 * 1024));
    let (status, _, reply) = count(&base, personal, &wire).await;
    assert_eq!(status, 413, "{reply}");
    assert_eq!(reply["type"], "error");
    assert!(anthropic.lock().unwrap().is_empty());
    assert_eq!(snapshot(&f.store.pool).await, before);
}

static MINUTE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
fn test_minute() -> i64 {
    MINUTE.load(std::sync::atomic::Ordering::SeqCst)
}

/// The per-key window, on a clock the test controls: exactly the limit is
/// admitted in one minute, the next count is 429 before any upstream call,
/// and the next minute admits again.
#[dbtest::test]
async fn counts_are_metered_per_key(pool: PgPool) {
    let limits = crate::inference::EngineLimits {
        counts_per_key_per_minute: 3,
        ..Default::default()
    };
    MINUTE.store(1_000, std::sync::atomic::Ordering::SeqCst);
    let World {
        base,
        personal,
        anthropic,
        _servers,
        ..
    } = world_with(
        pool,
        Setup {
            limits,
            minute: Some(test_minute),
            ..Default::default()
        },
    )
    .await;
    for _ in 0..3 {
        assert_eq!(count(&base, &personal, &body("company/smart")).await.0, 200);
    }
    let (status, _, reply) = count(&base, &personal, &body("company/smart")).await;
    assert_eq!(status, 429, "{reply}");
    assert_eq!(reply["error"]["type"], "rate_limit_error");
    assert_eq!(
        anthropic.lock().unwrap().len(),
        3,
        "refused before the upstream"
    );
    MINUTE.store(1_001, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(count(&base, &personal, &body("company/smart")).await.0, 200);
}

#[dbtest::test]
async fn official_sdk_counts_on_both_kinds_of_route(pool: PgPool) {
    let World {
        f,
        base,
        personal,
        team,
        anthropic,
        openai,
        _servers,
        ..
    } = world(pool).await;
    // The team key's workspace may not use `company/smart`.
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1 AND model_id=$2")
        .bind(f.team.workspace_id)
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let before = snapshot(&f.store.pool).await;
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/count-tokens.mjs");
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("node")
            .arg(script)
            .env("SDK_TEST_BASE_URL", &base)
            .env("SDK_TEST_API_KEY", &personal)
            .env("SDK_TEST_OTHER_KEY", &team)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("SDK deadline")
    .expect("node and npm ci are required for SDK contracts");
    assert!(
        output.status.success(),
        "SDK count contract failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = anthropic.lock().unwrap().clone();
    assert_eq!(captured.len(), 2, "one upstream count per native request");
    assert!(captured.iter().all(|(h, p, _)| {
        p == "/v1/messages/count_tokens" && h["x-api-key"] == "provider-secret"
    }));
    // The beta namespace adds its own beta; values are forwarded verbatim.
    assert!(
        captured[1].0["anthropic-beta"]
            .to_str()
            .unwrap()
            .split(',')
            .all(|b| b.trim() == "token-counting-2024-11-01")
    );
    assert!(openai.lock().unwrap().is_empty());
    assert_eq!(snapshot(&f.store.pool).await, before, "no durable row");
}

static CONNECTION_MINUTE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(5);
fn connection_minute() -> i64 {
    CONNECTION_MINUTE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Provider-backed counts share one window per provider connection
/// credential across keys and workspaces; estimates never spend it. Every
/// outcome is visible in `gateway_token_counts_total{outcome}`.
#[dbtest::test]
async fn provider_counts_share_a_per_connection_window(pool: PgPool) {
    use crate::metrics::METRICS;
    let limits = crate::inference::EngineLimits {
        counts_per_connection_per_minute: 2,
        ..Default::default()
    };
    let World {
        base,
        personal,
        team,
        anthropic,
        openai,
        _servers,
        ..
    } = world_with(
        pool,
        Setup {
            limits,
            minute: Some(connection_minute),
            ..Default::default()
        },
    )
    .await;
    let before = ["provider", "estimate", "limited", "error"].map(|o| METRICS.token_count_total(o));
    assert_eq!(count(&base, &personal, &body("company/smart")).await.0, 200);
    assert_eq!(count(&base, &team, &body("company/smart")).await.0, 200);
    let (status, _, reply) = count(&base, &team, &body("company/smart")).await;
    assert_eq!(status, 429, "another key, the same credential: {reply}");
    assert_eq!(reply["error"]["type"], "rate_limit_error");
    assert_eq!(anthropic.lock().unwrap().len(), 2);
    let (status, source, _) = count(&base, &team, &body("company/team-only")).await;
    assert_eq!(
        (status, source.as_deref()),
        (200, Some("estimate; method=utf8-bytes-div-4")),
        "estimates are not provider-backed"
    );
    assert!(openai.lock().unwrap().is_empty());
    assert_eq!(
        count(&base, &personal, &body("company/missing")).await.0,
        404
    );
    let after = ["provider", "estimate", "limited", "error"].map(|o| METRICS.token_count_total(o));
    // Other tests count in parallel: each outcome moved by at least this run's.
    for ((b, a), n) in before.iter().zip(after).zip([2, 1, 1, 1]) {
        assert!(a - b >= n, "{before:?} -> {after:?}");
    }
}

/// Counts run in their own pool: with one count slot and one generation
/// permit, a count that waits on the upstream fills only the count pool (a
/// second count is 429) while generation still runs. Dropping the client
/// request cancels the upstream call.
#[dbtest::test]
async fn the_count_pool_is_separate_and_a_disconnect_cancels_upstream(pool: PgPool) {
    let limits = crate::inference::EngineLimits {
        count_concurrency: 1,
        max_concurrent: 1,
        ..Default::default()
    };
    let World {
        base,
        personal,
        team,
        upstream,
        _servers,
        ..
    } = world_with(
        pool,
        Setup {
            limits,
            ..Default::default()
        },
    )
    .await;
    upstream.answer(Reply::Hang);
    let started = upstream.started.notified();
    let pending = tokio::spawn({
        let (base, personal) = (base.clone(), personal.clone());
        async move { count(&base, &personal, &body("company/smart")).await }
    });
    started.await;
    let (status, _, reply) = count(&base, &team, &body("company/smart")).await;
    assert_eq!(status, 429, "the count pool is full: {reply}");
    assert_eq!(reply["error"]["type"], "rate_limit_error");
    let generation = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", &personal)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(generation.status(), 200, "generation keeps its permit");
    assert!(!upstream.dropped.load(std::sync::atomic::Ordering::SeqCst));
    pending.abort();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !upstream.dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "upstream call not cancelled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    upstream.answer(Reply::Count);
    assert_eq!(
        count(&base, &team, &body("company/smart")).await.0,
        200,
        "the slot is free again"
    );
}

/// One authorization change that bypasses every cache version bump.
enum Change {
    RevokeKey,
    RestrictKey,
    RemoveGrant,
    DisableDeployment,
}
/// With the versioned caches on and never refreshed (a stale replica), a
/// count warmed by a success still sees each change at once: the live read
/// (`live_candidates`) never serves the cached candidate list.
async fn stale_cache_case(pool: PgPool, change: Change, expected: u16) {
    let World {
        f,
        base,
        personal,
        _servers,
        ..
    } = world_with(
        pool,
        Setup {
            stale_cache: true,
            ..Default::default()
        },
    )
    .await;
    for _ in 0..2 {
        assert_eq!(count(&base, &personal, &body("company/smart")).await.0, 200);
    }
    let key = f.principal.key_id;
    let workspace = f.principal.workspace_id;
    let sql = match change {
        Change::RevokeKey => format!("UPDATE api_keys SET revoked_at=now() WHERE id='{key}'"),
        Change::RestrictKey => format!(
            "INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES('{workspace}','{key}')"
        ),
        Change::RemoveGrant => format!(
            "DELETE FROM workspace_model_grants WHERE workspace_id='{workspace}' AND model_id='{}'",
            f.model
        ),
        Change::DisableDeployment => {
            format!(
                "UPDATE deployments SET enabled=false WHERE model_id='{}'",
                f.model
            )
        }
    };
    sqlx::raw_sql(&sql).execute(&f.store.pool).await.unwrap();
    let (status, _, reply) = count(&base, &personal, &body("company/smart")).await;
    assert_eq!(status, expected, "{reply}");
}
#[dbtest::test]
async fn a_stale_cache_never_serves_a_revoked_key(pool: PgPool) {
    stale_cache_case(pool, Change::RevokeKey, 401).await;
}
#[dbtest::test]
async fn a_stale_cache_never_serves_a_new_key_restriction(pool: PgPool) {
    stale_cache_case(pool, Change::RestrictKey, 404).await;
}
#[dbtest::test]
async fn a_stale_cache_never_serves_a_removed_grant(pool: PgPool) {
    stale_cache_case(pool, Change::RemoveGrant, 404).await;
}
#[dbtest::test]
async fn a_stale_cache_never_serves_a_disabled_deployment(pool: PgPool) {
    stale_cache_case(pool, Change::DisableDeployment, 404).await;
}

/// A model the key may use but whose deployments do not serve `messages`
/// is 501, as for generation; the 404 cases are the model being unknown,
/// ungranted or restricted.
#[dbtest::test]
async fn a_granted_model_without_a_messages_route_is_501(pool: PgPool) {
    let World {
        f,
        base,
        personal,
        anthropic,
        _servers,
        ..
    } = world(pool).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let (status, _, reply) = count(&base, &personal, &body("company/smart")).await;
    assert_eq!(status, 501, "{reply}");
    assert_eq!(reply["error"]["type"], "invalid_request_error");
    assert!(anthropic.lock().unwrap().is_empty());
}

async fn count_with(base: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut request = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{base}/v1/messages/count_tokens"))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(body("company/smart").to_string());
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// The authorization cases on real PostgreSQL, each checked live.
#[dbtest::test]
async fn authorization_cases_on_the_count_route(pool: PgPool) {
    let World {
        f,
        base,
        personal,
        team,
        anthropic,
        _servers,
        ..
    } = world(pool).await;
    let bearer = format!("Bearer {personal}");
    assert_eq!(
        count_with(&base, &[("authorization", &bearer)]).await.0,
        200,
        "Bearer form"
    );
    for (name, headers) in [
        (
            "both credential headers",
            vec![
                ("authorization", bearer.as_str()),
                ("x-api-key", personal.as_str()),
            ],
        ),
        (
            "a session cookie only",
            vec![("cookie", "omg_session=not-a-key")],
        ),
        ("no credential", vec![]),
    ] {
        let (status, reply) = count_with(&base, &headers).await;
        assert_eq!(status, 401, "{name}");
        assert_eq!(reply["error"]["type"], "authentication_error", "{name}");
    }
    assert_eq!(
        count(&base, &personal, &body("company/team-only")).await.0,
        404,
        "a personal-workspace key asking for another workspace's model"
    );
    let (key, workspace) = (f.principal.key_id, f.principal.workspace_id);
    let run = |sql: String| {
        let pool = f.store.pool.clone();
        async move { sqlx::raw_sql(&sql).execute(&pool).await.unwrap() }
    };
    run(format!("INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES('{workspace}','{key}')")).await;
    assert_eq!(
        count(&base, &personal, &body("company/smart")).await.0,
        404,
        "key model restriction"
    );
    run(format!(
        "DELETE FROM key_model_restrictions WHERE governance_key_id='{key}'"
    ))
    .await;
    assert_eq!(count(&base, &personal, &body("company/smart")).await.0, 200);
    run(format!("UPDATE api_keys SET expires_at=now()-interval '1 minute' WHERE workspace_id='{}' AND name='admission validation fixture'", f.team.workspace_id)).await;
    assert_eq!(
        count(&base, &team, &body("company/smart")).await.0,
        401,
        "expired key"
    );
    run(format!(
        "UPDATE workspaces SET disabled_at=now() WHERE id='{workspace}'"
    ))
    .await;
    assert_eq!(
        count(&base, &personal, &body("company/smart")).await.0,
        401,
        "disabled workspace"
    );
    run(format!(
        "UPDATE workspaces SET disabled_at=NULL WHERE id='{workspace}'"
    ))
    .await;
    assert_eq!(count(&base, &personal, &body("company/smart")).await.0, 200);
    run(format!(
        "DELETE FROM platform_role_grants WHERE user_id='{}'",
        f.owner
    ))
    .await;
    assert_eq!(
        count(&base, &personal, &body("company/smart")).await.0,
        401,
        "owner's platform role removed"
    );
    assert_eq!(
        anthropic.lock().unwrap().len(),
        3,
        "only the three admitted counts reached Anthropic"
    );
}

/// What Anthropic's counting endpoint may answer, and what the client sees.
#[dbtest::test]
async fn anthropic_count_replies_are_bounded_and_classified(pool: PgPool) {
    let World {
        base,
        personal,
        upstream,
        _servers,
        ..
    } = world(pool).await;
    let error = |kind: &str| {
        json!({"type":"error","error":{"type":kind,"message":"SECRET upstream text"}}).to_string()
    };
    for (reply, status, kind) in [
        (
            Reply::Raw(
                200,
                json!({"input_tokens":1,"pad":"x".repeat(16 * 1024)}).to_string(),
            ),
            502,
            "api_error",
        ),
        (
            Reply::Raw(200, json!({"input_tokens":1.5}).to_string()),
            502,
            "api_error",
        ),
        (
            Reply::Raw(200, json!({"input_tokens":"42"}).to_string()),
            502,
            "api_error",
        ),
        (
            Reply::Raw(200, json!({"input_tokens":-1}).to_string()),
            502,
            "api_error",
        ),
        (
            Reply::Raw(429, error("rate_limit_error")),
            429,
            "rate_limit_error",
        ),
        (Reply::Raw(500, error("api_error")), 502, "overloaded_error"),
        (
            Reply::Raw(529, error("overloaded_error")),
            529,
            "overloaded_error",
        ),
    ] {
        upstream.answer(reply);
        let (got, _, body) = count(&base, &personal, &body("company/smart")).await;
        assert_eq!(
            (got, body["error"]["type"].as_str()),
            (status, Some(kind)),
            "{body}"
        );
        assert!(!body.to_string().contains("SECRET"), "{body}");
    }
}
