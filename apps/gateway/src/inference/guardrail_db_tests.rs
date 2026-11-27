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
    let (credential, region) = match provider {
        "bedrock" => ("aws:default", Some("us-east-1")),
        _ => ("env:NOT_RESOLVED", None),
    };
    add_deployment(&f, provider, credential, region, "gpt-test", 1).await;
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
    } else if provider == "bedrock" {
        registry
            .register(Arc::new(
                crate::providers::bedrock::BedrockAdapter::for_test(&upstream),
            ))
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
    type_default_with(w, action, categories, patterns, &[]).await
}
/// As [`type_default`], for a personal-data rule naming `kinds`.
pub(super) async fn type_default_with(
    w: &World,
    action: &str,
    categories: &str,
    patterns: &[String],
    kinds: &[&str],
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
    let protection = if !kinds.is_empty() {
        "personal_data"
    } else if patterns.is_empty() {
        "secrets"
    } else {
        "custom"
    };
    let on_error = if action == "block" {
        "fail_closed"
    } else {
        "fail_open"
    };
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,patterns,kinds) VALUES($1,$2,0,'Rule',$3,'pattern',$4::text[],$5,'user_text','off',$6,'{generation,embeddings}','deny',$7,$8)")
        .bind(rule).bind(set).bind(protection).bind(categories).bind(action).bind(on_error).bind(patterns).bind(kinds)
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

/// #841: a personal-data rule looks only for the kinds it names, records a
/// decision per kind it found (with the kind, never the value), and blocks on
/// its kinds only.
#[dbtest::test]
async fn a_personal_data_rule_looks_for_its_kinds_only(pool: PgPool) {
    let w = world(pool, "'chat_completions'").await;
    let kinds = async |w: &World| -> Vec<(String, Option<String>, i32)> {
        sqlx::query_as("SELECT action,kind,count FROM guardrail_events WHERE category='personal_data' ORDER BY kind")
            .fetch_all(&w.f.store.pool)
            .await
            .unwrap()
    };
    let text = "mail alice.smith@acme-corp.io or bob@acme-corp.io from 8.8.8.8 (and 10.0.0.1), card 4111 1111 1111 1111";
    type_default_with(&w, "monitor", "{personal_data}", &[], &["email", "ip"]).await;
    let chat = json!({"model":"company/smart","messages":[{"role":"user","content":text}]});
    let (status, _, _) = send(&w, "/v1/chat/completions", chat.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        kinds(&w).await,
        [
            ("monitored".into(), Some("email".into()), 2),
            ("monitored".into(), Some("ip".into()), 1),
        ],
        "the card is not a kind this rule names; the private address is not personal data"
    );
    // A blocking rule on cards refuses the same text before anything is sent,
    // and lets through text without a card.
    type_default_with(&w, "block", "{personal_data}", &[], &["card"]).await;
    let hits = w.hits.load(Ordering::SeqCst);
    let (status, _, body) = send(&w, "/v1/chat/completions", chat).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "guardrail_input_blocked");
    assert_eq!(body["error"]["guardrail"]["category"], "personal_data");
    assert!(!body.to_string().contains("4111"), "{body}");
    assert_eq!(w.hits.load(Ordering::SeqCst), hits, "nothing was sent");
    let blocked: (String, Option<String>, bool) = sqlx::query_as("SELECT action,kind,refused FROM guardrail_events WHERE category='personal_data' AND action='blocked'")
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    assert_eq!(blocked, ("blocked".into(), Some("card".into()), true));
    let clean = json!({"model":"company/smart","messages":[{"role":"user","content":"mail alice.smith@acme-corp.io"}]});
    assert_eq!(
        send(&w, "/v1/chat/completions", clean).await.0,
        StatusCode::OK
    );
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
    let responses = json!({"model":"company/smart","max_output_tokens":16,"input":[{"role":"user","content":[
        {"type":"input_text","text":format!("deploy with {}", secret())},{"type":"input_image","image_url":PNG}]}]});
    let (status, _, body) = send(&w, "/v1/responses", responses).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "guardrail_input_blocked");
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
            ("responses".into(), "blocked".into()),
        ]
    );
    // Beside each block, the image was recorded as not read.
    let skipped: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM guardrail_events WHERE action='skipped' AND NOT refused",
    )
    .fetch_one(&w.f.store.pool)
    .await
    .unwrap();
    assert_eq!(skipped, 2);
}

