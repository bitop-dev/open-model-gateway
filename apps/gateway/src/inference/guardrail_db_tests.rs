//! Guardrail input enforcement end to end (#840): real PostgreSQL, the HTTP
//! router and a counting mock upstream. A refused input reserves nothing and
//! reaches no provider; every protocol carries the documented body; decisions
//! are recorded without text; and a canary value sent through every action
//! appears in no table, log or metric.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{Router, body::Body, http::StatusCode, response::Response};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::admission_validation_tests::{
    Server, add_deployment, issue_key, serve, snapshot, start_gateway,
};
use crate::governance::tests::db::{Fixture, fixture};
use crate::inference::error::InferenceError;
use crate::providers::{
    ProviderRegistry,
    anthropic::AnthropicAdapter,
    openai::OpenAiAdapter,
    secrets::{Secret, SecretResolver},
};

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
        Secret::new("test-secret-value".into())
    }
}

fn mock(hits: Arc<AtomicUsize>) -> Router {
    Router::new().fallback(move |uri: axum::http::Uri| {
        let hits = hits.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let body = if uri.path().ends_with("/messages") {
                json!({"id":"m1","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"ok"}],
                    "stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}})
            } else if uri.path().ends_with("/embeddings") {
                json!({"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"model":"m","usage":{"prompt_tokens":1,"total_tokens":1}})
            } else {
                json!({"id":"c1","object":"chat.completion","created":1,"model":"m",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        }
    })
}

pub(super) struct World {
    pub(super) f: Fixture,
    pub(super) token: String,
    pub(super) base: String,
    pub(super) hits: Arc<AtomicUsize>,
    pub(super) upstream: Server,
    _gateway: Server,
}
pub(super) async fn world(pool: PgPool, protocols: &str) -> World {
    world_on(pool, protocols, "openai").await
}
pub(super) async fn world_on(pool: PgPool, protocols: &str, provider: &str) -> World {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(f.deployment)
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "UPDATE models SET supported_protocols=ARRAY[{protocols}]"
    ))
    .execute(&f.store.pool)
    .await
    .unwrap();
    add_deployment(&f, provider, "env:NOT_RESOLVED", None, "gpt-test", 1).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let (upstream, up) = serve(mock(hits.clone())).await;
    let mut registry = ProviderRegistry::default();
    if provider == "anthropic" {
        registry
            .register(Arc::new(AnthropicAdapter::for_test(
                Arc::new(Resolver),
                format!("{upstream}/v1"),
            )))
            .unwrap();
    } else {
        registry
            .register(Arc::new(OpenAiAdapter::for_test(
                Arc::new(Resolver),
                format!("{upstream}/v1"),
            )))
            .unwrap();
    }
    let (base, gw) = start_gateway(&f, registry).await;
    World {
        f,
        token,
        base,
        hits,
        upstream: up,
        _gateway: gw,
    }
}

/// The type default of the principal's workspace kind, with one rule.
pub(super) async fn type_default(
    w: &World,
    action: &str,
    categories: &str,
    patterns: &[String],
) -> (Uuid, Uuid) {
    let pool = &w.f.store.pool;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(w.f.principal.workspace_id)
        .fetch_one(pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM guardrail_rule_sets WHERE layer='type_default' AND kind=$1",
    )
    .bind(&kind)
    .fetch_optional(&mut *tx)
    .await
    .unwrap();
    let set = match existing {
        Some(set) => {
            sqlx::query(
                "UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1",
            )
            .bind(set)
            .bind(w.f.owner)
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::query("DELETE FROM guardrail_rules WHERE rule_set_id=$1")
                .bind(set)
                .execute(&mut *tx)
                .await
                .unwrap();
            set
        }
        None => {
            let set = Uuid::now_v7();
            sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default',$2,false,$3)")
                .bind(set).bind(&kind).bind(w.f.owner).execute(&mut *tx).await.unwrap();
            set
        }
    };
    let rule = Uuid::now_v7();
    let protection = if patterns.is_empty() {
        "secrets"
    } else {
        "custom"
    };
    let on_error = if action == "block" {
        "fail_closed"
    } else {
        "fail_open"
    };
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,patterns) VALUES($1,$2,0,'Rule',$3,'pattern',$4::text[],$5,'user_text','off',$6,'{generation,embeddings}','deny',$7)")
        .bind(rule).bind(set).bind(protection).bind(categories).bind(action).bind(on_error).bind(patterns)
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    (set, rule)
}

/// A gateway key's shape, assembled here (never a real key).
pub(super) fn secret() -> String {
    let hex: String = "0123456789abcdef".chars().cycle().take(96).collect();
    format!("{}_{}.{}", "omg", &hex[..32], &hex[32..])
}

pub(super) async fn send(
    w: &World,
    path: &str,
    body: Value,
) -> (StatusCode, Option<String>, Value) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .post(format!("{}{path}", w.base))
        .bearer_auth(&w.token)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let retry = response
        .headers()
        .get("x-should-retry")
        .map(|v| v.to_str().unwrap().to_owned());
    (status, retry, response.json().await.unwrap_or(Value::Null))
}

pub(super) async fn events(w: &World) -> Vec<(String, String, String, i32)> {
    sqlx::query_as(
        "SELECT operation,action,category,count FROM guardrail_events ORDER BY created_at,id",
    )
    .fetch_all(&w.f.store.pool)
    .await
    .unwrap()
}

#[dbtest::test]
async fn a_blocked_input_reserves_nothing_and_every_protocol_says_why(pool: PgPool) {
    let w = world(pool, "'chat_completions','messages','responses'").await;
    let (set, rule) = type_default(&w, "block", "{secret}", &[]).await;
    let before = snapshot(&w.f.store.pool).await;
    let key = secret();
    let guardrail = json!({"policy_id": set, "rule_id": rule, "stage": "input", "category": "secret", "detector": "pattern"});
    let chat = json!({"model":"company/smart","messages":[{"role":"user","content":format!("deploy with {key}")}]});
    let (status, retry, body) = send(&w, "/v1/chat/completions", chat).await;
    assert_eq!(
        (status, retry.as_deref()),
        (StatusCode::BAD_REQUEST, Some("false"))
    );
    assert_eq!(body["error"]["code"], "guardrail_input_blocked");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["guardrail"], guardrail);
    let responses = json!({"model":"company/smart","input":format!("deploy with {key}"),"max_output_tokens":16});
    let (status, _, body) = send(&w, "/v1/responses", responses).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "guardrail_input_blocked");
    assert_eq!(body["error"]["guardrail"], guardrail);
    let messages = json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":[{"type":"text","text":format!("deploy with {key}")}]}]});
    let (status, retry, body) = send(&w, "/v1/messages", messages).await;
    assert_eq!(
        (status, retry.as_deref()),
        (StatusCode::BAD_REQUEST, Some("false"))
    );
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["guardrail"], guardrail);
    // Nothing reached a provider and nothing was reserved or recorded as an
    // execution; the three decisions are recorded.
    assert_eq!(w.hits.load(Ordering::SeqCst), 0);
    assert_eq!(snapshot(&w.f.store.pool).await, before);
    let recorded = events(&w).await;
    let ops: Vec<&str> = recorded.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(ops, ["chat_completions", "responses", "messages"]);
    assert!(
        recorded
            .iter()
            .all(|e| (e.1.as_str(), e.2.as_str(), e.3) == ("blocked", "secret", 1))
    );
    // Text without a secret passes, once.
    let clean = json!({"model":"company/smart","messages":[{"role":"user","content":"hello"}]});
    let (status, _, _) = send(&w, "/v1/chat/completions", clean).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.hits.load(Ordering::SeqCst), 1);
}

