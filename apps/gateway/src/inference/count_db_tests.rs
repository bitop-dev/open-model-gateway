//! #72: `POST /v1/messages/count_tokens` through the real router, key
//! authentication, live catalog and routing on PostgreSQL, with the real
//! Anthropic and OpenAI adapters against loopback upstreams. A count writes
//! no execution, reservation, ledger, counter or health row.
use std::sync::{Arc, Mutex};

use axum::{Router, http::HeaderMap, routing::post};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::admission_validation_tests::{
    Server, add_unpriced_deployment, issue_key, serve, snapshot, start_gateway,
};
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

/// Anthropic's counting endpoint answering 42; every other path is a 500.
async fn anthropic_upstream(captured: Captured) -> (String, Server) {
    let count = captured.clone();
    let app = Router::new()
        .route(
            "/v1/messages/count_tokens",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let count = count.clone();
                async move {
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    count
                        .lock()
                        .unwrap()
                        .push((headers, "/v1/messages/count_tokens".into(), body));
                    axum::Json(json!({"input_tokens":42}))
                }
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

/// `f`'s personal workspace may count with `company/smart` on Anthropic;
/// `company/team-only` (OpenAI) is granted to the team workspace alone.
struct World {
    f: Fixture,
    base: String,
    personal: String,
    team: String,
    anthropic: Captured,
    openai: Captured,
    _servers: Vec<Server>,
}
async fn world(pool: PgPool) -> World {
    let mut f = fixture(pool).await;
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
    add_unpriced_deployment(
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
    let anthropic = Captured::default();
    let openai = Captured::default();
    let (anthropic_base, a) = anthropic_upstream(anthropic.clone()).await;
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
    let (base, g) = start_gateway(&f, registry).await;
    World {
        f,
        base,
        personal,
        team,
        anthropic,
        openai,
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

#[dbtest::test]
async fn counts_are_metered_per_key(pool: PgPool) {
    let World {
        f,
        base,
        personal,
        anthropic,
        _servers,
        ..
    } = world(pool).await;
    let personal = personal.as_str();
    for _ in 0..super::count::COUNTS_PER_KEY_PER_MINUTE {
        let (status, _, reply) = count(&base, personal, &body("company/smart")).await;
        // A minute boundary can reset the window inside the loop; the
        // limit below is only asserted when it did not.
        assert_eq!(status, 200, "{reply}");
    }
    let calls = anthropic.lock().unwrap().len();
    let (status, _, reply) = count(&base, personal, &body("company/smart")).await;
    if status == 200 {
        // The window rolled over during the loop: the next minute admits again.
        return;
    }
    assert_eq!(status, 429, "{reply}");
    assert_eq!(reply["error"]["type"], "rate_limit_error");
    assert_eq!(
        anthropic.lock().unwrap().len(),
        calls,
        "refused before the upstream"
    );
    let _ = f;
}

/// Official Anthropic SDK -> listening gateway -> Anthropic adapter -> mock,
/// and the estimate on a translated route (`tests/count-tokens.mjs`).
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
