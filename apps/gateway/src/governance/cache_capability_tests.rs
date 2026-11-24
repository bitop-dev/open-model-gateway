//! Cache-control pricing follows the route adapter's declared capability
//! (`ProviderAdapter::forwards_cache_controls`), through the real HTTP path,
//! engine and PostgreSQL admission: the same native Messages request with a
//! `cache_control` hint on an Anthropic route (forwards the hint: pinned TTL
//! write and read prices required, refused before any write otherwise) and on
//! an OpenAI route (drops it: priced as a plain request). The holds are exact.
use super::{tests::db::*, *};
use crate::{
    inference::{Engine, EngineLimits},
    providers::{
        ProviderRegistry,
        anthropic::AnthropicAdapter,
        openai::OpenAiAdapter,
        secrets::{Secret, SecretResolver},
    },
};
use axum::{Json, Router, http::Uri};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
        Secret::new("server-provider-key".into())
    }
}
struct Servers(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Servers {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// (upstream path, held µUSD of the pending reservation it was called under)
type Calls = Arc<Mutex<Vec<(String, Option<i64>)>>>;

async fn serve(app: Router, servers: &mut Servers) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    servers.0.push(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    base
}

fn upstream(db: sqlx::PgPool, calls: Calls) -> Router {
    Router::new().fallback(move |uri: Uri, Json(body): Json<Value>| {
        let (db, calls) = (db.clone(), calls.clone());
        async move {
            let held: Option<i64> = sqlx::query_scalar("SELECT held_microusd FROM governance_reservations WHERE state='pending' ORDER BY admitted_at DESC LIMIT 1").fetch_one(&db).await.unwrap();
            calls.lock().unwrap().push((uri.path().to_owned(), held));
            if uri.path().ends_with("/messages") {
                // Anthropic received the hint natively.
                assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
                Json(json!({"id":"upstream","type":"message","role":"assistant","model":"private","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,
                    "usage":{"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":4,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":4}}}))
            } else {
                assert!(!body.to_string().contains("cache_control"), "{body}");
                Json(json!({"id":"upstream","object":"chat.completion","created":0,"model":"private",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}))
            }
        }
    })
}

/// A price of `version` on `deployment`; v2/v3 with every cache rate priced
/// except the read rate when `read` is false.
async fn publish(f: &Fixture, deployment: Uuid, version: i32, read: bool) -> Uuid {
    if version == 1 {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,100,50,1)").bind(id).bind(deployment).execute(&f.store.pool).await.unwrap();
        return id;
    }
    let mut rates = rates();
    if !read {
        rates.read = CacheRate::NotApplicable;
    }
    f.cache_price(deployment, version, rates).await
}

#[dbtest::test]
async fn cache_control_holds_follow_the_route_adapter(pool: sqlx::PgPool) {
    let f = fixture(pool).await;
    // `company/smart` stays on the fixture's OpenAI connection; `company/claude`
    // is an Anthropic route. Both serve native Messages.
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['messages']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let (model, connection, claude) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'company/claude',ARRAY['messages'])").bind(model).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Claude','anthropic','env:NOT_RESOLVED',true)").bind(connection).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'private',true)").bind(claude).bind(model).bind(connection).execute(&f.store.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')",
    )
    .bind(f.principal.workspace_id)
    .bind(model)
    .execute(&f.store.pool)
    .await
    .unwrap();
    let key = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'cache capability',$4)")
        .bind(key.id).bind(f.principal.workspace_id).bind(f.owner).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();

    let calls: Calls = Arc::default();
    let mut servers = Servers(vec![]);
    let mock = serve(upstream(f.store.pool.clone(), calls.clone()), &mut servers).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            format!("{mock}/v1"),
        )))
        .unwrap();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolver),
            format!("{mock}/v1"),
        )))
        .unwrap();
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let base = serve(
        crate::http::router_with_engine(f.store.clone(), None, engine),
        &mut servers,
    )
    .await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    // (price version, cache read priced, model) -> the admitted attempt's
    // held µUSD and settlement state, or `None` when refused with
    // `cache_control` before any write.
    let mut observed = Vec::new();
    for (version, read) in [(1, true), (2, true), (2, false), (3, true), (3, false)] {
        for (model, deployment) in [("company/claude", claude), ("company/smart", f.deployment)] {
            let price = publish(&f, deployment, version, read).await;
            let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let before = calls.lock().unwrap().len();
            let response = client
                .post(format!("{base}/v1/messages"))
                .bearer_auth(&key.token)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", "extended-cache-ttl-2025-04-11")
                .json(&json!({"model":model,"max_tokens":10,
                    "system":[{"type":"text","text":"cache me","cache_control":{"type":"ephemeral","ttl":"1h"}}],
                    "messages":[{"role":"user","content":"hello"}]}))
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            let body: Value = response.json().await.unwrap();
            let after: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let calls = calls.lock().unwrap().clone();
            if status == 400 {
                assert!(
                    body["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("cache_control"),
                    "{body}"
                );
                assert_eq!(after, rows, "no reservation before the refusal");
                assert_eq!(calls.len(), before, "no upstream call");
                observed.push(format!("v{version} read={read} {model}: refused"));
                continue;
            }
            assert_eq!(status, 200, "v{version} read={read} {model}: {body}");
            assert_eq!(after, rows + 1);
            assert_eq!(calls.len(), before + 1);
            let (held, state, pinned): (Option<i64>, String, Uuid) = sqlx::query_as("SELECT held_microusd,state,price_id FROM governance_reservations WHERE deployment_id=$1 ORDER BY admitted_at DESC LIMIT 1")
                .bind(deployment).fetch_one(&f.store.pool).await.unwrap();
            assert_eq!(calls[before].1, held, "the upstream ran under that hold");
            assert_eq!(
                calls[before].0.ends_with("/messages"),
                model == "company/claude"
            );
            assert_eq!(pinned, price);
            observed.push(format!(
                "v{version} read={read} {model}: held {held:?}, {state}"
            ));
        }
    }
    // Per route and price version, the hold or refusal: the route adapter
    // alone decides whether the cache hint is priced. The Anthropic route
    // forwards it, so it needs pinned TTL write and read prices (refused
    // without them, before any write). The OpenAI route drops it and is held
    // as a plain request; its upstream reports no cache allocation against
    // cache-priced v2/v3 rates, so the usage stays unknown (the hold is
    // retained, never treated as zero).
    assert_eq!(
        observed,
        [
            "v1 read=true company/claude: refused",
            "v1 read=true company/smart: held Some(110), settled",
            "v2 read=true company/claude: held Some(570), settled",
            "v2 read=true company/smart: held Some(570), unknown",
            "v2 read=false company/claude: refused",
            "v2 read=false company/smart: held Some(560), unknown",
            "v3 read=true company/claude: held Some(570), settled",
            "v3 read=true company/smart: held Some(570), unknown",
            "v3 read=false company/claude: refused",
            "v3 read=false company/smart: held Some(560), unknown",
        ]
    );
}