#[dbtest::test]
async fn embeddings_inputs_are_inspected(pool: PgPool) {
    let w = world(pool, "'embeddings'").await;
    type_default(&w, "block", "{secret}", &[]).await;
    let (status, retry, body) = send(
        &w,
        "/v1/embeddings",
        json!({"model":"company/smart","input":["fine", format!("and {}", secret())]}),
    )
    .await;
    assert_eq!(
        (status, retry.as_deref()),
        (StatusCode::BAD_REQUEST, Some("false"))
    );
    assert_eq!(body["error"]["code"], "guardrail_input_blocked");
    assert_eq!(w.hits.load(Ordering::SeqCst), 0);
    assert!(
        !body.to_string().contains("omg_"),
        "the body never echoes the match"
    );
    assert_eq!(events(&w).await[0].0, "embeddings");
}

#[dbtest::test]
async fn a_monitor_rule_records_and_lets_the_request_through(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    type_default(&w, "monitor", "{custom}", &["PROJECT-[0-9]{4}".into()]).await;
    let chat = json!({"model":"company/smart","messages":[{"role":"user","content":"PROJECT-1234 and PROJECT-5678, again PROJECT-1234"}]});
    let (status, _, _) = send(&w, "/v1/chat/completions", chat).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        events(&w).await,
        [(
            "chat_completions".into(),
            "monitored".into(),
            "custom".into(),
            2
        )]
    );
}