#[dbtest::test]
async fn an_image_is_refused_by_a_blocking_rule_on_an_anthropic_route(pool: PgPool) {
    blocking_rule_with_an_image_on(pool, "anthropic").await;
}

#[dbtest::test]
async fn an_image_is_refused_by_a_blocking_rule_on_a_bedrock_route(pool: PgPool) {
    blocking_rule_with_an_image_on(pool, "bedrock").await;
}

/// On the routes that carry images to Anthropic and Bedrock, as on OpenAI:
/// a blocking rule refuses a Chat Completions request with an image
/// (`guardrail_not_supported`) with nothing admitted or sent, and a secret
/// in the text beside the image is still found.
async fn blocking_rule_with_an_image_on(pool: PgPool, provider: &str) {
    let w = world_on(pool, "'chat_completions'", provider).await;
    images_on(&w).await;
    type_default(&w, "block", "{secret}", &[]).await;
    let before = snapshot(&w.f.store.pool).await;
    let (status, _, body) =
        send(&w, "/v1/chat/completions", chat_with_image("what is this?")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{provider}: {body}");
    assert_eq!(
        body["error"]["code"], "guardrail_not_supported",
        "{provider}"
    );
    let (status, _, body) = send(
        &w,
        "/v1/chat/completions",
        chat_with_image(&format!("deploy with {}", secret())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{provider}: {body}");
    assert_eq!(
        body["error"]["code"], "guardrail_input_blocked",
        "{provider}"
    );
    assert!(
        !body.to_string().contains("iVBOR"),
        "the image is never echoed"
    );
    assert_eq!(w.hits.load(Ordering::SeqCst), 0, "{provider}: nothing sent");
    assert_eq!(
        snapshot(&w.f.store.pool).await,
        before,
        "{provider}: nothing admitted"
    );
}

#[dbtest::test]
async fn a_messages_image_is_refused_by_a_blocking_rule_on_a_native_route(pool: PgPool) {
    blocking_rule_with_a_messages_image_on(pool, "anthropic").await;
}

#[dbtest::test]
async fn a_messages_image_is_refused_by_a_blocking_rule_on_a_translated_route(pool: PgPool) {
    blocking_rule_with_a_messages_image_on(pool, "openai").await;
}

/// A Messages image block is uninspectable on a native route (forwarded in
/// its own envelope) and on a translated one alike: a blocking rule refuses
/// it (`guardrail_not_supported`) before anything is admitted or sent, and
/// a secret in the text beside it is still found.
async fn blocking_rule_with_a_messages_image_on(pool: PgPool, provider: &str) {
    let w = world_on(pool, "'messages'", provider).await;
    images_on(&w).await;
    type_default(&w, "block", "{secret}", &[]).await;
    let before = snapshot(&w.f.store.pool).await;
    let data = PNG.split_once(',').unwrap().1;
    let body = |text: &str| {
        json!({"model":"company/smart","max_tokens":16,"messages":[{"role":"user","content":[
            {"type":"text","text":text},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}]}]})
    };
    let (status, _, value) = send(&w, "/v1/messages", body("what is this?")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{provider}: {value}");
    assert_eq!(
        value["error"]["type"], "invalid_request_error",
        "{provider}"
    );
    assert_eq!(
        value["error"]["guardrail"]["category"], "secret",
        "{provider}: {value}"
    );
    let refused: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM guardrail_events WHERE refused ORDER BY created_at,id",
    )
    .fetch_all(&w.f.store.pool)
    .await
    .unwrap();
    assert_eq!(
        refused,
        ["skipped"],
        "{provider}: the image could not be read"
    );
    let (status, _, value) = send(
        &w,
        "/v1/messages",
        body(&format!("deploy with {}", secret())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{provider}: {value}");
    assert!(
        !value.to_string().contains("iVBOR"),
        "the image is never echoed"
    );
    let refused: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM guardrail_events WHERE refused ORDER BY created_at,id",
    )
    .fetch_all(&w.f.store.pool)
    .await
    .unwrap();
    assert_eq!(
        refused,
        ["skipped", "blocked"],
        "{provider}: the text beside it was read"
    );
    assert_eq!(w.hits.load(Ordering::SeqCst), 0, "{provider}: nothing sent");
    assert_eq!(
        snapshot(&w.f.store.pool).await,
        before,
        "{provider}: nothing admitted"
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

/// #841 (0099) on an installation with guardrails: rules written before the
/// migration (secrets, custom, a tightening) and their decisions are kept and
/// read as before (no kinds, no kind); a personal-data rule can then be
/// stored with its kinds, and only with them.
#[sqlx::test(migrations = false)]
async fn the_kinds_migration_keeps_earlier_rules_and_decisions(pool: PgPool) {
    let all = sqlx::migrate!("./enterprise_migrations");
    let through = |last: i64| sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= last).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    // Every migration before this one.
    through(98).run(&pool).await.unwrap();
    let f = crate::governance::tests::db::fixture(pool.clone()).await;
    let ws = f.principal.workspace_id;
    // The type default of the Team's kind, which its local layer tightens.
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(f.team.workspace_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (set, local) = (Uuid::now_v7(), Uuid::now_v7());
    let (secrets, custom) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default',$2,false,$3)")
        .bind(set).bind(&kind).bind(f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,0,'Secrets','secrets','pattern','{secret}','monitor','user_text','off','fail_open','{generation}','deny')")
        .bind(secrets).bind(set).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,patterns) VALUES($1,$2,1,'Codes','custom','pattern','{custom}','block','user_text','off','fail_closed','{generation}','deny','{ACME-[0-9]+}')")
        .bind(custom).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    // A local layer is written by one of the workspace's owners or admins.
    let team = f.team.workspace_id;
    let admin: Uuid = sqlx::query_scalar("SELECT user_id FROM effective_workspace_memberships WHERE workspace_id=$1 AND role IN ('owner','admin') LIMIT 1")
        .bind(team).fetch_one(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,workspace_id,updated_by) VALUES($1,'workspace_local',$2,$3)")
        .bind(local).bind(team).bind(admin).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,tightens_rule_id,input_action) VALUES($1,$2,0,$3,'block')")
        .bind(Uuid::now_v7()).bind(local).bind(secrets).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,action,count) VALUES($1,$2,$3,$4,'chat_completions','company/smart','input',$5,$6,1,'type_default','pattern','secret','monitored',2)")
        .bind(Uuid::now_v7()).bind(Uuid::now_v7()).bind(ws).bind(f.principal.key_id).bind(set).bind(secrets)
        .execute(&pool).await.unwrap();

    through(i64::MAX).run(&pool).await.unwrap();
    let kinds: Vec<Vec<String>> =
        sqlx::query_scalar("SELECT kinds FROM guardrail_rules ORDER BY rule_set_id,position")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        kinds,
        [Vec::<String>::new(), vec![], vec![]],
        "earlier rules name no kinds"
    );
    let event: Option<String> = sqlx::query_scalar("SELECT kind FROM guardrail_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(event, None);
    let mut conn = pool.acquire().await.unwrap();
    let read =
        crate::guardrails::stored::load(&mut conn, crate::guardrails::stored::Scope::Type(&kind))
            .await
            .unwrap()
            .expect("readable")
            .expect("the type default");
    assert_eq!(read.rules.len(), 2);
    drop(conn);
    // A personal-data rule is stored with its kinds, and not without them.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
        .bind(set)
        .bind(f.owner)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable,kinds) VALUES($1,$2,2,'Cards','personal_data','pattern','{personal_data}','block','user_text','off','fail_closed','{generation}','deny','{card}')")
        .bind(Uuid::now_v7()).bind(set).execute(&mut *tx).await.unwrap();
    let missing = sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,3,'All','personal_data','pattern','{personal_data}','block','user_text','off','fail_closed','{generation}','deny')")
        .bind(Uuid::now_v7()).bind(set).execute(&mut *tx).await;
    assert_eq!(
        sqlstate(missing.map(|_| ())),
        "23514",
        "a personal-data rule without kinds"
    );
    drop(tx);
    // A decision's kind is checked in the month partition that existed
    // before the migration and in one created after it: only a known kind,
    // and only on a personal-data decision.
    let created: Vec<(String, String)> = sqlx::query_as(
        "SELECT parent,partition FROM omg_ensure_partitions(0,clock_timestamp()+interval '14 months')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        created
            .iter()
            .any(|(parent, _)| parent == "guardrail_events"),
        "{created:?}"
    );
    for at in [
        "clock_timestamp()",
        "clock_timestamp()+interval '14 months'",
    ] {
        let insert = |category: &'static str, kind: &'static str| {
            let pool = pool.clone();
            let sql = format!(
                "INSERT INTO guardrail_events(id,root_request_id,workspace_id,api_key_id,operation,model,stage,rule_set_id,rule_id,revision,layer,detector_kind,category,kind,action,count,created_at) VALUES($1,$2,$3,$4,'chat_completions','company/smart','input',$5,$6,1,'type_default','pattern',$7,$8,'monitored',1,{at})"
            );
            async move {
                sqlx::query(&sql)
                    .bind(Uuid::now_v7())
                    .bind(Uuid::now_v7())
                    .bind(ws)
                    .bind(f.principal.key_id)
                    .bind(set)
                    .bind(secrets)
                    .bind(category)
                    .bind(kind)
                    .execute(&pool)
                    .await
                    .map(|_| ())
            }
        };
        assert_eq!(sqlstate(insert("secret", "email").await), "23514", "{at}");
        assert_eq!(
            sqlstate(insert("personal_data", "passport").await),
            "23514",
            "{at}"
        );
        insert("personal_data", "email").await.expect(at);
    }
}