#[dbtest::test]
async fn a_canary_through_every_action_is_stored_logged_and_measured_nowhere(pool: PgPool) {
    let capture = crate::log_capture_test::Capture::start();
    // An Anthropic route, which takes image blocks.
    let w = world_on(pool, "'chat_completions','messages'", "anthropic").await;
    let canary = format!("CANARY-{}", &Uuid::now_v7().simple().to_string()[..12]);
    let pattern = "CANARY-[0-9a-f]{12}".to_owned();
    let chat = json!({"model":"company/smart","messages":[{"role":"user","content":format!("note {canary}")}]});
    let with_image = json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":[
        {"type":"text","text":format!("note {canary}")},
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="}}]}]});
    // Monitored (and skipped: the image cannot be read), with a rule this
    // release cannot run beside it (detector_failed, fail-open).
    let (set, _) = type_default(&w, "monitor", "{custom}", std::slice::from_ref(&pattern)).await;
    let mut tx = w.f.store.pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
        .bind(set)
        .bind(w.f.owner)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,1,'Later','moderation','systemone','{moderation}','monitor','user_text','off','fail_open','{generation}','deny')")
        .bind(Uuid::now_v7()).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        send(&w, "/v1/chat/completions", chat.clone()).await.0,
        StatusCode::OK
    );
    let (status, _, body) = send(&w, "/v1/messages", with_image).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Blocked.
    type_default(&w, "block", "{custom}", std::slice::from_ref(&pattern)).await;
    let (status, _, body) = send(&w, "/v1/chat/completions", chat).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        !body.to_string().contains(&canary),
        "error bodies never echo the match"
    );
    let mut actions: Vec<String> = events(&w).await.into_iter().map(|e| e.1).collect();
    actions.sort();
    actions.dedup();
    assert_eq!(
        actions,
        ["blocked", "detector_failed", "monitored", "skipped"]
    );
    // Every table, the log and the metrics.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname='public' ORDER BY tablename",
    )
    .fetch_all(&w.f.store.pool)
    .await
    .unwrap();
    for table in tables {
        let rows: String = sqlx::query_scalar(&format!(
            "SELECT coalesce(jsonb_agg(to_jsonb(t)),'[]'::jsonb)::text FROM public.\"{table}\" t"
        ))
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
        assert!(!rows.contains(&canary), "found in table {table}");
    }
    let metrics = crate::metrics::METRICS.render(None).await;
    assert!(metrics.contains("gateway_guardrail_decisions_total"));
    for (name, text) in [("metrics", metrics), ("log", capture.text())] {
        assert!(!text.contains(&canary), "found in the {name}");
    }
}

#[dbtest::test]
async fn a_token_count_is_inspected_before_it_reaches_the_provider(pool: PgPool) {
    let w = world_on(pool, "'messages'", "anthropic").await;
    type_default(&w, "block", "{secret}", &[]).await;
    let body = json!({"model":"company/smart","messages":[{"role":"user","content":format!("count {}", secret())}]});
    let (status, retry, body) = send(&w, "/v1/messages/count_tokens", body).await;
    assert_eq!(
        (status, retry.as_deref()),
        (StatusCode::BAD_REQUEST, Some("false")),
        "{body}"
    );
    assert_eq!(body["error"]["guardrail"]["category"], "secret");
    assert_eq!(
        w.hits.load(Ordering::SeqCst),
        0,
        "nothing was sent to count"
    );
}

/// A one-pixel PNG as a data URL (an image the gateway accepts).
const PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
fn chat_with_image(text: &str) -> Value {
    json!({"model":"company/smart","messages":[{"role":"user","content":[
        {"type":"text","text":text},{"type":"image_url","image_url":{"url":PNG}}]}]})
}
async fn images_on(w: &World) {
    sqlx::query("UPDATE deployments SET image_input=true")
        .execute(&w.f.store.pool)
        .await
        .unwrap();
}

/// An image part cannot be inspected: a blocking rule refuses the request
/// (`guardrail_not_supported`) before anything is admitted or sent, on Chat
/// Completions and Responses; the text beside the image is still read, so a
/// secret there is a finding, which answers.
#[dbtest::test]
async fn an_image_is_refused_by_a_blocking_rule_and_the_text_beside_it_is_read(pool: PgPool) {
    let w = world(pool, "'chat_completions','responses'").await;
    images_on(&w).await;
    let (set, rule) = type_default(&w, "block", "{secret}", &[]).await;
    let before = snapshot(&w.f.store.pool).await;
    let guardrail = json!({"policy_id": set, "rule_id": rule, "stage": "input", "category": "secret", "detector": "pattern"});
    let (status, retry, body) =
        send(&w, "/v1/chat/completions", chat_with_image("what is this?")).await;
    assert_eq!(
        (status, retry.as_deref()),
        (StatusCode::BAD_REQUEST, Some("false")),
        "{body}"
    );
    assert_eq!(body["error"]["code"], "guardrail_not_supported");
    assert_eq!(body["error"]["guardrail"], guardrail);
    let responses = json!({"model":"company/smart","max_output_tokens":16,"input":[{"role":"user","content":[
        {"type":"input_text","text":"what is this?"},{"type":"input_image","image_url":PNG}]}]});
    let (status, _, body) = send(&w, "/v1/responses", responses).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "guardrail_not_supported");
    // A secret in the text beside an image is found: the block answers.
    let (status, _, body) = send(
        &w,
        "/v1/chat/completions",
        chat_with_image(&format!("deploy with {}", secret())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "guardrail_input_blocked");
    assert!(
        !body.to_string().contains("iVBOR"),
        "the image is never echoed"
    );
    // Nothing reached a provider or was reserved.
    assert_eq!(w.hits.load(Ordering::SeqCst), 0);
    assert_eq!(snapshot(&w.f.store.pool).await, before);
    let refused: Vec<(String, String)> = sqlx::query_as(
        "SELECT operation,action FROM guardrail_events WHERE refused ORDER BY created_at,id",
    )
    .fetch_all(&w.f.store.pool)
    .await
    .unwrap();
    assert_eq!(
        refused,
        [
            ("chat_completions".into(), "skipped".into()),
            ("responses".into(), "skipped".into()),
            ("chat_completions".into(), "blocked".into()),
        ]
    );
}

/// Under a rule that does not block, an image passes and the rule records
/// `skipped` (it could not read the request); a secret beside the image is
/// still found and recorded `monitored`.
#[dbtest::test]
async fn an_image_under_a_monitor_rule_passes_and_is_recorded_skipped(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    images_on(&w).await;
    type_default(&w, "monitor", "{secret}", &[]).await;
    let (status, _, body) =
        send(&w, "/v1/chat/completions", chat_with_image("what is this?")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        events(&w).await,
        [(
            "chat_completions".into(),
            "skipped".into(),
            "secret".into(),
            1
        )]
    );
    let (status, _, body) = send(
        &w,
        "/v1/chat/completions",
        chat_with_image(&format!("deploy with {}", secret())),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut last: Vec<(String, i32)> = events(&w).await[1..]
        .iter()
        .map(|e| (e.1.clone(), e.3))
        .collect();
    last.sort();
    assert_eq!(last, [("monitored".into(), 1), ("skipped".into(), 1)]);
    assert_eq!(w.hits.load(Ordering::SeqCst), 2);
}