/// 0099 on a database where a rule already looks for personal data (no
/// release wrote one: it was written directly) stops and names the rows,
/// and applies once they are gone.
#[sqlx::test(migrations = false)]
async fn the_kinds_migration_stops_on_a_personal_data_rule_written_directly(pool: PgPool) {
    let all = sqlx::migrate!("./enterprise_migrations");
    let through = |last: i64| sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= last).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    through(98).run(&pool).await.unwrap();
    let f = crate::governance::tests::db::fixture(pool.clone()).await;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(f.team.workspace_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let set = Uuid::now_v7();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO guardrail_rule_sets(id,layer,kind,local_only,updated_by) VALUES($1,'type_default',$2,false,$3)")
        .bind(set).bind(&kind).bind(f.owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO guardrail_rules(id,rule_set_id,position,name,protection,detector_kind,categories,input_action,redact_scope,output_mode,on_error,workloads,uninspectable) VALUES($1,$2,0,'PII','personal_data','pattern','{personal_data}','block','user_text','off','fail_closed','{generation}','deny')")
        .bind(Uuid::now_v7()).bind(set).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let err = through(i64::MAX).run(&pool).await.unwrap_err().to_string();
    assert!(
        err.contains("1 guardrail rule(s) already look for personal_data"),
        "{err}"
    );
    let version: i64 =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, 98, "nothing of 0099 applied");
    // Removed (by the set's writer, which moves its revision), it applies.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE guardrail_rule_sets SET revision=revision+1,updated_by=$2 WHERE id=$1")
        .bind(set)
        .bind(f.owner)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM guardrail_rules WHERE rule_set_id=$1")
        .bind(set)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    through(i64::MAX).run(&pool).await.unwrap();
}

fn sqlstate(r: Result<(), sqlx::Error>) -> String {
    match r {
        Err(sqlx::Error::Database(e)) => e.code().map(|c| c.into_owned()).unwrap_or_default(),
        other => format!("{other:?}"),
    }
}
